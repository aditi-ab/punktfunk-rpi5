//! The shell's icons: [Lucide](https://lucide.dev), the same marks the console (gamepad UI) and
//! the GTK shell draw, from the same masters in `assets/lucide`.
//!
//! Those two shells stroke the path data. This one cannot — windows-reactor has no vector
//! element — so it draws Lucide's own icon FONT instead, one glyph per mark, from the
//! codepoints in [`pf_client_core::lucide`]. A `FontIcon` is sized by the control and tinted
//! from the foreground brush, exactly as the `SymbolIcon` it replaced was: an icon on an accent
//! button comes out white, one in the sidebar takes the theme's own colour, and every one of
//! them is a vector at any DPI.
//!
//! 🛑 The obvious alternative — baking PNGs, the way [`super::os_icons`] and
//! [`super::launcher_icons`] carry their brand marks — was tried and is WRONG for UI icons.
//! Reactor's `.icon()` builds a `BitmapIcon` with no size set and with `ShowAsMonochrome(false)`,
//! so a bitmap is stuck at one baked colour on every theme AND renders at its source pixel count
//! read as DIPs. It looked right in the settings sidebar (a `NavigationViewItem`'s template
//! clamps its icon) and was enormous on every button (an ordinary `Button` does not). The brand
//! marks can live with a baked colour because they ARE a fixed colour; a UI icon cannot.
//!
//! ## Where the font comes from
//!
//! `ms-appx:///` resolves to the app's install folder, which for this unpackaged-or-packaged
//! shell means "next to the exe". The font gets there twice over:
//!
//! * a dev `cargo build` — `build.rs` stages it into `target/<profile>/Assets/`, beside the
//!   App SDK bootstrap that `windows-reactor-setup` puts there;
//! * a shipped build — it is checked in under `clients/windows/packaging/assets/`, which
//!   `pack-msix.ps1` copies wholesale into the layout's `Assets\`, and the installer and the
//!   portable zip are packed from that same layout.

use pf_client_core::lucide as set;
use windows_reactor::Icon;

/// The font as XAML names it: the file, then `#` and the family inside it. Both halves matter —
/// a family name alone would only find a font installed system-wide, which this one is not.
pub(super) const FAMILY: &str = "ms-appx:///Assets/lucide.ttf#lucide";

/// One mark's glyph. An unknown name yields an empty string, which draws nothing — a control
/// without its icon, rather than a "missing glyph" box. [`every_name_ships`] keeps the shell
/// from relying on that.
pub(super) fn glyph(name: &str) -> String {
    set::glyph(name).map(String::from).unwrap_or_default()
}

/// A mark for any control that takes an icon — a button, a list row, a navigation item. The
/// control sizes it and the theme colours it; there is nothing per-site to pass.
pub(super) fn icon(name: &str) -> Icon {
    Icon::font_family(glyph(name), FAMILY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark this shell asks for by name really ships, and each resolves to its own glyph
    /// in the Private Use Area. A typo would otherwise be one silently empty control.
    #[test]
    fn every_name_ships() {
        let named = [
            "arrow-left",
            "arrow-right",
            "chart-column",
            "check",
            "circle-help",
            "copy",
            "ellipsis",
            "gamepad-2",
            "keyboard",
            "maximize",
            "plus",
            "refresh-cw",
            "rotate-cw",
            "save",
            "send",
            "settings",
            "trash-2",
            "volume-2",
            "x",
        ];
        for name in named {
            let g = set::glyph(name).unwrap_or_else(|| panic!("the icon set has no '{name}'"));
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&g),
                "{name}: {g:?} is not a private-use codepoint"
            );
        }
        assert!(glyph("no-such-icon").is_empty());
    }

    /// Every slot the ring can hold has a glyph — the ring's discs are the one place this shell
    /// cannot fall back to a word, because the word is what the mark replaced.
    #[test]
    fn every_ring_slot_has_a_glyph() {
        use pf_client_core::overlay_actions::{catalogue, slot_icon, OverlayConfig, RingPlatform};
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        for group in catalogue(&cfg, RingPlatform::Desktop) {
            for entry in group.entries {
                if entry.id.is_empty() {
                    continue; // the empty slot draws the plus, checked below
                }
                let name = slot_icon(&entry.id, "").unwrap_or_else(|| panic!("{}", entry.id));
                assert!(
                    set::glyph(name).is_some(),
                    "{}: the font has no '{name}'",
                    entry.id
                );
            }
        }
        for name in ["mic-off", "ellipsis", "plus"] {
            assert!(set::glyph(name).is_some(), "the font has no '{name}'");
        }
    }

    /// The family string names a FILE and a family inside it. A bare family name would silently
    /// fall back to the UI font and draw private-use boxes, which is the failure this catches.
    #[test]
    fn the_family_names_the_bundled_file() {
        assert!(FAMILY.starts_with("ms-appx:///Assets/"));
        assert!(FAMILY.ends_with(".ttf#lucide"));
    }
}
