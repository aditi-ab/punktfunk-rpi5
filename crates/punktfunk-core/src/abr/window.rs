//! Assembling one report window from what the session did.
//!
//! Counter anchors (loss, lost frames, wire bytes) are differenced against the
//! snapshot the last window closed on, so a window is a delta, not a level.
//! The latency signals arrive as sums and counts because that is what the
//! embedders gather. A window whose numbers describe something other than the
//! link — the tail of a capacity burst, a host pipeline rebuild — is
//! discarded whole: one bogus congestion verdict ends slow start for good.

use super::sample::{WindowActivity, WindowSample, WINDOW};
use crate::stats::Stats;
use std::time::Instant;

/// A closed report window: the controller's input, plus what the embedder
/// still owes the host for it.
pub(crate) struct Closed {
    pub sample: WindowSample,
    /// The window describes a burst tail or a rebuild, not the link. No
    /// report goes out and the controller never sees it.
    pub discarded: bool,
    /// Session total for the [`crate::quic::DeliveryReport`] this window owes,
    /// if any.
    pub delivery: Option<u64>,
}

/// What the pump measures between report ticks.
pub(crate) struct WindowAccumulator {
    /// Audio's wire reservation, spent whether video flows or not. Added so
    /// the delivered rate lives in the same budget as the target.
    audio_reserved_kbps: u32,
    /// Host marks idle-keepalive repeats. Older hosts are
    /// [`WindowActivity::Unmarked`].
    marks_repeats: bool,
    last_report: Instant,
    /// Counter anchors: this window is the session totals minus these.
    recovered: u64,
    late: u64,
    received: u64,
    dropped: u64,
    bytes: u64,
    /// Latest session snapshot. The pump samples once per iteration and every
    /// window number is differenced from it.
    stats: Stats,
    owd_sum_ns: i128,
    owd_frames: u32,
    au_frames: u32,
    au_repeats: u32,
    decode_sum_us: u64,
    decode_count: u32,
    encode_sum_us: u64,
    encode_count: u32,
    recovery_kf: u32,
    flushed: bool,
    discard: bool,
    /// [`crate::quic::DeliveryReport`] cadence: every window while nothing has
    /// arrived, once when the first packets land, then silence.
    delivery_confirmed: bool,
}

impl WindowAccumulator {
    pub(crate) fn new(audio_reserved_kbps: u32, marks_repeats: bool, now: Instant) -> Self {
        WindowAccumulator {
            audio_reserved_kbps,
            marks_repeats,
            last_report: now,
            recovered: 0,
            late: 0,
            received: 0,
            dropped: 0,
            bytes: 0,
            stats: Stats::default(),
            owd_sum_ns: 0,
            owd_frames: 0,
            au_frames: 0,
            au_repeats: 0,
            decode_sum_us: 0,
            decode_count: 0,
            encode_sum_us: 0,
            encode_count: 0,
            recovery_kf: 0,
            flushed: false,
            discard: false,
            delivery_confirmed: false,
        }
    }

    pub(crate) fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The session counters as of now. Every window number is a delta of two
    /// of these, so the pump feeds one per iteration — a total-loss drought
    /// completes no frame but still moves them.
    pub(crate) fn on_stats(&mut self, st: &Stats) {
        self.stats = *st;
    }

    /// One completed access unit. `repeat` is the host's idle keepalive mark:
    /// an arrived AU that carries no new content.
    pub(crate) fn on_au(&mut self, repeat: bool) {
        self.au_frames = self.au_frames.saturating_add(1);
        if repeat {
            self.au_repeats = self.au_repeats.saturating_add(1);
        }
    }

    /// Capture → received for one AU. Rising delay under zero loss is queue
    /// growth, the signal that arrives before the loss does.
    pub(crate) fn on_owd(&mut self, ns: i128) {
        self.owd_sum_ns += ns;
        self.owd_frames += 1;
    }

