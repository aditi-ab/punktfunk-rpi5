---
title: "Roadmap"
description: "What's shipped, what's in progress, and what's next for punktfunk."
---

A quick map of where punktfunk is today and where it's heading. For the detailed, dated changelog,
see [Status & Progress](/docs/status).

**Legend:** ✅ shipped · 🟡 in progress · 🔭 planned · ⛔ blocked upstream

## At a glance

| Area | |
|---|---|
| Protocol core — FEC · crypto · C ABI | ✅ |
| GameStream host (works with Moonlight) | ✅ |
| Native `punktfunk/1` protocol | ✅ |
| Linux host (KWin · GNOME · gamescope · Sway) | ✅ |
| Windows host (NVIDIA) | ✅ beta |
| Apple client (macOS · iOS · iPadOS · tvOS) | ✅ |
| Linux client (GTK4) | ✅ |
| Android client (phone · TV) | ✅ |
| Windows client | 🟡 |
| Web console + pairing | ✅ |
| Concurrent sessions (shared desktop) | ✅ |
| Network speed test + bitrate | ✅ |
| HDR / 10-bit streaming | ✅ Windows host · ⛔ Linux host |
| Sub-frame pipelining (latency) | 🔭 |

## ✅ Shipped

- **The host, two ways.** The lower-latency native [`punktfunk/1`](/docs/how-it-works) protocol (QUIC
  control + UDP data with GF(2¹⁶) Leopard FEC + AES-GCM) — the secure default — and, opt-in via
  `serve --gamestream`, a GameStream host any [Moonlight](/docs/moonlight) client can use. Both run
  from one process.
- **Native-resolution virtual displays** on Linux across KWin, GNOME/Mutter, gamescope, and
  Sway/wlroots, with a fully zero-copy GPU path to NVENC (stable 240 fps at 5120×1440).
- **A native Windows host** (NVIDIA, x64) — a signed installer with secure-desktop capture and a
  bundled virtual-display driver, and the only host that can stream **HDR** (10-bit BT.2020 PQ,
  captured from an HDR Windows desktop and encoded as HEVC Main10). See
  [Windows Host](/docs/windows-host). *(Beta — newer than the Linux host.)*
- **Clients on every platform** — native apps for **Apple** (macOS, iOS, iPadOS, tvOS), **Linux**,
  **Android** (phone + TV), and **Windows**, each with hardware decode, controllers including
  DualSense, audio + mic, and automatic host discovery. See [Clients](/docs/clients).
- **Secure by default** — SPAKE2 PIN pairing with pinned reconnects, one-click delegated approval from
  the web console, and mDNS LAN auto-discovery.
- **Tuned for latency** — concurrent sessions (stream one desktop to several devices at once),
  mid-stream resolution renegotiation, a cross-machine clock-skew handshake, a 1 Gbps+ data plane, and
  an in-app network speed test that informs the bitrate picker.

## 🟡 In progress

- **Windows client on-glass validation.** The hardware (D3D11VA) decode, HDR present, and GUI are
  built and ship as a signed MSIX — they just need verification on real GPU hardware.
- **Apple stage-2 presenter as the default.** The lower-latency `VTDecompressionSession` →
  `CAMetalLayer` path is live behind an opt-in flag and graduating to the default.
- **Web console parity.** Surfacing the speed test and bitrate picker the apps already have.
- **Windows host hardening.** Broader real-world testing, AMD/Intel encode (NVIDIA-only today), and
  bundling the ViGEm gamepad driver.

## 🔭 Planned

- **Sub-frame pipelining.** Overlap encode and transmit within a single frame (a direct NVENC slice
  path) — the next big latency lever at high resolutions.
- **True glass-to-glass latency** measured end to end (capture → on-screen present).
- **gamescope multi-user isolation.** Per-session input and audio so concurrent clients are fully
  independent desktops (the shared-desktop case already works).
- **Peer-approved pairing.** Approve a new device from an already-paired device's own app.

## ⛔ Parked / blocked

- **HDR / 10-bit on the *Linux* host.** HDR streaming already works from a
  [Windows host](/docs/windows-host) to an HDR-capable client (Windows, Android). On Linux it's
  blocked upstream — no shipping compositor emits a 10-bit/HDR capture stream yet — and ready the
  moment one does.
- **Advanced DualSense voice-coil haptics.** Scoped and shelved (it rides the controller's USB audio
  interface, with near-zero game support on Linux). Adaptive triggers, rumble, and the lightbar
  already ship.
