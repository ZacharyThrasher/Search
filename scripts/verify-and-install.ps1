<#
.SYNOPSIS
  One step, one UAC prompt: check the content feed against the real NTFS
  journal, and only if that passes, install the new helper, popup and indexer.

.DESCRIPTION
  1. builds (normal user)
  2. ONE UAC prompt, then elevated:
       - scripts\content-live-test.ps1  (private helper pipe, private index; your
         installed helper and index are untouched)
       - if it passed: the elevated half of scripts\install.ps1 (helper files and
         the SearchHelper task, restart)
  3. back as your normal user: the popup and indexer are copied, autostarted and
     started (the rest of scripts\install.ps1), so neither runs elevated.

  If the live test fails nothing is installed. Output: bench-results\content-live-*.txt.

.EXAMPLE
  .\scripts\verify-and-install.ps1
  .\scripts\verify-and-install.ps1 -SkipLiveTest    # just install
#>
param(
    [switch]$NoBuild,
    [switch]$SkipLiveTest,
    # Internal: the elevated half.
    [switch]$Elevated,
    [string]$Src,
    [string]$User
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

if ($Elevated) {
    try {
        $env:SEARCH_RUNNER = '1'   # no "press Enter" pauses inside the child scripts
        if (-not $SkipLiveTest) {
            # -NoBuild: it was built a moment ago as your normal user; no build tools run elevated.
            & (Join-Path $PSScriptRoot 'content-live-test.ps1') -NoBuild
            if ($LASTEXITCODE -ne 0) { Write-Host 'LIVE TEST FAILED: nothing was installed.' -ForegroundColor Red; Read-Host 'Press Enter to close' | Out-Null; exit 2 }
        }
        & (Join-Path $PSScriptRoot 'install.ps1') -HelperStep -Src $Src -User $User
        exit $LASTEXITCODE
    } catch {
        Write-Host "FAILED: $($_.Exception.Message)" -ForegroundColor Red
        Read-Host 'Press Enter to close' | Out-Null
        exit 1
    }
}

# The popup and indexer are started by the second half of this script, so it must be a normal
# user's: from an administrator window they would start elevated (searchi refuses to).
if (([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Run this from a normal (non-administrator) PowerShell window. It asks for administrator rights itself, once, for the helper part only.' -ForegroundColor Red
    exit 1
}
Set-Location $repo
if (-not $NoBuild) {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Write-Host 'build failed' -ForegroundColor Red; exit 1 }
}
$target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
$release = Join-Path $target 'release'
$user = "$env:USERDOMAIN\$env:USERNAME"

$argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Elevated', '-NoBuild',
    '-Src', "`"$release`"", '-User', "`"$user`"")
if ($SkipLiveTest) { $argList += '-SkipLiveTest' }
$p = Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait -PassThru
if ($p.ExitCode -ne 0) { Write-Host "Stopped (exit $($p.ExitCode)); nothing more was installed." -ForegroundColor Red; exit $p.ExitCode }

# Normal user again: popup + indexer.
& (Join-Path $PSScriptRoot 'install.ps1') -NoBuild -SkipHelper -NoPause
