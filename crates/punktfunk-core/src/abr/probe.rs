//! The startup capacity probe: one burst, once, to learn what the link holds.
//!
//! Two seconds after video actually flows the client asks the host for an
//! 800 ms burst beside the stream and reads the ceiling off what arrived. The
//! burst damages the window it lands in, so that window is discarded; if it
//! took the keyframe with it the session asks for a new one. Every deadline
//! here exists because a host may simply not answer: an unanswered burst that
//! latched `active` would suppress the report tick for the rest of the
//! session.

use std::time::{Duration, Instant};

/// Burst length. Long enough to fill the bottleneck queue and drain it,
/// short enough that the picture survives it on most links.
const PROBE_MS: u32 = 800;
/// Wait after video flows before bursting. The first frames are the encoder's
/// IDR and the decoder's bring-up; a burst on top of them measures neither.
const PROBE_DELAY: Duration = Duration::from_secs(2);
/// Queue and QUIC loss recovery sit between the host's "complete" and our
/// receipt. A result later than this is not about the burst.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What the burst delivered, as the pump's probe state froze it.
#[derive(Clone, Copy, Debug)]
pub struct ProbeReport {
    /// Wire bytes (header plus shard) the burst delivered. `0` = declined.
    pub delivered_bytes: u64,
    /// Throughput denominator: the client receive interval when the burst
    /// produced one, else the host's send window.
    pub window_ms: u32,
    /// Host send-window duration. `0` = the host declined the burst.
    pub host_duration_ms: u32,
    /// The measured client interval, for the log. `0` = none.
    pub client_interval_ms: u32,
}

/// What a burst's report was worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measured {
    /// Not the burst this client asked for — an embedder speed test, or a
    /// report already consumed. Nothing is learned, nothing is rebased.
    NotOurs,
    /// The host declined the burst: the negotiated ceiling stands.
    Declined,
    /// Link capacity, headroom already taken off.
    Ceiling(u32),
}

/// The startup burst's whole life: armed, in flight, answered or abandoned.
pub(crate) struct CapacityProbe {
    /// Burst target. `PUNKTFUNK_ABR_PROBE_KBPS`, or twice the stream cap.
    target_kbps: u32,
    /// When to fire. `None` = fired already, or never armed.
    fire_at: Option<Instant>,
    /// Our own burst is out and its result is still owed.
    result_by: Option<Instant>,
    /// Any burst is in flight, ours or an embedder speed test.
    active: bool,
    /// An in-flight burst that outlives this is unanswered: let it go, or the
    /// report tick stays suppressed forever.
    watchdog: Option<Instant>,
    /// `frames_completed` when the burst started: "did any frame survive",
    /// not "has one ever arrived".
    frames_at_start: u64,
}

