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

// ---- Source-timestamp playout (design/presenter-cadence-rework.md, WP3) --------------------

/// Tuning for one cadence loop. Gains are SHIFT COUNTS — the loop is fixed-point i64 throughout,
/// so it runs identically on every client and in the offline harness, and carries no float into a
/// present path.
///
/// ⚠ **These values are provisional, and saying so is part of the design.** The plan asks for
/// constants fitted to recorded `(src_pts, received, decoded)` traces (its spike S2), and S2 was
/// never run — the 2026-08-05 baseline records that omission itself. What is here is derived from
/// first principles (a proportional time constant of tens of frames, an integral an order slower,
/// a cushion of a few mean-absolute-deviations) and is honest about being a starting point. The
/// first real trace should replace them, with the trace named beside each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CadenceTuning {
    /// Proportional gain on the offset estimate: `1 >> offset_shift` of the residual per frame.
    pub offset_shift: u8,
    /// Integral gain on the per-frame rate (skew) term: `1 >> skew_shift`.
    pub skew_shift: u8,
    /// EMA weight for the residual mean-absolute-deviation.
    pub jitter_shift: u8,
    /// Per-sample residual clamp — one outlier must not yank the estimate.
    pub error_clamp_ns: i64,
    /// Cushion = `mad * cushion_num / cushion_den`, clamped to
    /// `[cushion_floor_ns, frame_interval_ns]`.
    pub cushion_num: u16,
    pub cushion_den: u16,
    pub cushion_floor_ns: i64,
    /// Source-timestamp gap beyond which the loop re-anchors instead of tracking.
    pub reanchor_gap_ns: i64,
}

impl CadenceTuning {
    /// For callers that snap the due time onto a display grid afterwards: the snap-up itself
    /// carries roughly half a refresh of implicit slack, so the cushion can be small.
    pub const fn snapping() -> CadenceTuning {
        CadenceTuning {
            offset_shift: 5,
            skew_shift: 10,
            jitter_shift: 5,
            error_clamp_ns: 20_000_000,
            cushion_num: 2,
            cushion_den: 1,
            cushion_floor_ns: 500_000,
            reanchor_gap_ns: 500_000_000,
        }
    }

    /// For callers presenting at the due time directly (VRR, direct scanout): no implicit slack,
    /// so the cushion must cover more of the distribution on its own.
    pub const fn free_running() -> CadenceTuning {
        CadenceTuning {
            cushion_num: 3,
            cushion_floor_ns: 2_000_000,
            ..CadenceTuning::snapping()
        }
    }
}

/// Loop health for the 1 Hz line — the numbers that say whether the cushion is doing its job.
///
/// Residual PERCENTILES are deliberately absent: this type is allocation-free and holds no
/// histogram, and the client stat paths (the judder metric) are where distributions belong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CadenceHealth {
    /// Frames folded since the last [`CadenceClock::reset`].
    pub frames: u64,
    /// …of which the due time was already past when the frame became presentable. The direct
    /// signal that the cushion is too small.
    pub late: u64,
    /// Times the loop gave up tracking and re-anchored (gap, regression, or explicit reset).
    pub reanchors: u64,
    pub offset_ns: i64,
    pub skew_ns: i64,
    pub jitter_ns: i64,
    pub cushion_ns: i64,
}

