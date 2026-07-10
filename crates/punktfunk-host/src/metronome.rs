//! Detector for METRONOMIC event cycles — evenly-spaced disturbances repeating every few seconds.
//!
//! The "periodic double-jolt" symptom class field reports keep describing is a host/display-side
//! disturbance on a stable multi-second period (display-topology churn, display-poller software,
//! virtual-display present timing). Random network loss is bursty and irregular; a stable period is
//! a machine, and saying so in the host log turns a "nothing in the logs :/" report into a
//! self-diagnosis. Two feeds today: served client-recovery IDRs (`punktfunk1`) and IDD-push capture
//! stalls (`capture::windows::idd_push`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Pure evenly-spaced-events detector (unit-tested below).
///
/// Events within [`Self::COALESCE`] count as ONE (a double-jolt's paired disturbances — e.g. the
/// cooldown re-issue of a lost keyframe ~0.7 s after the first — are one user-visible cycle). When
/// the gaps between the last [`Self::STREAK`] events are all within ±[`Self::TOLERANCE`] of their
/// mean, [`Self::note`] returns the mean period for the caller to warn with, then stays quiet for
/// [`Self::REWARN`] while the cycle persists.
pub(crate) struct Metronome {
    events: VecDeque<Instant>,
    last_warn: Option<Instant>,
}

impl Metronome {
    /// Events closer together than this are the same user-visible disturbance.
    const COALESCE: Duration = Duration::from_millis(1500);
    /// Consecutive evenly-spaced events before the cycle counts as metronomic.
    const STREAK: usize = 4;
    /// "Evenly spaced" = every gap within this fraction of the mean gap.
    const TOLERANCE: f64 = 0.2;
    /// Once warned, re-warn at most this often while the cycle persists.
    const REWARN: Duration = Duration::from_secs(30);

    pub(crate) fn new() -> Self {
        Self {
            events: VecDeque::new(),
            last_warn: None,
        }
    }

    /// Record a disturbance at `now`; `Some(mean period)` exactly when the metronomic-cycle
    /// warning should fire.
    pub(crate) fn note(&mut self, now: Instant) -> Option<Duration> {
        if self
            .events
            .back()
            .is_some_and(|last| now.duration_since(*last) < Self::COALESCE)
        {
            return None;
        }
        self.events.push_back(now);
        if self.events.len() > Self::STREAK {
            self.events.pop_front();
        }
        if self.events.len() < Self::STREAK {
            return None;
        }
        let gaps: Vec<f64> = self
            .events
            .iter()
            .zip(self.events.iter().skip(1))
            .map(|(a, b)| b.duration_since(*a).as_secs_f64())
            .collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        if mean <= 0.0
            || gaps
                .iter()
                .any(|g| (g - mean).abs() > mean * Self::TOLERANCE)
        {
            return None;
        }
        if self
            .last_warn
            .is_some_and(|t| now.duration_since(t) < Self::REWARN)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(Duration::from_secs_f64(mean))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a [`Metronome`] a schedule of event offsets (ms from a common origin) and return
    /// what each `note` produced.
    fn cadence_run(offsets_ms: &[u64]) -> Vec<Option<Duration>> {
        let base = Instant::now();
        let mut c = Metronome::new();
        offsets_ms
            .iter()
            .map(|ms| c.note(base + Duration::from_millis(*ms)))
            .collect()
    }

    #[test]
    fn cadence_detects_metronomic_events() {
        // Four events ~4 s apart (±5%) → the fourth trips the detector at ~4 s.
        let out = cadence_run(&[0, 4_000, 8_100, 11_950]);
        assert_eq!(out[..3], [None, None, None]);
        let period = out[3].expect("metronomic series must be detected");
        assert!(
            (period.as_secs_f64() - 3.98).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn cadence_coalesces_double_jolt_pairs() {
        // The field signature: a jolt pair (second event ~0.7 s after the first, e.g. the IDR
        // cooldown re-issue) every ~4 s. Each pair is ONE event; detection still lands on the
        // ~4 s cycle.
        let out = cadence_run(&[
            0, 700, // pair 1
            4_000, 4_700, // pair 2
            8_000, 8_650,  // pair 3
            12_000, // pair 4 (first event trips it)
        ]);
        assert!(out[..6].iter().all(Option::is_none));
        let period = out[6].expect("coalesced pairs must still read as a 4 s cycle");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn cadence_ignores_irregular_bursts() {
        // Genuine Wi-Fi-style loss: irregular gaps → never flagged.
        assert!(cadence_run(&[0, 2_000, 9_000, 12_500, 21_000])
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn cadence_rewarns_at_most_every_30s() {
        // A persisting 4 s cycle: warn on the 4th event (t=12 s), then stay quiet until ≥30 s
        // past the warn — the t=44 s event (index 11) is the first at or beyond t=42 s.
        let offsets: Vec<u64> = (0..12).map(|i| i * 4_000).collect();
        let out = cadence_run(&offsets);
        let warned: Vec<usize> = out
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.map(|_| i))
            .collect();
        assert_eq!(warned, vec![3, 11], "warn indices");
    }
}
