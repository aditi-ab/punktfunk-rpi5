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
function Get-BtbnFfmpeg {
  param([string]$Dir, [string]$ZipTag)   # ZipTag: 'win64' (x64) or 'winarm64' (ARM64 cross tree)
  if (Test-Path (Join-Path $Dir 'lib\avcodec.lib')) { info "FFmpeg ($ZipTag) already present at $Dir"; return }
  info "fetching FFmpeg ($ZipTag, BtbN lgpl-shared)"
  $url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n7.1-latest-$ZipTag-lgpl-shared-7.1.zip"
  $zip = "$Dir.zip"; $tmp = "$Dir-extract"
  Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force   # BtbN zips have one top-level folder
  $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
  if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
  Move-Item -Path $inner.FullName -Destination $Dir
  Remove-Item -Force $zip; Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Get-BtbnFfmpeg -Dir "C:\Users\Public\ffmpeg"       -ZipTag 'win64'
Get-BtbnFfmpeg -Dir "C:\Users\Public\ffmpeg-arm64" -ZipTag 'winarm64'

# --- Inno Setup (ISCC.exe) for the host installer build (windows-host.yml). pack-host-installer.ps1
# locates it at its fixed Program Files path, so it need not be on PATH - just present. ---
if (-not (Test-Path "C:\Program Files (x86)\Inno Setup 6\ISCC.exe")) {
  if (Get-Command choco -ErrorAction SilentlyContinue) {
    info "installing Inno Setup (ISCC)"
    choco install innosetup -y --no-progress
  } else { Write-Warning "Inno Setup not found and choco unavailable - install it for windows-host.yml." }
}

# --- Drop punktfunk's env vars into the generic runner's daemon wrapper extension point (see
# unom/infra's scripts/setup-gitea-runner-base.ps1) so the act_runner daemon - and therefore every
# job it runs - sees FFMPEG_DIR without unom/infra needing to know punktfunk exists. ---
$projectEnv = "C:\Users\Public\act-runner\project-env.ps1"
@'
$env:FFMPEG_DIR = "C:\Users\Public\ffmpeg"
$env:PATH = "C:\Users\Public\ffmpeg\bin;" + $env:PATH
'@ | Set-Content -Encoding UTF8 $projectEnv
info "wrote $projectEnv (FFMPEG_DIR) - restart the gitea-act-runner scheduled task to pick it up"

info "punktfunk extras provisioned OK."