/// Plays frames out on the SOURCE's cadence instead of on their arrival instant.
///
/// The defect it exists for: every client presents a frame as soon as it is decoded, so the
/// transport's jitter — and, on a host whose compositor delivers raggedly, the compositor's —
/// lands on the glass 1:1. The loop estimates the offset between the source clock and the present
/// clock, and hands back a due time on the source's own timeline plus a cushion sized to the
/// measured jitter.
///
/// **Type-2 on purpose.** It tracks offset *and* per-frame rate, because two free-running crystals
/// produce a ramp and a proportional-only loop lags a ramp forever.
///
/// **It smooths the offset, never the timestamps.** The due time is `src_pts + offset + cushion`,
/// so genuine variation in the source's own cadence — a variable-rate renderer, an irregular
/// capture tick — passes straight through. Only the transport's contribution to `ready − pts` is
/// filtered. Any change that makes due times more evenly spaced than the source is a bug, not an
/// improvement; `preserves_source_cadence` is the test that says so.
///
/// **Domain-agnostic by construction.** A constant offset between clock domains (monotonic vs
/// realtime) is absorbed by the offset estimator, so a caller feeds `ready_ns` and reads `due_ns`
/// in ONE domain and needs no clock conversion anywhere in this path. Suspend/resume breaks the
/// constant — that is a discontinuity, and [`reset`](Self::reset) covers it.
///
/// Prior art is ordinary and old: MPEG-2 TS PCR recovery and RTP playout scheduling (RFC 3550
/// §6.4.1 carries the jitter estimator this MAD mirrors).
#[derive(Debug, Clone)]
pub struct CadenceClock {
    tuning: CadenceTuning,
    /// `ready − src_pts`, smoothed. Absorbs the clock-domain constant.
    offset_ns: i64,
    /// Per-frame drift of that offset — the integral term.
    skew_ns: i64,
    /// EMA of |residual|, the cushion's input.
    mad_ns: i64,
    /// `None` until the first sample anchors the loop.
    last_pts_ns: Option<u64>,
    /// Last frame interval seen, so [`cushion_ns`](Self::cushion_ns) can apply its ceiling.
    frame_interval_ns: i64,
    health: CadenceHealth,
}

impl CadenceClock {
    pub fn new(tuning: CadenceTuning) -> CadenceClock {
        CadenceClock {
            tuning,
            offset_ns: 0,
            skew_ns: 0,
            mad_ns: 0,
            last_pts_ns: None,
            frame_interval_ns: 0,
            health: CadenceHealth::default(),
        }
    }

    /// Force a re-anchor on the next sample. Call on every discontinuity the client already knows
    /// about: reanchor, codec rebuild, surface recreate, jump-to-live, resume.
    pub fn reset(&mut self) {
        self.last_pts_ns = None;
        self.skew_ns = 0;
        // `mad_ns` deliberately SURVIVES. It describes the link, not the stream, and a cushion
        // that collapsed to its floor at every rebuild would spend the next few hundred frames
        // presenting late — the exact failure the cushion exists to prevent.
    }

    /// Fold one presentable frame and return when it is due, in the present clock domain.
    ///
    /// `ready_ns` is when the frame became presentable; `frame_interval_ns` is the nominal source
    /// interval and the cushion's ceiling.
    ///
    /// The result **may be earlier than `ready_ns`** — that is a late frame, and the caller's
    /// contract is "already due ⇒ present at the next opportunity", never "drag the grid back to
    /// now". Clamping here would quietly turn every late frame into a fresh anchor.
    pub fn due_ns(&mut self, src_pts_ns: u64, ready_ns: i64, frame_interval_ns: i64) -> i64 {
        self.frame_interval_ns = frame_interval_ns;
        self.health.frames += 1;
        let pts = src_pts_ns as i64;
        let raw = ready_ns - pts;

        let anchored = match self.last_pts_ns {
            // Source time going BACKWARDS, or a gap so long the estimate cannot be trusted to
            // have tracked across it: re-anchor rather than slew for seconds.
            Some(last)
                if src_pts_ns < last || src_pts_ns - last > self.tuning.reanchor_gap_ns as u64 =>
            {
                false
            }
            Some(_) => true,
            None => false,
        };
        if anchored {
            // Advance the estimate one frame on the rate term, then correct it by a bounded
            // fraction of what the new sample says.
            self.offset_ns = self.offset_ns.saturating_add(self.skew_ns);
            let err = (raw - self.offset_ns)
                .clamp(-self.tuning.error_clamp_ns, self.tuning.error_clamp_ns);
            self.offset_ns = self
                .offset_ns
                .saturating_add(shr_toward_zero(err, self.tuning.offset_shift));
            self.skew_ns = self
                .skew_ns
                .saturating_add(shr_toward_zero(err, self.tuning.skew_shift));
            let dev = err.abs() - self.mad_ns;
            self.mad_ns += shr_toward_zero(dev, self.tuning.jitter_shift);
        } else {
            self.offset_ns = raw;
            self.skew_ns = 0;
            self.health.reanchors += 1;
        }
        self.last_pts_ns = Some(src_pts_ns);

        let due = pts
            .saturating_add(self.offset_ns)
            .saturating_add(self.cushion_ns());
        if due < ready_ns {
            self.health.late += 1;
        }
        self.publish();
        due
    }

