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

## 3. Configure

The package ships the systemd **user** unit, the `/dev/uinput` udev rule, the socket-buffer sysctl
tuning, and an example config. As the desktop user, grant gamepad access and write the GNOME config:

```sh
sudo usermod -aG input "$USER"     # /dev/uinput for virtual gamepads (re-login to apply)
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

## 4. Run

Start the host as a user service from **inside your GNOME session** (so it can reach Mutter):

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f      # watch it come up + print its fingerprint
```

The host listens on UDP `9777` (native punktfunk/1) plus the GameStream ports, and advertises itself
over mDNS. It requires **PIN pairing** by default (secure on a LAN) — arm pairing from the web
console (next step) and pair once from your [client](/docs/clients).

### Web console

The console (status, paired devices, arm pairing) ships as `punktfunk-web`:

```sh
systemctl --user enable --now punktfunk-web
# read the auto-generated login password, then open http://<host-ip>:47992
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

#### Console login password

The console is password-protected. On first start `punktfunk-web-init` generates a random login
password and saves it to `~/.config/punktfunk/web-password` (as `PUNKTFUNK_UI_PASSWORD=…`). Read it
back at any time — from the init service's journal, or straight from the file:

```sh
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
```

To set your own password, edit that file (`PUNKTFUNK_UI_PASSWORD=<your-password>`) and restart the
console: `systemctl --user restart punktfunk-web`. Forgot it? This is the recovery path linked from
the console login screen — see [Forgot your Password?](/docs/forgot-password).

To run the host automatically at boot — including on a **headless** machine with no monitor — see
[Running as a Service](/docs/running-as-a-service).

## Troubleshooting

- **gnome-shell fails to start / "GPU … not supported by EGL":** the NVIDIA GL/EGL userspace is
  missing. Install `libnvidia-gl-<version>` (step 1) and confirm
  `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` exists.
- **Capture fails with "Session creation inhibited":** a **locked** GNOME session blocks screen
  capture. On a headless/always-on host, disable the lock — see
  [Running as a Service](/docs/running-as-a-service).
- More in [Troubleshooting](/docs/troubleshooting).

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

The host binary lands at `target/release/punktfunk-host`. Write `~/.config/punktfunk/host.env` as in
step 3, then run it inside your GNOME session:

```sh
cargo run --release -p punktfunk-host -- serve --gamestream
```

(The native plane is always on; `--gamestream` adds the Moonlight-compat surface this guide's
GameStream ports refer to — trusted LAN only. Drop it for a secure native-only host.)
