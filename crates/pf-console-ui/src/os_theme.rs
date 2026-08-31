//! The desktop's own theme, when the embedding binary follows one ("Follow system theme").
//!
//! The console never reads a file: the SERVICE side — the session binary on Linux, watching
//! Omarchy's theme; a future Android feed — pushes the four values here whenever they change.
//! A process-wide slot with a revision, because the watcher lives on a worker thread and the
//! shell checks once per frame on the render thread; the revision is what makes that check a
//! cheap compare instead of a rebuild.
//!
//! Colours are plain sRGB tuples like [`crate::library::Palette`]'s, not Skia types, so the
//! service side needs nothing from Skia to publish one.

use std::sync::Mutex;

/// A desktop theme in the four values every consumer acts on.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsTheme {
    pub light: bool,
    pub background: (f64, f64, f64),
    pub foreground: (f64, f64, f64),
    pub accent: (f64, f64, f64),
}

static CURRENT: Mutex<(u64, Option<OsTheme>)> = Mutex::new((0, None));

/// Publish the theme, or its absence. The revision moves only on an actual change, so a 2 s
/// poll that keeps reading the same file publishes for free.
pub fn set_os_theme(t: Option<OsTheme>) {
    let mut cur = CURRENT.lock().unwrap();
    if cur.1 != t {
        cur.0 += 1;
        cur.1 = t;
    }
}

/// The current theme and its revision — the shell's per-frame rebuild check.
pub(crate) fn os_theme() -> (u64, Option<OsTheme>) {
    *CURRENT.lock().unwrap()
}

/// Is there a theme to follow at all? Gates the settings row: availability rather than
/// platform, so a platform that starts publishing needs no row edit.
pub(crate) fn available() -> bool {
    CURRENT.lock().unwrap().1.is_some()
}

/// The theme's accent, nudged toward its foreground until it separates from its background.
///
/// The curated palette table never needs this — its accents are picked against their own
/// fields — but an OS theme is arbitrary: several Omarchy light themes ship an accent that
/// is handsome as a fill and illegible as focus ink. 3:1 is WCAG's floor for non-text, which
/// is what the console's accent is (focus wash, selected pill, caret). ⚠ An accent already
/// equal to the foreground has nowhere to move, so this returns the best it managed.
pub(crate) fn readable_accent(t: &OsTheme) -> (f64, f64, f64) {
    let mut c = t.accent;
    for _ in 0..10 {
        if contrast(c, t.background) >= 3.0 {
            break;
        }
        c = mix(c, t.foreground, 0.15);
    }
    c
}

/// Straight sRGB lerp — `t` is how far toward `b`.
pub(crate) fn mix(a: (f64, f64, f64), b: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    let m = |x: f64, y: f64| x + (y - x) * t;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// WCAG contrast ratio between two colours: 1.0 identical, 21.0 black on white.
fn contrast(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let lum = |c: (f64, f64, f64)| {
        let lin = |v: f64| {
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ⚠ No test here touches the process-wide slot: the ONE allowed to (libtest runs tests
    // on parallel threads, so a second would race it) lives in `screens::settings::tests`,
    // where the row gating it feeds is reachable too.

    #[test]
    fn a_washed_out_accent_is_lifted_and_a_sound_one_is_left_alone() {
        // Everforest Light's shape: a pale accent on a pale field.
        let washed = OsTheme {
            light: true,
            background: (0.992, 0.965, 0.890),
            foreground: (0.361, 0.416, 0.447),
            accent: (0.874, 0.627, 0.0),
        };
        assert!(
            contrast(washed.accent, washed.background) < 3.0,
            "the premise"
        );
        assert!(contrast(readable_accent(&washed), washed.background) >= 3.0);

        let sound = OsTheme {
            light: false,
            background: (0.102, 0.106, 0.149),
            foreground: (0.753, 0.792, 0.961),
            accent: (0.478, 0.635, 0.969),
        };
        assert_eq!(readable_accent(&sound), sound.accent);
    }
}
