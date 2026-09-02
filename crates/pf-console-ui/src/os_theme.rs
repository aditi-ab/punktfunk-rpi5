//! The desktop's own theme, pushed by the embedding binary ("Follow system theme").
//!
//! The console never reads a file. The session binary (or a future Android feed)
//! publishes four sRGB tuples here; a process-wide slot plus a revision lets the
//! watcher live on a worker thread and the shell compare once per frame.
//! Colours are tuples, not Skia types, so the publisher needs no Skia.

use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsTheme {
    pub light: bool,
    pub background: (f64, f64, f64),
    pub foreground: (f64, f64, f64),
    pub accent: (f64, f64, f64),
}

static CURRENT: Mutex<(u64, Option<OsTheme>)> = Mutex::new((0, None));

/// The revision moves only on a real change so a 2 s poll is free.
pub fn set_os_theme(t: Option<OsTheme>) {
    let mut cur = CURRENT.lock().unwrap();
    if cur.1 != t {
        cur.0 += 1;
        cur.1 = t;
    }
}

/// Revision for the shell's per-frame rebuild check.
pub(crate) fn os_theme() -> (u64, Option<OsTheme>) {
    *CURRENT.lock().unwrap()
}

/// The settings row keys off this, not the platform.
pub(crate) fn available() -> bool {
    CURRENT.lock().unwrap().1.is_some()
}

/// Nudge accent toward foreground until contrast vs background is ≥ 3:1 (WCAG non-text).
/// Ten steps of 0.15; an accent already equal to the foreground has nowhere to go.
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

/// sRGB lerp; `t` is how far toward `b` (not an [`OsTheme`]).
pub(crate) fn mix(a: (f64, f64, f64), b: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    let m = |x: f64, y: f64| x + (y - x) * t;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// WCAG contrast: 1.0 identical, 21.0 black on white.
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

    // Do not touch CURRENT here. libtest is parallel; the one allowed setter is
    // `screens::settings::tests`, where the row that reads it is reachable too.

    #[test]
    fn a_washed_out_accent_is_lifted_and_a_sound_one_is_left_alone() {
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
