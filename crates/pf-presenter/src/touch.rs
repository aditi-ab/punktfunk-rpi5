//! Touchscreen fingers → host mouse for the `trackpad`/`pointer` touch-input models — an
//! incremental port of the Android client's gesture engine (clients/android
//! `TouchInput.kt`) and its Apple twin (`TouchMouse.swift`) so all three touch clients feel
//! identical. The third model, `touch`, never reaches here: those fingers go on the wire as
//! real multi-touch contacts (`Capture::on_touch_*`).
//!
//! Two mouse models share one gesture vocabulary:
//!  * **trackpad** (default): the cursor STAYS PUT on touch-down and moves by the finger's
//!    relative delta with mild acceleration — swipe to nudge, lift and re-swipe to walk it
//!    across, tap to click where it is. What makes a cursor reachable on a small screen.
//!  * **pointer**: the cursor jumps to the finger and follows it (absolute moves through the
//!    aspect-fit letterbox) — direct pointing.
//!
//! Shared gestures: tap = left click · two-finger tap = right click · two-finger drag =
//! scroll · tap-then-press-and-drag = held left drag · three-finger tap = cycle the stats
//! overlay tier. Three or more fingers never scroll: the Android/Apple twins map a
//! three-finger vertical SWIPE to their local soft keyboard, and SDL builds have none to
//! summon, so here a three-finger drag does nothing but disqualify the tap.
//!
//! Unlike the Android/Apple hosts (which hand the engine a whole event's worth of changed
//! touches at once), SDL delivers ONE finger transition per event, so this is a strictly
//! incremental state machine: it keeps every live finger's position and recomputes the
//! centroid itself. Positions are in physical window pixels (the caller multiplies SDL's
//! normalized 0..1 finger coordinates by the window's pixel size) so the pixel-based
//! ballistics constants port from Android 1:1; timestamps are milliseconds.

use std::collections::HashMap;

// Gesture/ballistics tuning (physical px / ms), matching the Android reference exactly.
/// Movement under this (px) still counts as a tap, not a drag.
const TAP_SLOP: f32 = 12.0;
/// A new touch this soon (ms) after a tap, near it, starts a held left-button drag.
const TAP_DRAG_MS: f64 = 250.0;
/// One finger held still this long (ms) presses the left button and drags until it lifts —
/// the touch idiom for "pick this up" (windows, text, files).
const LONG_PRESS_MS: f64 = 500.0;
/// Two-finger pan distance (px) per 120-unit wheel notch (smaller = faster scroll).
const SCROLL_DIV: f32 = 4.0;
/// The dial (design/touch-client-overlay.md §2.1): a two-finger TWIST opens the quick-action
/// ring. Below this rotation the gesture is still a scroll candidate — natural scrolls rotate a
/// few degrees, and this is what absorbs them; much below 8° two-finger scrolling gets flaky.
const DIAL_ARM_DEG: f32 = 10.0;
/// At this rotation the ring commits: it stays open after the fingers lift.
const DIAL_COMMIT_DEG: f32 = 30.0;
/// Centroid travel (px) beyond this before arming means scroll, not dial.
const DIAL_SLOP: f32 = 2.0 * TAP_SLOP;
/// SDL reports the fingers of one input frame as separate events with the same timestamp, so
/// the twist is judged only when the other finger's position is from the same frame (its event
/// is at most this far back) — halfway through a plain scroll step the finger-to-finger vector
/// looks rotated by tens of degrees.
const DIAL_FRAME_MS: f64 = 4.0;
/// …or when the other finger has not moved for this long: a thumb pivot, one finger turning
/// around a still one, is a real twist and never completes a pair.
const DIAL_PIVOT_MS: f64 = 50.0;
/// Base finger-px → host-px gain (~1:1, never twitchy).
const POINTER_SENS: f32 = 1.3;
/// Above `ACCEL_SPEED_FLOOR` px/ms the gain ramps by `ACCEL_GAIN` per px/ms, capped at
/// `ACCEL_MAX` so a fast swipe can't fling the cursor uncontrollably.
const ACCEL_GAIN: f32 = 0.6;
const ACCEL_SPEED_FLOOR: f32 = 0.3;
const ACCEL_MAX: f32 = 3.0;

/// GameStream mouse button ids.
const BTN_LEFT: u32 = 1;
const BTN_RIGHT: u32 = 3;

