---
title: "Apple Stage-2 Presenter (handoff)"
description: "Implementation plan for the explicit VTDecompressionSession → CAMetalLayer presenter — hand-paced present + true decode→present (glass-to-glass) measurement. Written so a Mac agent can pick it up."
---

A pickup-ready plan for the **stage-2 Apple presenter**. The current **stage-1** presenter feeds
compressed HEVC straight into `AVSampleBufferDisplayLayer`, which hardware-decodes **and presents
internally with no per-frame callback** — so we can't stamp decode or present, and we can't hand-pace.
Stage-2 takes explicit control: decode with `VTDecompressionSession`, present decoded frames through a
`CAMetalLayer` driven by a display link. Two wins: **~0.5 refresh off the present tail** (the biggest
client latency term at 60 Hz) and **true decode→present / glass-to-glass** numbers.

All of this is **macOS/iOS/tvOS-only** — build + validate on a Mac (`swift build && swift test`, then
live against a Linux host). The host + connector side is already done: `PunktfunkConnection.clockOffsetNs`
(the connect-time skew offset, host minus client) is what makes the present timestamp cross-machine
valid. See [Status](/docs/status) and roadmap §12.

## Where it plugs into the existing code

| Existing (stage-1) | Stage-2 change |
|---|---|
| `StreamPump` pulls AUs → `AnnexB.sampleBuffer` → `layer.enqueue` (compressed) | A `Stage2Pump` (or a mode flag on `StreamPump`) feeds AUs to `VTDecompressionSessionDecodeFrame` instead |
| `StreamView`/`StreamViewIOS` host an `AVSampleBufferDisplayLayer` | Host a `CAMetalLayer` (+ a display link); keep the input-capture + HUD overlay unchanged |
| `AnnexB.formatDescription(fromIDR:)` builds the format desc, refreshed on every IDR | **Reused** — it's the `VTDecompressionSession`'s format description; recreate the session when it changes |
| `LatencyMeter` records capture→client-receipt at `onFrame` | Extend to record **decode-completion** and **present** stages (below) |

Keep stage-1 behind a `UserDefaults` flag (e.g. `punktfunk.presenter = "stage1" | "stage2"`) so a
regression can fall back — `AVSampleBufferDisplayLayer` is the known-good path.

## Decode: VTDecompressionSession

1. Create the session from the IDR's `CMVideoFormatDescription`
   (`AnnexB.formatDescription(fromIDR:)`):
   ```
   VTDecompressionSessionCreate(
     allocator: nil,
     formatDescription: fmt,
     decoderSpecification: nil,           // hardware by default; no need to force
     imageBufferAttributes: [
       kCVPixelBufferMetalCompatibilityKey: true,
       kCVPixelBufferPixelFormatTypeKey:
         kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, // 8-bit SDR; 10-bit (…10BiPlanar) for HDR later
     ],
     outputCallback: <C-callback>,
     decompressionSessionOut: &session)
   ```
2. Per AU: build the same `CMSampleBuffer` as stage-1 (`AnnexB.sampleBuffer(au:format:)`, PTS =
   `au.ptsNs` @ 1e9 timescale) and submit:
   ```
   VTDecompressionSessionDecodeFrame(session, sampleBuffer,
     flags: ._EnableAsynchronousDecompression,
     frameRefcon: <pts or a boxed context>, infoFlagsOut: nil)
   ```
3. The **output callback** delivers `(status, infoFlags, imageBuffer: CVImageBuffer?, presentationTimeStamp, …)`.
   `presentationTimeStamp` is `au.ptsNs` (the host capture clock). **Stamp decode-completion here**
   (`CLOCK_REALTIME` ns), retain the `CVPixelBuffer`, and push `{pts, pixelBuffer, decodedNs}` into a
   small NSLock-guarded ring (the "ready" queue) the display link drains.
4. **IDR / mode change**: when `AnnexB.formatDescription` yields a new desc, check
   `VTDecompressionSessionCanAcceptFormatDescription`; if not, finish-and-recreate the session (same
   trigger stage-1 uses to refresh `format`). On decoder error (`kVTVideoDecoderBadDataErr`, etc.) drop
   to the next IDR — there's no out-of-band extradata; recovery keyframes re-carry the parameter sets.

## Present: CAMetalLayer + display link

