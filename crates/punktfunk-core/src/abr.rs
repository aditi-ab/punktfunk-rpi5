//! Adaptive bitrate: the client-side AIMD controller behind the "Automatic" bitrate setting.
//!
//! Runs inside [`crate::client`]'s data-plane pump on the same 750 ms cadence as the adaptive-FEC
//! [`crate::quic::LossReport`], deciding when to ask the host for a different encoder bitrate via
//! [`crate::quic::SetBitrate`]. Division of labour with adaptive FEC: **FEC answers fast, random
//! loss** (Wi-Fi bursts, RF noise — recoverable redundancy is the right tool); **bitrate answers
//! persistent congestion** (the link simply can't carry the rate — more FEC only adds load). The
//! controller therefore reacts to *sustained* signals only:
//!
//! - **unrecoverable frames** — loss exceeded the FEC budget (the stream visibly froze/recovered);
//! - **heavy loss** — a window whose shard loss is beyond what FEC should be left to absorb alone;
//! - **one-way-delay rise** — capture→received latency (host-clock skew corrected) climbing above
//!   its rolling baseline: standing queue growth, the *pre-loss* signature of a saturated link
//!   (bufferbloat) — this is the early-warning signal loss-based control lacks;
//! - **a jump-to-live flush** — the pump discarded its backlog, the strongest "we were behind"
//!   evidence there is.
//!
//! AIMD shape: two consecutive bad windows ⇒ multiplicative decrease (×0.7, floored); ~10 s of
//! clean windows ⇒ additive-ish increase (+~6 %, ceilinged at the session's starting rate — the
//! controller recovers *back to* what was negotiated, never beyond it). Changes are rate-limited
//! (each one costs the IDR the host's rebuilt encoder opens with) and the whole controller
//! disables itself against a host that never answers [`crate::quic::BitrateChanged`] (an older
//! build that ignores unknown control messages).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Never ask for less than this — below it the stream is unusable anyway and the floor keeps a
/// mis-measured window from cratering the session.
const FLOOR_KBPS: u32 = 5_000;
/// Consecutive bad windows before a decrease — one window can be a scheduler blip or a single
/// Wi-Fi scan; two in a row (1.5 s) is a condition.
const BAD_WINDOWS_TO_DECREASE: u32 = 2;
/// Consecutive clean windows before probing back up (~10 s at the 750 ms cadence): recovery is
/// deliberately much slower than backoff, classic AIMD.
const CLEAN_WINDOWS_TO_INCREASE: u32 = 13;
/// Minimum gap between requested changes — every accepted change costs an encoder rebuild + IDR
/// on the host, and back-to-back steps would outrun the ack/effect round trip.
const CHANGE_COOLDOWN: Duration = Duration::from_secs(3);
/// Window shard loss beyond which the window counts bad even without an unrecoverable frame:
/// 2 % sustained is congestion territory, not the random tail FEC exists for.
const HEAVY_LOSS_PPM: u32 = 20_000;
/// How far the window's mean one-way delay may sit above the rolling baseline before it counts
/// as queue growth. 25 ms is far beyond jitter at any streamable frame rate.
const OWD_RISE_US: i64 = 25_000;
/// Rolling window (in 750 ms report windows, ~30 s) whose minimum mean is the OWD baseline.
/// Long enough to remember the uncongested floor, short enough to follow genuine path changes.
const BASELINE_WINDOWS: usize = 40;
/// Requests sent without a single [`crate::quic::BitrateChanged`] ack before concluding the host
/// predates bitrate renegotiation and going quiet for the rest of the session.
const MAX_UNACKED: u32 = 3;

/// One decision per report window; `Some(kbps)` = send a [`crate::quic::SetBitrate`].
pub(crate) struct BitrateController {
    /// `false` = permanently off (explicit user bitrate, an old host, or ack silence).
    enabled: bool,
    /// The rate we believe the host encodes at (updated by acks; requests are not assumed).
    current_kbps: u32,
    /// The session's starting (negotiated) rate — the recovery ceiling.
    ceiling_kbps: u32,
    floor_kbps: u32,
    /// Recent window mean OWDs (µs); the rolling min is the uncongested baseline.
    owd_means: VecDeque<i64>,
    bad_windows: u32,
    clean_windows: u32,
    last_change: Option<Instant>,
    /// Requests since the last ack — reaching [`MAX_UNACKED`] disables the controller.
    unacked: u32,
}

