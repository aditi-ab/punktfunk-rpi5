<#
.SYNOPSIS
  Install the bundled pf-vdisplay (punktfunk) virtual-display driver - our own all-Rust UMDF IddCx
  indirect-display driver, built from source per release (packaging/windows/build-pf-vdisplay.ps1).
  Runs ELEVATED at setup time (invoked from the installer's [Run] section). Best-effort: warns and exits
  0 on any failure - the host degrades to a physical display without a virtual display, so a driver
  hiccup must never abort the whole install.

.DESCRIPTION
  -Dir holds the staged payload (pf_vdisplay.inf/.cat/.dll + signing .cer + nefconc.exe). Steps:
    1. Trust the self-signed driver cert (machine Root + TrustedPublisher) so PnP installs it silently
       (the punktfunk-driver cert the build signs the driver + catalog with).
    2. Create the ROOT device node IF ABSENT (gated - a blind re-create spawns a phantom duplicate, and
       the host's open_device() binds interface index 0; crates/punktfunk-host/src/vdisplay/windows/pf_vdisplay.rs).
       ALWAYS via nefconc (a clean ROOT\DISPLAY node) - NEVER devgen, which makes persistent SWD\DEVGEN
       software devices that survive reboot + registry deletion and resurrect on every driver install.
    3. Stage + bind the driver (pnputil /add-driver /install - modern, in-box, idempotent).

  Class/ClassGuid are read from the .inf so they always match the shipped driver.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install-pf-vdisplay.ps1 -Dir C:\path\to\pf-vdisplay
#>
[CmdletBinding()]
param(
    [string]$Dir = $PSScriptRoot,
    [string]$HardwareId = 'root\pf_vdisplay'   # matches pf_vdisplay.inf [Standard.NTamd64]
)
# Never abort the installer on a driver failure.
$ErrorActionPreference = 'Continue'
trap { Write-Warning "pf-vdisplay install error: $_"; exit 0 }

function Test-PfVdisplayPresent {
    $devs = Get-PnpDevice -Class Display -PresentOnly -ErrorAction SilentlyContinue
    foreach ($d in $devs) {
        $hw = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' `
                -ErrorAction SilentlyContinue).Data
        if ($hw -and ($hw | Where-Object { $_ -ieq $HardwareId })) { return $true }
    }
    return $false
}

$inf = Get-ChildItem -Path $Dir -Filter pf_vdisplay.inf -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
$cer = Get-ChildItem -Path $Dir -Filter *.cer -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
$nef = Get-ChildItem -Path $Dir -Filter 'nefconc.exe' -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $inf) { Write-Warning "no pf_vdisplay.inf in $Dir; skipping driver install."; exit 0 }
Write-Host "pf-vdisplay inf: $($inf.FullName)"

# 1) Trust the self-signed driver cert (a self-signed driver needs the cert in BOTH the machine Root
#    store, so the chain validates, and TrustedPublisher, so PnP installs without a prompt).
if ($cer) {
    Write-Host "==> importing $($cer.Name) to Root + TrustedPublisher"
    certutil.exe -addstore -f Root "$($cer.FullName)" | Out-Null
    certutil.exe -addstore -f TrustedPublisher "$($cer.FullName)" | Out-Null
}
else { Write-Warning "no .cer in $Dir - driver may not install silently (untrusted publisher)" }

# 2) Create the root device node only if it isn't already there. nefconc, NEVER devgen.
if (Test-PfVdisplayPresent) {
    Write-Host "pf-vdisplay device node already present - leaving it as-is."
}
elseif ($nef) {
    $infText = Get-Content -Raw $inf.FullName
    $classGuid = ([regex]::Match($infText, '(?im)^\s*ClassGuid\s*=\s*(\{[0-9A-Fa-f-]+\})')).Groups[1].Value
    $className = ([regex]::Match($infText, '(?im)^\s*Class\s*=\s*([^\s;]+)')).Groups[1].Value
    if (-not $classGuid) { $classGuid = '{4d36e968-e325-11ce-bfc1-08002be10318}'; $className = 'Display' } # Display class
    Write-Host "==> nefconc --create-device-node hwid=$HardwareId class=$className $classGuid"
    & $nef.FullName --create-device-node --hardware-id $HardwareId --class-name $className --class-guid $classGuid
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) {
        Write-Warning "nefconc --create-device-node returned $LASTEXITCODE"
    }
}
else { Write-Warning "nefconc.exe not found in $Dir - cannot create the pf-vdisplay device node." }

# 3) Stage + bind the driver (idempotent; re-staging the same .inf is harmless).
Write-Host "==> pnputil /add-driver $($inf.Name) /install"
& pnputil.exe /add-driver "$($inf.FullName)" /install
$rc = $LASTEXITCODE
if ($rc -eq 3010) { Write-Host "    driver installed; a reboot is recommended." }
elseif ($rc -ne 0) { Write-Warning "pnputil /add-driver returned $rc" }

exit 0
