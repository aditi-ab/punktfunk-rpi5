//! Pointer and touch input inside the console.
//!
//! Widgets act on press, not release. Focused list and carousel items scroll toward
//! the centre, so the pressed row has already moved by lift; click-on-release would
//! hit the wrong row. A finger's press reaches them only at its lift: `Touch` turns
//! a drag into scroll ticks and a tap into a press at the contact point.
//!
//! Coordinates are device pixels: the run loop converts (it owns the window and the
//! display scale). A widget hit-tests the rect it drew last frame.

use pf_client_core::console::{PointerButton, PointerInput};
use skia_safe::Rect;

/// Max finger wander (design units × `k`) that still counts as a tap. 12 dp is
/// classic touch slop; in device pixels it matches Android ViewConfiguration.
const TOUCH_SLOP_DP: f64 = 12.0;
/// Dominant-axis travel (design units × `k`) per synthetic scroll tick. 56 is
/// the menu row pitch (`widgets::ROW_H` + gap), so the list tracks the finger.
pub(crate) const DRAG_TICK_DP: f64 = 56.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    pub x: f64,
    pub y: f64,
    pub kind: PointerKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerKind {
    /// Primary button down, or a finger's tap (sent at its lift) — the acting edge.
    Press,
    /// Primary button or finger up. Ignored today; kept so a drag can close.
    Release,
    /// Motion, with or without a button held.
    Move,
    /// Gesture abandoned (pointer left the window).
    Cancel,
    /// One scroll step; `up` = away from the user.
    Scroll { up: bool },
    /// Secondary (right) button down — the pointer's B. The shell handles it for every screen.
    Back,
}

impl Pointer {
    pub fn press(&self) -> bool {
        self.kind == PointerKind::Press
    }

    /// Half-open, so neighbours can share an edge. An empty rect never hits — culled
    /// list rows store `Rect::new_empty()` and keep their indices aligned.
    pub fn hits(&self, rect: Rect) -> bool {
        let (x, y) = (self.x as f32, self.y as f32);
        x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
    }

    pub fn pick(&self, rects: &[Rect]) -> Option<usize> {
        rects.iter().position(|r| self.hits(*r))
    }
}

/// Host pointer events as [`Pointer`]s: one translator per surface, so every surface
/// reads a finger the same way.
///
/// A mouse press acts on contact. A finger down arms; a lift within slop is a tap,
/// sent as Press + Release at the *anchor*, since widgets hit-test last frame's rects
/// and the focused item may have scrolled since. Past slop the gesture axis-locks, and
/// every [`DRAG_TICK_DP`]·k of travel is one Scroll tick at the anchor; that lift acts
/// on nothing. A second finger is ignored. Secondary-down is Back and its release is
/// dropped, or a right-click would pop two screens. Wheel is discrete scroll.
#[derive(Default)]
pub(crate) struct Touch {
    gesture: Option<Gesture>,
}

#[derive(Clone, Copy, Debug)]
enum Gesture {
    /// Finger down, still within slop.
    Armed { x: f64, y: f64 },
    /// Slop exceeded. Axis-locked from the first exit so diagonal jitter cannot
    /// alternate a carousel with a list. `last` is the last tick's dominant-axis pos.
    Drag {
        x: f64,
        y: f64,
        horizontal: bool,
        last: f64,
    },
}

