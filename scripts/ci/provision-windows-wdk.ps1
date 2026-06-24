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

$kitRoot  = 'C:\Program Files (x86)\Windows Kits\10'
$iddcxInc = Join-Path $kitRoot "Include\$SdkVersion\um\iddcx"   # iddcx ships ONLY with the WDK -> reliable "installed" signal
$kmDir    = Join-Path $kitRoot "Include\$SdkVersion\km"          # kernel-mode SDK headers (ntddk/wdm) — also WDK-only

# ---- 1. WDK ---- (iddcx presence is the reliable "WDK installed" signal)
if (Test-Path $iddcxInc) {
  info "WDK already present (iddcx headers at $iddcxInc) — skipping install."
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

# ---- 2b. LLVM 21.1.2 (libclang for wdk-sys bindgen) ----
# wdk-sys's bindgen layout tests overflow (E0080 on threadlocaleinfostruct etc.) with LLVM ToT / dev
# (22+) builds — the runner's default C:\Program Files\LLVM. The windows-drivers-rs maintainers confirm
# the RELEASED LLVM 21.1.2 builds clean (discussion #591). Install it to a dedicated path so the client
# builds' LLVM is untouched; the driver-build CI job points LIBCLANG_PATH here.
$llvmDir  = 'C:\llvm-21'
$libclang = Join-Path $llvmDir 'bin\libclang.dll'
if (Test-Path $libclang) {
  info "LLVM 21.1.2 already present ($libclang) — skipping."
} else {
  $url = 'https://github.com/llvm/llvm-project/releases/download/llvmorg-21.1.2/LLVM-21.1.2-win64.exe'
  $tmp = Join-Path $env:TEMP 'LLVM-21.1.2-win64.exe'
  info "Downloading LLVM 21.1.2 -> $tmp"
  Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
  info ("downloaded {0:N1} MB; installing silently (NSIS /S /D=$llvmDir)..." -f ((Get-Item $tmp).Length / 1MB))
  # NSIS: /S = silent; /D=<path> MUST be last and unquoted (path has no spaces).
  $p = Start-Process -FilePath $tmp -ArgumentList '/S', "/D=$llvmDir" -Wait -PassThru
  info "LLVM installer exit = $($p.ExitCode)"
  if (-not (Test-Path $libclang)) { throw "LLVM 21.1.2 install did not produce $libclang (exit $($p.ExitCode))" }
}

# ---- 3. Verify (enumerate the REAL layout; fail only on build-essential absences) ----
Write-Host ""
Write-Host "===== post-provision verification ====="
function found($label, $val) { Write-Host ("{0,-26} {1}" -f $label, $val) }

# WDF UMDF headers live under Include\wdf\umdf\<ver>\ (NOT under the SDK-version dir).
$umdfRoot = Join-Path $kitRoot 'Include\wdf\umdf'
$umdfVers = (Get-ChildItem $umdfRoot -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ','
$umdfHdr  = Get-ChildItem -Path $umdfRoot -Filter 'wdf.h' -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
$iddcxVers = (Get-ChildItem $iddcxInc -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ','

found 'Include\wdf\umdf vers' "[$umdfVers]"
found 'wdf.h'                 ($(if ($umdfHdr) { $umdfHdr } else { 'MISSING' }))
found 'km SDK headers'        ($(if (Test-Path $kmDir) { $kmDir } else { 'MISSING' }))
found 'um/iddcx versions'     "[$iddcxVers]"
foreach ($t in 'inf2cat.exe','stampinf.exe','signtool.exe','makecat.exe','InfVerif.exe') {
  $hit = Get-ChildItem -Path $kitRoot -Filter $t -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
  found $t ($(if ($hit) { $hit } else { 'MISSING' }))
}
try { $cw = (& cargo wdk --version 2>&1) -join ' ' } catch { $cw = '' }
found 'cargo-wdk'             $cw
found 'LLVM 21.1.2 libclang'  ($(if (Test-Path $libclang) { $libclang } else { 'MISSING' }))

# Block only on the genuinely build-essential pieces (headers + iddcx + cargo-wdk + the pinned libclang).
# inf2cat arch quirks are non-fatal — cargo-wdk locates the WDK tools itself.
$essential = ($null -ne $umdfHdr) -and (Test-Path $kmDir) -and ($iddcxVers -ne '') -and ($cw -match 'wdk') -and (Test-Path $libclang)
if (-not $essential) { throw "provisioning incomplete: need wdf.h + km headers + iddcx + cargo-wdk + LLVM 21.1.2 (see above)" }
info "WDK + cargo-wdk + LLVM 21.1.2 provisioned OK. Driver builds pin Version_Number=$SdkVersion + LIBCLANG_PATH=$llvmDir\bin."
