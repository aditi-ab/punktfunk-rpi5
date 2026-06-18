---
title: "Windows Host"
description: "Feasibility and scoping for a Windows host backend."
---


**Status: scoped, deferred — but de-risked.** A Windows host is architecturally an *"add a backend"*
job, not a parallel port. The one thing that used to make it **large** — the per-client *virtual*
output, which has no user-mode Windows API and seemingly needed a self-signed kernel Indirect
Display Driver (IDD) — is **solved by reusing [SudoVDA](https://github.com/VirtualDrivers), the
Sunshine Virtual Display Adapter**: a pre-built, signed IDD that creates virtual displays at
arbitrary `WxH@Hz` on demand. We install it and drive its control interface; **no driver to write or
WHQL-sign.** That turns the headline feature from XL into a medium backend. This doc records what's
left so the work can be picked up deliberately.

(Grounded in a 4-agent read of the host crate, 2026-06-10; SudoVDA path added 2026-06-11.)

## What's already done for us

punktfunk is cleanly layered. **~95% of the codebase is platform-agnostic and reuses verbatim:**

| Reusable as-is | Why |
|---|---|
| `punktfunk-core` (protocol, FEC, crypto, session, transport, **C ABI**) | Zero platform deps — no `cfg(linux)` anywhere; the C ABI is already cross-platform |
| QUIC control plane (`quic.rs`, pairing, mode negotiation) | quinn + tokio are portable |
| GameStream P1.1 (mDNS, serverinfo, pairing, RTSP, ENet) — *except* `stream.rs`/`audio.rs` | pure wire logic |
| Management REST API (`mgmt.rs`) + OpenAPI | axum/tokio, portable |
| Pipeline + `punktfunk1.rs` orchestration | trait-generic — calls `capturer.next_frame()`, `encoder.submit/poll()`; **needs zero changes** |
| The **trait boundaries** themselves: `Capturer`, `Encoder`, `VirtualDisplay`, `InputInjector`, `AudioCapturer`, `VirtualMic` | platform-neutral signatures; Linux deps are already isolated under `[target.'cfg(target_os="linux")'.dependencies]` |

So a Windows host is **new `#[cfg(target_os = "windows")]` backend modules behind the existing
traits** — the per-frame path, protocol, and control plane don't move. No architectural refactor is
required; the boundaries are already in the right places.

## What a Windows host needs (new code)

Each row is a Linux backend that needs a Windows sibling. Effort is the *implementation* effort;
all reuse the existing trait.

| Subsystem | Linux today | Windows equivalent | Effort | Notes |
|---|---|---|---|---|
| **Capture** | xdg ScreenCast portal → PipeWire (dmabuf) | **DXGI Desktop Duplication** (or Windows.Graphics.Capture) → D3D11 texture | M | DXGI gives a GPU `B8G8R8A8` texture directly |
| **Virtual display** | KWin/Mutter/Sway/gamescope protocols | **SudoVDA** (pre-built signed IDD) — install + drive its control API to add/remove a `WxH@Hz` virtual monitor per session | **M** | ✅ **no longer the blocker**: SudoVDA is the same IDD Sunshine ships, so no driver to author or sign. The `VirtualDisplay` backend = enable the adapter, create a monitor at the client's mode, capture it (DXGI), tear it down on session end. Fallback if SudoVDA is absent: capture an existing monitor (loses native-resolution) |
| **Encode** | `ffmpeg-next` NVENC, CUDA hwframes | Media Foundation H.264/HEVC/AV1, **or** NVENC SDK direct with a D3D11 device context (`AVD3D11VADeviceContext`) | M–L | `encode.rs` AU/codec logic + NVENC option strings are portable; only the hwdevice + frame-pool glue swaps |
| **Zero-copy bridge** | dmabuf → EGL/Vulkan → CUDA | D3D11 texture → NVENC (shared texture / `cudaImportExternalMemory` + D3D12 fence) | M | **optional** — a portable CPU-copy path already exists, so v1 can skip this |
| **Input (ptr/kbd)** | libei (RemoteDesktop portal) / wlr protocols | **SendInput** (`keybd_event`/`mouse_event`) | S | the VK→evdev table just becomes VK→`VIRTUAL_KEY` (already Win32-native) |
| **Input (gamepads)** | uinput X-Box-360 pad + FF rumble | **ViGEm** (Virtual Gamepad Emulation) + HID reports | M | rumble back-channel maps to ViGEm notifications |
| **Audio capture** | PipeWire sink-monitor | **WASAPI loopback** (`IAudioCaptureClient`) | S–M | also produces interleaved f32 — same `AudioCapturer` contract |
| **Virtual mic** | PipeWire `Audio/Source` | virtual audio device (VB-Cable-style WDM driver) or WASAPI render-to-fake-device | M | needs a driver or a bundled 3rd-party cable |
| **`sendmmsg` batching** | `gamestream/stream.rs` | already has a `cfg(not(linux))` per-packet fallback | — | nothing to do |

**Rough total: ~2,000–4,000 LOC of new Rust** (no C++ driver — SudoVDA is reused as-is), spread over
capture/encode/vdisplay/input/audio. With the driver problem solved, the overall effort is now
**medium**; the input+audio layer alone is *small–medium*.

## Recommended phasing (when picked up)

1. **Phase 0 — "basic Windows host" (no virtual display).** Capture an *existing* monitor (DXGI
   Desktop Duplication) → Media Foundation/NVENC encode → SendInput + WASAPI loopback. This proves
   the whole stack on Windows with the smallest surface, reusing all of core/QUIC/GameStream/mgmt.
   It loses the per-client native-resolution output but is a working Windows host quickly.
2. **Phase 1 — the virtual display via SudoVDA.** A `VirtualDisplay` backend that enables SudoVDA,
   creates a monitor at the client's exact `WxH@Hz`, captures it (DXGI), and tears it down on session
   end — restoring punktfunk's headline feature with **no driver authoring or signing**. (Ship/guide
   the SudoVDA install as a host prerequisite, like the udev rule on Linux.)
3. **Phase 2 — input + audio parity.** ViGEm gamepads + rumble; WASAPI virtual mic; D3D11→NVENC
   zero-copy.

## Why it's deferred (not started now)

- The remaining work is **medium** and mechanical, but **none of it is buildable or testable on the
  Linux dev box** — it would be unvalidated code until there's a Windows box in the loop.
- SudoVDA removed the hard blocker (the signed kernel driver); what's left is a backend port, picked
  up whenever a Windows target is in scope.

The architecture is ready whenever the work is scheduled; this doc + the clean trait boundaries are
the down payment. Start at **Phase 0** for the fastest path to a working Windows host.
