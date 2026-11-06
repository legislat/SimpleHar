# Assemble the curated PUBLIC release asset for one version tag.
# PowerShell 5.1 compatible. Read-only over its inputs: it writes only
# inside -OutDir and never touches the repository or the source folder.
#
# The zip carries exactly the five-entry release package manifest (the one
# documented contract - never the personal deploy directory):
#   simpleharn.exe
#   SimpleHarn.vbs
#   README.md
#   LICENSE
#   docs/assets/control-page.png
# The scrub record and SHA256SUMS.txt are written beside the zip in -OutDir,
# not inside it. SHA256SUMS.txt covers the exe and the zip (a file cannot
# carry the hash of the zip that contains it).
# The zip is named SimpleHarn-<version-without-v>-win-x64.zip (the version
# without the leading v), the name the README quick start links.
#
# Refusals (all hard FAIL):
#   - the source dir (the exe's folder unless -SourceDir says otherwise)
#     contains ModelConf, logs, simpleharn.json, or START-NOTES.txt:
#     those are personal runtime files and stay local (PUBLICATION.md);
#   - the exe hash differs from -ExpectedSha256 (the hash the changelog
#     records for that tag);
#   - any staged text file trips the personal machine-path blocklist that
#     scripts/release-check.ps1 check 9 enforces.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/make-public-asset.ps1 `
#     -Version v1.1.3 -ExePath <release-exe> `
#     -ExpectedSha256 <sha256> -OutDir <fresh-output-dir>
# Run from the repository root (README.md, LICENSE, SimpleHarn.vbs, and
# docs/assets/control-page.png are read here).
# Exit 0 only when every step passes.

param(
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$ExePath,
  [Parameter(Mandatory = $true)][string]$ExpectedSha256,
  [Parameter(Mandatory = $true)][string]$OutDir,
  [string]$SourceDir
)

$ErrorActionPreference = 'Continue'

$script:allPass = $true

function Report([bool]$ok, [string]$message) {
  if ($ok) { Write-Output ("PASS " + $message) }
  else { Write-Output ("FAIL " + $message); $script:allPass = $false }
}

# 1. Argument shape.
Report ($Version -match '^v\d+\.\d+\.\d+$') ("version argument names a tag: " + $Version)
$exeFull = $null
if (Test-Path -LiteralPath $ExePath -PathType Leaf) {
  $exeFull = (Resolve-Path -LiteralPath $ExePath).Path
  Report $true ("exe present: " + [IO.Path]::GetFileName($exeFull))
} else {
  Report $false ("exe not found at the given path: " + $ExePath)
}
if (-not $SourceDir) { $SourceDir = [IO.Path]::GetDirectoryName($ExePath) }
if (-not $script:allPass) {
  Write-Output "ASSET BUILD FAIL (arguments invalid; build stopped)"
  exit 1
}

# 2. Refuse a personal deploy directory as the source.
$forbidden = @('ModelConf', 'logs', 'simpleharn.json', 'START-NOTES.txt')
$found = @()
foreach ($name in $forbidden) {
  if (Test-Path -LiteralPath (Join-Path $SourceDir $name)) { $found += $name }
}
Report ($found.Count -eq 0) ("source dir carries no personal runtime files" +
  $(if ($found.Count -gt 0) { "; REFUSE: found " + ($found -join ', ') } else { "" }))
if ($found.Count -gt 0) {
  Write-Output "ASSET BUILD FAIL (REFUSED: the source dir is a personal deploy directory;"
  Write-Output "  copy the exe alone to a clean folder and point -ExePath at it)"
  exit 1
}

# 3. Hash provenance against the recorded value.
$actualSha = (Get-FileHash -LiteralPath $exeFull -Algorithm SHA256).Hash
$hashOk = ($actualSha -ieq $ExpectedSha256)
Report $hashOk ("exe SHA256 " + $(if ($hashOk) { "matches" } else { "DIFFERS from" }) +
  " the expected " + $ExpectedSha256)
if (-not $hashOk) { Write-Output "  actual:   " + $actualSha }
if (-not $script:allPass) {
  Write-Output "ASSET BUILD FAIL (hash provenance broken; build stopped)"
  exit 1
}

