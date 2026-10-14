<#
.SYNOPSIS
  Prove step 2 on a real NTFS volume: live USN updates and instant restart.
  Self-elevates (raw volume access needs it).

.DESCRIPTION
  Everything runs against a private snapshot folder (bench-results\live-snap),
  so your real snapshots are untouched. The only things written to disk are
  that snapshot and a few tiny files in -TestDir, all deleted at the end. The
  USN journal is only read, never created or changed.

  1. COLD     full MFT scan, save snapshot, then create / rename / delete files
              and time how long search takes to notice each change (must be < 1 s)
  2. WARM     restart from the snapshot (no scan): prints the ready time, then
              runs the same live test again
  3. CATCH-UP create/rename/delete files while nothing is running, restart, and
              check the journal replay brought the index up to date
  4. RESTARTS three more cold-process restarts, timed from outside

  Output goes to bench-results\live-<timestamp>.txt (and the console).

.EXAMPLE
  .\scripts\live-test.ps1
  .\scripts\live-test.ps1 -Volumes C,E -TestDir E:\search-live-test
#>
param(
    [string[]]$Volumes = @('C'),
    [string]$TestDir = (Join-Path $env:TEMP 'search-live-test')
)

$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-Volumes', ($Volumes -join ','), '-TestDir', "`"$TestDir`"")
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait
    exit
}

[Console]::OutputEncoding = [Text.Encoding]::UTF8
Set-Location $repo
New-Item -Force -ItemType Directory bench-results | Out-Null
$out = "bench-results\live-$(Get-Date -Format 'yyyyMMdd-HHmmss').txt"
$lines = New-Object System.Collections.Generic.List[string]
function Log([string]$s) { Write-Host $s; $lines.Add($s) }

$snap = Join-Path $repo 'bench-results\live-snap'
if (Test-Path $snap) { Remove-Item -Recurse -Force $snap }
New-Item -ItemType Directory -Force $snap | Out-Null

cargo build --release
if ($LASTEXITCODE -ne 0) { if (-not $env:SEARCH_RUNNER) { Read-Host 'build failed, press Enter' }; exit 1 }
$exe = Join-Path (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory 'release\s.exe'

Log "# $(Get-Date -Format o)  host=$env:COMPUTERNAME  commit=$(git rev-parse --short HEAD)"
Log "# volumes=$($Volumes -join ',')  test dir=$TestDir  snapshots=$snap"

$volArgs = @()
foreach ($v in $Volumes) { $volArgs += @('-v', $v) }
$failures = New-Object System.Collections.Generic.List[string]

# Run s.exe, log everything, return the output lines.
function RunS([string[]]$sArgs) {
    Log ""
    Log "> s.exe $($sArgs -join ' ')"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $o = & $exe @sArgs 2>&1 | ForEach-Object { "$_" }
    $sw.Stop()
    $o | ForEach-Object { Log $_ }
    Log ("  (process wall time {0:N2} s, exit code {1})" -f $sw.Elapsed.TotalSeconds, $LASTEXITCODE)
    return ,@($o)
}

$base = @('--data-dir', $snap) + $volArgs

# 1. cold start + live test
Log "`n## 1. COLD: full scan, save snapshot, live create/rename/delete"
$o = RunS ($base + @('--rescan', '--live-test', $TestDir))
if ($LASTEXITCODE -ne 0) { $failures.Add('cold live test') }
if (-not (Test-Path (Join-Path $snap '*.snap'))) { $failures.Add('no snapshot was written') }

# 2. warm restart + live test
Log "`n## 2. WARM: restart from the snapshot (look for 'snapshot:' and 'caught up' lines), then live test again"
$o = RunS ($base + @('--live-test', $TestDir))
if ($LASTEXITCODE -ne 0) { $failures.Add('warm live test') }
if ($o -match 'rescanning|scanned .* entries') { $failures.Add('warm start rescanned instead of loading the snapshot') }

# 3. catch-up: change things while nothing is running
Log "`n## 3. CATCH-UP: change files while s.exe is NOT running, then restart"
New-Item -Force -ItemType Directory $TestDir | Out-Null
$id = 'catchup' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$keep = Join-Path $TestDir "$id-keep-before.txt"
$gone = Join-Path $TestDir "$id-gone.txt"
$dirA = Join-Path $TestDir "$id-folder-before"
Set-Content $keep 'x'; Set-Content $gone 'x'
New-Item -ItemType Directory $dirA | Out-Null; Set-Content (Join-Path $dirA "$id-inside.txt") 'x'
Rename-Item $keep "$id-keep-after.txt"
Remove-Item $gone
Rename-Item $dirA "$id-folder-after"
Log "  changed: created keep+gone+folder, renamed keep, deleted gone, renamed folder (token $id)"
$o = RunS ($base + @($id))
$text = ($o -join "`n")
$checks = @(
    @{ name = 'renamed file found under new name'; ok = ($text -match "$id-keep-after\.txt") },
    @{ name = 'old file name is gone';             ok = ($text -notmatch "$id-keep-before") },
    @{ name = 'deleted file is gone';              ok = ($text -notmatch "$id-gone") },
    @{ name = 'file inside renamed folder follows'; ok = ($text -match "$id-folder-after\\$id-inside\.txt") },
    @{ name = 'old folder name is gone';           ok = ($text -notmatch "$id-folder-before") },
    @{ name = 'journal replayed (no rescan)';      ok = ($text -notmatch 'rescanning|scanned .* entries') }
)
foreach ($c in $checks) {
    Log ("  {0,-4} {1}" -f $(if ($c.ok) { 'ok' } else { 'FAIL' }), $c.name)
    if (-not $c.ok) { $failures.Add("catch-up: $($c.name)") }
}

# 4. restart timing from the outside
Log "`n## 4. RESTART x3 (process start to answered query)"
foreach ($i in 1..3) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $o = & $exe @($base + @('--quiet', '--no-count', 'notepad')) 2>&1 | ForEach-Object { "$_" }
    $sw.Stop()
    $ready = ($o | Where-Object { $_ -match '^ready:' }) -join ''
    Log ("  run {0}: {1:N2} s wall   {2}" -f $i, $sw.Elapsed.TotalSeconds, $ready)
}

# cleanup
if (Test-Path $TestDir) { Remove-Item -Recurse -Force $TestDir -ErrorAction SilentlyContinue }

Log ""
if ($failures.Count -eq 0) { Log "RESULT: PASS" } else { Log "RESULT: FAIL"; $failures | ForEach-Object { Log "  - $_" } }
Set-Content -Path $out -Value $lines -Encoding UTF8
Write-Host "`nSaved to $((Resolve-Path $out).Path)"
if (-not $env:SEARCH_RUNNER) { Read-Host 'Press Enter to close' }   # the admin runner sets SEARCH_RUNNER: no console to wait on
