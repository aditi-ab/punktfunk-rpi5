//! Speed-test probe state (`ProbeState`, pump-mirrored) and the public `ProbeOutcome`.

/// Accumulated state of an in-flight / finished speed test. The data-plane pump mirrors the
/// session's probe-scoped receive counters here; the control task finalizes the delivered figure
/// and folds in the host's [`ProbeResult`] when it lands. Read by [`NativeClient::probe_result`].
///
/// Counting at the *packet* level (every delivered wire packet) — not whole reassembled probe AUs —
/// is what makes the measurement degrade gracefully: once loss exceeds the FEC budget no AU
/// completes, so the old AU-based count cliffed to zero even though most bytes still arrived.
/// Counting *probe* packets only (the reassembler stamps dedicated counters at its FLAG_PROBE
/// routing) keeps video out of the numerator: the burst pauses video, but frames already in
/// flight land during its head, and resumed video lands between the last probe packet and the
/// host's report — both used to inflate the all-datagram byte delta this mirrored before.
#[derive(Default)]
pub(crate) struct ProbeState {
    /// A probe is in progress: set by `request_probe`, cleared when the host's [`ProbeResult`]
    /// lands (a re-probe just overwrites the whole state — the latest one wins).
    pub(crate) active: bool,
    /// Probe-scoped receive counters (`Stats::probe_*`) at the burst's start (snapshotted by the
    /// pump on its first tick while active) and latest, mirrored every pump iteration.
    pub(crate) base_packets: Option<u64>,
    pub(crate) base_bytes: Option<u64>,
    pub(crate) rx_packets_now: u64,
    pub(crate) rx_bytes_now: u64,
    /// First / last probe-packet arrival stamps (monotonic ns, 0 = none yet), mirrored from the
    /// probe-scoped session counters. Their difference is the interval the delivered bytes
    /// actually arrived in — the honest throughput denominator (see
    /// [`measured_interval_ms`](Self::measured_interval_ms)).
    pub(crate) first_arrival_ns: u64,
    pub(crate) last_arrival_ns: u64,
    /// Delivered wire packets / plaintext bytes (header + shard), frozen when the host's report lands
    /// (so resumed video after the burst can't inflate them).
    pub(crate) delivered_packets: u64,
    pub(crate) delivered_bytes: u64,
    /// The client-measured receive interval (ms), frozen alongside the delivered figures; 0 = no
    /// usable interval (the burst delivered fewer than two probe packets) — consumers fall back
    /// to [`host_duration_ms`](Self::host_duration_ms) via
    /// [`throughput_window_ms`](Self::throughput_window_ms).
    pub(crate) client_interval_ms: u32,
    /// The host's end-of-burst report.
    pub(crate) host_goodput_bytes: u64,
    pub(crate) host_au: u32,
    /// Wire packets the host actually put on the link, and the ones its send buffer dropped.
    pub(crate) host_wire_packets: u32,
    pub(crate) host_send_dropped: u32,
    /// The host's measured burst duration (the throughput denominator's FALLBACK — see
    /// [`throughput_window_ms`](Self::throughput_window_ms)).
    pub(crate) host_duration_ms: u32,
    /// The host's `ProbeResult` arrived → the measurement is final.
    pub(crate) done: bool,
    /// The requested burst length, so the pump can arm a watchdog for a host that never answers.
    /// Without one, an ignored `ProbeRequest` latches `active` forever and the pump's whole report
    /// tick — loss reports, the ABR window feed, the standing-latency ladder and pending clock
    /// re-syncs — stays suppressed for the rest of the session.
    pub(crate) duration_ms: u32,
}

impl ProbeState {
    /// The client-measured receive interval of a finished burst, in ms: first → last
    /// probe-packet arrival, floored at 1 (a sub-ms burst divided by 0 ms would read as
    /// infinite throughput). `None` — the caller falls back to the host's duration — when
    /// fewer than two probe packets arrived or the stamps are degenerate (unset / identical /
    /// reversed): a single arrival spans no interval.
    ///
    /// Why not the host's `duration_ms`: it measures the SEND window, which closes while the
    /// bottleneck (switch/kernel) queue is still draining toward the client — the tail of the
    /// bytes lands *after* it. Dividing client-side bytes by the host-side window therefore
    /// overstates the link: a 1 GbE link under a 2 Gbps burst target "measured" 1266 Mbps and
    /// handed the ABR an 886 Mbps ceiling it could never deliver — and
    /// [`set_ceiling`](crate::abr::BitrateController::set_ceiling) never lowers, so the lie
    /// was permanent for the session.
    pub(crate) fn measured_interval_ms(first_ns: u64, last_ns: u64, packets: u64) -> Option<u32> {
        if packets < 2 || first_ns == 0 || last_ns <= first_ns {
            return None;
        }
        let ms = ((last_ns - first_ns) / 1_000_000).max(1);
        Some(u32::try_from(ms).unwrap_or(u32::MAX))
    }

    /// The throughput denominator, in ms: the client-measured receive interval when the burst
    /// produced one, else the host's send-window duration (an old measurement is better than
    /// none — and strictly conservative territory only when packets were too few to matter).
    pub(crate) fn throughput_window_ms(&self) -> u32 {
        if self.client_interval_ms > 0 {
            self.client_interval_ms
        } else {
            self.host_duration_ms
        }
    }
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
    /// The throughput denominator, in milliseconds: the client-measured receive interval
    /// (first → last probe-packet arrival) once `done`; the host's measured send-window
    /// duration when the burst delivered fewer than two probe packets (no interval to measure
    /// from). The host duration alone overstates throughput — its window closes while the
    /// bottleneck queue is still draining toward the client.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_needs_two_packets_and_a_nonzero_span() {
        // <2 packets: no interval exists — the caller must fall back to the host duration.
        assert_eq!(ProbeState::measured_interval_ms(0, 0, 0), None);
        assert_eq!(
            ProbeState::measured_interval_ms(5_000_000, 5_000_000, 1),
            None
        );
        // Two packets in the same ns / a reversed pair / an unset first stamp: same fallback.
        assert_eq!(
            ProbeState::measured_interval_ms(5_000_000, 5_000_000, 2),
            None
        );
        assert_eq!(
            ProbeState::measured_interval_ms(9_000_000, 5_000_000, 2),
            None
        );
        assert_eq!(ProbeState::measured_interval_ms(0, 5_000_000, 2), None);
    }

    #[test]
    fn interval_is_floored_at_one_ms() {
        // Two packets 0.4 ms apart truncate to 0 ms — the floor keeps the division honest
        // instead of infinite.
        assert_eq!(ProbeState::measured_interval_ms(1_000, 401_000, 2), Some(1));
    }

    #[test]
    fn interval_measures_first_to_last_arrival() {
        assert_eq!(
            ProbeState::measured_interval_ms(1_000_000, 801_000_000, 1_000),
            Some(800)
        );
    }

    #[test]
    fn throughput_window_falls_back_to_the_host_duration() {
        // No client interval frozen (a <2-packet burst) → the host's send window is the
        // denominator, exactly the old behavior.
        let p = ProbeState {
            host_duration_ms: 800,
            ..Default::default()
        };
        assert_eq!(p.throughput_window_ms(), 800);
        // With an interval, the client measurement wins — the 1 GbE field case: the same
        // bytes over 1010 ms instead of the host's 800 ms is the difference between an
        // honest ~940 Mbps and an impossible 1266 Mbps.
        let p = ProbeState {
            client_interval_ms: 1_010,
            host_duration_ms: 800,
            ..Default::default()
        };
        assert_eq!(p.throughput_window_ms(), 1_010);
    }
}
