---
title: GNOME (Mutter)
description: Configure a punktfunk host for GNOME — host.env, the EGL/lock traps, and a headless session.
---

Configure a host running **GNOME**. The host drives GNOME's Mutter compositor to create a per-client
virtual display over D-Bus (`RecordVirtual`), zero-copy. This page assumes the host is already
installed — see [Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora), or [Arch](/docs/arch).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

Write `~/.config/punktfunk/host.env` with the GNOME settings. The host auto-detects the compositor
from your session, so the explicit `PUNKTFUNK_COMPOSITOR` is belt-and-braces:

```ini
# ~/.config/punktfunk/host.env
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=GNOME
PUNKTFUNK_COMPOSITOR=mutter
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
```

You must be on a **Wayland** session (not X11), and Mutter must be **≥ 48**. See the
[Configuration reference](/docs/configuration) for every option.

## The GL/EGL userspace

On NVIDIA, gnome-shell fails to start — or the host logs **"GPU … not supported by EGL"** — when the
NVIDIA GL/EGL userspace is missing. The base driver package doesn't always pull it in. Install your
distro's NVIDIA GL/EGL userspace package — on **Ubuntu/Debian** it's `libnvidia-gl-<version>` matching
your driver; on **Fedora/Arch** it ships with the RPM Fusion / repo driver — then confirm the glvnd
vendor file exists:

```sh
ls /usr/share/glvnd/egl_vendor.d/10_nvidia.json    # must exist
```

Installing the driver itself is covered on your distro's install page
([Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora), [Arch](/docs/arch)).

## Do not lock the session

A **locked** GNOME session blocks screen capture — the host fails with
**"Session creation inhibited"**. On an always-on or headless host there's no one to unlock it, so
disable the lock:

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

## Start the host

With `host.env` in place, start the host from **inside your GNOME session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f   # watch it come up and print its identity fingerprint
```

Then bring up [The Web Console](/docs/web-console) to arm pairing and connect a
[client](/docs/clients). For an always-on box, see the [headless session](#headless-session) below.

## Headless session

To run with no monitor and no login, keep a GNOME Wayland session up at all times and start the host
without a login. Have GDM auto-login your user:

```ini
# /etc/gdm3/custom.conf  (Ubuntu)   ·   /etc/gdm/custom.conf  (Fedora)
[daemon]
AutomaticLoginEnable = true
AutomaticLogin = your-user
```

Disable the lock (see [above](#do-not-lock-the-session)), then enable the host user service and let it
linger past logout:

```sh
systemctl --user enable --now punktfunk-host
sudo loginctl enable-linger "$USER"
```

Reboot and the host comes up on the auto-login session. Full walkthrough:
[Running as a Service](/docs/running-as-a-service).

## Troubleshooting

More fixes — black screen, discovery, pairing — in [Troubleshooting](/docs/troubleshooting).

Once the host is up, bring the console up and pair — see [The Web Console](/docs/web-console).
