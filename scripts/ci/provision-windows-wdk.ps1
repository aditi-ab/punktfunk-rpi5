# Provision the Windows Driver Kit (WDK) + cargo-wdk on the self-hosted windows-amd64 runner, so the
# all-Rust UMDF drivers (pf-vdisplay + the gamepad drivers, unified on microsoft/windows-drivers-rs)
# can build there. See docs/windows-host-rewrite.md (M0).
#
# The runner already has the base Windows SDK 10.0.26100 (um/ headers) + MSVC + LLVM + Rust, but NOT the
# WDK — no km/ + wdf/ + um/iddcx headers, no inf2cat/stampinf/devgen. wdk-sys's bindgen needs those.
# Idempotent: skips the WDK install if the km/wdf headers are already present, and cargo-wdk if already
# installed. Safe to run repeatedly. Runs non-interactively (/q /norestart) — never auto-reboots.
#
# Invoked by .gitea/workflows/windows-drivers-provision.yml (workflow_dispatch) and referenced by
# scripts/ci/setup-windows-runner.ps1. Run as the runner's account (SYSTEM) with admin rights.
[CmdletBinding()]
param(
  # WDK 26100 standalone bootstrapper. Source: https://learn.microsoft.com/windows-hardware/drivers/download-the-wdk
  # (bump this when the runner's base SDK version changes; must match an installed Windows SDK version).
  [string]$WdkSetupUrl = 'https://download.microsoft.com/download/41fb59c2-1723-45f9-a270-96b73ad58233/KIT_BUNDLE_WDK_MEDIACREATION/wdksetup.exe',
  [string]$SdkVersion  = '10.0.26100.0'
)

$ErrorActionPreference = 'Stop'
function info($m) { Write-Host "[provision-wdk] $m" }

$kitRoot = 'C:\Program Files (x86)\Windows Kits\10'
$wdfHdr  = Join-Path $kitRoot "Include\$SdkVersion\km\wdf\umdf\2.31\wdf.h"
$iddcxInc = Join-Path $kitRoot "Include\$SdkVersion\um\iddcx"

# ---- 1. WDK ----
if (Test-Path $wdfHdr) {
  info "WDK already present ($wdfHdr) — skipping install."
} else {
  $tmp = Join-Path $env:TEMP 'wdksetup.exe'
  info "Downloading WDK bootstrapper -> $tmp"
  Invoke-WebRequest -Uri $WdkSetupUrl -OutFile $tmp -UseBasicParsing
  info ("downloaded {0:N1} MB" -f ((Get-Item $tmp).Length / 1MB))
  info "Installing WDK silently (/q /norestart) — this can take several minutes..."
  $p = Start-Process -FilePath $tmp -ArgumentList '/q','/norestart' -Wait -PassThru
  info "wdksetup exit code = $($p.ExitCode)"
  if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {
    # 3010 = success, reboot required (acceptable for headers/libs/tools; no reboot done).
    throw "WDK install failed (exit $($p.ExitCode))"
  }
}

# ---- 2. cargo-wdk (the windows-drivers-rs build+package tool: cargo build -> stampinf/inf2cat/signtool) ----
$haveCargoWdk = $false
try { & cargo wdk --version *> $null; if ($LASTEXITCODE -eq 0) { $haveCargoWdk = $true } } catch {}
if ($haveCargoWdk) {
  info "cargo-wdk already installed."
} else {
  info "Installing cargo-wdk (cargo install --locked cargo-wdk)..."
  & cargo install --locked cargo-wdk
  if ($LASTEXITCODE -ne 0) { throw "cargo install cargo-wdk failed ($LASTEXITCODE)" }
}

# ---- 3. Verify ----
Write-Host ""
Write-Host "===== post-provision verification ====="
$ok = $true
function check($label, $cond) { Write-Host ("{0,-28} {1}" -f $label, ($(if ($cond) {'OK'} else {'MISSING'}))); if (-not $cond) { $script:ok = $false } }
check "km/wdf/umdf/2.31/wdf.h" (Test-Path $wdfHdr)
$iddcxVers = (Get-ChildItem $iddcxInc -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ','
Write-Host ("{0,-28} [{1}]" -f 'um/iddcx versions', $iddcxVers)
check "iddcx headers" ($iddcxVers -ne '')
foreach ($t in 'inf2cat.exe','stampinf.exe','signtool.exe') {
  $hit = Get-ChildItem -Path $kitRoot -Filter $t -Recurse -ErrorAction SilentlyContinue |
         Where-Object { $_.FullName -match '\\x64\\' } | Select-Object -First 1 -ExpandProperty FullName
  check $t ($null -ne $hit)
}
try { $cw = (& cargo wdk --version 2>&1) -join ' ' } catch { $cw = '' }
check "cargo-wdk" ($cw -match 'wdk')
Write-Host "cargo-wdk version: $cw"

if (-not $ok) { throw "provisioning incomplete — see MISSING above" }
info "WDK + cargo-wdk provisioned. Driver builds should pin Version_Number=$SdkVersion."
