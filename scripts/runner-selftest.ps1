<#
.SYNOPSIS
  Harmless check that the admin runner works: reports whether it is running
  elevated and as whom. Changes nothing.
#>
param([string]$Note = '')

$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "runner-selftest: $(Get-Date -Format o)"
Write-Host "  user      : $($id.Name)"
Write-Host "  elevated  : $admin"
Write-Host "  cwd       : $(Get-Location)"
Write-Host "  via runner: $([bool]$env:SEARCH_RUNNER)"
if ($Note) { Write-Host "  note      : $Note" }
if ($env:SEARCH_RUNNER -and -not $admin) { Write-Host 'WARNING: ran through the runner but NOT elevated' }
exit $(if ($admin) { 0 } else { 3 })
