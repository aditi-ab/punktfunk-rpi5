// The platform-independent heart of the presenters: one thread pulling AUs from the
// connection into an AVSampleBufferDisplayLayer, with the format description refreshed
// on every IDR (the host opens with an IDR carrying in-band parameter sets; recovery
// keyframes re-send them — there is no out-of-band extradata, ever). Shared by the
// macOS StreamLayerView and the iOS/iPadOS stream view.

import AVFoundation
import Foundation
import os

private let pumpLog = ClientLog(category: "video")

/// One pump per instance; create a fresh StreamPump per start (the stop is permanent —
/// a restart hands the old pump its own token, so it can never be revived by a newer start()).
final class StreamPump {
    private let token = StopFlag()

    /// Pump thread: pull AUs, wrap, enqueue. Non-IDR AUs before the first format
    /// description are dropped. `onFrame`/`onSessionEnd` fire on the pump thread.
    ///
    /// `endToEndMeter` is stage-1's ONLY latency instrument, and it measures capture→ENQUEUE —
    /// not capture→glass like the Metal rungs: the layer decodes AND presents after our hand-off,
    /// and AVSampleBufferDisplayLayer has no presented callback, so the tail past enqueue (its
    /// internal decode + the video-plane flip) is unmeasurable from the app. Cross-rung
    /// comparisons must read this as e2e MINUS decode+display and settle the remainder on
    /// camera. It is still worth wiring: matching pre-tail halves between rungs pins any felt
    /// difference on the present tail — the video-plane-vs-compositor question itself.
    func start(
        connection: PunktfunkConnection,
        layer: AVSampleBufferDisplayLayer,
        endToEndMeter: LatencyMeter? = nil,
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
            var intake = AUIntake(connection: connection, gate: gate, recovery: recovery)
            var wasFailed = false
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased CM/layer temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    guard let ready = try intake.next(onFrame: onFrame, onDecodedSize: onDecodedSize)
                    else { return true }
                    let au = ready.au
                    let failed = layer.status == .failed
                    if failed {
                        // Decode wedged hard (the cold-first-connect case — a lost/corrupt opening
                        // IDR): flush and, unless THIS AU is the recovering IDR (the intake re-anchored),
                        // re-gate on the next in-band parameter sets and keep asking — enqueuing a
                        // delta into a failed layer can't recover it.
                        if !wasFailed { pumpLog.warning("video: display layer .failed — flushing + re-anchoring") }
                        layer.flush()
                        gate.arm() // a wedged decoder is a loss — freeze until the re-anchor
                        if !ready.idr { intake.requireIDR() }
                    }
                    wasFailed = failed
                    // A delta between a loss and its re-anchor references the lost picture; one
                    // such AU wedges the layer's decoder until an IDR. Withheld, the anchor lands.
                    if ready.step.withhold { return true }
                    guard let f = intake.pump.format,
                          let sample = connection.videoCodec.sampleBuffer(au: au, format: f),
                          !token.isStopped // don't enqueue a stale frame after a restart
                    else { return true }
                    // Freeze-until-reanchor: while holding, WITHHOLD this concealed post-loss frame by
                    // flagging it DoNotDisplay — the layer still decodes it (keeping the reference
                    // chain fed) but shows the last GOOD picture until a clean re-anchor lifts the
                    // gate. Folded from the AU's wire flags (stage-1 has no decode callback).
                    if gate.onDecoded(flags: au.flags) {
                        // Capture→enqueue (see start's doc). Only frames that will DISPLAY:
                        // a withheld frame never reaches glass, so its enqueue instant would
                        // dilute the population the Metal rungs are compared against. The
                        // offset is read PER ENQUEUE — it is live (mid-stream re-synced) and
                        // caching it rebuilds the stale-offset corruption (see clockOffsetNs).
                        endToEndMeter?.record(ptsNs: au.ptsNs, offsetNs: connection.clockOffsetNs)
                    } else {
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