impl BitrateController {
    /// `start_kbps` is the Welcome-resolved session bitrate when the user chose Automatic, or `0`
    /// to build a permanently-disabled controller (explicit bitrate / an old host that didn't
    /// echo one — no known ceiling to work against).
    pub(crate) fn new(start_kbps: u32) -> Self {
        BitrateController {
            enabled: start_kbps > 0,
            current_kbps: start_kbps,
            ceiling_kbps: start_kbps,
            floor_kbps: FLOOR_KBPS.min(start_kbps.max(1)),
            owd_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
            unacked: 0,
        }
    }

    /// The host's [`crate::quic::BitrateChanged`] ack: its clamp is authoritative for what the
    /// encoder now targets, and any ack proves the host renegotiates (resets the silence counter).
    pub(crate) fn on_ack(&mut self, kbps: u32) {
        if kbps > 0 {
            self.current_kbps = kbps;
        }
        self.unacked = 0;
    }

    /// Feed one report window; returns the rate to request now, if any. `dropped` = frames that
    /// went FEC-unrecoverable in the window, `loss_ppm` the window's [`crate::quic::LossReport`]
    /// figure, `owd_mean_us` the window's mean skew-corrected capture→received latency (`None`
    /// without a clock handshake), `flushed` = the pump's jump-to-live fired in the window.
    pub(crate) fn on_window(
        &mut self,
        now: Instant,
        dropped: u64,
        loss_ppm: u32,
        owd_mean_us: Option<i64>,
        flushed: bool,
    ) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        if self.unacked >= MAX_UNACKED {
            // The host never answered: an older build. Go quiet instead of spamming a message it
            // logs as unknown every few seconds.
            self.enabled = false;
            tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            return None;
        }
        // OWD: compare against the rolling-min baseline of PRIOR windows (so a rising window
        // doesn't drag its own baseline up), then record it.
        let owd_bad = match owd_mean_us {
            Some(mean) => {
                let bad = self
                    .owd_means
                    .iter()
                    .min()
                    .is_some_and(|&base| mean > base + OWD_RISE_US);
                if self.owd_means.len() == BASELINE_WINDOWS {
                    self.owd_means.pop_front();
                }
                self.owd_means.push_back(mean);
                bad
            }
            None => false,
        };
        let bad = dropped > 0 || loss_ppm >= HEAVY_LOSS_PPM || owd_bad || flushed;
        if bad {
            self.bad_windows += 1;
            self.clean_windows = 0;
        } else {
            self.clean_windows += 1;
            self.bad_windows = 0;
        }
        let cooled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= CHANGE_COOLDOWN);
        if !cooled {
            return None;
        }
        if self.bad_windows >= BAD_WINDOWS_TO_DECREASE && self.current_kbps > self.floor_kbps {
            let next = ((self.current_kbps as u64 * 7 / 10) as u32).max(self.floor_kbps);
            self.bad_windows = 0;
            return self.request(next, now);
        }
        if self.clean_windows >= CLEAN_WINDOWS_TO_INCREASE && self.current_kbps < self.ceiling_kbps
        {
            let next = (self.current_kbps + self.current_kbps / 16 + 1).min(self.ceiling_kbps);
            self.clean_windows = 0;
            return self.request(next, now);
        }
        None
    }

    fn request(&mut self, kbps: u32, now: Instant) -> Option<u32> {
        self.last_change = Some(now);
        self.unacked += 1;
        // `current_kbps` is NOT updated here — the host's ack is authoritative. A lost/ignored
        // request just recomputes from the same base next time (and counts toward MAX_UNACKED).
        Some(kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window cadence matching the pump's 750 ms tick, safely past the change cooldown when
    /// stepped 5× between decisions.
    const TICK: Duration = Duration::from_millis(750);

    fn ticks(start: Instant, n: u32) -> Instant {
        start + TICK * n
    }

    /// Drive `n` clean windows, asserting no decision fires before the clean threshold.
    fn run_clean(c: &mut BitrateController, start: Instant, from: u32, n: u32) -> Option<u32> {
        let mut out = None;
        for i in from..from + n {
            out = c.on_window(ticks(start, i), 0, 0, Some(10_000), false);
            if out.is_some() {
                return out;
            }
        }
        out
    }

    #[test]
    fn disabled_when_not_automatic_or_old_host() {
        // start 0 = explicit user bitrate or a host that didn't echo one → permanently off.
        let mut c = BitrateController::new(0);
        let now = Instant::now();
        assert_eq!(c.on_window(now, 5, 900_000, Some(500_000), true), None);
    }

    #[test]
    fn two_bad_windows_step_down_multiplicatively() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // One bad window is a blip — no reaction.
        assert_eq!(c.on_window(ticks(start, 0), 1, 0, None, false), None);
        // The second consecutive bad window backs off ×0.7.
        assert_eq!(c.on_window(ticks(start, 1), 1, 0, None, false), Some(14_000));
        c.on_ack(14_000);
        // Still bad after the cooldown → another ×0.7 step from the ACKED rate.
        assert_eq!(c.on_window(ticks(start, 6), 1, 0, None, false), None); // bad #1 again
        assert_eq!(c.on_window(ticks(start, 7), 1, 0, None, false), Some(9_800));
    }

    #[test]
    fn cooldown_blocks_back_to_back_steps() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(c.on_window(ticks(start, 0), 1, 0, None, false), None);
        assert_eq!(c.on_window(ticks(start, 1), 1, 0, None, false), Some(14_000));
        c.on_ack(14_000);
        // Two more bad windows land INSIDE the 3 s cooldown (ticks 2,3 = 1.5/2.25 s) → held.
        assert_eq!(c.on_window(ticks(start, 2), 1, 0, None, false), None);
        assert_eq!(c.on_window(ticks(start, 3), 1, 0, None, false), None);
    }

    #[test]
    fn floor_is_never_crossed() {
        let mut c = BitrateController::new(6_000);
        let start = Instant::now();
        assert_eq!(c.on_window(ticks(start, 0), 1, 0, None, false), None);
        // ×0.7 of 6000 = 4200 < floor → clamped to 5000.
        assert_eq!(c.on_window(ticks(start, 1), 1, 0, None, false), Some(5_000));
        c.on_ack(5_000);
        // At the floor, further bad windows request nothing.
        assert_eq!(c.on_window(ticks(start, 6), 1, 0, None, false), None);
        assert_eq!(c.on_window(ticks(start, 7), 1, 0, None, false), None);
    }

    #[test]
    fn sustained_clean_recovers_toward_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(c.on_window(ticks(start, 0), 1, 0, None, false), None);
        assert_eq!(c.on_window(ticks(start, 1), 1, 0, None, false), Some(14_000));
        c.on_ack(14_000);
        // 13 clean windows → one additive step up (14000 + 14000/16 + 1 = 14876).
        let up = run_clean(&mut c, start, 2, 13);
        assert_eq!(up, Some(14_876));
        c.on_ack(14_876);
        // Fully recovered → clean windows at the ceiling stay quiet (never probe past start).
        c.on_ack(20_000);
        assert_eq!(run_clean(&mut c, start, 40, 20), None);
    }

    #[test]
    fn owd_rise_alone_is_a_congestion_signal() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // Establish a ~10 ms baseline over a few clean windows.
        for i in 0..4 {
            assert_eq!(c.on_window(ticks(start, i), 0, 0, Some(10_000), false), None);
        }
        // Delay climbs 40 ms above baseline with ZERO loss — bufferbloat. Two windows → back off.
        assert_eq!(c.on_window(ticks(start, 4), 0, 0, Some(50_000), false), None);
        assert_eq!(
            c.on_window(ticks(start, 5), 0, 0, Some(52_000), false),
            Some(14_000)
        );
    }

    #[test]
    fn ack_silence_disables_the_controller() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut sent = 0;
        let mut i = 0;
        // Keep every window bad and never ack: exactly MAX_UNACKED requests, then silence.
        while i < 60 {
            if c.on_window(ticks(start, i), 1, 0, None, false).is_some() {
                sent += 1;
            }
            i += 1;
        }
        assert_eq!(sent, MAX_UNACKED);
    }

    #[test]
    fn flush_counts_as_a_bad_window() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(c.on_window(ticks(start, 0), 0, 0, None, true), None);
        assert_eq!(c.on_window(ticks(start, 1), 0, 0, None, true), Some(14_000));
    }
}
