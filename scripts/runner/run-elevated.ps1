<#
.SYNOPSIS
  Ask the installed admin runner to run a project script elevated, wait, and
  show the result. Runs WITHOUT elevation (that is the point).

.EXAMPLE
  .\scripts\runner\run-elevated.ps1 live-test.ps1
  .\scripts\runner\run-elevated.ps1 live-test.ps1 -ScriptArgs '-Volumes','C','-TestDir','C:\Temp\x'
  .\scripts\runner\run-elevated.ps1 runner-selftest.ps1

  Exit code is the script's own; 125 = refused by the runner, 124 = no answer in time.
#>
param(
    [Parameter(Mandatory, Position = 0)][string]$Script,
    [string[]]$ScriptArgs = @(),
    [string]$Root = 'C:\Sandbox\Search',
    [string]$TaskName = 'SearchRunner',
    [int]$TimeoutSec = 2400,
    [int]$TailLines = 40
)

$ErrorActionPreference = 'Stop'
$Root = [IO.Path]::GetFullPath($Root).TrimEnd('\')
# A bare name means "the scripts folder next to the one this helper lives in".
$scriptPath = if ([IO.Path]::IsPathRooted($Script)) { $Script } else { Join-Path (Split-Path $PSScriptRoot -Parent) $Script }
$scriptPath = [IO.Path]::GetFullPath($scriptPath)

$reqDir = Join-Path $Root '.runner\requests'
if (-not (Test-Path -LiteralPath $reqDir)) { Write-Error "Admin runner is not set up ($reqDir missing). Run setup-admin-runner.ps1 once, as admin."; exit 126 }

$id = '{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), (-join ((1..6) | ForEach-Object { '{0:x}' -f (Get-Random -Maximum 16) }))
$tmp = Join-Path $reqDir "$id.tmp"
@{ script = $scriptPath; args = @($ScriptArgs) } | ConvertTo-Json | Set-Content -Path $tmp -Encoding UTF8
Rename-Item -LiteralPath $tmp -NewName "$id.json"   # appears whole or not at all

& schtasks.exe /Run /TN $TaskName | Out-Null
if ($LASTEXITCODE -ne 0) {
    # "already running" is fine: the wrapper drains the queue before it exits.
    Write-Host "(schtasks /Run returned $LASTEXITCODE; waiting in case the runner is already busy)"
}

$status = Join-Path $Root "runner-results\$id\status.json"
$t = Get-Date
$s = $null
while (((Get-Date) - $t).TotalSeconds -lt $TimeoutSec) {
    if (Test-Path -LiteralPath $status) {
        $s = Get-Content -LiteralPath $status -Raw | ConvertFrom-Json
        if ($s.state -notin 'running') { break }
    }
    Start-Sleep -Milliseconds 500
}

if (-not $s -or $s.state -eq 'running') {
    Write-Host "No answer after $TimeoutSec s (request $id). Check C:\ProgramData\SearchRunner\runs.log and Task Scheduler."
    exit 124
}
Write-Host "request $id : $($s.state)$(if ($s.state -eq 'rejected') { ' - ' + $s.reason } else { '  exit ' + $s.exitCode })"
if ($s.output -and (Test-Path -LiteralPath $s.output)) {
    Write-Host "output : $($s.output)"
    Write-Host '--- last lines ---'
    Get-Content -LiteralPath $s.output -Tail $TailLines
}
if ($s.state -eq 'rejected') { exit 125 }
if ($s.state -eq 'timeout') { exit 124 }
exit ([int]$s.exitCode)
