//! SDL fingers → host mouse for the `trackpad`/`pointer` models.
//!
//! Incremental port of Android `TouchInput.kt` and Apple `TouchMouse.swift`.
//! The `touch` model never reaches here: those contacts go on the wire as
//! real multi-touch (`Capture::on_touch_*`).
//!
//! `trackpad` (default): cursor stays put on down; relative delta with mild
//! acceleration. `pointer`: cursor jumps to the finger (absolute, letterbox).
//! Tap = left click · two-finger tap = right · two-finger drag = scroll ·
//! tap-then-press = held left drag · three-finger tap = cycle stats overlay.
//! Three or more fingers never scroll (the twins map a three-finger swipe to
//! their keyboard; SDL builds have none).
//!
//! SDL delivers one finger transition per event, so this machine keeps every
//! live finger and recomputes the centroid. Positions are physical window
//! pixels so ballistics port from Android 1:1; timestamps are milliseconds.

use std::collections::HashMap;

/// px; under this, a lift is still a tap.
const TAP_SLOP: f32 = 12.0;
/// ms after a tap, nearby: the next down starts a held left drag.
const TAP_DRAG_MS: f64 = 250.0;
/// ms of a still single finger: press left and hold until lift.
const LONG_PRESS_MS: f64 = 500.0;
/// px of two-finger pan per WHEEL(120) notch.
const SCROLL_DIV: f32 = 4.0;
/// Degrees of two-finger twist before the quick-action ring arms. Natural scrolls
/// rotate a few degrees; much below 8° two-finger scrolling gets flaky.
const DIAL_ARM_DEG: f32 = 10.0;
/// Degrees at which the ring stays open after lift.
const DIAL_COMMIT_DEG: f32 = 30.0;
/// Centroid travel (px) beyond this before arming means scroll, not dial.
const DIAL_SLOP: f32 = 2.0 * TAP_SLOP;
/// ms. SDL splits one frame's fingers into separate events with the same timestamp.
/// Judge twist only against a position this fresh — a mid-scroll pair otherwise
/// looks rotated by tens of degrees.
const DIAL_FRAME_MS: f64 = 4.0;
/// ms of stillness on the other finger: a thumb-pivot is a real twist and never
/// completes a same-frame pair.
const DIAL_PIVOT_MS: f64 = 50.0;
/// Finger-px → host-px gain (~1:1).
const POINTER_SENS: f32 = 1.3;
/// Extra gain per px/ms above `ACCEL_SPEED_FLOOR`. `ACCEL_MAX` stops a fast swipe
/// flinging the cursor.
const ACCEL_GAIN: f32 = 0.6;
const ACCEL_SPEED_FLOOR: f32 = 0.3;
const ACCEL_MAX: f32 = 3.0;

/// GameStream mouse button ids.
const BTN_LEFT: u32 = 1;
const BTN_RIGHT: u32 = 3;

/// Letterbox mapping: host pixels plus content size. `pointer` absolute moves carry
/// this; it matches the `MouseMoveAbs` packing the host rescales into its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abs {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Wire intent. `Capture` in `input.rs` sends each one and folds `CycleStats` back
/// to the run loop; the `InputKind` translation lives there so this crate stays
/// free of core.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    MoveRel { dx: i32, dy: i32 },
    MoveAbs(Abs),
    /// GameStream button id; `down` is press/release.
    Button { gs: u32, down: bool },
    /// `axis` 0 = vertical, 1 = horizontal; `delta` in WHEEL(120) units.
    Scroll { axis: u32, delta: i32 },
    /// Three-finger tap. The run loop owns the overlay tier.
    CycleStats,
    /// Armed twist driving the quick-action ring. `progress` 0…1; `x`/`y` are the
    /// centroid in window px. Emitted on every move once armed.
    Dial {
        progress: f32,
        clockwise: bool,
        x: f32,
        y: f32,
    },
    /// Reached `DIAL_COMMIT_DEG`; the ring stays open after lift.
    DialCommit,
    /// Ended short of commit, or wound back below the arm angle after commit.
    DialCancel,
}

/// Finger-to-finger vector at the second finger's landing, compared on every move.
#[derive(Clone, Copy)]
struct Dial {
    ids: (u64, u64),
    vec: (f32, f32),
    /// Centroid at second-finger landing. Travel past `DIAL_SLOP` before arming is a scroll.
    anchor: (f32, f32),
    armed: bool,
    committed: bool,
}

