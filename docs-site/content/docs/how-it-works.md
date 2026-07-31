---
title: How It Works
description: The ideas behind Punktfunk — per-client virtual displays, the two protocols, and trust.
---

You don't need to know any of this to use Punktfunk, but it helps to understand what's happening
when you connect.

## A virtual display, sized to your device

When a client connects, the host asks your desktop to create a **new virtual display** at exactly the
client's resolution and refresh rate, captures that display, and streams it. The virtual display is
real to your desktop — apps can be moved onto it, games open on it — but it isn't tied to any physical
monitor. When the client disconnects, the virtual display goes away.

That's why a 1080p60 laptop and a 1440p120 desktop can stream from the same host **at the same time**,
each at its own mode — they each get their own virtual display.

How the virtual display is created depends on your host:

| Host | How |
|---|---|
| **GNOME** (Mutter) | A virtual monitor via the screen-cast API |
| **KDE Plasma** (KWin) | A virtual output via KWin's screencast |
| **Bazzite / Steam** (gamescope) | A nested gamescope session launched at the client's mode |
| **[Hyprland](/docs/hyprland)** | A headless output added with `hyprctl`, captured through xdg-desktop-portal-hyprland |
| **Sway** (wlroots) | A headless output added to the running session |
| **Windows** | A virtual-display driver — including Punktfunk's own **indirect display driver** the host pushes frames straight into — a real virtual display, no physical monitor, even on the secure desktop |

That last one is the distinctive part on Windows: rather than only capturing an existing screen,
Punktfunk has **its own indirect display driver (IDD)**, and the host can push finished frames
**straight into the driver**. You get the same on-the-fly virtual display the Linux compositors give
you — at the client's exact mode, with no physical monitor or dummy HDMI dongle, and even on the
secure desktop (UAC / lock screen). That tight, push-based integration is unusual among Windows
streaming hosts.

## From screen to GPU to wire

Captured frames never touch the CPU on their way to the encoder — a **zero-copy GPU path** that keeps
latency low even at high resolutions and frame rates.

Which encoder runs depends on your GPU: **NVIDIA** → NVENC on both platforms; **AMD** → AMF and
**Intel** → QSV on Windows. On Linux AMD and Intel share one path — **Vulkan Video** for HEVC and
AV1, with **VAAPI** for H.264 and as the fallback when Vulkan encode isn't available. There's also a
GPU-less software H.264 encoder: on Windows the host picks it when it finds no supported GPU, and on
Linux you turn it on yourself (`PUNKTFUNK_ENCODER=software`, see [Configuration](/docs/configuration)).

Client and host then negotiate the codec: **HEVC** by default, **AV1** where both sides support it,
**H.264** on the software path, and — if you pick it on a wired link — **[PyroWave](/docs/pyrowave)**,
an intra-only wavelet codec that trades bandwidth for a fraction of a millisecond of codec latency.
**HDR** (10-bit BT.2020 PQ) rides the same path where the host's capture, its encoder, the codec and
your client all allow it; [HDR](/docs/hdr) has the four-link chain and what each host and client can
really do.

## Two protocols

Punktfunk speaks two protocols over the same host:

- **GameStream** — the protocol Moonlight uses. Start the host with `--gamestream` and any
  [Moonlight](/docs/moonlight) client connects with no special software. This is the most compatible way in.
- **punktfunk/1 (native)** — a purpose-built protocol with a QUIC control channel and a UDP data
  channel hardened with forward error correction and encryption. It's lower-latency and more resilient
  on imperfect networks, and it's what the [native clients](/docs/clients) (Apple, Linux, Windows,
  Android) use.

The native `punktfunk/1` plane runs by default (the secure default); add `--gamestream` and both planes
serve from a single host process — Moonlight clients use GameStream, the native clients use punktfunk/1.

## Pairing and trust

The first time a device connects, you pair it: the host shows a short **PIN**, you type it into the
client, and the two remember each other. After that the device reconnects automatically on a pinned
cryptographic identity — no PIN, no account, no cloud. See [Pairing & Trust](/docs/pairing).

## Finding hosts

Hosts advertise themselves on your local network, so clients can **discover** them automatically
instead of needing an IP address. The native clients and Moonlight both list hosts they find on the
LAN.

## Multiple devices at once

A host can stream to several clients simultaneously — your laptop and your TV both viewing (and
controlling) the desktop, each at its own resolution. See [Multiple devices](/docs/configuration#multiple-devices-at-once).
