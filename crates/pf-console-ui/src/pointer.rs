//! Pointer and touch input inside the console.
//!
//! The console is a focus UI: a pad moves a cursor and presses A. A pointer brings its
//! own cursor, so every widget resolves a press directly onto whatever is under it and
//! **acts on the press**, not on the release.
//!
//! That is deliberate, not a shortcut. Both the menu list and the two carousels scroll
//! the FOCUSED item toward the centre of the screen, so the thing you pressed has already
//! slid out from under your finger by the time it lifts. A click-on-release rule would
//! have to chase it, and on a touchscreen — where the finger doesn't move but the content
//! does — it would routinely land on the wrong row. Press-to-act has no such race, and
//! the console has no drag gesture for it to compete with.
//!
//! Coordinates are device pixels: the run loop converts (it owns the window and therefore
//! the display scale), and a widget hit-tests the very rect it drew last frame.

use skia_safe::Rect;

/// A pointer/touch interaction, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pointer {
    pub x: f64,
    pub y: f64,
    pub kind: PointerKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PointerKind {
    /// The primary button went down, or a finger touched the glass — the acting edge.
    Press,
    /// The primary button or finger came up. Widgets ignore it today; it is carried so a
    /// later drag gesture has an edge to close on.
    Release,
    /// Motion, with or without a button held.
    Move,
    /// The gesture was abandoned (the pointer left the window).
    Cancel,
    /// One scroll step; `up` = away from the user.
    Scroll { up: bool },
    /// The secondary (right) button went down — the pointer's B. Handled by the shell for
    /// every screen at once, so no screen has to remember to offer a way back.
    Back,
}

impl Pointer {
    /// Is this the edge widgets act on?
    pub(crate) fn press(&self) -> bool {
        self.kind == PointerKind::Press
    }

    /// Inside `rect`? Half-open, so neighbouring rects can share an edge without both
    /// claiming the same pixel. An EMPTY rect never hits — which is what lets a list
    /// record `Rect::new_empty()` for rows it culled and keep its indices aligned.
    pub(crate) fn hits(&self, rect: Rect) -> bool {
        let (x, y) = (self.x as f32, self.y as f32);
        x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
    }

    /// The index of the first rect under the pointer.
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
        // A culled row's placeholder must never swallow a press.
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
