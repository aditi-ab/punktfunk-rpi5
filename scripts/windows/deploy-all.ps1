<#
  Rebuild + redeploy everything: the Windows host service AND the web management console.
  Thin wrapper around deploy-host.ps1 + build-web.ps1 - see scripts\windows\README.md for what
  each one does on its own (rollback behavior, build env, etc).

    powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1

  Run from an elevated PowerShell. deploy-host.ps1 throws (and rolls itself back) on a failed
  build/start, which stops this script before the web console step runs.
#>
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

Write-Host "=========================================="
Write-Host " 1/2  host service"
Write-Host "=========================================="
& (Join-Path $here 'deploy-host.ps1')

Write-Host ""
Write-Host "=========================================="
Write-Host " 2/2  web console"
Write-Host "=========================================="
& (Join-Path $here 'build-web.ps1')

Write-Host ""
Write-Host "DONE - host + web console redeployed."
