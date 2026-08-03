//! Circular (directional) statistics for phase-locked capture (design/phase-locked-capture.md):
//! the client-side half of the controller's v2 error signal, plus the panel-grid learner every
//! vsync-aware presenter paces against. Pure math, no features — shared so every presenter
//! (Android today, iOS and the desktop session client next) computes the SAME statistic the host
//! controller was tuned against, and so the controller's simulation tests can generate their
//! synthetic reports through the identical code path.

/// Plausible panel periods: ~24 Hz to ~500 Hz. A spacing outside this is a clock glitch, not a
/// display mode, and must never reach the estimate.
const PANEL_PERIOD_RANGE_NS: std::ops::RangeInclusive<i64> = 2_000_000..=42_000_000;

/// Spacings within this of the estimate are the same grid — absorbs ordinary timeline jitter.
const PANEL_GRID_TOLERANCE_NS: i64 = 200_000;

/// Consecutive WIDER observations required before the estimate grows. One stray wide sample is a
/// scheduling hiccup; eight in a row (~66 ms at 120 Hz) is a display that really did slow down.
const PANEL_WIDEN_STREAK: u8 = 8;

/// The panel's true refresh period, learned from observed vsync/frame-timeline spacing.
///
/// A presenter subdivides its release targets onto this grid, so an estimate FINER than the panel
/// makes it aim at instants that never arrive and release faster than the display consumes —
/// which is why the estimate has to be able to move both ways.
///
/// Seeding is the reason this is not simply "believe the last sample". The platform's *configured*
/// mode is not the panel: under a per-uid frame-rate override a 120 Hz panel reports 60
/// (`Display.getRefreshRate` returns the override — observed on-glass, A024), and the app's own
/// choreographer callbacks arrive at the down-rated rate while the panel scans at its own. The
/// mode TABLE is honest about what the panel *can* do, so it is the seed; the timeline spacing is
/// honest about what it is *doing*, so it is the correction.
///
/// The asymmetry is deliberate. **Narrowing is immediate**: a finer real grid is always safe to
/// subdivide onto, and it is the down-rate case the seed most often gets wrong. **Widening needs
/// [`PANEL_WIDEN_STREAK`] consecutive agreeing observations** and then adopts the *narrowest* of
/// them, because a wide sample is far more likely to be a missed callback than a mode change.
///
/// ⚠ 0.23.0 shipped this learner as narrow-only, seeded from the display mode the app *requests*
/// (`preferredDisplayModeId` is a hint the system may refuse). A refused 120 Hz switch therefore
/// left the presenter pacing a 60 Hz panel on an 8.33 ms grid with no way back — permanently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PanelGrid {
    period_ns: i64,
    widen_streak: u8,
    /// Narrowest wider-than-estimate spacing seen during the current streak.
    widen_candidate: i64,
}

impl PanelGrid {
    /// Seed from the display mode's refresh rate (`0` = unknown — the first plausible observation
    /// then sets the estimate outright).
    pub fn seeded(hz: i32) -> PanelGrid {
        PanelGrid {
            period_ns: if hz > 0 { 1_000_000_000 / hz as i64 } else { 0 },
            widen_streak: 0,
            widen_candidate: 0,
        }
    }

    /// The learned period, or `0` while unknown.
    pub fn period_ns(&self) -> i64 {
        self.period_ns
    }