/// Trackpad/pointer state machine. One per session; `trackpad` false is pointer.
/// Fed only direct touchscreen fingers.
pub struct Gestures {
    trackpad: bool,
    /// `-1` when invert-scroll is on. Applied where the notch is made, matching the
    /// twins and the wheel path.
    scroll_sign: i32,
    /// Live fingers → window px. A move event carries only the finger that changed.
    positions: HashMap<u64, (f32, f32)>,
    /// Live fingers → last-event time (ms). The dial's same-frame test.
    times: HashMap<u64, f64>,
    active: bool,
    start: (f32, f32),
    /// First-finger down time (ms). Long-press clock.
    down_t: f64,
    max_fingers: usize,
    moved: bool,
    scrolling: bool,
    scroll_anchor: (f32, f32),
    /// A notch went on the wire this gesture: scroll for its lifetime, never a dial.
    scroll_emitted: bool,
    dial: Option<Dial>,
    /// Many-finger centroid, per finger count (0 = none). Fingers never land and lift
    /// in the same event, so a count change must re-anchor, not read as travel.
    many_count: usize,
    many_anchor: (f32, f32),
    drag_held: bool,
    // Tracked finger, last position/time, and sub-pixel remainder so a slow drag
    // is not lost to integer truncation.
    track_id: Option<u64>,
    prev: (f32, f32),
    prev_t: f64,
    carry: (f32, f32),
    // Last tap's up-time and point: a nearby down inside TAP_DRAG_MS holds left.
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

