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
| **M4** — native client decode + present (Apple first) | 🟡 stage 1 live; stage-2 presenter built + decode-tested (opt-in, present needs live validation) |

## Live on the boxes

| Box | Role | Compositor | Notes |
|---|---|---|---|
| **home-worker-2** (dev) | KDE/KWin appliance | kwin (headless Plasma) | QEMU VM, passthrough RTX 5070 Ti; `serve --native` user unit |
| **home-worker-3** (GNOME) | GNOME/Mutter appliance | mutter (RecordVirtual) | RTX 4090; autologin GNOME Wayland; `serve --native` user unit. See [Ubuntu — GNOME](/docs/ubuntu-gnome) |
| **home-bazzite-1** | SteamOS-like host | gamescope | host-managed Steam session at client mode. See [Bazzite Setup](/docs/bazzite) |

All three appliances advertise over mDNS (`_punktfunk._udp`) and require PIN pairing by default.

## Progress log

### 2026-06-12
- **CI + deployment landed** (see the [CI & Docker](/docs/ci) guide). Gitea Actions, three
  workflows: Rust workspace checks inside the new `punktfunk-rust-ci` builder image (Ubuntu 26.04,
  full link-dep stack incl. a libcuda stub — 141/141 tests green in-container), web + docs-site
  build/typecheck, `docker.yml` building+pushing `punktfunk-web`/`punktfunk-docs`/`punktfunk-rust-ci`
  to the registry, and `apple.yml` (xcframework → `swift build`/`swift test`) on a new **host-mode
  macOS runner** (`home-mac-mini-1`, provisioned by `scripts/ci/setup-macos-runner.sh`; macOS
  Local-Network privacy forces it to run as a root LaunchDaemon). Host and native clients stay
  un-dockerized by design. **This site now deploys automatically**: `deploy-docs` ships it to
  unom-1:3220, Caddy serves <https://docs.punktfunk.unom.io> — live and verified.
- **Concurrent sessions** — the host no longer serves one client at a time. The accept loop spawns
  each session (`JoinSet`), bounded by `--max-concurrent` (default 4, a NVENC bound; overflow waits
  in the accept queue). Each session keeps its own virtual output + encoder; they share the
  host-lifetime input/audio/mic services — i.e. **multiple devices viewing/controlling the same
  desktop** on kwin/mutter/wlroots. Validated live on the GNOME box: two clients connected at once
  → **two independent Mutter virtual outputs (1280×720 + 1920×1080) streaming simultaneously**
  (39 MB + 48 MB). gamescope's *independent-desktops* (multi-user) isolation — per-session
  input/audio — is a follow-up.
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
  Two physical-NVIDIA gotchas documented in [Ubuntu — GNOME](/docs/ubuntu-gnome).
- **Encode|send thread split** validated on real NIC (`send_dropped=0` at 720p60 / 1080p120). (`b295a5b`)

### Earlier (see roadmap + git log)
- **1 Gbps data plane**: batched `sendmmsg`/`recvmmsg` + microburst-cap paced send thread.
- **Boot appliance**: headless KDE session + host systemd units (no login).
- **Speed test + settable bitrate**: negotiation + bandwidth probe (host side).
- **DualSense** UHID + haptics; gamepads live; mic uplink; AV1 + surround (unit/live-capture tested).

## In flight / next

See the [Roadmap](/docs/roadmap) for the ordered list. Near-term:
- **True glass-to-glass**: Apple client present-stamp (decode→present) + host render→capture term.
- **Apple stage-2 presenter** (`VTDecompressionSession` → `CAMetalLayer`) — built + decode-unit-tested + live-validated on glass behind the `punktfunk.presenter` flag (capture→present ~11 ms p50); make it the default after a few resolution/HDR checks.
- **Mandatory PIN pairing + delegated pairing approval** (an already-paired device approves a new one).
- **gamescope multi-user isolation** — per-session input/audio so concurrent sessions are independent
  desktops (the shared-desktop multi-view case landed).
- **bazzite** kept up to date (currently offline; one rebuild behind).
