# Release check for the GIT_WORKFLOW.md release rule. PowerShell 5.1 compatible.
# Read-only: it never tags, pushes, or modifies anything.
#
# Checks:
#   1. Cargo.toml declares a version.
#   2. A latest Git tag exists (a missing tag fails with a clear message).
#   3. The latest tag equals v<version>, unless -Prepare says this tree is
#      actively preparing a new release; then the version must exceed the tag.
#   4. CHANGELOG.md contains the version string.
#   5. README.md names the latest tag, or the prepared version with -Prepare.
#   6. No known stale phrase appears in current durable docs or source
#      (archive paths excluded).
#   7. `cargo doc --no-deps` is warning-free (skip with -SkipDoc).
#   8. The webui status fixture quotes the same version as Cargo.toml (a bump
#      that forgets the fixture pin ships a red test; v1.1.3 proved the gap).
#   9. Public docs carry no machine paths (README.md, CHANGELOG.md,
#      CONTRIBUTING.md, docs/**). The ban targets personal locations on the
#      operator machine; documented engine defaults stay allowed (the llama.cpp
#      install default). docs/maintenance/ holds frozen historical audit
#      records, scanned at their intake, so they are exempt. Rule home:
#      docs/process/GIT_WORKFLOW.md (release section).
#
# Exit 0 only when every check passes.

param(
  [switch]$Prepare,
  [switch]$SkipDoc
)

$ErrorActionPreference = 'Continue'

$script:allPass = $true

function Report([bool]$ok, [string]$message) {
  if ($ok) { Write-Output ("PASS " + $message) }
  else { Write-Output ("FAIL " + $message); $script:allPass = $false }
}

function Get-VersionKey([string]$v) {
  $parts = ($v -replace '^v', '') -split '\.'
  $nums = @(0, 0, 0)
  for ($i = 0; $i -lt 3 -and $i -lt $parts.Count; $i++) {
    $n = 0
    if ([int]::TryParse(($parts[$i] -replace '[^0-9].*$', ''), [ref]$n)) { $nums[$i] = $n }
  }
  return $nums[0] * 1000000 + $nums[1] * 1000 + $nums[2]
}

# 1. Version in Cargo.toml.
$version = $null
if (Test-Path 'Cargo.toml') {
  $m = Select-String -Path 'Cargo.toml' -Pattern '^\s*version\s*=\s*"([^"]+)"' |
    Select-Object -First 1
  if ($m) { $version = $m.Matches[0].Groups[1].Value }
}
Report ([bool]$version) ("Cargo.toml declares a version" + $(if ($version) { ": " + $version } else { "" }))

# 2. Latest tag.
$tag = $null
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  Report $false "git not on PATH"
} else {
  $global:LASTEXITCODE = -1
  $tagOut = git describe --tags --abbrev=0 2>$null
  $tagCode = $LASTEXITCODE
  if ($tagCode -eq 0 -and $tagOut) { $tag = ("$tagOut").Trim() }
}
Report ([bool]$tag) $(if ($tag) {
  "latest tag found: " + $tag
} else {
  "no Git tag found; a release needs an annotated tag v<version> (run git tag -a v<version>)"
})

if (-not $version -or -not $tag) {
  Write-Output "RELEASE CHECK FAIL (version or tag missing; later checks skipped)"
  exit 1
}

# 3. Tag equals v<version>, or the tree prepares a newer release.
if ($Prepare) {
  $ok = (Get-VersionKey $version) -gt (Get-VersionKey $tag)
  Report $ok ("preparing a new release: version " + $version + " exceeds tag " + $tag)
} else {
  Report ($tag -eq ("v" + $version)) ("latest tag " + $tag + " equals v" + $version)
}

# 4. Changelog entry for the version.
$changelogOk = (Test-Path 'CHANGELOG.md') -and
  (Select-String -Path 'CHANGELOG.md' -Pattern $version -SimpleMatch -Quiet)
Report $changelogOk ("CHANGELOG.md contains version " + $version)

# 5. README release claim names the tag, or the prepared version with -Prepare.
$readmeTag = if ($Prepare) { "v" + $version } else { $tag }
$readmeOk = (Test-Path 'README.md') -and
  (Select-String -Path 'README.md' -Pattern $readmeTag -SimpleMatch -Quiet)
Report $readmeOk ("README.md names tag " + $readmeTag)

