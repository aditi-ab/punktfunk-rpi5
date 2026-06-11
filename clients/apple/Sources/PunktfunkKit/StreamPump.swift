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
        layer.flush() // drop any frames a previous connection left queued

        let thread = Thread {
            var format: CMVideoFormatDescription?
            while token.isLive {
                do {
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    onFrame?(au)
                    if let f = AnnexB.formatDescription(fromIDR: au.data) {
                        format = f // refreshed on every IDR (mode changes included)
                    }
                    if layer.status == .failed {
                        // Decode wedged: flush and re-gate on the next in-band parameter
                        // sets — resuming with a delta frame can't recover. (A
                        // request-IDR channel on punktfunk/1 is a host-side TODO; with the
                        // host's infinite GOP this may otherwise stay black until the
                        // next recovery keyframe.)
                        layer.flush()
                        format = AnnexB.formatDescription(fromIDR: au.data)
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
