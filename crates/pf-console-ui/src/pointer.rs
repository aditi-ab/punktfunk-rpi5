//! Pointer and touch input inside the console.
//!
//! Widgets act on press, not release. Focused list and carousel items scroll toward
//! the centre, so the pressed row has already moved by lift; click-on-release would
//! hit the wrong row. There is no drag gesture competing with press-to-act.
//!
//! Coordinates are device pixels: the run loop converts (it owns the window and the
//! display scale). A widget hit-tests the rect it drew last frame.

use skia_safe::Rect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pointer {
    pub x: f64,
    pub y: f64,
    pub kind: PointerKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PointerKind {
    /// Primary button down, or a finger down — the acting edge.
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
    pub(crate) fn press(&self) -> bool {
        self.kind == PointerKind::Press
    }

    /// Half-open, so neighbours can share an edge. An empty rect never hits — culled
    /// list rows store `Rect::new_empty()` and keep their indices aligned.
    pub(crate) fn hits(&self, rect: Rect) -> bool {
        let (x, y) = (self.x as f32, self.y as f32);
        x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
    }

    pub(crate) fn pick(&self, rects: &[Rect]) -> Option<usize> {
        rects.iter().position(|r| self.hits(*r))
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
