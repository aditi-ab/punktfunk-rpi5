---
title: "Headless KDE Box Setup"
description: "Run punktfunk on a headless box with a nested KWin/Plasma session — the boot-appliance pattern."
---

How to run a punktfunk host on a **headless** box (no physical display / KMS scanout) using the
**KWin** backend: a nested headless Plasma session on `WAYLAND_DISPLAY=wayland-kde`, captured into a
per-client virtual output. This is the dev-box pattern (a QEMU VM with a passthrough NVIDIA GPU, no
KMS scanout → everything renders offscreen via `renderD128`).

## Requirements

- **KWin ≥ 6.5.6** (headless `--virtual` gained `createVirtualOutput`), or a DRM backend. On a box
  with no KMS scanout, `kwin --drm` is impossible — use the headless/virtual path below.
- NVIDIA driver with GL/EGL userspace (see [Linux Host Setup](/docs/linux-setup) for the build deps).

## Bring up the session

The headless Plasma session is launched by [`scripts/headless/run-headless-kde.sh`](https://github.com),
which starts `kwin --virtual` on `wayland-kde` plus the full Plasma desktop (portals, polkit agent, a
supervised plasmashell). It sets the env Plasma needs — notably `XDG_MENU_PREFIX=plasma-`, without
which plasmashell runs but the launcher menu is empty:

```sh
# shell 1 — the compositor session
bash scripts/headless/run-headless-kde.sh 1920x1080

# shell 2 — the host
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE PUNKTFUNK_VIDEO_SOURCE=virtual \
  PUNKTFUNK_ZEROCOPY=1 cargo run -rp punktfunk-host -- serve --native
```

## Boot appliance (no login, comes up at boot)

Two user systemd units bring the whole thing up at boot with no interaction:

```sh
cp scripts/punktfunk-kde-session.service scripts/punktfunk-host.service ~/.config/systemd/user/
cp scripts/host.env.example ~/.config/punktfunk/host.env    # edit for the kwin backend
systemctl --user daemon-reload
systemctl --user enable punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"   # start user units at boot WITHOUT a login
reboot
```

`punktfunk-kde-session.service` runs the headless KWin/Plasma session; `punktfunk-host.service`
(`serve --native`) `After=`s it and starts listening immediately (it only touches the compositor
per session, so the ordering is soft). `host.env` for this backend:

```sh
WAYLAND_DISPLAY=wayland-kde
XDG_CURRENT_DESKTOP=KDE
PUNKTFUNK_COMPOSITOR=kwin
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
```

## Other backends

The same box can stream a **nested app** (no desktop) via the gamescope backend, or attach to GNOME
([GNOME Box Setup](/docs/gnome-box)) or Sway/wlroots. Each compositor keeps its own
`VirtualDisplay` backend — there's no cross-compositor protocol for client-sized outputs.
