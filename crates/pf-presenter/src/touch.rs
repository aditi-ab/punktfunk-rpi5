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
//! overlay tier. (The Android/Apple twins additionally map a three-finger vertical SWIPE to
//! their local soft keyboard and gate scroll to exactly two fingers for it; SDL builds have
//! no soft keyboard to summon, so here 2+ fingers scroll.)
//!
//! Unlike the Android/Apple hosts (which hand the engine a whole event's worth of changed
//! touches at once), SDL delivers ONE finger transition per event, so this is a strictly
//! incremental state machine: it keeps every live finger's position and recomputes the
//! centroid itself. Positions are in physical window pixels (the caller multiplies SDL's
//! normalized 0..1 finger coordinates by the window's pixel size) so the pixel-based
//! ballistics constants port from Android 1:1; timestamps are milliseconds.

use punktfunk_core::input::InputKind;
use std::collections::HashMap;

// Gesture/ballistics tuning (physical px / ms), matching the Android reference exactly.
/// Movement under this (px) still counts as a tap, not a drag.
const TAP_SLOP: f32 = 12.0;
/// A new touch this soon (ms) after a tap, near it, starts a held left-button drag.
const TAP_DRAG_MS: f64 = 250.0;
/// Two-finger pan distance (px) per 120-unit wheel notch (smaller = faster scroll).
const SCROLL_DIV: f32 = 4.0;
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

/// A wire intent the engine emits; the owner ([`Capture`](crate::input::Capture)) translates
/// each into an actual `send_input`, and folds [`CycleStats`](Act::CycleStats) back to the
/// run loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl Act {
    /// The `(InputKind, code, x, y, flags)` this intent sends. `Button`/`CycleStats` don't map
    /// to a single motion send, so callers special-case them; this covers the motion/scroll
    /// intents shared with the raw pointer path.
    pub fn wire(self) -> Option<(InputKind, u32, i32, i32, u32)> {
        match self {
            Act::MoveRel { dx, dy } => Some((InputKind::MouseMove, 0, dx, dy, 0)),
            Act::MoveAbs(a) => Some((
                InputKind::MouseMoveAbs,
                0,
                a.x,
                a.y,
                ((a.w & 0xffff) << 16) | (a.h & 0xffff),
            )),
            Act::Scroll { axis, delta } => Some((InputKind::MouseScroll, axis, delta, 0, 0)),
            Act::Button { .. } | Act::CycleStats => None,
        }
    }
}

/// The trackpad/pointer gesture state machine. One per session; `trackpad` picks the model
/// (false = pointer). Fed only DIRECT touchscreen fingers.
pub struct Gestures {
    trackpad: bool,
    /// Live fingers → current window-pixel position (the centroid needs every finger, but a
    /// move event only carries the one that changed).
    positions: HashMap<u64, (f32, f32)>,
    /// A gesture is in flight (≥ 1 finger down since the first touch).
    active: bool,
    start: (f32, f32),
    max_fingers: usize,
    moved: bool,
    scrolling: bool,
    /// Finger count the scroll centroid is anchored at — re-anchor on a count change so a
    /// 2→3 transition isn't read as a scroll notch.
    scroll_count: usize,
    scroll_anchor: (f32, f32),
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
    pub fn new(trackpad: bool) -> Gestures {
        Gestures {
            trackpad,
            positions: HashMap::new(),
            active: false,
            start: (0.0, 0.0),
            max_fingers: 0,
            moved: false,
            scrolling: false,
            scroll_count: 0,
            scroll_anchor: (0.0, 0.0),
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
        if first {
            self.active = true;
            self.start = (wx, wy);
            self.max_fingers = 0;
            self.moved = false;
            self.scrolling = false;
            self.scroll_count = 0;
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
        acts
    }

    /// A finger moved.
    pub fn motion(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        if !self.active || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        self.positions.insert(id, (wx, wy));
        if self.positions.len() >= 2 {
            self.scroll_by_centroid()
        } else if !self.scrolling {
            // One finger, and the gesture never became a scroll (dropping back from two
            // fingers to one must not jerk the cursor).
            self.single_finger(id, wx, wy, abs, t)
        } else {
            Vec::new()
        }
    }

    /// A finger lifted. Only when the LAST finger lifts does the gesture conclude (into a
    /// click / drag-end / stats cycle). `t` is the up-time in milliseconds.
    pub fn up(&mut self, id: u64, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        self.positions.remove(&id);
        if self.track_id == Some(id) {
            self.track_id = None;
        }
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

    /// Forget all in-flight gesture state (capture release / session teardown). Any left
    /// button the engine is holding is released by the owner's held-button flush, so this
    /// only clears state — it never re-emits wire events.
    pub fn reset(&mut self) {
        self.positions.clear();
        self.track_id = None;
        self.active = false;
        self.scrolling = false;
        self.moved = false;
        self.drag_held = false;
        self.last_tap_up = 0.0;
    }

    /// Two (or more) fingers → scroll by the centroid delta; never move the cursor. Fires a
    /// notch per `SCROLL_DIV` px of pan and re-anchors on fire; finger up scrolls up, finger
    /// right scrolls right (the host WHEEL(120) convention).
    fn scroll_by_centroid(&mut self) -> Vec<Act> {
        let mut acts = Vec::new();
        let n = self.positions.len() as f32;
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &(px, py) in self.positions.values() {
            sx += px;
            sy += py;
        }
        let (cx, cy) = (sx / n, sy / n);
        // (Re-)anchor on scroll start AND whenever the finger count changes.
        if !self.scrolling || self.positions.len() != self.scroll_count {
            self.scrolling = true;
            self.scroll_count = self.positions.len();
            self.scroll_anchor = (cx, cy);
        }
        let notches_y = ((self.scroll_anchor.1 - cy) / SCROLL_DIV) as i32;
        let notches_x = ((cx - self.scroll_anchor.0) / SCROLL_DIV) as i32;
        if notches_y != 0 {
            acts.push(Act::Scroll {
                axis: 0,
                delta: notches_y * 120,
            });
            self.scroll_anchor.1 = cy;
            self.moved = true;
        }
        if notches_x != 0 {
            acts.push(Act::Scroll {
                axis: 1,
                delta: notches_x * 120,
            });
            self.scroll_anchor.0 = cx;
            self.moved = true;
        }
        acts
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
        let mut g = Gestures::new(true);
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
        let mut g = Gestures::new(false);
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
        let mut g = Gestures::new(true);
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
        let mut g = Gestures::new(true);
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
        let mut g = Gestures::new(true);
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
        let mut g = Gestures::new(false);
        let _ = g.down(1, 100.0, 100.0, abs_at(300, 300), 0.0);
        let acts = g.motion(1, 140.0, 120.0, abs_at(360, 340), 16.0);
        assert_eq!(acts, vec![Act::MoveAbs(abs_at(360, 340))]);
    }

    #[test]
    fn two_finger_pan_scrolls_by_the_centroid() {
        let mut g = Gestures::new(true);
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
    fn tap_then_press_drag_holds_the_left_button() {
        let mut g = Gestures::new(true);
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

    #[test]
    fn reset_clears_a_drag_without_re_emitting() {
        let mut g = Gestures::new(true);
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
