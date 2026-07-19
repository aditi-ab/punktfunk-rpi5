<#
  Rebuild + redeploy everything: the Windows host service, the web management console AND the
  plugin/script runner. Thin wrapper around deploy-host.ps1 + build-web.ps1 + build-scripting.ps1 -
  see scripts\windows\README.md for what each one does on its own (rollback behavior, build env, etc).

    powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1
    powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-all.ps1 -EnableScriptingTask

  All three modules are ALWAYS deployed - a host binary whose runner bundle is stale (or missing)
  fails `punktfunk-host plugins add` outright, since the CLI forwards package ops to the runner it
  finds next to the exe.

  Run from an elevated PowerShell. deploy-host.ps1 throws (and rolls itself back) on a failed
  build/start, which stops this script before the later steps run.
#>
param(
    [switch]$EnableScriptingTask                     # forwarded to build-scripting.ps1 (opt-in task)
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot

Write-Host "=========================================="
Write-Host " 1/3  host service"
Write-Host "=========================================="
& (Join-Path $here 'deploy-host.ps1')

Write-Host ""
Write-Host "=========================================="
Write-Host " 2/3  web console"
Write-Host "=========================================="
& (Join-Path $here 'build-web.ps1')

Write-Host ""
Write-Host "=========================================="
Write-Host " 3/3  plugin/script runner"
Write-Host "=========================================="
& (Join-Path $here 'build-scripting.ps1') -EnableTask:$EnableScriptingTask

Write-Host ""
Write-Host "DONE - host + web console + plugin runner redeployed."
