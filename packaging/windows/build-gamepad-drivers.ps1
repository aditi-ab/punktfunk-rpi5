<#
.SYNOPSIS
  Build + sign the punktfunk virtual-gamepad UMDF drivers (pf-gamepad = DualSense/DualShock 4/Edge/Deck, pf-xusb =
  Xbox 360 / XInput) FROM SOURCE, in CI, and stage them for the host installer. The gamepad analogue of
  build-pf-vdisplay.ps1 - replaces the checked-in prebuilt binaries (packaging/windows/gamepad-drivers/)
  so the .dll/.inf/.cat stay in lockstep with the source and never go stale.

.DESCRIPTION
  Both drivers are members of the in-tree drivers workspace (packaging/windows/drivers/), so one
  `cargo build --release` builds the whole workspace (this shares wdk-sys/wdk-build + the bindgen pin with
  pf-vdisplay). Then, per driver: CLEAR the FORCE_INTEGRITY PE bit, sign the .dll, stampinf a DriverVer
  into the INF; then Inf2Cat both catalogs and sign them. Both drivers share ONE self-signed cert (or a
  supplied DRIVER_CERT secret) + ONE exported .cer - the layout `punktfunk-host.exe driver install
  --gamepad` consumes (per-driver .inf/.cat/.dll + one shared punktfunk-driver.cer).

  Output (-Out): pf_gamepad.{dll,inf,cat} + pf_xusb.{dll,inf,cat} + pf_mouse.{dll,inf,cat} +
  punktfunk-driver.cer. (pf_mouse is the resident virtual HID pointer, not a gamepad - it shares
  this pipeline + the --gamepad install path.)

.EXAMPLE
  pwsh -File build-gamepad-drivers.ps1 -Out C:\t\gamepad
#>
[CmdletBinding()]
param(
    [string]$DriversDir = (Join-Path $PSScriptRoot 'drivers'),
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$DriverVer,
    [string]$CertPfxB64 = $env:DRIVER_CERT_PFX_B64,
    [string]$CertPassword = $env:DRIVER_CERT_PASSWORD,
    # 'auto' (default) = required iff this is a v* tag build; 'true'/'false' to force. See below.
    [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

# The decoded signing key must not outlive this script. It is a STABLE key now — trusted as a
# machine root on every box that installs punktfunk — so a .pfx left behind in a build directory is
# a standing credential on a machine that runs build jobs, not the throwaway it used to be. A
# script-scope trap covers the failure paths; Remove-SigningPfx is also called on the way out.
$script:ShredPfx = $null
function Remove-SigningPfx {
    if ($script:ShredPfx -and (Test-Path $script:ShredPfx)) {
        Remove-Item $script:ShredPfx -Force -ErrorAction SilentlyContinue
        $script:ShredPfx = $null
    }
}
# `break` is for explicitness, not correctness: measured on the runner, a bare trap and
# trap+break behave identically here (exit 1, no resumption) for `throw` at script scope,
# `throw` inside a function, and a cmdlet error under EAP=Stop. Kept because it states the
# intent — shred, then re-throw — instead of relying on a default that is easy to misread.
trap { Remove-SigningPfx; break }

$DriversDir = (Resolve-Path $DriversDir).Path
$clear = Join-Path $PSScriptRoot 'clear-force-integrity.ps1'

$drivers = @(
    @{ crate = 'pf-gamepad';   dll = 'pf_gamepad.dll';   inx = 'pf-gamepad\pf_gamepad.inx';     inf = 'pf_gamepad.inf';   cat = 'pf_gamepad.cat' }
    @{ crate = 'pf-xusb';      dll = 'pf_xusb.dll';      inx = 'pf-xusb\pf_xusb.inx';           inf = 'pf_xusb.inf';      cat = 'pf_xusb.cat' }
    # Not a gamepad, but it rides the identical UMDF HID pipeline + the same install path
    # (`driver install --gamepad` adds every staged .inf): the resident virtual HID mouse that
    # keeps SM_MOUSEPRESENT true so DWM composites a cursor on headless hosts.
    @{ crate = 'pf-mouse';     dll = 'pf_mouse.dll';     inx = 'pf-mouse\pf_mouse.inx';         inf = 'pf_mouse.inf';     cat = 'pf_mouse.cat' }
)
foreach ($d in $drivers) {
    if (-not (Test-Path (Join-Path $DriversDir $d.inx))) { throw "no $($d.inx) under $DriversDir" }
}

# --- WDK build env ----------------------------------------------------------------------------
if (-not $env:Version_Number) { $env:Version_Number = '10.0.26100.0' }
if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')) {
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
}
# Build into the DEFAULT workspace target dir (not an external CARGO_TARGET_DIR) - wdk-build walks up
# from OUT_DIR for a Cargo.lock and doesn't support out-of-tree target dirs. See build-pf-vdisplay.ps1.
$rel = Join-Path $DriversDir 'target\x86_64-pc-windows-msvc\release'

# --- 1. build (release) - one build covers the whole workspace --------------------------------
if (-not $SkipBuild) {
    Write-Host "==> cargo build --release (drivers workspace) in $DriversDir"
    $prevTarget = $env:CARGO_TARGET_DIR
    Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    Push-Location $DriversDir
    & cargo build --release
    $rc = $LASTEXITCODE
    Pop-Location
    if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    if ($rc -ne 0) { throw "gamepad drivers cargo build failed ($rc)" }
}
foreach ($d in $drivers) {
    if (-not (Test-Path (Join-Path $rel $d.dll))) { throw "driver not built: $(Join-Path $rel $d.dll)" }
}

# --- 2. WDK sign tools ------------------------------------------------------------------------
$kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
function Find-Tool([string]$name, [string]$arch) {
    (Get-ChildItem "$kits\*\$arch\$name" -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1).FullName
}
$signtool = Find-Tool 'signtool.exe' 'x64'
$stampinf = Find-Tool 'stampinf.exe' 'x64'
$inf2cat = Find-Tool 'Inf2Cat.exe'  'x86'
foreach ($t in @($signtool, $stampinf, $inf2cat)) {
    if (-not $t) { throw 'a WDK tool (signtool/stampinf/Inf2Cat) was not found - install the Windows 10/11 WDK.' }
}

# --- 3. signing cert (supplied stable pfx OR fresh self-signed; shared by both drivers) -------
# FAIL CLOSED on a real release, same rule as the host/MSIX pack scripts. The fallback below mints
# a cert per BUILD, and the installer trusts whatever .cer ships in the bundle — so the signature
# proves nothing about origin, and each upgrade adds another self-signed root CA to the user's
# machine under the same name. That is survivable for canary and dev builds; shipping it in a
# release is not. ('auto' resolves from GITHUB_REF so a new workflow inherits the guard.)
$requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
               else { [Convert]::ToBoolean($RequireSignedCert) }
$cleanupCert = $null
if ($CertPfxB64) {
    Write-Host '==> signing with supplied driver cert (DRIVER_CERT_PFX_B64)'
    $pfx = Join-Path (Split-Path -Parent $Out) 'driver-signing.pfx'
    $script:ShredPfx = $pfx
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($CertPfxB64))
    $sec = if ($CertPassword) { ConvertTo-SecureString $CertPassword -AsPlainText -Force } else { $null }
    $signArgs = @('/f', $pfx); if ($CertPassword) { $signArgs += @('/p', $CertPassword) }
    $pubForCer = if ($sec) { Get-PfxCertificate -FilePath $pfx -Password $sec } else { Get-PfxCertificate -FilePath $pfx }
}
elseif ($requireCert) {
    throw ("release build ($env:GITHUB_REF) with no DRIVER_CERT_PFX_B64 — refusing to sign drivers " +
           "with a per-build throwaway cert. Set the DRIVER_CERT_PFX_B64 / DRIVER_CERT_PASSWORD " +
           "secrets (packaging/windows/README.md), or pass -RequireSignedCert false for a test build.")
}
else {
    Write-Host '==> no DRIVER_CERT_PFX_B64 -> generating a fresh self-signed driver cert (the installer trusts the bundled .cer at install time)'
    $cleanupCert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=punktfunk-driver' `
        -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(10)
    $signArgs = @('/sha1', $cleanupCert.Thumbprint)
    $pubForCer = $cleanupCert
}