- `CAMetalLayer` (device = system default, `pixelFormat = .bgra8Unorm`, `framebufferOnly = true`,
  `drawableSize` = stream WxH). The view: macOS `NSView`/iOS `UIView` whose `layerClass`/backing layer
  is the `CAMetalLayer` (mirror `StreamView`/`StreamViewIOS`).
- **Display link** drives present: macOS `CVDisplayLink` (or `CADisplayLink` on macOS 14+),
  iOS/tvOS `CADisplayLink`. Each callback carries the **target present timestamp** (`CVTimeStamp` /
  `targetTimestamp`).
- Each vsync: pop the **newest** ready frame (drop older undisplayed ones — low-latency default; no
  smoothing buffer to start), render a fullscreen quad sampling the **biplanar YUV** (luma +
  chroma planes via `CVMetalTextureCache`) with a BT.709 YUV→RGB fragment shader, then
  `commandBuffer.present(drawable)` (or `present(drawable, atTime:)`). **Stamp present time** for the
  frame just shown (use the display link's target timestamp converted to `CLOCK_REALTIME`).
- Colorspace: BT.709 8-bit for now (matches the host's SDR). HDR (BT.2020/PQ, 10-bit `…10BiPlanar` +
  EDR `CAMetalLayer.wantsExtendedDynamicRangeContent`) is a later tie-in with the HDR roadmap (§10).

### Cheaper intermediate (2a) if the Metal path is too big in one step
Decode with `VTDecompressionSession` (gets the **decode-completion timestamp** = capture→decoded),
then wrap the decoded `CVPixelBuffer` in a `CMSampleBuffer` and `enqueue` it into the existing
`AVSampleBufferDisplayLayer` (it accepts uncompressed pixel buffers too). This yields the decode term
**without** a Metal renderer — but **not** true present (the layer still presents internally). Ship 2a
first if useful; 2b (CAMetalLayer + display link) is required for the on-glass present stamp.

## Measurement (the whole point)

Extend `LatencyMeter` (or add per-stage meters) so each frame records three instants, all
`CLOCK_REALTIME` ns, all shifted by `connection.clockOffsetNs` to the host clock:

- **capture→decoded** = `decodedNs + offset − pts_ns` (VideoToolbox decode latency, cross-machine)
- **decode→present** = `presentedNs − decodedNs` (the present tail stage-2 shortens)
- **capture→present** = `presentedNs + offset − pts_ns` — **the glass-to-glass number** (modulo the
  host render→capture term, still unmeasured; see roadmap §12)

Surface `capture→present` p50/p95 in the HUD (extend the existing `model.latency*` line in
`ContentView`). `skewCorrected` stays false when `clockOffsetNs == 0` (old host) — then the numbers are
same-host-only, as today.

## Validation

- `swift test`: add a decode-output test (decode a known IDR built like
  `VideoToolboxRoundTripTests` → assert a `CVPixelBuffer` of the right dimensions + the
  decode callback fires). Present is display-bound — validate it **live** via the HUD number.
- Live: connect to a Linux host (`m3-host --source virtual` on the GNOME box; see
  [Ubuntu — GNOME](/docs/ubuntu-gnome)), confirm `capture→present` is a few ms over `capture→client`
  and that `decode→present` shrank vs. an `AVSampleBufferDisplayLayer` baseline.
- Compare against the headless reference number: `punktfunk-client-rs` reports skew-corrected
  capture→reassembled (~1.3 ms p50 GNOME box → dev box); capture→present should be that **+ decode +
  present**.

## Gotchas

- VT decode is **async**; the output callback runs on a VT-managed thread — don't block it, just stamp
  + enqueue. Retain the `CVPixelBuffer` until presented (the ring owns it).
- `VTDecompressionSessionDecodeFrame` wants the **same** `CMSampleBuffer` shape stage-1 builds (AVCC
  length-prefixed NALs, in-band parameter sets in the format desc, never as extradata).
- `CAMetalLayer.drawableSize` must track mode changes (the host can `Reconfigure` mid-stream — watch
  `PunktfunkConnection.mode`/the new-IDR dimensions).
- Don't add a jitter/smoothing buffer for the first cut — present newest-ready for lowest latency; a
  pacing policy can come later if frames look uneven.
- Keep `clients/apple/README.md`'s "Stage 2" item + [Status](/docs/status) updated when this lands.