# 4. Stage the five-entry manifest (writes only inside -OutDir).
$staged = Join-Path $OutDir ("staged-" + $Version)
New-Item -ItemType Directory -Force -Path $staged | Out-Null
Copy-Item -LiteralPath $exeFull -Destination (Join-Path $staged 'simpleharn.exe') -Force
Copy-Item -LiteralPath 'SimpleHarn.vbs' -Destination (Join-Path $staged 'SimpleHarn.vbs') -Force
Copy-Item -LiteralPath 'README.md' -Destination (Join-Path $staged 'README.md') -Force
Copy-Item -LiteralPath 'LICENSE' -Destination (Join-Path $staged 'LICENSE') -Force
New-Item -ItemType Directory -Force -Path (Join-Path $staged 'docs\assets') | Out-Null
Copy-Item -LiteralPath 'docs/assets/control-page.png' `
  -Destination (Join-Path $staged 'docs\assets\control-page.png') -Force
Report $true "staged simpleharn.exe, SimpleHarn.vbs, README.md, LICENSE, docs/assets/control-page.png"

# 5. Machine-path scan over the staged text files (same blocklist and
#    spirit as release-check.ps1 check 9 and scripts/check-docs.mjs; nothing
#    here carries an exemption). The exe and the png are excluded: both are
#    binary, and a tagged build legitimately embeds build-machine paths in
#    Rust panic metadata. The exe's identity is pinned by the SHA256 check
#    above. LICENSE carries no extension and is scanned like every text file.
$scanFiles = Get-ChildItem -LiteralPath $staged -File -Recurse |
  Where-Object { ($_.Extension -ne '.exe') -and ($_.Extension -ne '.png') } |
  ForEach-Object { $_.FullName }
# Personal machine-path bans: read the one shared set (scripts/machine-path-bans.json)
# so this scan, scripts/release-check.ps1 check 9, and scripts/check-docs.mjs can
# never disagree. The author-machine root is banned whole, with no exception (FR-15).
$bansPath = Join-Path $PSScriptRoot 'machine-path-bans.json'
$bansError = ''
$personalPathPatterns = @()
try {
  $personalPathPatterns = @(
    (Get-Content -LiteralPath $bansPath -Raw -Encoding UTF8 |
      ConvertFrom-Json).bans)
} catch {
  $bansError = "$_"
}
$personalPathPatterns = @($personalPathPatterns |
  Where-Object { ($_ -is [string]) -and ($_ -ne '') })
if ($bansError -or $personalPathPatterns.Count -eq 0) {
  Report $false ("shared machine-path ban set missing or empty: " + $bansPath +
    $(if ($bansError) { " (" + $bansError + ")" } else { "" }))
} else {
  $pathHits = @()
  foreach ($pattern in $personalPathPatterns) {
    $hits = Select-String -Path $scanFiles -Pattern $pattern -ErrorAction SilentlyContinue
    foreach ($hit in $hits) { $pathHits += ([IO.Path]::GetFileName($hit.Path) + ":" + $hit.LineNumber) }
  }
  Report ($pathHits.Count -eq 0) ("staged files carry no machine paths" +
    $(if ($pathHits.Count -gt 0) { ": " + (($pathHits | Sort-Object -Unique) -join "; ") } else { "" }))
}

if (-not $script:allPass) {
  Write-Output "ASSET BUILD FAIL (staged content failed a check; build stopped)"
  exit 1
}

# 6. Zip and sums. The zip name drops the tag's leading v: the README quick
#    start links SimpleHarn-<version-without-v>-win-x64.zip.
$assetVersion = $Version -replace '^v', ''
$zip = Join-Path $OutDir ("SimpleHarn-" + $assetVersion + "-win-x64.zip")
$zipItems = Get-ChildItem -LiteralPath $staged | ForEach-Object { $_.FullName }
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path $zipItems -DestinationPath $zip -CompressionLevel Optimal
Report $true ("zipped to " + [IO.Path]::GetFileName($zip))

$zipSha = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
$sums = ("" + $actualSha + "  simpleharn.exe`n" + $zipSha + "  " + [IO.Path]::GetFileName($zip) + "`n")
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS.txt'), $sums)
Report $true "wrote SHA256SUMS.txt beside the zip (exe + zip)"

# 7. Scrub record beside the zip (it documents the build; it is not package
#    content and no longer ships inside the zip).
$scrub = @'
# Publication scrub record - <VERSION> public asset

Date: <DATE>. Built by scripts/make-public-asset.ps1 per docs/process/PUBLICATION.md.

Bundle (inside the zip): simpleharn.exe (SHA256 <EXESHA>), SimpleHarn.vbs,
README.md, LICENSE, docs/assets/control-page.png. SHA256SUMS.txt and this
record ship beside the zip, not inside it.

Included from the repository: README.md and LICENSE as tagged; the launcher
SimpleHarn.vbs; the control-page screenshot as committed after its scrub.

Excluded by rule: ModelConf, logs, the deployment config file
(simpleharn.json), and
START-NOTES.txt. These name personal paths and rows and stay local.

Checks: the exe hash was compared against the changelog value for this
tag; every staged text file was scanned against the personal machine-path
blocklist that scripts/release-check.ps1 check 9 enforces. The exe and the
screenshot are binary and are pinned by the recorded hashes instead.
'@
$scrub = $scrub.Replace('<VERSION>', $Version).Replace('<DATE>', (Get-Date -Format 'yyyy-MM-dd')).Replace('<EXESHA>', $actualSha)
[IO.File]::WriteAllText((Join-Path $OutDir 'PUBLICATION-SCRUB.md'), $scrub)
Report $true "wrote PUBLICATION-SCRUB.md beside the zip"

Write-Output ("  exe sha256: " + $actualSha)
Write-Output ("  zip sha256: " + $zipSha)
Write-Output "ASSET BUILD PASS"
exit 0