/// A finger's position in the letterboxed video content rect (absolute host pixels + the
/// content surface size) — what `pointer` mode's absolute moves carry. Mirrors the
/// `MouseMoveAbs` packing the host rescales into its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abs {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// A wire intent the engine emits; the owner (`Capture` in `input.rs`, which also holds the
/// intent → `InputKind` translation) sends each one, and folds [`CycleStats`](Act::CycleStats)
/// back to the run loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    /// Relative cursor motion (`MouseMove`).
    MoveRel { dx: i32, dy: i32 },
    /// Absolute cursor position through the letterbox (`MouseMoveAbs`).
    MoveAbs(Abs),
    /// A mouse button transition (`gs` = GameStream id; `down` = press/release).
    Button { gs: u32, down: bool },
    /// A wheel step: `axis` 0 = vertical, 1 = horizontal; `delta` in WHEEL(120) units.
    Scroll { axis: u32, delta: i32 },
    /// Three-finger tap: cycle the stats-overlay verbosity tier (the run loop owns it).
    CycleStats,
    /// A two-finger twist is turning the quick-action ring: `progress` 0…1 drives its unwind
    /// frame by frame, `clockwise` is the hand's direction, `x`/`y` (window px) the centroid
    /// the ring is centred on. Emitted on every move once the twist has armed.
    Dial {
        progress: f32,
        clockwise: bool,
        x: f32,
        y: f32,
    },
    /// The twist reached `DIAL_COMMIT_DEG`: the ring stays open after the fingers lift.
    DialCommit,
    /// The twist ended short of commit, or wound back below the arm angle after committing:
    /// the ring winds back in and nothing was sent — no scroll, no click.
    DialCancel,
}

/// The dial's share of a two-finger gesture: the finger-to-finger vector the moment the second
/// finger landed, compared against the live one on every move.
#[derive(Clone, Copy)]
struct Dial {
    ids: (u64, u64),
    vec: (f32, f32),
    /// The centroid at the second finger's landing — travel past `DIAL_SLOP` before arming
    /// means the hand is scrolling, not turning.
    anchor: (f32, f32),
    armed: bool,
    committed: bool,
}

/// The trackpad/pointer gesture state machine. One per session; `trackpad` picks the model
/// (false = pointer). Fed only DIRECT touchscreen fingers.
pub struct Gestures {
    trackpad: bool,
    /// `-1` with the invert-scroll setting on: applied where the notch is made (the twins do
    /// the same), so the touch path honours the setting the wheel path already did.
    scroll_sign: i32,
    /// Live fingers → current window-pixel position (the centroid needs every finger, but a
    /// move event only carries the one that changed).
    positions: HashMap<u64, (f32, f32)>,
    /// Live fingers → the time (ms) of their last event; the dial's same-frame test.
    times: HashMap<u64, f64>,
    /// A gesture is in flight (≥ 1 finger down since the first touch).
    active: bool,
    start: (f32, f32),
    /// When the first finger landed (ms) — the long-press clock.
    down_t: f64,
    max_fingers: usize,
    moved: bool,
    scrolling: bool,
    scroll_anchor: (f32, f32),
    /// A scroll notch went on the wire this gesture: it is a scroll for its lifetime, never a
    /// dial.
    scroll_emitted: bool,
    /// The two-finger twist in flight, from the second finger's landing to any lift.
    dial: Option<Dial>,
    /// Three-or-more-finger centroid anchor, per finger count (0 = none): real fingers never
    /// land or lift in the same event, so a count change must re-anchor, not read as travel.
    many_count: usize,
    many_anchor: (f32, f32),
    /// A tap-then-press-and-drag is holding the left button down for this whole gesture.
    drag_held: bool,
    // Trackpad relative-motion state: the tracked finger, its last position/time, and the
    // sub-pixel remainder so a slow drag isn't lost to integer truncation.
    track_id: Option<u64>,
    prev: (f32, f32),
    prev_t: f64,
    carry: (f32, f32),
    // Tap-drag arming: a quick tap leaves a window in which the next nearby touch drags.
    last_tap_up: f64,
    last_tap_pt: (f32, f32),
}

impl Gestures {
    pub fn new(trackpad: bool, invert_scroll: bool) -> Gestures {
        Gestures {
            trackpad,
            scroll_sign: if invert_scroll { -1 } else { 1 },
            positions: HashMap::new(),
            times: HashMap::new(),
            active: false,
            start: (0.0, 0.0),
            down_t: 0.0,
            max_fingers: 0,
            moved: false,
            scrolling: false,
            scroll_anchor: (0.0, 0.0),
            scroll_emitted: false,
            dial: None,
            many_count: 0,
            many_anchor: (0.0, 0.0),
            drag_held: false,
            track_id: None,
            prev: (0.0, 0.0),
            prev_t: 0.0,
            carry: (0.0, 0.0),
            last_tap_up: 0.0,
            last_tap_pt: (0.0, 0.0),
        }
    }

