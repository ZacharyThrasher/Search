<#
.SYNOPSIS
  Remove what install.ps1 set up: the SearchHelper task, the files in
  C:\Program Files\Search, the popup autostart and its copy of the exe.
  Asks for admin once. The helper's snapshots and log go with
  C:\Program Files\Search; the popup's config and log in %LOCALAPPDATA%\Search
  are kept unless you pass -RemoveData.
#>
param(
    [switch]$RemoveData,
    [switch]$NoPause,
    [switch]$HelperStep
)

$ErrorActionPreference = 'Stop'
$taskName = 'SearchHelper'
$progDir = Join-Path $env:ProgramFiles 'Search'
$popDir = Join-Path $env:LOCALAPPDATA 'Programs\Search'

if ($HelperStep) {
    try {
        $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if (-not $isAdmin) { throw 'not elevated' }
        Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue | Stop-ScheduledTask -ErrorAction SilentlyContinue
        Get-Process searchd -ErrorAction SilentlyContinue | Stop-Process -Force
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        if (Test-Path $progDir) { Remove-Item -Recurse -Force $progDir }
        Write-Host 'Helper task and files removed.'
        exit 0
    } catch {
        Write-Host "FAILED: $($_.Exception.Message)" -ForegroundColor Red
        Read-Host 'Press Enter to close' | Out-Null
        exit 1
    }
}

try {
    Get-Process searchpop -ErrorAction SilentlyContinue | Stop-Process -Force
    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SearchPopup' -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    if (Test-Path $popDir) { Remove-Item -Recurse -Force $popDir }

    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-HelperStep')
    $p = Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw 'the helper step did not finish' }

    if ($RemoveData) {
        $data = Join-Path $env:LOCALAPPDATA 'Search'
        if (Test-Path $data) { Remove-Item -Recurse -Force $data }
        Write-Host "Removed $data"
    }
    Write-Host 'Uninstalled.'
} catch {
    Write-Host "UNINSTALL FAILED: $($_.Exception.Message)" -ForegroundColor Red
}
if (-not $NoPause) { Read-Host 'Press Enter to close' | Out-Null }