# --- 4. stage + clear FORCE_INTEGRITY + sign dlls + stampinf infs ------------------------------
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
if (-not $DriverVer) { $now = Get-Date; $DriverVer = '9.9.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm') }

foreach ($d in $drivers) {
    $sDll = Join-Path $Out $d.dll
    $sInf = Join-Path $Out $d.inf
    Copy-Item (Join-Path $rel $d.dll) $sDll -Force
    Copy-Item (Join-Path $DriversDir $d.inx) $sInf -Force   # stampinf rewrites this copy in place
    & powershell -NoProfile -ExecutionPolicy Bypass -File $clear -Path $sDll | Out-Null
    & $signtool sign /fd SHA256 @signArgs $sDll | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool sign ($($d.dll)) failed ($LASTEXITCODE)" }
    & $stampinf -f $sInf -d '*' -a 'amd64' -u '2.15.0' -v $DriverVer | Out-Null
}

# --- 5. Inf2Cat both catalogs (one pass over -Out), then sign each -----------------------------
& $inf2cat /driver:$Out /os:10_X64 /uselocaltime | Out-Null
foreach ($d in $drivers) {
    $sCat = Join-Path $Out $d.cat
    if (-not (Test-Path $sCat)) { throw "Inf2Cat did not produce $sCat" }
    & $signtool sign /fd SHA256 @signArgs $sCat | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "signtool sign ($($d.cat)) failed ($LASTEXITCODE)" }
}

# --- 6. one shared public .cer ----------------------------------------------------------------
Export-Certificate -Cert $pubForCer -FilePath (Join-Path $Out 'punktfunk-driver.cer') | Out-Null
if ($cleanupCert) { Remove-Item "Cert:\CurrentUser\My\$($cleanupCert.Thumbprint)" -Force -ErrorAction SilentlyContinue }
Remove-SigningPfx

Write-Host "==> built + signed gamepad drivers  DriverVer=$DriverVer  ->  $Out"
Get-ChildItem $Out -File | ForEach-Object { "    $($_.Name)  ($($_.Length) bytes)" }
