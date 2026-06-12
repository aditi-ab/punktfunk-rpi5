// Per-frame latency sampler for the live HUD: records capture->client-receipt latency and drains
// percentiles on demand. NSLock rather than an actor — the writer is the non-async pump/arrival
// path (same pattern as the app's FrameMeter).

import Foundation

/// Samples the **capture->client-receipt** latency of each access unit and reports percentiles.
///
/// The latency is `now - pts_ns`, where `pts_ns` is the host's capture wall clock (the AU's pts) and
/// `now` is the client's `CLOCK_REALTIME` instant the AU was received, shifted by the connect-time
/// **clock-skew offset** (`PunktfunkConnection.clockOffsetNs`, host minus client) so the difference
/// is valid across machines. `offsetNs == 0` means an old host that didn't answer the skew handshake
/// (or genuinely synced clocks) — the number is then only meaningful same-host.
///
/// SCOPE (stage-1 presenter): this covers host capture -> encode -> FEC -> network -> reassembly ->
/// decrypt -> handed to the presenter. It does **not** include the on-device VideoToolbox decode or
/// the `AVSampleBufferDisplayLayer` present — that layer decodes and presents compressed samples
/// internally with no per-frame callback. True decode->present (the full glass-to-glass) needs the
/// stage-2 presenter (`VTDecompressionSession` decode-completion + `CAMetalLayer`/display-link
/// present); this meter is the substrate it will extend.
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

    /// Record one frame whose latency is `atNs + offsetNs - ptsNs` — an EXPLICIT client instant
    /// rather than now. The stage-2 presenter uses this to stamp capture→present at the display
    /// link's target present time (not the moment the present call ran). All in `CLOCK_REALTIME`.
    public func record(ptsNs: UInt64, atNs: Int64, offsetNs: Int64) {
        let latNs = atNs &+ offsetNs &- Int64(bitPattern: ptsNs)
        // Drop absurd values (a clock step, a wildly wrong offset, or garbage pts).
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
