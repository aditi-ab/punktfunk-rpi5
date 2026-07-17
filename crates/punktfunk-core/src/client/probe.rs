//! Speed-test probe state (`ProbeState`, pump-mirrored) and the public `ProbeOutcome`.

/// Accumulated state of an in-flight / finished speed test. The data-plane pump mirrors the
/// session's packet-level receive counters here; the control task finalizes the delivered figure
/// and folds in the host's [`ProbeResult`] when it lands. Read by [`NativeClient::probe_result`].
///
/// Counting at the *packet* level (every delivered wire packet) — not whole reassembled probe AUs —
/// is what makes the measurement degrade gracefully: once loss exceeds the FEC budget no AU
/// completes, so the old AU-based count cliffed to zero even though most bytes still arrived.
#[derive(Default)]
pub(crate) struct ProbeState {
    /// A probe is in progress: set by `request_probe`, cleared when the host's [`ProbeResult`]
    /// lands (a re-probe just overwrites the whole state — the latest one wins).
    pub(crate) active: bool,
    /// `session.stats()` receive counters at the burst's start (snapshotted by the pump on its first
    /// tick while active) and latest, mirrored every pump iteration.
    pub(crate) base_packets: Option<u64>,
    pub(crate) base_bytes: Option<u64>,
    pub(crate) rx_packets_now: u64,
    pub(crate) rx_bytes_now: u64,
    /// Delivered wire packets / plaintext bytes (header + shard), frozen when the host's report lands
    /// (so resumed video after the burst can't inflate them).
    pub(crate) delivered_packets: u64,
    pub(crate) delivered_bytes: u64,
    /// The host's end-of-burst report.
    pub(crate) host_goodput_bytes: u64,
    pub(crate) host_au: u32,
    /// Wire packets the host actually put on the link, and the ones its send buffer dropped.
    pub(crate) host_wire_packets: u32,
    pub(crate) host_send_dropped: u32,
    /// The host's measured burst duration (the throughput denominator).
    pub(crate) host_duration_ms: u32,
    /// The host's `ProbeResult` arrived → the measurement is final.
    pub(crate) done: bool,
}

/// A finished/partial speed-test measurement, returned by [`NativeClient::probe_result`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeOutcome {
    /// The host's end-of-burst report has arrived — the numbers below are final.
    pub done: bool,
    /// Delivered wire bytes (header + shard) / packets the client received during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// The burst duration the host measured, in milliseconds (the throughput denominator).
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kilobits/second). The figure to
    /// drive a [`Hello::bitrate_kbps`] choice from (allow headroom for the FEC overhead + loss).
    pub throughput_kbps: u32,
    /// Link loss = `(wire_packets_sent − received) / wire_packets_sent`, percent. Packets the host
    /// put on the wire that didn't arrive.
    pub loss_pct: f32,
    /// Host-side drop = `send_dropped / (wire_packets_sent + send_dropped)`, percent. Packets the
    /// host's send buffer couldn't accept (raise `net.core.wmem_max` / lower the rate). Distinct
    /// from `loss_pct`: this is the host failing to keep up, not the link dropping traffic.
    pub host_drop_pct: f32,
    /// Wire packets the host put on the link and the ones its send buffer dropped (raw counts).
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
}
