// Stage-2 presenter orchestrator: a pump thread pulls AUs → VideoDecoder; the decoder's async output
// drops the newest decoded frame into a 1-slot ring; the hosting view's display link calls `renderTick`
// once per vsync to draw + present the newest ready frame and stamp the unified latency stages
// (end-to-end capture→on-glass, plus the decode and display stage terms —
// design/stats-unification.md). Mirrors StreamPump's lifecycle (one per start; cancel is permanent).
//
// Threading: the pump runs on its own thread; the decoder callback on a VT thread; `renderTick` +
// `start`/`stop` on the MAIN thread (the view's CADisplayLink fires there). Only the ring (lock-guarded)
// and the decoder/presenter (internally locked / main-hopped) cross threads.

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
    /// Return a frame the display link took but could not present (a transient `nextDrawable`
    /// failure). Kept only while the slot is still empty — a newer decoded frame wins, so
    /// newest-ready ordering is preserved. Without this, a failed render silently LOSES the
    /// frame, and under the host's infinite GOP a static scene sends no replacement until the
    /// next damage — the stale picture would persist.
    func putBack(_ f: ReadyFrame) {
        lock.lock()
        if frame == nil { frame = f }
        lock.unlock()
    }
}

public final class Stage2Pipeline {
    private let ring = ReadyRing()
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    private let endToEndMeter: LatencyMeter?
    private let displayMeter: LatencyMeter?
    private let recovery = KeyframeRecovery()
    private var token = StopFlag()
    private var offsetNs: Int64 = 0
    /// Signalled when the pump thread exits, so `stop()` can join it (bounded) before `decoder.reset()`
    /// — otherwise a pump iteration already past its `token.isStopped` check can rebuild a decode session
    /// right after the reset (a brief orphan session). `pumpJoinable` is armed by `start`, consumed by
    /// the first `stop` (so the idempotent second `stop`/deinit doesn't block on an already-drained
    /// semaphore). start/stop are sequential lifecycle calls, so the plain flag is safe.
    private let pumpStopped = DispatchSemaphore(value: 0)
    private var pumpJoinable = false

    /// The Metal layer the hosting view installs + sizes.
    public var layer: CAMetalLayer { presenter.layer }

    /// Unified-stats meters (design/stats-unification.md): `endToEndMeter` records the headline
    /// end-to-end (capture→on-glass, skew-corrected); `decodeMeter` the decode stage
    /// (received→decoded); `displayMeter` the display stage (decoded→on-glass, the ring wait +
    /// render + vsync — the tail stage-2 exists to shorten). All optional: metering never gates
    /// the presenter choice. Returns nil if Metal can't be set up (headless / no GPU) — caller
    /// falls back to the stage-1 presenter.
    public init?(
        endToEndMeter: LatencyMeter?,
        decodeMeter: LatencyMeter? = nil,
        displayMeter: LatencyMeter? = nil
    ) {
        guard let presenter = MetalVideoPresenter.make() else { return nil }
        self.presenter = presenter
        self.endToEndMeter = endToEndMeter
        self.displayMeter = displayMeter
        let ring = ring
        let recovery = recovery
        self.decoder = VideoDecoder(
            onDecoded: { frame in
                // Decode stage = received→decoded, both client CLOCK_REALTIME (offset 0 — no
                // skew applies). Stamped at decode completion, so it covers every decoded frame,
                // including ones the newest-wins ring drops before present.
                decodeMeter?.record(
                    ptsNs: UInt64(frame.receivedNs), atNs: frame.decodedNs, offsetNs: 0)
                ring.submit(frame)
            },
            // Async decode failure (a bad P-frame referencing a lost/corrupt IDR): the pump resets to
            // re-gate on the next IDR, and we ask the host to send one now (infinite GOP — it wouldn't
            // otherwise come soon). Throttled in KeyframeRecovery.
            onDecodeError: { _ in recovery.request() })
    }

    /// Start pulling AUs into the decoder. MAIN THREAD. `onFrame` fires per AU at receipt (the
    /// host+network / capture→received meter, exactly as stage-1); `onSessionEnd` on close.
    /// `clockOffsetNs` (host minus client) makes the end-to-end stamp cross-machine valid.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        offsetNs = connection.clockOffsetNs
        recovery.bind(connection) // arm host-keyframe recovery for this session
        token = StopFlag() // fresh token per start — a stop is permanent (like StreamPump)

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
            while !token.isStopped {
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
                    guard let f = format, !token.isStopped else { continue }
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
                            if !token.isStopped { onSessionEnd?() }
                            break
                        }
                    }
                } catch {
                    if !token.isStopped { onSessionEnd?() }
                    break // session closed
                }
            }
        }
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        pumpJoinable = true
        thread.start()
    }

    /// MAIN thread, once per vsync. Present the newest ready frame (if any). The latency stamps
    /// use the drawable's ACTUAL on-glass instant (`addPresentedHandler`/`presentedTime` — the
    /// handler fires on a Metal callback thread; the meters are thread-safe), falling back to
    /// `targetPresentNs` — the display link's target present instant, already converted to
    /// `CLOCK_REALTIME` (see `realtimeNs(forDisplayLinkTimestamp:)`) — when the system reports
    /// no presented time (a dropped drawable). A frame that could not be rendered (no drawable
    /// yet) goes back into the ring so the next tick retries it.
    public func renderTick(targetPresentNs: Int64) {
        guard let frame = ring.take() else { return }
        let offsetNs = offsetNs
        let endToEndMeter = endToEndMeter
        let displayMeter = displayMeter
        let rendered = presenter.render(frame.pixelBuffer, isHDR: frame.isHDR) { presentedNs in
            let atNs = presentedNs ?? targetPresentNs
            // End-to-end = capture→on-glass, measured directly (skew-corrected via the
            // connect-time clock offset) — the HUD headline.
            endToEndMeter?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: offsetNs)
            // Display stage = decoded → on-glass. Both instants are client CLOCK_REALTIME,
            // so no skew offset applies.
            displayMeter?.record(ptsNs: UInt64(frame.decodedNs), atNs: atNs, offsetNs: 0)
        }
        if !rendered { ring.putBack(frame) }
    }

    /// Stop the pump (≤ one poll timeout) and drop the decode session. MAIN THREAD; idempotent. Does not
    /// close the connection. A restart needs a fresh Stage2Pipeline (the stop is permanent).
    public func stop() {
        token.stop()
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

    deinit { token.stop() }

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
