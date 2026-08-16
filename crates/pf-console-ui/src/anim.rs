//! The console shell's motion vocabulary. Two kinds of movement, deliberately kept apart:
//!
//! **Springs** ([`Spring`], wrapping `library::spring_advance`) for anything the user
//! pushes around — cursors, trays, recoil, and now the screen transitions themselves —
//! where velocity must carry across a retarget. [`SpringSpec`] and the [`springs`] table
//! are how a feel is named rather than spelled out as a `k`/`c` pair.
//!
//! **Timed choreography** ([`Entrance`] + the easing functions) for the fire-and-forget
//! kind, where the shape over a fixed window matters more than momentum and there is
//! nothing to interrupt. An entrance is a pure function of the clock, so a screen holds
//! one and asks it per item instead of keeping per-item state.
//!
//! There used to be a general-purpose `Progress` timer here. It went when the screen
//! transition — its last caller — became a spring: reversing a timer mid-flight means
//! either a snap or a second animation, which is the whole reason that work happened.

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

    /// Screen push/pop. Just under critical — a whole screen that visibly bounces reads as
    /// broken rather than alive — but sprung rather than tweened because the velocity has
    /// to carry: a Back pressed mid-push retargets this spring to 0 and the screen turns
    /// around where it is, which a time-based curve cannot do without snapping.
    pub(crate) const NAV: SpringSpec = SpringSpec {
        response: 0.42,
        damping: 0.88,
    };
    /// Row and tile focus. Deliberately the loosest of the table: the whisker of overshoot
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

// --- Entrance choreography ------------------------------------------------------------

/// Ease-out-back: crosses 1.0 near the end and settles back onto it, so an arriving card
/// reads as THROWN into place rather than slid. `c1` is 1.2 rather than the CSS-standard
/// 1.70158 — at full strength the overshoot reads as a bounce, which is a different (and
/// sillier) gesture.
pub(crate) fn ease_out_back(t: f64) -> f64 {
    const C1: f64 = 1.2;
    const C3: f64 = C1 + 1.0;
    let u = t.clamp(0.0, 1.0) - 1.0;
    1.0 + C3 * u * u * u + C1 * u * u
}

/// The share of an item's window spent fading in. Short on purpose: the card is solid
/// well before it stops moving, so what you read is the motion and not a dissolve.
const FADE_SHARE: f64 = 0.34;

/// How a staggered entrance is shaped.
#[derive(Clone, Copy)]
pub(crate) struct EntranceSpec {
    /// How long ONE item takes to arrive.
    pub window: f64,
    /// Delay added per step of distance from the anchor.
    pub stagger: f64,
    /// Ceiling on that delay. Without it a 400-title shelf would still be arriving a
    /// minute later; with it, everything past `cap / stagger` items away starts together —
    /// five or six, for the specs below. Which is why the two move as a pair: that ratio IS
    /// the number of steps anyone ever sees fan, so raising `stagger` alone buys a wider
    /// offset across fewer steps and lands the far half of a shelf in one block.
    pub cap: f64,
}

pub(crate) mod entrances {
    use super::EntranceSpec;

    /// Carousel and coverflow cards — the loud one, and the reason this exists.
    ///
    /// The stagger is measured against a card's VISIBLE life, not against `window`: the fade
    /// is over at `FADE_SHARE` and [`super::ease_out_back`] is already at 0.89 by that same
    /// point, so the last two thirds of the window is a crawl nobody can see. What is left is
    /// ~0.2 s of readable action, and a neighbour starting a little past halfway through it is
    /// what makes a strip arrive as a sequence rather than as one soft event. Judged against
    /// the whole 0.6 s a stagger half this size looks generous; judged against the 0.2 s that
    /// reads it is four frames, and since every surface here culls to a handful of items,
    /// four frames is the whole event and not the gap between two of its steps.
    pub(crate) const CARDS: EntranceSpec = EntranceSpec {
        window: 0.6,
        stagger: 0.12,
        cap: 0.6,
    };
    /// Menu rows. Same language, deliberately quieter: a settings list that fans open like
    /// a shelf of box art is a settings list showing off. Quieter still has to be countable,
    /// though — under about three frames apart the rows read as one soft arrival rather than
    /// as a ripple — so this is a shorter offset, not an absent one.
    pub(crate) const ROWS: EntranceSpec = EntranceSpec {
        window: 0.42,
        stagger: 0.055,
        cap: 0.33,
    };
}

