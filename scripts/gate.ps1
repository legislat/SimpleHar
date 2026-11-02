# Cardless gate: the executable form of the command list in
# docs/process/VERIFICATION.md. PowerShell 5.1 compatible.
#
# The gate never starts an engine, never loads a model, never binds a port,
# and never calls a live harness. It runs the five commands in order, stops on
# the first failure, and prints PASS/FAIL per command plus the overall result.
# Counts are taken from the command output only; nothing is hardcoded.

$ErrorActionPreference = 'Continue'

$gate = @(
  @{ Name = 'cargo check --all-targets'; Exe = 'cargo'; CmdArgs = @('check', '--all-targets') },
  @{ Name = 'cargo test'; Exe = 'cargo'; CmdArgs = @('test') },
  @{ Name = 'cargo clippy --all-targets -- -D warnings'; Exe = 'cargo'; CmdArgs = @('clippy', '--all-targets', '--', '-D', 'warnings') },
  @{ Name = 'cargo fmt --check'; Exe = 'cargo'; CmdArgs = @('fmt', '--check') },
  @{ Name = 'node tests/webui/webui_dom_test.mjs'; Exe = 'node'; CmdArgs = @('tests/webui/webui_dom_test.mjs') }
)

function Show-Counts([string]$name, [string[]]$output) {
  # Print only counts the command itself printed. Never invent a count.
  if ($name -like 'node *') {
    $passed = @($output | Where-Object { $_ -match '^PASS ' }).Count
    $failed = @($output | Where-Object { $_ -match '^FAIL ' }).Count
    Write-Output ("COUNTS " + $name + ": " + $passed + " passed, " + $failed + " failed (from tool output)")
    return
  }
  foreach ($line in $output) {
    if ($line -match 'test result:') {
      Write-Output ("COUNTS " + $name + ": " + $line.Trim())
    }
  }
}

$overallPass = $true

foreach ($step in $gate) {
  if (-not (Get-Command $step.Exe -CommandType Application -ErrorAction SilentlyContinue)) {
    Write-Output ("FAIL " + $step.Name + " :: executable not found: " + $step.Exe)
    $overallPass = $false
    break
  }

  Write-Output ("RUN " + $step.Name)
  $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
  $argList = $step.CmdArgs
  $output = & $step.Exe @argList 2>&1 | ForEach-Object { $_.ToString() }
  $stopwatch.Stop()
  $code = $LASTEXITCODE
  $duration = [Math]::Round($stopwatch.Elapsed.TotalSeconds, 1)

  foreach ($line in $output) {
    Write-Output $line
  }

  if ($code -eq 0) {
    Show-Counts $step.Name $output
    Write-Output ("PASS " + $step.Name + " (exit 0, " + $duration + "s)")
  } else {
    Show-Counts $step.Name $output
    Write-Output ("FAIL " + $step.Name + " (exit " + $code + ", " + $duration + "s)")
    $overallPass = $false
    break
  }
}

if ($overallPass) {
  Write-Output 'GATE PASS (cardless)'
  exit 0
}

Write-Output 'GATE FAIL (cardless): stop on the first failed command, fix the cause, re-run.'
exit 1