    /// Pointer mode jumps the cursor to `abs` on the first finger. `t` is ms.
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
            // Second finger: snapshot the pair vector unless this gesture already scrolled.
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
            n if n > 2 => acts.extend(self.end_dial(false)),
            _ => {}
        }
        acts
    }

    pub fn motion(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        if !self.active || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        self.positions.insert(id, (wx, wy));
        self.times.insert(id, t);
        // Below three fingers, drop the many-finger anchor so a 3→2→3 bounce re-anchors
        // instead of reading the count change as travel.
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
            // One finger and never a scroll: dropping 2→1 must not jerk the cursor.
            _ if !self.scrolling => self.single_finger(id, wx, wy, abs, t),
            _ => Vec::new(),
        }
    }

    /// The gesture concludes only on the last lift (click / drag-end / stats). `t` is ms.
    pub fn up(&mut self, id: u64, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        self.positions.remove(&id);
        self.times.remove(&id);
        if self.track_id == Some(id) {
            self.track_id = None;
        }
        // Any lift ends the twist. Committed: the ring stays open. Short of commit: wind
        // back, and keep the remaining finger inert (`scrolling`) so it cannot move the
        // cursor.
        acts.extend(self.end_dial(true));
        if !self.positions.is_empty() || !self.active {
            return acts;
        }
        self.active = false;
        if self.drag_held {
            self.drag_held = false;
            acts.push(Act::Button {
                gs: BTN_LEFT,
                down: false,
            });
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
                    self.last_tap_up = t;
                    self.last_tap_pt = self.start;
                }
            }
        }
        acts
    }

    /// A still finger produces no event, so long-press needs the clock. Call once per
    /// run-loop iteration. `t` is ms.
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

    /// Drop in-flight state (capture release / session teardown). Never re-emits; the
    /// owner's held-button flush releases any left button the engine was holding.
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

    /// Two-finger move. `Some` when the twist owns the gesture (scroll never runs);
    /// `None` when the hand is scrolling, or might still be.
    ///
    /// Order: a notch already sent ⇒ never a dial; centroid past `DIAL_SLOP` before
    /// arming ⇒ scroll; rotation ≥ `DIAL_ARM_DEG` ⇒ the twist owns it. Progress is
    /// `(|Δφ| − arm) / (commit − arm)`. At 1 the ring commits; winding back to 0 after
    /// a commit closes it. A pinch with no rotation never arms and moves no centroid.
    fn dial_step(&mut self, id: u64, t: f64) -> Option<Vec<Act>> {
        let dial = self.dial?;
        if !dial.armed && self.scroll_emitted {
            return None;
        }
        // Judge rotation only against a current other-finger position: this same input
        // frame, or older than a pivot's stillness. In between, that finger's event for
        // this frame has not arrived, and the pair vector is stale.
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
        // Signed rotation of the pair vector; positive is clockwise on a y-down screen.
        let phi = cross.atan2(dot).to_degrees();
        let (cx, cy) = self.centroid();
        if !dial.armed {
            let travel = (cx - dial.anchor.0).hypot(cy - dial.anchor.1);
            if travel >= DIAL_SLOP {
                return None;
            }
            // Undecided: no notch yet — a notch is final, and a real twist drifts past
            // `SCROLL_DIV` long before 10°. Follow the centroid so a later scroll starts
            // smoothly once the slop is crossed.
            if phi.abs() < DIAL_ARM_DEG {
                self.scrolling = true;
                self.scroll_anchor = (cx, cy);
                return Some(Vec::new());
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

    /// The twist is over (a finger lifted or a third landed). An armed-but-uncommitted
    /// ring winds back in; a committed ring stays open for the UI.
    fn end_dial(&mut self, _lift: bool) -> Vec<Act> {
        match self.dial.take() {
            Some(d) if d.armed && !d.committed => vec![Act::DialCancel],
            _ => Vec::new(),
        }
    }

    fn centroid(&self) -> (f32, f32) {
        let n = self.positions.len() as f32;
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        for &(px, py) in self.positions.values() {
            sx += px;
            sy += py;
        }
        (sx / n, sy / n)
    }

    /// Two fingers: scroll by centroid delta, never move the cursor. One notch per
    /// `SCROLL_DIV` px, then re-anchor. Finger-up / finger-right match host WHEEL(120).
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

    /// Three or more fingers: no scroll, no cursor. Travel past `TAP_SLOP` disqualifies
    /// the tap. Clear scroll and the tracked finger so a 3→2 drop cannot fire a notch
    /// from the centroid jump or a stale track position.
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

    fn single_finger(&mut self, id: u64, wx: f32, wy: f32, abs: Abs, t: f64) -> Vec<Act> {
        let mut acts = Vec::new();
        if (wx - self.start.0).abs() > TAP_SLOP || (wy - self.start.1).abs() > TAP_SLOP {
            self.moved = true;
        }
        if !self.trackpad {
            acts.push(Act::MoveAbs(abs));
            return acts;
        }
        // Zero delta this frame if the tracked finger changed, so lifting one of several
        // fingers never jumps the cursor.
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
        let speed = dx.hypot(dy) / dt_ms;
        let accel = (1.0 + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).max(0.0)).min(ACCEL_MAX);
        let gain = POINTER_SENS * accel;
        self.carry.0 += dx * gain;
        self.carry.1 += dy * gain;
        let out_x = self.carry.0 as i32; // toward zero; remainder keeps the sign
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

/// px. No finger drag moves this far in one SDL event; a leaked absolute position does.
const LEAK_PX: f32 = 150.0;

/// Gaming Mode: Steam Input owns the touchscreen and replays it as a mouse whose
/// "relative" deltas are absolute positions. Under the stream's relative-mouse lock
/// those walk the host cursor into a corner. SDL sees no fingers, so every touch
/// model is bypassed; `SDL_TOUCH_MOUSE_EVENTS` cannot help (the events are Steam's).
/// Drop a gamescope session with no finger yet and a delta no finger drag produces.
/// A real mouse in Gaming Mode keeps working: its deltas are small.
pub struct SteamTouchMouse {
    game_mode: bool,
    /// A direct-touch finger reached SDL this session: the touchscreen is ours.
    pub fingers_seen: bool,
    /// A non-direct finger was ignored this session — said once in the log.
    pub indirect_seen: bool,
    leaked: bool,
    noticed: bool,
}

impl SteamTouchMouse {
    pub fn new(game_mode: bool) -> Self {
        Self {
            game_mode,
            fingers_seen: false,
            indirect_seen: false,
            leaked: false,
            noticed: false,
        }
    }

    pub fn leaks(&mut self, xrel: f32, yrel: f32) -> bool {
        if !self.game_mode || self.fingers_seen {
            return false;
        }
        let leak = xrel.abs() >= LEAK_PX || yrel.abs() >= LEAK_PX;
        if leak {
            self.leaked = true;
        }
        leak
    }

    /// `true` once, after the first leak: the caller shows the notice.
    pub fn take_notice(&mut self) -> bool {
        let first = self.leaked && !self.noticed;
        if first {
            self.noticed = true;
        }
        first
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gaming_mode_drops_leaked_touch_positions_until_a_finger_is_seen() {
        let mut m = super::SteamTouchMouse::new(true);
        assert!(!m.leaks(4.0, -3.0));
        assert!(!m.take_notice());
        assert!(m.leaks(340.0, 12.0));
        assert!(m.take_notice());
        assert!(!m.take_notice());
        assert!(m.leaks(0.0, -420.0));
        // A real finger reached SDL: stop treating large deltas as leaks.
        m.fingers_seen = true;
        assert!(!m.leaks(340.0, 12.0));
        // Outside Gaming Mode a mouse may move that far in one event.
        let mut d = super::SteamTouchMouse::new(false);
        assert!(!d.leaks(340.0, 12.0));
        assert!(!d.take_notice());
    }

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
        // 40 px in 16 ms: acceleration should exceed 1:1.
        let acts = g.motion(1, 140.0, 100.0, ABS, 16.0);
        match acts.as_slice() {
            [Act::MoveRel { dx, dy }] => {
                assert!(*dx >= 40, "expected accelerated dx ≥ raw 40, got {dx}");
                assert_eq!(*dy, 0);
            }
            other => panic!("expected one MoveRel, got {other:?}"),
        }
        // Moved: the lift is not a tap.
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
        // Both up 40 px: centroid up → positive (finger-up) notches.
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
        // Twins use a three-finger swipe for a keyboard; emit nothing, and travel kills the tap.
        let mut acts = g.motion(1, 100.0, 160.0, ABS, 10.0);
        acts.extend(g.motion(2, 130.0, 160.0, ABS, 12.0));
        acts.extend(g.motion(3, 160.0, 160.0, ABS, 14.0));
        assert_eq!(acts, vec![], "a three-finger drag must emit nothing");
        acts.extend(g.up(1, 40.0));
        acts.extend(g.up(2, 41.0));
        acts.extend(g.up(3, 42.0));
        assert_eq!(acts, vec![]);
    }

    #[test]
    fn tap_then_press_drag_holds_the_left_button() {
        let mut g = Gestures::new(true, false);
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
        let down2 = g.down(2, 52.0, 51.0, ABS, 120.0);
        assert_eq!(
            down2,
            vec![Act::Button {
                gs: BTN_LEFT,
                down: true
            }]
        );
        let _ = g.motion(2, 90.0, 51.0, ABS, 140.0);
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
        // 7° steps so no sample sits on a threshold where float rounding picks the side.
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
        // Committed: lift leaves the ring open.
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
    fn a_drifting_twist_still_arms_and_scrolls_nothing() {
        // Real fingers drift a few px per sample while turning. Under the slop that
        // drift is not a scroll — one notch would lock the gesture before 10°.
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = Vec::new();
        for step in 1..=5 {
            let c = (120.0 + 2.0 * step as f32, 200.0 - 2.0 * step as f32); // ~14 px in all
            let (p1, p2) = twisted(c, 7.0 * step as f32);
            acts.extend(g.motion(1, p1.0, p1.1, ABS, 10.0 * step as f64));
            acts.extend(g.motion(2, p2.0, p2.1, ABS, 10.0 * step as f64 + 1.0));
        }
        assert!(
            !acts.iter().any(|a| matches!(a, Act::Scroll { .. })),
            "no notch while the pair is undecided: {acts:?}"
        );
        assert!(
            acts.iter().any(|a| matches!(a, Act::Dial { .. })),
            "the twist arms despite the drift: {acts:?}"
        );
        assert!(acts.contains(&Act::DialCommit), "35° commits: {acts:?}");
    }

    #[test]
    fn a_scroll_then_a_rotation_stays_a_scroll() {
        let mut g = Gestures::new(true, false);
        let c = (120.0, 200.0);
        let _ = g.down(1, 100.0, 200.0, ABS, 0.0);
        let _ = g.down(2, 140.0, 200.0, ABS, 2.0);
        let mut acts = g.motion(1, 100.0, 170.0, ABS, 10.0); // 30 px up: past the slop
        acts.extend(g.motion(2, 140.0, 170.0, ABS, 11.0));
        assert!(acts.iter().any(|a| matches!(a, Act::Scroll { .. })));
        let (p1, p2) = twisted((c.0, 170.0), 40.0); // twist after the scroll already owns it
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
        let _ = g.motion(1, 90.0, 50.0, ABS, 620.0);
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
        assert!(g.up(1, 700.0).is_empty());

        let _ = g.down(2, 50.0, 50.0, ABS, 1000.0);
        let _ = g.down(3, 80.0, 50.0, ABS, 1010.0);
        let _ = g.up(3, 1020.0); // second finger came and went: no press
        assert!(g.tick(1600.0).is_empty());
    }

    #[test]
    fn reset_clears_a_drag_without_re_emitting() {
        let mut g = Gestures::new(true, false);
        let _ = g.down(1, 50.0, 50.0, ABS, 0.0);
        let _ = g.up(1, 5.0);
        let _ = g.down(2, 51.0, 50.0, ABS, 50.0);
        g.reset();
        // Reset must not leave a stuck drag: a later tap is an ordinary click.
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
