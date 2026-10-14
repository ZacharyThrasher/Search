<#
.SYNOPSIS
  The code the "SearchRunner" scheduled task runs (elevated). Installed by
  setup-admin-runner.ps1 into C:\ProgramData\SearchRunner, where only
  administrators can edit it.

.DESCRIPTION
  Drains the request queue in <Root>\.runner\requests. Each request is a JSON
  file naming one allow-listed script and its arguments. A request is run
  only if ALL of these hold, otherwise it is rejected and logged:

    - the script's file name is on the allow-list (-Allow)
    - it sits in a folder called "scripts" somewhere under -Root
    - no folder between -Root and the script is a junction/symlink
    - every argument is made of plain characters (no quotes, $, ;, |, &, <, >, `)

  It never runs anything else, never reads outside -Root, and never takes the
  Root or allow-list from a request: those are fixed in the task definition.

  Every run is appended to <LogDir>\runs.log (administrator-writable only)
  with the script's SHA-256 and git commit, and gets a folder
  <Root>\runner-results\<id>\ holding status.json and output.txt.
#>
param(
    [string]$Root = 'C:\Sandbox\Search',
    [string[]]$Allow = @('live-test.ps1', 'bench-mft.ps1', 'runner-selftest.ps1'),
    [string]$LogDir = (Join-Path $env:ProgramData 'SearchRunner'),
    [int]$TimeoutMinutes = 30
)

$ErrorActionPreference = 'Stop'
$Root = [IO.Path]::GetFullPath($Root).TrimEnd('\')
$reqDir = Join-Path $Root '.runner\requests'
$resDir = Join-Path $Root 'runner-results'
$log = Join-Path $LogDir 'runs.log'

function Audit([string]$msg) {
    try {
        New-Item -ItemType Directory -Force $LogDir | Out-Null
        Add-Content -Path $log -Value ("{0}  {1}" -f (Get-Date -Format 'yyyy-MM-ddTHH:mm:ss.fffzzz'), $msg) -Encoding UTF8
    } catch { }   # a failing log must not become a way to block or crash the runner
}

function Set-Status([string]$dir, [hashtable]$s) {
    New-Item -ItemType Directory -Force $dir | Out-Null
    $s | ConvertTo-Json | Set-Content -Path (Join-Path $dir 'status.json') -Encoding UTF8
}

function Test-Plain([string]$s) { $s -match '^[A-Za-z0-9_.:\\/,=+@ -]{0,260}$' }

# $null when fine, otherwise why the request is refused.
function Get-Problem($req) {
    if (-not $req.script -or $req.script -isnot [string]) { return 'no script given' }
    $full = try { [IO.Path]::GetFullPath($req.script) } catch { return 'bad script path' }
    if (-not $full.StartsWith($Root + '\', [StringComparison]::OrdinalIgnoreCase)) { return 'script is outside the project folder' }
    $leaf = Split-Path $full -Leaf
    if ($Allow -notcontains $leaf) { return "script '$leaf' is not on the allow-list" }
    if ((Split-Path (Split-Path $full -Parent) -Leaf) -ne 'scripts') { return "script is not in a 'scripts' folder" }
    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) { return 'script does not exist' }
    # Walk from the script up to Root: a junction or symlink could point anywhere.
    $p = $full
    while ($p.Length -gt $Root.Length) {
        if ((Get-Item -LiteralPath $p -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { return "'$p' is a link" }
        $p = Split-Path $p -Parent
    }
    foreach ($a in @($req.args)) {
        if ($null -eq $a) { continue }
        if ($a -isnot [string] -or -not (Test-Plain $a)) { return 'an argument has characters that are not allowed' }
    }
    return $null
}

function Invoke-Request([IO.FileInfo]$file) {
    $id = [IO.Path]::GetFileNameWithoutExtension($file.Name)
    if ($id -notmatch '^[A-Za-z0-9_-]{1,64}$') {
        Audit "REJECTED  file='$($file.Name)'  reason=bad request id"
        Remove-Item -LiteralPath $file.FullName -Force
        return
    }
    $out = Join-Path $resDir $id
    if (Test-Path -LiteralPath $out) {   # an id is used once; never replay
        Audit "REJECTED  id=$id  reason=id already used"
        Remove-Item -LiteralPath $file.FullName -Force
        return
    }
    $req = try { Get-Content -LiteralPath $file.FullName -Raw | ConvertFrom-Json } catch { $null }
    Remove-Item -LiteralPath $file.FullName -Force   # consumed either way
    $argv = if ($req -and $req.args) { @($req.args | ForEach-Object { [string]$_ }) } else { @() }
    $problem = if ($req) { Get-Problem $req } else { 'request is not valid JSON' }
    if ($problem) {
        Audit "REJECTED  id=$id  script='$($req.script)'  reason=$problem"
        Set-Status $out @{ id = $id; state = 'rejected'; reason = $problem }
        return
    }

    $script = [IO.Path]::GetFullPath($req.script)
    $hash = (Get-FileHash -LiteralPath $script -Algorithm SHA256).Hash
    $commit = try { (& git -C (Split-Path $script -Parent) rev-parse --short HEAD 2>$null) } catch { '' }
    $started = Get-Date
    Audit "START     id=$id  script='$script'  args='$($argv -join ' ')'  sha256=$hash  commit=$commit"
    Set-Status $out @{ id = $id; state = 'running'; script = $script; sha256 = $hash; commit = "$commit"; started = $started.ToString('o') }

    $stdout = Join-Path $out 'stdout.tmp'
    $stderr = Join-Path $out 'stderr.tmp'
    $quoted = ($argv | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $psArgs = "-NoProfile -ExecutionPolicy Bypass -File `"$script`" $quoted"
    $env:SEARCH_RUNNER = '1'   # tells the scripts there is no console to wait on
    $timedOut = $false
    $code = -1
    try {
        $p = Start-Process -FilePath 'powershell.exe' -ArgumentList $psArgs -WorkingDirectory (Split-Path (Split-Path $script -Parent) -Parent) `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden -PassThru
        $null = $p.Handle   # without this, ExitCode comes back empty after a timed WaitForExit
        if (-not $p.WaitForExit($TimeoutMinutes * 60 * 1000)) {
            $timedOut = $true
            & taskkill.exe /PID $p.Id /T /F | Out-Null
        } else { $code = $p.ExitCode }
    } catch {
        Audit "ERROR     id=$id  $($_.Exception.Message)"
    }
    Remove-Item Env:\SEARCH_RUNNER -ErrorAction SilentlyContinue

    $o = Join-Path $out 'output.txt'
    $parts = @($stdout, $stderr) | Where-Object { Test-Path -LiteralPath $_ }
    if ($parts) { Get-Content -LiteralPath $parts | Set-Content -Path $o -Encoding UTF8 } else { Set-Content -Path $o -Value '' }
    Remove-Item -LiteralPath $parts -Force -ErrorAction SilentlyContinue

    $state = if ($timedOut) { 'timeout' } else { 'done' }
    Audit "FINISH    id=$id  state=$state  exit=$code  seconds=$([int]((Get-Date) - $started).TotalSeconds)"
    Set-Status $out @{
        id = $id; state = $state; exitCode = $code; script = $script; sha256 = $hash; commit = "$commit"
        started = $started.ToString('o'); finished = (Get-Date).ToString('o'); output = $o
    }
}

if (-not (Test-Path -LiteralPath $reqDir)) { exit 0 }
if ((Get-Item -LiteralPath $reqDir -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    Audit "ABORT     request folder is a link"; exit 1
}
New-Item -ItemType Directory -Force $resDir | Out-Null

# Drain the queue; look once more after a pause for a request that landed as we finished.
$idle = 0
while ($idle -lt 2) {
    $pending = @(Get-ChildItem -LiteralPath $reqDir -Filter '*.json' -File | Sort-Object LastWriteTime)
    if (-not $pending) { $idle++; Start-Sleep -Seconds 2; continue }
    $idle = 0
    foreach ($f in $pending) { Invoke-Request $f }
}
