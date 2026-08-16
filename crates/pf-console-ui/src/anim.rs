//! The console shell's motion vocabulary. Two kinds of movement, deliberately kept
//! apart: **springs** (`Spring`, wrapping `library::spring_advance`) for anything the
//! user pushes around — cursors, trays, recoil — where velocity must carry across
//! retargets; and **timed progressions** (`Progress` + the easing functions) for
//! fire-and-forget choreography — screen entrances/exits, fades — where a deterministic
//! duration matters more than momentum.

use crate::library::spring_advance;

/// Ease-out cubic — fast start, gentle landing. The screen-transition curve (the WinUI
/// shell's entrance tween uses the same shape).
pub(crate) fn ease_out_cubic(t: f64) -> f64 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// Exponential approach: move `current` toward `target` with time-constant `tau`
/// seconds. Frame-rate independent, never overshoots — the focus-scale smoothing
/// (SwiftUI's `.smooth(0.18)` reads the same).
pub(crate) fn approach(current: f64, target: f64, dt: f64, tau: f64) -> f64 {
    current + (target - current) * (1.0 - (-dt / tau).exp())
}

/// A spring named the way a designer reasons about one: `response` is the period of a
/// full undamped oscillation in seconds ("how long does it take to get there"), `damping`
/// is ζ (1.0 lands dead, below it overshoots). The integrator wants `k`/`c` instead, and
/// [`SpringSpec::kc`] is the conversion — the same one SwiftUI's
/// `.spring(response:dampingFraction:)` performs.
#[derive(Clone, Copy)]
pub(crate) struct SpringSpec {
    pub response: f64,
    pub damping: f64,
}

impl SpringSpec {
    /// `(k, c)` for [`Spring::step`], from `ω = 2π/response`: `k = ω²`, `c = 2ζω`.
    ///
    /// `const` so [`springs`] costs nothing at runtime — which is also why it is written
    /// with `ω` rather than `2ζ√k`: `√k` IS `ω`, and `f64::sqrt` is not callable in a
    /// const context. Same numbers, no root.
    pub(crate) const fn kc(self) -> (f64, f64) {
        let w = std::f64::consts::TAU / self.response;
        (w * w, 2.0 * self.damping * w)
    }
}

/// The console's motion vocabulary: feel is chosen from this table rather than from
/// constants scattered through the widgets, so "the focus pop" is one name with one
/// definition instead of a τ someone picked in a hurry.
///
/// These are the SHELL's springs. The carousel's own pairs (`library::SPRING_K/C`,
/// `BUMP_K/C`) deliberately stay raw: they are tuned numbers shared with the GTK
/// launcher's math, and restating them as specs would invite a "tidy-up" that changes the
/// coverflow's feel on both surfaces at once.
pub(crate) mod springs {
    use super::SpringSpec;

    /// Row and tile focus. Deliberately the loosest of the three: the whisker of overshoot
    /// IS the pop that makes a focused row feel picked up rather than merely tinted.
    pub(crate) const FOCUS: SpringSpec = SpringSpec {
        response: 0.30,
        damping: 0.80,
    };
    /// The tab pill and the keyboard tray. One spec because they are one gesture — a
    /// single object gliding to a new seat — and it is the pair [`TRAY_K`]/[`TRAY_C`]
    /// already encode (see `spring_spec_matches_the_tray_constants`).
    pub(crate) const INDICATOR: SpringSpec = SpringSpec {
        response: 0.32,
        damping: 0.86,
    };
    /// The confirm dip. Fast and loose, so a press reads as a press and not as a fade.
    pub(crate) const PRESS: SpringSpec = SpringSpec {
        response: 0.18,
        damping: 0.65,
    };
}

/// A damped spring with persistent velocity. `k`/`c` choose the feel; see the pairs in
/// [`crate::library`] (cursor chase, boundary bump) and [`TRAY_K`]/[`TRAY_C`] below.
#[derive(Clone, Copy)]
pub(crate) struct Spring {
    pub pos: f64,
    pub vel: f64,
}

impl Spring {
    pub(crate) fn rest(pos: f64) -> Spring {
        Spring { pos, vel: 0.0 }
    }

    pub(crate) fn step(&mut self, target: f64, k: f64, c: f64, dt: f64) {
        (self.pos, self.vel) = spring_advance(self.pos, self.vel, target, k, c, dt);
    }

    /// [`Spring::step`] with the feel named instead of spelled out.
    pub(crate) fn step_spec(&mut self, target: f64, spec: SpringSpec, dt: f64) {
        let (k, c) = spec.kc();
        self.step(target, k, c, dt);
    }