    /// The window's client decode-stage accumulator, however the embedder
    /// gathered it. Absent (`count` 0) means nobody reports it.
    pub(crate) fn on_decode_latency(&mut self, sum_us: u64, count: u32) {
        self.decode_sum_us += sum_us;
        self.decode_count += count;
    }

    /// Host encode-stage timings from the `0xCF` stage report.
    pub(crate) fn on_encode_latency(&mut self, sum_us: u64, count: u32) {
        self.encode_sum_us += sum_us;
        self.encode_count += count;
    }

    /// Decode-recovery keyframe asks that went out this window.
    pub(crate) fn on_keyframe_asks(&mut self, n: u32) {
        self.recovery_kf = self.recovery_kf.saturating_add(n);
    }

    /// A jump-to-live: the client could not hold the rate. Severe.
    pub(crate) fn on_flush(&mut self) {
        self.flushed = true;
    }

    /// This window describes something other than the link.
    pub(crate) fn discard(&mut self) {
        self.discard = true;
    }

    /// How long this window has been open, ms — for the log that says why it
    /// is being thrown away.
    pub(crate) fn open_ms(&self) -> u64 {
        self.last_report.elapsed().as_millis() as u64
    }

    /// Past the report cadence, and no burst is in flight to distort it.
    pub(crate) fn due(&self, now: Instant, probing: bool) -> bool {
        !probing && now.duration_since(self.last_report) >= WINDOW
    }

    /// Every anchor forward to now, dropping what the window held.
    ///
    /// The capacity burst lands in the packet and byte counters but never in
    /// the decoder, so without this the first window after it reads as a
    /// throughput that never happened.
    pub(crate) fn rebase(&mut self, now: Instant) {
        let st = self.stats;
        self.recovered = st.fec_recovered_shards;
        self.late = st.fec_late_shards;
        self.received = st.packets_received;
        self.dropped = st.frames_dropped;
        self.bytes = wire_bytes(&st);
        self.last_report = now;
        self.discard = true;
        self.flushed = false;
    }

    /// The byte anchor alone. Video that landed under a suppressed report tick
    /// would otherwise be one window's worth of a much longer span.
    pub(crate) fn rebase_bytes(&mut self) {
        self.bytes = wire_bytes(&self.stats);
    }

    /// Close the window: the sample the controller judges, and what the host
    /// is owed for it.
    pub(crate) fn close(&mut self, now: Instant) -> Closed {
        let st = self.stats;
        let discarded = std::mem::take(&mut self.discard);
        let dropped = st.frames_dropped.wrapping_sub(self.dropped);
        let loss_ppm = crate::quic::window_loss_ppm(
            st.fec_recovered_shards.wrapping_sub(self.recovered),
            st.fec_late_shards.wrapping_sub(self.late),
            st.packets_received.wrapping_sub(self.received),
        );
        // Wire throughput vs target: headers, seals and FEC parity included
        // (they spend the budget), minus probe filler, plus the audio
        // reservation.
        let window_ms = now.duration_since(self.last_report).as_millis().max(1) as u64;
        let actual_kbps = ((wire_bytes(&st).wrapping_sub(self.bytes).saturating_mul(8) / window_ms)
            as u32)
            .saturating_add(self.audio_reserved_kbps);
        let mean = |sum: u64, count: u32| (count > 0).then(|| (sum / u64::from(count)) as i64);
        let sample = WindowSample {
            now,
            dropped,
            loss_ppm,
            owd_mean_us: (self.owd_frames > 0)
                .then(|| (self.owd_sum_ns / i128::from(self.owd_frames) / 1_000) as i64),
            decode_mean_us: mean(self.decode_sum_us, self.decode_count),
            encode_mean_us: mean(self.encode_sum_us, self.encode_count),
            actual_kbps,
            flushed: self.flushed,
            recovery_kf: self.recovery_kf,
            activity: activity(self.marks_repeats, self.au_frames, self.au_repeats),
        };
        // A discarded window stays silent, so it also owes no delivery count.
        let delivery =
            (!discarded && self.owes_delivery(st.packets_received)).then_some(st.packets_received);
        self.reset(now);
        Closed {
            sample,
            discarded,
            delivery,
        }
    }

