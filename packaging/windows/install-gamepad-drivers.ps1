<#
.SYNOPSIS
  Install the bundled punktfunk virtual-gamepad UMDF drivers - pf_dualsense (DualSense + DualShock 4,
  one type-aware HID driver) and pf_xusb (Xbox 360 XUSB companion for classic XInput). Runs ELEVATED
  at setup time (invoked from the installer's [Run] section). Best-effort: warns and exits 0 on any
  failure, so a driver hiccup never aborts the whole install (gamepad input degrades gracefully - a
  session still streams without a pad).

.DESCRIPTION
  -Dir holds the staged payload: pf_dualsense.{inf,cat,dll}, pf_xusb.{inf,cat,dll}, and the signing
  .cer. Steps:
    1. Trust the self-signed driver cert (machine Root + TrustedPublisher) so pnputil adds it silently.
    2. pnputil /add-driver each .inf - adds the package to the driver store. (No /install or device-node
       creation: the host SwDeviceCreate's the per-session devnodes itself when a client forwards a pad,
       so PnP binds the store driver on demand.)

  ASCII-only on purpose: this is run by the installer via Windows PowerShell 5.1, which mis-decodes a
  BOM-less UTF-8 non-ASCII char (e.g. an em-dash) as a smart-quote and breaks parsing.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install-gamepad-drivers.ps1 -Dir C:\path\to\gamepad
#>
[CmdletBinding()]
param([string]$Dir = $PSScriptRoot)
# Never abort the installer on a driver failure.
$ErrorActionPreference = 'Continue'
trap { Write-Warning "gamepad driver install error: $_"; exit 0 }

# 1) Trust the self-signed driver cert (Root so the chain validates + TrustedPublisher so pnputil adds
#    it without a prompt).
$cer = Get-ChildItem -Path $Dir -Filter *.cer -ErrorAction SilentlyContinue | Select-Object -First 1
if ($cer) {
    Write-Host "==> importing $($cer.Name) to Root + TrustedPublisher"
    certutil.exe -addstore -f Root "$($cer.FullName)" | Out-Null
    certutil.exe -addstore -f TrustedPublisher "$($cer.FullName)" | Out-Null
}
else { Write-Warning "no .cer in $Dir; drivers may not install silently (untrusted publisher)" }

# 2) Add each driver package to the store (idempotent; re-adding the same .inf is harmless).
$infs = Get-ChildItem -Path $Dir -Filter *.inf -ErrorAction SilentlyContinue
if (-not $infs) { Write-Warning "no driver .inf in $Dir; skipping gamepad driver install."; exit 0 }
foreach ($inf in $infs) {
    Write-Host "==> pnputil /add-driver $($inf.Name)"
    & pnputil.exe /add-driver "$($inf.FullName)"
    $rc = $LASTEXITCODE
    if ($rc -eq 3010) { Write-Host "    added; a reboot is recommended." }
    elseif ($rc -ne 0) { Write-Warning "pnputil /add-driver $($inf.Name) returned $rc" }
}

exit 0
