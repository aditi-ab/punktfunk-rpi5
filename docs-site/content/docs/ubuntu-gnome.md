---
title: Ubuntu — GNOME
description: Set up a punktfunk host on Ubuntu with the GNOME desktop (Mutter).
---

Set up a punktfunk host on **Ubuntu** (Desktop or Server) running **GNOME**. The host uses GNOME's
Mutter compositor to create a per-client virtual display. Tested on Ubuntu 24.04+ and GNOME 48+.

> New to this? Skim [Requirements](/docs/requirements) first.

## 1. NVIDIA driver

Install the recommended NVIDIA driver:

```sh
sudo ubuntu-drivers install      # or: sudo apt install nvidia-driver-<version>
```

Then make sure the **GL/EGL userspace** is present — GNOME on NVIDIA needs it, and the base driver
package doesn't always pull it in. Install the `libnvidia-gl` package matching your driver version:

```sh
sudo apt install libnvidia-gl-<version>   # e.g. libnvidia-gl-550
```

Reboot, then confirm the driver and KMS modeset:

```sh
nvidia-smi
cat /sys/module/nvidia_drm/parameters/modeset   # should print Y
```

If modeset is not `Y`:

```sh
echo 'options nvidia-drm modeset=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf
sudo update-initramfs -u && sudo reboot
```

> **Secure Boot:** on a machine with Secure Boot **enabled**, the NVIDIA kernel module won't load
> until you enrol its signing key. If `nvidia-smi` reports it can't talk to the driver, run
> `sudo mokutil --import /var/lib/shim-signed/mok/MOK.der` (set a one-time password), reboot, and
> choose **Enrol MOK** at the blue screen. Or disable Secure Boot in firmware.

## 2. Dependencies

Install the build toolchain and runtime libraries:

```sh
sudo apt install build-essential pkg-config cmake clang libclang-dev nasm git curl \
  pipewire pipewire-pulse wireplumber libpipewire-0.3-dev libspa-0.2-dev \
  libwayland-dev wayland-protocols libxkbcommon-dev libopus-dev \
  libdrm-dev libgbm-dev libegl-dev libgles-dev mesa-common-dev libva-dev \
  ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libnvidia-egl-wayland1 libnvidia-egl-gbm1 libei-dev
```

Install Rust if you don't have it:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## 3. Build

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release -p punktfunk-host
```

The host binary is at `target/release/punktfunk-host`.

## 4. Configure

The host reads its settings from `~/.config/punktfunk/host.env`. For GNOME:

```sh
mkdir -p ~/.config/punktfunk
cat > ~/.config/punktfunk/host.env <<'ENV'
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=GNOME
PUNKTFUNK_COMPOSITOR=mutter
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
ENV
```

See the [Configuration reference](/docs/configuration) for every option.

## 5. Run

From a terminal **inside your GNOME session** (so the host can reach Mutter):

```sh
cargo run --release -p punktfunk-host -- serve --native
```

The host starts listening, prints its fingerprint, and advertises itself on the network. Now
[connect a client](/docs/clients).

To run it automatically at boot — including on a **headless** machine with no monitor — see
[Running as a Service](/docs/running-as-a-service).

## Troubleshooting

- **gnome-shell fails to start / "GPU … not supported by EGL":** the NVIDIA GL/EGL userspace is
  missing. Install `libnvidia-gl-<version>` (step 1) and confirm
  `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` exists.
- **Capture fails with "Session creation inhibited":** a **locked** GNOME session blocks screen
  capture. On a headless/always-on host, disable the lock — see
  [Running as a Service](/docs/running-as-a-service).
- More in [Troubleshooting](/docs/troubleshooting).
