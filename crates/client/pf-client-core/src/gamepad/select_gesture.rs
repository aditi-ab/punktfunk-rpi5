//! Hold-Select→Guide and the Select chords: pure state over button transitions and a clock.

use punktfunk_core::input::gamepad as wire;
use std::time::{Duration, Instant};

/// Hold Select alone this long and it is the host's Guide ([`SelectGesture`]). The physical
/// Guide never reaches the host cleanly where the local shell owns it.
pub(super) const GUIDE_HOLD: Duration = Duration::from_millis(350);

/// A lone Select held this long sends Guide while still held. Until then an exit-chord partner
/// may still join: 1 s covers pressing Select first, then reaching for the shoulders.
pub(super) const GUIDE_COMMIT: Duration = Duration::from_millis(1000);

/// The exit chord's buttons besides Select (the worker's `ESCAPE_CHORD`).
pub(super) const EXIT_PARTNERS: [u32; 3] = [wire::BTN_LB, wire::BTN_RB, wire::BTN_START];

/// Delay between a held-back Select tap's press and its scheduled release. Core folds
/// per-transition sends into one `GamepadState`; down+up in one window vanish.
pub(super) const TAP_PRESS: Duration = Duration::from_millis(50);

/// What a Select+button chord on a forwarded pad asks the client for.
///
/// [`Ring`](Self::Ring) withholds the press it takes, because A is the dial's own confirm. That
/// is why the client says whether anything is listening
/// ([`GamepadService::set_chords_live`](super::GamepadService::set_chords_live)): a chord
/// nothing receives would eat a face button the game was owed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectChord {
    /// Select+A — open the quick-action dial. The A press is withheld.
    Ring,
    /// Select+X — step the stats overlay's tier. Both presses still reach the game.
    Stats,
}

/// What a button pressed with Select already held asks for, whether or not the guide gesture
/// is on. Select-first only: A is the ring's own confirm, and X is a face button a game wants.
/// Once the hold became Guide, both belong to whatever that opened on the host.
pub(super) fn select_chord(held: &[u32], bit: u32, select_as_guide: bool) -> Option<SelectChord> {
    if select_as_guide || !held.contains(&wire::BTN_BACK) {
        return None;
    }
    match bit {
        wire::BTN_A => Some(SelectChord::Ring),
        wire::BTN_X => Some(SelectChord::Stats),
        _ => None,
    }
}

/// Hold-Select→Guide. Pure: transitions + clock emit `(bit, down)` pairs. Select pressed
/// alone is pending; with another button already down it passes through. Pending ends on
/// the first of:
/// - a press: a real Select (deferred down first), or past [`GUIDE_HOLD`] Guide, held until
///   release. An [`EXIT_PARTNERS`] press is always a real Select, so no exit chord is Guide.
/// - release: a tap of Guide past [`GUIDE_HOLD`], else of Select — press now, up
///   [`TAP_PRESS`] later (back-to-back folds into nothing).
/// - [`GUIDE_COMMIT`]: Guide, held until release.
#[derive(Default)]
pub(super) struct SelectGesture {
    pending_since: Option<Instant>,
    as_guide: bool,
    /// A tap's owed release: when, and of which bit.
    release_due: Option<(Instant, u32)>,
    /// Ring chord: the pending press never went out, so the release is dropped here.
    swallowed: bool,
}

impl SelectGesture {
    /// Returns true when the press is held back. `alone` = no other button held.
    pub(super) fn on_select_down(
        &mut self,
        now: Instant,
        alone: bool,
        out: &mut Vec<(u32, bool)>,
    ) -> bool {
        // A previous tap's scheduled release is still owed: lift it before the new press.
        if let Some((_, bit)) = self.release_due.take() {
            out.push((bit, false));
        }
        if alone {
            self.pending_since = Some(now);
            return true;
        }
        false
    }

    /// Select is the host's Guide, on the wire or still pending past [`GUIDE_HOLD`].
    pub(super) fn is_guide(&self, now: Instant) -> bool {
        self.as_guide || self.pending_past(now, GUIDE_HOLD)
    }

    fn pending_past(&self, now: Instant, hold: Duration) -> bool {
        self.pending_since
            .is_some_and(|t| now.saturating_duration_since(t) >= hold)
    }

    /// Ring chord: a pending Select never goes out, so its later release is dropped too.
    pub(super) fn swallow_for_ring(&mut self) {
        if self.pending_since.take().is_some() {
            self.swallowed = true;
        }
    }

    /// A press ends a pending Select ([`SelectGesture`]); its down goes out before the new
    /// button's.
    pub(super) fn on_other_down(&mut self, bit: u32, now: Instant, out: &mut Vec<(u32, bool)>) {
        let guide = self.pending_past(now, GUIDE_HOLD) && !EXIT_PARTNERS.contains(&bit);
        if self.pending_since.take().is_some() {
            self.as_guide = guide;
            let select = if guide {
                wire::BTN_GUIDE
            } else {
                wire::BTN_BACK
            };
            out.push((select, true));
        }
    }

