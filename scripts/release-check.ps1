# Release check for the GIT_WORKFLOW.md release rule. PowerShell 5.1 compatible.
# Read-only: it never tags, pushes, or modifies anything.
#
# Checks:
#   1. Cargo.toml declares a version.
#   2. A latest Git tag exists (a missing tag fails with a clear message).
#   3. The latest tag equals v<version>, unless -Prepare says this tree is
#      actively preparing a new release; then the version must exceed the tag.
#   4. CHANGELOG.md contains the version string.
#   5. README.md names the latest tag.
#
# Exit 0 only when every check passes.

param(
  [switch]$Prepare
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
$tagOut = git describe --tags --abbrev=0 2>$null
if ($LASTEXITCODE -eq 0 -and $tagOut) { $tag = ("$tagOut").Trim() }
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

# 5. README release claim names the tag.
$readmeOk = (Test-Path 'README.md') -and
  (Select-String -Path 'README.md' -Pattern $tag -SimpleMatch -Quiet)
Report $readmeOk ("README.md names tag " + $tag)

if ($script:allPass) {
  Write-Output "RELEASE CHECK PASS"
  exit 0
}
Write-Output "RELEASE CHECK FAIL"
exit 1
