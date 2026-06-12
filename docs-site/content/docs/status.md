---
title: "Status & Progress"
description: "Where the work stands, what's live on each box, and a running progress log."
---

The living progress tracker. Milestone-level status lives in [`CLAUDE.md`](https://github.com)
and the design in the [Implementation Plan](/docs/implementation-plan); this page is the
**current state + a dated log** of what landed, kept up to date as work happens. Newest first.

## Milestones at a glance

| Milestone | State |
|---|---|
| **M1** — `punktfunk-core` + C ABI (protocol · FEC · crypto) | ✅ complete & hardened |
| **M2** — GameStream host (Moonlight-compatible) | ✅ working end-to-end; HDR/surround-audio polish open |
| **M3** — `punktfunk/1` native protocol (QUIC control + UDP data) | ✅ full session planes, validated live |
| **M4** — native client decode + present (Apple first) | 🟡 stage 1 done (first light); stage-2 presenter next |

## Live on the boxes

| Box | Role | Compositor | Notes |
|---|---|---|---|
| **home-worker-2** (dev) | KDE/KWin appliance | kwin (headless Plasma) | QEMU VM, passthrough RTX 5070 Ti; `serve --native` user unit |
| **home-worker-3** (GNOME) | GNOME/Mutter appliance | mutter (RecordVirtual) | RTX 4090; autologin GNOME Wayland; `serve --native` user unit. See [GNOME Box Setup](/docs/gnome-box) |
| **home-bazzite-1** | SteamOS-like host | gamescope | host-managed Steam session at client mode. See [Bazzite Setup](/docs/bazzite) |

All three appliances advertise over mDNS (`_punktfunk._udp`) and require PIN pairing by default.

## Progress log

### 2026-06-12
- **Apple client latency HUD** — `PunktfunkConnection.clockOffsetNs` (from the C-ABI getter) +
  `LatencyMeter` surface a skew-corrected **capture→client-receipt** p50/p95 in the macOS HUD: the
  first cross-machine latency the real Apple client reports. (Stage-1 `AVSampleBufferDisplayLayer`
  has no present callback, so decode→present is excluded — that needs the stage-2 presenter.)
  Needs an `xcframework` rebuild + `swift test` on the Mac to validate.
- **Skew handshake in the connector + C ABI** — `quic::clock_sync` is now a shared core helper used
  by both the reference client and `NativeClient`; the connector runs it at connect and exposes the
  host clock offset over the C ABI (`punktfunk_connection_clock_offset_ns`). This is the substrate
  the Apple client needs for the decode→present (glass-to-glass) term.
- **Wall-clock skew handshake** (`ClockProbe`/`ClockEcho`, 8 NTP rounds after `Start`) — makes the
  client's capture→reassembled latency valid **cross-machine**. Validated GNOME box → dev box:
  offset −1.57 ms removed, **p50 1.30 ms** skew-corrected. (`05bc9ab`)
- **Native LAN auto-discovery** — host advertises `_punktfunk._udp` (TXT: fingerprint, pairing,
  proto); `punktfunk-client-rs --discover` lists hosts. Validated cross-LAN. (`4fff464`)
- **Third test box stood up** — home-worker-3 (Ubuntu 26.04, RTX 4090, GNOME 50): first GNOME/Mutter
  zero-copy streaming on a real desktop; **1 Gbps probe clean** (625 MB/5 s, `send_dropped=0`).
  Two physical-NVIDIA gotchas documented in [GNOME Box Setup](/docs/gnome-box).
- **Encode|send thread split** validated on real NIC (`send_dropped=0` at 720p60 / 1080p120). (`b295a5b`)

### Earlier (see roadmap + git log)
- **1 Gbps data plane**: batched `sendmmsg`/`recvmmsg` + microburst-cap paced send thread.
- **Boot appliance**: headless KDE session + host systemd units (no login).
- **Speed test + settable bitrate**: negotiation + bandwidth probe (host side).
- **DualSense** UHID + haptics; gamepads live; mic uplink; AV1 + surround (unit/live-capture tested).

## In flight / next

See the [Roadmap](/docs/roadmap) for the ordered list. Near-term:
- **True glass-to-glass**: Apple client present-stamp (decode→present) + host render→capture term.
- **Apple stage-2 presenter** (`VTDecompressionSession` → `CAMetalLayer`).
- **Mandatory PIN pairing + delegated pairing approval**; concurrent sessions.
- **bazzite** kept up to date (currently offline; one rebuild behind).