    /// Fold one observed grid spacing. Returns `true` when [`period_ns`](Self::period_ns) changed.
    pub fn observe(&mut self, spacing_ns: i64) -> bool {
        if !PANEL_PERIOD_RANGE_NS.contains(&spacing_ns) {
            return false; // implausible — a clock glitch, not a display mode
        }
        if self.period_ns == 0 {
            self.reset_streak();
            self.period_ns = spacing_ns;
            return true;
        }
        if spacing_ns < self.period_ns - PANEL_GRID_TOLERANCE_NS {
            self.reset_streak();
            self.period_ns = spacing_ns;
            return true;
        }
        if spacing_ns > self.period_ns + PANEL_GRID_TOLERANCE_NS {
            self.widen_streak = self.widen_streak.saturating_add(1);
            self.widen_candidate = if self.widen_candidate == 0 {
                spacing_ns
            } else {
                self.widen_candidate.min(spacing_ns)
            };
            if self.widen_streak >= PANEL_WIDEN_STREAK {
                self.period_ns = self.widen_candidate;
                self.reset_streak();
                return true;
            }
            return false;
        }
        self.reset_streak(); // this sample agreed — the run of wider ones is broken
        false
    }

    fn reset_streak(&mut self) {
        self.widen_streak = 0;
        self.widen_candidate = 0;
    }
}

