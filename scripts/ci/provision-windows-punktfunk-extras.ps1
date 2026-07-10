# Layers punktfunk-specific tooling onto the shared unom Windows CI runner: per-arch FFmpeg
# (host + client native builds), Inno Setup (the host installer), and the aarch64-pc-windows-msvc
# rustup target (windows-msix.yml's ARM64 leg). The runner itself - act_runner, Node, rustup,
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

# --- ARM64 cross-compile target (windows.yml / windows-msix.yml build aarch64-pc-windows-msvc off
# this x64 box; the ARM64 MSVC cross compiler itself comes from unom/infra's generic VS Build
# Tools provisioning, which already includes the ARM64 component). ---
$rustup = "C:\Users\Public\.cargo\bin\rustup.exe"
if (Test-Path $rustup) {
  info "rustup target add aarch64-pc-windows-msvc"
  & $rustup target add aarch64-pc-windows-msvc
} else {
  Write-Warning "rustup not found at $rustup - has unom/infra's setup-gitea-runner-base.ps1 run on this box yet?"
}

# --- FFmpeg shared trees for the host (amf-qsv encode) + clients (decode). BtbN **lgpl-shared**
# builds: the AMD/Intel AMF + Intel QSV encoders, swscale, and the HEVC decoder are all present in
# the LGPL build, and punktfunk never calls the GPL-only encoders (x264/x265 - software encode is
# the separate BSD-2 openh264 crate; NVENC is the direct NVIDIA SDK). lgpl-shared keeps the
# bundled DLLs LGPL-2.1+ (dynamic linking satisfies the relink duty) rather than GPL, so the
# shipped installer/MSIX stay consistent with punktfunk's MIT OR Apache-2.0 posture.
# MIGRATION: a runner previously provisioned with the old *gpl-shared* trees must be
# re-provisioned - delete C:\Users\Public\ffmpeg and C:\Users\Public\ffmpeg-arm64, then re-run.
# These DLLs are bundled verbatim into the code-signed host installer/MSIX, so the download is
# SHA-256-pinned (like VB-CABLE below): BtbN's `latest` tag is a ROLLING release whose assets are
# re-uploaded over time, so an unverified fetch would let a hijacked/MITM'd upstream asset land
# signed DLLs in users' installs. The pins below were captured 2026-07-10 from the then-current
# n7.1 lgpl-shared build. When BtbN re-rolls `latest`, this fetch FAILS CLOSED (hash mismatch) —
# that is intentional: re-download, re-verify the new archive, and update the two pins here.
#   Refresh a pin:  (Get-FileHash .\ffmpeg-<tag>.zip -Algorithm SHA256).Hash
function Get-BtbnFfmpeg {
  param([string]$Dir, [string]$ZipTag, [string]$Sha)   # ZipTag: 'win64' (x64) or 'winarm64' (ARM64 cross tree)
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
Get-BtbnFfmpeg -Dir "C:\Users\Public\ffmpeg"       -ZipTag 'win64'    -Sha '89F3469706E5D53AEA5CF34AEE63E62CE746E6159D7AEE473D330B02A47558E6'
Get-BtbnFfmpeg -Dir "C:\Users\Public\ffmpeg-arm64" -ZipTag 'winarm64' -Sha 'D96B4CE08CEBDCC6AD0E3934A3F962915E440EEFB9D73831AFEA4D80E35129A5'

# --- Vulkan-Headers (pf-ffvk's bindgen: libavutil/hwcontext_vulkan.h includes <vulkan/vulkan.h>,
# and Windows has no system copy). Headers only - the loader (vulkan-1.dll) is a GPU-driver
# component and is never linked at build time, so the full Vulkan SDK is deliberately NOT
# required. Pinned Khronos tag; bump deliberately alongside FFmpeg/driver expectations. ---
$vkHdrDir = "C:\Users\Public\vulkan-headers"
$vkHdrTag = "v1.4.309"
if (-not (Test-Path (Join-Path $vkHdrDir 'include\vulkan\vulkan.h'))) {
  info "fetching Vulkan-Headers $vkHdrTag"
  $url = "https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/$vkHdrTag.zip"
  $zip = "$vkHdrDir.zip"; $tmp = "$vkHdrDir-extract"
  Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force   # one top-level Vulkan-Headers-<ver> folder
  $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
  if (Test-Path $vkHdrDir) { Remove-Item -Recurse -Force $vkHdrDir }
  Move-Item -Path $inner.FullName -Destination $vkHdrDir
  Remove-Item -Force $zip; Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
} else { info "Vulkan-Headers already present at $vkHdrDir" }

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

# --- VB-CABLE (the streaming virtual microphone the host installer bundles). Pinned official
# package, SHA-256 verified - a silent hash change means VB-Audio shipped a new pack: verify it,
# then update BOTH the pin here and the notice if terms changed (packaging/windows/licenses/
# VB-CABLE-NOTICE.txt). Donationware by VB-Audio (https://vb-audio.com), redistributed under
# VB-Audio's bundling grant; only the base cable, never A+B/C+D. windows-host.yml points
# VBCABLE_DIR here so pack-host-installer.ps1 bundles it. ---
$vbDir = "C:\Users\Public\vbcable"
$vbUrl = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip"
$vbSha = "B950E39F01AF1D04EA623C8F6D8EB9B6EA5C477C637295FABF20631C85116BFB"
if (-not (Test-Path (Join-Path $vbDir 'VBCABLE_Setup_x64.exe'))) {
  info "fetching VB-CABLE (official base package, pinned)"
  $vbZip = "$vbDir.zip"
  Invoke-WebRequest -Uri $vbUrl -OutFile $vbZip -UseBasicParsing
  $got = (Get-FileHash $vbZip -Algorithm SHA256).Hash
  if ($got -ne $vbSha) { Remove-Item $vbZip -Force; throw "VB-CABLE download hash mismatch (got $got, pinned $vbSha) - vendor package changed; re-verify before re-pinning." }
  if (Test-Path $vbDir) { Remove-Item -Recurse -Force $vbDir }
  Expand-Archive -Path $vbZip -DestinationPath $vbDir -Force   # flat zip (setup exes + signed drivers)
  Remove-Item $vbZip -Force
  info "VB-CABLE staged at $vbDir"
} else { info "VB-CABLE already present at $vbDir" }

# --- Drop punktfunk's env vars into the generic runner's daemon wrapper extension point (see
# unom/infra's scripts/setup-gitea-runner-base.ps1) so the act_runner daemon - and therefore every
# job it runs - sees FFMPEG_DIR without unom/infra needing to know punktfunk exists. ---
$projectEnv = "C:\Users\Public\act-runner\project-env.ps1"
@'
$env:FFMPEG_DIR = "C:\Users\Public\ffmpeg"
$env:VBCABLE_DIR = "C:\Users\Public\vbcable"
$env:PF_FFVK_VULKAN_INCLUDE = "C:\Users\Public\vulkan-headers\include"
$env:PATH = "C:\Users\Public\ffmpeg\bin;" + $env:PATH
'@ | Set-Content -Encoding UTF8 $projectEnv
info "wrote $projectEnv (FFMPEG_DIR, VBCABLE_DIR, PF_FFVK_VULKAN_INCLUDE) - restart the gitea-act-runner scheduled task to pick it up"

info "punktfunk extras provisioned OK."
