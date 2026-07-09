---
title: Understanding the Stats Overlay
description: What every number in the punktfunk stats HUD means, and how to compare them fairly with Moonlight/Sunshine.
---

Every punktfunk client has an in-stream stats overlay. All clients use **the same
vocabulary, the same measurement points, and the same math**, so a number on your
phone means exactly what the same number means on your desktop.

## The four measurement points

Every latency figure is the time between two of these four points in a video frame's
life:

1. **capture** — the host grabs the frame from the (virtual) display. Stamped on the
   host's clock and carried with the frame.
2. **received** — your client has fully received and reassembled the frame from the
   network (after any FEC recovery), before decoding.
3. **decoded** — the video decoder has produced the picture.
4. **displayed** — the picture is handed to the screen (as close to "photons" as the
   platform lets us measure).

## Reading the overlay

```
1920×1080@120 · 119 fps · 38.2 Mb/s · HEVC 10-bit HDR · GPU decode
end-to-end 14.2 ms p50 · 19.8 p95 · capture→on-glass
= host 3.1 + network 6.7 + decode 2.1 + display 2.3
lost 3 (0.1%) · skipped 1 · FEC 12
```

- **Line 1 — the stream.** Resolution@refresh, frames received per second, and the
  received video bitrate (goodput — FEC overhead not counted), plus codec details.
- **Line 2 — the headline.** `end-to-end` is the *directly measured* time from host
  capture to the endpoint named at the end of the line (`capture→on-glass` here).
  `p50` = the typical frame (median), `p95` = the slow outliers. This is the one
  number that summarizes your stream.
- **Line 3 — where the time goes.** The stages **tile the end-to-end interval** —
  each starts where the previous one ends, so they add up to the headline:
  - `host` — capture → sent: the host's own share (capture read, encode, error
    coding, the paced send), reported by the host itself once per frame.
  - `network` — sent → received: the network flight plus reassembly on your device.
  - `decode` — received → decoded, on your device.
  - `display` — decoded → displayed: waiting for the right screen refresh, rendering,
    and vsync.

  Against an **older host** that doesn't report its share yet, the first two terms
  merge into a single `host+network` number — same total, one split fewer.

  (Stage values are per-stage medians, so they sum only *approximately* to the
  headline median — percentiles aren't perfectly additive. The headline is measured
  directly, never computed as a sum.)
