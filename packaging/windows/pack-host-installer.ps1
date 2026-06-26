<#
.SYNOPSIS
  Build + sign the punktfunk Windows host installer (Inno Setup setup.exe).

.DESCRIPTION
  From a release `cargo build -p punktfunk-host --features nvenc` output (the exe), this:
    1. resolves a code-signing cert (supplied stable .pfx from CI secrets OR an ephemeral self-signed
       CN=unom - same scheme as the client's pack-msix.ps1) and exports the public .cer,
    2. signs the inner punktfunk-host.exe,
    3. stages the pf-vdisplay virtual-display driver bundle (unless -NoDriver),
    4. runs ISCC to build punktfunk-host-setup-<ver>.exe,
    5. signs the setup.exe (timestamp best-effort),
    6. emits HOST_SETUP_PATH / HOST_CER_PATH to GITHUB_ENV for the publish step.

  Idempotent; safe to re-run. Run on the Windows runner / dev box (MSVC + Windows SDK + Inno Setup).

.EXAMPLE
  pwsh -File pack-host-installer.ps1 -Version 0.2.137 -TargetDir C:\t\release -OutDir C:\t\out
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,                 # e.g. 0.2.137 or 1.4.0 (free-form)
    [Parameter(Mandatory = $true)][string]$TargetDir,               # cargo --release dir (has punktfunk-host.exe)
    [string]$OutDir = (Join-Path $TargetDir 'installer'),
    [string]$Publisher = 'CN=unom',
    [string]$PfxBase64 = $env:MSIX_CERT_PFX_B64,                    # reuse the client's signing secret
    [string]$PfxPassword = $env:MSIX_CERT_PASSWORD,
    [string]$FfmpegDir = $env:FFMPEG_DIR,                           # bundle its bin\*.dll (amf-qsv build)
    [string]$WebDir = $env:WEB_OUTPUT_DIR,                          # built web .output tree -> bundle the mgmt console
    [string]$BunExe = $env:BUN_EXE,                                # portable bun.exe runtime for the console
    [switch]$NoDriver,                                              # build without the bundled pf-vdisplay driver
    [switch]$NoSign                                                 # skip signing (local debug)
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Keep the traditional "check $LASTEXITCODE myself" model: don't let pwsh 7.4 turn a non-zero native
# exit into a terminating error (it would bypass Sign-File's timestamp-then-retry fallback below).
$PSNativeCommandUseErrorActionPreference = $false

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$iss = Join-Path $here 'punktfunk-host.iss'
$exe = Join-Path $TargetDir 'punktfunk-host.exe'
if (-not (Test-Path $exe)) { throw "missing build artifact 'punktfunk-host.exe' in $TargetDir (did 'cargo build --release -p punktfunk-host --features nvenc' run?)" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# --- locate ISCC (Inno Setup) + signtool (Windows SDK) ---------------------------------------
function Find-Iscc {
    foreach ($p in @(
            'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
            'C:\Program Files\Inno Setup 6\ISCC.exe')) {
        if (Test-Path $p) { return $p }
    }
    $c = Get-Command iscc -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    throw "ISCC.exe (Inno Setup 6, any 6.x) not found - install it (choco install innosetup -y)."
}
function Find-SdkTool([string]$name) {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $hit = Get-ChildItem -Path $root -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\(10\.0\.\d+\.\d+)\\x64\\' } |
        Sort-Object { [version]([regex]::Match($_.FullName, '\\(10\.0\.\d+\.\d+)\\x64\\').Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$name not found under $root - install the Windows 10/11 SDK." }
    $hit.FullName
}
$iscc = Find-Iscc
Write-Host "ISCC: $iscc"

# --- signing cert (supplied stable pfx OR ephemeral self-signed) -----------------------------
$pfxPath = Join-Path $OutDir 'signing.pfx'
$cerPath = Join-Path $OutDir "punktfunk-host-windows_${Version}.cer"
$signtool = $null
if (-not $NoSign) {
    $signtool = Find-SdkTool 'signtool.exe'
    Write-Host "signtool: $signtool"
    if ($PfxBase64) {
        Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
        [IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($PfxBase64))
    }
    else {
        Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
        if (-not $PfxPassword) { $PfxPassword = 'punktfunk' }
        $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName 'punktfunk host (self-signed)' `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        $sec = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
        Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $pfxPath -Password $sec | Out-Null
        Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
    }
    # Always export the public .cer. For a self-signed cert it's the file users import once
    # (LocalMachine\TrustedPublisher) so SmartScreen/UAC trusts the signed setup.exe; for a real CA
    # cert it's a harmless extra.
    $pwsec = if ($PfxPassword) { ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText } else { $null }
    $pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $pfxPath -Password $pwsec } else { Get-PfxCertificate -FilePath $pfxPath }
    Export-Certificate -Cert $pubCert -FilePath $cerPath | Out-Null
    Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
}

function Sign-File([string]$Path) {
    if ($NoSign) { return }
    $signArgs = @('sign', '/fd', 'SHA256', '/f', $pfxPath)
    if ($PfxPassword) { $signArgs += @('/p', $PfxPassword) }
    & $signtool ($signArgs + @('/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', $Path))
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "timestamped sign failed for $Path - retrying without a timestamp"
        & $signtool ($signArgs + @($Path))
        if ($LASTEXITCODE -ne 0) { throw "signtool sign failed for $Path ($LASTEXITCODE)" }
    }
}

# --- sign the inner exe before it's packed ----------------------------------------------------
Sign-File $exe

# --- resolve + validate the installer's source files ------------------------------------------
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path
$hostEnvSrc = Join-Path $repoRoot 'scripts\windows\host.env.example'
$readmeSrc = Join-Path $here 'README.md'
foreach ($p in @($exe, $hostEnvSrc, $readmeSrc, $iss)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "installer source file missing: $p" }
}

# ISCC is a 32-bit program. On the self-hosted runner (which runs as SYSTEM) the checkout lives
# under C:\Windows\System32\config\systemprofile\..., and WOW64 file-system redirection rewrites a
# 32-bit process's System32 reads to SysWOW64 (where the files don't exist) -> ISCC dies at
# script-open with "path not found". So stage every file ISCC reads (the .iss + the two payload
# files) into the non-redirected build dir under C:\t. (BinDir/StageDir/OutputDir already live there.)
$hostEnv = Join-Path $OutDir 'host.env.example'
$readme = Join-Path $OutDir 'README.md'
$issLocal = Join-Path $OutDir 'punktfunk-host.iss'
Copy-Item -LiteralPath $hostEnvSrc -Destination $hostEnv -Force
Copy-Item -LiteralPath $readmeSrc -Destination $readme -Force
Copy-Item -LiteralPath $iss -Destination $issLocal -Force

$defines = @(
    "/DMyAppVersion=$Version",
    "/DBinDir=$TargetDir",
    "/DOutputDir=$OutDir",
    "/DHostEnv=$hostEnv",
    "/DReadme=$readme"
)

# --- build (from source) + stage the pf-vdisplay virtual-display driver -----------------------
# pf-vdisplay is our all-Rust IddCx driver (packaging/windows/drivers/). It is now BUILT FROM SOURCE
# every release (build-pf-vdisplay.ps1) instead of shipping a checked-in prebuilt binary: the vendored
# binary went stale (its .cat stopped covering an edited .inf -> pnputil SPAPI_E_FILE_HASH_NOT_IN_CATALOG
# on every box, and it predated IOCTL_SET_RENDER_ADAPTER the host needs on hybrid/Optimus GPUs). Building
# here keeps the .dll/.inf/.cat in lockstep + ships current driver features. stage-pf-vdisplay.ps1 then
# adds the fetched nefcon device tool. (Needs the WDK build env; -NoDriver skips it for a WDK-less pack.)
if (-not $NoDriver) {
    $built = Join-Path $OutDir 'pfvd-built'
    & (Join-Path $here 'build-pf-vdisplay.ps1') -Out $built
    $stage = Join-Path $OutDir 'stage'
    & (Join-Path $here 'stage-pf-vdisplay.ps1') -OutDir $stage -VendorDir $built
    Copy-Item (Join-Path $here 'install-pf-vdisplay.ps1') (Join-Path $stage 'install-pf-vdisplay.ps1') -Force
    $defines += "/DStageDir=$stage"
}
else { Write-Host "-NoDriver: building installer WITHOUT the bundled pf-vdisplay driver" }

# --- build (from source) + stage the punktfunk virtual-gamepad UMDF drivers --------------------
# pf-dualsense (DualSense / DualShock 4) + pf-xusb (Xbox 360 / XInput) are members of the same drivers
# workspace as pf-vdisplay, built from source per release (build-gamepad-drivers.ps1) - same anti-stale
# reasoning as pf-vdisplay; the prior checked-in binaries under gamepad-drivers/ are retired. install-
# gamepad-drivers.ps1 adds each to the store (the host SwDeviceCreate's the per-session devnodes).
if (-not $NoDriver) {
    $gpBuilt = Join-Path $OutDir 'gamepad-built'
    # -SkipBuild: build-pf-vdisplay.ps1 above already `cargo build`s the WHOLE drivers workspace (incl.
    # the gamepad cdylibs), so just sign+stage them here - no redundant second full build.
    & (Join-Path $here 'build-gamepad-drivers.ps1') -Out $gpBuilt -SkipBuild
    $gpStage = Join-Path $OutDir 'gamepad'
    if (Test-Path $gpStage) { Remove-Item -Recurse -Force $gpStage }
    New-Item -ItemType Directory -Force -Path $gpStage | Out-Null
    Copy-Item (Join-Path $gpBuilt '*') $gpStage -Force
    Copy-Item (Join-Path $here 'install-gamepad-drivers.ps1') (Join-Path $gpStage 'install-gamepad-drivers.ps1') -Force
    $defines += "/DGamepadStageDir=$gpStage"
    Write-Host "==> built + staged gamepad UMDF drivers -> $gpStage"
}

# --- stage the FFmpeg shared DLLs (AMD/Intel AMF/QSV build) ------------------------------------
# A host built with --features amf-qsv link-imports avcodec/avutil/swscale/... so the shared DLLs
# MUST sit next to the exe (it won't start otherwise). Bundle them from $FfmpegDir\bin - the same
# BtbN gpl-shared tree the build linked against. A nvenc/software-only build doesn't import them, so
# this is a harmless extra there; skipped entirely when $FfmpegDir is unset.
$ffmpegBinSrc = if ($FfmpegDir) { Join-Path $FfmpegDir 'bin' } else { $null }
if ($ffmpegBinSrc -and (Test-Path $ffmpegBinSrc)) {
    $dlls = Get-ChildItem -Path $ffmpegBinSrc -Filter '*.dll' -ErrorAction SilentlyContinue
    if ($dlls) {
        $ffmpegStage = Join-Path $OutDir 'ffmpeg'
        New-Item -ItemType Directory -Force -Path $ffmpegStage | Out-Null
        $dlls | ForEach-Object { Copy-Item $_.FullName -Destination $ffmpegStage -Force }
        $defines += "/DFfmpegBin=$ffmpegStage"
        Write-Host "bundling $($dlls.Count) FFmpeg DLL(s) from $ffmpegBinSrc"
    }
}
else { Write-Host "no FFMPEG_DIR\bin -> installer built WITHOUT FFmpeg DLLs (nvenc/software-only host)" }

# --- stage the web management console (the self-contained .output tree + a portable bun + launcher) -
# The console runs as the PunktfunkWeb scheduled task (`bun {app}\web\.output\server\index.mjs`),
# auto-wired to the host's loopback mgmt API. Stage everything ISCC reads into $OutDir (the
# non-WOW64-redirected C:\t area, same reason as the .iss/host.env staging above). The .output is
# self-contained (Nitro noExternals - deps bundled + tree-shaken, no node_modules), so bun runs it
# directly; omitted when -WebDir/-BunExe are unset (host-only installer, e.g. a local debug pack).
if ($WebDir -and (Test-Path $WebDir) -and $BunExe -and (Test-Path $BunExe)) {
    $webStage = Join-Path $OutDir 'web'
    if (Test-Path $webStage) { Remove-Item $webStage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $webStage | Out-Null
    Copy-Item (Join-Path $WebDir '*') -Destination $webStage -Recurse -Force
    $bunStage = Join-Path $OutDir 'bun.exe'
    Copy-Item -LiteralPath $BunExe -Destination $bunStage -Force
    $webRun = Join-Path $OutDir 'web-run.cmd'
    $webSetup = Join-Path $OutDir 'web-setup.ps1'
    Copy-Item (Join-Path $repoRoot 'scripts\windows\web-run.cmd') -Destination $webRun -Force
    Copy-Item (Join-Path $repoRoot 'scripts\windows\web-setup.ps1') -Destination $webSetup -Force
    $defines += "/DWebDir=$webStage"
    $defines += "/DBunExe=$bunStage"
    $defines += "/DWebRunCmd=$webRun"
    $defines += "/DWebSetup=$webSetup"
    Write-Host "bundling the web console from $WebDir (+ bun $BunExe)"
}
else { Write-Host "no -WebDir/-BunExe -> installer built WITHOUT the web console" }

# --- build + stage the HDR Vulkan layer (pf-vkhdr-layer) --------------------------------------
# A tiny always-on Vulkan implicit layer (cdylib) that advertises HDR10/scRGB surface formats on the
# virtual display so Vulkan games (Doom: The Dark Ages, etc.) can enable HDR while streaming - the
# NVIDIA/AMD ICDs hide HDR formats on an indirect display even though they accept+present a forced HDR
# swapchain there. Self-gated on the display's actual advanced-color state, so it's a no-op on SDR.
# Standalone crate (own [workspace]); built here and registered by the installer. Skipped if cargo
# is unavailable or the build fails -> installer is produced WITHOUT the layer (non-fatal).
$layerSrc = Join-Path $here 'pf-vkhdr-layer'
if (Test-Path (Join-Path $layerSrc 'Cargo.toml')) {
    $layerTarget = Join-Path $OutDir 'vklayer-target'
    Write-Host "==> building pf-vkhdr-layer (cdylib)"
    $prevTarget = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $layerTarget
    Push-Location $layerSrc
    & cargo build --release
    $layerExit = $LASTEXITCODE
    Pop-Location
    if ($prevTarget) { $env:CARGO_TARGET_DIR = $prevTarget } else { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    $layerDll = Join-Path $layerTarget 'release\pf_vkhdr_layer.dll'
    if ($layerExit -eq 0 -and (Test-Path $layerDll)) {
        $layerStage = Join-Path $OutDir 'vklayer'
        New-Item -ItemType Directory -Force -Path $layerStage | Out-Null
        Copy-Item $layerDll (Join-Path $layerStage 'pf_vkhdr_layer.dll') -Force
        Copy-Item (Join-Path $layerSrc 'pf_vkhdr_layer.json') (Join-Path $layerStage 'pf_vkhdr_layer.json') -Force
        Sign-File (Join-Path $layerStage 'pf_vkhdr_layer.dll')
        $defines += "/DVkLayerDir=$layerStage"
        Write-Host "==> staged pf-vkhdr-layer -> $layerStage"
    }
    else { Write-Warning "pf-vkhdr-layer build failed ($layerExit) - installer built WITHOUT the HDR Vulkan layer" }
}
else { Write-Host "no pf-vkhdr-layer crate -> installer built WITHOUT the HDR Vulkan layer" }

# --- build the installer (from the non-redirected copy under C:\t) -----------------------------
Write-Host "==> ISCC $($defines -join ' ') $issLocal"
& $iscc @defines $issLocal
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

$setup = Join-Path $OutDir "punktfunk-host-setup-$Version.exe"
if (-not (Test-Path $setup)) { throw "expected installer not produced: $setup" }

# --- sign the setup.exe + clean up ------------------------------------------------------------
Sign-File $setup
Remove-Item $pfxPath -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "==> installer: $setup"
if (-not $NoSign) {
    Write-Host "==> trust the cert once per machine (self-signed builds), then the signed setup.exe is trusted:"
    Write-Host "    Import-Certificate -FilePath '$cerPath' -CertStoreLocation Cert:\LocalMachine\TrustedPublisher"
}
if ($env:GITHUB_ENV) {
    "HOST_SETUP_PATH=$setup" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    if (-not $NoSign) { "HOST_CER_PATH=$cerPath" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8 }
}
