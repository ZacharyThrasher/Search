<#
.SYNOPSIS
  Install the hotkey popup and its elevated index helper. Run from a normal
  PowerShell; it asks for admin once (a single UAC prompt) for the helper part.

.DESCRIPTION
  Two pieces, because raw volume access needs admin but the popup must not
  have it (whatever it launches would inherit it):

    searchd.exe  (elevated)   owns the in-RAM index and the USN tail, answers
                              the popup over a per-user named pipe.
      - copied to  C:\Program Files\Search\  (only admins can write there, so
        an unprivileged process cannot swap it for something else)
      - run by a scheduled task "SearchHelper": at YOUR logon, highest
        privileges, hidden, restarted if it dies. You may start/stop the
        task; only admins may change it.

    searchpop.exe (normal)    the window. Copied to
        %LOCALAPPDATA%\Programs\Search\ and started at logon via
        HKCU\...\Run.

  To update after a rebuild, run this script again (it stops both programs,
  replaces the files, and restarts them). Remove everything with uninstall.ps1.
  The helper keeps its snapshots and log in C:\Program Files\Search\data (admin-write
  only, readable by you); the popup keeps its config and log in %LOCALAPPDATA%\Search.

.EXAMPLE
  .\scripts\install.ps1
  .\scripts\install.ps1 -NoBuild      # use the binaries already in target\release
#>
param(
    [switch]$NoBuild,
    [switch]$NoPause,
    # Internal: the elevated half.
    [switch]$HelperStep,
    [string]$Src,
    [string]$User
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$taskName = 'SearchHelper'
$progDir = Join-Path $env:ProgramFiles 'Search'
$popDir = Join-Path $env:LOCALAPPDATA 'Programs\Search'

function Pause-IfInteractive { if (-not $NoPause -and -not $env:SEARCH_RUNNER) { Read-Host 'Press Enter to close' | Out-Null } }

# ---------------------------------------------------------------- elevated half
if ($HelperStep) {
    try {
        $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if (-not $isAdmin) { throw 'not elevated' }
        $userSid = (New-Object Security.Principal.NTAccount($User)).Translate([Security.Principal.SecurityIdentifier]).Value

        # Stop any running helper before replacing its file.
        Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue | Stop-ScheduledTask -ErrorAction SilentlyContinue
        Get-Process searchd -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep -Milliseconds 500

        New-Item -ItemType Directory -Force $progDir | Out-Null
        Copy-Item -LiteralPath (Join-Path $Src 'searchd.exe') -Destination (Join-Path $progDir 'searchd.exe') -Force
        Copy-Item -LiteralPath (Join-Path $Src 'searchq.exe') -Destination (Join-Path $progDir 'searchq.exe') -Force
        # Program Files is admin-write by default; make sure nothing weaker was inherited.
        & icacls.exe $progDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'could not lock down the program folder' }

        # Snapshots and the helper's log go in a subfolder of it. The helper is elevated, so it
        # must never write where a normal process could plant a link (junction, symlink, hard
        # link) to redirect the write; this folder only admins can change. Only you and admins
        # can read it, because the snapshots list every file name on the machine.
        $dataDir = Join-Path $progDir 'data'
        New-Item -ItemType Directory -Force $dataDir | Out-Null
        & icacls.exe $dataDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${userSid}:(OI)(CI)RX" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'could not lock down the data folder' }

        $action = New-ScheduledTaskAction -Execute (Join-Path $progDir 'searchd.exe')
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $User
        $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) `
            -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -StartWhenAvailable
        Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force `
            -Description 'Elevated index helper for the Search popup. Replace the exe by re-running scripts\install.ps1.' | Out-Null

        # You (and admins) may start and stop it; only admins may change it.
        $svc = New-Object -ComObject 'Schedule.Service'
        $svc.Connect()
        $svc.GetFolder('\').GetTask($taskName).SetSecurityDescriptor("D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;$userSid)", 0)

        Start-ScheduledTask -TaskName $taskName
        Write-Host "Helper installed in $progDir and started."
        exit 0
    } catch {
        Write-Host "HELPER SETUP FAILED: $($_.Exception.Message)" -ForegroundColor Red
        Read-Host 'Press Enter to close' | Out-Null
        exit 1
    }
}

# ------------------------------------------------------------- normal half
try {
    $user = "$env:USERDOMAIN\$env:USERNAME"
    $release = Join-Path $repo 'target\release'
    if (-not $NoBuild) {
        Push-Location $repo
        cargo build --release
        $rc = $LASTEXITCODE
        Pop-Location
        if ($rc -ne 0) { throw 'build failed' }
    }
    foreach ($f in 'searchd.exe', 'searchq.exe', 'searchpop.exe') {
        if (-not (Test-Path (Join-Path $release $f))) { throw "$f not found in $release (build first)" }
    }

    # Popup: stop, copy, autostart, start.
    Get-Process searchpop -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300
    New-Item -ItemType Directory -Force $popDir | Out-Null
    Copy-Item -LiteralPath (Join-Path $release 'searchpop.exe') -Destination (Join-Path $popDir 'searchpop.exe') -Force
    Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SearchPopup' -Value ('"{0}"' -f (Join-Path $popDir 'searchpop.exe'))

    # Helper: one UAC prompt.
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-HelperStep',
        '-Src', "`"$release`"", '-User', "`"$user`"")
    $p = Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw 'the helper step did not finish (was the UAC prompt declined?)' }

    Start-Process -FilePath (Join-Path $popDir 'searchpop.exe')
    Write-Host ''
    Write-Host 'Done. Press Alt+Space (change it in %LOCALAPPDATA%\Search\config.toml).'
    Write-Host 'First start scans the drives once (about 10-30 s); the popup says so until it is ready.'
} catch {
    Write-Host "SETUP FAILED: $($_.Exception.Message)" -ForegroundColor Red
}
Pause-IfInteractive