- **Line 4 — reliability** (only shown when something is nonzero). `lost` = frames the
  network dropped beyond FEC's ability to recover; `skipped` = frames your client
  chose not to display because a newer one had already arrived; `FEC` = packet shards
  the error correction recovered this second (loss that you *didn't* feel).

All values refresh once per second over the last second of frames.

### Clocks, and the `(same-host clock)` tag

`end-to-end` and `host+network` span two machines, so they need the two clocks to
agree: at connect, the client runs an NTP-style handshake with the host and corrects
for the measured clock offset. If that handshake wasn't possible, the overlay appends
**`(same-host clock)`** — the numbers are then only trustworthy when client and host
run on the same machine. `decode` and `display` are single-machine measurements and
are always exact.

### What each platform can measure

Not every platform exposes a true "displayed" instant, so the headline's endpoint is
always spelled out rather than pretending:

| client | headline | why |
|---|---|---|
| Windows, macOS/iOS (Metal presenter), Linux | `capture→on-glass` / `capture→displayed` | present instant available (GTK measures at hand-off to the compositor, which adds about one compositor cycle after it) |
| Android | `capture→displayed` | MediaCodec's per-frame render callback reports SurfaceFlinger's render timestamp; on the rare window where no callback is delivered (the platform may drop them under load) the HUD falls back to `capture→decoded` |
| macOS/iOS fallback presenter | `capture→received` | the system video layer hides decode and present timing entirely |

A shorter chain means the number is **smaller because it measures less** — check the
endpoint before comparing two devices.

## Comparing with Moonlight / Sunshine

Moonlight's overlay and punktfunk's measure different slices of the pipeline, and the
single biggest difference is:

> **Moonlight has no end-to-end number.** Its overlay shows separate client-side
> segments (decode time, queue delay, render time) and — on Sunshine hosts — a
> host-side number. Nothing in Moonlight measures capture-to-glass, and nothing
> measures the network flight of video frames. punktfunk's `end-to-end` line has **no
> Moonlight counterpart** — never compare it against any single Moonlight line.

To compare fairly, reconstruct an approximate end-to-end from Moonlight's lines:

```
Moonlight ≈ host processing latency (avg)
          + ½ × average network latency
          + average decoding time
          + average frame queue delay
          + average rendering time
```

…and compare *that* against punktfunk's `end-to-end`. (It's still approximate:
Moonlight's segments are averages over a slightly different window, and the ½·RTT term
stands in for a one-way frame flight that Moonlight doesn't measure.)

### Line-by-line matrix

| Moonlight overlay line | What it actually measures | punktfunk equivalent | Comparable? |
|---|---|---|---|
| `Video stream: WxH FPS` | Received **plus inferred-lost** frames/s (host-rate estimate from frame sequence gaps) | `fps` (line 1) | ≈ equal when loss is near zero; punktfunk counts received frames only |
| `Incoming frame rate from network` | Frames reassembled from the network per second | `fps` (line 1) | **Yes — direct** |
| `Decoding frame rate` (desktop only) | Frames leaving the decoder per second | not shown separately (equals `fps` unless the decoder is falling behind) | — |
| `Rendering frame rate` (desktop only) | Frames actually presented per second | `fps` minus `skipped` | Approximately |
| `Host processing latency min/max/avg` (Sunshine hosts) | Host capture → just-before-send, reported by Sunshine per frame | `host` (line 3) — the host reports capture→fully-sent per frame the same way | **Yes — direct** (punktfunk's includes the paced send itself, Sunshine's stops just before it; avg vs p50) |
| `Frames dropped by your network connection` | Frame-sequence gaps ÷ total frames | `lost` (line 4) | **Yes — direct** |
| `Frames dropped due to network jitter` | Decoded frames the *client's pacer* chose to drop ÷ decoded frames | `skipped` (line 4) | Approximately (both are client-side pacing decisions, despite Moonlight's name) |
| `Average network latency` | The **control connection's round-trip time** (ENet RTT + variance) — not video frame latency | `network` (line 3) is the closest concept, but it's the *actual one-way frame path* (flight + reassembly), not an RTT | **No direct comparison.** Roughly, punktfunk's `network` ≈ ½ × an idle RTT plus serialization time of the frame |
| `Average decoding time` | Mean time from decoder enqueue to picture out | `decode` (p50) | Yes (mean vs median; both include decoder queueing) |
| `Average frame queue delay` | Mean time a decoded frame waits for its vsync slot | inside `display` | Sum the two Moonlight lines → |
| `Average rendering time (incl. V-sync latency)` | Mean duration of the present call | inside `display` | …and compare against punktfunk's `display` |
| *(no equivalent)* | — | `end-to-end` — true capture→glass, clock-skew-corrected across machines | **punktfunk only** |
| *(no equivalent)* | — | `FEC` recovered shards (loss absorbed invisibly) | punktfunk only |

Other differences worth knowing when squinting at both overlays side by side:

- **Averages vs percentiles.** Moonlight's time values are means; punktfunk shows
  medians (p50) with a p95 for the headline. Under jitter, a mean sits above the
  median — Moonlight's numbers read slightly "worse" than an equivalent p50.
- **Windows.** Both refresh about once per second; Moonlight over a ~1–2 s sliding
  window, punktfunk over the last full second.
- **Host frame rate.** Moonlight's headline FPS estimates what the *host* produced
  (received + lost). punktfunk shows what your client actually received, and reports
  loss separately.
