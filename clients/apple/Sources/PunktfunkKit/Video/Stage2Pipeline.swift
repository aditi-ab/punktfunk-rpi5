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
// • Present PACING is the stage-2/3/4 presenter split (`PresentPacing`, chosen per session by
//   SessionPresenter from the presenter setting / PUNKTFUNK_PRESENTER): stage-2 presents on
//   frame arrival; stage-3 additionally gates presents on the on-glass callback (`PresentGate`)
//   so the layer's FIFO image queue can never saturate; stage-4 (iOS/tvOS) presents into
//   CAMetalDisplayLink-vended drawables the moment a frame decodes — deadline pacing, where the
//   queue cannot exist at all — see PresentPacing's doc for the full rationale. Under deadline
//   pacing the render thread below is fed by BOTH the decoder callback and the link's per-refresh
//   updates (which vend the drawable), and the V-Sync policy/vsync clock don't apply.
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
import Metal
import PunktfunkShared
import QuartzCore
import os

/// PUNKTFUNK_PRESENT_DEBUG=1: the render thread prints a once-per-second line with the decode
/// (ring-submit) rate, present rate, failed/empty wakes and the slowest render call — for
/// diagnosing pacing regressions without instruments. Plain print: the unbundled CLI client's
/// stdout is the cheapest reliable capture channel.
let presentDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_DEBUG"] == "1"

/// The pf-present line's os_log mirror (subsystem io.unom.punktfunk, category "present") — the
/// SessionModel "stats" mirror's sibling, so DEADLINE sessions stream their pacing decomposition
/// to Console.app wirelessly with no env var / Xcode attach. Always on for deadline pacing (the
/// stats are a few arrays + one log line per second); other pacings keep the env-gated print.
private let presentLog = Logger(subsystem: "io.unom.punktfunk", category: "present")

/// Decoded-frame hand-off between the decode half and the render thread. The POLICY is the
/// user's presentation intent (design/apple-presentation-rebuild.md — the 2026-07 rebuild that
/// replaced the visible stage picker):
///
/// - `.newestWins` (Prioritize lowest latency, the default): a 1-slot ring — the decoder
///   overwrites (drops the older undisplayed frame), the render thread takes-and-clears. Zero
///   store by construction: any deeper app-held buffer ahead of a latch-paced display becomes a
///   STANDING queue costing one full refresh per slot, forever (the depth-2 gate post-mortem —
///   see SessionPresenter.gateDepth).
/// - `.fifo(capacity: K)` (Prioritize smoothness): a small deliberate jitter buffer. The
///   decoder appends; overflow drops the OLDEST (bounded added latency — the newest keeps
///   flowing); the render thread pops the oldest ONE per present opportunity, so the cadence is
///   the display's. `take` withholds frames until the buffer has PREROLLED to capacity once —
///   without preroll a steady stream drains every frame on arrival and headroom never builds —
///   and re-arms preroll when it runs dry (an underflow: the previous frame persists on glass,
///   a repeat by omission, while headroom rebuilds). Each buffered frame ≈ one refresh interval
///   of jitter absorbed for one interval of added display latency, which the metrics SHOW —
///   only the OS present floor is shaved from the HUD, never the user's chosen buffer.
///
/// Sendable; lock-guarded — decoder callbacks and the render thread cross here.
public enum FrameStorePolicy: Sendable, Equatable {
    case newestWins
    case fifo(capacity: Int)
}

public final class FrameStore<Frame>: @unchecked Sendable {
    private let lock = NSLock()
    private let capacity: Int // 1 = newest-wins semantics
    private let isFifo: Bool
    private var frames: [Frame] = []
    private var prerolled = false
    /// Submissions since the last `drainSubmitted` — the decode rate for the pf-present line.
    private var submitted = 0
    /// Smoothness accounting for the pf-present line: frames dropped by a full buffer, and
    /// runs-dry that re-armed preroll.
    private var overflowDrops = 0
    private var underflows = 0

    public init(policy: FrameStorePolicy) {
        switch policy {
        case .newestWins:
            capacity = 1
            isFifo = false
        case .fifo(let k):
            capacity = max(1, k)
            isFifo = true
        }
    }

    func submit(_ f: Frame) {
        lock.lock()
        if isFifo {
            frames.append(f)
            if frames.count > capacity {
                frames.removeFirst() // oldest goes — bounded latency, the newest keeps flowing
                overflowDrops += 1
            }
        } else {
            frames = [f] // newest wins; the replaced frame is the intended drop point
        }
        submitted += 1
        lock.unlock()
    }

