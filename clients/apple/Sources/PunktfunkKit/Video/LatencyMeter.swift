// Per-frame latency-stage sampler for the live HUD: records one interval per frame (an end
// instant minus a start instant, both CLOCK_REALTIME ns) and drains percentiles on demand.
// NSLock rather than an actor — the writers are the non-async pump/decode/present paths (same
// pattern as the app's FrameMeter).

import Foundation

/// Samples one **latency stage** per frame and reports percentiles. One instance per stage of the
/// unified stats model (design/stats-unification.md):
///
/// - `host+network` = capture→received: `record(ptsNs:offsetNs:)` at AU receipt.
/// - `decode` = received→decoded and `display` = decoded→displayed: client-local single-clock
///   stages — `record(ptsNs:atNs:offsetNs:)` with the start instant as `ptsNs` and `offsetNs: 0`.
/// - `end-to-end` = capture→displayed, measured directly (never summed from the stages):
///   `record(ptsNs:atNs:offsetNs:)` at present.
///
/// For the host-anchored intervals (capture→…) the sample is `end + offset - pts_ns`, where
/// `pts_ns` is the host's capture wall clock (the AU's pts) and the LIVE **clock-skew
/// offset** (`PunktfunkConnection.clockOffsetNs`, host minus client, mid-stream re-synced —
/// read it per record, never cached) makes the difference valid
/// across machines. `offsetNs == 0` means an old host that didn't answer the skew handshake (or
/// genuinely synced clocks) — the number is then only meaningful same-host, and the HUD tags the
/// end-to-end line `(same-host clock)`.
public final class LatencyMeter: @unchecked Sendable {
    private let lock = NSLock()
    private var samplesUs: [Int64] = []
    private var skewCorrected = false
    /// Samples `record` refused as impossible since the last `drainTrimmed` (see the guard).
    private var trimmed = 0
    /// The most recent sample and the instant it ended, for `latestSample(asOfNs:maxAgeMs:)` —
    /// a LEVEL, not a window, so `drain` deliberately leaves both alone.
    private var latestNs: Int64 = 0
    private var latestAtNs: Int64 = 0

    public init() {}

    /// Record one frame at receipt (now). `ptsNs` is the host capture clock (the AU's pts);
    /// `offsetNs` is the host-client clock offset from the skew handshake (0 = uncorrected).
    public func record(ptsNs: UInt64, offsetNs: Int64) {
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let nowNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
        record(ptsNs: ptsNs, atNs: nowNs, offsetNs: offsetNs)
    }

    /// Record one frame whose sample is `atNs + offsetNs - ptsNs` — an EXPLICIT end instant
    /// rather than now. `ptsNs` is the stage's start point: the AU pts for the host-anchored
    /// intervals, or a client stamp (receivedNs / decodedNs, with `offsetNs: 0`) for the local
    /// decode/display stages. The stage-2 presenter stamps its present-side samples at the
    /// display link's target present time (not the moment the present call ran). All in
    /// `CLOCK_REALTIME`.
    public func record(ptsNs: UInt64, atNs: Int64, offsetNs: Int64) {
        let latNs = atNs &+ offsetNs &- Int64(bitPattern: ptsNs)
        // Drop absurd values (a clock step, a wildly wrong offset, garbage pts, or a stage whose
        // start stamp is missing/after its end) — samples are clamped to (0, 10 s). COUNTED, not
        // silent: a cluster of non-positive samples is the signature of a wrong clock offset
        // (client-local stages can't go negative), and a meter that quietly trims the impossible
        // half of a shifted distribution presents the surviving tail as a plausible small number
        // — field 2026-08-13: "e2e 0–3 ms p50 / 23 ms p95" on a session whose true hostnet was
        // ~18 ms. `drainTrimmed` surfaces the count so the window can be MARKED suspect instead
        // of looking healthy.
        guard latNs > 0, latNs < 10_000_000_000 else {
            lock.lock()
            trimmed += 1
            lock.unlock()
            return
        }
        lock.lock()
        samplesUs.append(latNs / 1000)
        latestNs = latNs
        latestAtNs = atNs
        if offsetNs != 0 { skewCorrected = true }
        lock.unlock()
    }

    /// The most recent single sample in ns, or `nil` if none has landed or the last one ended more
    /// than `maxAgeMs` before `nowNs` (both `CLOCK_REALTIME`). Unlike `drain`, this reports a level
    /// rather than a window, and reading it consumes nothing.
    ///
    /// **What it is for.** Read off the END-TO-END meter, this is the video plane's live
    /// glass-to-glass figure — `displayed + clockOffset − pts`, exactly the shape `AvSync` compares
    /// audio against — and it is the reference the A/V sync loop needs. It is published from
    /// `record`, so BOTH present paths (arrival and deadline) feed it without either knowing that
    /// audio exists.
    ///
    /// **Why staleness is not optional.** The number is a level, so absent an age check it would
    /// simply keep its last value forever. This client has a state where that matters: the
    /// backgrounded keep-alive keeps audio playing and DROPS video decode entirely, so the loop
    /// would go on steering the ring against a reference minutes old and frozen. Expiring it
    /// returns `nil`, which is the same "no reference yet" case as session start — the loop holds
    /// its last correction and stops chasing. `nowNs` is caller-supplied rather than read fresh so
    /// the audio side compares against exactly the instant it timestamped its own frame at.
    ///
    /// Only the PAST is bounded. A present stamp can legitimately sit a hair ahead of the reader's
    /// clock (the deadline presenter stamps at the link's target present time), and discarding the
    /// only reference we have over a fraction of a refresh would make it flap in and out; a stamp
    /// wildly in the future instead yields a huge offset, which `AvSync` refuses on its own terms.
    public func latestSample(asOfNs nowNs: Int64, maxAgeMs: Int) -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        guard latestNs > 0 else { return nil }
        guard (nowNs &- latestAtNs) <= Int64(maxAgeMs) * 1_000_000 else { return nil }
        return latestNs
    }

    public struct Stats: Sendable {
        public let p50Ms: Double
        public let p95Ms: Double
        public let p99Ms: Double
        public let count: Int
        /// True if the skew offset was applied (a host that answered the handshake) — i.e. the
        /// numbers are cross-machine valid, not just same-host.
        public let skewCorrected: Bool
    }

    /// Take-and-reset the count of impossible samples `record` refused (see its guard). Drained
    /// SEPARATELY from `drain()` on purpose: with a badly wrong offset EVERY sample of a window
    /// can be non-positive, `drain()` then returns `nil` — and a count folded into `Stats` would
    /// vanish with it, hiding the very windows that scream loudest. This survives an empty window.
    public func drainTrimmed() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = trimmed
        trimmed = 0
        return n
    }

    /// Percentiles over the samples accumulated since the last drain, then reset the window. `nil`
    /// when no samples arrived in the interval.
    public func drain() -> Stats? {
        lock.lock()
        let sorted = samplesUs.sorted()
        let corrected = skewCorrected
        samplesUs.removeAll(keepingCapacity: true)
        skewCorrected = false
        lock.unlock()
        guard !sorted.isEmpty else { return nil }
        func pct(_ p: Double) -> Double {
            let i = min(Int(Double(sorted.count) * p), sorted.count - 1)
            return Double(sorted[i]) / 1000.0 // us -> ms
        }
        return Stats(
            p50Ms: pct(0.50), p95Ms: pct(0.95), p99Ms: pct(0.99),
            count: sorted.count, skewCorrected: corrected)
    }
}