    /// A frame whose timestamp is not on the source cadence — a repeat the host re-anchored at
    /// submit, or a stamp its plausibility gate replaced with "now". Those samples do not lie on
    /// the source's timeline, and folding them in would drag the offset estimate toward "now"
    /// exactly when the stream is idle and the estimate matters most.
    ///
    /// Returns a due time from the CURRENT estimate, leaving offset, skew and jitter untouched:
    /// the frame is simply due once it is ready, cushioned like any other.
    pub fn note_off_cadence(&mut self, ready_ns: i64, frame_interval_ns: i64) -> i64 {
        self.frame_interval_ns = frame_interval_ns;
        ready_ns.saturating_add(self.cushion_ns())
    }

    pub fn jitter_ns(&self) -> i64 {
        self.mad_ns
    }

    /// How far past the estimate a frame is held, to absorb the measured jitter.
    ///
    /// The one-frame-interval ceiling is an INVARIANT, not a tunable: a cushion past a whole frame
    /// buys latency for smoothness the source cannot supply, and at that point the honest fix is a
    /// deeper buffer the user asked for, not a loop quietly holding frames.
    pub fn cushion_ns(&self) -> i64 {
        let den = self.tuning.cushion_den.max(1) as i64;
        let want = self.mad_ns.saturating_mul(self.tuning.cushion_num as i64) / den;
        let ceiling = if self.frame_interval_ns > 0 {
            self.frame_interval_ns
        } else {
            i64::MAX
        };
        want.clamp(self.tuning.cushion_floor_ns.min(ceiling), ceiling)
    }

    pub fn health(&self) -> CadenceHealth {
        let mut h = self.health;
        h.offset_ns = self.offset_ns;
        h.skew_ns = self.skew_ns;
        h.jitter_ns = self.mad_ns;
        h.cushion_ns = self.cushion_ns();
        h
    }

    fn publish(&mut self) {
        self.health.offset_ns = self.offset_ns;
        self.health.skew_ns = self.skew_ns;
        self.health.jitter_ns = self.mad_ns;
    }
}

