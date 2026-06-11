//! Live counters for the frame-pacing / quality logic and the web UI.

use std::sync::atomic::{AtomicU64, Ordering};

/// Immutable snapshot, copied across the C ABI as `PunktfunkStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    /// Packets the host could NOT hand to the kernel because the send buffer was full (WouldBlock)
    /// — the dominant loss mode at very high bitrate. Distinct from `packets_dropped` (recv-side
    /// reassembler rejects). A non-zero, growing value means the link/encoder is outrunning the
    /// send path; raise `net.core.wmem_max` / lower the bitrate, or wait for paced batched sending.
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Atomic accumulators owned by a [`Session`](crate::session::Session). Snapshot to
/// [`Stats`] for readers. `Relaxed` ordering is fine: these are monotonic counters
/// read for display, never used to synchronize other memory.
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
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
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
            bytes_sent: self.bytes_sent.load(l),
            bytes_received: self.bytes_received.load(l),
        }
    }
}
