---
title: Understanding the Stats Overlay
description: What every number in the Punktfunk stats HUD means, and how to compare them fairly with Moonlight/Sunshine.
---

Every Punktfunk client has an in-stream stats overlay. All clients use **the same
vocabulary and the same four measurement points**, so a stage name on your phone means
what the same name means on your desktop.

Two platforms differ in the *math*: on **iOS and tvOS** the headline is **floor-shaved**.
The fixed depth of Apple's present pipeline — roughly two refresh intervals, which no
client can pace under — is excluded from it, and the Detailed tier prints the excluded
term on its own line as `os present +X.X excluded (display pipeline minimum)`. Add that
floor back before holding an iPhone, iPad or Apple TV's `capture→on-glass` next to a
macOS, Linux, Windows or Android one. (The macOS client shaves nothing: it presents
straight to the display, with no such pipeline depth to measure, so its numbers are raw.)

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

## Detail levels

The overlay has four levels — **Off → Compact → Normal → Detailed** — that you cycle live
in-stream:

| Platform | Cycle with |
|---|---|
| Linux · Windows · Steam Deck | **Ctrl+Alt+Shift+S** |
| macOS / iPad (pointer or trackpad) | **⌃⌥⇧S** or a **three-finger tap** |
| Android · iPhone | a **three-finger tap** |

