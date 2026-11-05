# Fresh-clone onboarding check. PowerShell 5.1 compatible.
# Clones the repository to a temp directory and verifies the durable onboarding path.
# Read-only: it never modifies the source tree or the clone after creation.
#
# Checks:
#   1. The repository clones without error.
#   2. AGENTS.md and docs/DOCS_README.md exist.
#   3. Every durable document named in the maintained list below exists.
#   4. The document checkers pass on the clone (links resolve).
#
# Exit 0 only when every check passes. Run at release and weekly, not every push.

param(
  [string]$SourcePath = ".",
  [switch]$KeepClone,
  [switch]$SkipCheckers
)

$ErrorActionPreference = 'Continue'
$script:allPass = $true

function Report([bool]$ok, [string]$message) {
  if ($ok) { Write-Output ("PASS " + $message) }
  else { Write-Output ("FAIL " + $message); $script:allPass = $false }
}

$source = (Resolve-Path -LiteralPath $SourcePath).Path
if (-not (Test-Path -LiteralPath (Join-Path $source ".git"))) {
  Write-Output ("FAIL source is not a Git repository: " + $source)
  exit 1
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("simpleharn-onboarding-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempRoot | Out-Null
$clone = Join-Path $tempRoot "repo"

try {
  Write-Output ("Cloning " + $source + " to " + $clone)
  git clone --quiet --no-hardlinks $source $clone 2>&1 | Out-Null
  $cloneOk = ($LASTEXITCODE -eq 0)
  Report $cloneOk "git clone succeeded"
  if (-not $cloneOk) {
    Write-Output "FRESH-CLONE CHECK FAIL"
    exit 1
  }

  # Maintained beside `docs/DOCS_README.md`. When the docs map gains a durable
  # document, add it here too.
  $required = @(
    "AGENTS.md",
    "README.md",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "docs/DOCS_README.md",
    "docs/architecture/DOCTRINE.md",
    "docs/architecture/DECISIONS.md",
    "docs/architecture/OVERVIEW.md",
    "docs/process/ENGINEERING.md",
    "docs/process/VERIFICATION.md",
    "docs/process/GIT_WORKFLOW.md",
    "docs/process/TASKS.md",
    "docs/process/HANDOFF.md",
    "docs/process/REVIEW.md",
    "docs/process/GOTCHAS.md",
    "docs/process/PUBLICATION.md"
  )
  foreach ($rel in $required) {
    Report (Test-Path -LiteralPath (Join-Path $clone $rel)) ("required file exists: " + $rel)
  }

  $reference = @(
    "docs/reference/CONFIG.md",
    "docs/reference/MODELS.md",
    "docs/reference/API.md",
    "docs/reference/OPERATIONS.md",
    "docs/reference/TROUBLESHOOTING.md"
  )
  foreach ($rel in $reference) {
    Report (Test-Path -LiteralPath (Join-Path $clone $rel)) ("reference file exists: " + $rel)
  }

  if (-not $SkipCheckers) {
    if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
      Report $false "node not on PATH"
    } else {
      Push-Location $clone
      try {
        $global:LASTEXITCODE = -1
        $docOut = node scripts/check-docs.mjs 2>&1 | Out-String
        $docCode = $LASTEXITCODE
        Write-Output $docOut.Trim()
        Report ($docCode -eq 0) "check-docs passed on the clone"
        $global:LASTEXITCODE = -1
        $insOut = node scripts/check-instructions.mjs 2>&1 | Out-String
        $insCode = $LASTEXITCODE
        Write-Output $insOut.Trim()
        Report ($insCode -eq 0) "check-instructions passed on the clone"
      } finally {
        Pop-Location
      }
    }
  }
} finally {
  if (-not $KeepClone) {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
  } else {
    Write-Output ("Clone kept at " + $clone)
  }
}

if ($script:allPass) {
  Write-Output "FRESH-CLONE CHECK PASS"
  exit 0
}
Write-Output "FRESH-CLONE CHECK FAIL"
exit 1