    /// A finger touched down. `abs` is its letterbox mapping (pointer mode jumps the cursor
    /// there on the first finger). `t` is milliseconds.
    pub fn down(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        let first = self.positions.is_empty() && !self.active;
        self.positions.insert(id, (wx, wy));
        self.times.insert(id, t);
        if first {
            self.active = true;
            self.start = (wx, wy);
            self.down_t = t;
            self.max_fingers = 0;
            self.moved = false;
            self.scrolling = false;
            self.scroll_emitted = false;
            self.dial = None;
            self.many_count = 0;
            // A touch landing just after a quick tap nearby = tap-and-drag.
            self.drag_held = t - self.last_tap_up < TAP_DRAG_MS
                && (wx - self.last_tap_pt.0).abs() < TAP_SLOP
                && (wy - self.last_tap_pt.1).abs() < TAP_SLOP;
            self.last_tap_up = 0.0; // consume the arming either way
            if !self.trackpad {
                acts.push(Act::MoveAbs(abs)); // pointer: place the cursor before any press
            }
            if self.drag_held {
                acts.push(Act::Button {
                    gs: BTN_LEFT,
                    down: true,
                });
            }
            self.track_id = Some(id);
            self.prev = (wx, wy);
            self.prev_t = t;
            self.carry = (0.0, 0.0);
        }
        self.max_fingers = self.max_fingers.max(self.positions.len());
        match self.positions.len() {
            // The second finger: remember the finger-to-finger vector — the dial's reference —
            // unless this gesture already scrolled (then it is a scroll for its lifetime).
            2 if !self.scroll_emitted && self.dial.is_none() => {
                if let Some((&other, &op)) = self.positions.iter().find(|(k, _)| **k != id) {
                    self.dial = Some(Dial {
                        ids: (other, id),
                        vec: (wx - op.0, wy - op.1),
                        anchor: self.centroid(),
                        armed: false,
                        committed: false,
                    });
                }
            }
            // A third finger ends the twist: wound back if it had armed and not committed.
            n if n > 2 => acts.extend(self.end_dial(false)),
            _ => {}
        }
        acts
    }

    /// A finger moved.
    pub fn motion(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        if !self.active || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        self.positions.insert(id, (wx, wy));
        self.times.insert(id, t);
        // Dropping below three fingers forgets the many-finger anchor, so a 3→2→3 bounce
        // re-anchors instead of reading the count change as travel.
        if self.positions.len() < 3 {
            self.many_count = 0;
        }
        match self.positions.len() {
            2 => self
                .dial_step(id, t)
                .unwrap_or_else(|| self.scroll_by_centroid()),
            n if n >= 3 => {
                self.many_fingers();
                Vec::new()
            }
            // One finger, and the gesture never became a scroll (dropping back from two
            // fingers to one must not jerk the cursor).
            _ if !self.scrolling => self.single_finger(id, wx, wy, abs, t),
            _ => Vec::new(),
        }
    }

    /// A finger lifted. Only when the LAST finger lifts does the gesture conclude (into a
    /// click / drag-end / stats cycle). `t` is the up-time in milliseconds.
    pub fn up(&mut self, id: u64, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        self.positions.remove(&id);
        self.times.remove(&id);
        if self.track_id == Some(id) {
            self.track_id = None;
        }
        // Any lift ends the twist. Committed: the ring stays open and the UI owns it from
        // here. Short of commit: it winds back in, and the finger still down (if any) must
        // not start moving the cursor — the gesture stays "scrolling", i.e. inert.
        acts.extend(self.end_dial(true));
        if !self.positions.is_empty() || !self.active {
            return acts; // other fingers still down (or no live gesture)
        }
        self.active = false;
        if self.drag_held {
            self.drag_held = false;
            acts.push(Act::Button {
                gs: BTN_LEFT,
                down: false,
            }); // end the held drag
        } else if !self.moved {
            match self.max_fingers {
                n if n >= 3 => acts.push(Act::CycleStats),
                2 => {
                    acts.push(Act::Button {
                        gs: BTN_RIGHT,
                        down: true,
                    });
                    acts.push(Act::Button {
                        gs: BTN_RIGHT,
                        down: false,
                    });
                }
                _ => {
                    acts.push(Act::Button {
                        gs: BTN_LEFT,
                        down: true,
                    });
                    acts.push(Act::Button {
                        gs: BTN_LEFT,
                        down: false,
                    });
                    self.last_tap_up = t; // arm tap-drag
                    self.last_tap_pt = self.start;
                }
            }
        }
        acts
    }

