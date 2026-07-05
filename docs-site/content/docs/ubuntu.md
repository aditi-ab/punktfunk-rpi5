---
title: Ubuntu / Debian
description: Install the punktfunk host on Ubuntu or Debian with apt.
---

Install a punktfunk host on **Ubuntu** (Desktop or Server) or **Debian** from the apt registry. This
page covers the distro-level setup — GPU driver, package, gamepad access. It works with either GNOME
or KDE; how the host creates its virtual display and injects input is desktop-specific, so pick your
desktop on the [configure pages](#configure-your-desktop) afterward rather than here.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## 1. GPU driver

On **NVIDIA**, install the recommended driver:

```sh
sudo ubuntu-drivers install      # or: sudo apt install nvidia-driver-<version>
```

Then make sure the **GL/EGL userspace** is present — Wayland compositors on NVIDIA need it, and the
base driver package doesn't always pull it in. Install the `libnvidia-gl` package matching your
driver version:

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

On **AMD/Intel** none of the NVIDIA steps apply. Encode runs through VAAPI on the Mesa stack —
`mesa-va-drivers` on AMD, `intel-media-driver` on Intel — which your desktop install already provides.

## 2. Install the host (apt)

`punktfunk-host` is published as a `.deb` to the public Gitea apt registry, so the box installs and
updates with plain `apt`. The registry is public — no auth needed, just trust its signing key:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://git.unom.io/api/packages/unom/debian/repository.key \
  | sudo tee /etc/apt/keyrings/punktfunk.asc >/dev/null

echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] https://git.unom.io/api/packages/unom/debian stable main" \
  | sudo tee /etc/apt/sources.list.d/punktfunk.list

sudo apt update
sudo apt install punktfunk-host
```

`punktfunk-host` `Recommends` the browser console (`punktfunk-web`), so apt pulls it in by default.
The desktop *client* (`punktfunk-client`) is a separate package for the machine you stream *to* — not
installed on a host. The NVIDIA driver is **not** a dependency — you installed it out of band in
step 1. Later updates are just `sudo apt update && sudo apt upgrade`.

The `stable` component above is the stable channel. To track pre-release builds instead, see
[Release Channels](/docs/channels).

## 3. Grant gamepad access

Virtual gamepads inject through `/dev/uinput`, which is gated by the `input` group. Add yourself and
re-login so the new group membership takes effect:

```sh
sudo usermod -aG input "$USER"     # re-login to apply
```

## Configure your desktop

How the host creates its virtual display and injects input depends on your desktop, not your distro.
Continue on the page for the desktop you run — it covers your `host.env`, any compositor quirks, and
starting the host:

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Sway / wlroots](/docs/sway)

Then bring up [The Web Console](/docs/web-console) to arm pairing and connect your first
[client](/docs/clients). To run the host at boot — including fully **headless** — see
[Running as a Service](/docs/running-as-a-service).

## Appendix — build from source

If the apt registry doesn't have a build for your release, or you want to track `main` directly,
compile the host yourself (no clean updates / no packaged units — you wire those up by hand).

Install the build toolchain and runtime libraries:

```sh
sudo apt install build-essential pkg-config cmake clang libclang-dev nasm git curl \
  pipewire pipewire-pulse wireplumber libpipewire-0.3-dev libspa-0.2-dev \
  libwayland-dev wayland-protocols libxkbcommon-dev libopus-dev \
  libdrm-dev libgbm-dev libegl-dev libgles-dev mesa-common-dev libva-dev \
  ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libnvidia-egl-wayland1 libnvidia-egl-gbm1 libei-dev
```

Install Rust if you don't have it, then build:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release -p punktfunk-host
```

The host binary lands at `target/release/punktfunk-host`. Configure your desktop as above, then run
it from inside your session:

```sh
cargo run --release -p punktfunk-host -- serve --gamestream
```

(The native plane is always on; `--gamestream` adds the Moonlight-compat surface — trusted LAN only.
Drop it for a secure native-only host.)
