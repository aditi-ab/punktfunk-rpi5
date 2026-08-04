//! Desktop-audio capture POLICY — the parts of [`wasapi_cap`](super::wasapi_cap) that are pure
//! decisions rather than WASAPI plumbing, split out for the same reason
//! [`wiring_plan`](super::wiring_plan) is: so they compile and their unit tests RUN on every
//! platform. Both of these encode field-report behaviour, and regressing either must fail CI on
//! Linux too, not only on a Windows box.
//!
//! * [`FightDamper`] — how hard to fight another program for the default playback device.
//! * [`CaptureStats`] — the audio plane's vitals, so a log can tell a quiet host from a broken
//!   endpoint from one we are damaging ourselves.

use std::time::{Duration, Instant};

/// Default-playback re-assertions inside [`FIGHT_WINDOW`] before we stop fighting.
pub(crate) const FIGHT_LIMIT: u32 = 4;
pub(crate) const FIGHT_WINDOW: Duration = Duration::from_secs(20);
/// How long to leave the default alone once another program has proven it will take it back.
pub(crate) const FIGHT_BACKOFF: Duration = Duration::from_secs(60);

/// Damping for the default-playback tug-of-war (WP2.4).
///
/// The 2026-08-03 field log recorded seven full re-assert cycles in sixteen seconds — something on
/// that box re-set the default playback to CABLE Input every ~4 s and we snapped it back every
/// time, each round a capture teardown plus a wiring pass with `IPolicyConfig` writes. Winning that
/// argument is not possible and every round was an audible dropout, so: re-assert a few times
/// (transient churn does settle), then concede for a minute and say so once.
///
/// Time is passed IN rather than read here, which keeps the policy pure and testable.
pub(crate) struct FightDamper {
    /// Re-assertions in the current window, and when the window opened.
    count: u32,
    window_started: Instant,
    /// Set while we are deliberately not fighting.
    paused_until: Option<Instant>,
    /// One warning per fight burst, and one per concession.
    warned_fighting: bool,
    warned_giving_up: bool,
    now: Instant,
}

impl FightDamper {
    pub(crate) fn new(now: Instant) -> FightDamper {
        FightDamper {
            count: 0,
            window_started: now,
            paused_until: None,
            warned_fighting: false,
            warned_giving_up: false,
            now,
        }
    }

    /// A dud default-device change was observed at `now`.
    pub(crate) fn observed_at(&mut self, now: Instant) {
        self.now = now;
        if now.duration_since(self.window_started) >= FIGHT_WINDOW {
            self.window_started = now;
            self.count = 0;
            self.warned_fighting = false;
        }
        if self.paused_until.is_some_and(|t| now >= t) {
            self.paused_until = None;
            self.warned_giving_up = false;
            self.count = 0;
            self.window_started = now;
        }
    }

    /// Should we put the default back? False while paused, or once this window's budget is spent.
    pub(crate) fn should_reassert(&mut self) -> bool {
        if self.paused_until.is_some() {
            return false;
        }
        if self.count >= FIGHT_LIMIT {
            self.paused_until = Some(self.now + FIGHT_BACKOFF);
            return false;
        }
        self.count += 1;
        true
    }

    /// Warn on the FIRST re-assert of a burst only (the rest are noise).
    pub(crate) fn warn_now(&mut self) -> bool {
        !std::mem::replace(&mut self.warned_fighting, true)
    }

    /// Warn once when we concede.
    pub(crate) fn warn_giving_up(&mut self) -> bool {
        self.paused_until.is_some() && !std::mem::replace(&mut self.warned_giving_up, true)
    }

    /// Currently conceding (test/diagnostic accessor).
    pub(crate) fn is_paused(&self) -> bool {
        self.paused_until.is_some()
    }
}

/// How often the capture loop reports its vitals (WP0.2).
pub(crate) const STATS_EVERY: Duration = Duration::from_secs(30);

/// One reporting window's worth of capture vitals.
///
/// The point is to make three states that used to look identical in a log tell themselves apart: a
/// genuinely quiet host (`peak` ~0, no drops), a working stream (`peak` > 0), and a stream we are
/// damaging ourselves (`dropped_chunks` > 0). The 2026-08-03 field log — 3,600 lines, filed over an
/// audio-quality complaint — could distinguish none of them, because the audio plane logged nothing
/// at all between "capturing" and the session ending.
#[derive(Default)]
pub(crate) struct CaptureStats {
    pub(crate) frames: u64,
    /// Interleaved SAMPLES seen — the RMS denominator. Deliberately separate from `frames`:
    /// dividing the sum of squares by the frame count instead inflates RMS by sqrt(channels),
    /// which made a sine report an RMS equal to its own peak.
    pub(crate) samples: u64,
    /// Loudest |sample| in the window — tells a silent endpoint from a working one.
    pub(crate) peak: f32,
    /// Sum of squares, for the window's RMS: a level far below peak means a badly attenuated
    /// endpoint (a parked device sitting at 20 % volume costs ~14 dB before Opus ever sees it).
    pub(crate) sumsq: f64,
    /// Chunks the encode thread was too slow to take. Silent data loss, previously uncounted:
    /// the encoder simply concatenates across the hole, so it is a click AND a permanent shift of
    /// everything after it.
    pub(crate) dropped_chunks: u64,
}

