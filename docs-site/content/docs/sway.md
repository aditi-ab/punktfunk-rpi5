---
title: Sway / wlroots
description: Configure a punktfunk host on a wlroots compositor (Sway, River).
---

Sway (and other wlroots-proper compositors like River) can host: the host adds a per-client headless
output at the client's exact mode with `swaymsg create_output` and captures it through the
xdg-desktop-portal-wlr (xdpw) ScreenCast portal, injecting input via the wlroots virtual
pointer/keyboard protocols.

> On **Hyprland**? It's a separate first-class backend (its own `hyprctl` IPC and xdph portal) —
> see [Hyprland](/docs/hyprland). This page is for sway and other wlroots-proper compositors.

This is **not a primary target.** It works and is validated live on **sway 1.11** (zero-copy), but it
sees far less testing than the KDE and GNOME paths — expect rougher edges. If you have a choice,
[KDE](/docs/kde) or [GNOME](/docs/gnome) are the better-exercised desktops.

This page assumes the package is already installed — see [Arch](/docs/arch), [Ubuntu](/docs/ubuntu),
or [Fedora](/docs/fedora).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

The host auto-detects a wlroots session, so you usually need nothing here. To force the backend, set
these in `~/.config/punktfunk/host.env`:

```ini
PUNKTFUNK_COMPOSITOR=wlroots      # aliases: sway, wlr (Hyprland has its own: PUNKTFUNK_COMPOSITOR=hyprland)
PUNKTFUNK_INPUT_BACKEND=wlr
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy capture→encode is ON by default; auto-falls back to CPU. Set PUNKTFUNK_ZEROCOPY=0 to force CPU.
```

See [Configuration](/docs/configuration) for the full reference.

## How it works

- **Video** — the host adds a headless output at the client's exact mode with `swaymsg create_output`.
  This uses Sway's IPC specifically; other wlroots-proper compositors (River, …) are best-effort on
  this path. (Hyprland is driven by its own [backend](/docs/hyprland), not this one.)
- **Capture** — it captures that output through the **xdg-desktop-portal-wlr (xdpw)** ScreenCast
  portal. The host writes a managed chooser config so the output pick is automatic — no interactive
  picker dialog to answer.
- **Input** — mouse and keyboard are injected via the wlroots **virtual pointer** and **virtual
  keyboard** protocols.

For how long the virtual output lives, and extend-vs-exclusive topology, see
[Virtual displays](/docs/virtual-displays).

## Requirements

- A running wlroots-proper session (Sway, River, …). On Hyprland, use the
  [Hyprland backend](/docs/hyprland) instead.
- **xdg-desktop-portal-wlr (xdpw)** installed and running — the host captures through its ScreenCast
  portal. Without it there is no video.

## Start the host

With the backend selected, start the host from **inside your Sway session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f
```

## Bring up the console and pair

Enable the web console, read its login password, and arm PIN pairing — see
[The Web Console](/docs/web-console). Then [connect a client](/docs/clients).