impl CapacityProbe {
    /// `target_kbps` of `None` sizes the burst from the stream cap. `armed` is
    /// `PUNKTFUNK_ABR_PROBE` plus the session being Automatic at all.
    pub(crate) fn new(
        armed: bool,
        target_kbps: Option<u32>,
        stream_cap_kbps: u32,
        now: Instant,
    ) -> Self {
        CapacityProbe {
            target_kbps: target_kbps.unwrap_or_else(|| probe_target_kbps(stream_cap_kbps)),
            fire_at: armed.then(|| now + PROBE_DELAY),
            result_by: None,
            active: false,
            watchdog: None,
            frames_at_start: 0,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// A burst went in or out of flight. `Some(frames_at_start)` on the
    /// trailing edge: the caller compares it against the frames completed to
    /// see whether the burst took every picture with it.
    pub(crate) fn on_active(
        &mut self,
        active: bool,
        duration_ms: u32,
        frames_completed: u64,
        now: Instant,
    ) -> Option<u64> {
        let ended = self.active && !active;
        if !self.active && active {
            let burst = Duration::from_millis(u64::from(duration_ms));
            self.watchdog = Some(now + burst + PROBE_TIMEOUT);
            self.frames_at_start = frames_completed;
        }
        if !active {
            self.watchdog = None;
        }
        self.active = active;
        ended.then_some(self.frames_at_start)
    }

    /// Fire when it is due and video is actually flowing; otherwise wait
    /// another delay. A slow host bring-up is still emitting its first IDR.
    pub(crate) fn poll(&mut self, now: Instant, frames_completed: u64) -> Option<(u32, u32)> {
        let due = self.fire_at.is_some_and(|at| now >= at);
        if !due {
            return None;
        }
        if self.active || frames_completed == 0 {
            self.fire_at = Some(now + PROBE_DELAY);
            return None;
        }
        self.fire_at = None;
        self.result_by = Some(now + PROBE_TIMEOUT);
        tracing::info!(
            target_kbps = self.target_kbps,
            duration_ms = PROBE_MS,
            "adaptive bitrate: startup link-capacity probe"
        );
        Some((self.target_kbps, PROBE_MS))
    }

    /// The request never reached the control task: nothing is in flight, so
    /// nothing is owed.
    pub(crate) fn on_dropped(&mut self) {
        self.result_by = None;
    }

    /// A burst nobody answered. `true` when the embedder's probe state has to
    /// be released so reports resume.
    pub(crate) fn expired(&mut self, now: Instant) -> bool {
        if self.watchdog.is_some_and(|at| now >= at) {
            self.watchdog = None;
            self.active = false;
            tracing::warn!(
                "speed-test probe unanswered — clearing it so loss reports and ABR resume"
            );
            return true;
        }
        if self.result_by.is_some_and(|at| now >= at) {
            self.result_by = None;
            tracing::info!(
                "adaptive bitrate: capacity probe timed out (old host?) — keeping negotiated ceiling"
            );
            return true;
        }
        false
    }

    /// The host's end-of-burst report. Answered once: the embedder mirrors a
    /// finished probe's state for as long as it stands, so the same report
    /// arrives again on the next iteration.
    pub(crate) fn on_result(&mut self, r: ProbeReport) -> Measured {
        if self.result_by.take().is_none() {
            return Measured::NotOurs;
        }
        if r.host_duration_ms == 0 || r.delivered_bytes == 0 {
            tracing::info!(
                "adaptive bitrate: capacity probe declined — keeping negotiated ceiling"
            );
            return Measured::Declined;
        }
        // Over the CLIENT receive interval: the host send window closes while
        // the bottleneck queue is still draining, so its duration overstates.
        let delivered_kbps =
            (r.delivered_bytes.saturating_mul(8) / u64::from(r.window_ms.max(1))) as u32;
        let ceiling = delivered_kbps.saturating_mul(7) / 10;
        tracing::info!(
            delivered_kbps,
            ceiling_kbps = ceiling,
            client_interval_ms = r.client_interval_ms,
            host_duration_ms = r.host_duration_ms,
            "adaptive bitrate: link-capacity probe done — climb ceiling set"
        );
        Measured::Ceiling(ceiling)
    }
}

/// Capacity-probe burst target in kbps. `set_ceiling` clamps to the stream
/// cap, so bits above `cap / 0.7` are discarded; ×2 clears the 1.43× bar with
/// margin.
///
/// `u32::MAX` (a mode [`stream_ceiling_kbps`](super::stream_ceiling_kbps)
/// declines to size) keeps 2 Gbps — also the hard ceiling; this can only
/// lower the target.
pub(crate) fn probe_target_kbps(stream_cap_kbps: u32) -> u32 {
    stream_cap_kbps.saturating_mul(2).min(2_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{CHROMA_IDC_420, CODEC_H264, CODEC_HEVC};

    fn report(delivered_bytes: u64, window_ms: u32) -> ProbeReport {
        ProbeReport {
            delivered_bytes,
            window_ms,
            host_duration_ms: 800,
            client_interval_ms: window_ms,
        }
    }

    /// The embedder's probe state keeps saying "done" until the next burst
    /// overwrites it, so the same report arrives on every iteration. Reading
    /// it twice would re-base the byte anchor forever, and the session would
    /// never see a window it could climb on.
    #[test]
    fn a_finished_burst_is_measured_exactly_once() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(true, Some(400_000), 100_000, now);
        assert_eq!(p.poll(now + PROBE_DELAY, 1), Some((400_000, PROBE_MS)));
        // 1 MB over 800 ms is 10 Mbps; the ceiling keeps 70 % of it.
        assert_eq!(
            p.on_result(report(1_000_000, 800)),
            Measured::Ceiling(7_000)
        );
        assert_eq!(
            p.on_result(report(1_000_000, 800)),
            Measured::NotOurs,
            "the same report must not be read twice"
        );
    }

    /// An embedder speed test finishes too, and its numbers are not the
    /// controller's to learn from.
    #[test]
    fn a_probe_nobody_asked_for_teaches_nothing() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(false, None, 100_000, now);
        assert_eq!(p.on_result(report(9_000_000, 800)), Measured::NotOurs);
    }

    /// Burst must prove the stream cap and no more. Above `cap / 0.7`
    /// is discarded by `set_ceiling` (see
    /// `abr::tests::the_stream_bound_clamps_a_learned_ceiling_only`).
    #[test]
    fn the_probe_target_proves_the_stream_cap_without_overshooting_it() {
        for (w, h, hz, codec, depth) in [
            (1280, 720, 60, CODEC_HEVC, 8),
            (1920, 1080, 60, CODEC_H264, 8),
            (2560, 1440, 120, CODEC_HEVC, 8),
            (3840, 2160, 120, CODEC_HEVC, 10),
        ] {
            let cap = super::super::stream_ceiling_kbps(w, h, hz, codec, depth, CHROMA_IDC_420);
            let target = probe_target_kbps(cap);
            assert!(
                target.saturating_mul(7) / 10 >= cap,
                "{w}x{h}@{hz}: a {target} kbps burst cannot prove a {cap} kbps cap"
            );
            assert!(
                target <= cap.saturating_mul(2),
                "{w}x{h}@{hz}: {target} kbps chases capacity the clamp discards"
            );
        }
        assert_eq!(probe_target_kbps(u32::MAX), 2_000_000);
        assert_eq!(probe_target_kbps(1_500_000), 2_000_000);
    }
}