    /// Whether this window owes the host a [`crate::quic::DeliveryReport`].
    ///
    /// Every window while `packets_received` is 0 (the host escalates on
    /// that), then once when the first packets land, then silence. Older
    /// hosts log every unknown control message.
    fn owes_delivery(&mut self, packets_received: u64) -> bool {
        let owed = packets_received == 0 || !self.delivery_confirmed;
        self.delivery_confirmed = packets_received > 0;
        owed
    }

    /// Anchors forward, accumulators empty. A discarded window's counts must
    /// not leak into the next one.
    fn reset(&mut self, now: Instant) {
        let st = self.stats;
        self.recovered = st.fec_recovered_shards;
        self.late = st.fec_late_shards;
        self.received = st.packets_received;
        self.dropped = st.frames_dropped;
        self.bytes = wire_bytes(&st);
        self.last_report = now;
        self.owd_sum_ns = 0;
        self.owd_frames = 0;
        self.au_frames = 0;
        self.au_repeats = 0;
        self.decode_sum_us = 0;
        self.decode_count = 0;
        self.encode_sum_us = 0;
        self.encode_count = 0;
        self.recovery_kf = 0;
        self.flushed = false;
    }
}

/// This window's new-content evidence.
///
/// No arrivals are [`WindowActivity::Empty`]: quiet like idle, not an
/// older-host unmarked window. Repeat-only (every arrived AU a host-marked
/// repeat) is [`WindowActivity::Active`]`(0)` and counts toward re-arm.
/// Arrivals on a host that does not mark repeats are
/// [`WindowActivity::Unmarked`]. Stillness is never inferred from a blackout.
fn activity(marks_repeats: bool, frames: u32, repeats: u32) -> WindowActivity {
    if frames == 0 {
        WindowActivity::Empty
    } else if marks_repeats {
        WindowActivity::Active(frames.saturating_sub(repeats))
    } else {
        WindowActivity::Unmarked
    }
}

/// Wire measure: every received media-plane byte (headers, seals and FEC
/// parity spend the budget) minus speed-test filler.
fn wire_bytes(st: &Stats) -> u64 {
    st.bytes_received.wrapping_sub(st.probe_bytes_received)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DeliveryReport: "zero" while true, one confirmation when video
    /// starts, then silence (older hosts warn per unknown message).
    #[test]
    fn the_delivery_count_is_reported_while_zero_then_once_more_and_never_again() {
        let mut w = WindowAccumulator::new(0, true, Instant::now());
        for _ in 0..5 {
            assert!(
                w.owes_delivery(0),
                "a dead data plane must be re-reported every window"
            );
        }
        assert!(w.owes_delivery(500));
        for n in [900, 1_200, 90_000] {
            assert!(
                !w.owes_delivery(n),
                "a healthy session must not stream delivery reports"
            );
        }
    }

    /// A session that never receives must never look confirmed.
    #[test]
    fn a_session_that_receives_nothing_never_reports_itself_healthy() {
        let mut w = WindowAccumulator::new(0, true, Instant::now());
        for _ in 0..100 {
            assert!(w.owes_delivery(0));
            assert!(!w.delivery_confirmed);
        }
    }

    #[test]
    fn only_observed_repeats_make_an_idle_abr_window() {
        assert_eq!(activity(true, 45, 45), WindowActivity::Active(0));
        assert_eq!(activity(true, 45, 40), WindowActivity::Active(5));
        assert_eq!(activity(true, 0, 0), WindowActivity::Empty);
        assert_eq!(activity(false, 45, 0), WindowActivity::Unmarked);
        assert_eq!(activity(false, 0, 0), WindowActivity::Empty);
    }
}
