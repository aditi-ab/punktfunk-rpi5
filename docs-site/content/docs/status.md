---
title: "Status & Progress"
description: "Where the work stands across the core, the host, and the native clients."
---

A high-level view of where punktfunk stands. The ordered plan of work is on the
[Roadmap](/docs/roadmap), and milestone-level detail lives in
[`CLAUDE.md`](https://git.unom.io/unom/punktfunk/src/branch/main/CLAUDE.md).

## Milestones at a glance

| Milestone | State |
|---|---|
| **Core** — `punktfunk-core` + C ABI (protocol · FEC · crypto) | ✅ complete & hardened |
| **GameStream host** (Moonlight-compatible) | ✅ working end-to-end; HDR/surround-audio polish open |
| **Native protocol** — `punktfunk/1` (QUIC control + UDP data, GF(2¹⁶) Leopard FEC + AES-GCM) | ✅ full session planes, validated live |
| **Windows host** (NVIDIA, x64) | 🟡 implemented & shipping as a signed installer; NVIDIA-only, newer than the Linux host |
| **macOS / iOS / iPadOS / tvOS client** | ✅ full client; on-glass stage-2 presenter behind an opt-in flag, becoming the default |
| **Linux client** (`punktfunk-client`, GTK4/libadwaita) | ✅ full client; VAAPI zero-copy decode + software fallback |
| **Windows client** (`punktfunk-client`, WinUI 3) | ✅ stage 1 complete; ships as signed MSIX; on-glass hardware validation pending |
| **Android client** (phone + Android TV) | ✅ full client; hardware HEVC decode + HDR10 |
| **Web console** (over the management API) | ✅ status · devices · pairing |

## What works today

punktfunk is a low-latency desktop and game streaming **host** with first-class **Linux and Windows**
support — and native **clients** on macOS, iOS/iPadOS/tvOS, Linux, Windows, and Android. (The Windows
host is newer than the Linux host.)

- **Two protocols.** The host speaks the **GameStream** protocol, so any **Moonlight**
  client works out of the box, plus its own lower-latency **`punktfunk/1`** protocol
  (QUIC control plane + UDP data plane with GF(2¹⁶) Leopard FEC and AES-GCM).
- **Native resolution, no scaling.** Every session gets a virtual display at the client's
  exact resolution and refresh rate, via per-compositor backends for **KWin**,
  **gamescope**, **Mutter**, and **Sway/wlroots**.
- **Zero-copy GPU pipeline.** Captured frames stay on the GPU (dmabuf → CUDA → NVENC) with
  automatic split-encode at very high resolutions. Stable 240 fps at 5120×1440 has been
  measured.
- **HDR (10-bit), on the Windows host.** An HDR Windows desktop is captured and encoded as HEVC
  Main10 (BT.2020 PQ) to HDR-capable clients (Windows, Android). Linux hosts stream 8-bit for now —
  HDR there is blocked upstream at the compositor.
- **Secure by default.** A **SPAKE2 PIN pairing** ceremony establishes trust (the host
  shows a 4-digit PIN; an attacker gets a single online guess, no offline dictionary
  attack). Trust-on-first-use (TOFU) remains an explicit opt-in for fully trusted LANs.
- **LAN auto-discovery.** Hosts advertise over mDNS (`_punktfunk._udp`); clients browse and
  list them automatically.
- **Full input.** Mouse, keyboard, and gamepads (including DualSense touchpad, motion,
  rumble, lightbar, player LEDs, and adaptive triggers) in both directions.
- **Audio both ways.** Opus desktop audio host → client, plus an opt-in, paired-only client
  microphone uplink.
- **Management surface.** A REST management API with a checked-in OpenAPI document, plus a
  web console for status, paired devices, and pairing.

### Native clients

| Client | Highlights |
|---|---|
| **macOS / iOS / iPadOS / tvOS** | VideoToolbox HEVC decode, GameController capture, full DualSense feedback, mDNS discovery, PIN pairing + TOFU, network speed test, latency HUD. Stage-2 presenter (`VTDecompressionSession` → `CAMetalLayer`, ~11 ms p50 capture→present) is built and validated on glass behind an opt-in flag, becoming the default. Ships as one universal TestFlight build / App Store listing. |
| **Linux** (`punktfunk-client`) | GTK4/libadwaita. FFmpeg decode with VAAPI → DRM-PRIME dmabuf zero-copy (Intel/AMD; software fallback on NVIDIA), PipeWire audio + mic, SDL3 gamepads incl. DualSense, mDNS discovery, PIN pairing + TOFU, speed test. Ships as Flatpak, apt, rpm, and Arch packages. |
| **Windows** (`punktfunk-client`) | WinUI 3. D3D11VA zero-copy decode, HDR10, WASAPI audio + mic, SDL3 gamepads incl. DualSense, mDNS discovery, and the full PIN/TOFU trust surface are all implemented. Ships as a signed MSIX (x86_64 + ARM64). **Stage 1 complete; D3D11VA decode, HDR present, and the GUI are pending on-glass validation on real GPU hardware.** |
| **Android** (phone + Android TV) | Kotlin app with a Rust core over JNI. NDK `AMediaCodec` hardware HEVC decode + HDR10 (Main10/BT.2020 PQ), Opus/Oboe audio + mic, gamepad input with rumble/HID feedback, mDNS discovery, PIN pairing + TOFU (Keystore identity), live stats HUD, and D-pad/controller focus navigation for TV. Ships to the Google Play Internal Testing track. |

`punktfunk-probe` is a headless reference and measurement client (for testing and
benchmarking, not everyday use).

## Validated live on

The stack has been validated live on a range of hosts and clients:

- **Hosts:** Ubuntu (GNOME / KDE), Fedora KDE, and Bazzite (gamescope) on machines with
  RTX-class NVIDIA GPUs, across the KWin, gamescope, Mutter, and Sway/wlroots backends.
- **Clients:** native macOS, Linux, and Android clients against live hosts, plus stock
  Moonlight clients over the GameStream path.
- **Cross-machine latency** is measured and skew-corrected (a wall-clock handshake removes
  the inter-machine clock offset), so capture-to-present numbers are valid across the LAN.

## Highlights

Notable capabilities that have landed, newest first:

- **Native Linux client (stage 1).** GTK4/libadwaita app that links `punktfunk-core`
  directly: mDNS host list, TOFU + SPAKE2 PIN pairing, FFmpeg HEVC decode, PipeWire audio
  with mic uplink, SDL3 gamepad capture with rumble/lightbar feedback, layout-independent
  keyboard, absolute mouse, fullscreen, and a stats overlay. VAAPI → `GdkDmabufTexture`
  zero-copy decode on Intel/AMD with a proven software fallback.
- **Delegated pairing approval.** An unpaired device that knocks on a pairing-required host
  appears as a pending request in the web console's Pairing page; one click approves and
  pins its certificate — no PIN fetched out of band.
- **Concurrent sessions.** The host serves multiple clients at once (bounded by an NVENC
  limit), each with its own virtual output and encoder — e.g. stream the same desktop to a
  laptop and a TV simultaneously.
- **Cross-machine latency HUD + wall-clock skew handshake.** A short NTP-style handshake
  aligns client and host clocks, making capture-to-reassembled latency valid across
  machines; the Apple client surfaces a skew-corrected capture-to-receipt p50/p95 in its
  HUD.
- **Native LAN auto-discovery.** Hosts advertise `_punktfunk._udp` over mDNS (with TXT
  records carrying the protocol, cert fingerprint, and pairing requirement); clients
  discover and list them automatically.
- **1 Gbps data plane.** Batched `sendmmsg`/`recvmmsg`, a microburst-capped paced send
  thread, and larger socket buffers, exploiting the GF(2¹⁶) Leopard FEC that breaks the
  classic ~1 Gbps GameStream ceiling.
- **Boot appliance.** A headless compositor session plus host systemd units, so a host can
  come up and stream with no interactive login.
- **Network speed test + settable bitrate.** Bitrate negotiation and an in-band bandwidth
  probe inform the client's bitrate picker instead of guesswork.
- **Rich DualSense.** A full UHID DualSense backend on the host (gamepad, motion, touchpad,
  lightbar, player LEDs, adaptive triggers) with feedback carried back to the clients.
- **AV1 + surround audio** are implemented and unit/live-capture tested.

## In flight / next

See the [Roadmap](/docs/roadmap) for the ordered list. Near-term:

- **True glass-to-glass latency** — combine the client present-stamp (decode → present)
  with the host render → capture term for a complete end-to-end number.
- **Make the Apple stage-2 presenter the default** after a few more resolution/HDR checks.
- **On-glass validation of the Windows client** (D3D11VA decode, HDR present, GUI) on real
  GPU hardware.
- **gamescope multi-user isolation** — per-session input/audio so concurrent sessions can
  be fully independent desktops (the shared-desktop multi-view case already works).
