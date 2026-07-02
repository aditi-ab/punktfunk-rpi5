// Stage-2 presenter orchestrator: a pump thread pulls AUs → VideoDecoder; the decoder's async output
// drops the newest decoded frame into a 1-slot ring; the hosting view's display link calls `renderTick`
// once per vsync to draw + present the newest ready frame and stamp capture→present. Mirrors
// StreamPump's lifecycle (one per start; cancel is permanent).
//
// Threading: the pump runs on its own thread; the decoder callback on a VT thread; `renderTick` +
// `start`/`stop` on the MAIN thread (the view's CADisplayLink fires there). Only the ring (lock-guarded)
// and the decoder/presenter (internally locked / main-hopped) cross threads.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import QuartzCore

/// Weak-target wrapper for CADisplayLink. The link retains its target, so targeting a view directly
/// makes a `view → link → view` cycle that only `invalidate()` breaks — if a teardown is ever missed
/// the view leaks and keeps ticking. This proxy holds the handler weakly, so the view can deallocate
/// and its `deinit` invalidate the link.
public final class DisplayLinkProxy: NSObject {
    private let onTick: (CADisplayLink) -> Void
    public init(_ onTick: @escaping (CADisplayLink) -> Void) { self.onTick = onTick }
    @objc public func tick(_ link: CADisplayLink) { onTick(link) }
}

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

/// Throttled host keyframe requests for decode recovery. The decoder's async error callback (a VT
/// thread) and the pump thread (a submit failure) both signal a wedge; this coalesces them so the
/// control stream isn't flooded while the decode stays stalled for several frames until the requested
/// IDR lands. Bound to the live connection in `start`, unbound in `stop`.
private final class KeyframeRecovery: @unchecked Sendable {
    private let lock = NSLock()
    private var connection: PunktfunkConnection?
    private var lastNs: UInt64 = 0

    func bind(_ c: PunktfunkConnection?) {
        lock.lock(); connection = c; lastNs = 0; lock.unlock()
    }

    func request() {
        lock.lock()
        let now = DispatchTime.now().uptimeNanoseconds
        let due = lastNs == 0 || now &- lastNs > 100_000_000 // ≥ 100 ms since the last request
        if due { lastNs = now }
        let conn = due ? connection : nil
        lock.unlock()
        conn?.requestKeyframe()
    }
}

public final class Stage2Pipeline {
    private let ring = ReadyRing()
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    private let presentMeter: LatencyMeter
    private let recovery = KeyframeRecovery()
    private var token = PumpToken()
    private var offsetNs: Int64 = 0
    /// Signalled when the pump thread exits, so `stop()` can join it (bounded) before `decoder.reset()`
    /// — otherwise a pump iteration already past its `token.isLive` check can rebuild a decode session
    /// right after the reset (a brief orphan session). `pumpJoinable` is armed by `start`, consumed by
    /// the first `stop` (so the idempotent second `stop`/deinit doesn't block on an already-drained
    /// semaphore). start/stop are sequential lifecycle calls, so the plain flag is safe.
    private let pumpStopped = DispatchSemaphore(value: 0)
    private var pumpJoinable = false

    /// The Metal layer the hosting view installs + sizes.
    public var layer: CAMetalLayer { presenter.layer }

    /// `presentMeter` records capture→present (the glass-to-glass term). Returns nil if Metal can't be
    /// set up (headless / no GPU) — caller falls back to the stage-1 presenter.
    public init?(presentMeter: LatencyMeter) {
        guard let presenter = MetalVideoPresenter.make() else { return nil }
        self.presenter = presenter
        self.presentMeter = presentMeter
        let ring = ring
        let recovery = recovery
        self.decoder = VideoDecoder(
            onDecoded: { ring.submit($0) },
            // Async decode failure (a bad P-frame referencing a lost/corrupt IDR): the pump resets to
            // re-gate on the next IDR, and we ask the host to send one now (infinite GOP — it wouldn't
            // otherwise come soon). Throttled in KeyframeRecovery.
            onDecodeError: { _ in recovery.request() })
    }

    /// Start pulling AUs into the decoder. MAIN THREAD. `onFrame` fires per AU at receipt (capture→client
    /// meter, exactly as stage-1); `onSessionEnd` on close. `clockOffsetNs` (host minus client) makes the
    /// present stamp cross-machine valid.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        offsetNs = connection.clockOffsetNs
        recovery.bind(connection) // arm host-keyframe recovery for this session
        token = PumpToken() // fresh token per start — cancel is permanent (like StreamPump)

        // Configure the decoder's chroma + the layer's initial colorimetry before the first frame. The
        // chroma subsampling drives only the decode pixel format (orthogonal to HDR/depth); the HDR
        // config is the Welcome's latched value, which a mid-session flip then overrides per-frame.
        decoder.setChroma444(connection.isChroma444)
        decoder.setCodec(connection.videoCodec)
        presenter.configure(hdr: connection.isHDR)

