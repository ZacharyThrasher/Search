<#
.SYNOPSIS
  Prove step 4 against the real NTFS journal: edit files and watch them appear
  in, move within, and vanish from the content index. Self-elevates (the helper
  needs raw volume access).

.DESCRIPTION
  Everything runs privately: its own helper pipe (tag "ctest"), snapshots, content
  index and settings in a temp folder. Your installed helper, popup and index are
  not touched. Test files live in -TestDir (default: a folder under the system
  temp folder on C:), all deleted at the end.

    1. starts a helper (searchd, elevated) and an indexer (searchi) against each other
    2. creates a file                       -> its words become searchable
    3. rewrites it                          -> new words in, old words out
    4. renames it, then renames its folder  -> found under the new name only
    5. deletes it                           -> gone
    6. a PDF-less Office check: a .docx built on the fly is read in a child process

  Prints the delay of each step (save -> searchable) and exits 0 only if all passed.
  Output also goes to bench-results\content-live-<timestamp>.txt.

.EXAMPLE
  .\scripts\content-live-test.ps1
  .\scripts\content-live-test.ps1 -TestDir D:\search-content-test -Volume D
#>
param(
    [string]$Volume = 'C',
    [string]$TestDir = (Join-Path $env:TEMP 'search-content-live-test'),
    [int]$TimeoutSec = 40
)

$repo = Split-Path -Parent $PSScriptRoot
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-Volume', $Volume, '-TestDir', "`"$TestDir`"", '-TimeoutSec', $TimeoutSec)
    $p = Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $p.ExitCode
}

[Console]::OutputEncoding = [Text.Encoding]::UTF8
Set-Location $repo
New-Item -Force -ItemType Directory bench-results | Out-Null
$out = "bench-results\content-live-$(Get-Date -Format 'yyyyMMdd-HHmmss').txt"
$lines = New-Object System.Collections.Generic.List[string]
function Log([string]$s) { Write-Host $s; $lines.Add($s) }

cargo build --release
if ($LASTEXITCODE -ne 0) { if (-not $env:SEARCH_RUNNER) { Read-Host 'build failed, press Enter' }; exit 1 }
$target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
$bin = Join-Path $target 'release'

$work = Join-Path $env:TEMP "search-content-live-$PID"
Remove-Item -Recurse -Force $work, $TestDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $work 'helper'), (Join-Path $work 'content'), $TestDir | Out-Null
# Index exactly what we ask for: no default folder exclusions (the temp folder is one of them).
Set-Content -Path (Join-Path $work 'config.toml') -Value "[content]`ndefault_excludes = false`nworkers = 1`n" -Encoding UTF8
$tag = 'ctest'

Log "# $(Get-Date -Format o)  host=$env:COMPUTERNAME  commit=$(git rev-parse --short HEAD)"
Log "# volume=$Volume  test dir=$TestDir  work=$work"

$helper = Start-Process -FilePath (Join-Path $bin 'searchd.exe') -WindowStyle Hidden -PassThru `
    -ArgumentList @('--pipe-tag', $tag, '--data-dir', (Join-Path $work 'helper'), '-v', $Volume)
& (Join-Path $bin 'searchq.exe') --pipe-tag $tag --status --wait 900 | Out-Null
if ($LASTEXITCODE -ne 0) { Log 'FAIL: the helper did not come up'; Stop-Process -Id $helper.Id -Force; exit 1 }
$indexer = Start-Process -FilePath (Join-Path $bin 'searchi.exe') -WindowStyle Hidden -PassThru `
    -ArgumentList @('--pipe-tag', $tag, '--dir', (Join-Path $work 'content'), '--config', (Join-Path $work 'config.toml'), '--no-pause')

$failed = 0
function Search([string]$q) {
    $o = & (Join-Path $bin 'searchq.exe') --content --content-dir (Join-Path $work 'content') $q 2>&1
    return @($o | Where-Object { $_ -match '^\s+[\d.]+\s+\S+:' } | ForEach-Object { ($_ -replace '^\s+[\d.]+\s+', '') -replace ' \(page \d+\)$', '' })
}
# Wait until a search for $q returns exactly the paths in $want; report the delay.
function Expect([string]$what, [string]$q, [string[]]$want, [datetime]$since) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $got = Search $q
        if ((($got | Sort-Object) -join '|') -eq (($want | Sort-Object) -join '|')) {
            $ms = [int]((Get-Date) - $since).TotalMilliseconds
            Log ("  ok   {0,-46} {1,6} ms" -f $what, $ms)
            return
        }
        Start-Sleep -Milliseconds 250
    }
    Log "  FAIL $what : searching '$q' wanted [$($want -join ', ')] got [$((Search $q) -join ', ')]"
    $script:failed++
}