    /// Time passes with fingers down. A still finger produces no event, so the long-press
    /// arm needs the clock: one finger, never a second, under the tap slop, held for
    /// `LONG_PRESS_MS` → the left button goes down and the lift releases it exactly like a
    /// tap-then-drag. Call once per run-loop iteration; `t` is the finger events' clock.
    pub fn tick(&mut self, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        if self.active
            && self.positions.len() == 1
            && self.max_fingers == 1
            && !self.moved
            && !self.drag_held
            && t - self.down_t >= LONG_PRESS_MS
        {
            self.drag_held = true;
            acts.push(Act::Button {
                gs: BTN_LEFT,
                down: true,
            });
        }
        acts
    }

    /// Forget all in-flight gesture state (capture release / session teardown). Any left
    /// button the engine is holding is released by the owner's held-button flush, so this
    /// only clears state — it never re-emits wire events.
    pub fn reset(&mut self) {
        self.positions.clear();
        self.times.clear();
        self.track_id = None;
        self.active = false;
        self.scrolling = false;
        self.scroll_emitted = false;
        self.dial = None;
        self.moved = false;
        self.drag_held = false;
        self.last_tap_up = 0.0;
    }

    /// The dial's share of a two-finger move; `Some` when the twist owns the gesture (the
    /// scroll path then never runs), `None` when the hand is scrolling, or might still be.
    ///
    /// Rules, in order (design §2.1): a scroll notch already sent ⇒ never a dial; centroid
    /// travel past `DIAL_SLOP` before arming ⇒ scroll; rotation ≥ `DIAL_ARM_DEG` ⇒ the twist
    /// arms and owns the gesture; progress `(|Δφ| − arm) / (commit − arm)` feeds the ring; at
    /// 1 the ring commits, and winding back to 0 after a commit closes it again. A pinch with
    /// no rotation is nothing: it never arms and moves no centroid.
    fn dial_step(&mut self, id: u64, t: f64) -> Option<Vec<Act>> {
        let dial = self.dial?;
        if !dial.armed && self.scroll_emitted {
            return None;
        }
        // Judge the rotation only against a position of the other finger that is current:
        // from this same input frame, or older than a pivot's stillness. In between, the
        // other finger's event for this frame is still to come, and the vector would lie.
        let other = if dial.ids.0 == id {
            dial.ids.1
        } else {
            dial.ids.0
        };
        let gap = t - *self.times.get(&other)?;
        if gap > DIAL_FRAME_MS && gap < DIAL_PIVOT_MS {
            return dial.armed.then(Vec::new);
        }
        let (a, b) = (
            *self.positions.get(&dial.ids.0)?,
            *self.positions.get(&dial.ids.1)?,
        );
        let v = (b.0 - a.0, b.1 - a.1);
        let cross = dial.vec.0 * v.1 - dial.vec.1 * v.0;
        let dot = dial.vec.0 * v.0 + dial.vec.1 * v.1;
        // Signed rotation of the finger-to-finger vector; positive = clockwise on a y-down
        // screen.
        let phi = cross.atan2(dot).to_degrees();
        let (cx, cy) = self.centroid();
        if !dial.armed {
            let travel = (cx - dial.anchor.0).hypot(cy - dial.anchor.1);
            if travel >= DIAL_SLOP || phi.abs() < DIAL_ARM_DEG {
                return None;
            }
            self.moved = true; // a twist is never a tap
            self.scrolling = true; // and dropping to one finger must not jerk the cursor
        }
        let progress =
            ((phi.abs() - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG)).clamp(0.0, 1.0);
        let mut acts = vec![Act::Dial {
            progress,
            clockwise: phi > 0.0,
            x: cx,
            y: cy,
        }];
        let d = self.dial.as_mut()?;
        d.armed = true;
        if progress >= 1.0 && !d.committed {
            d.committed = true;
            acts.push(Act::DialCommit);
        } else if progress <= 0.0 && d.committed {
            d.committed = false;
            acts.push(Act::DialCancel);
        }
        Some(acts)
    }