# 6. Known stale phrases in current durable docs and source.
$stalePhrases = @("there is no auth", "arrive with their own turns")
$scanRoots = @("README.md", "AGENTS.md", "CONTRIBUTING.md", "docs/DOCS_README.md", "docs/process", "docs/architecture", "docs/reference", "docs/maintenance", "src", "tests")
$scanFiles = @()
foreach ($root in $scanRoots) {
  if (Test-Path -LiteralPath $root) {
    if ((Get-Item -LiteralPath $root).PSIsContainer) {
      $scanFiles += (Get-ChildItem -LiteralPath $root -Recurse -File |
        Where-Object { ($_.Extension -eq '.md' -or $_.Extension -eq '.rs') -and
          $_.FullName -notmatch '\\archive\\' } |
        ForEach-Object { $_.FullName })
    } else {
      $scanFiles += (Resolve-Path -LiteralPath $root).Path
    }
  }
}
$staleHits = @()
foreach ($phrase in $stalePhrases) {
  $hit = Select-String -Path $scanFiles -Pattern $phrase -SimpleMatch -List -ErrorAction SilentlyContinue
  if ($hit) { $staleHits += ($phrase + " -> " + $hit.Path) }
}
$staleOk = ($staleHits.Count -eq 0)
Report $staleOk ("no known stale phrase in current docs and source" +
  $(if ($staleHits.Count -gt 0) { ": " + ($staleHits -join "; ") } else { "" }))

# 7. cargo doc warnings.
if (-not $SkipDoc) {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Report $false "cargo not on PATH"
  } else {
    Write-Output "Running cargo doc --no-deps"
    $global:LASTEXITCODE = -1
    $docOutput = & cargo doc --no-deps 2>&1 | Out-String
    $docCode = $LASTEXITCODE
    $docOk = ($docCode -eq 0) -and ($docOutput -notmatch 'warning:')
    Report $docOk "cargo doc --no-deps is warning-free"
    if (-not $docOk) {
      $warningCount = ([regex]::Matches($docOutput, 'warning:')).Count
      Write-Output ("cargo doc warning count: " + $warningCount)
    }
  }
}

# 8. The webui fixture quotes the crate version in its TOP-LEVEL `version`
#    property, compared case-sensitively - exactly the key the parity test
#    `the_webui_fixture_is_a_document_the_endpoint_can_actually_emit` reads
#    (serde exact-key lookup). A first-regex-match read would compare a nested
#    decoy or a recased key instead (review B3R-01). The release-check never
#    runs tests, so the bump commit itself needs this mechanical guard.
$fixturePath = 'tests/webui/fixtures/status.json'
$fixtureOk = $false
if (Test-Path -LiteralPath $fixturePath) {
  $fixtureVersion = $null
  $fixtureError = $null
  try {
    $fixture = Get-Content -LiteralPath $fixturePath -Raw -Encoding UTF8 | ConvertFrom-Json
    $prop = @($fixture.PSObject.Properties) |
      Where-Object { $_.Name -ceq 'version' } |
      Select-Object -First 1
    if ($null -ne $prop) { $fixtureVersion = $prop.Value }
  } catch {
    $fixtureError = "$_"
  }
  if ($fixtureError) {
    Report $false ("webui fixture is not valid JSON: " + $fixtureError)
  } elseif ($null -eq $fixtureVersion) {
    Report $false ("webui fixture has no top-level version property: " + $fixturePath)
  } else {
    $fixtureOk = ($fixtureVersion -ceq $version)
    Report $fixtureOk ("webui fixture quotes top-level version " + $fixtureVersion +
      " matching Cargo.toml " + $version)
  }
} else {
  Report $false "webui fixture missing: $fixturePath"
}

# 9. Public docs carry no personal machine paths (operator ruling 2026-11-04,
#    review follow-up: DECISIONS.md and DOCTRINE.md are scanned like every
#    other live doc; the ledger line that leaked a research path was scrubbed
#    to a name-only reference). The blocklist names personal locations on the
#    operator machine: the repo roots, user homes, the scratch and backup
#    volumes, and the staging and rollback directory names. Legitimate
#    documented engine defaults are not personal and stay allowed: the
#    llama.cpp install default lives under C:\Inference, so the ban exempts
#    exactly that one subtree. docs/maintenance/** is the one exemption:
#    frozen historical audit records, scanned at their intake.
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
$publicDocs = @('README.md', 'CHANGELOG.md', 'CONTRIBUTING.md')
if (Test-Path 'docs') {
  # docs/maintenance/** holds frozen historical audit records (scanned at
  # their intake), so the live-doc ban does not rewrite them here.
  $publicDocs += (Get-ChildItem -LiteralPath 'docs' -Recurse -Filter '*.md' |
    Where-Object { $_.FullName -notmatch '\\maintenance\\' } |
    ForEach-Object { $_.FullName })
}
$machinePathHits = @()
foreach ($pattern in $personalPathPatterns) {
  $hits = Select-String -Path $publicDocs -Pattern $pattern -ErrorAction SilentlyContinue
  foreach ($hit in $hits) {
    $machinePathHits += ($hit.Path + ":" + $hit.LineNumber)
  }
}
Report ($machinePathHits.Count -eq 0) ("public docs carry no machine paths" +
  $(if ($machinePathHits.Count -gt 0) { ": " + (($machinePathHits | Sort-Object -Unique) -join "; ") } else { "" }))

if ($script:allPass) {
  Write-Output "RELEASE CHECK PASS"
  exit 0
}
Write-Output "RELEASE CHECK FAIL"
exit 1
