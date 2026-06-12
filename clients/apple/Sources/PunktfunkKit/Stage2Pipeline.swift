// Stage-2 presenter orchestrator: a pump thread pulls AUs → VideoDecoder; the decoder's async
// output drops the newest decoded frame into a 1-slot ring; the hosting view's display link
// calls `renderTick` once per vsync to draw + present the newest ready frame and stamp
// capture→present. Mirrors StreamPump's lifecycle (one per start; cancel is permanent).
//
// Threading: the pump runs on its own thread; the decoder callback on a VT thread; `renderTick`
// + `setDrawableSize` + `start`/`stop` on the MAIN thread (the view's CADisplayLink fires there).
// Only the ring + decoder cross threads and both are internally locked.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import QuartzCore

/// Newest-ready 1-slot ring: the decoder overwrites (drops the older undisplayed frame — lowest
/// latency, no smoothing buffer), the display link takes-and-clears. Sendable; lock-guarded.
private final class ReadyRing: @unchecked Sendable {
    private let lock = NSLock()
    private var frame: ReadyFrame?
    func submit(_ f: ReadyFrame) {
        lock.lock(); frame = f; lock.unlock()
    }
    func take() -> ReadyFrame? {
        lock.lock(); defer { lock.unlock() }
        let f = frame; frame = nil; return f
    }
}

/// Cancellation handle owned by one pump thread (same pattern as StreamPump).
private final class PumpToken: @unchecked Sendable {
    private let lock = NSLock()
    private var live = true
    var isLive: Bool { lock.lock(); defer { lock.unlock() }; return live }
    func cancel() { lock.lock(); live = false; lock.unlock() }
}

public final class Stage2Pipeline {
    private let ring = ReadyRing()
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    private let presentMeter: LatencyMeter
    private var token = PumpToken()
    private var offsetNs: Int64 = 0

    /// The Metal layer the hosting view installs + sizes. nil-init fails when Metal is
    /// unavailable so the caller can fall back to stage-1.
    public var layer: CAMetalLayer { presenter.layer }

    /// `presentMeter` records capture→present (the glass-to-glass term). Returns nil if Metal
    /// can't be set up (headless / no GPU) — caller falls back to the stage-1 presenter.
    public init?(presentMeter: LatencyMeter) {
        guard let presenter = MetalVideoPresenter() else { return nil }
        self.presenter = presenter
        self.presentMeter = presentMeter
        let ring = ring
        self.decoder = VideoDecoder(
            onDecoded: { ring.submit($0) },
            onDecodeError: { _ in /* the pump resets the session via reset() on the next IDR */ })
    }

    /// Start pulling AUs into the decoder. `onFrame` fires per AU at receipt (capture→client
    /// meter, exactly as stage-1); `onSessionEnd` on close. `clockOffsetNs` (host minus client)
    /// makes the present stamp cross-machine valid.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        offsetNs = connection.clockOffsetNs
        token = PumpToken() // fresh token per start — cancel is permanent (like StreamPump)
        let token = token
        let decoder = decoder
        let thread = Thread {
            var format: CMVideoFormatDescription?
            while token.isLive {
                do {
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    onFrame?(au)
                    if let f = AnnexB.formatDescription(fromIDR: au.data) {
                        format = f // refreshed on every IDR (mode changes included)
                    }
                    guard let f = format, token.isLive else { continue }
                    if !decoder.decode(au: au, format: f) {
                        // Submit/decoder error: drop the session and re-gate on the next IDR's
                        // in-band parameter sets (a delta frame can't recover) — stage-1's policy.
                        decoder.reset()
                    }
                } catch {
                    if token.isLive { onSessionEnd?() }
                    break // session closed
                }
            }
        }
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// MAIN thread, once per vsync. Present the newest ready frame (if any) and stamp
    /// capture→present at `targetPresentNs` — the display link's target present instant, already
    /// converted to `CLOCK_REALTIME` (see `realtimeNs(forDisplayLinkTimestamp:)`).
    public func renderTick(targetPresentNs: Int64) {
        guard let frame = ring.take() else { return }
        guard presenter.render(frame.pixelBuffer) else { return }
        presentMeter.record(ptsNs: frame.ptsNs, atNs: targetPresentNs, offsetNs: offsetNs)
    }

    /// MAIN thread. Keep the drawable matched to the negotiated mode (host can Reconfigure).
    public func setDrawableSize(_ size: CGSize) {
        presenter.setDrawableSize(size)
    }

    /// Stop the pump (≤ one poll timeout) and drop the decode session. Does not close the
    /// connection. A restart needs a fresh Stage2Pipeline (cancel is permanent).
    public func stop() {
        token.cancel()
        decoder.reset()
    }

    deinit { token.cancel() }

    /// Convert a `CADisplayLink.targetTimestamp` (CACurrentMediaTime basis) to a `CLOCK_REALTIME`
    /// nanosecond instant — the present clock the AU pts + skew offset live in. Projects to the
    /// target present time (when the frame is actually on glass), not the moment we drew.
    public static func realtimeNs(forDisplayLinkTimestamp t: CFTimeInterval) -> Int64 {
        let caNow = CACurrentMediaTime()
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let realtimeNow = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
        return realtimeNow + Int64((t - caNow) * 1_000_000_000)
    }
}
#endif
