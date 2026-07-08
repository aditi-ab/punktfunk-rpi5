// Stage-2 presenter orchestrator. GOAL ARCHITECTURE (the result of the 2026-07 pacing saga —
// read this before touching presentation):
//
//   net pump ──► VideoDecoder (VT async) ──► newest-wins 1-slot ring ──► RENDER THREAD ──► CAMetalLayer
//
// • The render thread is woken by FRAME ARRIVAL (the decoder callback signals it), never gated on
//   the display link: on macOS the WindowServer's damage tracking / FramePacing does not count our
//   out-of-band presents, so anything display-link-gated stalls exactly when the rest of the screen
//   goes quiet (adaptive-sync displays idle the link down). A decoded frame is always presented
//   promptly. The display link remains only as (a) a vsync CLOCK (phase + period, for the opt-in
//   V-Sync policy below), (b) a retry tick for a frame that couldn't get a drawable (`putBack`),
//   and (c) the iOS ProMotion rate hint.
// • The layer's own displaySyncEnabled stays FALSE on macOS — synced presents starve the drawable
//   pool outright (see MetalVideoPresenter's init for the post-mortem).
// • Present policy is a USER SETTING (DefaultsKey.vsync; PUNKTFUNK_PRESENT_MODE=immediate|vsync
//   overrides it for A/B), resolved once per session in start():
//     – V-Sync OFF (default): present immediately — lowest latency, the long-proven behavior.
//     – V-Sync ON: present(at: next vsync) predicted from the link's last phase/period, at most one
//       period ahead by construction, falling back to immediate when the link data is stale — a
//       schedule can never sit far in the future holding drawables hostage.
// • Rendering lives on its own thread so any `nextDrawable()` wait lands off-main (input, SwiftUI).
//
// The render thread also stamps the unified latency stages (end-to-end capture→on-glass + decode and
// display stage terms — design/stats-unification.md). Mirrors StreamPump's lifecycle (one per start;
// cancel is permanent). PUNKTFUNK_PRESENT_DEBUG=1 prints per-second pacing stats (see
// PresentDebugStats).
//
// Threading: the pump runs on its own thread; the decoder callback on a VT thread; the render loop on
// the render thread; `renderTick` + `start`/`stop` on the MAIN thread (the view's CADisplayLink fires
// there). Only the ring (lock-guarded), the vsync clock (lock-guarded), and the decoder/presenter
// (internally locked / staged) cross threads.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import QuartzCore

/// PUNKTFUNK_PRESENT_DEBUG=1: the render thread prints a once-per-second line with the decode
/// (ring-submit) rate, present rate, failed/empty wakes and the slowest render call — for
/// diagnosing pacing regressions without instruments. Plain print: the unbundled CLI client's
/// stdout is the cheapest reliable capture channel.
let presentDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_DEBUG"] == "1"

