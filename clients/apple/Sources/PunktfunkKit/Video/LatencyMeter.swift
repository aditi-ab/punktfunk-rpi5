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
/// `pts_ns` is the host's capture wall clock (the AU's pts) and the connect-time **clock-skew
/// offset** (`PunktfunkConnection.clockOffsetNs`, host minus client) makes the difference valid
/// across machines. `offsetNs == 0` means an old host that didn't answer the skew handshake (or
/// genuinely synced clocks) — the number is then only meaningful same-host, and the HUD tags the
/// end-to-end line `(same-host clock)`.
public final class LatencyMeter: @unchecked Sendable {
    private let lock = NSLock()
    private var samplesUs: [Int64] = []
    private var skewCorrected = false

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
        // start stamp is missing/after its end) — samples are clamped to (0, 10 s).
        guard latNs > 0, latNs < 10_000_000_000 else { return }
        lock.lock()
        samplesUs.append(latNs / 1000)
        if offsetNs != 0 { skewCorrected = true }
        lock.unlock()
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