    func drainSubmitted() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = submitted
        submitted = 0
        return n
    }

    /// Take-and-reset the smoothness counters (the pf-present `qDrop`/`qDry` stats).
    func drainSmoothing() -> (overflowDrops: Int, underflows: Int) {
        lock.lock()
        defer { lock.unlock() }
        let out = (overflowDrops, underflows)
        overflowDrops = 0
        underflows = 0
        return out
    }

    func take() -> Frame? {
        lock.lock()
        defer { lock.unlock() }
        if isFifo {
            if !prerolled {
                guard frames.count >= capacity else { return nil } // still building headroom
                prerolled = true
            }
            guard !frames.isEmpty else {
                underflows += 1 // ran dry — repeat by omission, rebuild headroom
                prerolled = false
                return nil
            }
            return frames.removeFirst()
        }
        let f = frames.first
        frames.removeAll(keepingCapacity: true)
        return f
    }

    /// Return a frame the render thread took but could not present (no drawable yet, or a
    /// transient render failure). Newest-wins keeps it only while the slot is still empty — a
    /// newer decoded frame wins; FIFO reinserts it at the FRONT (it is the oldest; a transient
    /// capacity+1 is trimmed by the next submit). Without this, a failed present silently LOSES
    /// the frame, and under the host's infinite GOP a static scene sends no replacement until
    /// the next damage — the stale picture would persist.
    func putBack(_ f: Frame) {
        lock.lock()
        if isFifo {
            frames.insert(f, at: 0)
        } else if frames.isEmpty {
            frames = [f]
        }
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

/// When a ready frame is pushed to the layer — the stage-2 vs stage-3 presenter split. Same decode
/// half, same newest-wins ring; only the present cadence differs.
///
/// - `arrival` (stage-2, the macOS default): present the moment a frame is decoded. Lowest latency
///   while the layer's image queue is shallow — but that queue is FIFO and consumed at one drawable
///   per refresh (iOS always vsync-latches; the macOS 26 compositor latch-paces our out-of-band
///   presents the same way when composited), so at stream rate ≈ refresh rate its depth is STICKY:
///   one early burst (session start, a Wi-Fi clump) fills it to `maximumDrawableCount` and — with
///   arrivals and latches then running at the same rate — it never drains. Every later frame rides
///   ~2–3 refreshes of queue (the measured 23–30 ms display stage on 120 Hz ProMotion panels), and
///   the full-queue regime is where host↔panel clock drift turns into periodic repeats/drops (the
///   "fixed-interval" jitter reports).
/// - `glass` (stage-3, the tvOS default): at most a small BOUNDED number of presented-but-
///   undisplayed drawables in flight (`PresentGate`; depth 1 — see `SessionPresenter.gateDepth`
///   for why deeper is a regression). The render thread presents only while a gate slot is free
///   (a drawable's presented handler reopens its slot and re-signals); frames decoded meanwhile
///   coalesce in the newest-wins ring. Freshness is preserved by DROPPING stale frames before
///   present instead of queueing them behind the display — the hidden queue latency becomes
///   explicit, correct frame drops. The residual cost: presents serialize on the on-glass
///   callback, whose own delivery latency pushes each present ~a refresh past the frame's decode
///   (the field-measured 14 ms display stage at 120 Hz vs the ~half-refresh floor).
/// - `deadline` (stage-4, the iOS/iPadOS default; iOS/tvOS only — see
///   `PresenterChoice.explicit`): a CAMetalDisplayLink vends ONE drawable per refresh
///   (`preferredFrameLatency` 1) into a newest-wins hand-off slot, and the render thread pairs
///   it with the newest decoded frame THE MOMENT either half arrives — usually the frame, into
///   an already-vended drawable. The image queue cannot exist (one vended drawable in flight,
///   ever), nothing serializes on the on-glass callback (the link's next vend is the pace), and
///   the present is deadline-timed by the system to latch the upcoming refresh. This is the only
///   pacing whose steady state can reach the sub-refresh display floor on the always-vsync-latch
///   platforms; `arrival`/`glass` remain the on-device A/B rungs.
///
/// macOS PyroWave sessions default to `glass` even though the platform default is stage-2: burst
/// presents into a composited (windowed) layer are the trigger pattern for the macOS DCP
/// "mismatched swapID's" KERNEL PANIC, and the one-in-flight gate removes that pattern — see
/// `SessionPresenter.pacing` for the full rationale.
public enum PresentPacing: Sendable {
    case arrival
    case glass
    case deadline
}

/// Newest-wins 1-slot hand-off box (the generic sibling of `ReadyRing`): deadline pacing's
/// drawable stash — the link thread `put`s each update's vended drawable (replacing an
/// unpresented older one, which just returns to the layer's pool), the render thread `take`s.
/// `putBack` returns a taken value only while the slot is still empty, so a fresher `put` from
/// the other thread is never clobbered by a stale return. Internal (not private) for unit tests.
/// Sendable; lock-guarded.
final class LatestBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T?
    func put(_ v: T) { lock.lock(); value = v; lock.unlock() }
    func putBack(_ v: T) {
        lock.lock()
        if value == nil { value = v }
        lock.unlock()
    }
    func take() -> T? {
        lock.lock()
        defer { lock.unlock() }
        let v = value
        value = nil
        return v
    }
}

/// Deadline pacing's staged frame-rate hint. SessionPresenter pushes the stream rate from the
/// MAIN thread (session start + every layout/Reconfigure); the link's own thread drains and
/// applies it, so the CAMetalDisplayLink is only ever touched from the thread that runs it. The
/// floor is PINNED at the stream rate — no idle ramp-down: with a low floor the link idles toward
/// it on a static scene (infinite GOP ⇒ no frames), and the first damage frame after idle would
/// wait out a slow tick before it could present. Empty wakes at stream rate are near-free; the
/// PANEL still idles via VRR because no presents happen. Sendable; lock-guarded.
private final class FrameRateHint: @unchecked Sendable {
    private let lock = NSLock()
    private var pending: CAFrameRateRange?
    func stage(hz: Float) {
        guard hz > 0 else { return }
        lock.lock()
        pending = CAFrameRateRange(minimum: hz, maximum: max(hz, 120), preferred: hz)
        lock.unlock()
    }
    func drain() -> CAFrameRateRange? {
        lock.lock()
        defer { lock.unlock() }
        let p = pending
        pending = nil
        return p
    }
}

/// The CAMetalDisplayLink delegate for deadline pacing: each per-refresh update stashes its
/// vended drawable (newest wins) and nudges the render thread — which also wakes on decoder
/// arrivals, so whichever half completes the (frame, drawable) pair triggers the present. Also
/// applies the staged frame-rate hint from the link's own thread. Retained by the link thread's
/// closure (the link holds it weak); captures only the shared boxes, never the pipeline — the
/// same no-self-capture rule as the pump/render threads.
private final class DeadlineLinkDelegate: NSObject, CAMetalDisplayLinkDelegate {
    private let stash: LatestBox<CAMetalDrawable>
    private let renderSignal: DispatchSemaphore
    private let hint: FrameRateHint
    private let stats: PresentDebugStats?
    /// The OS-floor sampler (design/apple-presentation-rebuild.md): every update's vend→glass
    /// lead is recorded so its p50 becomes the "OS present floor" the HUD subtracts from the
    /// shown display/e2e numbers. Self-adapting — reads ~2 refresh periods composited today,
    /// would read ~1 under direct-to-display, tracks VRR rate changes.
    private let floorMeter: LatencyMeter?
    /// One-shot: log the link's EFFECTIVE preferredFrameLatency after the first re-assert —
    /// reads 1 while vendLeadMs sits at ~2 periods ⇒ the scheduler ignores the request while
    /// the layer is composited (the promotion hunt); reads 2 ⇒ the system clamped it outright.
    private var loggedEffective = false

    init(
        stash: LatestBox<CAMetalDrawable>, renderSignal: DispatchSemaphore,
        hint: FrameRateHint, stats: PresentDebugStats?, floorMeter: LatencyMeter?
    ) {
        self.stash = stash
        self.renderSignal = renderSignal
        self.hint = hint
        self.stats = stats
        self.floorMeter = floorMeter
    }

