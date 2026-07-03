// Splits the unified stats model's `host+network` stage (capture→received) into its `host`
// (capture→fully-sent, reported per AU by the host on the 0xCF plane) and `network`
// (the remainder) terms — design/stats-unification.md Phase 2.
//
// Receipt samples are recorded per frame from the pump path; host timings are matched to them
// by exact pts (the 0xCF datagram carries the AU's own `pts_ns`). Best-effort by construction:
// a lost 0xCF datagram, an FEC-dropped AU, or an old host that never emits the plane simply
// contributes no split sample — the HUD then keeps the combined `host+network` line. NSLock
// rather than an actor — the receipt writer is the non-async pump path (same pattern as
// LatencyMeter/FrameMeter).

import Foundation

/// Per-frame `host` / `network` sampler: `recordReceipt` at AU receipt (pts + the combined
/// capture→received interval), `noteHostTiming` per drained 0xCF report, `drain` the window's
/// p50s once a second. The pending ring is bounded (drop-oldest) so an old host — receipts
/// forever, timings never — costs a fixed ~4 KB, not growth.
public final class HostNetworkSplitter: @unchecked Sendable {
    private let lock = NSLock()
    /// Received AUs awaiting their 0xCF host timing: (pts, combined capture→received µs).
    private var pending: [(ptsNs: UInt64, combinedUs: Int64)] = []
    private var hostUsSamples: [Int64] = []
    private var networkUsSamples: [Int64] = []
    /// ~1 s of frames at 240 fps; beyond it the oldest receipt can no longer expect a match.
    private static let pendingCap = 256

    public init() {}

    /// Record one frame at receipt. `ptsNs` is the host capture clock (the AU's pts),
    /// `receivedNs` the client `CLOCK_REALTIME` receipt instant (`AccessUnit.receivedNs`),
    /// `offsetNs` the connect-time host−client clock offset (0 = uncorrected). Same
    /// absurd-value clamp as LatencyMeter — a sample it would drop must not linger here.
    public func recordReceipt(ptsNs: UInt64, receivedNs: Int64, offsetNs: Int64) {
        let combinedNs = receivedNs &+ offsetNs &- Int64(bitPattern: ptsNs)
        guard combinedNs > 0, combinedNs < 10_000_000_000 else { return }
        lock.lock()
        pending.append((ptsNs: ptsNs, combinedUs: combinedNs / 1000))
        if pending.count > Self.pendingCap {
            pending.removeFirst(pending.count - Self.pendingCap)
        }
        lock.unlock()
    }

    /// Match one host timing (0xCF) to its receipt: `host` = the reported capture→sent,
    /// `network` = the combined interval minus it, floored at 0 (the terms tile per frame; a
    /// slightly-off skew offset must not produce a negative wire time). Unmatched timings —
    /// the AU was FEC-dropped, or its receipt raced this drain — are simply skipped.
    public func noteHostTiming(ptsNs: UInt64, hostUs: UInt32) {
        lock.lock()
        defer { lock.unlock() }
        guard let i = pending.firstIndex(where: { $0.ptsNs == ptsNs }) else { return }
        let combinedUs = pending.remove(at: i).combinedUs
        hostUsSamples.append(Int64(hostUs))
        networkUsSamples.append(max(0, combinedUs - Int64(hostUs)))
    }

    public struct Split: Sendable {
        public let hostP50Ms: Double
        public let networkP50Ms: Double
        public let count: Int
    }

    /// The window's p50s since the last drain, then reset (matched samples only; the pending
    /// ring survives — a receipt may still match a timing drained next tick). `nil` when no
    /// timing matched in the interval — the caller falls back to the combined stage.
    public func drain() -> Split? {
        lock.lock()
        let host = hostUsSamples.sorted()
        let network = networkUsSamples.sorted()
        hostUsSamples.removeAll(keepingCapacity: true)
        networkUsSamples.removeAll(keepingCapacity: true)
        lock.unlock()
        guard !host.isEmpty else { return nil }
        func p50(_ sorted: [Int64]) -> Double {
            Double(sorted[min(sorted.count / 2, sorted.count - 1)]) / 1000.0 // µs → ms
        }
        return Split(hostP50Ms: p50(host), networkP50Ms: p50(network), count: host.count)
    }

    /// Forget everything (pending receipts + window) — a fresh connection starts clean.
    public func reset() {
        lock.lock()
        pending.removeAll()
        hostUsSamples.removeAll()
        networkUsSamples.removeAll()
        lock.unlock()
    }
}
