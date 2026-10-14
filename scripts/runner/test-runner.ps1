<#
.SYNOPSIS
  Tests runner-wrapper.ps1's refusal rules and bookkeeping in a throwaway
  folder. Needs no admin rights and installs nothing.
#>
$ErrorActionPreference = 'Stop'
$wrapper = Join-Path $PSScriptRoot 'runner-wrapper.ps1'
$selftest = Join-Path (Split-Path $PSScriptRoot -Parent) 'runner-selftest.ps1'

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("runner-test-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$root = Join-Path $tmp 'root'
$logDir = Join-Path $tmp 'log'
$outside = Join-Path $tmp 'outside'
New-Item -ItemType Directory -Force "$root\scripts", "$root\other", "$root\sub", "$root\.runner\requests", $outside | Out-Null
Copy-Item $selftest "$root\scripts\runner-selftest.ps1"
Copy-Item $selftest "$root\other\runner-selftest.ps1"
Copy-Item $selftest "$outside\runner-selftest.ps1"
Set-Content "$root\scripts\evil.ps1" 'Write-Host evil'
New-Item -ItemType Junction -Path "$root\sub\scripts" -Target "$root\scripts" | Out-Null

function Req([string]$id, $script, $args_) {
    @{ script = $script; args = @($args_) } | ConvertTo-Json | Set-Content "$root\.runner\requests\$id.json" -Encoding UTF8
}
$good = "$root\scripts\runner-selftest.ps1"
Req 'ok1'      $good @('-Note', 'hello there')
Req 'evil'     "$root\scripts\evil.ps1" @()
Req 'outside'  "$outside\runner-selftest.ps1" @()
Req 'folder'   "$root\other\runner-selftest.ps1" @()
Req 'dotdot'   "$root\scripts\..\..\outside\runner-selftest.ps1" @()
Req 'inject1'  $good @('x; calc')
Req 'inject2'  $good @('$(calc)')
Req 'inject3'  $good @('"x" -Command calc')
Req 'junction' "$root\sub\scripts\runner-selftest.ps1" @()
Set-Content "$root\.runner\requests\badjson.json" '{ not json'
Set-Content "$root\.runner\requests\bad id!.json" '{}'

& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $wrapper -Root $root -LogDir $logDir -Allow 'runner-selftest.ps1' -TimeoutMinutes 2 | Out-Null

$fail = 0
function Check([string]$name, [bool]$ok) {
    Write-Host ("  {0,-4} {1}" -f $(if ($ok) { 'ok' } else { 'FAIL' }), $name)
    if (-not $ok) { $script:fail++ }
}
function St([string]$id) {
    $p = "$root\runner-results\$id\status.json"
    if (Test-Path $p) { Get-Content $p -Raw | ConvertFrom-Json } else { $null }
}

$s = St 'ok1'
Check 'allowed script runs'                 ($s -and $s.state -eq 'done')
Check 'it ran through the runner (env set)' ($s -and (Get-Content $s.output -Raw) -match 'via runner: True')
Check 'arguments reached the script'        ($s -and (Get-Content $s.output -Raw) -match 'note      : hello there')
Check 'status has hash and finish time'     ($s -and $s.sha256 -match '^[0-9A-F]{64}$' -and $s.finished)
foreach ($c in 'evil', 'outside', 'folder', 'dotdot', 'inject1', 'inject2', 'inject3', 'junction', 'badjson') {
    $r = St $c
    Check "refused: $c" ($r -and $r.state -eq 'rejected')
}
Check 'request files are consumed'          (-not (Get-ChildItem "$root\.runner\requests" -Filter *.json))
Check 'bad request id is dropped'           (-not (Test-Path "$root\runner-results\bad id!"))

# An id can only be used once.
Req 'ok1' $good @()
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $wrapper -Root $root -LogDir $logDir -Allow 'runner-selftest.ps1' | Out-Null
$log = Get-Content "$logDir\runs.log" -Raw
Check 'replayed id is refused'              ($log -match 'id already used')
Check 'log records start and finish'       ($log -match 'START +id=ok1' -and $log -match 'FINISH +id=ok1 +state=done')
Check 'log records refusals with reasons'  ($log -match 'REJECTED +id=evil.*not on the allow-list' -and $log -match 'is a link')

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
if ($fail) { Write-Host "$fail check(s) FAILED"; exit 1 } else { Write-Host 'all checks passed' }