    func metalDisplayLink(_ link: CAMetalDisplayLink, needsUpdate update: CAMetalDisplayLink.Update) {
        if let range = hint.drain(), link.preferredFrameRateRange != range {
            link.preferredFrameRateRange = range
        }
        // Re-assert the minimum-latency request every update (cheap compare): it was set once
        // before add(to:), and whether a pre-add set survives scheduling is exactly the kind of
        // thing the vendLeadMs stat exists to catch — belt and braces.
        if link.preferredFrameLatency != 1 { link.preferredFrameLatency = 1 }
        if !loggedEffective {
            loggedEffective = true
            let range = link.preferredFrameRateRange
            let msg = String(
                format: "deadline link up: effective preferredFrameLatency=%.2f "
                    + "range=%.0f-%.0f preferred=%.0f",
                link.preferredFrameLatency, range.minimum, range.maximum, range.preferred ?? 0)
            presentLog.info("\(msg, privacy: .public)")
        }
        // The link's own pipeline depth, measured: how far ahead of glass this vend runs.
        let leadS = update.targetPresentationTimestamp - CACurrentMediaTime()
        stats?.vendLead(ms: leadS * 1000)
        // Same measurement into the floor meter (as a LatencyMeter sample: end = now, start =
        // now − lead) — its 1 s p50 is the OS present floor SessionModel shaves off.
        if leadS > 0, let floorMeter {
            var ts = timespec()
            clock_gettime(CLOCK_REALTIME, &ts)
            let nowNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
            floorMeter.record(
                ptsNs: UInt64(nowNs - Int64(leadS * 1_000_000_000)), atNs: nowNs, offsetNs: 0)
        }
        stash.put(update.drawable)
        renderSignal.signal()
    }
}

/// Stage-3's present gate: admits `capacity` in-flight (presented, not yet on glass) drawables.
/// The render thread `tryAcquire`s before taking a frame; the drawable's presented handler
/// `release`s and re-signals the render thread. Depth 1 fully serializes presents on the on-glass
/// callback — which costs a refresh whenever the callback's own latency pushes the next present
/// past a vsync; depth 2 keeps one flip queued behind the one scanning out, so a decoded frame
/// presents immediately and latches the very next vsync while the queue still can't build (see
/// `SessionPresenter.gateDepth` for the per-platform choice). `staleAfter` is insurance against a
/// present whose handler never fires (the macOS "out-of-band presents aren't damage" hazard class
/// — see MetalVideoPresenter's init post-mortem): rather than freezing the stream, a full gate
/// force-opens a slot 100 ms after its oldest present, a visible ~10 fps degradation that
/// PUNKTFUNK_PRESENT_DEBUG's `forced` counter exposes (it reads 0 on healthy systems). Internal
/// (not private) for unit tests. Sendable; lock-guarded — the releaser runs on a Metal callback
/// thread.
final class PresentGate: @unchecked Sendable {
    /// How long one pending present may hold its slot before it's presumed lost.
    static let staleAfter: CFTimeInterval = 0.1

    private let lock = NSLock()
    private let capacity: Int
    /// Arm instants of the in-flight presents, oldest first (≤ `capacity` entries).
    private var armed: [CFTimeInterval] = []
    private var forced = 0

    /// `capacity` = the in-flight present budget (clamped to ≥ 1) — see the type doc.
    init(capacity: Int = 1) {
        self.capacity = max(1, capacity)
    }

    /// Arm the gate for one present. False = the gate is full of live presents (none stale) —
    /// leave the frame in the ring; a presented handler's release/re-signal (or the next
    /// display-link tick) retries with the freshest frame then.
    func tryAcquire(now: CFTimeInterval) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if armed.count >= capacity {
            // Full: reopen only by presuming the OLDEST in-flight present lost (its handler
            // never fired) rather than stalling the stream.
            guard let oldest = armed.first, now - oldest > Self.staleAfter else { return false }
            armed.removeFirst()
            forced += 1
        }
        armed.append(now)
        return true
    }

    /// One in-flight present reached glass (or was dropped, or its render failed before a present
    /// was registered) — free the oldest slot. A release with nothing in flight is a no-op; a
    /// lost present's handler firing late after its stale force-open can transiently over-admit
    /// one flip, which the next glass callback corrects.
    func release() {
        lock.lock()
        if !armed.isEmpty { armed.removeFirst() }
        lock.unlock()
    }

    /// Take-and-reset the force-open count (PUNKTFUNK_PRESENT_DEBUG's `forced` stat).
    func drainForced() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = forced
        forced = 0
        return n
    }
}

/// PUNKTFUNK_PRESENT_DEBUG=1 aggregation: one printed line per second from the render thread with
/// the decode rate, render outcomes, the slowest render call (≈ nextDrawable wait) and the deltas
/// between system-reported on-glass times (vsync-aligned presents show clean refresh-period
/// multiples; immediate flips scatter). Lock-guarded — `presented` lands on a Metal callback thread.
private final class PresentDebugStats: @unchecked Sendable {
    private let lock = NSLock()
    private var last = CACurrentMediaTime()
    private var ok = 0, failed = 0, empty = 0, dropped = 0, gated = 0, noDrawable = 0
    private var maxRenderMs = 0.0
    private var lastGlassNs: Int64 = 0
    private var glassDeltasMs: [Double] = []
    /// Present-issue → on-glass delay per frame (system presentedTime minus the render call's
    /// start) — the DIRECT decomposition of the display stage: ring/pairing wait lives upstream
    /// of it, queue + present-pipeline cost inside it. Standing queue reads as ~n×period here;
    /// a healthy latch reads under one period.
    private var latchMs: [Double] = []
    /// Deadline pacing: the link's own pipeline depth — `targetPresentationTimestamp - now` at
    /// each update. ~1 period means preferredFrameLatency=1 is honored (a vended drawable can
    /// reach glass at the NEXT refresh); ~2 periods means the system is running a frame ahead
    /// and one whole refresh of the display stage lives INSIDE the link, not in our pairing.
    private var vendLeadMs: [Double] = []
    /// Presented-but-not-yet-on-glass drawables right now / the window's peak — the direct
    /// measurement of the layer image-queue depth the stage-3 gate exists to bound (stage-2 on a
    /// 120 Hz panel saturates this at ~maximumDrawableCount; stage-3 pegs it at the gate depth).
    private var inFlight = 0
    private var maxInFlight = 0

    func emptyWake() { lock.lock(); empty += 1; lock.unlock() }

    /// A wake that found the stage-3 gate closed (a present still in flight) — the frame stays in
    /// the ring for the handler's re-signal. Includes display-link ticks while gated; a high count
    /// is normal, it just shows the gate working.
    func gatedWake() { lock.lock(); gated += 1; lock.unlock() }