/// One item's place in an entrance.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct EntranceAt {
    /// 0 → 1 travel on [`ease_out_back`]. The SCREEN decides what travels (a card's
    /// scale, its Y-turn, a row's rise) — this only says how far along it is.
    pub travel: f64,
    /// 0 → 1 opacity.
    pub fade: f64,
}

impl EntranceAt {
    /// At rest and fully opaque — what every item reads once the entrance is over.
    pub(crate) const SETTLED: EntranceAt = EntranceAt {
        travel: 1.0,
        fade: 1.0,
    };
}

/// The staggered card entrance, as a pure function of the shell clock: a screen holds ONE
/// of these and asks it per item, so there is no per-card state to keep in step with a
/// list that churns under discovery.
///
/// Armed once per screen MOUNT (the first frame after a push), not per frame-loop — the
/// desktop equivalent of arming on `onAppear`.
#[derive(Clone, Copy)]
pub(crate) struct Entrance {
    spec: EntranceSpec,
    anchor: usize,
    t0: f64,
    /// Snapshotted when the entrance is armed rather than read per item, so one entrance
    /// plays one way even if the setting is stepped while it runs.
    reduced: bool,
}

impl Entrance {
    /// Arm at `t0`, fanning out from `anchor` — which callers pass as the CURSOR, so a
    /// restored selection assembles around the eye instead of sweeping in from a corner.
    pub(crate) fn new(spec: EntranceSpec, anchor: usize, t0: f64) -> Entrance {
        Entrance {
            spec,
            anchor,
            t0,
            reduced: crate::theme::reduce_motion(),
        }
    }

    pub(crate) fn at(&self, i: usize, t: f64) -> EntranceAt {
        let elapsed = t - self.t0;
        if self.reduced {
            // A plain crossfade: no travel, no stagger, everything together.
            return EntranceAt {
                travel: 1.0,
                fade: (elapsed / self.spec.window).clamp(0.0, 1.0),
            };
        }
        let delay = (self.anchor.abs_diff(i) as f64 * self.spec.stagger).min(self.spec.cap);
        let w = ((elapsed - delay) / self.spec.window).clamp(0.0, 1.0);
        EntranceAt {
            travel: ease_out_back(w),
            fade: ease_out_cubic(w / FADE_SHARE),
        }
    }

