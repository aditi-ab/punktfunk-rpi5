//! "Are we really running under gamescope?" — one answer, because the obvious test lies.
//!
//! `GAMESCOPE_WAYLAND_DISPLAY` used to be read as proof on its own: gamescope exports it to its
//! children, so its presence meant Gaming Mode. **Our own flatpak breaks that.** The manifest sets
//! it unconditionally (`packaging/flatpak/io.unom.Punktfunk.yml`), because the vendored gamescope
//! WSI layer reads that one variable and nothing else to decide whether to negotiate HDR10 — so
//! inside the sandbox it is set on every launch, on every desktop. Every caller that took it for a
//! Gaming-Mode signal therefore fired on a plain GNOME/KDE login: the GTK shell launched
//! fullscreen, a stream ignored `fullscreen_on_stream = false`, the settings dialog swapped its
//! dropdowns for subpages, and the system-button policy picked Deck rules. Field-reported on
//! 2026-08-30 as "the GTK client just launches in fullscreen"; the manifest line landed in
//! `e1adc5d6` (2026-08-05, v0.25.0), which dates the regression.
//!
//! The variable still has to be exported — the WSI layer needs it — so the fix is here, in what
//! *we* accept as proof.

use std::ffi::OsStr;
use std::path::Path;

/// True only when a gamescope compositor is actually on the other end.
pub fn under_gamescope() -> bool {
    decide(
        std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        |name| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .is_some_and(|dir| Path::new(&dir).join(name).exists())
        },
    )
}

/// The rule, split out so it can be tested without mutating the process environment.
///
/// `WAYLAND_DISPLAY` decides it whenever we have one: a desktop session inside the flatpak still
/// gets `--socket=wayland`, so it names the DESKTOP compositor, and the mismatch is exactly what
/// the WSI layer itself bails on. Gaming Mode leaves us nothing to compare — it runs apps as X11
/// clients (`DISPLAY=:1`, no `WAYLAND_DISPLAY`) — so there the socket the variable names is the
/// proof, and the flatpak binds it (`--filesystem=xdg-run/gamescope-0`) for HDR anyway.
fn decide(
    gamescope: Option<&OsStr>,
    wayland: Option<&OsStr>,
    socket: impl Fn(&OsStr) -> bool,
) -> bool {
    let Some(gamescope) = gamescope else {
        return false;
    };
    match wayland {
        Some(wayland) => wayland == gamescope,
        None => socket(gamescope),
    }
}

#[cfg(test)]
mod tests {
    use super::decide;
    use std::ffi::OsStr;

    /// The flatpak shape that caused the field report: the manifest's variable is set, but we are
    /// on an ordinary desktop compositor. Everything else here is a shape that must still say yes.
    #[test]
    fn only_a_real_gamescope_counts() {
        let gs = Some(OsStr::new("gamescope-0"));
        let present = |_: &OsStr| true;
        let absent = |_: &OsStr| false;

        // Flatpak on a GNOME/KDE desktop: set by the manifest, desktop compositor on the socket.
        assert!(!decide(gs, Some(OsStr::new("wayland-0")), absent));
        // ...and it stays no even if some OTHER gamescope (a game) is running on the box.
        assert!(!decide(gs, Some(OsStr::new("wayland-0")), present));
        // Nested under a real gamescope: the display we are on IS the one named.
        assert!(decide(gs, gs, absent));
        // Gaming Mode as an X11 client: no WAYLAND_DISPLAY, so the live socket is the proof.
        assert!(decide(gs, None, present));
        // Same shape with no socket — nothing is listening, so nothing is there.
        assert!(!decide(gs, None, absent));
        // Never set at all: not a gamescope session by any reading.
        assert!(!decide(None, None, present));
    }
}