/// Newest-ready 1-slot ring: the decoder overwrites (drops the older undisplayed frame — lowest
/// latency, no smoothing buffer), the display link takes-and-clears. Sendable; lock-guarded.
private final class ReadyRing: @unchecked Sendable {
    private let lock = NSLock()
    private var frame: ReadyFrame?
    /// Ring submissions since the last `drainSubmitted` — the decode rate for the
    /// PUNKTFUNK_PRESENT_DEBUG stat line.
    private var submitted = 0
    func submit(_ f: ReadyFrame) {
        lock.lock(); frame = f; submitted += 1; lock.unlock()
    }
    func drainSubmitted() -> Int {
        lock.lock(); defer { lock.unlock() }
        let n = submitted; submitted = 0; return n
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

/// The display's vsync grid as last reported by the display link (target timestamp + period,
/// `CACurrentMediaTime` basis), written on main by `renderTick`, read by the render thread to
/// schedule V-Sync-mode presents. A shared box (like `ReadyRing`) so neither thread captures the
/// pipeline itself. Sendable; lock-guarded.
private final class VsyncClock: @unchecked Sendable {
    private let lock = NSLock()
    private var target: CFTimeInterval = 0
    private var period: CFTimeInterval = 0

    func set(target t: CFTimeInterval, period p: CFTimeInterval) {
        lock.lock(); target = t; period = p; lock.unlock()
    }

    /// The next vsync at or after `now`, extrapolated from the last reported phase/period — by
    /// construction less than one period ahead, so a scheduled present can never sit far in the
    /// future holding its drawable. nil (⇒ present immediately) when the link has reported nothing
    /// yet, its period is nonsense, or its data is STALE (an idle/suspended link on an
    /// adaptive-sync display — exactly the case where scheduling onto its grid stalls the stream).
    func nextVsync(after now: CFTimeInterval) -> CFTimeInterval? {
        lock.lock(); defer { lock.unlock() }
        guard period > 0.0005, target > 0, now - target < 0.25 else { return nil }
        if target >= now { return target }
        return target + ceil((now - target) / period) * period
    }
}

/// PUNKTFUNK_PRESENT_DEBUG=1 aggregation: one printed line per second from the render thread with
/// the decode rate, render outcomes, the slowest render call (≈ nextDrawable wait) and the deltas
/// between system-reported on-glass times (vsync-aligned presents show clean refresh-period
/// multiples; immediate flips scatter). Lock-guarded — `presented` lands on a Metal callback thread.
private final class PresentDebugStats: @unchecked Sendable {
    private let lock = NSLock()
    private var last = CACurrentMediaTime()
    private var ok = 0, failed = 0, empty = 0, dropped = 0
    private var maxRenderMs = 0.0
    private var lastGlassNs: Int64 = 0
    private var glassDeltasMs: [Double] = []

    func emptyWake() { lock.lock(); empty += 1; lock.unlock() }

    func renderReturned(ok rendered: Bool, tookMs: Double) {
        lock.lock()
        if rendered { ok += 1 } else { failed += 1 }
        maxRenderMs = max(maxRenderMs, tookMs)
        lock.unlock()
    }

    func presented(atNs: Int64?) {
        lock.lock()
        if let atNs {
            if lastGlassNs > 0 { glassDeltasMs.append(Double(atNs - lastGlassNs) / 1e6) }
            lastGlassNs = atNs
        } else {
            dropped += 1
        }
        lock.unlock()
    }

    func flushIfDue(ring: ReadyRing) {
        lock.lock()
        let now = CACurrentMediaTime()
        guard now - last >= 1 else { lock.unlock(); return }
        last = now
        let decoded = ring.drainSubmitted()
        let deltas = glassDeltasMs.sorted()
        let p50 = deltas.isEmpty ? 0 : deltas[deltas.count / 2]
        let dMax = deltas.last ?? 0
        let line = String(
            format: "pf-present decoded=%d ok=%d fail=%d empty=%d dropped=%d "
                + "maxRenderMs=%.1f glassDeltaMs p50=%.2f max=%.2f n=%d",
            decoded, ok, failed, empty, dropped, maxRenderMs, p50, dMax, deltas.count)
        ok = 0; failed = 0; empty = 0; dropped = 0
        maxRenderMs = 0
        glassDeltasMs.removeAll(keepingCapacity: true)
        lock.unlock()
        print(line)
        fflush(stdout) // stdout is a pipe when captured — flush per line or nothing shows
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

    /// Render-thread plumbing. `renderSignal` wakes the render thread — signalled by the DECODER
    /// callback on every frame (the primary trigger: presentation must never be gated on the
    /// display link, see the header) and by each display-link tick (the `putBack` retry + the
    /// vsync-clock refresh). Signals coalesce harmlessly (an extra wake finds an empty ring and
    /// goes back to sleep). `vsyncClock` is the link's last phase/period for V-Sync-mode
    /// scheduling. Lock-guarded boxes — the render thread, like the pump thread, must not capture
    /// `self`, or a missed stop() would leak a spinning pipeline. `renderStopped`/`renderJoinable`
    /// mirror the pump's bounded join.
    private let renderSignal = DispatchSemaphore(value: 0)
    private let vsyncClock = VsyncClock()
    private let renderStopped = DispatchSemaphore(value: 0)
    private var renderJoinable = false

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
        let renderSignal = renderSignal
        self.decoder = VideoDecoder(
            onDecoded: { frame in
                // Decode stage = received→decoded, both client CLOCK_REALTIME (offset 0 — no
                // skew applies). Stamped at decode completion, so it covers every decoded frame,
                // including ones the newest-wins ring drops before present.
                decodeMeter?.record(
                    ptsNs: UInt64(frame.receivedNs), atNs: frame.decodedNs, offsetNs: 0)
                ring.submit(frame)
                // FRAME ARRIVAL is the render trigger (never the display link — see the header).
                renderSignal.signal()
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
                    if let f = connection.videoCodec.formatDescription(fromKeyframe: au.data) {
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

        // The render thread: one present per display-link signal. It owns every layer format/colour/
        // drawable interaction (see MetalVideoPresenter's threading notes); with displaySyncEnabled on,
        // nextDrawable's up-to-a-frame wait lands here instead of on main. The 100 ms timed wait is
        // only the stop-flag poll for a session whose link stopped ticking.
        let ring = ring
        let endToEndMeter = endToEndMeter
        let displayMeter = displayMeter
        let offsetNs = offsetNs
        let renderSignal = renderSignal
        let renderStopped = renderStopped
        // Present policy — the user's V-Sync setting (default OFF = immediate, the long-proven
        // lowest-latency behavior); PUNKTFUNK_PRESENT_MODE=immediate|vsync overrides it for A/B.
        // Resolved once per session.
        let presentMode = ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_MODE"]
        let vsyncEnabled = presentMode == "vsync"
            || (presentMode != "immediate"
                && UserDefaults.standard.bool(forKey: DefaultsKey.vsync))
        let debugStats = presentDebug ? PresentDebugStats() : nil
        let vsyncClock = vsyncClock
        let renderThread = Thread {
            defer { renderStopped.signal() }
            while !token.isStopped {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring)
                    continue
                }
                guard !token.isStopped, let frame = ring.take() else {
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring)
                    continue
                }
                // V-Sync ON: flip on the next predicted vsync (< one period out, stale link ⇒
                // immediate — see VsyncClock). OFF: flip as soon as the GPU finishes.
                let presentAt = vsyncEnabled
                    ? vsyncClock.nextVsync(after: CACurrentMediaTime()) : nil
                let renderStarted = CACurrentMediaTime()
                let rendered = presenter.render(
                    frame.pixelBuffer, isHDR: frame.isHDR, presentAtMediaTime: presentAt
                ) { presentedNs in
                    // Fallback stamp for a dropped drawable (no system presentedTime): "now" on
                    // the Metal callback, converted to the CLOCK_REALTIME the meters live in.
                    let atNs = presentedNs
                        ?? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime())
                    // End-to-end = capture→on-glass, measured directly (skew-corrected via the
                    // connect-time clock offset) — the HUD headline.
                    endToEndMeter?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: offsetNs)
                    // Display stage = decoded → on-glass. Both instants are client CLOCK_REALTIME,
                    // so no skew offset applies.
                    displayMeter?.record(ptsNs: UInt64(frame.decodedNs), atNs: atNs, offsetNs: 0)
                    debugStats?.presented(atNs: presentedNs)
                }
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered { ring.putBack(frame) }
                debugStats?.flushIfDue(ring: ring)
            }
        }
        renderThread.name = "punktfunk-stage2-render"
        renderThread.qualityOfService = .userInteractive
        renderJoinable = true
        renderThread.start()
    }

