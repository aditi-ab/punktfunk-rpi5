//! Lucide chrome marks from the shared table [`pf_client_core::lucide`]
//! (`assets/lucide/*.svg`; ISC — THIRD-PARTY-NOTICES.txt).
//!
//! Named constants are this shell's own chrome. Ring slots resolve a Lucide
//! name through [`by_name`] so this list cannot drift from the slot table.
//! GTK strokes the same path strings with `gsk::Path`.

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

pub(crate) fn by_name(name: &str) -> Option<Icon> {
    pf_client_core::lucide::path(name).map(Icon)
}

/// Centre `(x, y)`, 24-unit box scaled to `box_px`. Stroke 2 is Lucide's
/// native weight, so it scales with the box.
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

    /// Tight bounds, not `bounds()`: an arc's conic control points sit outside
    /// the curve they draw, so the loose box flags a correct circle. Walk the
    /// whole shared table — GTK strokes the same strings and has no parser.
    #[test]
    fn every_icon_parses_and_fits_its_box() {
        for (name, data, _glyph) in pf_client_core::lucide::ALL {
            let path =
                parse_path::from_svg(data).unwrap_or_else(|| panic!("{name} does not parse"));
            let b = path.compute_tight_bounds();
            assert!(
                b.left >= -0.5 && b.top >= -0.5 && b.right <= 24.5 && b.bottom <= 24.5,
                "{name} leaves the box: {b:?}"
            );
        }
    }

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
