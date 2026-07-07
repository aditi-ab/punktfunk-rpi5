---
title: Hyprland
description: Configure a punktfunk host on a Hyprland session — headless output via hyprctl, capture via xdg-desktop-portal-hyprland.
---

Hyprland is a **first-class backend.** The host adds a per-client headless output at the client's
exact mode with `hyprctl`, captures it through the **xdg-desktop-portal-hyprland (xdph)** ScreenCast
portal (zero-copy dmabuf), and injects input via the wlroots virtual pointer/keyboard protocols —
which Hyprland still implements even after dropping wlroots in v0.42.

This is a distinct backend from [Sway / wlroots](/docs/sway): Hyprland has its own IPC (`hyprctl`)
and its own portal (xdph), so it is auto-detected and driven separately.

This page assumes the package is already installed — see [Arch](/docs/arch), [Ubuntu](/docs/ubuntu),
or [Fedora](/docs/fedora).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

The host auto-detects a Hyprland session, so you usually need nothing here. To force the backend, set
these in `~/.config/punktfunk/host.env`:

```ini
PUNKTFUNK_COMPOSITOR=hyprland
PUNKTFUNK_INPUT_BACKEND=wlr
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy capture→encode is ON by default; auto-falls back to CPU. Set PUNKTFUNK_ZEROCOPY=0 to force CPU.
```

See [Configuration](/docs/configuration) for the full reference.

## How it works

- **Video** — the host runs `hyprctl output create headless PF-1` and applies a monitor rule for the
  client's exact mode. Outputs are **named**, so there's no before/after diffing. Both config eras
  are supported: `hyprctl keyword monitor …` (≤ 0.54) and the Lua `hyprctl eval 'hl.monitor{…}'`
  (≥ 0.55), selected from `hyprctl version`.
- **Capture** — it captures that output through the **xdg-desktop-portal-hyprland (xdph)** ScreenCast
  portal. To pick the output without a GUI on a headless host, the host writes a managed
  `~/.config/hypr/xdph.conf` pointing xdph's `custom_picker_binary` at a small shim that selects the
  new output automatically — no interactive picker dialog to answer.
- **Input** — mouse and keyboard are injected via the wlroots **virtual pointer** and **virtual
  keyboard** protocols (Hyprland kept them). Gamepads and audio are compositor-independent.

For how long the virtual output lives, and extend-vs-exclusive topology, see
[Virtual displays](/docs/virtual-displays).

## Requirements

- A running Hyprland session (any recent release; validate on both a ≤ 0.54 and a ≥ 0.55 install).
- **xdg-desktop-portal-hyprland (xdph)** installed and running — the host captures through its
  ScreenCast portal, and steers its custom picker. Without it there is no video.
- The ScreenCast interface routed to xdph — see `scripts/headless/portals.conf` (a `[Hyprland]`
  section pins `org.freedesktop.impl.portal.ScreenCast=hyprland`).

## Permission system

Hyprland's permission system (`ecosystem.enforce_permissions`, 0.49+, **off by default**) can deny
direct screencopy and virtual-input clients — and denial is **silent**: capture goes to *black
frames* and input is *dropped*, with no error. If you've enabled it, grant the host explicitly in
your Hyprland config:

```ini
ecosystem {
    enforce_permissions = true
}

permission = /usr/bin/punktfunk-host, screencopy, allow
permission = /usr/bin/punktfunk-host, virtual-pointer, allow
permission = /usr/bin/punktfunk-host, virtual-keyboard, allow
```

The host logs a warning at startup when it detects enforcement is on. (Adjust the binary path to
where your package installed `punktfunk-host`.)

## Start the host

With the backend selected, start the host from **inside your Hyprland session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f
```

## Bring up the console and pair

Enable the web console, read its login password, and arm PIN pairing — see
[The Web Console](/docs/web-console). Then [connect a client](/docs/clients).