    /// Deadline pacing: a decoded frame is waiting but the link hasn't vended this interval's
    /// drawable yet — the frame presents on the link's next update. A high count just means
    /// decode outruns the link's phase; the wait is bounded by one refresh.
    func noDrawableWake() { lock.lock(); noDrawable += 1; lock.unlock() }

    /// Deadline pacing, LINK thread: one update's vend-to-target distance (see `vendLeadMs`).
    func vendLead(ms: Double) { lock.lock(); vendLeadMs.append(ms); lock.unlock() }

    func renderReturned(ok rendered: Bool, tookMs: Double) {
        lock.lock()
        if rendered {
            ok += 1
            inFlight += 1
            maxInFlight = max(maxInFlight, inFlight)
        } else {
            failed += 1
        }
        maxRenderMs = max(maxRenderMs, tookMs)
        lock.unlock()
    }

    func presented(atNs: Int64?, issuedNs: Int64) {
        lock.lock()
        inFlight = max(0, inFlight - 1) // clamp: the handler can beat renderReturned's increment
        if let atNs {
            if lastGlassNs > 0 { glassDeltasMs.append(Double(atNs - lastGlassNs) / 1e6) }
            lastGlassNs = atNs
            latchMs.append(Double(atNs - issuedNs) / 1e6)
        } else {
            dropped += 1
        }
        lock.unlock()
    }

    func flushIfDue(ring: FrameStore<ReadyFrame>, gate: PresentGate?) {
        lock.lock()
        let now = CACurrentMediaTime()
        guard now - last >= 1 else { lock.unlock(); return }
        last = now
        let decoded = ring.drainSubmitted()
        let smoothing = ring.drainSmoothing()
        let deltas = glassDeltasMs.sorted()
        let p50 = deltas.isEmpty ? 0 : deltas[deltas.count / 2]
        let dMax = deltas.last ?? 0
        let latches = latchMs.sorted()
        let latchP50 = latches.isEmpty ? 0 : latches[latches.count / 2]
        let latchMax = latches.last ?? 0
        let vends = vendLeadMs.sorted()
        let vendP50 = vends.isEmpty ? 0 : vends[vends.count / 2]
        let vendMax = vends.last ?? 0
        let inflightMax = maxInFlight
        let line = String(
            format: "pf-present decoded=%d ok=%d fail=%d empty=%d gated=%d noDrawable=%d "
                + "dropped=%d qDrop=%d qDry=%d maxRenderMs=%.1f inflightMax=%d forced=%d "
                + "glassDeltaMs p50=%.2f max=%.2f n=%d latchMs p50=%.2f max=%.2f "
                + "vendLeadMs p50=%.2f max=%.2f",
            decoded, ok, failed, empty, gated, noDrawable, dropped,
            smoothing.overflowDrops, smoothing.underflows, maxRenderMs, inflightMax,
            gate?.drainForced() ?? 0, p50, dMax, deltas.count, latchP50, latchMax,
            vendP50, vendMax)
        ok = 0; failed = 0; empty = 0; dropped = 0; gated = 0; noDrawable = 0
        maxRenderMs = 0
        maxInFlight = inFlight // the window peak restarts from the live depth
        glassDeltasMs.removeAll(keepingCapacity: true)
        latchMs.removeAll(keepingCapacity: true)
        vendLeadMs.removeAll(keepingCapacity: true)
        lock.unlock()
        // Console.app first (the on-device readout — see presentLog); stdout only under the env
        // lever (the CLI client's capture channel).
        presentLog.info("\(line, privacy: .public)")
        if presentDebug {
            print(line)
            fflush(stdout) // stdout is a pipe when captured — flush per line or nothing shows
        }
    }
}

/// Bridges the VideoToolbox decode-completion callback to the core Automatic-bitrate controller's
/// decode signal. Created as a pipeline property so the decoder's `onDecoded` callback (built in
/// `init`, before the connection exists) can capture it, then `start` binds the live connection +
/// the arming flag once known — the same "reference captured in init, configured in start" shape as
/// `recovery`/`gate`. `record` runs on VideoToolbox's callback thread; `bind` runs once on the main
/// thread before the pump feeds the first AU, so the plain fields are safe (set-once, then read).
private final class DecodeReport: @unchecked Sendable {
    private weak var connection: PunktfunkConnection?
    private var enabled = false
    func bind(_ connection: PunktfunkConnection) {
        self.connection = connection
        self.enabled = connection.wantsDecodeLatency()
    }
    /// Report received→decoded for one frame, in µs. Both stamps are client `CLOCK_REALTIME`
    /// (no skew). Skips when the controller isn't armed, so it's free to call on every decode.
    func record(receivedNs: Int64, decodedNs: Int64) {
        guard enabled, let c = connection else { return }
        let us = (decodedNs - receivedNs) / 1000
        if us > 0 { c.reportDecodeUs(UInt32(min(us, Int64(UInt32.max)))) }
    }
}

public final class Stage2Pipeline {
    private let ring: FrameStore<ReadyFrame>
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    /// Present cadence — `.arrival` (stage-2), `.glass` (stage-3, the present gate) or
    /// `.deadline` (stage-4, the CAMetalDisplayLink engine). Fixed for the pipeline's lifetime;
    /// SessionPresenter resolves it per session (see PresentPacing).
    private let pacing: PresentPacing
    /// The glass gate's in-flight present budget (`PresentGate` capacity) — meaningful only under
    /// `.glass`; SessionPresenter resolves it per platform (see `SessionPresenter.gateDepth`).
    private let gateDepth: Int
    /// macOS smoothness: pace presents onto the vsync grid (`present(at:)` via the VsyncClock),
    /// at most one per vsync, so the FIFO store drains on the display's cadence rather than on
    /// arrival. Ignored under `.deadline` (the link IS the cadence there).
    private let vsyncPaced: Bool
    private let endToEndMeter: LatencyMeter?
    private let decodeMeter: LatencyMeter?
    private let displayMeter: LatencyMeter?
    /// The measured OS present floor (deadline pacing only): each link update's vend→glass lead
    /// is recorded here, and its p50 is what SessionModel subtracts from the shown display/e2e
    /// numbers — the pipeline-depth cost no client controls (design/apple-presentation-rebuild.md).
    private let presentFloorMeter: LatencyMeter?
    private let recovery = KeyframeRecovery()
    /// Feeds the core Automatic-bitrate controller's decode signal from the decode callback; `start`
    /// binds the live connection + arming flag (see DecodeReport).
    private let decodeReport = DecodeReport()
    /// Post-loss freeze-until-reanchor gate (shared core policy via the C ABI). Created here seeded 0;
    /// `start` reseeds it to the live connection's drop count. Captured by the decoder callbacks
    /// (which withhold concealed frames) and driven by the pump (arm on a gap, poll per iteration).
    private let gate = ReanchorGate(framesDropped: 0)
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
    /// Deadline pacing's staged CAMetalDisplayLink frame-rate hint (see `FrameRateHint`).
    /// Created unconditionally (cheap); only the deadline link thread drains it.
    private let frameRateHint = FrameRateHint()