# Wait for the indexer to subscribe and settle its first look at the (empty) test folder.
$status = Join-Path $work 'content\status.json'
$t0 = Get-Date
while (((Get-Date) - $t0).TotalSeconds -lt 120) {
    if (Test-Path $status) { $s = Get-Content $status -Raw | ConvertFrom-Json; if ($s.phase -eq 'idle') { break } }
    Start-Sleep -Milliseconds 500
}
Log "indexer ready after $([int]((Get-Date) - $t0).TotalSeconds) s"

$w1 = 'zqxaardvark' + (Get-Random -Maximum 99999)
$w2 = 'zqxplatypus' + (Get-Random -Maximum 99999)
$w3 = 'zqxwombat' + (Get-Random -Maximum 99999)
$f = Join-Path $TestDir 'notes.txt'

Log 'create, rewrite, rename, move, delete:'
$t = Get-Date; Set-Content -Path $f -Value "meeting notes about the $w1 project" -Encoding UTF8
Expect 'new file is searchable' $w1 @($f) $t

$t = Get-Date; Set-Content -Path $f -Value "meeting notes about the $w2 project" -Encoding UTF8
Expect 'rewritten file: new word found' $w2 @($f) $t
Expect 'rewritten file: old word gone' $w1 @() $t

$f2 = Join-Path $TestDir 'renamed.txt'
$t = Get-Date; Rename-Item -LiteralPath $f -NewName 'renamed.txt'
Expect 'renamed file found under the new name' $w2 @($f2) $t

$sub = Join-Path $TestDir 'folder-a'
New-Item -ItemType Directory $sub | Out-Null
$f3 = Join-Path $sub 'inner.txt'
Set-Content -Path $f3 -Value "inner file about a $w3" -Encoding UTF8
Expect 'file in a new folder' $w3 @($f3) (Get-Date)
$t = Get-Date; Rename-Item -LiteralPath $sub -NewName 'folder-b'
$f3b = Join-Path (Join-Path $TestDir 'folder-b') 'inner.txt'
Expect 'folder rename: found under the new path' $w3 @($f3b) $t

$t = Get-Date; Remove-Item -LiteralPath $f2
Expect 'deleted file is gone' $w2 @() $t
$t = Get-Date; Remove-Item -Recurse -LiteralPath (Join-Path $TestDir 'folder-b')
Expect 'deleted folder: its files are gone' $w3 @() $t

# An Office file, built on the fly.
$w4 = 'zqxokapi' + (Get-Random -Maximum 99999)
Add-Type -AssemblyName System.IO.Compression.FileSystem
$docx = Join-Path $TestDir 'contract.docx'
$zip = [IO.Compression.ZipFile]::Open($docx, 'Create')
$entry = $zip.CreateEntry('word/document.xml')
$sw = New-Object IO.StreamWriter($entry.Open())
$sw.Write("<w:document xmlns:w=`"x`"><w:body><w:p><w:r><w:t>agreement about the $w4 merger</w:t></w:r></w:p></w:body></w:document>")
$sw.Dispose(); $zip.Dispose()
Expect 'docx is read and searchable' $w4 @($docx) (Get-Date)

Log ''
$prog = Get-Content $status -Raw | ConvertFrom-Json
Log "indexer: $($prog.indexed) files indexed, $($prog.passages) passages, phase '$($prog.phase)'"
Log ($(if ($failed -eq 0) { 'ALL PASSED' } else { "$failed STEP(S) FAILED" }))

& (Join-Path $bin 'searchi.exe') --stop --dir (Join-Path $work 'content') | Out-Null
Start-Sleep -Seconds 2
Stop-Process -Id $indexer.Id -Force -ErrorAction SilentlyContinue
& (Join-Path $bin 'searchq.exe') --pipe-tag $tag --shutdown | Out-Null
Start-Sleep -Seconds 1
Stop-Process -Id $helper.Id -Force -ErrorAction SilentlyContinue
Get-Process searchi -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$bin*" } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $work, $TestDir -ErrorAction SilentlyContinue
$lines | Set-Content $out -Encoding UTF8
if (-not $env:SEARCH_RUNNER) { Read-Host 'Press Enter to close' | Out-Null }
exit $failed
