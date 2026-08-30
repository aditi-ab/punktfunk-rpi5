//! The shell's icons: [Lucide](https://lucide.dev), the same marks the console (gamepad UI) and
//! the GTK shell draw, from the same masters — `assets/lucide/*.svg`, baked by
//! `scripts/gen-lucide-assets.sh`.
//!
//! The other two shells stroke the path data at run time and get the theme's colour for free.
//! This one cannot: windows-reactor has no vector element, its `Image` takes a raster URI, and
//! it builds every `BitmapIcon` with `ShowAsMonochrome(false)` — so a WinUI icon can be neither
//! drawn from a path nor tinted. The marks therefore ship pre-coloured and pre-sized, in the
//! three bakes the shell needs:
//!
//! * [`icon`] — mid-grey, the same `#8A8F98` the OS and launcher marks use, chosen once to stay
//!   legible on both the light and the dark WinUI theme.
//! * [`icon_on`] — white, for a control whose ground is the accent colour on both themes.
//! * [`ring_uri`] — white and much larger, for the quick-action ring's discs.
//!
//! ⚠ **The bake size IS the on-screen size** for anything handed to reactor's `.icon()`. It
//! builds a `BitmapIcon` and drops it into the control with no size set, and a `BitmapIcon`
//! measures at its source PIXEL count, read as DIPs — where the `SymbolIcon` it replaced
//! self-sized to its glyph. A `NavigationViewItem`'s template happens to clamp its icon and an
//! ordinary `Button` does not, so an oversized bake looks correct in the sidebar and enormous
//! on every button. The ring is the one exception: it draws through `Image`, which takes an
//! explicit width and height, so its bake can be as large as it likes.
//!
//! All three sets are embedded in the exe and materialized once into `%LOCALAPPDATA%\punktfunk\`,
//! the same disk-cache-to-URI pattern as [`super::os_icons`] and [`super::launcher_icons`] — and
//! for the same reason.

use std::path::PathBuf;
use std::sync::OnceLock;
use windows_reactor::Icon;

/// The shipped-token list: one line per mark, all three bakes. A name that is not here has no
/// art, and the control it was meant for renders without an icon — so [`every_name_ships`]
/// holds the shell to this list.
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8], &[u8], &[u8])] = &[$((
            $name,
            include_bytes!(concat!("../../assets/lucide/", $name, ".png")),
            include_bytes!(concat!("../../assets/lucide-on/", $name, ".png")),
            include_bytes!(concat!("../../assets/lucide-ring/", $name, ".png")),
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

/// The three cache directories, in the order [`ICONS`] carries them: grey, white, ring.
fn dirs() -> Option<[PathBuf; 3]> {
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("punktfunk");
    Some([
        base.join("lucide"),
        base.join("lucide-on"),
        base.join("lucide-ring"),
    ])
}

/// Materialize the embedded PNGs to disk (idempotent; a size mismatch rewrites, so a refreshed
/// mark — or a re-baked SIZE — in a newer build lands). Called once at GUI startup, before
/// anything renders.
pub(super) fn install() {
    let Some(dirs) = dirs() else { return };
    for d in &dirs {
        if std::fs::create_dir_all(d).is_err() {
            return; // controls just render without their marks
        }
    }
    for (token, grey, white, ring) in ICONS {
        for (dir, bytes) in dirs.iter().zip([grey, white, ring]) {
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

fn cached() -> &'static Option<[PathBuf; 3]> {
    static DIRS: OnceLock<Option<[PathBuf; 3]>> = OnceLock::new();
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

/// One bake's `file:///` URI, empty when there is nowhere to cache it.
fn uri(bake: usize, name: &str) -> String {
    match cached() {
        Some(dirs) => file_uri(&dirs[bake], name),
        None => String::new(),
    }
}

/// A mark for an ordinary control — a subtle button, a list row, a navigation item.
pub(super) fn icon(name: &str) -> Icon {
    Icon::bitmap(uri(0, name))
}

/// A mark for an ACCENT-filled control, whose ground is the accent colour on both themes. The
/// grey bake reads as smudged there; this one is white, like the on-accent text beside it.
pub(super) fn icon_on(name: &str) -> Icon {
    Icon::bitmap(uri(1, name))
}

/// The `file:///` URI of a mark's large WHITE bake — for an [`windows_reactor::Image`] laid on a
/// dark surface of our own, where no theme brush applies and the caller sets the size itself.
/// The quick-action ring's discs are the only such surface.
pub(super) fn ring_uri(name: &str) -> String {
    uri(2, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark has all three bakes and none is empty. A missing bake is an icon that
    /// silently disappears on exactly the surfaces that use it.
    ///
    /// The ring bake must also be much heavier than the button bakes. That is the guard on the
    /// trap in the module docs: a button mark's pixel count IS its on-screen size, so re-baking
    /// those at the ring's size would put a 96 DIP icon on every button. Comparable sizes here
    /// mean someone collapsed the two.
    #[test]
    fn every_mark_has_three_bakes_and_the_ring_one_is_larger() {
        assert!(!ICONS.is_empty());
        for (name, grey, white, ring) in ICONS {
            assert!(grey.len() > 40, "{name}: the grey bake is empty");
            assert!(white.len() > 40, "{name}: the white bake is empty");
            assert!(ring.len() > 100, "{name}: the ring bake is empty");
            assert_ne!(grey, white, "{name}: the grey and white bakes are one file");
            assert!(
                ring.len() > white.len() * 2,
                "{name}: the ring bake ({} bytes) is not larger than the button bake ({}) \
                 — the button marks would render at the ring's size",
                ring.len(),
                white.len()
            );
        }
    }

    /// The shipped list has no duplicates, and every name really is a Lucide mark this
    /// workspace ships — a typo here bakes nothing and shows nothing.
    #[test]
    fn every_name_ships() {
        let mut names: Vec<&str> = ICONS.iter().map(|(n, _, _, _)| *n).collect();
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
        let baked = |n: &str| ICONS.iter().any(|(t, _, _, _)| *t == n);
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
