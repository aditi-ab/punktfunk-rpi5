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
            // Format, decoded size, straggler filter and the keyframe WANT — shared with the
            // stage-2 pump, which had the same rules copied. `awaitingIDR` is retried every
            // iteration so a request the throttle swallowed is re-sent; loss itself goes through
            // the gate, where an RFI anchor heals it without an IDR.
            var pump = AUPumpState()
            // The layer's decoder reads the RPS itself, so a lost reference must be concealed
            // in the bitstream before enqueue (see HevcConcealer). Thread-confined; one per session.
            let concealer: HevcConcealer? = connection.videoCodec == .hevc ? HevcConcealer() : nil
            var wasUnrecoverable = false
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
                    if pump.awaitingIDR { recovery.request() }
                    // Loss recovery through the shared gate: a drop-count climb beyond the gap's
                    // credit arms the freeze and asks (the decoder conceals reference-missing
                    // deltas without flipping the layer to .failed), and an overdue freeze re-asks
                    // for the re-anchor. Polled every iteration so a total-loss drought still
                    // recovers when packets resume.
                    if gate.poll(framesDropped: connection.framesDropped()) { recovery.request() }

                    guard let received = try connection.nextAU(timeoutMs: 100) else { return true }
                    var au = received // the concealer may swap its bytes below
                    // A forward frame-index gap fires a throttled RFI (a clean P-frame, no IDR)
                    // and arms the freeze, credited with the gap width so the reassembler's
                    // ~120 ms-later framesDropped climb for the same loss cannot re-freeze a
                    // stream the anchor already healed. A lost anchor lapses into the gate's
                    // overdue re-ask above.
                    let gapWidth = connection.noteFrameIndexGapWidth(au.frameIndex)
                    if gapWidth > 0 { gate.arm(expectingDrops: UInt64(gapWidth)) }
                    onFrame?(au)
                    if pump.isStraggler(frameIndex: au.frameIndex) { return true }
                    var concealed = AUPumpState.Concealment.none
                    if let concealer {
                        switch concealer.conceal(au.data) {
                        case .intact:
                            concealed = .decodable
                        case .rewritten(let data):
                            concealed = .decodable
                            au = au.replacing(data: data)
                            pumpLog.notice(
                                "video: frame \(au.frameIndex, privacy: .public) names a lost reference — moved to a present picture until the re-anchor"
                            )
                        case .unrecoverable:
                            concealed = .unrecoverable
                        }
                        if concealed == .unrecoverable, !wasUnrecoverable {
                            pumpLog.warning(
                                "video: frame \(au.frameIndex, privacy: .public) names a lost reference with nothing to stand in — withholding until an IDR"
                            )
                        }
                        wasUnrecoverable = concealed == .unrecoverable
                    }
                    let idrFormat = connection.videoCodec.formatDescription(fromKeyframe: au.data)
                    let step = pump.note(
                        frameIndex: au.frameIndex, idrFormat: idrFormat, lossAhead: gapWidth > 0,
                        flags: au.flags, concealed: concealed)
                    if step.straggler { return true }
                    if step.askKeyframe { recovery.request() }
                    if let size = step.newSize { onDecodedSize?(size.width, size.height) }
                    if step.resumed {
                        let ms = Int(Date().timeIntervalSince(awaitingSince) * 1000)
                        pumpLog.notice("video: recovery IDR received — resumed after \(ms, privacy: .public) ms")
                    }
                    if step.startedFormatWait {
                        awaitingSince = Date()
                        pumpLog.warning(
                            "video: received AUs but no decodable format (missing/unparsed parameter sets) — requesting an IDR until one seeds it"
                        )
                    }
                    let failed = layer.status == .failed || layer.requiresFlushToResumeDecoding
                    if failed {
                        // Decode wedged (a lost opening IDR, or an iOS interruption that left the
                        // layer ignoring samples): flush and, unless THIS AU is the recovering IDR,
                        // re-gate on the next in-band parameter sets and keep asking.
                        if !wasFailed { pumpLog.warning("video: display layer wedged — flushing + re-anchoring") }
                        layer.flush()
                        gate.arm() // a wedged decoder is a loss — freeze until the re-anchor
                        if idrFormat == nil {
                            pump.requireIDR()
                            awaitingSince = Date()
                        }
                    }
                    wasFailed = failed
                    // A delta between a loss and its re-anchor references the lost picture; one
                    // such AU wedges the layer's decoder until an IDR. Withheld, the anchor lands.
                    if step.withhold { return true }
                    guard let f = pump.format,
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
