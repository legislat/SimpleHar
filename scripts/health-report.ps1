# Read-only process health report. PowerShell 5.1 compatible.
# Reports five metrics for the operator's cycle-start review:
#   1. Durable docs older than a quarter (90 days) or without a review date.
#   2. Debt age: the oldest open task in the queue.
#   3. Queue age: the oldest open row in the `## Now` section.
#   4. Gate failures: the final gate verdict in the newest gate evidence.
#   5. Worktree age: worktrees untouched for more than 7 days.
#
# It never writes, creates an issue, or changes state. It exits 1 when run outside
# the repository root, otherwise 0.

$ErrorActionPreference = 'Continue'

if (-not (Test-Path -LiteralPath 'AGENTS.md')) {
  Write-Output "PROCESS HEALTH REPORT FAIL: run this script from the repository root."
  exit 1
}

function Get-FrontMatterDate([string]$path) {
  $match = Select-String -LiteralPath $path -Pattern '^last_reviewed:\s*(\d{4}-\d{2}-\d{2})' -List
  if ($match) { return [datetime]::ParseExact($match.Matches[0].Groups[1].Value, 'yyyy-MM-dd', $null) }
  return $null
}

function Get-AgeDays([datetime]$date) {
  return [math]::Floor(((Get-Date) - $date).TotalDays)
}

