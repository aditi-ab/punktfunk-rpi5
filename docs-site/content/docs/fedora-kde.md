---
title: Fedora — KDE Plasma
description: Set up a punktfunk host on Fedora with KDE Plasma (KWin).
---

Set up a punktfunk host on **Fedora KDE** (the KDE Plasma spin). Like the Ubuntu KDE setup, the host
uses KWin to create per-client virtual displays — the difference is the package manager and the NVIDIA
driver source.

> Fedora KDE is the newest supported setup. The flow mirrors [Ubuntu — KDE](/docs/ubuntu-kde); this
> page covers the Fedora-specific bits.

## 1. NVIDIA driver

The cleanest source on Fedora is **RPM Fusion**:

```sh
sudo dnf install \
  https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
  https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm
sudo dnf install akmod-nvidia xorg-x11-drv-nvidia-cuda
```

Let the `akmod` build finish (a few minutes), then reboot. Verify:

```sh
nvidia-smi
cat /sys/module/nvidia_drm/parameters/modeset   # should print Y (RPM Fusion enables it by default)
```

> With **Secure Boot** enabled, RPM Fusion's `akmods` need their key enrolled — follow the
> [RPM Fusion Secure Boot guide](https://rpmfusion.org/Howto/Secure%20Boot), or disable Secure Boot.

## 2. Dependencies

```sh
sudo dnf install gcc gcc-c++ make cmake clang clang-devel nasm git \
  pipewire pipewire-pulseaudio wireplumber pipewire-devel \
  wayland-devel wayland-protocols-devel libxkbcommon-devel opus-devel \
  libdrm-devel mesa-libgbm-devel mesa-libEGL-devel mesa-libGLES-devel libva-devel \
  ffmpeg-free-devel libei-devel
```

> Fedora ships **FFmpeg** through RPM Fusion (`ffmpeg` + `ffmpeg-devel`) or the `-free` packages
> shown above. Either works; the host builds against the system FFmpeg.

Install Rust:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## 3. Build

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release -p punktfunk-host
```

## 4. Configure and run

Same as Ubuntu KDE — write `~/.config/punktfunk/host.env` for KWin and run `serve --native`:

```sh
mkdir -p ~/.config/punktfunk
cat > ~/.config/punktfunk/host.env <<'ENV'
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=KDE
PUNKTFUNK_COMPOSITOR=kwin
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
ENV

cargo run --release -p punktfunk-host -- serve --native
```

Make sure you're on a **KDE Wayland** session with **KWin ≥ 6.5.6**. Then
[connect a client](/docs/clients). For boot-time startup, see
[Running as a Service](/docs/running-as-a-service).
