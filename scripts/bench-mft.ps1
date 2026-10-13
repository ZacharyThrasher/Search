<#
.SYNOPSIS
  Build the release binary and benchmark the NTFS MFT index. Self-elevates.

.DESCRIPTION
  Raw volume access needs an elevated process, so this script re-launches
  itself through a UAC prompt if needed. Output goes to
  bench-results\mft-<timestamp>.txt (and the console).

.EXAMPLE
  .\scripts\bench-mft.ps1
  .\scripts\bench-mft.ps1 -Volumes C,E
#>
param(
    [string[]]$Volumes = @(),
    [int]$Runs = 200
)

$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Runs', $Runs)
    if ($Volumes.Count) { $argList += @('-Volumes', ($Volumes -join ',')) }
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait
    exit
}

Set-Location $repo
New-Item -Force -ItemType Directory bench-results | Out-Null
$out = "bench-results\mft-$(Get-Date -Format 'yyyyMMdd-HHmmss').txt"

cargo build --release
if ($LASTEXITCODE -ne 0) { Read-Host 'build failed, press Enter'; exit 1 }
$exe = Join-Path (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory 'release\s.exe'

$volArgs = @()
foreach ($v in $Volumes) { $volArgs += @('--volume', $v) }

& {
    "# $(Get-Date -Format o)  host=$env:COMPUTERNAME"
    & $exe --source mft @volArgs --bench $Runs --queries-file benches\queries.txt 2>&1
} | Tee-Object -FilePath $out

Write-Host "`nSaved to $out"
Read-Host 'Press Enter to close'
