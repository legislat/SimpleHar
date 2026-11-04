# Assemble the curated PUBLIC release asset for one version tag.
# PowerShell 5.1 compatible. Read-only over its inputs: it writes only
# inside -OutDir and never touches the repository or the source folder.
#
# The asset is the minimal public bundle (never the personal deploy
# directory): simplehar-se.exe, LICENSE, README.md,
# simplehar-se.example.json, PUBLICATION-SCRUB.md. SHA256SUMS.txt is
# written beside the zip in -OutDir and covers the exe and the zip (a
# file cannot carry the hash of the zip that contains it).
#
# Refusals (all hard FAIL):
#   - the source dir (the exe's folder unless -SourceDir says otherwise)
#     contains ModelConf, logs, simplehar-se.json, or START-NOTES.txt:
#     those are personal runtime files and stay local (PUBLICATION.md);
#   - the exe hash differs from -ExpectedSha256 (the hash the changelog
#     records for that tag);
#   - any staged file trips the personal machine-path blocklist that
#     scripts/release-check.ps1 check 9 enforces.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/make-public-asset.ps1 `
#     -Version v1.1.3 -ExePath <release-exe> `
#     -ExpectedSha256 <sha256> -OutDir <fresh-output-dir>
# Run from the repository root (README.md and LICENSE are read here).
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
$forbidden = @('ModelConf', 'logs', 'simplehar-se.json', 'START-NOTES.txt')
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

# 4. Stage the bundle (writes only inside -OutDir).
$staged = Join-Path $OutDir ("staged-" + $Version)
New-Item -ItemType Directory -Force -Path $staged | Out-Null
Copy-Item -LiteralPath $exeFull -Destination (Join-Path $staged 'simplehar-se.exe') -Force
Copy-Item -LiteralPath 'LICENSE' -Destination (Join-Path $staged 'LICENSE') -Force
Copy-Item -LiteralPath 'README.md' -Destination (Join-Path $staged 'README.md') -Force
Report $true "staged exe, LICENSE, README.md"

$exampleJson = @'
{
  "internal_host": "127.0.0.1",
  "vram_total_gb": 8.0,
  "vram_reserve_gb": 2.5,
  "categories": [
    { "id": "WORK", "public_port": 8708, "max_models": 1, "evictable_by": [] },
    { "id": "RP", "public_port": 8709, "max_models": 1, "evictable_by": ["WORK"] },
    { "id": "AUX", "public_port": 8710, "max_models": 6, "evictable_by": ["WORK", "RP"] },
    { "id": "EMB", "public_port": 8711, "max_models": 1, "evictable_by": ["WORK", "RP"] }
  ]
}
'@
[IO.File]::WriteAllText((Join-Path $staged 'simplehar-se.example.json'), $exampleJson)

$scrub = @'
# Publication scrub record - <VERSION> public asset

Date: <DATE>. Built by scripts/make-public-asset.ps1 per docs/process/PUBLICATION.md.

Bundle: simplehar-se.exe (SHA256 <EXESHA>), LICENSE, README.md,
simplehar-se.example.json, this record. SHA256SUMS.txt ships beside the zip.

Included from the repository: README.md and LICENSE as tagged; the example
config is a sanitized template written by the script (loopback 127.0.0.1,
no control_token, no wsl_user, no model rows).

Excluded by rule: ModelConf, logs, the deployment simplehar-se.json, and
START-NOTES.txt. These name personal paths and rows and stay local.

Checks: the exe hash was compared against the changelog value for this
tag; every staged text file was scanned against the personal machine-path
blocklist that scripts/release-check.ps1 check 9 enforces. The exe is
binary and is pinned by the recorded hash instead.
'@
$scrub = $scrub.Replace('<VERSION>', $Version).Replace('<DATE>', (Get-Date -Format 'yyyy-MM-dd')).Replace('<EXESHA>', $actualSha)
[IO.File]::WriteAllText((Join-Path $staged 'PUBLICATION-SCRUB.md'), $scrub)
Report $true "wrote simplehar-se.example.json and PUBLICATION-SCRUB.md"

# 5. Machine-path scan over the staged text files (same blocklist and
#    spirit as release-check.ps1 check 9; nothing here carries an
#    exemption). The exe is excluded: it is binary, and a tagged build
#    legitimately embeds build-machine paths in Rust panic metadata.
#    The exe's identity is pinned by the SHA256 check above, which is
#    exactly what release-check.ps1 treats as durable text.
$scanFiles = Get-ChildItem -LiteralPath $staged -File |
  Where-Object { $_.Extension -ne '.exe' } | ForEach-Object { $_.FullName }
$personalPathPatterns = @(
  'C:\\Inference\\(?!llama\.cpp)',
  'C:\\Users\\',
  'D:\\WORKSPACE-VS',
  'D:\\SimpleHar-SE-backups',
  '_release_v',
  '_test-',
  'SimpleHar-SE_release',
  'SimpleHar-SE_test'
)
$pathHits = @()
foreach ($pattern in $personalPathPatterns) {
  $hits = Select-String -Path $scanFiles -Pattern $pattern -ErrorAction SilentlyContinue
  foreach ($hit in $hits) { $pathHits += ([IO.Path]::GetFileName($hit.Path) + ":" + $hit.LineNumber) }
}
Report ($pathHits.Count -eq 0) ("staged files carry no machine paths" +
  $(if ($pathHits.Count -gt 0) { ": " + (($pathHits | Sort-Object -Unique) -join "; ") } else { "" }))

if (-not $script:allPass) {
  Write-Output "ASSET BUILD FAIL (staged content failed a check; build stopped)"
  exit 1
}

# 6. Zip and sums.
$zip = Join-Path $OutDir ("SimpleHar-SE-" + $Version + "-windows-x64.zip")
$zipFiles = Get-ChildItem -LiteralPath $staged -File | ForEach-Object { $_.FullName }
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path $zipFiles -DestinationPath $zip -CompressionLevel Optimal
Report $true ("zipped to " + [IO.Path]::GetFileName($zip))

$zipSha = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
$sums = ("" + $actualSha + "  simplehar-se.exe`n" + $zipSha + "  " + [IO.Path]::GetFileName($zip) + "`n")
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS.txt'), $sums)
Report $true "wrote SHA256SUMS.txt (exe + zip)"

Write-Output ("  exe sha256: " + $actualSha)
Write-Output ("  zip sha256: " + $zipSha)
Write-Output "ASSET BUILD PASS"
exit 0
