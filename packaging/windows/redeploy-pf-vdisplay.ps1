#requires -Version 5.1
<#
.SYNOPSIS
  One-shot DEV redeploy of the pf-vdisplay (punktfunk) virtual-display driver on the test box:
  (optional) build -> stop host -> stage+sign+install -> reload the adapter -> start host.

.DESCRIPTION
  Wraps drivers/deploy-dev.ps1 (which stages the freshly built pf_vdisplay.dll, clears its
  FORCE_INTEGRITY PE bit, signs it, stamps a STRICTLY-INCREASING DriverVer, builds+signs the catalog,
  and pnputil-installs it) with the two things the dev loop always needs around it:

    * The running host service HOLDS the driver's control device, and pnputil can't replace a busy
      DLL - so the host must be stopped across the install. This stops it first and starts it after.
    * pnputil /add-driver /install updates the driver STORE, but the OS keeps the LIVE adapter on the
      old binary until the device is reloaded - so this cycles the adapter (reset-pf-vdisplay.ps1)
      after install, which also clears the ghost monitor nodes for a clean slate.

  Run ELEVATED. Use -Build only from an MSVC dev shell (the driver's cargo build needs LIBCLANG_PATH
  + Version_Number=10.0.26100.0, per drivers/deploy-dev.ps1); otherwise build separately and omit it.

.PARAMETER Build       Run `cargo build` in packaging/windows/drivers first (needs the MSVC env).
.PARAMETER Service     Host service name. Default PunktfunkHost.
.PARAMETER Thumbprint  Passthrough to deploy-dev.ps1 (test-cert SHA-1). Omit to use its default.
.PARAMETER Nefconc     Passthrough to deploy-dev.ps1 (nefconc.exe path). Omit to use its default.
.PARAMETER Verify      After redeploy, probe to confirm ADD works (passes through to the reset's
                       -Verify; needs -Probe or punktfunk-probe.exe on PATH).
.PARAMETER Probe       Path to punktfunk-probe.exe for -Verify.

.EXAMPLE
  # already built the driver in an MSVC shell -> deploy it cleanly:
  powershell -ExecutionPolicy Bypass -File redeploy-pf-vdisplay.ps1
.EXAMPLE
  # build + deploy + verify, from an MSVC dev shell:
  powershell -ExecutionPolicy Bypass -File redeploy-pf-vdisplay.ps1 -Build -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe
#>
[CmdletBinding()]
param(
    [switch]$Build,
    [string]$Service = 'PunktfunkHost',
    [string]$Thumbprint,
    [string]$Nefconc,
    [switch]$Verify,
    [string]$Probe
)
$ErrorActionPreference = 'Stop'

$here       = Split-Path -Parent $MyInvocation.MyCommand.Path
$driversDir = Join-Path $here 'drivers'
$deploy     = Join-Path $driversDir 'deploy-dev.ps1'
$reset      = Join-Path $here 'reset-pf-vdisplay.ps1'
foreach ($f in @($deploy, $reset)) { if (-not (Test-Path $f)) { throw "missing helper: $f" } }

# 1) Optional rebuild (MSVC dev shell only).
if ($Build) {
    Write-Host "==> cargo build  (pf-vdisplay driver, $driversDir)"
    Push-Location $driversDir
    try {
        cargo build
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE) - is this an MSVC dev shell with LIBCLANG_PATH + Version_Number set?" }
    }
    finally { Pop-Location }
}

# 2) Stop the host (it holds the driver DLL; pnputil can't replace a busy binary).
$svc = Get-Service $Service -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -eq 'Running') {
    Write-Host "==> stopping $Service"
    Stop-Service $Service -Force
    Start-Sleep -Seconds 2
}

# 3) Stage + sign + install (strictly-increasing DriverVer so pnputil takes the new binary).
$deployArgs = @{ Install = $true }
if ($Thumbprint) { $deployArgs.Thumbprint = $Thumbprint }
if ($Nefconc)    { $deployArgs.Nefconc    = $Nefconc }
Write-Host "==> deploy-dev.ps1 -Install"
& $deploy @deployArgs

# 4) Reload the adapter so the OS loads the freshly-installed binary (+ clear ghost nodes). The reset
#    leaves the host alone (-NoHost) - we own the service lifecycle here.
Write-Host "==> reloading the pf-vdisplay adapter (clean slate)"
& $reset -NoHost

# 5) Start the host.
if ($svc) {
    Write-Host "==> starting $Service"
    Start-Service $Service
    Start-Sleep -Seconds 3
    Write-Host "    $Service status: $((Get-Service $Service -ErrorAction SilentlyContinue).Status)"
}

# 6) Optional verification probe.
if ($Verify) {
    $vArgs = @{ NoHost = $true; KeepGhosts = $true; Verify = $true }
    if ($Probe) { $vArgs.Probe = $Probe }
    & $reset @vArgs
}

Write-Host "pf-vdisplay redeploy done."
