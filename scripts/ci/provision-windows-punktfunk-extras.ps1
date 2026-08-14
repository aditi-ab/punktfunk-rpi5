# Layers punktfunk-specific tooling onto the shared unom Windows CI runner: FFmpeg (the HOST's
# amf-qsv encode leg, x64 only), Inno Setup (the host installer), and the aarch64-pc-windows-msvc
# rustup target (windows-client.yml's ARM64 leg). The runner itself - act_runner, Node, rustup,
# VS Build Tools/NASM/CMake/LLVM - is provisioned generically by unom/infra
# (windows-runner/windows-runner.pkr.hcl + proxmox/windows-runner's Terraform clone); this script
# is what punktfunk adds on top, since FFmpeg/Inno Setup/the ARM64 target aren't every project's
# concern. See also provision-windows-wdk.ps1 for the driver-build toolchain (also punktfunk-only).
#
# Idempotent - safe to re-run. Run ELEVATED (admin) on the runner.
[CmdletBinding()]
param()
$ErrorActionPreference = "Stop"
function info($m) { Write-Host "[provision-punktfunk-extras] $m" }

$env:RUSTUP_HOME = "C:\Users\Public\.rustup"
$env:CARGO_HOME  = "C:\Users\Public\.cargo"

# --- ARM64 cross-compile target (windows-client.yml builds aarch64-pc-windows-msvc off
# this x64 box; the ARM64 MSVC cross compiler itself comes from unom/infra's generic VS Build
# Tools provisioning, which already includes the ARM64 component). ---
$rustup = "C:\Users\Public\.cargo\bin\rustup.exe"
if (Test-Path $rustup) {
  info "rustup target add aarch64-pc-windows-msvc"
  & $rustup target add aarch64-pc-windows-msvc
} else {
  Write-Warning "rustup not found at $rustup - has unom/infra's setup-gitea-runner-base.ps1 run on this box yet?"
}

