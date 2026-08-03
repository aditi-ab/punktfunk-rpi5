//! Live counters for the frame-pacing / quality logic and the web UI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic now, in ns since an arbitrary process-wide epoch — the basis for the probe
/// arrival stamps below. Monotonic on purpose: the stamps are only ever DIFFERENCED on this
/// machine (the burst's receive interval), and a wall-clock step mid-burst — the exact event
/// the clock re-sync machinery exists for — must not corrupt the one measurement the ABR
/// ceiling is built from, so the CLOCK_REALTIME basis `pts_ns` uses is wrong here. Floored
/// at 1 so a stamp can never collide with the 0 = "unset" sentinel.
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
    /// Packets the host could NOT hand to the kernel because the send buffer was full (WouldBlock)
    /// — the dominant loss mode at very high bitrate. Distinct from `packets_dropped` (recv-side
    /// reassembler rejects). A non-zero, growing value means the link/encoder is outrunning the
    /// send path; raise `net.core.wmem_max` / lower the bitrate, or wait for paced batched sending.
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    /// Shards counted into [`fec_recovered_shards`](Self::fec_recovered_shards) that later ARRIVED
    /// — reordered delivery lets a block reconstruct early from parity, so the still-in-flight
    /// shards it "recovered" were late, not lost. Loss estimators must net this out
    /// (`recovered - late`, see [`window_loss_ppm`](crate::quic::window_loss_ppm)) or plain
    /// reordering reads as packet loss and spooks adaptive FEC + the bitrate controller.
    /// Deliberately NOT mirrored into the C-ABI `PunktfunkStats` (loss windows run in-core).
    pub fec_late_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Probe-scoped receive counters: wire packets / plaintext bytes carrying
    /// [`FLAG_PROBE`](crate::packet::FLAG_PROBE) (speed-test filler), counted at the
    /// reassembler's probe routing decision. `bytes_received` counts EVERY accepted datagram,
    /// so a speed-test numerator built from it inherits whatever video was in flight around
    /// the burst — these keep video out of the probe math. Deliberately NOT mirrored into the
    /// C-ABI `PunktfunkStats` (probe measurements surface via `ProbeOutcome`).
    /// Media bytes delivered to the video reassembler: DATA-shard payload only — no packet
    /// headers, no FEC parity, no probe filler, no audio. This is the rate the encoder's target
    /// is a promise about, and the only honest thing to compare that target against.
    /// `bytes_received` counts every accepted datagram, so a "delivered throughput" built from
    /// it rises with the FEC redundancy the host adds in answer to loss — which meant the
    /// adaptive-bitrate utilization gate ("did the pipeline actually carry ~the target?") read
    /// 25 % high exactly on the lossy links it exists for, and the never-decaying
    /// proven-throughput mark inherited the same inflation. Deliberately NOT mirrored into the
    /// C-ABI `PunktfunkStats`.
    pub media_bytes_received: u64,
    pub probe_packets_received: u64,
    pub probe_bytes_received: u64,
    /// First / last probe-packet arrival (monotonic ns, see [`now_monotonic_ns`]; 0 = none
    /// since the last probe arm). Their difference is the burst's client-side receive
    /// interval — the honest speed-test denominator: the host's send window closes while the
    /// switch/kernel queue toward the client is still draining, so dividing client bytes by
    /// the HOST duration overstates the link (a 1 GbE link "measured" 1266 Mbps). The client
    /// pump zeroes both when it arms a probe (`Session::reset_probe_arrivals`).
    pub probe_first_arrival_ns: u64,
    pub probe_last_arrival_ns: u64,
}

/// Atomic accumulators owned by a [`Session`](crate::session::Session). Snapshot to
/// [`Stats`] for readers. `Relaxed` ordering is fine: these are monotonic counters
/// read for display, never used to synchronize other memory. (The two probe arrival
/// stamps are the exception — slots, not counters — but they carry no synchronization
/// duty either: they are read hundreds of ms after the last write.)
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