    /// The twist is over (a finger lifted or a third landed). `cancel_uncommitted` winds an
    /// armed-but-uncommitted ring back in; a committed ring stays open for the UI.
    fn end_dial(&mut self, _lift: bool) -> Vec<Act> {
        match self.dial.take() {
            Some(d) if d.armed && !d.committed => vec![Act::DialCancel],
            _ => Vec::new(),
        }
    }

    /// The live fingers' centroid.
    fn centroid(&self) -> (f32, f32) {
        let n = self.positions.len() as f32;
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &(px, py) in self.positions.values() {
            sx += px;
            sy += py;
        }
        (sx / n, sy / n)
    }

    /// Exactly two fingers → scroll by the centroid delta; never move the cursor. Fires a
    /// notch per `SCROLL_DIV` px of pan and re-anchors on fire; finger up scrolls up, finger
    /// right scrolls right (the host WHEEL(120) convention).
    fn scroll_by_centroid(&mut self) -> Vec<Act> {
        let mut acts = Vec::new();
        let (cx, cy) = self.centroid();
        if !self.scrolling {
            self.scrolling = true;
            self.scroll_anchor = (cx, cy);
        }
        let notches_y = ((self.scroll_anchor.1 - cy) / SCROLL_DIV) as i32;
        let notches_x = ((cx - self.scroll_anchor.0) / SCROLL_DIV) as i32;
        if notches_y != 0 {
            acts.push(Act::Scroll {
                axis: 0,
                delta: notches_y * 120 * self.scroll_sign,
            });
            self.scroll_anchor.1 = cy;
            self.moved = true;
            self.scroll_emitted = true;
        }
        if notches_x != 0 {
            acts.push(Act::Scroll {
                axis: 1,
                delta: notches_x * 120 * self.scroll_sign,
            });
            self.scroll_anchor.0 = cx;
            self.moved = true;
            self.scroll_emitted = true;
        }
        acts
    }

    /// Three or more fingers: no scroll, no cursor motion. Centroid travel beyond `TAP_SLOP`
    /// disqualifies the tap (else a short three-finger swipe would still cycle the stats).
    /// Leaving the scroll state stale would read the 3→2 centroid jump as a wheel notch, and
    /// the tracked finger's position froze meanwhile, so both re-anchor fresh on the way back.
    fn many_fingers(&mut self) {
        let (cx, cy) = self.centroid();
        if self.positions.len() != self.many_count {
            self.many_count = self.positions.len();
            self.many_anchor = (cx, cy);
        } else if (cx - self.many_anchor.0).abs() > TAP_SLOP
            || (cy - self.many_anchor.1).abs() > TAP_SLOP
        {
            self.moved = true;
        }
        self.scrolling = false;
        self.track_id = None;
    }

    /// One finger, not scrolling: trackpad relative ballistics, or pointer absolute follow.
    fn single_finger(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        if (wx - self.start.0).abs() > TAP_SLOP || (wy - self.start.1).abs() > TAP_SLOP {
            self.moved = true;
        }
        if !self.trackpad {
            acts.push(Act::MoveAbs(abs)); // the cursor follows the finger
            return acts;
        }
        // Re-anchor (zero delta this frame) if the tracked finger changed, so lifting one of
        // several fingers never jumps the cursor.
        if self.track_id != Some(id) {
            self.track_id = Some(id);
            self.prev = (wx, wy);
            self.prev_t = t;
            return acts;
        }
        let dx = wx - self.prev.0;
        let dy = wy - self.prev.1;
        let dt_ms = (t - self.prev_t).max(1.0) as f32;
        self.prev = (wx, wy);
        self.prev_t = t;
        let speed = dx.hypot(dy) / dt_ms; // finger px per ms
        let accel = (1.0 + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).max(0.0)).min(ACCEL_MAX);
        let gain = POINTER_SENS * accel;
        self.carry.0 += dx * gain;
        self.carry.1 += dy * gain;
        let out_x = self.carry.0 as i32; // truncates toward zero → remainder kept with sign
        let out_y = self.carry.1 as i32;
        if out_x != 0 || out_y != 0 {
            acts.push(Act::MoveRel {
                dx: out_x,
                dy: out_y,
            });
            self.carry.0 -= out_x as f32;
            self.carry.1 -= out_y as f32;
        }
        acts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABS: Abs = Abs {
        x: 100,
        y: 200,
        w: 1280,
        h: 720,
    };

