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
}
