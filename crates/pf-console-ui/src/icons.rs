//! The console's icon set: [Lucide](https://lucide.dev) v0.462.0 (ISC — see
//! THIRD-PARTY-NOTICES.txt), the marks this shell draws, named out of the workspace's one
//! table — [`pf_client_core::lucide`], generated from `assets/lucide/*.svg`. Each icon is
//! its 24×24 SVG path data. Drawn by [`draw_icon`] through Skia's own SVG-path parser and
//! stroked at Lucide's native width 2 with round caps and joins — the marks render exactly
//! as designed, in the console's paints, at any size. The GTK shell strokes the SAME strings
//! with `gsk::Path`, so the two shells cannot draw one mark differently.
//!
//! Named here are only the marks the console's OWN chrome draws. A ring slot's mark is not
//! among them: the shared slot table hands out a NAME, which [`by_name`] resolves — one lookup
//! instead of one alias per slot, and no list here to fall out of step with that table.
//!
//! No icon font ships and no new dependency.

use crate::theme::stroke;
use skia_safe::{utils::parse_path, Canvas, Color4f, PaintCap, PaintJoin};

/// One icon's 24×24 path data.
#[derive(Clone, Copy)]
pub(crate) struct Icon(pub &'static str);

pub(crate) const CHEVRON_DOWN: Icon = Icon(pf_client_core::lucide::CHEVRON_DOWN);
pub(crate) const CHEVRON_LEFT: Icon = Icon(pf_client_core::lucide::CHEVRON_LEFT);
pub(crate) const CHEVRON_RIGHT: Icon = Icon(pf_client_core::lucide::CHEVRON_RIGHT);
pub(crate) const CHEVRON_UP: Icon = Icon(pf_client_core::lucide::CHEVRON_UP);
pub(crate) const CORNER_DOWN_LEFT: Icon = Icon(pf_client_core::lucide::CORNER_DOWN_LEFT);
pub(crate) const PLUS: Icon = Icon(pf_client_core::lucide::PLUS);

/// The mark a Lucide name stands for; `None` for a name this build does not ship. How the
/// ring's slots get their icons — they are keyed by name in the shared table, not here.
pub(crate) fn by_name(name: &str) -> Option<Icon> {
    pf_client_core::lucide::path(name).map(Icon)
}

/// Draw `icon` centred on `(x, y)`, its 24-unit box scaled to `box_px` across, stroked in
/// `color`. The stroke is 2 icon units — Lucide's own weight — so it scales with the box.
pub(crate) fn draw_icon(canvas: &Canvas, icon: Icon, x: f32, y: f32, box_px: f32, color: Color4f) {
    let Some(path) = parse_path::from_svg(icon.0) else {
        return;
    };
    let f = box_px / 24.0;
    let mut p = stroke(color, 2.0);
    p.set_stroke_cap(PaintCap::Round);
    p.set_stroke_join(PaintJoin::Round);
    canvas.save();
    canvas.translate((x - 12.0 * f, y - 12.0 * f));
    canvas.scale((f, f));
    canvas.draw_path(&path, &p);
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every SHIPPED icon's data parses with Skia's own SVG-path parser and stays inside the
    /// 24-unit box — the whole shared table, not just the marks this shell names, because the
    /// GTK shell strokes the same strings and has no parser of its own to fail loudly. The
    /// TIGHT bounds, not `bounds()`: an arc's conic control points sit outside the curve they
    /// draw, so the loose bounds flag a correct circle. A typo in a generated path string
    /// would otherwise be a silently missing mark on two shells.
    #[test]
    fn every_icon_parses_and_fits_its_box() {
        for (name, data) in pf_client_core::lucide::ALL {
            let path =
                parse_path::from_svg(data).unwrap_or_else(|| panic!("{name} does not parse"));
            let b = path.compute_tight_bounds();
            assert!(
                b.left >= -0.5 && b.top >= -0.5 && b.right <= 24.5 && b.bottom <= 24.5,
                "{name} leaves the box: {b:?}"
            );
        }
    }

    /// The marks this shell names really are the shared table's, not a stale copy of one, and
    /// a name it does not ship resolves to nothing rather than to the wrong mark.
    #[test]
    fn the_consoles_icons_come_from_the_shared_table() {
        assert_eq!(PLUS.0, pf_client_core::lucide::path("plus").unwrap());
        assert_eq!(
            by_name("gamepad-2").unwrap().0,
            pf_client_core::lucide::path("gamepad-2").unwrap()
        );
        assert!(by_name("no-such-icon").is_none());
    }
}
