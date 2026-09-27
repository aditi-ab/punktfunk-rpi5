//! The window, presenter, overlay and pads the run loop owns.

use super::*;

/// Apply capture to the window: pointer lock (relative mouse + hidden cursor) and a
/// keyboard grab so system chords reach the host while captured. SDL implements the
/// grab per platform (low-level hook / shortcuts-inhibit / XGrabKeyboard).
///
/// `inhibit` is [`Settings::inhibit_shortcuts`] — off leaves system chords with the
/// local shell. It only ever *removes* a grab: releasing input always hands chords back.
///
/// The `desktop` mouse model never locks: the pointer roams freely and the local cursor
/// is hidden over the window. The keyboard grab follows `inhibit` in both models —
/// desktop mode's unlocked pointer clicking another window is the way back.
/// `desktop` only matters while `on`.
///
/// `grants`: no pointer lock without POINTER, no keyboard grab without KEYBOARD.
/// On-sites pass `Capture::grants()`; off-sites pass `0`.
pub(super) fn apply_capture(
    window: &mut sdl3::video::Window,
    mouse: &sdl3::mouse::MouseUtil,
    on: bool,
    desktop: bool,
    inhibit: bool,
    grants: u32,
) {
    use punktfunk_core::quic::{GRANT_KEYBOARD, GRANT_POINTER};
    let pointer = grants & GRANT_POINTER != 0;
    mouse.set_relative_mouse_mode(window, on && !desktop && pointer);
    // The local cursor hides only while the host's cursor stands in for it — without
    // POINTER no send lands, so hiding it would leave a keyboard-only session with no cursor.
    mouse.show_cursor(!(on && pointer));
    let grab = on && inhibit && grants & GRANT_KEYBOARD != 0;
    if !window.set_keyboard_grab(grab) && grab {
        // The one refusal SDL reports is a missing mechanism. Said once per process: the
        // answer never changes mid-session. Under gamescope that is expected (it has no
        // shortcuts of its own) so it stays at debug rather than warning once per stream.
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            let err = sdl3::get_error();
            if pf_client_core::gamescope::under_gamescope() {
                tracing::debug!(error = %err, "no keyboard grab under gamescope — chords already ours");
            } else {
                tracing::warn!(
                    error = %err,
                    "capture system shortcuts is on, but this compositor offers no way to grab \
                     the keyboard — system chords stay with the local shell"
                );
            }
        }
    }
}

/// Overlay chrome UI scale: SDL's window display scale times `PUNKTFUNK_OSD_SCALE`.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; a 0
/// multiplier would collapse the OSD to an invisible panel. The 4× ceiling keeps a
/// bogus scale from covering the stream.
pub(super) fn overlay_scale(display_scale: f32, pref: f32) -> f32 {
    let base = if display_scale.is_finite() && display_scale > 0.0 {
        display_scale
    } else {
        1.0
    };
    let pref = if pref.is_finite() && pref > 0.0 {
        pref
    } else {
        1.0
    };
    (base * pref).clamp(0.5, 4.0)
}

/// How long an access toast holds the pill slot. The chip keeps the standing truth.
pub(super) const ACCESS_NOTICE_S: u64 = 6;

/// Capture hints (`ui_stream` parity — the words the user reads while released).
pub(super) const HINT_KEYBOARD: &str =
    "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+M mouse mode · Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats";
pub(super) const HINT_WITH_PAD: &str =
    "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+D disconnects · hold L1 + R1 + Start + Select to leave";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_scale_follows_dpi_and_survives_a_bogus_display() {
        assert_eq!(overlay_scale(1.0, 1.0), 1.0);
        assert_eq!(overlay_scale(1.5, 1.0), 1.5);
        assert_eq!(overlay_scale(2.0, 1.0), 2.0);
        // PUNKTFUNK_OSD_SCALE multiplies the display's own scale, it does not replace it.
        assert_eq!(overlay_scale(2.0, 1.25), 2.5);
        // SDL reports 0.0 when it cannot resolve the window's display — must not collapse
        // the panel to nothing.
        assert_eq!(overlay_scale(0.0, 1.0), 1.0);
        assert_eq!(overlay_scale(f32::NAN, 1.0), 1.0);
        assert_eq!(overlay_scale(-2.0, 1.0), 1.0);
        // A garbage preference degrades to "just the DPI", never to zero.
        assert_eq!(overlay_scale(1.5, 0.0), 1.5);
        assert_eq!(overlay_scale(1.5, f32::NAN), 1.5);
        // Clamped both ways so nothing can hide the OSD or bury the stream under it.
        assert_eq!(overlay_scale(1.0, 100.0), 4.0);
        assert_eq!(overlay_scale(1.0, 0.01), 0.5);
    }
}
