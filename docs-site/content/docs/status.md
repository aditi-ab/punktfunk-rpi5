---
title: "Status & Progress"
description: "Where the work stands across the core, the host, and the native clients."
---

A high-level view of where punktfunk stands. The ordered plan of work is on the
[Roadmap](/docs/roadmap).

## Milestones at a glance

| Milestone | State |
|---|---|
| **Core** — `punktfunk-core` + C ABI (protocol · FEC · crypto) | ✅ complete & hardened |
| **GameStream host** (Moonlight-compatible) | ✅ working end-to-end; HDR/surround-audio polish open |
| **Native protocol** — `punktfunk/1` (QUIC control + UDP data, GF(2¹⁶) Leopard FEC + AES-GCM) | ✅ full session planes, validated live |
| **Windows host** (x64) | 🟡 implemented & shipping as a signed installer; NVIDIA/AMD/Intel encode, newer than the Linux host |
| **macOS / iOS / iPadOS / tvOS client** | ✅ full client; on-glass-validated stage-2 presenter is the default |
| **Linux client** (`punktfunk-client`, relm4/GTK4 shell + Vulkan session) | ✅ full client; Vulkan Video → VAAPI → software decode |
| **Windows client** (`punktfunk-client`, WinUI 3) | ✅ ships as signed MSIX; hardware decode validated on NVIDIA + Intel; HDR on-glass validation pending |
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
  measured. A GPU-less software H.264 encoder exists as an explicit fallback.
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
| **macOS / iOS / iPadOS / tvOS** | VideoToolbox HEVC + AV1 decode (AV1 on hardware that decodes it — M3-class Macs, A17 Pro-class iPhones), GameController capture, full DualSense feedback, mDNS discovery, PIN pairing + TOFU, network speed test, latency HUD. Stage-2 presenter (`VTDecompressionSession` → `CAMetalLayer`, ~11 ms p50 capture→present) is validated on glass and is the default (stage 1 remains the fallback when Metal is unavailable). Ships as one universal TestFlight build / App Store listing. |
| **Linux** (`punktfunk-client`) | relm4/GTK4 shell; streams run in the spawned Vulkan session presenter. Hardware decode via Vulkan Video (all vendors, incl. NVIDIA) with VAAPI dmabuf and software fallbacks, PipeWire audio + mic, SDL3 gamepads incl. DualSense, mDNS discovery, PIN pairing + TOFU, speed test, Skia console UI + tiered stats overlay. Ships as Flatpak, apt, rpm, and Arch packages. |
| **Windows** (`punktfunk-client`) | WinUI 3 shell; streams run in the Vulkan session presenter with a Vulkan Video → D3D11VA → software decode chain, HDR10, WASAPI audio + mic, SDL3 gamepads incl. DualSense, mDNS discovery, and the full PIN/TOFU trust surface. Ships as a signed MSIX (x86_64 + ARM64). **Hardware decode validated on NVIDIA and Intel; HDR present pending on-glass validation.** |
| **Android** (phone + Android TV) | Kotlin app with a Rust core over JNI. NDK `AMediaCodec` hardware HEVC decode + HDR10 (Main10/BT.2020 PQ), Opus/Oboe audio + mic, gamepad input with rumble/HID feedback, mDNS discovery, PIN pairing + TOFU (Keystore identity), tiered stats overlay (three-finger tap), custom resolutions, and D-pad/controller focus navigation for TV. Ships to the Google Play Internal Testing track. |

`punktfunk-probe` is a headless reference and measurement client (for testing and
benchmarking, not everyday use).

## Validated live on

The stack has been validated live on a range of hosts and clients:

- **Hosts:** Ubuntu (GNOME / KDE), Fedora KDE, and Bazzite (gamescope) on machines with
  RTX-class NVIDIA GPUs, across the KWin, gamescope, Mutter, and Sway/wlroots backends.
- **Clients:** native macOS, Linux, Windows, and Android clients against live hosts, plus stock
  Moonlight clients over the GameStream path.
- **Cross-machine latency** is measured and skew-corrected (a wall-clock handshake removes
  the inter-machine clock offset), so capture-to-present numbers are valid across the LAN.

## Highlights

Notable capabilities that have landed, newest first:

- **Tiered stats overlay everywhere.** One shared stats vocabulary with **Compact / Normal /
  Detailed** levels on every client, cycled live in-stream — see
  [Understanding the Stats Overlay](/docs/stats).
- **Per-client display scaling on GNOME.** The host persists each client's scale itself and
  reapplies it on reconnect (GNOME's virtual-monitor API exposes no stable identity to key it on).
- **Adaptive bitrate.** With bitrate set to **Automatic**, the host re-targets the encoder
  mid-stream as the link's measured capacity changes.
- **Gamepad console shell.** The session client carries a full controller-driven shell — host
  list, PIN pairing, settings, screen transitions, and an on-screen keyboard — usable with no
  desktop at all (e.g. gamescope on a Steam Deck).
- **Native Linux client (stage 1).** GTK4/libadwaita app that links `punktfunk-core`
  directly: mDNS host list, TOFU + SPAKE2 PIN pairing, FFmpeg HEVC decode, PipeWire audio
  with mic uplink, SDL3 gamepad capture with rumble/lightbar feedback, layout-independent
  keyboard, absolute mouse, fullscreen, and a stats overlay. (Since re-architected: streams
  now run in a spawned Vulkan session presenter with Vulkan Video decode on all vendors.)
- **Delegated pairing approval.** An unpaired device that knocks on a pairing-required host
  appears as a pending request in the web console's Pairing page; one click approves and
  pins its certificate — no PIN fetched out of band.
- **Concurrent sessions.** The host serves multiple clients at once (bounded by an NVENC
  limit), each with its own virtual output and encoder — e.g. stream the same desktop to a
  laptop and a TV simultaneously.
- **Cross-machine latency HUD + wall-clock skew handshake.** A short NTP-style handshake
  aligns client and host clocks, making capture-to-received latency valid across
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
- **On-glass validation of the Windows client's HDR present path** (hardware decode and the
  GUI are already validated on NVIDIA and Intel).
- **gamescope multi-user isolation** — per-session input/audio so concurrent sessions can
  be fully independent desktops (the shared-desktop multi-view case already works).