/// Arithmetic shift that rounds toward ZERO, so a negative residual is damped by exactly as much
/// as its positive twin. A plain `>>` rounds toward −∞, which biases a loop that spends its whole
/// life within a few nanoseconds of zero error.
const fn shr_toward_zero(v: i64, shift: u8) -> i64 {
    if v < 0 {
        -((-v) >> shift)
    } else {
        v >> shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz in ns
    const P_US: u64 = 8_333; // …and in µs, the sample unit

    // ---- CadenceClock (design/presenter-cadence-rework-implementation-plan.md §2.3) --------

    /// A source stamping realtime, played out by a client whose present clock is monotonic and
    /// therefore a whole different era. The loop must never need to be told about this.
    const PTS0: u64 = 1_786_000_000_000_000_000;
    const DOMAIN: i64 = -1_785_000_000_000_000_000;
    /// Transport + decode: what `ready − pts` sits at once the domain is taken out.
    const DELAY: i64 = 12_000_000;

    /// Deterministic LCG in ±spread around zero — no OS randomness in tests.
    struct Lcg(u64);
    impl Lcg {
        fn noise(&mut self, spread_ns: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if spread_ns == 0 {
                return 0;
            }
            ((self.0 >> 33) as i64 % (2 * spread_ns)) - spread_ns
        }
    }

    fn pts_at(k: i64) -> u64 {
        (PTS0 as i64 + k * P) as u64
    }

    /// Run `n` frames of a well-behaved 120 Hz source and hand back the clock.
    fn settled(n: i64, spread_ns: i64) -> CadenceClock {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(7);
        for k in 0..n {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(spread_ns);
            c.due_ns(pts_at(k), ready, P);
        }
        c
    }

    #[test]
    fn settles_from_cold() {
        let c = settled(400, 1_000_000);
        let err = c.health().offset_ns - (DOMAIN + DELAY);
        assert!(
            err.abs() < 500_000,
            "offset must converge on the true transport delay, off by {err} ns"
        );
        assert_eq!(c.health().reanchors, 1, "only the cold start anchors");
    }

    /// The type-2 property, and the reason the loop carries a rate term at all: two free-running
    /// crystals produce a RAMP, and a proportional-only loop lags a ramp forever. Asserted against
    /// its own type-1 twin so the difference is the measurement, not a threshold I chose.
    #[test]
    fn tracks_a_clock_ramp() {
        const RAMP: i64 = 400; // ns per frame ≈ 48 ppm, an ordinary crystal pair
        let run = |tuning: CadenceTuning| -> i64 {
            let mut c = CadenceClock::new(tuning);
            let mut last_err = 0;
            for k in 0..4_000i64 {
                let ready = pts_at(k) as i64 + DOMAIN + DELAY + k * RAMP;
                c.due_ns(pts_at(k), ready, P);
                last_err = (ready - pts_at(k) as i64) - c.health().offset_ns;
            }
            last_err.abs()
        };
        let type2 = run(CadenceTuning::snapping());
        // The same loop with its integral gain switched off: a shift this large truncates every
        // residual to zero, which is exactly "proportional only".
        let type1 = run(CadenceTuning {
            skew_shift: 63,
            ..CadenceTuning::snapping()
        });
        assert!(
            type2 * 4 < type1,
            "a rate term must beat proportional-only on a ramp: {type2} ns vs {type1} ns"
        );
        assert!(type2 < 3_000, "steady-state ramp error {type2} ns");
    }

    #[test]
    fn rejects_a_single_outlier() {
        let mut c = settled(400, 200_000);
        let before = c.health().offset_ns;
        // One frame arrives half a second late — a stall, not a new operating point.
        let k = 400;
        c.due_ns(
            pts_at(k),
            pts_at(k) as i64 + DOMAIN + DELAY + 500_000_000,
            P,
        );
        let moved = (c.health().offset_ns - before).abs();
        // The clamped correction, plus the one frame of rate the loop advances by regardless —
        // that advance is the estimate doing its job, not the outlier moving it.
        let t = CadenceTuning::snapping();
        let bound = (t.error_clamp_ns >> t.offset_shift) + c.health().skew_ns.abs();
        assert!(
            moved <= bound,
            "one outlier moved the estimate {moved} ns, past the clamp's {bound} ns"
        );
    }

    #[test]
    fn reanchors_on_a_gap() {
        let mut c = settled(400, 200_000);
        let anchors = c.health().reanchors;
        // The stream was paused for two seconds; the estimate cannot have tracked across that.
        let far = pts_at(400) + 2_000_000_000;
        let ready = far as i64 + DOMAIN + DELAY + 4_000_000;
        c.due_ns(far, ready, P);
        assert_eq!(c.health().reanchors, anchors + 1);
        assert_eq!(
            c.health().offset_ns,
            ready - far as i64,
            "a re-anchor adopts the new sample outright rather than slewing to it"
        );
    }

    #[test]
    fn reanchors_on_regression() {
        let mut c = settled(400, 200_000);
        let anchors = c.health().reanchors;
        let back = pts_at(200); // source timestamps went backwards
        c.due_ns(back, back as i64 + DOMAIN + DELAY, P);
        assert_eq!(c.health().reanchors, anchors + 1);
    }

    /// A due time in the past is returned AS IS. Clamping it to `ready_ns` would quietly turn
    /// every late frame into a fresh anchor, which is how an arrival-driven presenter behaves —
    /// the thing this clock exists to stop being.
    #[test]
    fn late_frame_returns_past_due() {
        let mut c = settled(400, 200_000);
        let k = 400;
        let ready = pts_at(k) as i64 + DOMAIN + DELAY + 30_000_000; // 30 ms late
        let due = c.due_ns(pts_at(k), ready, P);
        assert!(
            due < ready,
            "a frame that arrived 30 ms late must read as already due"
        );
        assert_eq!(c.health().late, 1);
    }

    #[test]
    fn off_cadence_does_not_move_the_loop() {
        let mut c = settled(400, 500_000);
        let before = c.health();
        let due = c.note_off_cadence(1_000_000, P);
        let after = c.health();
        assert_eq!(before.offset_ns, after.offset_ns);
        assert_eq!(before.skew_ns, after.skew_ns);
        assert_eq!(before.jitter_ns, after.jitter_ns);
        assert_eq!(
            before.frames, after.frames,
            "and it is not a cadence sample"
        );
        assert_eq!(due, 1_000_000 + c.cushion_ns());
    }

    /// One domain in, same domain out: shifting the whole present-side trace by an arbitrary
    /// constant must change every due time by exactly that constant and nothing else. This is
    /// what lets each client feed its own clock without a conversion in the path.
    #[test]
    fn domain_offset_is_absorbed() {
        const SHIFT: i64 = 987_654_321_000;
        let run = |extra: i64| -> Vec<i64> {
            let mut c = CadenceClock::new(CadenceTuning::snapping());
            let mut rng = Lcg(11);
            (0..300i64)
                .map(|k| {
                    let ready = pts_at(k) as i64 + DOMAIN + DELAY + extra + rng.noise(2_000_000);
                    c.due_ns(pts_at(k), ready, P)
                })
                .collect()
        };
        let a = run(0);
        let b = run(SHIFT);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(y - x, SHIFT, "frame {i} shifted by {} not {SHIFT}", y - x);
        }
    }

    /// The invariant that separates this from a metronome: a source that genuinely runs at an
    /// irregular rate is REPRODUCED, not evened out. Anything that made these due spacings more
    /// uniform than the source's own would be a bug.
    #[test]
    fn preserves_source_cadence() {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        // A deliberately lumpy source: alternating short and long frames.
        let spacings: Vec<i64> = (0..300)
            .map(|k| if k % 2 == 0 { P / 2 } else { P * 3 / 2 })
            .collect();
        let mut pts = PTS0;
        let mut dues = Vec::new();
        let mut ptss = Vec::new();
        let mut rng = Lcg(13);
        for &s in &spacings {
            pts = (pts as i64 + s) as u64;
            let ready = pts as i64 + DOMAIN + DELAY + rng.noise(500_000);
            ptss.push(pts as i64);
            dues.push(c.due_ns(pts, ready, P));
        }
        // Compare the back half, once the loop has settled.
        for i in 200..dues.len() {
            let d_due = dues[i] - dues[i - 1];
            let d_pts = ptss[i] - ptss[i - 1];
            assert!(
                (d_due - d_pts).abs() < 200_000,
                "due spacing {d_due} must follow the source's {d_pts}"
            );
        }
    }

    #[test]
    fn cushion_respects_ceiling() {
        let mut c = CadenceClock::new(CadenceTuning::free_running());
        let mut rng = Lcg(17);
        // Jitter far wider than a frame — the cushion must still never exceed one interval.
        for k in 0..500i64 {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(40_000_000);
            c.due_ns(pts_at(k), ready, P);
            assert!(
                c.cushion_ns() <= P,
                "cushion {} ns exceeded the frame interval",
                c.cushion_ns()
            );
        }
        assert!(
            c.jitter_ns() > P,
            "the harness must actually have stressed it"
        );
    }

    // ---- Offline sim (§2.4): the real clock, replayed, against today's rule ----------------
    //
    // R7, the phase-lock v3 lesson: the harness imports the REAL type. A paraphrase once
    // "confirmed" a non-bug and cost a session.

    /// Judder as WP1 defines it: round each consecutive present spacing to whole panel periods,
    /// then report the ‰ of intervals that are not the modal count.
    fn judder_permille(presents: &[i64], panel_ns: i64) -> u32 {
        let counts: Vec<i64> = presents
            .windows(2)
            .map(|w| (w[1] - w[0] + panel_ns / 2) / panel_ns)
            .collect();
        if counts.is_empty() {
            return 0;
        }
        let mode = *counts
            .iter()
            .max_by_key(|c| counts.iter().filter(|x| x == c).count())
            .unwrap();
        let off = counts.iter().filter(|c| **c != mode).count();
        (off * 1000 / counts.len()) as u32
    }

    /// Turn a series of target instants into the refreshes a frame actually reaches glass on.
    ///
    /// Newest-wins, which is what both presenters really do: a frame whose slot is already
    /// claimed REPLACES the one aimed there rather than being delayed to the next refresh.
    /// Delaying it instead would ratchet — one clamp puts the sequence permanently ahead of its
    /// own targets and every later frame clamps too, producing a flawless metronome out of an
    /// arbitrarily jittery input, and scoring zero judder for both rules.
    fn present_slots(targets: &[i64], panel_ns: i64) -> Vec<i64> {
        let mut out: Vec<i64> = Vec::new();
        for &t in targets {
            let slot = (t + panel_ns - 1) / panel_ns * panel_ns;
            if out.last().is_none_or(|&last| slot > last) {
                out.push(slot);
            }
        }
        out
    }

    #[test]
    fn the_clock_beats_arrival_presentation_on_a_jittery_link() {
        // ±6 ms — the 2026-08-15 field shape, where KWin's screencast arrival offsets ran
        // 0.11–8.22 ms against an 8.33 ms period. Jitter much narrower than the panel period is
        // the case snapping absorbs on its own (and, at a lucky grid phase, absorbs entirely),
        // which is precisely why the reported host is the one worth simulating.
        const JITTER: i64 = 6_000_000;
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(23);
        let (mut arrival, mut cadence) = (Vec::new(), Vec::new());
        for k in 0..1_200i64 {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(JITTER);
            let due = c.due_ns(pts_at(k), ready, P);
            if k > 200 {
                // Today: aim at the frame the moment it is decoded. With the clock: aim at its
                // due time, and never before the frame exists.
                arrival.push(ready);
                cadence.push(ready.max(due));
            }
        }
        let ja = judder_permille(&present_slots(&arrival, P), P);
        let jc = judder_permille(&present_slots(&cadence, P), P);
        // The expected-gain figure §2.4 asks this harness to produce. `cargo test -- --nocapture`.
        println!(
            "sim: arrival {ja}‰ → cadence {jc}‰ (cushion {} ns)",
            c.cushion_ns()
        );
        assert!(
            jc < ja / 2,
            "source-timestamp playout must materially beat arrival: {jc}‰ vs {ja}‰"
        );
    }

    /// …and it must NOT "win" by flattening a source that is genuinely uneven — the same harness,
    /// a variable-rate source, and the clock is expected to reproduce its lumpiness.
    #[test]
    fn the_sim_does_not_reward_flattening_a_variable_source() {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(29);
        let mut pts = PTS0;
        let (mut dues, mut ptss) = (Vec::new(), Vec::new());
        for k in 0..600i64 {
            // A renderer alternating 60 and 120 fps work — real, and not a defect.
            pts = (pts as i64 + if k % 3 == 0 { 2 * P } else { P }) as u64;
            let ready = pts as i64 + DOMAIN + DELAY + rng.noise(500_000);
            ptss.push(pts as i64);
            dues.push(c.due_ns(pts, ready, P));
        }
        let jd = judder_permille(&dues[300..], P);
        let jp = judder_permille(&ptss[300..], P);
        assert_eq!(
            jd, jp,
            "the due-time cadence must score exactly what the source's own does"
        );
    }

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