Write-Output "PROCESS HEALTH REPORT"
Write-Output ("Generated: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Write-Output ""

# 1. Durable docs older than a quarter. README.md is not dated: it carries no front
# matter per the 2026-11-05 ruling (storefront page) and reviews on release.
$durable = @(
  "AGENTS.md",
  "CONTRIBUTING.md",
  "docs/DOCS_README.md"
)
$durable += (Get-ChildItem -LiteralPath "docs/process" -Filter "*.md" -File | ForEach-Object { $_.FullName })
$durable += (Get-ChildItem -LiteralPath "docs/architecture" -Filter "*.md" -File | ForEach-Object { $_.FullName })
$durable += (Get-ChildItem -LiteralPath "docs/reference" -Filter "*.md" -File | ForEach-Object { $_.FullName })

$stale = @()
$undated = @()
foreach ($doc in $durable) {
  $date = Get-FrontMatterDate $doc
  if ($null -eq $date) { $undated += $doc; continue }
  if ((Get-AgeDays $date) -gt 90) { $stale += ($doc + " (" + (Get-AgeDays $date) + " days)") }
}
Write-Output ("1. Durable docs older than 90 days: " + $stale.Count)
foreach ($item in $stale) { Write-Output ("   - " + $item) }
Write-Output ("   Durable docs without a review date: " + $undated.Count)
foreach ($item in $undated) { Write-Output ("   - " + $item) }
Write-Output ""

# 2 and 3. Queue age and active-row age.
$queuePath = "handoff/PENDING_CHECKLIST.md"
$openRows = @()
$nowRows = @()
if (Test-Path -LiteralPath $queuePath) {
  $section = ""
  foreach ($line in Get-Content -LiteralPath $queuePath) {
    if ($line -match '^##\s+(Now|Next|Later)\s*$') { $section = $Matches[1]; continue }
    if ($line -match '^-\s+\[\s\]\s+(TASK-[A-Za-z0-9-]+)') {
      $taskId = $Matches[1]
      $taskPath = Join-Path "handoff/tasks" ($taskId + ".md")
      $created = $null
      if (Test-Path -LiteralPath $taskPath) {
        $createdMatch = Select-String -LiteralPath $taskPath -Pattern '^\|\s*Created\s*\|\s*(\d{4}-\d{2}-\d{2})' -List
        if ($createdMatch) { $created = [datetime]::ParseExact($createdMatch.Matches[0].Groups[1].Value, 'yyyy-MM-dd', $null) }
      }
      $row = [pscustomobject]@{ ID = $taskId; Section = $section; Created = $created }
      $openRows += $row
      if ($section -eq "Now") { $nowRows += $row }
    }
  }
}
$oldest = $openRows | Where-Object { $null -ne $_.Created } | Sort-Object Created | Select-Object -First 1
if ($oldest) {
  Write-Output ("2. Debt age: oldest open task " + $oldest.ID + " at " + (Get-AgeDays $oldest.Created) + " days")
} else {
  Write-Output "2. Debt age: no dated open task"
}
$oldestNow = $nowRows | Where-Object { $null -ne $_.Created } | Sort-Object Created | Select-Object -First 1
if ($oldestNow) {
  Write-Output ("3. Queue age: oldest Now row " + $oldestNow.ID + " at " + (Get-AgeDays $oldestNow.Created) + " days; " + $nowRows.Count + " open Now rows")
} else {
  Write-Output ("3. Queue age: no dated Now row; " + $nowRows.Count + " open Now rows")
}
Write-Output ""

# 4. Latest gate evidence.
$gateLog = Get-ChildItem -LiteralPath "handoff/evidence" -Recurse -Filter "gate*.txt" -File -ErrorAction SilentlyContinue |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($gateLog) {
  $verdicts = Select-String -LiteralPath $gateLog.FullName -Pattern 'GATE (PASS|FAIL) \(cardless\)'
  $verdictText = "no final verdict line"
  if ($verdicts) {
    $lastVerdict = ($verdicts | Select-Object -Last 1).Line
    if ($lastVerdict -match 'GATE PASS') { $verdictText = "final verdict PASS" }
    else { $verdictText = "final verdict FAIL" }
  }
  Write-Output ("4. Gate failures: newest evidence " + $gateLog.FullName + " (" + $gateLog.LastWriteTime.ToString("yyyy-MM-dd HH:mm") + ") " + $verdictText)
} else {
  Write-Output "4. Gate failures: no gate evidence found"
}
Write-Output ""

# 5. Worktree age.
$worktrees = @()
$porcelain = git worktree list --porcelain 2>$null
$current = $null
foreach ($line in $porcelain) {
  if ($line -match '^worktree\s+(.+)$') { $current = $Matches[1]; continue }
  if ($line -eq '' -and $current) {
    $worktrees += $current
    $current = $null
  }
}
if ($current) { $worktrees += $current }
$staleWorktrees = @()
foreach ($wt in $worktrees) {
  if (-not (Test-Path -LiteralPath $wt)) { continue }
  $commitDate = git -C $wt log -1 --format=%cI 2>$null
  if ($LASTEXITCODE -ne 0 -or -not $commitDate) { continue }
  $lastActivity = [datetime]::Parse("$commitDate")
  $writes = @()
  $writes += (Get-ChildItem -LiteralPath $wt -File -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne '.git' } | ForEach-Object { $_.LastWriteTime })
  foreach ($dir in Get-ChildItem -LiteralPath $wt -Directory -Force -ErrorAction SilentlyContinue) {
    if ($dir.Name -eq '.git' -or $dir.Name -eq 'target') { continue }
    $writes += (Get-ChildItem -LiteralPath $dir.FullName -Recurse -File -Force -ErrorAction SilentlyContinue |
      ForEach-Object { $_.LastWriteTime })
  }
  $newest = ($writes | Measure-Object -Maximum).Maximum
  if ($null -ne $newest -and $newest -gt $lastActivity) { $lastActivity = $newest }
  $age = [math]::Floor(((Get-Date) - $lastActivity).TotalDays)
  if ($age -gt 7) { $staleWorktrees += ($wt + " (" + $age + " days)") }
}
Write-Output ("5. Worktrees untouched for more than 7 days: " + $staleWorktrees.Count + " of " + $worktrees.Count + " (age = last commit or newest file write; .git and target excluded)")
foreach ($item in $staleWorktrees) { Write-Output ("   - " + $item) }

Write-Output ""
Write-Output "END PROCESS HEALTH REPORT"
exit 0
