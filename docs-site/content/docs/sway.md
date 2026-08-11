---
title: Sway / wlroots
description: Configure a Punktfunk host on sway.
---

Sway can host: the host adds a per-client headless output at the client's exact mode with
`swaymsg create_output` and captures it through the xdg-desktop-portal-wlr (xdpw) ScreenCast portal,
injecting input via the wlroots virtual pointer/keyboard protocols.

Despite the backend's name, this path is **sway** specifically. Everything it does for video —
creating the headless output, setting its mode, listing your monitors — goes through sway's IPC
(`swaymsg`), so a wlroots compositor without that IPC (River, dwl, …) cannot host: the session fails
straight away with `swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)`.
Input would be fine there — it uses the wlroots virtual pointer/keyboard protocols, which those
compositors do have — but with no video there is no stream.

> On **Hyprland**? It's a separate first-class backend (its own `hyprctl` IPC and xdph portal) —
> see [Hyprland](/docs/hyprland). This page is for sway.

This is **not a primary target.** It works and is validated live on **sway 1.11** (zero-copy), but it
sees far less testing than the KDE and GNOME paths — expect rougher edges. If you have a choice,
[KDE](/docs/kde) or [GNOME](/docs/gnome) are the better-exercised desktops.

This page assumes the package is already installed — see [Arch](/docs/arch), [Ubuntu](/docs/ubuntu),
or [Fedora](/docs/fedora).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

The host auto-detects a wlroots session, so the starter `~/.config/punktfunk/host.env` is one line:

```ini
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy capture→encode is ON by default; auto-falls back to CPU. Set PUNKTFUNK_ZEROCOPY=0 to force CPU.
```

To force the backend (CI/testing — note that pinning turns live-session auto-detection **off**, so
the host stops following session switches):

```ini
PUNKTFUNK_COMPOSITOR=wlroots      # aliases: sway, wlr
PUNKTFUNK_INPUT_BACKEND=wlr
```

See [Configuration](/docs/configuration) for the full reference.

## How it works

- **Video** — the host adds a headless output at the client's exact mode with `swaymsg create_output`.
  This uses sway's IPC specifically, and so does everything else on the video side (mode setting,
  monitor listing). (Hyprland is driven by its own [backend](/docs/hyprland), not this one.)
- **Capture** — it captures that output through the **xdg-desktop-portal-wlr (xdpw)** ScreenCast
  portal. The host writes a managed chooser config so the output pick is automatic — no interactive
  picker dialog to answer.
- **Input** — mouse and keyboard are injected via the wlroots **virtual pointer** and **virtual
  keyboard** protocols.

For how long the virtual output lives, and extend-vs-exclusive topology, see
[Virtual displays](/docs/virtual-displays).

## Requirements

- A running **sway** session — its IPC socket (`SWAYSOCK`) is what the whole video path runs on. You
  don't have to export it: the host finds the live sway instance itself on every connect, so a
  `systemd --user` host works even though it never inherited your login shell's environment. On
  Hyprland, use the [Hyprland backend](/docs/hyprland) instead.
- **xdg-desktop-portal-wlr (xdpw)** installed and running — the host captures through its ScreenCast
  portal. Without it there is no video.
- **ScreenCast routed to xdpw** — only if another portal backend (gtk, gnome) is installed alongside
  it. `xdg-desktop-portal` picks one implementation per interface, and if it hands ScreenCast to the
  wrong backend the host steers an xdpw chooser nobody is reading. Pin it for your session by
  creating `~/.config/xdg-desktop-portal/sway-portals.conf`:

  ```ini
  [preferred]
  default=gtk
  org.freedesktop.impl.portal.ScreenCast=wlr
  ```

  Then `systemctl --user restart xdg-desktop-portal`. On a box with only xdpw installed there is
  nothing to choose between, so you can skip this.

## Start the host

With the backend selected, start the host from **inside your Sway session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f
```

This unit runs the secure native-only host. To also serve stock [Moonlight](/docs/moonlight)
clients, GameStream compat is opt-in (`PUNKTFUNK_GAMESTREAM=1` in `host.env`, trusted LANs only) —
see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Bring up the console and pair

Enable the web console, read its login password, and arm PIN pairing — see
[The Web Console](/docs/web-console). Then [connect a client](/docs/clients).
