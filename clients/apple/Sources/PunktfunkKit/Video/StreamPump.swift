// The platform-independent heart of the presenters: one thread pulling AUs from the
// connection into an AVSampleBufferDisplayLayer, with the format description refreshed
// on every IDR (the host opens with an IDR carrying in-band parameter sets; recovery
// keyframes re-send them — there is no out-of-band extradata, ever). Shared by the
// macOS StreamLayerView and the iOS/iPadOS stream view.

import AVFoundation
import Foundation
import os

private let pumpLog = Logger(subsystem: "io.unom.punktfunk", category: "video")

/// One pump per instance; create a fresh StreamPump per start (the stop is permanent —
/// a restart hands the old pump its own token, so it can never be revived by a newer start()).
final class StreamPump {
    private let token = StopFlag()

    /// Pump thread: pull AUs, wrap, enqueue. Non-IDR AUs before the first format
    /// description are dropped. `onFrame`/`onSessionEnd` fire on the pump thread.
    func start(
        connection: PunktfunkConnection,
        layer: AVSampleBufferDisplayLayer,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil
    ) {
        let token = token
        // Coalesced host keyframe requests (100 ms throttle — see KeyframeRecovery).
        let recovery = KeyframeRecovery()
        recovery.bind(connection)
        // Post-loss freeze-until-reanchor (shared core policy via the C ABI). Stage-1 has no per-frame
        // decode callback, so the gate is folded at ENQUEUE (from the AU's wire flags): a withheld
        // frame is still enqueued but flagged DoNotDisplay so the layer's decoder keeps the reference
        // chain fed while the last GOOD picture stays on glass — until a clean re-anchor lifts it.
        let gate = ReanchorGate(framesDropped: connection.framesDropped())
        // The layer is non-Sendable but its enqueue/flush are documented thread-safe, and after
        // this point only the pump thread drives it — assert that so the @Sendable Thread closure
        // may capture it.
        nonisolated(unsafe) let layer = layer
        layer.flush() // drop any frames a previous connection left queued

        let thread = Thread {
            var format: CMVideoFormatDescription?
            // Report the coded dims to the resize overlay only when they CHANGE (a new-mode IDR),
            // not on every loss-recovery IDR at the same size — so it fires once per real switch.
            var lastDecodedDims: CMVideoDimensions?
            var lastFramesDropped = connection.framesDropped()
            // Recovery is a persistent WANT, not a one-shot edge: set it on detected loss (or a
            // decoder reset), retry the throttled request EVERY iteration, and clear it only when a
            // fresh IDR actually re-anchors decode. The old code advanced `lastFramesDropped` on the
            // same edge it fired the throttled request — so a request swallowed by the throttle (a
            // second drop within the window, e.g. the lost recovery IDR itself being pruned) was
            // never re-sent: the counter went flat, the climb never re-fired, and the picture stayed
            // frozen for good while audio kept playing. The iPhone's lossy Wi-Fi hits this where the
            // Mac's Ethernet never does.
            var awaitingIDR = false
            var awaitingSince = Date.distantPast // when the current recovery began (for the resume log)
            var wasFailed = false
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased CM/layer temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    // Background keep-alive: drain one AU to keep QUIC flow control + host pacing
                    // healthy, then discard it BEFORE any decode/enqueue — no VideoToolbox/Metal work
                    // off-screen. Skips all recovery/gate bookkeeping too; exitBackground requests a
                    // fresh IDR and the re-anchor gate re-arms on the resumed frame-index gap.
                    if connection.isVideoDropped {
                        _ = try connection.nextAU(timeoutMs: 100)
                        return true
                    }
                    // Loss recovery (the primary path). Under the host's infinite GOP the only
                    // recovery keyframe is one we request. The reassembler drops unrecoverable AUs
                    // (framesDropped); the decoder then *conceals* the reference-missing deltas — a
                    // frozen / garbage picture that never flips the layer to .failed — so key off the
                    // drop count climbing, then keep asking (awaitingIDR) until an IDR lands. Polled
                    // every iteration so a total-loss drought still recovers when packets resume.
                    let dropped = connection.framesDropped()
                    if dropped > lastFramesDropped {
                        // Log only on the false→true transition (once per recovery cycle), not per
                        // dropped AU, so heavy loss doesn't spam the log.
                        if !awaitingIDR {
                            awaitingSince = Date()
                            pumpLog.notice(
                                "video: unrecoverable drop (framesDropped=\(dropped, privacy: .public)) — requesting recovery IDR")
                        }
                        lastFramesDropped = dropped
                        awaitingIDR = true
                    }
                    if awaitingIDR { recovery.request() }
                    // Freeze backstop: a drop-count climb arms the gate (should the frame-index gap
                    // below be lost too), and an overdue freeze re-asks for the re-anchor.
                    if gate.poll(framesDropped: dropped) { recovery.request() }

                    guard let au = try connection.nextAU(timeoutMs: 100) else { return true }
                    // Loss recovery (RFI): a forward frame-index gap fires a throttled reference-
                    // frame-invalidation request so an RFI-capable host (AMD LTR / NVENC) recovers
                    // with a cheap clean P-frame instead of a full IDR. The framesDropped-driven
                    // recovery above stays the backstop for when the recovery frame itself is lost.
                    // The same gap is the earliest, most precise signal to ARM the display freeze.
                    // Credited arm: the gap width pre-covers the reassembler's ~120 ms-later
                    // framesDropped climb for the same loss, so a fast RFI anchor that heals in
                    // between isn't re-frozen by it (the double-arm race).
                    let gapWidth = connection.noteFrameIndexGapWidth(au.frameIndex)
                    if gapWidth > 0 { gate.arm(expectingDrops: UInt64(gapWidth)) }
                    onFrame?(au)
                    let idrFormat = connection.videoCodec.formatDescription(fromKeyframe: au.data)
                    if let f = idrFormat {
                        format = f          // refreshed on every IDR (mode changes included)
                        let dims = CMVideoFormatDescriptionGetDimensions(f)
                        if lastDecodedDims?.width != dims.width || lastDecodedDims?.height != dims.height {
                            lastDecodedDims = dims
                            onDecodedSize?(Int(dims.width), Int(dims.height))
                        }
                        if awaitingIDR {
                            let ms = Int(Date().timeIntervalSince(awaitingSince) * 1000)
                            pumpLog.notice("video: recovery IDR received — resumed after \(ms, privacy: .public) ms")
                        }
                        awaitingIDR = false // a fresh IDR re-anchored decode — recovery complete
                    }
                    let failed = layer.status == .failed
                    if failed {
                        // Decode wedged hard (the cold-first-connect case — a lost/corrupt opening
                        // IDR): flush and, unless THIS AU is the recovering IDR (re-anchored above),
                        // re-gate on the next in-band parameter sets and keep asking — enqueuing a
                        // delta into a failed layer can't recover it.
                        if !wasFailed { pumpLog.warning("video: display layer .failed — flushing + re-anchoring") }
                        layer.flush()
                        gate.arm() // a wedged decoder is a loss — freeze until the re-anchor
                        if idrFormat == nil {
                            format = nil
                            awaitingIDR = true
                        }
                    }
                    wasFailed = failed
                    guard let f = format,
                          let sample = connection.videoCodec.sampleBuffer(au: au, format: f),
                          !token.isStopped // don't enqueue a stale frame after a restart
                    else { return true }
                    // Freeze-until-reanchor: while holding, WITHHOLD this concealed post-loss frame by
                    // flagging it DoNotDisplay — the layer still decodes it (keeping the reference
                    // chain fed) but shows the last GOOD picture until a clean re-anchor lifts the
                    // gate. Folded from the AU's wire flags (stage-1 has no decode callback).
                    if !gate.onDecoded(flags: au.flags) {
                        StreamPump.setDoNotDisplay(sample)
                    }
                    layer.enqueue(sample)
                    return true
                } catch {
                    if !token.isStopped {
                        onSessionEnd?()
                    }
                    return false // session closed
                }
                }
            }
        }
        thread.name = "punktfunk-pump"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Flag a sample decode-but-don't-display (`kCMSampleAttachmentKey_DoNotDisplay`). Used to
    /// withhold decoder-concealed post-loss frames while the re-anchor gate holds: the layer keeps
    /// its reference chain fed without flipping the frozen picture. No-op if the attachments array
    /// can't be materialized (then the frame just displays — the freeze degrades to the old behavior).
    private static func setDoNotDisplay(_ sample: CMSampleBuffer) {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(
            sample, createIfNecessary: true), CFArrayGetCount(attachments) > 0
        else { return }
        let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(
            dict,
            Unmanaged.passUnretained(kCMSampleAttachmentKey_DoNotDisplay).toOpaque(),
            Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection.
    func stop() {
        token.stop()
    }

    deinit { token.stop() }
}