    /// True when the gesture owned this release (caller skips the normal button-up).
    pub(super) fn on_select_up(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) -> bool {
        if self.swallowed {
            self.swallowed = false;
            return true;
        }
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
            return true;
        }
        if self.pending_since.is_some() {
            let bit = if self.is_guide(now) {
                wire::BTN_GUIDE
            } else {
                wire::BTN_BACK
            };
            self.pending_since = None;
            out.push((bit, true));
            self.release_due = Some((now + TAP_PRESS, bit));
            return true;
        }
        false
    }

    pub(super) fn poll(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) {
        if self.pending_past(now, GUIDE_COMMIT) {
            self.pending_since = None;
            self.as_guide = true;
            out.push((wire::BTN_GUIDE, true));
        }
        if let Some((due, bit)) = self.release_due {
            if now >= due {
                self.release_due = None;
                out.push((bit, false));
            }
        }
    }

    /// Slot close / disarm: nothing may stay down (or owed) on the wire.
    pub(super) fn flush(&mut self, out: &mut Vec<(u32, bool)>) {
        self.pending_since = None;
        self.swallowed = false;
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
        }
        if let Some((_, bit)) = self.release_due.take() {
            out.push((bit, false));
        }
    }
}

#[cfg(test)]
mod select_gesture_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_while_select_is_pending_swallows_both() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        let t = Instant::now();
        assert!(g.on_select_down(t, true, &mut out));
        g.swallow_for_ring();
        assert!(out.is_empty(), "the pending press stays swallowed: {out:?}");
        assert!(
            g.on_select_up(t, &mut out),
            "the release is owned, not forwarded"
        );
        assert!(out.is_empty(), "…and emits nothing: {out:?}");
    }

    #[test]
    fn select_then_a_or_x_is_a_chord_without_the_guide_gesture() {
        let held = |b: u32| select_chord(&[wire::BTN_BACK], b, false);
        assert_eq!(held(wire::BTN_A), Some(SelectChord::Ring));
        assert_eq!(held(wire::BTN_X), Some(SelectChord::Stats));
        // Every other face button is the game's, held Select or not.
        assert_eq!(held(wire::BTN_B), None);
        assert_eq!(held(wire::BTN_Y), None);
        assert_eq!(
            select_chord(&[wire::BTN_LB, wire::BTN_BACK], wire::BTN_A, false),
            Some(SelectChord::Ring),
            "other buttons held alongside do not disqualify it"
        );
        assert_eq!(select_chord(&[], wire::BTN_A, false), None, "A alone");
        assert_eq!(
            select_chord(&[wire::BTN_A], wire::BTN_BACK, false),
            None,
            "A first"
        );
        assert_eq!(
            select_chord(&[wire::BTN_BACK], wire::BTN_X, true),
            None,
            "Select became Guide"
        );
    }

    #[test]
    fn tap_delivers_press_then_scheduled_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out), "not held back");
        assert!(out.is_empty(), "a held-back press sends nothing yet");
        let up = t + Duration::from_millis(120);
        assert!(g.on_select_up(up, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        g.poll(up + TAP_PRESS - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "release went out early");
        g.poll(up + TAP_PRESS, &mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn hold_becomes_guide_down_until_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_COMMIT - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "guide went out while a chord could follow");
        assert!(g.is_guide(t + GUIDE_HOLD), "past the threshold it is Guide");
        g.poll(t + GUIDE_COMMIT, &mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, true)]);
        out.clear();
        g.poll(t + GUIDE_COMMIT * 4, &mut out);
        assert!(out.is_empty());
        assert!(g.on_select_up(t + GUIDE_COMMIT * 5, &mut out));
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
    }

    /// Past the threshold, what joins decides: an exit-chord partner keeps Select real, any
    /// other press is Guide's, and a release taps Guide.
    #[test]
    fn past_the_threshold_guide_waits_for_the_next_move() {
        let t = Instant::now();
        let late = t + GUIDE_HOLD;
        let rows = [
            ("Start", Some(wire::BTN_START), vec![(wire::BTN_BACK, true)]),
            ("L1", Some(wire::BTN_LB), vec![(wire::BTN_BACK, true)]),
            ("R1", Some(wire::BTN_RB), vec![(wire::BTN_BACK, true)]),
            ("A", Some(wire::BTN_A), vec![(wire::BTN_GUIDE, true)]),
            ("release", None, vec![(wire::BTN_GUIDE, true)]),
        ];
        for (name, press, want) in rows {
            let mut g = SelectGesture::default();
            let mut out = Vec::new();
            assert!(g.on_select_down(t, true, &mut out));
            match press {
                Some(bit) => g.on_other_down(bit, late, &mut out),
                None => assert!(g.on_select_up(late, &mut out)),
            }
            assert_eq!(out, want, "{name}");
            out.clear();
            g.poll(late + GUIDE_COMMIT, &mut out);
            let lift = press.map_or(vec![(wire::BTN_GUIDE, false)], |_| vec![]);
            assert_eq!(out, lift, "{name}: nothing more until release");
        }
    }

    #[test]
    fn second_button_makes_pending_select_real() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.on_other_down(wire::BTN_Y, t, &mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        assert!(!g.on_select_up(t + Duration::from_millis(200), &mut out));
        assert!(out.is_empty());
        g.poll(t + GUIDE_HOLD * 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn select_inside_a_combo_passes_through() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        assert!(!g.on_select_down(Instant::now(), false, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn quick_repress_lifts_owed_release_first() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        assert!(g.on_select_down(t + Duration::from_millis(100), true, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn flush_lifts_synthetic_guide_and_owed_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_COMMIT, &mut out);
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        g.flush(&mut out);
        assert!(out.is_empty(), "a never-sent pending Select ghosted a send");
    }
}