    fn abs_at(x: i32, y: i32) -> Abs {
        Abs {
            x,
            y,
            w: 1280,
            h: 720,
        }
    }

    #[test]
    fn trackpad_tap_is_a_left_click_with_no_motion() {
        let mut g = Gestures::new(true, false);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.up(1, 40.0));
        // A trackpad tap places no cursor and moves nothing — just a click.
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn pointer_tap_places_the_cursor_then_clicks() {
        let mut g = Gestures::new(false, false);
        let mut acts = g.down(1, 50.0, 50.0, abs_at(640, 360), 0.0);
        acts.extend(g.up(1, 40.0));
        assert_eq!(
            acts,
            vec![
                Act::MoveAbs(abs_at(640, 360)),
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn two_finger_tap_is_a_right_click() {
        let mut g = Gestures::new(true, false);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.down(2, 80.0, 52.0, ABS, 5.0));
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 42.0));
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_RIGHT,
                    down: true
                },
                Act::Button {
                    gs: BTN_RIGHT,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn three_finger_tap_cycles_stats() {
        let mut g = Gestures::new(true, false);
        let mut acts = g.down(1, 50.0, 50.0, ABS, 0.0);
        acts.extend(g.down(2, 80.0, 50.0, ABS, 2.0));
        acts.extend(g.down(3, 110.0, 50.0, ABS, 4.0));
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        acts.extend(g.up(3, 42.0));
        assert_eq!(acts, vec![Act::CycleStats]);
    }

    #[test]
    fn trackpad_drag_emits_relative_motion() {
        let mut g = Gestures::new(true, false);
        assert!(g.down(1, 100.0, 100.0, ABS, 0.0).is_empty());
        // A big move over 16 ms — relative, with acceleration, so it should exceed 1:1.
        let acts = g.motion(1, 140.0, 100.0, ABS, 16.0);
        match acts.as_slice() {
            [Act::MoveRel { dx, dy }] => {
                assert!(*dx >= 40, "expected accelerated dx ≥ raw 40, got {dx}");
                assert_eq!(*dy, 0);
            }
            other => panic!("expected one MoveRel, got {other:?}"),
        }
        // The gesture moved, so the lift is not a tap (no click).
        assert!(g.up(1, 32.0).is_empty());
    }

    #[test]
    fn pointer_motion_follows_the_finger_absolutely() {
        let mut g = Gestures::new(false, false);
        let _ = g.down(1, 100.0, 100.0, abs_at(300, 300), 0.0);
        let acts = g.motion(1, 140.0, 120.0, abs_at(360, 340), 16.0);
        assert_eq!(acts, vec![Act::MoveAbs(abs_at(360, 340))]);
    }

    #[test]
    fn two_finger_pan_scrolls_by_the_centroid() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 120.0, 200.0, ABS, 2.0);
        // Both fingers slide up 40 px → the centroid rises 40 px → +ve (finger-up) notches.
        let a1 = g.motion(1, 100.0, 160.0, ABS, 10.0);
        let a2 = g.motion(2, 120.0, 160.0, ABS, 12.0);
        let scrolls: Vec<_> = a1.into_iter().chain(a2).collect();
        assert!(
            scrolls
                .iter()
                .any(|a| matches!(a, Act::Scroll { axis: 0, delta } if *delta > 0)),
            "expected an upward vertical scroll, got {scrolls:?}"
        );
    }

    #[test]
    fn invert_scroll_flips_the_touch_notch() {
        let mut g = Gestures::new(true, true);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 120.0, 200.0, ABS, 2.0);
        let a1 = g.motion(1, 100.0, 160.0, ABS, 10.0);
        let a2 = g.motion(2, 120.0, 160.0, ABS, 12.0);
        let scrolls: Vec<_> = a1.into_iter().chain(a2).collect();
        // Fingers up, setting on → the notch goes the other way.
        assert!(
            scrolls
                .iter()
                .any(|a| matches!(a, Act::Scroll { axis: 0, delta } if *delta < 0)),
            "expected an inverted (negative) vertical scroll, got {scrolls:?}"
        );
        assert!(!scrolls
            .iter()
            .any(|a| matches!(a, Act::Scroll { delta, .. } if *delta > 0)));
    }

    #[test]
    fn three_finger_drag_scrolls_nothing() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 130.0, 200.0, ABS, 2.0);
        let _ = g.down(3, 160.0, 200.0, ABS, 4.0);
        // All three slide up 40 px: the twins reserve this for their keyboard swipe, so the
        // desktop must neither scroll nor move the cursor…
        let mut acts = g.motion(1, 100.0, 160.0, ABS, 10.0);
        acts.extend(g.motion(2, 130.0, 160.0, ABS, 12.0));
        acts.extend(g.motion(3, 160.0, 160.0, ABS, 14.0));
        assert_eq!(acts, vec![], "a three-finger drag must emit nothing");
        // …and the travel disqualifies the three-finger tap on lift.
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        acts.extend(g.up(3, 42.0));
        assert_eq!(acts, vec![]);
    }

    #[test]
    fn tap_then_press_drag_holds_the_left_button() {
        let mut g = Gestures::new(true, false);
        // Tap at (50,50), lifting at t=10.
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let click = g.up(1, 10.0);
        assert_eq!(
            click,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
        // A new touch nearby within the window arms a held drag: button down on touch, and
        // the whole gesture holds it until the lift.
        let down2 = g.down(2, 52.0, 51.0, ABS, 120.0);
        assert_eq!(
            down2,
            vec![Act::Button {
                gs: BTN_LEFT,
                down: true
            }]
        );
        let _ = g.motion(2, 90.0, 51.0, ABS, 140.0); // drag
        let end = g.up(2, 160.0);
        assert_eq!(
            end,
            vec![Act::Button {
                gs: BTN_LEFT,
                down: false
            }]
        );
    }

    /// Two fingers 40 px apart, rotated `deg` clockwise about `c`.
    fn twisted(c: (f32, f32), deg: f32) -> ((f32, f32), (f32, f32)) {
        let (s, k) = deg.to_radians().sin_cos();
        let (rx, ry) = (20.0 * k, 20.0 * s); // half the finger-to-finger vector
        ((c.0 - rx, c.1 - ry), (c.0 + rx, c.1 + ry))
    }

    fn dial_acts(acts: &[Act]) -> Vec<Act> {
        acts.iter()
            .copied()
            .filter(|a| matches!(a, Act::Dial { .. } | Act::DialCommit | Act::DialCancel))
            .collect()
    }

    #[test]
    fn a_pure_scroll_never_arms_the_dial() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = Vec::new();
        for step in 1..=10 {
            let y = 200.0 - 4.0 * step as f32;
            acts.extend(g.motion(1, 100.0, y, ABS, 10.0 * step as f64));
            acts.extend(g.motion(2, 140.0, y, ABS, 10.0 * step as f64 + 1.0));
        }
        assert!(acts.iter().any(|a| matches!(a, Act::Scroll { .. })));
        assert!(dial_acts(&acts).is_empty(), "{acts:?}");
        acts.clear();
        acts.extend(g.up(1, 200.0));
        acts.extend(g.up(2, 201.0));
        assert!(dial_acts(&acts).is_empty());
    }

    #[test]
    fn a_thirty_five_degree_twist_commits_at_the_first_sample_past_thirty() {
        let mut g = Gestures::new(true, false);
        let c = (120.0, 200.0);
        let (p1, p2) = twisted(c, 0.0);
        let _ = g.down(1, p1.0, p1.1, ABS, 0.0);
        let _ = g.down(2, p2.0, p2.1, ABS, 2.0);
        let mut commit_at = None;
        // 7° steps: no sample sits on a threshold, where float rounding decides the side.
        for step in 1..=5 {
            let deg = 7.0 * step as f32;
            let (p1, p2) = twisted(c, deg);
            let mut acts = g.motion(1, p1.0, p1.1, ABS, 10.0 * step as f64);
            acts.extend(g.motion(2, p2.0, p2.1, ABS, 10.0 * step as f64 + 1.0));
            assert!(
                !acts.iter().any(|a| matches!(a, Act::Scroll { .. })),
                "a twist never scrolls: {acts:?}"
            );
            let dial = dial_acts(&acts);
            if deg < DIAL_ARM_DEG {
                assert!(
                    dial.is_empty(),
                    "under the arm angle nothing fires: {dial:?}"
                );
            } else {
                let Some(Act::Dial {
                    progress,
                    clockwise,
                    ..
                }) = dial.first()
                else {
                    panic!("expected a Dial act at {deg}°, got {dial:?}");
                };
                assert!(*clockwise, "the ring turns the way the hand turns");
                let expected =
                    ((deg - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG)).clamp(0.0, 1.0);
                assert!(
                    (progress - expected).abs() < 0.05,
                    "{deg}°: {progress} vs {expected}"
                );
            }
            if dial.contains(&Act::DialCommit) && commit_at.is_none() {
                commit_at = Some(deg);
            }
        }
        assert_eq!(
            commit_at,
            Some(35.0),
            "commits exactly once, at the first sample past the commit angle"
        );
        // Committed: the lift leaves the ring open — no cancel, no click.
        let mut acts = g.up(1, 100.0);
        acts.extend(g.up(2, 101.0));
        assert_eq!(acts, vec![]);
    }

    #[test]
    fn a_twenty_degree_twist_then_a_lift_cancels_and_sends_nothing() {
        let mut g = Gestures::new(true, false);
        let c = (120.0, 200.0);
        let (p1, p2) = twisted(c, 0.0);
        let _ = g.down(1, p1.0, p1.1, ABS, 0.0);
        let _ = g.down(2, p2.0, p2.1, ABS, 2.0);
        let (p1, p2) = twisted(c, 20.0);
        let mut acts = g.motion(1, p1.0, p1.1, ABS, 10.0);
        acts.extend(g.motion(2, p2.0, p2.1, ABS, 11.0));
        assert!(matches!(acts.first(), Some(Act::Dial { .. })));
        assert!(!acts.contains(&Act::DialCommit));
        let mut lift = g.up(1, 50.0);
        assert_eq!(lift, vec![Act::DialCancel], "the ring winds back in");
        // The finger still down must not move the cursor, and the last lift is not a tap.
        lift.extend(g.motion(2, p2.0 + 40.0, p2.1, ABS, 60.0));
        lift.extend(g.up(2, 70.0));
        assert_eq!(lift, vec![Act::DialCancel]);
    }

    #[test]
    fn a_scroll_then_a_rotation_stays_a_scroll() {
        let mut g = Gestures::new(true, false);
        let c = (120.0, 200.0);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = g.motion(1, 100.0, 180.0, ABS, 10.0); // 20 px up: notches fire
        acts.extend(g.motion(2, 140.0, 180.0, ABS, 11.0));
        assert!(acts.iter().any(|a| matches!(a, Act::Scroll { .. })));
        let (p1, p2) = twisted((c.0, 180.0), 40.0); // then a big twist around the new centroid
        acts = g.motion(1, p1.0, p1.1, ABS, 20.0);
        acts.extend(g.motion(2, p2.0, p2.1, ABS, 21.0));
        assert!(
            dial_acts(&acts).is_empty(),
            "a scroll is a scroll for its lifetime: {acts:?}"
        );
    }

    #[test]
    fn long_press_arms_a_drag() {
        let mut g = Gestures::new(true, false);
        assert!(g.down(1, 50.0, 50.0, ABS, 0.0).is_empty());
        assert!(g.tick(400.0).is_empty(), "under the hold time: nothing");
        assert_eq!(
            g.tick(520.0),
            vec![Act::Button {
                gs: BTN_LEFT,
                down: true
            }]
        );
        assert!(g.tick(600.0).is_empty(), "arms once");
        let _ = g.motion(1, 90.0, 50.0, ABS, 620.0); // drags with the button held
        assert_eq!(
            g.up(1, 700.0),
            vec![Act::Button {
                gs: BTN_LEFT,
                down: false
            }]
        );
    }

    #[test]
    fn long_press_after_motion_or_a_second_finger_does_not_arm() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let _ = g.motion(1, 90.0, 50.0, ABS, 100.0); // past the slop: a swipe, not a press
        assert!(g.tick(600.0).is_empty());
        assert!(g.up(1, 700.0).is_empty()); // and a swipe is not a click either

        let _ = g.down(2, 50.0, 50.0, ABS, 1000.0);
        let _ = g.down(3, 80.0, 50.0, ABS, 1010.0);
        let _ = g.up(3, 1020.0); // a second finger came and went: no press
        assert!(g.tick(1600.0).is_empty());
    }

    #[test]
    fn reset_clears_a_drag_without_re_emitting() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let _ = g.up(1, 5.0); // arm
        let _ = g.down(2, 51.0, 50.0, ABS, 50.0); // drag begins (left held)
        g.reset();
        // After a reset a fresh tap is an ordinary click (no stuck drag state).
        let mut acts = g.down(3, 400.0, 400.0, ABS, 500.0);
        acts.extend(g.up(3, 510.0));
        assert_eq!(
            acts,
            vec![
                Act::Button {
                    gs: BTN_LEFT,
                    down: true
                },
                Act::Button {
                    gs: BTN_LEFT,
                    down: false
                },
            ]
        );
    }
}
