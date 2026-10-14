<#
.SYNOPSIS
  Build the release binary and benchmark the NTFS MFT index. Self-elevates.

.DESCRIPTION
  Raw volume access needs an elevated process, so this script re-launches
  itself through a UAC prompt if needed. Output goes to
  bench-results\mft-<timestamp>.txt (and the console).

  Sections:
    1. serial vs parallel MFT read on the small volumes (counts must agree)
    2. MFT read time per thread count on every volume
    3. full query benchmark with default settings

.EXAMPLE
  .\scripts\bench-mft.ps1
  .\scripts\bench-mft.ps1 -Volumes C,E
#>
param(
    [string[]]$Volumes = @(),
    [int]$Runs = 200,
    [switch]$Quick
)

$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Runs', $Runs)
    if ($Volumes.Count) { $argList += @('-Volumes', ($Volumes -join ',')) }
    if ($Quick) { $argList += '-Quick' }
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait
    exit
}

[Console]::OutputEncoding = [Text.Encoding]::UTF8
Set-Location $repo
New-Item -Force -ItemType Directory bench-results | Out-Null
$out = "bench-results\mft-$(Get-Date -Format 'yyyyMMdd-HHmmss').txt"
$lines = New-Object System.Collections.Generic.List[string]

function Log([string]$s) { Write-Host $s; $lines.Add($s) }

function RunS([string[]]$sArgs) {
    $sArgs = @('--no-snapshot') + $sArgs   # measure real scans, never a saved snapshot
    Log ""
    Log "> s.exe $($sArgs -join ' ')"
    & $exe @sArgs 2>&1 | ForEach-Object { Log ("$_") }
}

cargo build --release
if ($LASTEXITCODE -ne 0) { if (-not $env:SEARCH_RUNNER) { Read-Host 'build failed, press Enter' }; exit 1 }
$exe = Join-Path (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory 'release\s.exe'

Log "# $(Get-Date -Format o)  host=$env:COMPUTERNAME  commit=$(git rev-parse --short HEAD)"

$volArgs = @()
foreach ($v in $Volumes) { $volArgs += @('--volume', $v) }

# 1. serial vs parallel on the small volumes: entry counts should match
Log "`n## 1. serial vs parallel (E, G): entry counts must agree"
RunS (@('--source', 'mft', '-v', 'E', '-v', 'G', '--mft-threads', '1', '--quiet', 'notepad'))
RunS (@('--source', 'mft', '-v', 'E', '-v', 'G', '--mft-threads', '0', '--quiet', 'notepad'))

# 2. read time vs thread count
Log "`n## 2. MFT read time vs threads"
$counts = if ($Quick) { @(0) } else { @(4, 8, 16, 0) }
foreach ($n in $counts) {
    RunS (@('--source', 'mft') + $volArgs + @('--mft-threads', "$n", '--quiet', 'notepad'))
}

# 3. full benchmark
Log "`n## 3. query benchmark (default threads)"
RunS (@('--source', 'mft') + $volArgs + @('--bench', "$Runs", '--queries-file', 'benches\queries.txt'))

Set-Content -Path $out -Value $lines -Encoding UTF8
Write-Host "`nSaved to $out"
if (-not $env:SEARCH_RUNNER) { Read-Host 'Press Enter to close' }   # the admin runner sets SEARCH_RUNNER: no console to wait on
