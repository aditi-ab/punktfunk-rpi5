<#
.SYNOPSIS
  Build + sign the pf-vdisplay UMDF IddCx virtual-display driver FROM SOURCE, in CI, and stage it for the
  host installer. This REPLACES the old vendored-prebuilt-binary model (packaging/windows/pf-vdisplay/) -
  the binary went stale (frozen mid-June while the driver source kept moving), which silently shipped two
  field bugs: (1) the catalog no longer covered the edited INF (pnputil SPAPI_E_FILE_HASH_NOT_IN_CATALOG on
  every box), and (2) the binary predated IOCTL_SET_RENDER_ADAPTER that the host needs to pin the IDD render
  GPU on hybrid/Optimus boxes. Building every release from source keeps the .dll/.inf/.cat in lockstep and
  ships current driver features.

.DESCRIPTION
  Mirrors packaging/windows/drivers/deploy-dev.ps1 but for CI (release build, output to -Out, cert from a
  secret OR a fresh self-signed). Steps: cargo build (the wdk-sys/windows-drivers-rs driver workspace) ->
  CLEAR the FORCE_INTEGRITY PE bit (wdk-build links /INTEGRITYCHECK, which a non-EV cert can't satisfy) ->
  sign the .dll -> stampinf a strictly-increasing DriverVer into the INF -> Inf2Cat the catalog -> sign the
  catalog -> export the public .cer. Output (-Out): pf_vdisplay.{dll,inf,cat} + punktfunk-driver.cer.

  Requires the WDK build env: cargo + the x64 MSVC toolset, an LLVM compatible with the driver's bindgen
  (>= 0.72 supports current clang), LIBCLANG_PATH, and the Windows 10/11 WDK (the runner has these). Sets
  Version_Number for wdk-build if the caller didn't.

.EXAMPLE
  pwsh -File build-pf-vdisplay.ps1 -Out C:\t\pfvd -DriverVer 9.9.0626.1612
#>
[CmdletBinding()]
param(
    [string]$DriversDir = (Join-Path $PSScriptRoot 'drivers'),
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$DriverVer,                                   # default: 9.9.MMdd.HHmm (strictly-increasing)
    [string]$CertPfxB64 = $env:DRIVER_CERT_PFX_B64,       # optional stable driver-signing cert (CI secret)
    [string]$CertPassword = $env:DRIVER_CERT_PASSWORD,
    [switch]$SkipBuild                                    # reuse an existing target\...\release\pf_vdisplay.dll
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$PSNativeCommandUseErrorActionPreference = $false

$DriversDir = (Resolve-Path $DriversDir).Path
$inx = Join-Path $DriversDir 'pf-vdisplay\pf_vdisplay.inx'
$clear = Join-Path $PSScriptRoot 'clear-force-integrity.ps1'
if (-not (Test-Path $inx)) { throw "no pf_vdisplay.inx under $DriversDir" }

# --- WDK build env (wdk-build needs Version_Number; bindgen needs LIBCLANG_PATH) --------------
if (-not $env:Version_Number) { $env:Version_Number = '10.0.26100.0' }
if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')) {
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
}
# The driver MUST build into its DEFAULT target dir (under the driver workspace), NOT an external one:
# wdk-sys's build script calls wdk-build::find_top_level_cargo_manifest(), which walks UP from OUT_DIR
# for the first ancestor holding a Cargo.lock (it explicitly "does not support non-default target
# directories"). CI sets a shared CARGO_TARGET_DIR=C:\t, whose ancestors have no Cargo.lock -> the build
# script panics "a Cargo.lock file should exist in the same directory as the top-level Cargo.toml". So
# clear CARGO_TARGET_DIR for this build and let cargo use <driver-workspace>\target (its ancestors
# include the driver Cargo.lock). The driver has no CMake-from-source deps, so it doesn't need C:\t's
# MAX_PATH dodge, and its own [workspace] keeps it isolated from the host's tree regardless.
$drvTarget = Join-Path $DriversDir 'target'
$dll = Join-Path $drvTarget 'x86_64-pc-windows-msvc\release\pf_vdisplay.dll'

# --- 1. build (release) -----------------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Host "==> cargo build --release (pf-vdisplay) in $DriversDir (default target -> $drvTarget)"
    $prevTarget = $env:CARGO_TARGET_DIR
    Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    Push-Location $DriversDir
    & cargo build --release
    $rc = $LASTEXITCODE
    Pop-Location
    if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    if ($rc -ne 0) { throw "pf-vdisplay cargo build failed ($rc)" }
}
if (-not (Test-Path $dll)) { throw "driver not built: $dll" }

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