        let token = token
        let decoder = decoder
        let recovery = recovery
        let presenter = presenter
        let pumpStopped = pumpStopped
        let thread = Thread {
            defer { pumpStopped.signal() } // let stop() join the pump (bounded) before decoder.reset()
            var format: CMVideoFormatDescription?
            var lastFramesDropped = connection.framesDropped()
            // Persistent recovery WANT, not a one-shot edge (see StreamPump for the full rationale):
            // keep asking until an IDR lands so a request swallowed by the throttle is re-sent.
            var awaitingIDR = false
            // 4:4:4 backstop: a run of decode/create failures in a 4:4:4 session means this device can't
            // decode 4:4:4 at the negotiated resolution (the HW probe clears the common case but not a
            // resolution-ceiling miss). End cleanly instead of looping on a black screen.
            var decodeFailRun = 0
            while token.isLive {
                do {
                    // Loss recovery (the primary path). The reassembler drops unrecoverable AUs and the
                    // decoder conceals the reference-missing deltas — often WITHOUT an error callback —
                    // so key off the drop count climbing, then keep asking (awaitingIDR) until a fresh
                    // IDR re-anchors decode.
                    let dropped = connection.framesDropped()
                    if dropped > lastFramesDropped {
                        lastFramesDropped = dropped
                        awaitingIDR = true
                    }
                    if awaitingIDR { recovery.request() }
                    // Drain HDR mastering metadata (0xCE) and hand it to the PRESENTER (→ CAEDRMetadata).
                    // Polled UNCONDITIONALLY (not gated on connection.isHDR, the fixed Welcome flag): the
                    // host sends 0xCE only for HDR, INCLUDING a mid-session SDR→HDR transition (a game
                    // entering HDR — the host re-inits its encoder) the Welcome flag would never reflect.
                    // Non-blocking; nil for an SDR stream.
                    if let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
                        presenter.setHdrMeta(meta)
                    }
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    onFrame?(au)
                    if let f = AnnexB.formatDescription(fromIDR: au.data, codec: connection.videoCodec) {
                        format = f          // refreshed on every IDR (mode changes included)
                        awaitingIDR = false // a fresh IDR re-anchored decode — recovery complete
                    }
                    guard let f = format, token.isLive else { continue }
                    if decoder.decode(au: au, format: f) {
                        decodeFailRun = 0
                    } else {
                        // Submit/decoder error: drop the session and re-gate on the next IDR's in-band
                        // parameter sets (a delta frame can't recover) and keep asking for that IDR.
                        decoder.reset()
                        awaitingIDR = true
                        decodeFailRun += 1
                        // ~3 s of solid failure in a 4:4:4 session (and only there — a 4:2:0 loss
                        // recovers within a GOP) ⇒ 4:4:4 isn't decodable here; end the session.
                        if connection.isChroma444, decodeFailRun >= 180 {
                            if token.isLive { onSessionEnd?() }
                            break
                        }
                    }
                } catch {
                    if token.isLive { onSessionEnd?() }
                    break // session closed
                }
            }
        }
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        pumpJoinable = true
        thread.start()
    }

    /// MAIN thread, once per vsync. Present the newest ready frame (if any) and stamp capture→present at
    /// `targetPresentNs` — the display link's target present instant, already converted to
    /// `CLOCK_REALTIME` (see `realtimeNs(forDisplayLinkTimestamp:)`).
    public func renderTick(targetPresentNs: Int64) {
        guard let frame = ring.take() else { return }
        guard presenter.render(frame.pixelBuffer, isHDR: frame.isHDR) else { return }
        presentMeter.record(ptsNs: frame.ptsNs, atNs: targetPresentNs, offsetNs: offsetNs)
    }

    /// Stop the pump (≤ one poll timeout) and drop the decode session. MAIN THREAD; idempotent. Does not
    /// close the connection. A restart needs a fresh Stage2Pipeline (cancel is permanent).
    public func stop() {
        token.cancel()
        // Join the pump (bounded: ≤ one nextAU poll + an in-flight decode) before resetting the decoder,
        // so the pump can't rebuild a session right after the reset. Only the first stop joins; a
        // repeat/deinit stop skips the already-drained semaphore.
        if pumpJoinable {
            pumpJoinable = false
            _ = pumpStopped.wait(timeout: .now() + 0.5)
        }
        decoder.reset()
        recovery.bind(nil) // stop requesting keyframes once the session is torn down
    }

    deinit { token.cancel() }

    /// Convert a `CADisplayLink.targetTimestamp` (CACurrentMediaTime basis) to a `CLOCK_REALTIME`
    /// nanosecond instant — the present clock the AU pts + skew offset live in. Projects to the target
    /// present time (when the frame is actually on glass), not the moment we drew.
    public static func realtimeNs(forDisplayLinkTimestamp t: CFTimeInterval) -> Int64 {
        let caNow = CACurrentMediaTime()
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let realtimeNow = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
        return realtimeNow + Int64((t - caNow) * 1_000_000_000)
    }
}
#endif