# --- FFmpeg shared tree for the HOST's amf-qsv encode leg (windows-host.yml). BtbN **lgpl-shared**
# builds: the AMD/Intel AMF + Intel QSV encoders, swscale, and the HEVC decoder are all present in
# the LGPL build, and punktfunk never calls the GPL-only encoders (x264/x265 - software encode is
# the separate BSD-2 openh264 crate; NVENC is the direct NVIDIA SDK). lgpl-shared keeps the
# bundled DLLs LGPL-2.1+ (dynamic linking satisfies the relink duty) rather than GPL, so the
# shipped installer/MSIX stay consistent with punktfunk's MIT OR Apache-2.0 posture.
# ⚠ The CLIENT no longer links FFmpeg at all (M10, design/client-native-decode.md §6): it decodes
# with pf-vkdecode / pf-dxvadec / openh264 + rav1d. windows-client.yml sets no
# FFMPEG_DIR and the MSIX bundles no libav* DLLs, so only the x64 tree is fetched now - the ARM64
# one existed solely for the ARM64 client leg. Delete a stale C:\Users\Public\ffmpeg-arm64 by
# hand; this script does not remove what it no longer installs.
# MIGRATION: a runner previously provisioned with the old *gpl-shared* tree must be
# re-provisioned - delete C:\Users\Public\ffmpeg, then re-run.
# These DLLs are bundled verbatim into the code-signed host installer/MSIX, so the download is
# SHA-256-pinned (like VB-CABLE below): BtbN's `latest` tag is a ROLLING release whose assets are
# re-uploaded over time, so an unverified fetch would let a hijacked/MITM'd upstream asset land
# signed DLLs in users' installs. The pins below were captured 2026-07-10 from the then-current
# n7.1 lgpl-shared build. When BtbN re-rolls `latest`, this fetch FAILS CLOSED (hash mismatch) —
# that is intentional: re-download, re-verify the new archive, and update the two pins here.
#
# STILL n7.1 AFTER THE 2026-08-08 ffmpeg-next 8 -> 9 BUMP, on purpose. A crate major is a CEILING,
# not a target (ffmpeg-sys-next 9 spans libavcodec 56..63), so 7.1 keeps compiling; and Windows has
# no exposure to the soname break that forced the bump, because these DLLs are BUNDLED into the
# signed installer/MSIX rather than resolved from a system that can upgrade underneath them. BtbN
# publishes no FFmpeg 9 build at all right now (`latest` carries n7.1 and n8.1 only), so matching
# Arch is not even available. Moving this pin would swap the DLLs inside a code-signed installer and
# re-qualify AMF/QSV encode on real Intel/AMD hardware, which is its own change with its own on-glass
# pass — not a side effect of a Cargo bump. ⚠ One consequence to keep in mind while it stays here:
# 7.1's `AVD3D11VADeviceContext` is two UINTs shorter than 8/9's, which is why the mirror in
# crates/pf-encode/src/enc/windows/ffmpeg_win.rs deliberately stops at the common prefix.
#   Refresh a pin:  (Get-FileHash .\ffmpeg-<tag>.zip -Algorithm SHA256).Hash
function Get-BtbnFfmpeg {
  param([string]$Dir, [string]$ZipTag, [string]$Sha)   # ZipTag: 'win64' (x64); BtbN also publishes 'winarm64'
  if (Test-Path (Join-Path $Dir 'lib\avcodec.lib')) { info "FFmpeg ($ZipTag) already present at $Dir"; return }
  info "fetching FFmpeg ($ZipTag, BtbN lgpl-shared, SHA-256 pinned)"
  $url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n7.1-latest-$ZipTag-lgpl-shared-7.1.zip"
  $zip = "$Dir.zip"; $tmp = "$Dir-extract"
  Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  $got = (Get-FileHash $zip -Algorithm SHA256).Hash
  if ($got -ne $Sha) {
    Remove-Item $zip -Force
    throw "FFmpeg ($ZipTag) download hash mismatch (got $got, pinned $Sha). BtbN re-rolled the 'latest' build; re-verify the new archive and update the pinned SHA in this script before shipping."
  }
  if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force   # BtbN zips have one top-level folder
  $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
  if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
  Move-Item -Path $inner.FullName -Destination $Dir
  Remove-Item -Force $zip; Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Get-BtbnFfmpeg -Dir "C:\Users\Public\ffmpeg" -ZipTag 'win64' -Sha '89F3469706E5D53AEA5CF34AEE63E62CE746E6159D7AEE473D330B02A47558E6'

# --- No Vulkan-Headers here any more: they existed only for pf-ffvk's bindgen over
# libavutil/hwcontext_vulkan.h, and that crate is gone (M10). Nothing punktfunk builds on Windows
# needs Vulkan headers at compile time - ash generates its own bindings and dlopens vulkan-1.dll,
# which is a GPU-driver component. A stale C:\Users\Public\vulkan-headers is harmless; delete it
# by hand if you want the disk back. ---

# --- Inno Setup (ISCC.exe) for the host installer build (windows-host.yml). pack-host-installer.ps1
# locates it at its fixed Program Files path, so it need not be on PATH - just present. The .iss
# uses the 6.6+ styling (WizardStyle dark/dynamic + the windows11 style); an older 6.x compiles a
# plain-modern fallback, so upgrade a pre-6.6 install rather than silently shipping the old look. ---
$isccPath = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
$innoVer = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1' -ErrorAction SilentlyContinue).DisplayVersion
if (-not (Test-Path $isccPath) -or ($innoVer -and [version]$innoVer -lt [version]'6.6.0')) {
  if (Get-Command choco -ErrorAction SilentlyContinue) {
    info "installing/upgrading Inno Setup (ISCC; found: $innoVer)"
    choco upgrade innosetup -y --no-progress
  } else { Write-Warning "Inno Setup missing or pre-6.6 ($innoVer) and choco unavailable - install/upgrade it for windows-host.yml." }
}

# VB-CABLE provisioning removed (the audio-substrate program, 2026-08): the installer no longer
# bundles a cable - the host mints its audio endpoints from Steam's streaming drivers on the
# target box. A stale C:\Users\Public\vbcable on a runner is harmless and can be deleted.

# --- Drop punktfunk's env vars into the generic runner's daemon wrapper extension point (see
# unom/infra's scripts/setup-gitea-runner-base.ps1) so the act_runner daemon - and therefore every
# job it runs - sees FFMPEG_DIR without unom/infra needing to know punktfunk exists.
# FFMPEG_DIR + the PATH prepend are the HOST's (windows-host.yml amf-qsv: import libs at link time,
# the DLLs at test time). The client workflows ignore both - they link no libav*. ---
$projectEnv = "C:\Users\Public\act-runner\project-env.ps1"
@'
$env:FFMPEG_DIR = "C:\Users\Public\ffmpeg"
$env:PATH = "C:\Users\Public\ffmpeg\bin;" + $env:PATH
'@ | Set-Content -Encoding UTF8 $projectEnv
info "wrote $projectEnv (FFMPEG_DIR) - restart the gitea-act-runner scheduled task to pick it up"

# --- Azure Artifact Signing (formerly Trusted Signing) toolchain, for the signing step in
# windows-host.yml + windows-client.yml. Two pieces, neither of which the generic unom/infra image
# carries, and both of which fail in ways that do not name themselves:
#
#   1. The .NET 8 runtime. Azure.CodeSigning.Dlib.dll is a mixed-mode (C++/CLI) assembly - it ships
#      Ijwhost.dll and a runtimeconfig.json pinning Microsoft.NETCore.App 8.0.0 - so on a box with
#      no .NET runtime, signtool exits 3 having printed NOTHING AT ALL. Verified on .133 2026-08-14:
#      the box had pwsh 7 (self-contained, brings no shared runtime) and no dotnet whatsoever.
#   2. The signing client, installed MACHINE-WIDE under C:\trusted-signing rather than into a user's
#      .nuget. The act_runner daemon runs as SYSTEM, whose USERPROFILE is
#      C:\Windows\System32\config\systemprofile - so a per-user install under Administrator is
#      invisible to every job that actually builds. Find-AzureDlib in both pack scripts searches
#      this exact path for that reason; verified by resolving it from a SYSTEM scheduled task.
#
# Both are SHA-256 pinned against version-immutable URLs (a nuget.org flat-container package and the
# dotnet builds CDN are both immutable per version), so these fail closed on tampering rather than
# every time Microsoft ships a patch release. Bump version + hash together to move either. ---
$dotnetVer = '8.0.30'
$dotnetSha = 'E40F199C6D5584AFF0554C01163C3C8D9CCF6BEC3A577E4D967E41070772A1C1'
$tscVer = '1.0.95'
$tscSha = '3BFCF1E0A3CB42AF1692F0A8ED45C15DE070C2DE86F28A59B2795D904D8A920F'

if (Test-Path 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App') {
  info "shared .NET runtime already present ($((Get-ChildItem 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App' | ForEach-Object Name) -join ', '))"
} else {
  info "installing .NET $dotnetVer runtime (required by Azure.CodeSigning.Dlib.dll)"
  $dn = "$env:TEMP\dotnet-runtime-$dotnetVer-win-x64.exe"
  Invoke-WebRequest -Uri "https://builds.dotnet.microsoft.com/dotnet/Runtime/$dotnetVer/dotnet-runtime-$dotnetVer-win-x64.exe" -OutFile $dn -UseBasicParsing
  $got = (Get-FileHash $dn -Algorithm SHA256).Hash
  if ($got -ne $dotnetSha) { Remove-Item $dn -Force; throw ".NET runtime download hash mismatch (got $got, pinned $dotnetSha)." }
  # -Wait is load-bearing: the bundle is a GUI PE that returns immediately when invoked with &,
  # leaving $LASTEXITCODE unset and racing any completion check against the install.
  $p = Start-Process -FilePath $dn -ArgumentList '/install', '/quiet', '/norestart' -Wait -PassThru
  Remove-Item $dn -Force -ErrorAction SilentlyContinue
  if ($p.ExitCode -ne 0) { throw ".NET runtime installer exited $($p.ExitCode)." }
  if (-not (Test-Path 'C:\Program Files\dotnet\shared\Microsoft.NETCore.App')) { throw ".NET runtime installer reported success but installed no shared runtime." }
}

$tscDir = "C:\trusted-signing\microsoft.trusted.signing.client\$tscVer"
if (Test-Path (Join-Path $tscDir 'bin\x64\Azure.CodeSigning.Dlib.dll')) {
  info "Trusted Signing client $tscVer already present at $tscDir"
} else {
  info "installing Microsoft.Trusted.Signing.Client $tscVer (machine-wide, for SYSTEM)"
  $nupkg = "$env:TEMP\microsoft.trusted.signing.client.$tscVer.nupkg"
  Invoke-WebRequest -Uri "https://api.nuget.org/v3-flatcontainer/microsoft.trusted.signing.client/$tscVer/microsoft.trusted.signing.client.$tscVer.nupkg" -OutFile $nupkg -UseBasicParsing
  $got = (Get-FileHash $nupkg -Algorithm SHA256).Hash
  if ($got -ne $tscSha) { Remove-Item $nupkg -Force; throw "Trusted Signing client download hash mismatch (got $got, pinned $tscSha)." }
  if (Test-Path $tscDir) { Remove-Item -Recurse -Force $tscDir }
  New-Item -ItemType Directory -Force -Path $tscDir | Out-Null
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  [System.IO.Compression.ZipFile]::ExtractToDirectory($nupkg, $tscDir)
  Remove-Item $nupkg -Force -ErrorAction SilentlyContinue
  Get-ChildItem -Path $tscDir -Recurse -File | Unblock-File -ErrorAction SilentlyContinue
  if (-not (Test-Path (Join-Path $tscDir 'bin\x64\Azure.CodeSigning.Dlib.dll'))) { throw "extracted $tscVer but bin\x64\Azure.CodeSigning.Dlib.dll is absent." }
}

info "punktfunk extras provisioned OK."