# --- 3. signing cert (supplied stable pfx OR fresh self-signed) -------------------------------
$cleanupCert = $null
if ($CertPfxB64) {
    Write-Host '==> signing with supplied driver cert (DRIVER_CERT_PFX_B64)'
    $pfx = Join-Path $Out '..\driver-signing.pfx'
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($CertPfxB64))
    $sec = if ($CertPassword) { ConvertTo-SecureString $CertPassword -AsPlainText -Force } else { $null }
    $signArgs = @('/f', $pfx); if ($CertPassword) { $signArgs += @('/p', $CertPassword) }
    $pubForCer = if ($sec) { Get-PfxCertificate -FilePath $pfx -Password $sec } else { Get-PfxCertificate -FilePath $pfx }
}
else {
    Write-Host '==> no DRIVER_CERT_PFX_B64 -> generating a fresh self-signed driver cert (the installer trusts the bundled .cer at install time)'
    $cleanupCert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=punktfunk-driver' `
        -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(10)
    $signArgs = @('/sha1', $cleanupCert.Thumbprint)
    $pubForCer = $cleanupCert
}

# --- 4. stage + clear FORCE_INTEGRITY + sign + cat --------------------------------------------
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$sDll = Join-Path $Out 'pf_vdisplay.dll'
$sInf = Join-Path $Out 'pf_vdisplay.inf'
$sCat = Join-Path $Out 'pf_vdisplay.cat'
$sCer = Join-Path $Out 'punktfunk-driver.cer'
Copy-Item $dll $sDll -Force
Copy-Item $inx $sInf -Force   # stampinf rewrites this copy in place

# Clear FORCE_INTEGRITY BEFORE signing (it edits the PE, invalidating any signature).
& powershell -NoProfile -ExecutionPolicy Bypass -File $clear -Path $sDll | Out-Null

if (-not $DriverVer) { $now = Get-Date; $DriverVer = '9.9.{0}.{1}' -f $now.ToString('MMdd'), $now.ToString('HHmm') }

& $signtool sign /fd SHA256 @signArgs $sDll | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool sign (dll) failed ($LASTEXITCODE)" }
& $stampinf -f $sInf -d '*' -a 'amd64' -u '2.15.0' -v $DriverVer | Out-Null
& $inf2cat /driver:$Out /os:10_X64 /uselocaltime | Out-Null
if (-not (Test-Path $sCat)) { throw "Inf2Cat did not produce $sCat" }
& $signtool sign /fd SHA256 @signArgs $sCat | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool sign (cat) failed ($LASTEXITCODE)" }
Export-Certificate -Cert $pubForCer -FilePath $sCer | Out-Null
if ($cleanupCert) { Remove-Item "Cert:\CurrentUser\My\$($cleanupCert.Thumbprint)" -Force -ErrorAction SilentlyContinue }

# --- 5. guard: assert the freshly-built catalog covers the inf + dll ---------------------------
# Built-from-source can't drift, but this catches a botched stampinf/Inf2Cat ordering. Test-FileCatalog
# itself can't always OPEN a catalog signed by a not-yet-trusted cert (it throws UnableToOpenCatalogFile),
# so treat ITS failure as inconclusive (warn) - but a real coverage miss still fails the build.
$cat = $null
try { $cat = Test-FileCatalog -CatalogFilePath $sCat -Path $Out -FilesToSkip 'pf_vdisplay.cat', 'punktfunk-driver.cer' -Detailed }
catch { Write-Warning "catalog coverage guard inconclusive (Test-FileCatalog: $($_.Exception.Message))" }
if ($cat) {
    $covered = @($cat.CatalogItems.Keys)
    foreach ($need in @('pf_vdisplay.inf', 'pf_vdisplay.dll')) {
        if (-not ($covered | Where-Object { $_ -like "*$need" })) {
            throw "catalog coverage guard: $need is NOT in $sCat (stampinf/Inf2Cat ordering bug?)"
        }
    }
    Write-Host "    catalog covers pf_vdisplay.inf + pf_vdisplay.dll (status=$($cat.Status))"
}

Write-Host "==> built + signed pf-vdisplay  DriverVer=$DriverVer  ->  $Out"
Get-ChildItem $Out -File | ForEach-Object { "    $($_.Name)  ($($_.Length) bytes)" }