    /// Has every item landed? The farthest one starts at `cap` and takes `window`, so the
    /// whole thing is over at their sum whatever `len` is — which is why callers can drop
    /// the entrance entirely (and stop paying for its transforms) without counting items.
    pub(crate) fn done(&self, t: f64) -> bool {
        t - self.t0 >= self.spec.cap + self.spec.window
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

    /// The entrance envelope's four load-bearing properties. Asserted on FADE rather than
    /// travel wherever "further along" is the question: travel rides an ease-out-BACK, so
    /// it crosses 1.0 and comes back, and a card at 80 % of its window can legitimately be
    /// further displaced than one that has already landed. Fade is the monotone channel.
    #[test]
    fn entrance_envelope() {
        let e = Entrance::new(entrances::CARDS, 5, 0.0);

        // The anchor leads. It is the cursor, so the strip assembles around the eye.
        for t in [0.05, 0.2, 0.4, 0.7, 1.1] {
            let anchor = e.at(5, t).fade;
            for i in 0..14 {
                assert!(
                    e.at(i, t).fade <= anchor + 1e-9,
                    "item {i} beat the anchor at t={t}"
                );
            }
            // …and symmetric neighbours arrive together.
            assert_eq!(e.at(3, t), e.at(7, t));
        }

        // The cap holds: past `cap / stagger` steps out — five, for CARDS — everything starts
        // at once. This is what keeps a 400-title shelf from still arriving a minute later.
        // Derived rather than spelled out, so re-tuning the pair re-aims the probe with it.
        let beyond = (entrances::CARDS.cap / entrances::CARDS.stagger).ceil() as usize + 1;
        assert_eq!(e.at(5 + beyond, 0.3), e.at(5 + 250, 0.3));

        // Monotone once started, and never outside 0..=1.
        let mut last = 0.0;
        for step in 0..=120 {
            let at = e.at(9, f64::from(step) * 0.01);
            assert!(at.fade >= last - 1e-9, "fade went backwards at step {step}");
            assert!((0.0..=1.0).contains(&at.fade));
            last = at.fade;
        }

        // Identity at the end — the whole thing is over at cap + window, whatever `len` is,
        // which is what lets a screen drop the entrance instead of counting items.
        let over = entrances::CARDS.cap + entrances::CARDS.window;
        assert!(e.done(over));
        assert!(!e.done(over - 0.01));
        for i in [5, 9, 400] {
            assert_eq!(e.at(i, over), EntranceAt::SETTLED, "item {i} never landed");
        }
    }

    /// Whether the strip reads as a SEQUENCE, which none of `entrance_envelope`'s shape
    /// properties can see: a spec with `stagger` at zero satisfies every one of them and
    /// arrives as a single soft event. Three numbers decide it, all derived from the specs
    /// so that a re-tune which quietly undoes the fan fails here rather than on a couch.
    #[test]
    fn entrance_stagger_reads_as_a_sequence() {
        // A neighbour must still be visibly behind while the anchor is halfway through its
        // FADE — `window` flatters the stagger badly, because an item is perceptually done a
        // third of the way through it, so this is measured against the part that reads.
        let separation = |spec: EntranceSpec| {
            let e = Entrance::new(spec, 5, 0.0);
            let t_mid = 0.5 * FADE_SHARE * spec.window;
            e.at(5, t_mid).fade - e.at(6, t_mid).fade
        };
        let cards = separation(entrances::CARDS);
        assert!(cards > 0.7, "CARDS neighbours arrive together: {cards}");
        let rows = separation(entrances::ROWS);
        assert!(rows > 0.5, "ROWS is quieter, not staggerless: {rows}");

        for (name, spec, budget) in [
            ("CARDS", entrances::CARDS, 1.25),
            ("ROWS", entrances::ROWS, 0.8),
        ] {
            // `cap / stagger` is how many steps ever fan, and every surface culls to a
            // handful of items — the coverflow shows five. Let this drop and a wider
            // `stagger` buys a bigger offset across fewer steps, which is worse, not better.
            let steps = spec.cap / spec.stagger;
            assert!(steps >= 4.0, "{name} fans only {steps} steps");
            // The other end: "too long" should be a failing test rather than an opinion. The
            // item under the cursor is untouched by both — its delay is 0 — so this budget
            // buys peripheral polish and never makes anyone wait.
            let total = spec.cap + spec.window;
            assert!(total <= budget, "{name} takes {total} s to retire");
        }
    }

    /// Ease-out-back must overshoot — that is the difference between a card being thrown
    /// into place and slid there — and must still land exactly on 1.0.
    #[test]
    fn ease_out_back_overshoots_then_lands() {
        // To a tolerance at 0, not exactly: the polynomial is `1 − 2.2 + 1.2`, which is
        // zero in real arithmetic and −2.2e−16 in f64. The contract is "starts where the
        // card starts", and a fifth of a femto-unit is that.
        assert!(ease_out_back(0.0).abs() < 1e-12);
        assert_eq!(ease_out_back(1.0), 1.0);
        assert_eq!(ease_out_back(2.0), 1.0, "clamped");
        let peak = (0..=100)
            .map(|i| ease_out_back(f64::from(i) / 100.0))
            .fold(f64::MIN, f64::max);
        assert!(peak > 1.0, "no overshoot at all: {peak}");
        assert!(peak < 1.12, "overshoot reads as a bounce: {peak}");
    }

    /// Reduced motion turns the entrance into a plain crossfade: no travel, no stagger.
    /// Snapshotted at arm time, so one entrance plays one way even if the setting moves.
    #[test]
    fn entrance_under_reduced_motion_is_a_staggerless_crossfade() {
        crate::theme::set_reduce_motion(true);
        let e = Entrance::new(entrances::CARDS, 5, 0.0);
        crate::theme::set_reduce_motion(false);
        for t in [0.0, 0.1, 0.3, 0.6] {
            let a = e.at(5, t);
            assert_eq!(a.travel, 1.0, "nothing travels");
            assert_eq!(a, e.at(200, t), "and nothing staggers");
        }
        assert_eq!(e.at(0, 0.6), EntranceAt::SETTLED);
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
        let nav = peak_overshoot(springs::NAV);
        assert!(nav < 0.01, "NAV must not visibly overshoot, got {nav}");
        assert!(
            peak_overshoot(springs::PRESS) > focus,
            "PRESS is the loosest of the table"
        );
    }
}
