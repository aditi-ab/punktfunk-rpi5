//! The console's icon set: [Lucide](https://lucide.dev) v0.462.0 (ISC — see
//! THIRD-PARTY-NOTICES.txt), each icon carried as its 24×24 SVG path data, every shape of
//! the source SVG folded into one path string by `scripts/gen-lucide-icons.py`. Drawn by
//! [`draw_icon`] through Skia's own SVG-path parser and stroked at Lucide's native width 2
//! with round caps and joins — the marks render exactly as designed, in the console's
//! paints, at any size. No icon font ships and no new dependency: the set is a page of
//! string constants.

use crate::theme::stroke;
use skia_safe::{utils::parse_path, Canvas, Color4f, PaintCap, PaintJoin};

/// One icon's 24×24 path data.
#[derive(Clone, Copy)]
pub(crate) struct Icon(pub &'static str);

pub(crate) const CHART_COLUMN: Icon = Icon("M3 3v16a2 2 0 0 0 2 2h16M18 17V9M13 17V5M8 17v-3");
pub(crate) const CHEVRON_DOWN: Icon = Icon("M6 9l6 6 6-6");
pub(crate) const CHEVRON_LEFT: Icon = Icon("M15 18l-6-6 6-6");
pub(crate) const CHEVRON_RIGHT: Icon = Icon("M9 18l6-6-6-6");
pub(crate) const CHEVRON_UP: Icon = Icon("M18 15l-6-6-6 6");
pub(crate) const CORNER_DOWN_LEFT: Icon = Icon("M9 10L4 15L9 20M20 4v7a4 4 0 0 1-4 4H4");
pub(crate) const ELLIPSIS: Icon =
    Icon("M11 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0M18 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0M4 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0");
pub(crate) const GAMEPAD_2: Icon = Icon(
    "M6 11L10 11M8 9L8 13M15 12L15.01 12M18 10L18.01 10M17.32 5H6.68a4 4 0 0 0-3.978 3.59c-.006.052-.01.101-.017.152C2.604 9.416 2 14.456 2 16a3 3 0 0 0 3 3c1 0 1.5-.5 2-1l1.414-1.414A2 2 0 0 1 9.828 16h4.344a2 2 0 0 1 1.414.586L17 18c.5.5 1 1 2 1a3 3 0 0 0 3-3c0-1.545-.604-6.584-.685-7.258-.007-.05-.011-.1-.017-.151A4 4 0 0 0 17.32 5z",
);
pub(crate) const KEYBOARD: Icon = Icon(
    "M10 8h.01M12 12h.01M14 8h.01M16 12h.01M18 8h.01M6 8h.01M7 16h10M8 12h.01M4 4h16a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-16a2 2 0 0 1 -2 -2v-12a2 2 0 0 1 2 -2z",
);
pub(crate) const LOG_OUT: Icon =
    Icon("M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17L21 12L16 7M21 12L9 12");
pub(crate) const MIC_OFF: Icon = Icon(
    "M2 2L22 22M18.89 13.23A7.12 7.12 0 0 0 19 12v-2M5 10v2a7 7 0 0 0 12 5M15 9.34V5a3 3 0 0 0-5.68-1.33M9 9v3a3 3 0 0 0 5.12 2.12M12 19L12 22",
);
pub(crate) const MIC: Icon = Icon(
    "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3ZM19 10v2a7 7 0 0 1-14 0v-2M12 19L12 22",
);
pub(crate) const MOON: Icon = Icon("M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z");
pub(crate) const PLUS: Icon = Icon("M5 12h14M12 5v14");
pub(crate) const POINTER: Icon = Icon(
    "M22 14a8 8 0 0 1-8 8M18 11v-1a2 2 0 0 0-2-2a2 2 0 0 0-2 2M14 10V9a2 2 0 0 0-2-2a2 2 0 0 0-2 2v1M10 9.5V4a2 2 0 0 0-2-2a2 2 0 0 0-2 2v10M18 11a2 2 0 1 1 4 0v3a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15",
);
pub(crate) const POWER: Icon = Icon("M12 2v10M18.4 6.6a9 9 0 1 1-12.77.04");
pub(crate) const ROTATE_CW: Icon =
    Icon("M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8M21 3v5h-5");
pub(crate) const SEND: Icon = Icon(
    "M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11zM21.854 2.147l-10.94 10.939",
);
pub(crate) const SQUARE: Icon =
    Icon("M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2z");

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

    const ALL: [(&str, Icon); 19] = [
        ("chart-column", CHART_COLUMN),
        ("chevron-down", CHEVRON_DOWN),
        ("chevron-left", CHEVRON_LEFT),
        ("chevron-right", CHEVRON_RIGHT),
        ("chevron-up", CHEVRON_UP),
        ("corner-down-left", CORNER_DOWN_LEFT),
        ("ellipsis", ELLIPSIS),
        ("gamepad-2", GAMEPAD_2),
        ("keyboard", KEYBOARD),
        ("log-out", LOG_OUT),
        ("mic-off", MIC_OFF),
        ("mic", MIC),
        ("moon", MOON),
        ("plus", PLUS),
        ("pointer", POINTER),
        ("power", POWER),
        ("rotate-cw", ROTATE_CW),
        ("send", SEND),
        ("square", SQUARE),
    ];

    /// Every icon's data parses with Skia's own SVG-path parser and stays inside the
    /// 24-unit box. The TIGHT bounds, not `bounds()`: an arc's conic control points sit
    /// outside the curve they draw, so the loose bounds flag a correct circle. A typo in a
    /// path string would otherwise be a silently missing mark.
    #[test]
    fn every_icon_parses_and_fits_its_box() {
        for (name, icon) in ALL {
            let path =
                parse_path::from_svg(icon.0).unwrap_or_else(|| panic!("{name} does not parse"));
            let b = path.compute_tight_bounds();
            assert!(
                b.left >= -0.5 && b.top >= -0.5 && b.right <= 24.5 && b.bottom <= 24.5,
                "{name} leaves the box: {b:?}"
            );
        }
    }
}
