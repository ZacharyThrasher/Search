<#
.SYNOPSIS
  Remove the admin runner installed by setup-admin-runner.ps1. Self-elevates.

.DESCRIPTION
  Deletes the "SearchRunner" scheduled task and the copied wrapper. The audit
  log (C:\ProgramData\SearchRunner\runs.log) is kept so you can still see what
  ran; add -Purge to delete it and the queue/results folders too.

.EXAMPLE
  .\scripts\runner\uninstall-admin-runner.ps1
  .\scripts\runner\uninstall-admin-runner.ps1 -Purge
#>
param(
    [string]$Root = 'C:\Sandbox\Search',
    [string]$TaskName = 'SearchRunner',
    [switch]$Purge
)

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-Root', "`"$Root`"", '-TaskName', "`"$TaskName`"")
    if ($Purge) { $argList += '-Purge' }
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait
    exit
}

$ErrorActionPreference = 'Stop'
try {
    $Root = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    $prog = Join-Path $env:ProgramData 'SearchRunner'

    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed scheduled task '$TaskName'."
    } else {
        Write-Host "No scheduled task '$TaskName' (already removed)."
    }

    if (Test-Path -LiteralPath $prog) {
        Add-Content -Path (Join-Path $prog 'runs.log') -Value ("{0}  UNINSTALL purge={1}" -f (Get-Date -Format o), [bool]$Purge)
        Remove-Item -LiteralPath (Join-Path $prog 'runner-wrapper.ps1') -Force -ErrorAction SilentlyContinue
        if ($Purge) {
            Remove-Item -LiteralPath $prog -Recurse -Force
            foreach ($d in @("$Root\.runner", "$Root\runner-results")) {
                if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
            }
            Write-Host 'Purged the program folder, log, queue and results.'
        } else {
            Write-Host "Kept the audit log: $prog\runs.log (use -Purge to delete it too)."
        }
    }
    Write-Host 'Done. Nothing can start elevated through this runner any more.'
} catch {
    Write-Host "UNINSTALL FAILED: $($_.Exception.Message)" -ForegroundColor Red
}
Read-Host 'Press Enter to close'
