---
title: "GNOME / Mutter Host Setup"
description: "Bring up an Ubuntu GNOME desktop as a headless punktfunk appliance — the physical-NVIDIA gotchas, autologin, and the Mutter virtual-output path."
---

How to bring up an **Ubuntu GNOME** box as a punktfunk host using the **Mutter** backend (per-client
virtual output via the `RecordVirtual` D-Bus API, full zero-copy). Validated live on home-worker-3
(Ubuntu 26.04, RTX 4090, GNOME Shell 50). Two gotchas here that a QEMU VM never hits — a **physical**
NVIDIA box has Secure Boot and needs the GLVND EGL vendor — so they're called out explicitly.

## 1. NVIDIA driver under Secure Boot

Install the driver (`ubuntu-drivers` recommends the right `-open` build):

```sh
sudo apt-get install -y nvidia-driver-595-open    # match what `ubuntu-drivers devices` recommends
```

On a physical box with **Secure Boot enabled**, the DKMS module is signed with a local MOK that
isn't enrolled, so `modprobe nvidia` fails with **`Key was rejected by service`** and `nvidia-smi`
reports it can't talk to the driver. Enroll the MOK (no BIOS change, survives kernel/driver updates):

```sh
sudo mokutil --import /var/lib/shim-signed/mok/MOK.der   # set a throwaway one-time password
sudo reboot
# At the blue "Shim UEFI key management" screen on boot: Enroll MOK -> Continue -> Yes -> <password>.
# Needs console access (the screen is firmware-level, not reachable over SSH).
```

After reboot, `nvidia-smi` should show the GPU. (Alternatively, disable Secure Boot in firmware.)

## 2. GNOME Wayland needs the GLVND NVIDIA EGL vendor

If gnome-shell logs **`GPU /dev/dri/cardN ... not supported by EGL`** / `No EGL display` and
`libEGL warning: ... driver (null)`, GLVND has no NVIDIA EGL vendor and falls back to Mesa for the
NVIDIA card. The missing file is `/usr/share/glvnd/egl_vendor.d/10_nvidia.json`, shipped by
`libnvidia-gl-<DRV>` — which `nvidia-driver-NNN-open` does **not** always pull in:

```sh
sudo apt-get install -y libnvidia-gl-595         # provides 10_nvidia.json
```

(The EGL external-platform JSONs `10_nvidia_wayland` / `15_nvidia_gbm` are usually already present.)
`nvidia-drm modeset=1` must also be set (it's in `/etc/modprobe.d/nvidia-graphics-drivers-kms.conf`
on a normal install) for Wayland on NVIDIA.

## 3. A GNOME Wayland session for Mutter to attach to

The host attaches to a running GNOME **Wayland** session (`wayland-0`). On a headless box, autologin
provides it (a connected monitor also lets the session boot; a truly headless box would need
`gnome-shell --headless --virtual-monitor`). Enable GDM autologin:

```ini
# /etc/gdm3/custom.conf
[daemon]
AutomaticLoginEnable = true
AutomaticLogin = <your-user>
```

### Disable the screen lock (important for a headless appliance)

A **locked** GNOME session makes Mutter reject RemoteDesktop/ScreenCast with
`RemoteDesktop.CreateSession: ... Session creation inhibited` — so capture fails after the session
auto-locks on idle. There's no human to unlock a headless box, so disable it (in the user session):

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.screensaver idle-activation-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

## 4. Host config + appliance unit

`~/.config/punktfunk/host.env` for the Mutter backend:

```sh
XDG_RUNTIME_DIR=/run/user/1000
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=GNOME
PUNKTFUNK_COMPOSITOR=mutter
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
```

Run it as a persistent appliance — the standard user unit + linger (no kde-session unit needed here,
autologin provides the session):

```sh
mkdir -p ~/.config/systemd/user
cp scripts/punktfunk-host.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now punktfunk-host
sudo loginctl enable-linger "$USER"   # start the user unit at boot without an interactive login
```

`serve --native` then listens on GameStream + native QUIC (9777), creates a per-client Mutter virtual
output at the client's exact mode, and streams it zero-copy. Confirm it's up:
`punktfunk-client-rs --discover` from another box should list it.