    /// Snap onto `target` once the motion is imperceptible (stops per-frame damage).
    pub(crate) fn settle(&mut self, target: f64, eps_pos: f64, eps_vel: f64) {
        if (target - self.pos).abs() < eps_pos && self.vel.abs() < eps_vel {
            self.pos = target;
            self.vel = 0.0;
        }
    }
}

/// The keyboard tray's slide (SwiftUI `.spring(response: 0.32, dampingFraction: 0.86)`:
/// k = (2π/response)², c = 2·ζ·√k).
pub(crate) const TRAY_K: f64 = 385.0;
pub(crate) const TRAY_C: f64 = 33.7;

/// A clamped 0→1 timer for fire-and-forget choreography. `advance` returns the RAW
/// progress — callers apply their easing so one Progress can drive several curves.
#[derive(Clone, Copy)]
pub(crate) struct Progress {
    t: f64,
    duration: f64,
}

impl Progress {
    pub(crate) fn new(duration: f64) -> Progress {
        Progress { t: 0.0, duration }
    }

    pub(crate) fn advance(&mut self, dt: f64) -> f64 {
        self.t = (self.t + dt / self.duration).min(1.0);
        self.t
    }

    pub(crate) fn value(&self) -> f64 {
        self.t
    }

    pub(crate) fn done(&self) -> bool {
        self.t >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5, "front-loaded");
        assert_eq!(ease_out_cubic(2.0), 1.0, "clamped");
    }

    #[test]
    fn approach_converges_and_never_overshoots() {
        let mut v = 0.0;
        for _ in 0..120 {
            v = approach(v, 1.0, 1.0 / 60.0, 0.06);
            assert!(v <= 1.0);
        }
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn progress_completes_on_time() {
        let mut p = Progress::new(0.3);
        let mut steps = 0;
        while !p.done() {
            p.advance(1.0 / 60.0);
            steps += 1;
        }
        assert!((17..=19).contains(&steps), "{steps}"); // 0.3 s at 60 Hz
    }

    #[test]
    fn spring_settles() {
        let mut s = Spring::rest(0.0);
        for _ in 0..240 {
            s.step(1.0, 200.0, 24.0, 1.0 / 60.0);
            s.settle(1.0, 0.001, 0.01);
        }
        assert_eq!((s.pos, s.vel), (1.0, 0.0));
    }

    /// The conversion, pinned against the one pair that predates the table. `TRAY_K`/`TRAY_C`
    /// were hand-derived from `.spring(response: 0.32, dampingFraction: 0.86)` and written
    /// ROUNDED (385.0/33.7 against an exact 385.53/33.77), so this asserts to 0.3 % rather
    /// than exactly — the point is that the arithmetic in `kc` is the arithmetic those
    /// constants came from, not that someone re-typed more digits.
    #[test]
    fn spring_spec_matches_the_tray_constants() {
        let (k, c) = springs::INDICATOR.kc();
        assert!(
            (k - TRAY_K).abs() / TRAY_K < 0.003,
            "k: spec says {k}, TRAY_K is {TRAY_K}"
        );
        assert!(
            (c - TRAY_C).abs() / TRAY_C < 0.003,
            "c: spec says {c}, TRAY_C is {TRAY_C}"
        );
    }

    /// Peak of a unit step as a fraction over the target — 0.0 when the spring never
    /// crosses. Integrated at 120 Hz so the measurement is the SPRING's shape and not the
    /// sampling's.
    fn peak_overshoot(spec: SpringSpec) -> f64 {
        let mut s = Spring::rest(0.0);
        let mut peak: f64 = 0.0;
        for _ in 0..600 {
            s.step_spec(1.0, spec, 1.0 / 120.0);
            peak = peak.max(s.pos);
        }
        (peak - 1.0).max(0.0)
    }

    /// The table's damping choices, stated as behaviour rather than as numbers: FOCUS is
    /// under-damped ENOUGH to read as a pop, INDICATOR is damped enough that a gliding pill
    /// doesn't wobble at the end of its travel. A future re-tune that breaks either breaks
    /// this.
    #[test]
    fn spec_damping_choices_show_up_as_overshoot() {
        let focus = peak_overshoot(springs::FOCUS);
        assert!(focus > 0.005, "FOCUS must visibly overshoot, got {focus}");
        let indicator = peak_overshoot(springs::INDICATOR);
        assert!(
            indicator < 0.01,
            "INDICATOR must not visibly overshoot, got {indicator}"
        );
        assert!(
            peak_overshoot(springs::PRESS) > focus,
            "PRESS is the loosest of the table"
        );
    }
}
