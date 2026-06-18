// The platform-independent heart of the presenters: one thread pulling AUs from the
// connection into an AVSampleBufferDisplayLayer, with the format description refreshed
// on every IDR (the host opens with an IDR carrying in-band parameter sets; recovery
// keyframes re-send them — there is no out-of-band extradata, ever). Shared by the
// macOS StreamLayerView and the iOS/iPadOS stream view.

import AVFoundation
import Foundation

/// Cancellation handle owned by exactly one pump thread — a restart hands the old pump
/// its own token, so it can never be revived by a newer start().
private final class PumpToken: @unchecked Sendable {
    private let lock = NSLock()
    private var live = true
    var isLive: Bool {
        lock.lock()
        defer { lock.unlock() }
        return live
    }
    func cancel() {
        lock.lock()
        live = false
        lock.unlock()
    }
}

/// One pump per instance; create a fresh StreamPump per start (cancel is permanent).
final class StreamPump {
    private let token = PumpToken()

    /// Pump thread: pull AUs, wrap, enqueue. Non-IDR AUs before the first format
    /// description are dropped. `onFrame`/`onSessionEnd` fire on the pump thread.
    func start(
        connection: PunktfunkConnection,
        layer: AVSampleBufferDisplayLayer,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        let token = token
        // The layer is non-Sendable but its enqueue/flush are documented thread-safe, and after
        // this point only the pump thread drives it — assert that so the @Sendable Thread closure
        // may capture it.
        nonisolated(unsafe) let layer = layer
        layer.flush() // drop any frames a previous connection left queued

        let thread = Thread {
            var format: CMVideoFormatDescription?
            var lastKeyframeRequest = Date.distantPast
            var lastFramesDropped = connection.framesDropped()
            // Coalesced host keyframe request: the decode stays wedged for several frames until
            // the IDR lands, so requesting on every frame would flood the control stream.
            func requestKeyframeThrottled() {
                let now = Date()
                if now.timeIntervalSince(lastKeyframeRequest) > 0.25 {
                    connection.requestKeyframe()
                    lastKeyframeRequest = now
                }
            }
            while token.isLive {
                do {
                    // Loss recovery (the primary recovery path). Under the host's infinite GOP the
                    // only recovery keyframe is one we request. The reassembler drops unrecoverable
                    // AUs (framesDropped); the decoder then *conceals* the reference-missing delta
                    // frames that follow — a frozen / garbage picture, WITHOUT flipping the layer to
                    // .failed — so the .failed check below rarely fires after a real network blip.
                    // Ask the host for a fresh IDR whenever the drop count climbs. Polled every
                    // iteration (not just per AU) so a total-loss drought still recovers the moment
                    // packets resume and the reassembler counts the gap.
                    let dropped = connection.framesDropped()
                    if dropped > lastFramesDropped {
                        lastFramesDropped = dropped
                        requestKeyframeThrottled()
                    }
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    onFrame?(au)
                    if let f = AnnexB.formatDescription(fromIDR: au.data) {
                        format = f // refreshed on every IDR (mode changes included)
                    }
                    if layer.status == .failed {
                        // Decode wedged hard (the cold-first-connect case — a lost/corrupt opening
                        // IDR): flush and re-gate on the next in-band parameter sets (resuming with
                        // a delta frame can't recover), AND ask the host for a fresh IDR. Throttled:
                        // the layer stays .failed across several polls until the IDR lands.
                        layer.flush()
                        format = AnnexB.formatDescription(fromIDR: au.data)
                        requestKeyframeThrottled()
                    }
                    guard let f = format,
                          let sample = AnnexB.sampleBuffer(au: au, format: f),
                          token.isLive // don't enqueue a stale frame after a restart
                    else { continue }
                    layer.enqueue(sample)
                } catch {
                    if token.isLive {
                        onSessionEnd?()
                    }
                    break // session closed
                }
            }
        }
        thread.name = "punktfunk-pump"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection.
    func stop() {
        token.cancel()
    }

    deinit { token.cancel() }
}
