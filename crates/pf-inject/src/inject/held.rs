//! What a session pressed and has not released. The injector outlives every session, so a
//! client that vanishes mid-press leaves the button, key or finger latched for the next one
//! (Mutter keeps the implicit grab; the Windows touch refresher re-injects a held contact).

use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::HashSet;

/// Per kind. A flood of never-released codes cannot grow the sets; codes past it stay latched.
const MAX_HELD: usize = 256;

/// Fed every event on its way to the injector; [`Self::release`] at session end.
#[derive(Default)]
pub struct HeldInput {
    buttons: HashSet<u32>,
    keys: HashSet<u32>,
    touch: HashSet<u32>,
}

impl HeldInput {
    pub fn note(&mut self, ev: &InputEvent) {
        let (set, down) = match ev.kind {
            InputKind::MouseButtonDown => (&mut self.buttons, true),
            InputKind::MouseButtonUp => (&mut self.buttons, false),
            InputKind::KeyDown => (&mut self.keys, true),
            InputKind::KeyUp => (&mut self.keys, false),
            // Only an Up ends a contact: the refresher defeats Windows' own staleness lift.
            InputKind::TouchDown => (&mut self.touch, true),
            InputKind::TouchUp => (&mut self.touch, false),
            _ => return,
        };
        if !down {
            set.remove(&ev.code);
        } else if set.len() < MAX_HELD {
            set.insert(ev.code);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buttons.is_empty() && self.keys.is_empty() && self.touch.is_empty()
    }

    /// The matching up for everything still held; leaves nothing held.
    pub fn release(&mut self) -> Vec<InputEvent> {
        let up = |kind, code| InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        };
        let mut out = Vec::new();
        out.extend(
            self.buttons
                .drain()
                .map(|c| up(InputKind::MouseButtonUp, c)),
        );
        out.extend(self.keys.drain().map(|c| up(InputKind::KeyUp, c)));
        out.extend(self.touch.drain().map(|c| up(InputKind::TouchUp, c)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: InputKind, code: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        }
    }

    #[test]
    fn only_what_is_still_down_is_released_once() {
        let mut held = HeldInput::default();
        for e in [
            ev(InputKind::MouseButtonDown, 1),
            ev(InputKind::MouseButtonDown, 3),
            ev(InputKind::MouseButtonUp, 1),
            ev(InputKind::KeyDown, 42),
            ev(InputKind::TouchDown, 7),
            ev(InputKind::MouseMove, 0),
        ] {
            held.note(&e);
        }
        let mut ups: Vec<_> = held.release().iter().map(|e| (e.kind, e.code)).collect();
        ups.sort_by_key(|&(k, c)| (k as u8, c));
        assert_eq!(
            ups,
            [
                (InputKind::KeyUp, 42),
                (InputKind::MouseButtonUp, 3),
                (InputKind::TouchUp, 7)
            ]
        );
        assert!(held.is_empty());
        assert!(held.release().is_empty());
    }
}