impl CaptureStats {
    pub(crate) fn observe(&mut self, samples: &[f32], channels: u32) {
        self.frames += (samples.len() / channels.max(1) as usize) as u64;
        self.samples += samples.len() as u64;
        for &s in samples {
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sumsq += (s as f64) * (s as f64);
        }
    }

    /// `(peak dBFS, rms dBFS, delivered %)` for this window. Silence reports -120 dB rather than
    /// -inf so the log line stays parseable.
    pub(crate) fn summary(&self, elapsed: Duration, sample_rate: u32) -> (f64, f64, f64) {
        let rms = (self.sumsq / (self.samples as f64).max(1.0)).sqrt();
        let db = |v: f64| if v > 0.0 { 20.0 * v.log10() } else { -120.0 };
        // Expected frames for the window — a shortfall means the endpoint is not delivering at
        // real time (a stalling virtual device), which a peak/RMS alone cannot show.
        let expected = elapsed.as_secs_f64() * sample_rate as f64;
        (
            db(self.peak as f64),
            db(rms),
            (self.frames as f64 / expected.max(1.0)) * 100.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays the 2026-08-03 field shape: a dud default change every ~2 s, forever. We must put
    /// the default back a few times, then concede — and warn exactly once for each.
    #[test]
    fn fight_damper_concedes_instead_of_looping_forever() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        let (mut warns_fighting, mut warns_giving_up) = (0, 0);
        for i in 0..8 {
            d.observed_at(t0 + Duration::from_millis(i * 2_000));
            if d.should_reassert() {
                reasserts += 1;
                if d.warn_now() {
                    warns_fighting += 1;
                }
            } else if d.warn_giving_up() {
                warns_giving_up += 1;
            }
        }
        assert_eq!(
            reasserts, FIGHT_LIMIT,
            "must stop after the window's budget"
        );
        assert_eq!(warns_fighting, 1, "one warning per burst, not one per flip");
        assert_eq!(warns_giving_up, 1, "concede exactly once");
    }

    /// Occasional, genuinely transient churn must ALWAYS be corrected — the damper must not
    /// accumulate across widely-spaced events and quietly stop doing its job.
    #[test]
    fn fight_damper_always_fixes_isolated_changes() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        for i in 1..=10 {
            d.observed_at(t0 + FIGHT_WINDOW * i);
            if d.should_reassert() {
                reasserts += 1;
            }
        }
        assert_eq!(reasserts, 10, "isolated changes must always be corrected");
    }

    /// After the backoff expires the damper re-arms, so a program that goes quiet and comes back
    /// later is fought again rather than being conceded to for the rest of the session.
    #[test]
    fn fight_damper_rearms_after_the_backoff() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        for i in 0..FIGHT_LIMIT + 2 {
            d.observed_at(t0 + Duration::from_millis(i as u64 * 500));
            d.should_reassert();
        }
        assert!(d.is_paused(), "should have conceded");
        d.observed_at(t0 + FIGHT_BACKOFF + FIGHT_WINDOW * 2);
        assert!(d.should_reassert(), "must re-arm once the backoff expires");
    }

    /// Peak/RMS must separate the states a log could not previously tell apart.
    #[test]
    fn capture_stats_separate_silence_from_signal() {
        let mut quiet = CaptureStats::default();
        quiet.observe(&[0.0; 480], 2);
        let (peak, rms, _) = quiet.summary(Duration::from_secs(1), 48_000);
        assert_eq!(peak, -120.0, "digital silence reports the floor, not -inf");
        assert_eq!(rms, -120.0);

        let mut loud = CaptureStats::default();
        let tone: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin() * 0.5).collect();
        loud.observe(&tone, 2);
        assert_eq!(
            loud.frames, 240,
            "480 interleaved stereo samples = 240 frames"
        );
        let (peak, rms, _) = loud.summary(Duration::from_secs(1), 48_000);
        assert!(
            peak > -8.0 && peak <= 0.0,
            "peak {peak} dBFS should track a 0.5 tone"
        );
        // A sine's RMS is its amplitude / sqrt(2) — about 3 dB below peak. Getting this equal to
        // peak is exactly what a frames-vs-samples mix-up in the denominator looks like, so the
        // margin is asserted rather than just the ordering.
        assert!(
            rms < peak - 2.0,
            "RMS {rms} vs peak {peak}: a sine must sit ~3 dB below its peak"
        );
    }

    /// The delivered-percentage is what shows an endpoint that has stopped feeding us in real
    /// time — invisible in peak/RMS, and the shape a stalling virtual device makes.
    #[test]
    fn capture_stats_report_a_delivery_shortfall() {
        let mut full = CaptureStats::default();
        full.observe(&vec![0.1f32; 48_000 * 2], 2); // exactly 1 s of stereo
        let (_, _, pct) = full.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 100.0).abs() < 1.0, "expected ~100 %, got {pct}");

        let mut half = CaptureStats::default();
        half.observe(&vec![0.1f32; 48_000], 2); // 0.5 s of stereo in a 1 s window
        let (_, _, pct) = half.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 50.0).abs() < 1.0, "expected ~50 %, got {pct}");
    }
}
