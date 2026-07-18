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
| Windows host (NVIDIA · AMD · Intel) | ✅ beta |
| Apple client (macOS · iOS · iPadOS · tvOS) | ✅ |
| Linux client (GTK4 shell + Vulkan session) | ✅ |
| Android client (phone · TV) | ✅ |
| Windows client | 🟡 |
| Web console + pairing | ✅ |
| Concurrent sessions (shared desktop) | ✅ |
| Network speed test + bitrate | ✅ |
| HDR / 10-bit streaming | ✅ Windows host · ⛔ Linux host |
| Surround audio (5.1 / 7.1) | ✅ |
| Sub-frame pipelining (latency) | 🔭 |

## ✅ Shipped

- **The host, two ways.** The lower-latency native [`punktfunk/1`](/docs/how-it-works) protocol (QUIC
  control + UDP data with GF(2¹⁶) Leopard FEC + AES-GCM) — the secure default — and, opt-in via
  `serve --gamestream`, a GameStream host any [Moonlight](/docs/moonlight) client can use. Both run
  from one process.
- **Native-resolution virtual displays** on Linux across KWin, GNOME/Mutter, gamescope, and
  Sway/wlroots, with a fully zero-copy GPU path to NVENC (stable 240 fps at 5120×1440).
- **A native Windows host** (x64; NVIDIA/AMD/Intel encode) — a signed installer with secure-desktop capture and a
  bundled virtual-display driver, and the only host that can stream **HDR** (10-bit BT.2020 PQ,
  captured from an HDR Windows desktop and encoded as HEVC Main10). See
  [Windows Host](/docs/windows-host). *(Beta — newer than the Linux host.)*
- **Clients on every platform** — native apps for **Apple** (macOS, iOS, iPadOS, tvOS), **Linux**,
  **Android** (phone + TV), and **Windows**, each with hardware decode, controllers including
  DualSense, audio + mic, and automatic host discovery. See [Clients](/docs/clients).
- **Secure by default** — SPAKE2 PIN pairing with pinned reconnects, one-click delegated approval from
  the web console, and mDNS LAN auto-discovery.
- **Tuned for latency** — concurrent sessions (stream one desktop to several devices at once),
  mid-stream resolution renegotiation, a cross-machine clock-skew handshake, a 1 Gbps+ data plane,
  an in-app network speed test that informs the bitrate picker, and **automatic adaptive bitrate**
  (the encoder re-targets mid-stream when bitrate is set to Automatic).
- **Surround sound** — 5.1 and 7.1 end to end: the host encodes multichannel via multistream Opus and
  the native clients render more than two channels, with clean, synchronous stereo where the path is stereo.
- **Clipboard sync** — bidirectional **text and images** between host and client, carried on a side
  plane over the native protocol's QUIC channel.

## 🟡 In progress

- **Windows client on-glass validation.** Hardware decode and the GUI are validated on NVIDIA and
  Intel; the HDR present path still needs verification on real HDR hardware.
- **Apple presenter polish.** The lower-latency `VTDecompressionSession` → `CAMetalLayer` stage-2
  path is now the default; HDR brightness and 4:4:4 still need on-glass validation.
- **Web console parity.** Surfacing the speed test and bitrate picker the apps already have.
- **Windows host hardening.** Broader real-world testing — especially on-glass validation of the
  AMD (AMF) and Intel (QSV) encode paths, which are CI-green but newer than NVENC.

## 🔭 Planned

- **Magic multi-user support.** Map a connecting client to a real user account on the host and log
  them in automatically. The client picks an identity — e.g. an **Apple TV profile** — which maps to
  an available host profile (likely behind a second per-user auth layer); on connect to an idle host,
  the user lands in *their own* desktop/session, signed in, without touching the host. Turns one box
  into a personal, profile-aware streaming target for a household.
- **Object-based spatial audio.** With 5.1/7.1 surround shipped, the frontier is object-based sound.
  Capturing game audio objects on native Windows is blocked by a closed renderer API, but **Proton
  already routes them through Wine's `ISpatialAudioClient`** — where it currently *discards* the
  dynamic/height objects and their 3D positions. Finishing that rendering would give every Proton gamer
  real spatial sound, and tunnelling the objects + positions to the client would let punktfunk spatialize
  them *on the client* — head-tracked remote spatial audio (to AirPods and the like) that no streaming
  stack does today.
- **Sub-frame pipelining.** Overlap encode and transmit within a single frame (a direct NVENC slice
  path) — the next big latency lever at high resolutions.
- **True glass-to-glass latency** measured end to end (capture → on-screen present).
- **gamescope multi-user isolation.** Per-session input and audio so concurrent clients are fully
  independent desktops (the shared-desktop case already works).
- **Peer-approved pairing.** Approve a new device from an already-paired device's own app.
- **WAN / anywhere access.** Reach a host beyond the LAN — NAT traversal (ICE/STUN/TURN) plus a
  self-hostable relay for when no direct path can be punched, and **QUIC connection migration** so a
  client roams between networks (Wi-Fi ↔ cellular) without dropping the session. The control plane is
  already QUIC over UDP, so this is mostly signalling and relay rather than a protocol change.
- **VRR / adaptive-sync passthrough.** Variable refresh end to end — the host renders at a variable
  rate and the client presents with tearing-control/VRR instead of a fixed cadence, for tear- and
  judder-free gaming. Builds on the client's presentation-feedback path and the per-session virtual
  outputs.
- **Desktop quality-of-life.** More of the essentials that make remote *work* pleasant, each a new side
  plane over the existing QUIC datagram channel: **multi-monitor streaming** (present the host's several
  outputs as separate client windows) and **virtual-webcam redirection** (the client's camera shows up
  as a webcam on the host, so video calls run on the remote machine). *(Clipboard sync already shipped —
  see above.)*

## ⛔ Parked / blocked

- **HDR / 10-bit on the *Linux* host.** HDR streaming already works from a
  [Windows host](/docs/windows-host) to an HDR-capable client (Windows, Android). On Linux it's
  blocked upstream — no shipping compositor emits a 10-bit/HDR capture stream yet — and ready the
  moment one does.
- **Advanced DualSense voice-coil haptics.** Scoped and shelved (it rides the controller's USB audio
  interface, with near-zero game support on Linux). Adaptive triggers, rumble, and the lightbar
  already ship.
