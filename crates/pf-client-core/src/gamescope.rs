//! Whether a gamescope compositor is actually on the other end.
//!
//! `GAMESCOPE_WAYLAND_DISPLAY` is not proof: the flatpak sets it on every launch so the
//! vendored gamescope WSI layer can negotiate HDR10 (`packaging/flatpak/io.unom.Punktfunk.yml`).
//! Callers that treated the variable as Gaming Mode then fullscreened a GNOME/KDE session.
//!
//! [`under_gamescope`] is the only check. With `WAYLAND_DISPLAY` set, identity with the
//! gamescope name means we are nested under that compositor. Gaming Mode is X11 (`DISPLAY=:1`,
//! no `WAYLAND_DISPLAY`); the live `xdg-run` socket named by the variable is then the proof.

use std::ffi::OsStr;
use std::path::Path;

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

/// Testable form of [`under_gamescope`]. Does not read the process environment.
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

    #[test]
    fn only_a_real_gamescope_counts() {
        let gs = Some(OsStr::new("gamescope-0"));
        let present = |_: &OsStr| true;
        let absent = |_: &OsStr| false;

        // Manifest sets gamescope-0; desktop compositor is wayland-0.
        assert!(!decide(gs, Some(OsStr::new("wayland-0")), absent));
        // Socket existence is not enough while WAYLAND_DISPLAY names the desktop.
        assert!(!decide(gs, Some(OsStr::new("wayland-0")), present));
        assert!(decide(gs, gs, absent));
        // Gaming Mode is X11: no WAYLAND_DISPLAY; the named socket is the proof.
        assert!(decide(gs, None, present));
        assert!(!decide(gs, None, absent));
        assert!(!decide(None, None, present));
    }
}