    /// The Metal layer the hosting view installs + sizes.
    public var layer: CAMetalLayer { presenter.layer }

    /// Unified-stats meters (design/stats-unification.md): `endToEndMeter` records the headline
    /// end-to-end (capture→on-glass, skew-corrected); `decodeMeter` the decode stage
    /// (received→decoded); `displayMeter` the display stage (decoded→on-glass, the ring wait +
    /// render + vsync — the tail stage-2 exists to shorten). All optional: metering never gates
    /// the presenter choice. Returns nil if Metal can't be set up (headless / no GPU) — caller
    /// falls back to the stage-1 presenter. `pacing` selects the stage-2 (arrival) vs stage-3
    /// (glass-gated) present cadence — see PresentPacing; `gateDepth` is the glass gate's
    /// in-flight present budget (see `SessionPresenter.gateDepth`).
    public init?(
        endToEndMeter: LatencyMeter?,
        decodeMeter: LatencyMeter? = nil,
        displayMeter: LatencyMeter? = nil,
        presentFloorMeter: LatencyMeter? = nil,
        pacing: PresentPacing = .arrival,
        gateDepth: Int = 1,
        storePolicy: FrameStorePolicy = .newestWins,
        vsyncPaced: Bool = false
    ) {
        guard let presenter = MetalVideoPresenter.make() else { return nil }
        self.presenter = presenter
        self.pacing = pacing
        self.gateDepth = gateDepth
        self.vsyncPaced = vsyncPaced
        self.ring = FrameStore(policy: storePolicy)
        self.endToEndMeter = endToEndMeter
        self.decodeMeter = decodeMeter
        self.displayMeter = displayMeter
        self.presentFloorMeter = presentFloorMeter
        let ring = ring
        let recovery = recovery
        let renderSignal = renderSignal
        let gate = gate
        let decodeReport = decodeReport
        self.decoder = VideoDecoder(
            onDecoded: { frame in
                // Decode stage = received→decoded, both client CLOCK_REALTIME (offset 0 — no
                // skew applies). Stamped at decode completion, so it covers every decoded frame,
                // including ones the re-anchor gate withholds or the newest-wins ring drops.
                decodeMeter?.record(
                    ptsNs: UInt64(frame.receivedNs), atNs: frame.decodedNs, offsetNs: 0)
                // Same interval, reported to the core bitrate controller so Automatic caps at this
                // device's real decode limit instead of the network link ceiling. Every decoded
                // frame (not just presented ones), so a newest-wins drop can't hide the backlog.
                decodeReport.record(receivedNs: frame.receivedNs, decodedNs: frame.decodedNs)
                // Freeze-until-reanchor: WITHHOLD a decoder-concealed post-loss frame (the gray/
                // garbage VideoToolbox returns Ok for a reference-missing delta) — don't submit it,
                // so the CAMetalLayer keeps its last good drawable on glass. The gate lifts (returns
                // present) on a proven clean re-anchor (IDR / RFI anchor / 2nd recovery mark) or the
                // bounded backstop. decoderKeyframe=false: VT doesn't flag IDRs, the wire FLAG_SOF does.
                guard gate.onDecoded(flags: frame.flags) else { return }
                ring.submit(frame)
                // FRAME ARRIVAL is the render trigger (never the display link — see the header).
                renderSignal.signal()
            },
            // Async decode failure (a bad P-frame referencing a lost/corrupt IDR): fold it into the
            // gate's no-output streak (which arms the freeze after a short run, matching the desktop),
            // and when that trips ask the host for a fresh IDR now (infinite GOP — it wouldn't
            // otherwise come soon). Throttled in KeyframeRecovery.
            onDecodeError: { _ in if gate.onNoOutput() { recovery.request() } })
    }

