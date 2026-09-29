//! Live counters for the frame-pacing / quality logic and the web UI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic ns since an arbitrary process-wide epoch. Probe arrival stamps are
/// only ever differenced on this machine; a wall-clock step mid-burst would
/// corrupt the interval, so `pts_ns`'s CLOCK_REALTIME basis is wrong here.
/// Floored at 1 so a stamp never collides with the 0 = "unset" sentinel.
pub(crate) fn now_monotonic_ns() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64).max(1)
}

/// Immutable snapshot, copied across the C ABI as `PunktfunkStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    /// WouldBlock on send: the kernel send buffer was full. Distinct from
    /// `packets_dropped` (recv-side reassembler rejects).
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    /// Recovered shards that later arrived: reordering reconstructed the block
    /// from parity while the originals were still in flight. Loss estimators
    /// must net this out (`recovered - late`, see [`window_loss_ppm`](crate::quic::window_loss_ppm))
    /// or reordering reads as loss. Not mirrored into the C-ABI `PunktfunkStats`.
    pub fec_late_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// DATA-shard payload delivered to the video reassembler — no headers, FEC
    /// parity, probe filler, or audio. `bytes_received` includes FEC redundancy,
    /// so a utilization gate built from it inflates on lossy links. Not mirrored
    /// into the C-ABI `PunktfunkStats`.
    pub media_bytes_received: u64,
    /// Wire packets / plaintext bytes carrying [`FLAG_PROBE`](crate::packet::FLAG_PROBE).
    /// `bytes_received` counts every accepted datagram, so a speed-test numerator
    /// built from it inherits in-flight video. Not mirrored into the C-ABI
    /// `PunktfunkStats` (probe measurements surface via `ProbeOutcome`).
    pub probe_packets_received: u64,
    pub probe_bytes_received: u64,
    /// First / last probe-packet arrival (monotonic ns, see [`now_monotonic_ns`];
    /// 0 = none since the last arm). Difference is the client receive interval:
    /// the host send window closes while the path is still draining, so host
    /// duration overstates the link. Zeroed on arm (`Session::reset_probe_arrivals`).
    pub probe_first_arrival_ns: u64,
    pub probe_last_arrival_ns: u64,
}

/// Atomic accumulators owned by a [`Session`](crate::session::Session). Snapshot
/// to [`Stats`] for readers. `Relaxed` is enough: these never synchronize other
/// memory. Probe arrival stamps are slots, not counters, and are read well after
/// the last write.
#[derive(Default)]
pub struct StatsCounters {
    pub frames_submitted: AtomicU64,
    pub frames_completed: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub packets_sent: AtomicU64,
    pub packets_received: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub packets_send_dropped: AtomicU64,
    pub fec_recovered_shards: AtomicU64,
    pub fec_late_shards: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub media_bytes_received: AtomicU64,
    pub probe_packets_received: AtomicU64,
    pub probe_bytes_received: AtomicU64,
    pub probe_first_arrival_ns: AtomicU64,
    pub probe_last_arrival_ns: AtomicU64,
}

impl StatsCounters {
    #[inline]
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Stats {
        let l = Ordering::Relaxed;
        Stats {
            frames_submitted: self.frames_submitted.load(l),
            frames_completed: self.frames_completed.load(l),
            frames_dropped: self.frames_dropped.load(l),
            packets_sent: self.packets_sent.load(l),
            packets_received: self.packets_received.load(l),
            packets_dropped: self.packets_dropped.load(l),
            packets_send_dropped: self.packets_send_dropped.load(l),
            fec_recovered_shards: self.fec_recovered_shards.load(l),
            fec_late_shards: self.fec_late_shards.load(l),
            bytes_sent: self.bytes_sent.load(l),
            bytes_received: self.bytes_received.load(l),
            media_bytes_received: self.media_bytes_received.load(l),
            probe_packets_received: self.probe_packets_received.load(l),
            probe_bytes_received: self.probe_bytes_received.load(l),
            probe_first_arrival_ns: self.probe_first_arrival_ns.load(l),
            probe_last_arrival_ns: self.probe_last_arrival_ns.load(l),
        }
    }
}
