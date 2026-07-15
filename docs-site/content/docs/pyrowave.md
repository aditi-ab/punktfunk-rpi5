---
title: PyroWave (wired-LAN codec)
description: The opt-in ultra-low-latency wavelet codec for wired links — what it is, the bandwidth it needs, and how to turn it on.
---

PyroWave is an **opt-in** video codec mode for links that can afford real bandwidth: wired
Ethernet, a docked Steam Deck, a 2.5GbE LAN. It trades bitrate for latency — instead of
H.264/HEVC/AV1 on the GPU's video engine, frames are compressed with
[PyroWave](https://github.com/Themaister/pyrowave), an intra-only wavelet codec running as plain
Vulkan compute. Punktfunk vendors a pinned copy and runs it on both ends.

**It is never selected automatically.** HEVC/AV1 remain the codecs for Wi-Fi and everything
else; PyroWave engages only when *you* pick it on the client **and** the host supports it. If
either side can't, the session silently falls back to the normal codec ladder.

## Why you'd want it

- **Codec latency drops by an order of magnitude.** Encode and decode each take a fraction of a
  millisecond of GPU compute (measured ~0.15 ms encode / ~0.07 ms decode at 1080p on an
  RTX 5070 Ti), versus one-to-several milliseconds per side for the hardware H.26x pipelines.
- **Every frame is a keyframe.** There is no GOP, no reference chain, no keyframe round-trip
  after packet loss — a lost frame costs exactly that frame, and the next one is already a
  complete picture. The whole IDR/recovery apparatus that produces loss-time stutter simply
  doesn't exist in this mode.
- **Uniform frame sizes.** The rate control hits its per-frame byte budget exactly, so the
  pacer sees a flat load instead of 20–40× keyframe spikes.

## What it costs

Bandwidth. At the codec's ~1.6 bits-per-pixel operating point (4:2:0, 60 fps):

| Mode | Bitrate |
|---|---|
| 1280×800 @ 60 (Deck) | ≈ 100 Mbps |
| 1920×1080 @ 60 | ≈ 200 Mbps |
| 2560×1440 @ 60 | ≈ 355 Mbps |
| 3840×2160 @ 60 | ≈ 800 Mbps |

120 Hz doubles these. Gigabit Ethernet tops out around 940 Mbps of payload, so 4K60 wants
2.5GbE (or a lower rate). **Do not run this over Wi-Fi** — that's what HEVC/AV1 are for.

Also: PyroWave is **8-bit SDR only**. An HDR session stays on HEVC/AV1 regardless of this
setting.

## Turning it on

1. **Host** (Linux): nothing to do — default builds ship the codec and every Linux GPU host
   advertises it. AMD/Intel hosts encode from the capture dmabuf zero-copy; on an NVIDIA host
   a PyroWave session currently captures via CPU RGB (a modest host-side cost that only
   affects sessions that picked this codec — everything else keeps its zero-copy path).
2. **Client**:
   - Linux session client: set **Settings → Video codec → PyroWave (wired LAN)** in the
     gamepad console, or launch with `PUNKTFUNK_PREFER_PYROWAVE=1`.
   - Apple (Mac, Apple TV 4K, iPad — wired networking strongly recommended): set
     **Settings → Codec → PyroWave (wired LAN)**. The option appears only on devices whose
     GPU passes the decode probe (Apple Silicon and A13-class or newer); picking it forces
     the session SDR.
3. Leave the bitrate on Automatic: a PyroWave session pins itself to the ~1.6 bpp rate for
   your mode (≈200 Mbps at 1080p60). An explicit bitrate is honored if you set one, but the
   adaptive-bitrate controller stays off either way — this codec has no useful low-rate
   regime, so under sustained loss the right move is switching back to HEVC, not degrading.
   The pin follows the resolution: a mid-stream resize (e.g. Match window) re-pins the rate
   for the new mode, so resizing a window down also cuts the bandwidth.

The stats overlay shows `pyrowave` as the decode path when the mode is active.

## Current limits

- Linux host + Linux client (including docked Deck) and Apple clients (native Metal decode
  on Mac / Apple TV 4K / iPad) today; the Windows host is tracked on the
  [roadmap](/docs/roadmap).
- 8-bit SDR, 4:2:0 only.