    /// Start pulling AUs into the decoder. MAIN THREAD. `onFrame` fires per AU at receipt (the
    /// host+network / capture→received meter, exactly as stage-1); `onSessionEnd` on close.
    /// `clockOffsetNs` (host minus client) makes the end-to-end stamp cross-machine valid.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil
    ) {
        offsetNs = connection.clockOffsetNs
        recovery.bind(connection) // arm host-keyframe recovery for this session
        decodeReport.bind(connection) // arm the Automatic-bitrate decode signal for this session
        gate.reseed(framesDropped: connection.framesDropped()) // baseline the freeze to this session
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
        let reanchorGate = gate
        // PyroWave rides a different decode half: no CMFormatDescription/VideoToolbox machinery
        // (a wavelet AU has no parameter sets), no keyframe recovery or re-anchor freeze (the
        // stream is all-intra and Phase 4's partial delivery WANTS lossy frames on glass as
        // localized blur, not a freeze). The ready ring, render thread, pacing and meters are
        // shared unchanged.
        let thread: Thread
        if connection.videoCodec == .pyrowave {
            thread = Self.makePyroWavePump(
                connection: connection, token: token, pumpStopped: pumpStopped,
                ring: ring, renderSignal: renderSignal,
                device: presenter.metalDevice, queue: presenter.metalQueue,
                decodeMeter: decodeMeter,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize)
        } else {
            thread = Thread {
            defer { pumpStopped.signal() } // let stop() join the pump (bounded) before decoder.reset()
            var format: CMVideoFormatDescription?
            // Report coded dims to the resize overlay only on a CHANGE (new-mode IDR), not per
            // loss-recovery IDR at the same size (see StreamPump).
            var lastDecodedDims: CMVideoDimensions?
            var lastFramesDropped = connection.framesDropped()
            // Persistent recovery WANT, not a one-shot edge (see StreamPump for the full rationale):
            // keep asking until an IDR lands so a request swallowed by the throttle is re-sent.
            var awaitingIDR = false
            // 4:4:4 backstop: a run of decode/create failures in a 4:4:4 session means this device can't
            // decode 4:4:4 at the negotiated resolution (the HW probe clears the common case but not a
            // resolution-ceiling miss). End cleanly instead of looping on a black screen.
            var decodeFailRun = 0
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased VT/CM temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    // Background keep-alive: drain one AU (flow control + host pacing) and discard it
                    // BEFORE any VideoToolbox decode or Metal render — no GPU work off-screen. The
                    // decoder session is left intact; exitBackground requests a fresh IDR and the
                    // re-anchor gate arms on the resumed frame-index gap so concealed frames are
                    // withheld until it lands.
                    if connection.isVideoDropped {
                        _ = try connection.nextAU(timeoutMs: 100)
                        return true
                    }
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
                    // Freeze backstop: a drop-count climb arms the gate (in case the frame-index gap
                    // below was itself lost), and an overdue freeze re-asks for the re-anchor.
                    if reanchorGate.poll(framesDropped: dropped) { recovery.request() }
                    // Drain HDR mastering metadata (0xCE) and hand it to the PRESENTER (→ CAEDRMetadata).
                    // Polled UNCONDITIONALLY (not gated on connection.isHDR, the fixed Welcome flag): the
                    // host sends 0xCE only for HDR, INCLUDING a mid-session SDR→HDR transition (a game
                    // entering HDR — the host re-inits its encoder) the Welcome flag would never reflect.
                    // Non-blocking; nil for an SDR stream.
                    if let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
                        presenter.setHdrMeta(meta)
                    }
                    guard let au = try connection.nextAU(timeoutMs: 100) else { return true }
                    // Loss recovery (RFI): a forward frame-index gap fires a throttled reference-
                    // frame-invalidation request so an RFI-capable host (AMD LTR / NVENC) recovers
                    // with a cheap clean P-frame instead of a full IDR. The framesDropped-driven
                    // recovery above stays the backstop for when the recovery frame itself is lost.
                    // The same gap is the earliest, most precise signal to ARM the display freeze —
                    // the following concealed frames are withheld until a clean re-anchor.
                    if connection.noteFrameIndexGap(au.frameIndex) { reanchorGate.arm() }
                    onFrame?(au)
                    if let f = connection.videoCodec.formatDescription(fromKeyframe: au.data) {
                        format = f          // refreshed on every IDR (mode changes included)
                        let dims = CMVideoFormatDescriptionGetDimensions(f)
                        if lastDecodedDims?.width != dims.width || lastDecodedDims?.height != dims.height {
                            lastDecodedDims = dims
                            onDecodedSize?(Int(dims.width), Int(dims.height))
                        }
                        awaitingIDR = false // a fresh IDR re-anchored decode — recovery complete
                    }
                    guard let f = format, !token.isStopped else { return true }
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
                            return false
                        }
                    }
                    return true
                } catch {
                    if !token.isStopped { onSessionEnd?() }
                    return false // session closed
                }
                }
            }
            }
        }
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        pumpJoinable = true
        thread.start()

        // The present half. Deadline pacing (stage-4) swaps it wholesale: a CAMetalDisplayLink
        // vends the drawables and its per-refresh updates co-drive the render thread — see
        // startDeadlinePresenter. The V-Sync policy below doesn't apply there (the link deadline-
        // times every present). Deadline sessions ALWAYS carry the stats (their pf-present line
        // streams to Console.app via presentLog — the on-device pacing decomposition).
        let debugStats = (presentDebug || pacing == .deadline) ? PresentDebugStats() : nil
        if pacing == .deadline {
            startDeadlinePresenter(debugStats: debugStats)
            return
        }

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
        // `vsyncPaced` (macOS smoothness) FORCES vsync scheduling — the FIFO store must drain
        // on the display cadence, one frame per vsync, or the buffer degenerates to arrival.
        let vsyncPaced = vsyncPaced
        let vsyncEnabled = vsyncPaced || presentMode == "vsync"
            || (presentMode != "immediate"
                && UserDefaults.standard.bool(forKey: DefaultsKey.vsync))
        let vsyncClock = vsyncClock
        // Stage-3's bounded in-flight present gate; nil = stage-2's present-on-arrival. A local
        // (like the ring) so neither the render thread nor the presented handlers capture `self`.
        let gate: PresentGate? = pacing == .glass ? PresentGate(capacity: gateDepth) : nil
        let renderThread = Thread {
            defer { renderStopped.signal() }
            // macOS smoothness: the vsync this thread last presented onto — at most ONE present
            // per vsync so the FIFO drains on the display's cadence. Thread-confined.
            var lastPresentTarget: CFTimeInterval = 0
            // Every iteration drains its own autorelease pool (`return` = the old `continue`):
            // this thread has no runloop, and `nextDrawable()` AUTORELEASES each CAMetalDrawable —
            // without a per-iteration pool every presented frame's drawable object (plus its
            // texture-descriptor/array retinue, ~2 MB/min at 120 fps) piles up until session end.
            while !token.isStopped { autoreleasepool {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // Smoothness pacing: this vsync's present slot already taken — the frame stays
                // in the store, and the next display-link tick re-signals. (Tolerance well under
                // any refresh period; a stale clock ⇒ nil target ⇒ no dedup, present flows.)
                if vsyncPaced, let t = vsyncClock.nextVsync(after: CACurrentMediaTime()),
                   abs(t - lastPresentTarget) < 0.002 {
                    debugStats?.gatedWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // Stage-3: while a present is in flight, don't take from the ring at all — frames
                // keep coalescing there (newest wins, the intended drop point) and the presented
                // handler re-signals the moment the slot frees. Checked BEFORE the take so a gated
                // frame is never bounced through putBack.
                if let gate, !gate.tryAcquire(now: CACurrentMediaTime()) {
                    debugStats?.gatedWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                guard !token.isStopped, let frame = ring.take() else {
                    gate?.release() // armed but nothing to render — don't hold the gate stale
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // V-Sync ON: flip on the next predicted vsync (< one period out, stale link ⇒
                // immediate — see VsyncClock). OFF: flip as soon as the GPU finishes.
                let presentAt = vsyncEnabled
                    ? vsyncClock.nextVsync(after: CACurrentMediaTime()) : nil
                let renderStarted = CACurrentMediaTime()
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                let onGlass: (Int64?) -> Void = { presentedNs in
                    // Stage-3: the flip reached glass (or was dropped) — free the present slot,
                    // then re-signal so the freshest waiting ring frame goes out immediately.
                    if let gate {
                        gate.release()
                        renderSignal.signal()
                    }
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
                    debugStats?.presented(atNs: presentedNs, issuedNs: issuedNs)
                }
                // One present tail, two decode sources: the VideoToolbox biplanar buffer or the
                // PyroWave Metal planes — the ring, pacing and meters are agnostic to which.
                let rendered: Bool
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    rendered = presenter.render(
                        pixelBuffer, isHDR: isHDR, presentAtMediaTime: presentAt,
                        onPresented: onGlass)
                case .planar(let planes):
                    rendered = presenter.renderPlanar(
                        planes, presentAtMediaTime: presentAt, onPresented: onGlass)
                }
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered {
                    gate?.release() // no present registered — its handler will never fire
                    ring.putBack(frame)
                } else if vsyncPaced, let presentAt {
                    lastPresentTarget = presentAt // this vsync's slot is now taken
                }
                debugStats?.flushIfDue(ring: ring, gate: gate)
            } }
        }
        renderThread.name = "punktfunk-stage2-render"
        renderThread.qualityOfService = .userInteractive
        renderJoinable = true
        renderThread.start()
    }

    /// Deadline pacing's present half (stage-4 — see `PresentPacing.deadline`): a
    /// CAMetalDisplayLink on its own runloop thread vends ONE drawable per refresh into the
    /// newest-wins stash, and the render thread pairs it with the newest decoded frame the
    /// moment either half completes the pair — the common case is a decoded frame presenting
    /// instantly into an already-vended drawable, which the system then latches at the upcoming
    /// refresh (`preferredFrameLatency` 1). No image queue can form (one vended drawable in
    /// flight, ever) and nothing serializes on the on-glass callback. An unpresented stashed
    /// drawable is simply replaced by the next update (back to the layer's pool), so the stash
    /// is never stale by more than a refresh while the link runs.
    ///
    /// Threading mirrors the arrival/glass half: neither thread captures `self`; the link is
    /// created, driven and invalidated entirely on its own thread (CAMetalDisplayLink is only
    /// ever touched there — the frame-rate hint crosses via `FrameRateHint`); the link thread's
    /// runloop iterations each drain an autorelease pool (a vended CAMetalDrawable is
    /// autoreleased like a `nextDrawable()` one — see the render loop's identical rule); the
    /// 100 ms runloop horizon is the stop-flag poll, so teardown is bounded without a join.
    private func startDeadlinePresenter(debugStats: PresentDebugStats?) {
        let token = token
        let ring = ring
        let renderSignal = renderSignal
        let renderStopped = renderStopped
        let presenter = presenter
        let endToEndMeter = endToEndMeter
        let displayMeter = displayMeter
        let offsetNs = offsetNs
        let hint = frameRateHint
        let layer = presenter.layer
        let stash = LatestBox<CAMetalDrawable>()

        let floorMeter = presentFloorMeter
        // The link starts LAZILY — the render thread triggers this after the FIRST decoded
        // frame's reconcileLayer. Started eagerly it vends into the layer's initial 0×0
        // drawableSize for the whole connect window: every vend fails allocation and the system
        // logs "[CAMetalLayer nextDrawable] returning nil because allocation failed" once per
        // refresh until the first frame arrives. Before that frame there is nothing to present
        // anyway, and the first frame waits at most one refresh for the first vend.
        let startLink: () -> Void = {
            let linkThread = Thread {
                let delegate = DeadlineLinkDelegate(
                    stash: stash, renderSignal: renderSignal, hint: hint, stats: debugStats,
                    floorMeter: floorMeter)
                let link = CAMetalDisplayLink(metalLayer: layer)
                link.preferredFrameLatency = 1 // wake as late as fits: latch the NEXT refresh
                if let range = hint.drain() { link.preferredFrameRateRange = range }
                link.delegate = delegate // weak — this closure is the strong ref
                link.add(to: RunLoop.current, forMode: .default)
                while !token.isStopped {
                    autoreleasepool {
                        _ = RunLoop.current.run(
                            mode: .default, before: Date(timeIntervalSinceNow: 0.1))
                    }
                }
                link.invalidate()
            }
            linkThread.name = "punktfunk-stage4-link"
            linkThread.qualityOfService = .userInteractive
            linkThread.start()
        }

        let renderThread = Thread {
            defer { renderStopped.signal() }
            // Whether startLink ran — render-thread confined (only this thread triggers it).
            var linkLive = false
            // Per-iteration autorelease pool — same contract as the arrival/glass loop (the
            // vended drawable and its retinue are autoreleased objects on a runloop-less thread).
            while !token.isStopped { autoreleasepool {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                // Present needs the PAIR — frame first. The frame drives the layer reconcile,
                // which must run even when NO drawable is vended yet: the link vends from the
                // layer's CURRENT config, so drawableSize/format have to be right before a vend
                // can succeed at all (see reconcileLayer — the session-start bootstrap, where
                // the layer still has its initial 0×0 size and every vend fails allocation).
                guard !token.isStopped, let frame = ring.take() else {
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    presenter.reconcileLayer(
                        decodedSize: CGSize(
                            width: CVPixelBufferGetWidth(pixelBuffer),
                            height: CVPixelBufferGetHeight(pixelBuffer)),
                        isHDR: isHDR)
                case .planar(let planes):
                    presenter.reconcileLayer(
                        decodedSize: CGSize(width: planes.width, height: planes.height),
                        isHDR: planes.pq)
                }
                // First frame: the layer now has a real config — start vending (see startLink).
                if !linkLive {
                    linkLive = true
                    startLink()
                }
                guard let drawable = stash.take() else {
                    // No vend yet (session start: the reconcile above just unblocked the
                    // allocator, the link's next update delivers; steady state: decode beat the
                    // link's phase). putBack keeps newest-wins — a fresher decode replaces this
                    // frame while it waits, and the update's signal retries the pairing.
                    ring.putBack(frame)
                    debugStats?.noDrawableWake()
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                let renderStarted = CACurrentMediaTime()
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                let onGlass: (Int64?) -> Void = { presentedNs in
                    let atNs = presentedNs
                        ?? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime())
                    endToEndMeter?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: offsetNs)
                    displayMeter?.record(ptsNs: UInt64(frame.decodedNs), atNs: atNs, offsetNs: 0)
                    debugStats?.presented(atNs: presentedNs, issuedNs: issuedNs)
                }
                let rendered: Bool
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    rendered = presenter.render(
                        pixelBuffer, isHDR: isHDR, into: drawable, onPresented: onGlass)
                case .planar(let planes):
                    rendered = presenter.renderPlanar(
                        planes, into: drawable, onPresented: onGlass)
                }
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered {
                    // The vended drawable is spent either way (an unused/mismatched one drops
                    // back to the pool); the frame retries on the link's next vend. A format
                    // mismatch (mid-session HDR flip caught between the layer reconfigure and
                    // the next vend) self-heals the same way — see encodePresent's guard.
                    ring.putBack(frame)
                }
                debugStats?.flushIfDue(ring: ring, gate: nil)
            } }
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
    /// replacement frame. Arrival/glass pacing only — deadline sessions have no CADisplayLink
    /// (their CAMetalDisplayLink's updates are both clock and retry).
    public func renderTick(targetMediaTime: CFTimeInterval, period: CFTimeInterval) {
        vsyncClock.set(target: targetMediaTime, period: period)
        renderSignal.signal()
    }

    /// MAIN thread (SessionPresenter — session start + every layout/Reconfigure): hint the
    /// deadline link with the stream cadence. Staged; the link's own thread applies it (see
    /// `FrameRateHint`). No-op under arrival/glass pacing, where the hosting view's CADisplayLink
    /// is the hinted link.
    public func setFrameRateHint(hz: Float) {
        frameRateHint.stage(hz: hz)
    }

    /// Forward the layout-derived drawable pixel size to the presenter (MAIN thread — see
    /// `MetalVideoPresenter.setDrawableTarget`).
    public func setDrawableTarget(_ size: CGSize) {
        presenter.setDrawableTarget(size)
    }

    #if os(macOS)
    /// The windowed-mode PyroWave present target (see `MetalVideoPresenter.surfaceLayer` — the
    /// DCP swapID-panic mitigation). The hosting view installs it as a sibling above `layer`.
    public var surfaceLayer: CALayer { presenter.surfaceLayer }

    /// Forward the windowed-vs-fullscreen present routing (MAIN thread — see
    /// `MetalVideoPresenter.setSurfacePresents`).
    public func setSurfacePresents(_ on: Bool) {
        presenter.setSurfacePresents(on)
    }
    #endif

    /// Forward the display's current EDR headroom to the presenter (MAIN thread — a `UIScreen`
    /// read). tvOS flips HDR presentation between PQ passthrough and the in-shader tone-map on
    /// it; see `MetalVideoPresenter.setDisplayHeadroom`.
    public func setDisplayHeadroom(_ headroom: CGFloat) {
        presenter.setDisplayHeadroom(headroom)
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

    /// The PyroWave pump: AUs go straight into the Metal wavelet decoder (no VideoToolbox, no
    /// format descriptions), decoded planes ride the same ready ring / render thread. All-intra
    /// stream, so none of the VT pump's recovery machinery applies: keyframe/RFI requests are
    /// silenced host-side for this codec, and a lossy (partial-delivery) frame is MEANT to
    /// present as localized blur — never a freeze. Static + capture-by-parameter for the same
    /// reason the VT pump avoids capturing `self` (a missed stop must not leak a live pipeline).
    private static func makePyroWavePump(
        connection: PunktfunkConnection, token: StopFlag, pumpStopped: DispatchSemaphore,
        ring: FrameStore<ReadyFrame>, renderSignal: DispatchSemaphore,
        device: MTLDevice, queue: MTLCommandQueue,
        decodeMeter: LatencyMeter?,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)?
    ) -> Thread {
        // The chunk-aligned parse window = the session's negotiated shard payload (Welcome);
        // the 64-byte floor mirrors the Rust client's guard against a nonsense value.
        let windowSize = max(64, Int(connection.shardPayload))
        return Thread {
            defer { pumpStopped.signal() }
            // Compiles the two compute kernels on the session's first frames' thread — ~tens of
            // ms, once per session. Failure = this device can't run the negotiated codec (the
            // advertisement probe should have prevented this); end the session cleanly.
            guard let decoder = MetalWaveletDecoder(device: device, queue: queue) else {
                if !token.isStopped { onSessionEnd?() }
                return
            }
            // Newest decoded frame index — a late partial (the reassembler's 30 ms fuse can
            // deliver one behind a newer complete frame) must not travel back in time.
            var newestIndex: UInt32?
            var lastDims: (w: Int, h: Int)?
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                    do {
                        // Background keep-alive: drain + discard before the Metal wavelet decode
                        // (PyroWave is all-intra, so the resumed frame heals on its own — no IDR
                        // request needed, just no GPU work off-screen).
                        if connection.isVideoDropped {
                            _ = try connection.nextAU(timeoutMs: 100)
                            return true
                        }
                        guard let au = try connection.nextAU(timeoutMs: 100) else { return true }
                        onFrame?(au)
                        if let newest = newestIndex,
                           Int32(bitPattern: au.frameIndex &- newest) <= 0 {
                            return true // stale (or duplicate) frame — skip
                        }
                        guard !token.isStopped else { return true }
                        let chunkAligned =
                            au.flags & PunktfunkConnection.userFlagChunkAligned != 0
                        let ptsNs = au.ptsNs
                        let receivedNs = au.receivedNs
                        let flags = au.flags
                        let submitted = decoder.decode(
                            au: au.data, chunkAligned: chunkAligned, windowSize: windowSize
                        ) { planes in
                            // Metal completed-handler thread — stamp + enqueue, don't block
                            // (the exact contract of the VT output callback).
                            guard let planes else { return }
                            var ts = timespec()
                            clock_gettime(CLOCK_REALTIME, &ts)
                            let decodedNs =
                                Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
                            decodeMeter?.record(
                                ptsNs: UInt64(receivedNs), atNs: decodedNs, offsetNs: 0)
                            ring.submit(
                                ReadyFrame(
                                    ptsNs: ptsNs, receivedNs: receivedNs, decodedNs: decodedNs,
                                    image: .planar(planes), flags: flags))
                            renderSignal.signal()
                        }
                        if submitted {
                            newestIndex = au.frameIndex
                            // Decoded-size changes come from the SOF dims (this is also how a
                            // mid-stream Reconfigure lands here) — report like the VT pump.
                            if let size = decoder.decodedSize,
                               lastDims?.w != size.width || lastDims?.h != size.height {
                                lastDims = (size.width, size.height)
                                onDecodedSize?(size.width, size.height)
                            }
                        }
                        // A dropped AU (malformed / SOF lost / too few blocks) is just skipped:
                        // every PyroWave frame is independently decodable, the next one heals.
                        return true
                    } catch {
                        if !token.isStopped { onSessionEnd?() }
                        return false // session closed
                    }
                }
            }
        }
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
