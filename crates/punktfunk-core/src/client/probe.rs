//! Speed-test probe state (`ProbeState`, pump-mirrored) and the public `ProbeOutcome`.

/// In-flight / finished speed test. The data-plane pump mirrors probe-scoped
/// receive counters; the control task freezes the delivered figure and folds
/// in the host [`ProbeResult`]. Read by [`NativeClient::probe_result`].
///
/// Count delivered *probe* packets (reassembler FLAG_PROBE counters), not
/// reassembled AUs (loss above FEC completes none and the count cliffs to
/// zero) and not all datagrams (in-flight video at burst head/tail inflates
/// the numerator).
#[derive(Default)]
pub(crate) struct ProbeState {
    /// Set by `request_probe`; cleared when the host [`ProbeResult`] lands.
    /// A re-probe overwrites the whole state.
    pub(crate) active: bool,
    /// Probe-scoped `Stats::probe_*` at burst start (first pump tick while
    /// active) and latest; mirrored every pump iteration.
    pub(crate) base_packets: Option<u64>,
    pub(crate) base_bytes: Option<u64>,
    pub(crate) rx_packets_now: u64,
    pub(crate) rx_bytes_now: u64,
    /// First / last probe-packet arrival (monotonic ns, 0 = none). Their
    /// difference is the throughput denominator — see [`measured_interval_ms`].
    pub(crate) first_arrival_ns: u64,
    pub(crate) last_arrival_ns: u64,
    /// Wire packets / plaintext bytes (header + shard), frozen when the host
    /// report lands so resumed video cannot inflate them.
    pub(crate) delivered_packets: u64,
    pub(crate) delivered_bytes: u64,
    /// Client receive interval (ms), frozen with the delivered figures.
    /// 0 = fewer than two probe packets; consumers use
    /// [`throughput_window_ms`](Self::throughput_window_ms).
    pub(crate) client_interval_ms: u32,
    /// Host end-of-burst report.
    pub(crate) host_goodput_bytes: u64,
    pub(crate) host_au: u32,
    pub(crate) host_wire_packets: u32,
    pub(crate) host_send_dropped: u32,
    /// Host send-window duration — fallback denominator, see
    /// [`throughput_window_ms`](Self::throughput_window_ms).
    pub(crate) host_duration_ms: u32,
    pub(crate) done: bool,
    /// Requested burst length. The pump arms a watchdog from this: an ignored
    /// `ProbeRequest` would latch `active` and suppress the whole report tick
    /// (loss, ABR, standing-latency, clock re-sync) for the rest of the session.
    pub(crate) duration_ms: u32,
}

impl ProbeState {
    /// Client receive interval of a finished burst, ms: first → last probe
    /// arrival, floored at 1 (a sub-ms span would divide as infinite).
    /// `None` when fewer than two packets arrived or stamps are unset /
    /// identical / reversed — the caller falls back to the host duration.
    ///
    /// The host `duration_ms` is the SEND window: it closes while the
    /// bottleneck queue still drains, so client bytes / host window overstates
    /// the link. [`crate::abr::Driver::set_ceiling`]
    /// never lowers, so a high reading sticks for the session.
    pub(crate) fn measured_interval_ms(first_ns: u64, last_ns: u64, packets: u64) -> Option<u32> {
        if packets < 2 || first_ns == 0 || last_ns <= first_ns {
            return None;
        }
        let ms = ((last_ns - first_ns) / 1_000_000).max(1);
        Some(u32::try_from(ms).unwrap_or(u32::MAX))
    }

    /// Throughput denominator, ms: client receive interval when the burst
    /// produced one, else the host send-window duration. While bursting the
    /// interval is live, first to latest arrival over `delivered_packets`, so
    /// a partial read carries a window; the host report freezes it.
    pub(crate) fn throughput_window_ms(&self, delivered_packets: u64) -> u32 {
        let client_ms = if self.done {
            self.client_interval_ms
        } else {
            Self::measured_interval_ms(
                self.first_arrival_ns,
                self.last_arrival_ns,
                delivered_packets,
            )
            .unwrap_or(0)
        };
        if client_ms > 0 {
            client_ms
        } else {
            self.host_duration_ms
        }
    }
}

/// Finished or partial speed-test, from [`NativeClient::probe_result`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeOutcome {
    /// Host end-of-burst report has arrived; the numbers below are final.
    pub done: bool,
    /// Delivered wire bytes (header + shard) / packets during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// Throughput denominator, ms: client first→last arrival, live while
    /// bursting and final once `done`; host send-window when fewer than two
    /// probe packets arrived. Host duration alone overstates: its window
    /// closes while the bottleneck still drains toward the client.
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kbps).
    /// Drive [`Hello::bitrate_kbps`] from this; leave headroom for FEC + loss.
    pub throughput_kbps: u32,
    /// Link loss = `(wire_packets_sent − received) / wire_packets_sent`, percent.
    pub loss_pct: f32,
    /// Host-side drop = `send_dropped / (wire_packets_sent + send_dropped)`,
    /// percent. Distinct from `loss_pct`: send buffer, not the link.
    pub host_drop_pct: f32,
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_needs_two_packets_and_a_nonzero_span() {
        // <2 packets: no interval; caller falls back to host duration.
        assert_eq!(ProbeState::measured_interval_ms(0, 0, 0), None);
        assert_eq!(
            ProbeState::measured_interval_ms(5_000_000, 5_000_000, 1),
            None
        );
        // Same ns, reversed, or unset first stamp: same fallback.
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
        // 0.4 ms truncates to 0 ms; the floor keeps the division finite.
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
        // No client interval (<2 packets) → host send window.
        let p = ProbeState {
            host_duration_ms: 800,
            done: true,
            ..Default::default()
        };
        assert_eq!(p.throughput_window_ms(1), 800);
        let p = ProbeState {
            client_interval_ms: 1_010,
            host_duration_ms: 800,
            done: true,
            ..Default::default()
        };
        assert_eq!(p.throughput_window_ms(1_000), 1_010);
    }

    #[test]
    fn throughput_window_is_live_while_bursting() {
        // Before the host report: first → latest arrival, so a partial read has a window.
        let p = ProbeState {
            first_arrival_ns: 1_000_000,
            last_arrival_ns: 201_000_000,
            ..Default::default()
        };
        assert_eq!(p.throughput_window_ms(40), 200);
        assert_eq!(p.throughput_window_ms(1), 0); // one packet: no interval yet
    }
}
