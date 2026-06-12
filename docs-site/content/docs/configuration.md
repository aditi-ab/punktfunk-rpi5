---
title: Configuration
description: The host.env settings — compositor, resolution, bitrate, input — and how to tune them.
---

The host reads its settings from **`~/.config/punktfunk/host.env`** (a simple `KEY=value` file). Your
[setup guide](/docs/requirements) gives you a starting `host.env` for your desktop; this page is the
reference.

## Session settings

These tell the host which desktop session to attach to. Your setup guide sets them for you.

| Setting | What it does |
|---|---|
| `WAYLAND_DISPLAY` | The Wayland socket of your session (`wayland-0` for a normal desktop). |
| `XDG_CURRENT_DESKTOP` | Your desktop (`GNOME`, `KDE`). |
| `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` | Needed when the host runs outside your interactive session (e.g. as a service). |

## Core settings

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_COMPOSITOR` | `mutter` · `kwin` · `gamescope` · `wlroots` | Which backend creates the virtual display. Match your desktop. |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` · `portal` | `virtual` creates a per-client display at its exact mode (the normal choice). `portal` captures an existing monitor instead. |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` | GPU zero-copy capture→encode. Leave on; it falls back to a CPU path automatically. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `gamescope` · `wlr` · `uinput` | How input is injected. `libei` for GNOME/KDE, `gamescope` for Bazzite. |

## Resolution and refresh rate

You don't set these on the host — **the client chooses them**. When a device connects, the host
creates a virtual display at that device's resolution and refresh rate. A 1080p60 laptop and a
1440p120 desktop each get their own. (With Moonlight, set the mode in Moonlight's settings; with the
Apple app, it uses the device's display.)

## Bitrate

The client requests a bitrate; the host encodes to it. To find a good value for your link:

- **Apple app:** use the built-in **speed test** (a host card's menu → *Test Network Speed*). It
  measures your link and suggests a bitrate, then applies it.
- **Moonlight:** set the bitrate in Moonlight's settings. Start moderate and raise it.

## Multiple devices at once

A host can stream to several clients simultaneously — each gets its own virtual display at its own
resolution. This is the natural way to put your desktop on a laptop *and* a TV at the same time (both
see and control the same desktop).

The number of simultaneous streams is bounded by your GPU's encoder. Cap it with
`--max-concurrent N` on the host command line (default 4); extra clients wait until a slot frees.

## Codec and FEC

- The host encodes **HEVC (H.265)** by default; **AV1** is available for clients that support it.
- The native protocol adds forward error correction for lossy links. `PUNKTFUNK_FEC_PCT=N` sets the
  redundancy percentage (the default is sensible for a normal LAN).

## Diagnostics

- `PUNKTFUNK_PERF=1` logs per-stage timing (capture, encode, send) — handy when tuning latency.
- `RUST_LOG=info` (or `debug`) controls log verbosity.