    /// MAIN thread, once per display-link tick: refresh the vsync clock (V-Sync-mode scheduling)
    /// and nudge the render thread. The nudge is NOT the presentation trigger — frame arrival is
    /// (see the header) — it only retries a frame a transient `nextDrawable` failure put back into
    /// the ring, which matters under the host's infinite GOP where a static scene sends no
    /// replacement frame.
    public func renderTick(targetMediaTime: CFTimeInterval, period: CFTimeInterval) {
        vsyncClock.set(target: targetMediaTime, period: period)
        renderSignal.signal()
    }

    /// Forward the layout-derived drawable pixel size to the presenter (MAIN thread — see
    /// `MetalVideoPresenter.setDrawableTarget`).
    public func setDrawableTarget(_ size: CGSize) {
        presenter.setDrawableTarget(size)
    }

    /// Stop the pump + render thread (≤ one poll timeout each) and drop the decode session. MAIN
    /// THREAD; idempotent. Does not close the connection. A restart needs a fresh Stage2Pipeline
    /// (the stop is permanent).
    public func stop() {
        token.stop()
        // Join the pump (bounded: ≤ one nextAU poll + an in-flight decode) before resetting the decoder,
        // so the pump can't rebuild a session right after the reset. Only the first stop joins; a
        // repeat/deinit stop skips the already-drained semaphore.
        if pumpJoinable {
            pumpJoinable = false
            _ = pumpStopped.wait(timeout: .now() + 0.5)
        }
        // Wake + join the render thread (bounded: it may sit in `nextDrawable` for up to ~a frame; a
        // timed-out join is fine — the loop exits at its next stop-flag check, and a final present on
        // the detached layer is harmless).
        if renderJoinable {
            renderJoinable = false
            renderSignal.signal()
            _ = renderStopped.wait(timeout: .now() + 0.5)
        }
        decoder.reset()
        recovery.bind(nil) // stop requesting keyframes once the session is torn down
    }

    deinit {
        token.stop()
        renderSignal.signal() // wake the render thread so it can observe the stop and exit
    }

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