impl Touch {
    /// One host event at design scale `k`. `deliver` takes each [`Pointer`] it becomes
    /// and says whether it was consumed; so does the return value.
    pub(crate) fn feed(
        &mut self,
        input: PointerInput,
        k: f64,
        mut deliver: impl FnMut(Pointer) -> bool,
    ) -> bool {
        let (x, y, kind) = match input {
            PointerInput::Move { x, y } => {
                if self.gesture.is_some() {
                    self.drag(f64::from(x), f64::from(y), k, &mut deliver);
                    return true;
                }
                (x, y, PointerKind::Move)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch,
            } => {
                if touch {
                    if self.gesture.is_none() {
                        self.gesture = Some(Gesture::Armed {
                            x: f64::from(x),
                            y: f64::from(y),
                        });
                    }
                    return true;
                }
                (x, y, PointerKind::Press)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Secondary,
                ..
            } => (x, y, PointerKind::Back),
            PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            } => match self.gesture.take() {
                Some(Gesture::Armed { x, y }) => {
                    let consumed = deliver(Pointer {
                        x,
                        y,
                        kind: PointerKind::Press,
                    });
                    deliver(Pointer {
                        x,
                        y,
                        kind: PointerKind::Release,
                    });
                    return consumed;
                }
                // Drag: lift acts on nothing; ticks already fired.
                Some(Gesture::Drag { .. }) => return true,
                None => (x, y, PointerKind::Release),
            },
            PointerInput::Up { .. } => return true,
            PointerInput::Wheel { x, y, dy } => {
                if dy == 0.0 {
                    return true;
                }
                (x, y, PointerKind::Scroll { up: dy > 0.0 })
            }
            PointerInput::Cancel => {
                self.reset();
                (0.0, 0.0, PointerKind::Cancel)
            }
        };
        deliver(Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind,
        })
    }

    /// Forget a live finger without acting; its lift then arrives as a plain Release.
    pub(crate) fn reset(&mut self) {
        self.gesture = None;
    }

    /// Advance a finger Move. Past slop, lock to the dominant axis; every
    /// [`DRAG_TICK_DP`]·k of travel is one scroll tick at the anchor.
    /// Down/right = previous (wheel-up); up/left = next.
    fn drag(&mut self, x: f64, y: f64, k: f64, deliver: &mut impl FnMut(Pointer) -> bool) {
        match self.gesture {
            Some(Gesture::Armed { x: ax, y: ay }) => {
                let (dx, dy) = (x - ax, y - ay);
                if dx.hypot(dy) >= TOUCH_SLOP_DP * k {
                    let horizontal = dx.abs() > dy.abs();
                    self.gesture = Some(Gesture::Drag {
                        x: ax,
                        y: ay,
                        horizontal,
                        // Ticks start where slop was left, not at the anchor.
                        last: if horizontal { x } else { y },
                    });
                }
            }
            Some(Gesture::Drag {
                x: ax,
                y: ay,
                horizontal,
                last,
            }) => {
                let pos = if horizontal { x } else { y };
                let tick = DRAG_TICK_DP * k;
                let steps = ((pos - last) / tick).trunc();
                if steps != 0.0 {
                    self.gesture = Some(Gesture::Drag {
                        x: ax,
                        y: ay,
                        horizontal,
                        last: last + steps * tick,
                    });
                    let up = steps > 0.0;
                    for _ in 0..steps.abs() as u32 {
                        deliver(Pointer {
                            x: ax,
                            y: ay,
                            kind: PointerKind::Scroll { up },
                        });
                    }
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Pointer {
        Pointer {
            x,
            y,
            kind: PointerKind::Press,
        }
    }

    #[test]
    fn hit_testing_is_half_open_and_skips_empty_rects() {
        let r = Rect::from_xywh(10.0, 10.0, 20.0, 20.0);
        assert!(at(10.0, 10.0).hits(r), "the top-left corner is inside");
        assert!(
            !at(30.0, 20.0).hits(r),
            "the right edge belongs to the next"
        );
        assert!(!at(9.0, 20.0).hits(r));
        assert!(!at(0.0, 0.0).hits(Rect::new_empty()));
    }

    #[test]
    fn pick_returns_the_first_match() {
        let rects = [
            Rect::new_empty(),
            Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
            Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        ];
        assert_eq!(at(5.0, 5.0).pick(&rects), Some(1));
        assert_eq!(at(50.0, 5.0).pick(&rects), None);
    }
}
