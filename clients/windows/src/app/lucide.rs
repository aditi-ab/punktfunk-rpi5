//! The shell's icons: [Lucide](https://lucide.dev), the same marks the console (gamepad UI) and
//! the GTK shell draw, from the same masters — `assets/lucide/*.svg`, baked by
//! `scripts/gen-lucide-assets.sh`.
//!
//! The other two shells stroke the path data at run time and get the theme's colour for free.
//! This one cannot: windows-reactor has no vector element, its `Image` takes a raster URI, and
//! it builds every `BitmapIcon` with `ShowAsMonochrome(false)` — so a WinUI icon can be neither
//! drawn from a path nor tinted. The marks therefore ship pre-coloured, in the two colours the
//! shell actually needs:
//!
//! * [`icon`] — mid-grey, the same `#8A8F98` the OS and launcher marks use, chosen once to stay
//!   legible on both the light and the dark WinUI theme.
//! * [`icon_on`] / [`uri_on`] — white, for the two surfaces that are dark whatever the theme:
//!   an accent-filled button, and the quick-action ring's discs.
//!
//! Both sets are embedded in the exe and materialized once into `%LOCALAPPDATA%\punktfunk\`, the
//! same disk-cache-to-URI pattern as [`super::os_icons`] and [`super::launcher_icons`] — and for
//! the same reason.

use std::path::PathBuf;
use std::sync::OnceLock;
use windows_reactor::Icon;

/// The shipped-token list: one line per mark, both bakes. A name that is not here has no art,
/// and the control it was meant for renders without an icon — so [`every_name_ships`] holds the
/// shell to this list.
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8], &[u8])] = &[$((
            $name,
            include_bytes!(concat!("../../assets/lucide/", $name, ".png")),
            include_bytes!(concat!("../../assets/lucide-on/", $name, ".png")),
        )),*];
    };
}

icons![
    "arrow-left",
    "arrow-right",
    "chart-column",
    "check",
    "chevron-right",
    "copy",
    "circle-help",
    "ellipsis",
    "gamepad-2",
    "keyboard",
    "log-out",
    "maximize",
    "mic",
    "mic-off",
    "moon",
    "plus",
    "pointer",
    "power",
    "refresh-cw",
    "rotate-cw",
    "save",
    "send",
    "settings",
    "square",
    "trash-2",
    "volume-2",
    "x",
];

/// The two cache directories: the grey bake and the white one.
fn dirs() -> Option<(PathBuf, PathBuf)> {
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("punktfunk");
    Some((base.join("lucide"), base.join("lucide-on")))
}

/// Materialize the embedded PNGs to disk (idempotent; a size mismatch rewrites, so a refreshed
/// mark in a newer build lands). Called once at GUI startup, before anything renders.
pub(super) fn install() {
    let Some((grey, white)) = dirs() else { return };
    if std::fs::create_dir_all(&grey).is_err() || std::fs::create_dir_all(&white).is_err() {
        return; // controls just render without their marks
    }
    for (token, grey_png, white_png) in ICONS {
        for (dir, bytes) in [(&grey, grey_png), (&white, white_png)] {
            let p = dir.join(format!("{token}.png"));
            let fresh = std::fs::metadata(&p)
                .map(|m| m.len() != bytes.len() as u64)
                .unwrap_or(true);
            if fresh {
                let _ = std::fs::write(&p, bytes);
            }
        }
    }
}

fn cached() -> &'static Option<(PathBuf, PathBuf)> {
    static DIRS: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    DIRS.get_or_init(dirs)
}

fn file_uri(dir: &std::path::Path, name: &str) -> String {
    format!(
        "file:///{}",
        dir.join(format!("{name}.png"))
            .to_string_lossy()
            .replace('\\', "/")
    )
}

/// The `file:///` URI of a mark's grey bake, empty when there is nowhere to cache it.
fn uri(name: &str) -> String {
    match cached() {
        Some((grey, _)) => file_uri(grey, name),
        None => String::new(),
    }
}

/// The `file:///` URI of a mark's WHITE bake — for an [`windows_reactor::Image`] laid on a dark
/// surface of our own, where no theme brush applies. The ring's discs are the only such surface.
pub(super) fn uri_on(name: &str) -> String {
    match cached() {
        Some((_, white)) => file_uri(white, name),
        None => String::new(),
    }
}

/// A mark for an ordinary control — a subtle button, a list row, a navigation item.
pub(super) fn icon(name: &str) -> Icon {
    Icon::bitmap(uri(name))
}

/// A mark for an ACCENT-filled control, whose ground is the accent colour on both themes. The
/// grey bake reads as smudged there; this one is white, like the on-accent text beside it.
pub(super) fn icon_on(name: &str) -> Icon {
    Icon::bitmap(uri_on(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark is baked in BOTH colours and neither bake is empty. One missing bake is an
    /// icon that silently disappears on exactly the surfaces that use that colour.
    #[test]
    fn every_mark_is_baked_in_both_colours() {
        assert!(!ICONS.is_empty());
        for (name, grey, white) in ICONS {
            assert!(grey.len() > 100, "{name}: the grey bake is empty");
            assert!(white.len() > 100, "{name}: the white bake is empty");
            assert_ne!(grey, white, "{name}: the two bakes are the same file");
        }
    }

    /// The shipped list has no duplicates, and every name really is a Lucide mark this
    /// workspace ships — a typo here bakes nothing and shows nothing.
    #[test]
    fn every_name_ships() {
        let mut names: Vec<&str> = ICONS.iter().map(|(n, _, _)| *n).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "a name is listed twice");
        for name in names {
            assert!(
                pf_client_core::lucide::path(name).is_some(),
                "'{name}' is not in the shared icon set"
            );
        }
    }

    /// Every slot the ring can hold has its mark baked here — the ring's discs are the one
    /// place this shell CANNOT fall back to a word, because the word is what we replaced.
    #[test]
    fn every_ring_slot_has_a_baked_mark() {
        use pf_client_core::overlay_actions::{catalogue, slot_icon, OverlayConfig, RingPlatform};
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        let baked = |n: &str| ICONS.iter().any(|(t, _, _)| *t == n);
        for group in catalogue(&cfg, RingPlatform::Desktop) {
            for entry in group.entries {
                if entry.id.is_empty() {
                    continue; // the empty slot draws the plus, which is baked below
                }
                let name = slot_icon(&entry.id, "").unwrap_or_else(|| panic!("{}", entry.id));
                assert!(baked(name), "{}: '{name}' is not baked", entry.id);
            }
        }
        for name in ["mic-off", "ellipsis", "plus"] {
            assert!(baked(name), "'{name}' is not baked");
        }
    }
}
