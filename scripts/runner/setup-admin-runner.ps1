<#
.SYNOPSIS
  One-time setup of the "admin runner": lets Claude threads run a short list
  of project scripts with admin rights, without a UAC prompt each time.
  Run it ONCE, from a normal PowerShell (it asks for admin itself).

.DESCRIPTION
  What it creates (nothing else on the system is touched):

    1. C:\ProgramData\SearchRunner\       editable by administrators only
         runner-wrapper.ps1               the code the task runs (a copy: later
                                          edits in the repo do NOT change it)
         runs.log                         one line per request, with script hash
    2. A scheduled task "SearchRunner"    runs as YOU, "highest privileges",
                                          NO triggers (only starts when asked),
                                          1 hour limit, never two at once.
                                          You may run it; only admins may change it.
    3. <Root>\.runner\requests and <Root>\runner-results  (queue and output)

  What the task will run, and only this: the scripts live-test.ps1,
  bench-mft.ps1 and runner-selftest.ps1, from a "scripts" folder under -Root
  (default C:\Sandbox\Search, which includes its git worktrees), with plain
  arguments. Anything else in a request is refused and logged.

  Be clear about the trade-off: those scripts are ordinary files in a folder
  Claude can edit, so Claude can in effect run admin code that lives there.
  The runner cannot be pointed at other folders, other scripts, or arbitrary
  commands. Review runs.log any time; remove everything with
  uninstall-admin-runner.ps1.

.EXAMPLE
  .\scripts\runner\setup-admin-runner.ps1
  .\scripts\runner\setup-admin-runner.ps1 -Root D:\Dev\Search
#>
param(
    [string]$Root = 'C:\Sandbox\Search',
    [string]$TaskName = 'SearchRunner',
    # Account allowed to start the task and write requests (default: you).
    [string]$User = "$env:USERDOMAIN\$env:USERNAME"
)

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-Root', "`"$Root`"", '-TaskName', "`"$TaskName`"", '-User', "`"$User`"")
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait
    exit
}

$ErrorActionPreference = 'Stop'
try {
    $Root = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    if (-not (Test-Path -LiteralPath $Root -PathType Container)) { throw "Root folder not found: $Root" }
    $src = Join-Path $PSScriptRoot 'runner-wrapper.ps1'
    if (-not (Test-Path -LiteralPath $src)) { throw "runner-wrapper.ps1 not found next to this script" }
    $userSid = (New-Object Security.Principal.NTAccount($User)).Translate([Security.Principal.SecurityIdentifier]).Value

    # 1. Admin-only program folder, readable by everyone (so the log can be checked).
    $prog = Join-Path $env:ProgramData 'SearchRunner'
    New-Item -ItemType Directory -Force $prog | Out-Null
    & icacls.exe $prog /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'could not lock down the program folder' }
    $dest = Join-Path $prog 'runner-wrapper.ps1'
    Copy-Item -LiteralPath $src -Destination $dest -Force
    Add-Content -Path (Join-Path $prog 'runs.log') -Value ("{0}  SETUP     root='{1}'  user='{2}'  wrapper sha256={3}" -f (Get-Date -Format o), $Root, $User, (Get-FileHash $dest).Hash)

    # 2. Queue and results folders, writable by you.
    foreach ($d in @("$Root\.runner\requests", "$Root\runner-results")) {
        New-Item -ItemType Directory -Force $d | Out-Null
        & icacls.exe $d /grant "*${userSid}:(OI)(CI)M" | Out-Null
    }

    # 3. The task: on demand only, as you, elevated.
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$dest`" -Root `"$Root`""
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Hours 1) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Settings $settings -Force `
        -Description 'On-demand elevated runner for Search project scripts. See C:\ProgramData\SearchRunner\runs.log.' | Out-Null

    # You (and nobody else but admins) may start it; only admins may change it.
    $svc = New-Object -ComObject 'Schedule.Service'
    $svc.Connect()
    $task = $svc.GetFolder('\').GetTask($TaskName)
    $task.SetSecurityDescriptor("D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;$userSid)", 0)

    Write-Host ""
    Write-Host "Done. Task '$TaskName' is installed and has no triggers."
    Write-Host "  program folder : $prog"
    Write-Host "  requests/results: $Root\.runner , $Root\runner-results"
    Write-Host "  audit log       : $prog\runs.log"
    Write-Host ""
    Write-Host "Tell Claude it is done. Remove it any time with scripts\runner\uninstall-admin-runner.ps1"
} catch {
    Write-Host "SETUP FAILED: $($_.Exception.Message)" -ForegroundColor Red
}
Read-Host 'Press Enter to close'