/// Circular (vector-mean) statistics of latch samples against a display period: the mean latch
/// mod the period (ns) and the coherence (‰).
///
/// The mean is what a phase controller can actually steer under jitter — the MEDIAN of a
/// period-spanning distribution is immovable (shifting a uniform-mod-P distribution's mean
/// leaves its median untouched; the controller-v1 on-glass lesson, 2026-07-31). The coherence
/// (the resultant length `R` of the unit phasors, scaled to ‰) says whether ANY phase exists to
/// steer: 0 = arrivals uniformly smeared over the period (alignment is physically pointless),
/// 1000 = perfectly phase-locked.
///
/// `None` under 8 samples or a non-positive period — too little evidence to report a phase.
pub fn circular_latch(samples_us: &[u64], period_ns: i64) -> Option<(u64, u16)> {
    if samples_us.len() < 8 || period_ns <= 0 {
        return None;
    }
    let period_us = period_ns as f64 / 1000.0;
    let (mut x, mut y) = (0.0f64, 0.0f64);
    for &s in samples_us {
        let theta = (s as f64 % period_us) / period_us * std::f64::consts::TAU;
        x += theta.cos();
        y += theta.sin();
    }
    let n = samples_us.len() as f64;
    let r = (x * x + y * y).sqrt() / n;
    let mean_theta = y.atan2(x).rem_euclid(std::f64::consts::TAU);
    let mean_ns = (mean_theta / std::f64::consts::TAU * period_ns as f64) as u64;
    Some((mean_ns, (r * 1000.0) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz in ns
    const P_US: u64 = 8_333; // …and in µs, the sample unit

    #[test]
    fn identical_samples_are_fully_coherent() {
        let (mean, coh) = circular_latch(&[4_000; 16], P).unwrap();
        assert!(coh >= 995, "identical phases must read ~1000‰, got {coh}");
        assert!(
            (mean as i64 - 4_000_000).abs() < 20_000,
            "mean {mean} ≉ 4.0 ms"
        );
    }

    #[test]
    fn uniform_grid_over_the_period_is_incoherent() {
        // 16 samples evenly spanning one period — the resultant vector cancels.
        let samples: Vec<u64> = (0..16).map(|i| i * P_US / 16).collect();
        let (_, coh) = circular_latch(&samples, P).unwrap();
        assert!(coh < 100, "a uniform phase smear must read ~0‰, got {coh}");
    }

    #[test]
    fn cluster_straddling_the_wrap_averages_at_the_boundary() {
        // Half the samples just below the period boundary, half just above 0: an ARITHMETIC
        // mean would report ~P/2 (maximally wrong); the circular mean must sit at the boundary.
        let samples = [
            P_US - 200,
            P_US - 100,
            P_US - 50,
            P_US - 150,
            100,
            50,
            150,
            200,
        ];
        let (mean, coh) = circular_latch(&samples, P).unwrap();
        let dist_to_boundary = (mean as i64).min((P - mean as i64).abs());
        assert!(
            dist_to_boundary < 500_000,
            "circular mean {mean} must hug the wrap boundary"
        );
        assert!(
            coh > 900,
            "a tight straddling cluster is still coherent, got {coh}"
        );
    }

    #[test]
    fn too_few_samples_report_nothing() {
        assert!(circular_latch(&[1_000; 7], P).is_none());
        assert!(circular_latch(&[1_000; 16], 0).is_none());
    }
}

#[cfg(test)]
mod panel_grid_tests {
    use super::*;

    const P120: i64 = 8_333_333;
    const P60: i64 = 16_666_666;

    #[test]
    fn seeds_from_the_mode_and_reports_unknown_without_one() {
        assert_eq!(PanelGrid::seeded(120).period_ns(), 8_333_333);
        assert_eq!(PanelGrid::seeded(0).period_ns(), 0);
        let mut g = PanelGrid::seeded(0);
        assert!(
            g.observe(P120),
            "the first plausible sample sets an unseeded grid"
        );
        assert_eq!(g.period_ns(), P120);
    }

    #[test]
    fn narrows_immediately_when_the_panel_is_faster_than_the_mode_said() {
        // The down-rate case: the mode table read 60, the timelines run at 120.
        let mut g = PanelGrid::seeded(60);
        assert!(g.observe(P120));
        assert_eq!(g.period_ns(), P120, "a finer real grid is adopted at once");
    }

    /// The 0.23.0 bug: `preferredDisplayModeId` is a request, so a refused 120 Hz switch seeds a
    /// 120 Hz grid on a panel that is really running 60. The narrow-only learner could never
    /// climb back, and the presenter aimed at instants the panel never reached.
    #[test]
    fn widens_back_out_when_the_requested_mode_was_refused() {
        let mut g = PanelGrid::seeded(120);
        for i in 0..PANEL_WIDEN_STREAK - 1 {
            assert!(!g.observe(P60), "sample {i} must not widen on its own");
            assert_eq!(g.period_ns(), P120);
        }
        assert!(
            g.observe(P60),
            "a sustained run of wider spacings widens the grid"
        );
        assert_eq!(g.period_ns(), P60);
    }

    #[test]
    fn one_stray_wide_sample_never_widens() {
        let mut g = PanelGrid::seeded(120);
        for _ in 0..40 {
            assert!(!g.observe(P60));
            assert!(!g.observe(P120)); // an agreeing sample breaks the run
        }
        assert_eq!(
            g.period_ns(),
            P120,
            "alternating samples must not accumulate"
        );
    }

    #[test]
    fn widening_adopts_the_narrowest_of_the_run() {
        let mut g = PanelGrid::seeded(120);
        // A run of wide spacings that includes some very wide outliers.
        let run = [
            P60,
            33_000_000,
            P60 + 400_000,
            41_000_000,
            P60,
            P60,
            P60,
            P60,
        ];
        for s in run {
            g.observe(s);
        }
        assert_eq!(
            g.period_ns(),
            P60,
            "the estimate takes the narrowest of the run, never an outlier"
        );
    }

    #[test]
    fn implausible_spacings_are_ignored_entirely() {
        let mut g = PanelGrid::seeded(120);
        for _ in 0..100 {
            assert!(!g.observe(0));
            assert!(!g.observe(-1));
            assert!(!g.observe(1_000_000)); // 1000 Hz — below the range floor
            assert!(!g.observe(100_000_000)); // 10 Hz — above the ceiling
        }
        assert_eq!(g.period_ns(), P120);
    }

    #[test]
    fn a_transient_narrow_glitch_self_heals() {
        // Narrowing is immediate, so a glitch DOES poison the estimate — the point is that it is
        // no longer permanent (0.23.0's learner had no way back).
        let mut g = PanelGrid::seeded(120);
        assert!(g.observe(2_100_000), "a glitch narrows the estimate");
        assert_eq!(g.period_ns(), 2_100_000);
        for _ in 0..PANEL_WIDEN_STREAK {
            g.observe(P120);
        }
        assert_eq!(g.period_ns(), P120, "and the real grid wins it back");
    }
}