**Ctrl+Alt+Shift+S** is one of a small set of shortcuts a stream reserves; the others — release
captured input, switch mouse mode, disconnect, mute the microphone — are in
[Getting your input back](/docs/input#getting-your-input-back).

**Compact** is a one-line pill (fps · end-to-end ms · Mb/s, plus a loss flag when frames are being
lost). **Normal** adds the stream line and the p50/p95 headline. **Detailed** adds the per-stage
breakdown everywhere; on Linux/Windows it also adds the encoder's target bitrate, the decode path,
an HDR tag and a chroma tag, on Android the decoder plus the full codec/bit-depth/colour line, and
on iOS/tvOS the excluded OS present floor.
You can also set the level a stream starts at in each client's
[Settings](/docs/client-settings#overlay). The examples below are the **Detailed** view.

The overlay follows your display's scaling, so it should already be readable. To nudge it, set
`PUNKTFUNK_OSD_SCALE` in the **client's** environment (0.5×–4×) — see [Configuration →
Client-side](/docs/configuration#client-side-native-clients).

## Reading the overlay

Every client reports the same measurements, but each family lays them out a little
differently. Linux · Windows · Steam Deck:

```
1920×1080@120 · 120 fps · 24.3 Mb/s · target 30 Mb/s (auto) · vulkan · HDR
e2e 14.2/19.8 ms (p50/p95) · host 3.1 · net 6.7 · decode 2.1 · display 2.3 ms (pace 0.6 + latch 1.7)
host: queue 0.6 · encode 1.8 · xfer 0.2 · pace 0.5 ms
present: mailbox
lost 3 (2.4%)
```

Android:

```
1920×1080@120   120 fps   24.3 Mb/s
c2.qti.hevc.decoder · low-latency
HEVC · 10-bit · HDR (BT.2020 PQ) · 4:2:0
end-to-end 14.2 ms p50 · 19.8 p95 · capture→displayed
= host 3.1 + network 6.7 + decode 2.1 + display 2.3
lost 3 (2.4%) · skipped 1 · FEC 12
```

iOS · tvOS (headline and `display` both floor-shaved, so they still add up — the raw
end-to-end here is 30.7 ms, the 16.7 ms floor of a 120 Hz screen included). macOS lays the
same lines out, but with raw numbers and no `os present` line:

```
1920×1080@120  120 fps  24.3 Mb/s
end-to-end 14.0 ms p50 · 19.6 p95 · capture→on-glass
= host 3.1 + network 6.7 + decode 2.1 + display 2.1
os present +16.7 excluded (display pipeline minimum)
lost 3 (2.4%)
```

- **Line 1 — the stream.** Resolution@refresh, frames received per second, and the
  received video bitrate (goodput — FEC overhead not counted). Linux/Windows follow the
  measured rate with `target N Mb/s` — what the host's encoder is currently *allowed* to
  produce — so a quiet desktop under a large grant (measured far below target) reads
  differently from an encoder pinned at its cap (measured hugging the target). `(auto)`
  means the [Automatic bitrate](/docs/client-settings#bitrate) controller owns the target
  and moves it with network conditions; no target at all means an older host that doesn't
  report one. Then the decode path, an [HDR](/docs/hdr) tag (`HDR`, or `HDR→SDR` when a PQ
  stream is tone-mapped onto an SDR screen), and — when you asked for
  [full chroma](/docs/client-settings) — the resolved chroma: `4:4:4` when the host
  granted it, `4:4:4→4:2:0` when it couldn't. Android puts its decoder and the negotiated
  codec, bit depth, colour and chroma on rows of their own underneath; the Apple clients
  don't report a codec at all.
  If the session resolved to a [settings profile](/docs/profiles-and-links), its name closes this
  line. On **Android** a `⚠ panel NN Hz` warning joins it whenever the device's panel is refreshing
  *below* the stream's rate — the tell for a phone or TV governor that ignored the requested mode,
  which otherwise reads as inexplicable judder plus a refresh of extra latency.
- **Line 2 — the headline.** `end-to-end` (`e2e` on Linux/Windows) is the *directly
  measured* time from host capture to the endpoint named at the end of the line —
  `capture→on-glass` or `capture→displayed`. On Linux/Windows the endpoint is the moment
  the frame is genuinely **visible** wherever the GPU driver can report it (most can);
  where it can't, the measurement stops at the instant the frame is handed to the display
  and so reads slightly optimistic. `p50` = the typical frame (median), `p95` = the slow
  outliers. This is the one number that summarizes your stream.
- **Line 3 — where the time goes.** The first four stages **tile the end-to-end interval** —
  each starts where the previous one ends, so they add up to the headline. The two extra
  terms under them are not extra time: one is excluded from the total, the other sits inside a
  stage that's already counted.
  - `host` — capture → sent: the host's own share (capture read, encode, error
    coding, the paced send), reported by the host itself once per frame.
  - `network` (`net` on Linux/Windows) — sent → received: the network flight plus
    reassembly on your device.
  - `decode` — received → decoded, on your device.
  - `display` — decoded → displayed: waiting for the right screen refresh, rendering,
    and vsync. On Linux/Windows it splits into `(pace + latch)` when your driver reports
    true on-glass timing: **pace** is Punktfunk's own work — getting the decoded frame
    submitted — and **latch** is the wait for the display to take it. A large `latch` is
    the screen's refresh cycle, not the stream; a large `pace` is us. (`pace` is also the
    fair number to compare against an iPhone or iPad, whose figure already has its
    equivalent of `latch` removed.)
  - `os present` *(iOS and tvOS)* — the fixed depth of the OS present pipeline, which is
    excluded from both the headline and `display` and printed here so you can add it
    back.
  - `client queue` *(Apple only)* — how long a received frame waited before the decoder
    pulled it. It's the front part of `decode`, not time on top of it. Hidden below 2 ms;
    a value that persists is a standing receive backlog on the client.
  - `display X (pace A + latch B)` and `presents N` *(Android only)* — when the timeline presenter
    is running it splits `display` in two: `pace` is the wait it deliberately holds the frame for
    its target refresh, `latch` is SurfaceFlinger picking it up and scanning it out. `presents`
    counts the frames confirmed on glass this second — well below `fps` means the presenter is
    dropping or serializing frames; an `fps` shortfall with `presents` keeping up is upstream of
    the client.

  Against an **older host** that doesn't report its share yet, the first two terms
  merge into a single `host+network` number (`host+net` on Linux/Windows) — same total,
  one split fewer. On Linux/Windows, Detailed adds one further line — `host: queue … ·
  encode … · xfer … · pace …` — splitting the host's own share into its stages, when the
  host reports them.

  Linux/Windows Detailed also carries a **`present:`** line naming how frames are reaching
  your screen: the display mode in use (`mailbox`, `fifo`, `fifo-latest-ready`, …), `vrr yes`/`vrr no` once the
  client has *measured* whether your screen is following the stream's cadence (it is
  reported only when measured — no guess from what the display claims), and, when the
  [presentation setting](/docs/client-settings#video) is *Smoothness*, the word
  `smoothing`. Counters join it only when they're doing something — `qdrop`/`qdry` mean
  the smoothing buffer overflowed or ran dry (a jittery link), and `gated`/`forced`
  belong to the pacing that keeps frames from stacking up behind the display.

  (Stage values are per-stage medians, so they sum only *approximately* to the
  headline median — percentiles aren't perfectly additive. The headline is measured
  directly, never computed as a sum.)
- **Line 4 — reliability** (only shown when something is nonzero). `lost` = frames the
  network dropped beyond FEC's ability to recover — every client reports it. `skipped`
  (frames your client chose not to display because a newer one had already arrived) and
  `FEC` (packet shards the error correction recovered this second — loss you *didn't*
  feel) are reported by the **Android client only**; the other clients show `lost` alone.

All values refresh once per second over the last second of frames.

### Clocks, and the `(same-host clock)` tag

`end-to-end` and `host+network` span two machines, so they need the two clocks to
agree: at connect, the client runs an NTP-style handshake with the host and corrects
for the measured clock offset. If that handshake wasn't possible, the overlay appends
**`(same-host clock)`** — the numbers are then only trustworthy when client and host
run on the same machine. `decode` and `display` are single-machine measurements and
are always exact.

### What each platform can measure

Not every platform exposes a true "displayed" instant, so the point the headline stops at
differs by client — and the clients that have a choice name it on the line rather than
pretending:

| client | headline | why |
|---|---|---|
| Windows, Linux | `capture→on-glass` | present instant available (measured right after the Vulkan swapchain present); published raw |
| macOS (Metal presenter) | `capture→on-glass` | present instant available (the system's on-glass time for the flip); published raw |
| iOS/tvOS (Metal presenter) | `capture→on-glass` | present instant available, but the OS present floor is **excluded** from the number and printed separately as `os present +X.X excluded` |
| Android | `capture→displayed` | MediaCodec's per-frame render callback reports SurfaceFlinger's render timestamp; on the rare window where no callback is delivered (the platform may drop them under load) the HUD falls back to `capture→decoded` |
| macOS/iOS fallback presenter | `capture→received` | the system video layer hides decode and present timing entirely |

A shorter chain means the number is **smaller because it measures less** — check the
endpoint before comparing two devices, and add the excluded `os present` floor back to an
iOS or tvOS client's headline before holding it next to another platform's.

## Comparing with Moonlight / Sunshine

Moonlight's overlay and Punktfunk's measure different slices of the pipeline, and the
single biggest difference is:

> **Moonlight has no end-to-end number.** Its overlay shows separate client-side
> segments (decode time, queue delay, render time) and — on Sunshine hosts — a
> host-side number. Nothing in Moonlight measures capture-to-glass, and nothing
> measures the network flight of video frames. Punktfunk's `end-to-end` line has **no
> Moonlight counterpart** — never compare it against any single Moonlight line.

To compare fairly, reconstruct an approximate end-to-end from Moonlight's lines:

```
Moonlight ≈ host processing latency (avg)
          + ½ × average network latency
          + average decoding time
          + average frame queue delay
          + average rendering time
```

…and compare *that* against Punktfunk's `end-to-end`. (It's still approximate:
Moonlight's segments are averages over a slightly different window, and the ½·RTT term
stands in for a one-way frame flight that Moonlight doesn't measure.)

### Line-by-line matrix

| Moonlight overlay line | What it actually measures | Punktfunk equivalent | Comparable? |
|---|---|---|---|
| `Video stream: WxH FPS` | Received **plus inferred-lost** frames/s (host-rate estimate from frame sequence gaps) | `fps` (line 1) | ≈ equal when loss is near zero; Punktfunk counts received frames only |
| `Incoming frame rate from network` | Frames reassembled from the network per second | `fps` (line 1) | **Yes — direct** |
| `Decoding frame rate` (desktop only) | Frames leaving the decoder per second | not shown separately (equals `fps` unless the decoder is falling behind) | — |
| `Rendering frame rate` (desktop only) | Frames actually presented per second | `fps` minus `skipped` (Android only) | Approximately |
| `Host processing latency min/max/avg` (Sunshine hosts) | Host capture → just-before-send, reported by Sunshine per frame | `host` (line 3) — the host reports capture→fully-sent per frame the same way | **Yes — direct** (Punktfunk's includes the paced send itself, Sunshine's stops just before it; avg vs p50) |
| `Frames dropped by your network connection` | Frame-sequence gaps ÷ total frames | `lost` (line 4) | **Yes — direct** |
| `Frames dropped due to network jitter` | Decoded frames the *client's pacer* chose to drop ÷ decoded frames | `skipped` (line 4, Android only) | Approximately (both are client-side pacing decisions, despite Moonlight's name) |
| `Average network latency` | The **control connection's round-trip time** (ENet RTT + variance) — not video frame latency | `network` (line 3) is the closest concept, but it's the *actual one-way frame path* (flight + reassembly), not an RTT | **No direct comparison.** Roughly, Punktfunk's `network` ≈ ½ × an idle RTT plus serialization time of the frame |
| `Average decoding time` | Mean time from decoder enqueue to picture out | `decode` (p50) | Yes (mean vs median; both include decoder queueing) |
| `Average frame queue delay` | Mean time a decoded frame waits for its vsync slot | inside `display` | Sum the two Moonlight lines → |
| `Average rendering time (incl. V-sync latency)` | Mean duration of the present call | inside `display` | …and compare against Punktfunk's `display` |
| *(no equivalent)* | — | `end-to-end` — true capture→glass, clock-skew-corrected across machines | **Punktfunk only** |
| *(no equivalent)* | — | `FEC` recovered shards (loss absorbed invisibly; Android only) | Punktfunk only |

Other differences worth knowing when squinting at both overlays side by side:

- **Averages vs percentiles.** Moonlight's time values are means; Punktfunk shows
  medians (p50) with a p95 for the headline. Under jitter, a mean sits above the
  median — Moonlight's numbers read slightly "worse" than an equivalent p50.
- **Windows.** Both refresh about once per second; Moonlight over a ~1–2 s sliding
  window, Punktfunk over the last full second.
- **Host frame rate.** Moonlight's headline FPS estimates what the *host* produced
  (received + lost). Punktfunk shows what your client actually received, and reports
  loss separately.

## Recording a capture for a bug report

The overlay only ever shows the last second. To capture a whole run, use the host's own recorder —
the **Performance** page in the [web console](/docs/web-console):

1. Press **Start capture**. Sampling happens at the host's existing aggregation boundary (about
   every 1–2 s), so arming it costs the stream nothing.
2. Reproduce the problem. The live graphs fill in as it runs.
3. Press **Stop & save**. The recording appears in the list below, and survives a host restart.

A recording carries per-stage p50/p99 pipeline latency, new frames/s versus re-encoded holds/s
(source starvation), the attempted wire bitrate against the target, and frame/packet/send drops plus
FEC recoveries. Its header names the **encoder backend and the GPU** that produced it — without
those, a stage split can't be read at all.

**Download** saves it as a `.json` file you can attach to a report; **Delete** removes it. On disk
they live on the host in `~/.config/punktfunk/captures/` (`%ProgramData%\punktfunk\captures\` on
Windows) until you delete them.
