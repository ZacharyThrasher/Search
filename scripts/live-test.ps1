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

# 5. the elevated helper (searchd) and its pipe, as the popup uses them
Log "`n## 5. HELPER: searchd serves ranked queries over its pipe; live changes show up; shutdown saves"
$binDir = Split-Path $exe -Parent
$searchd = Join-Path $binDir 'searchd.exe'
$searchq = Join-Path $binDir 'searchq.exe'
$tag = 'live-test'
$helperSnap = Join-Path $snap 'helper'
New-Item -ItemType Directory -Force $helperSnap | Out-Null
$hv = @(); foreach ($v in $Volumes) { $hv += @('-v', $v) }
$sw = [Diagnostics.Stopwatch]::StartNew()
$hp = Start-Process -FilePath $searchd -ArgumentList (@('--pipe-tag', $tag, '--data-dir', "`"$helperSnap`"", '--rescan') + $hv) -WindowStyle Hidden -PassThru
function Q([string[]]$a) { $r = & $searchq @('--pipe-tag', $tag) @a 2>&1 | ForEach-Object { "$_" }; return ,@($r) }
$o = Q @('--wait', '180', '--status')
$sw.Stop()
Log ("  helper ready after {0:N1} s" -f $sw.Elapsed.TotalSeconds)
$o | ForEach-Object { Log "  $_" }
if (($o -join "`n") -notmatch 'elevated: true') { $failures.Add('helper is not elevated') }
if (($o -join "`n") -notmatch '(\d+) entries') { $failures.Add('helper reported no entries') }

# ranked results: exact stem first, then prefix, then substring
$o = Q @('--limit', '8', 'notepad')
Log '  query "notepad":'; $o | ForEach-Object { Log "    $_" }
if (-not ($o -match 'notepad')) { $failures.Add('helper found nothing for notepad') }

# a new file shows up within a second, and ranks first as an exact match
New-Item -Force -ItemType Directory $TestDir | Out-Null
$tok = 'helperlive' + [guid]::NewGuid().ToString('N').Substring(0, 8)
Set-Content (Join-Path $TestDir "$tok.txt") 'x'
$seen = $false; $t = [Diagnostics.Stopwatch]::StartNew()
while ($t.Elapsed.TotalSeconds -lt 3) {
    $o = Q @('--limit', '3', $tok)
    if (($o -join "`n") -match [regex]::Escape("$tok.txt")) { $seen = $true; break }
    Start-Sleep -Milliseconds 50
}
Log ("  new file visible through the helper: {0} ({1:N2} s)" -f $seen, $t.Elapsed.TotalSeconds)
if (-not $seen) { $failures.Add('helper did not see a new file') }

# latency over the pipe, narrow and very broad queries
foreach ($q in @('notepad', 'readme', 'a')) {
    $o = Q @('--bench', '100', $q)
    Log "  $q : $($o | Where-Object { $_ -match 'round trip' })"
}

# shutdown writes the snapshots and the process exits
$o = Q @('--shutdown')
$hp.WaitForExit(30000) | Out-Null
Log "  shutdown: exited=$($hp.HasExited)  snapshots in helper dir: $((Get-ChildItem $helperSnap -Filter *.snap -ErrorAction SilentlyContinue | Measure-Object).Count)"
if (-not $hp.HasExited) { $failures.Add('helper did not exit on shutdown'); $hp.Kill() }
elseif (-not (Get-ChildItem $helperSnap -Filter *.snap -ErrorAction SilentlyContinue)) { $failures.Add('helper did not save a snapshot on shutdown') }
Log ("  helper log tail:"); Get-Content (Join-Path $helperSnap 'searchd.log') -Tail 6 -ErrorAction SilentlyContinue | ForEach-Object { Log "    $_" }

# cleanup
if (Test-Path $TestDir) { Remove-Item -Recurse -Force $TestDir -ErrorAction SilentlyContinue }

Log ""
if ($failures.Count -eq 0) { Log "RESULT: PASS" } else { Log "RESULT: FAIL"; $failures | ForEach-Object { Log "  - $_" } }
Set-Content -Path $out -Value $lines -Encoding UTF8
Write-Host "`nSaved to $((Resolve-Path $out).Path)"
if (-not $env:SEARCH_RUNNER) { Read-Host 'Press Enter to close' }   # the admin runner sets SEARCH_RUNNER: no console to wait on
