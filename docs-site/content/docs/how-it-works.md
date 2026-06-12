---
title: How It Works
description: The ideas behind punktfunk — per-client virtual displays, the two protocols, and trust.
---

You don't need to know any of this to use punktfunk, but it helps to understand what's happening
when you connect.

## A virtual display, sized to your device

When a client connects, the host asks your desktop to create a **new virtual display** at exactly the
client's resolution and refresh rate, captures that display, and streams it. The virtual display is
real to your desktop — apps can be moved onto it, games open on it — but it isn't tied to any physical
monitor. When the client disconnects, the virtual display goes away.

That's why a 1080p60 laptop and a 1440p120 desktop can stream from the same host **at the same time**,
each at its own mode — they each get their own virtual display.

How the virtual display is created depends on your desktop:

| Desktop | How |
|---|---|
| **GNOME** (Mutter) | A virtual monitor via the screen-cast API |
| **KDE Plasma** (KWin) | A virtual output via KWin's screencast |
| **Bazzite / Steam** (gamescope) | A nested gamescope session launched at the client's mode |
| **Sway** (wlroots) | A headless output added to the running session |

## From screen to GPU to wire

Captured frames never touch the CPU on their way to the encoder. They go straight from the
compositor to the GPU's NVENC hardware encoder (HEVC/H.264/AV1) and out to the network — a **zero-copy
GPU path** that keeps latency low even at high resolutions and frame rates.

## Two protocols

punktfunk speaks two protocols over the same host:

- **GameStream** — the protocol Moonlight uses. Any [Moonlight](/docs/moonlight) client connects with
  no special software. This is the most compatible way in.
- **punktfunk/1 (native)** — a purpose-built protocol with a QUIC control channel and a UDP data
  channel hardened with forward error correction and encryption. It's lower-latency and more resilient
  on imperfect networks, and it's what the [Apple app](/docs/clients) uses.

Both run from a single host process, so you don't choose up front — Moonlight clients use GameStream,
the native clients use punktfunk/1.

## Pairing and trust

The first time a device connects, you pair it: the host shows a short **PIN**, you type it into the
client, and the two remember each other. After that the device reconnects automatically on a pinned
cryptographic identity — no PIN, no account, no cloud. See [Pairing & Trust](/docs/pairing).

## Finding hosts

Hosts advertise themselves on your local network, so clients can **discover** them automatically
instead of needing an IP address. The Apple app and Moonlight both list hosts they find on the LAN.

## Multiple devices at once

A host can stream to several clients simultaneously — your laptop and your TV both viewing (and
controlling) the desktop, each at its own resolution. See [Multiple devices](/docs/configuration#multiple-devices).
