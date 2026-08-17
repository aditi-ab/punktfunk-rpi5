---
title: Hyprland
description: Configure a Punktfunk host on a Hyprland session — headless output via hyprctl, capture via xdg-desktop-portal-hyprland.
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

The host auto-detects a Hyprland session, so the starter `~/.config/punktfunk/host.env` is one line:

```ini
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy capture→encode is ON by default; auto-falls back to CPU. Set PUNKTFUNK_ZEROCOPY=0 to force CPU.
```

To force the backend (CI/testing — note that pinning turns live-session auto-detection **off**, so
the host stops following session switches):

```ini
PUNKTFUNK_COMPOSITOR=hyprland
PUNKTFUNK_INPUT_BACKEND=wlr
```

See [Configuration](/docs/configuration) for the full reference.

## How it works

- **Video** — the host runs `hyprctl output create headless PF-1` and applies a monitor rule for the
  client's exact mode. Outputs are **named**, so there's no before/after diffing. The rule uses
  `hyprctl keyword monitor …` (the hyprlang config manager — the default on every release, 0.55
  included) and falls back to the Lua `hyprctl eval 'hl.monitor{…}'` only if you've opted into the
  Lua config manager. The host confirms the output actually adopted the mode before streaming.
- **Capture** — it captures that output through the **xdg-desktop-portal-hyprland (xdph)** ScreenCast
  portal. To pick the output without a GUI on a headless host, the host writes a managed
  `~/.config/hypr/xdph.conf` pointing xdph's `custom_picker_binary` at a small shim that selects the
  new output automatically — no interactive picker dialog to answer.
- **Window placement** — the headless output is an *extension*: it sits beside your real monitors and
  nothing promotes it or turns them off. Hyprland opens a new window on the **focused** monitor, so
  the host runs `hyprctl dispatch focusmonitor PF-…` — once when the output is ready, and again right
  before it launches anything from your library. Without that, games open on whichever physical
  monitor had focus and the stream shows a bare desktop.
- **Input** — mouse and keyboard are injected via the wlroots **virtual pointer** and **virtual
  keyboard** protocols (Hyprland kept them). Gamepads and audio are compositor-independent.

For how long the virtual output lives, and extend-vs-exclusive topology, see
[Virtual displays](/docs/virtual-displays).

## Requirements

- A running Hyprland session (the `hyprctl`/xdph contracts are verified on **0.55.4**; older
  releases share the same `hyprctl` surface).
- **xdg-desktop-portal-hyprland (xdph)** installed and running — the host captures through its
  ScreenCast portal, and steers its custom picker. Without it there is no video.
- **ScreenCast routed to xdph** — only if another portal backend (gtk, wlr) is installed alongside
  it. `xdg-desktop-portal` picks one implementation per interface, and if it hands ScreenCast to the
  wrong backend the host steers an xdph picker nobody is reading. Pin it for your session by creating
  `~/.config/xdg-desktop-portal/hyprland-portals.conf` (the name is your session's desktop —
  `XDG_CURRENT_DESKTOP`, which is `Hyprland` here — lowercased):

  ```ini
  [preferred]
  default=gtk
  org.freedesktop.impl.portal.ScreenCast=hyprland
  ```

  Then `systemctl --user restart xdg-desktop-portal`. On a box with only xdph installed there is
  nothing to choose between, so you can skip this.

## Troubleshooting: black / no video (headless output at 0×0)

A headless output only gets a framebuffer once the compositor can allocate one. On some GPU/driver
combinations (notably NVIDIA, and in nested test setups) that GBM/dmabuf allocation fails and the
output stays `0×0` — you'll see `GBM: Failed to allocate a GBM buffer: bo null` in the Hyprland log
(cf. [Sunshine #4197](https://github.com/LizardByte/Sunshine/issues/4197)). The host detects this
and fails the session with a clear error rather than streaming a blank surface. If you hit it,
capture the Hyprland log (`hyprctl` instance dir → `hyprland.log`) and check your GPU's GBM support;
running Hyprland as a real session (not nested) is the supported configuration.

## Troubleshooting: black client + "unavailable cursor mode 4"

A black client, `pipeline build failed` in the host log, and **`unavailable cursor mode 4`** from
xdph are one failure, not three.

`4` is the ScreenCast portal's *metadata* cursor mode, which the host prefers when the client draws
the pointer locally (desktop mouse mode). xdg-desktop-portal-hyprland **does not offer that mode** —
on a current stack (Hyprland 0.56.2, xdph 1.4.1) its `AvailableCursorModes` is `3`, meaning hidden
and embedded only. Asking for a mode the backend does not advertise is not a soft failure:
`xdg-desktop-portal` rejects the call outright, so the cast died during setup and the client had
nothing to show.

Updating xdph does **not** fix this — the mode is absent on current versions, not just old ones.
Hosts from this release check what your portal advertises and use an embedded cursor instead, so the
session streams. If you are on an older host, switch the client to **game mouse mode**: that stops
it asking for the metadata cursor at all.

If the pointer misbehaves on an xdph that *does* advertise metadata support, pin the mode:

```sh
PUNKTFUNK_PORTAL_CURSOR_MODE=embedded
```

See [Configuration](/docs/configuration#compositor-specific-linux).

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

This unit runs `serve --gamestream`, so it serves stock [Moonlight](/docs/moonlight) clients as well
as the native ones. For a native-only host, see
[What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Bring up the console and pair

Enable the web console, read its login password, and arm PIN pairing — see
[The Web Console](/docs/web-console). Then [connect a client](/docs/clients).
