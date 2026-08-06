//! Controller button glyphs and the hint bar — the "controls legend" pill every console
//! screen pins bottom-leading (the Apple client resolves real SF glyphs per pad via
//! `sfSymbolsName`; here the shapes are drawn). The style follows the ACTIVE pad:
//! PlayStation controllers read ✕/○/□/△, everything else reads ABXY letters, and with
//! no pad at all the legend swaps to keyboard keycaps — the console stays fully
//! drivable either way.

use crate::theme::{fg, Fonts, W};
use punktfunk_core::config::GamepadPref;
use skia_safe::{Canvas, Paint, Path, Point, RRect, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GlyphStyle {
    /// ABXY letter badges (Xbox / Steam Deck / generic).
    Letters,
    /// PlayStation face shapes (DualSense / DualShock 4).
    Shapes,
    /// No controller — keyboard keycaps.
    Keyboard,
}

impl GlyphStyle {
    pub(crate) fn from_pref(pref: Option<GamepadPref>) -> GlyphStyle {
        match pref {
            Some(GamepadPref::DualSense | GamepadPref::DualSenseEdge | GamepadPref::DualShock4) => {
                GlyphStyle::Shapes
            }
            Some(_) => GlyphStyle::Letters,
            None => GlyphStyle::Keyboard,
        }
    }
}

/// What a hint's glyph depicts. `Key` renders a literal keycap chip in any style (used
/// for keyboard fallbacks and the Deck's "Steam + X" keyboard chord).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintKey {
    Confirm,
    Back,
    Secondary,
    Tertiary,
    Shoulders,
    /// ◀ ▶ — left/right adjusts the focused value.
    Adjust,
    Key(&'static str),
}

pub(crate) struct Hint {
    pub key: HintKey,
    pub label: String,
}

impl Hint {
    pub(crate) fn new(key: HintKey, label: impl Into<String>) -> Hint {
        Hint {
            key,
            label: label.into(),
        }
    }
}

const LABEL_SIZE: f64 = 14.0;
const BADGE_D: f64 = 22.0; // face-button badge diameter

/// The hint bar pill, anchored at its BOTTOM-LEFT corner. Returns the pill's size.
pub(crate) fn hint_bar(
    canvas: &Canvas,
    fonts: &Fonts,
    hints: &[Hint],
    style: GlyphStyle,
    x: f64,
    bottom: f64,
    k: f64,
) -> (f64, f64) {
    if hints.is_empty() {
        return (0.0, 0.0);
    }
    let pad = 13.0 * k;
    let gap_hint = 18.0 * k;
    let gap_glyph = 7.0 * k;
    let widths: Vec<(f64, f64)> = hints
        .iter()
        .map(|h| {
            (
                glyph_width(fonts, h.key, style, k),
                fonts.measure(&h.label, W::SemiBold, LABEL_SIZE * k) as f64,
            )
        })
        .collect();
    let content_w: f64 = widths.iter().map(|(g, l)| g + gap_glyph + l).sum::<f64>()
        + gap_hint * (hints.len() - 1) as f64;
    let h = BADGE_D * k + 2.0 * pad;
    let w = content_w + 2.0 * pad;
    let rect = Rect::from_xywh((x) as f32, (bottom - h) as f32, w as f32, h as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(rect, (h / 2.0) as f32, (h / 2.0) as f32),
        &Paint::new(crate::theme::shade(0.30), None),
    );
    canvas.draw_rrect(
        RRect::new_rect_xy(rect, (h / 2.0) as f32, (h / 2.0) as f32),
        &Paint::new(fg(0.06), None),
    );
    let mut sp = Paint::new(fg(0.12), None);
    sp.set_style(skia_safe::PaintStyle::Stroke);
    sp.set_stroke_width(1.0);
    sp.set_anti_alias(true);
    canvas.draw_rrect(
        RRect::new_rect_xy(rect, (h / 2.0) as f32, (h / 2.0) as f32),
        &sp,
    );

    let cy = bottom - h / 2.0;
    let mut pen = x + pad;
    for (hint, (gw, lw)) in hints.iter().zip(&widths) {
        draw_glyph(canvas, fonts, hint.key, style, pen, cy, k);
        pen += gw + gap_glyph;
        // Baseline centered on the badge (cap height ≈ 0.72 em for Geist).
        fonts.draw(
            canvas,
            &hint.label,
            pen,
            cy + LABEL_SIZE * k * 0.36,
            W::SemiBold,
            LABEL_SIZE * k,
            fg(0.85),
        );
        pen += lw + gap_hint;
    }
    (w, h)
}

fn glyph_width(fonts: &Fonts, key: HintKey, style: GlyphStyle, k: f64) -> f64 {
    match resolved(key, style) {
        Resolved::Badge(_) | Resolved::Adjust => BADGE_D * k,
        Resolved::Shoulders => 2.0 * shoulder_w(fonts, k) + 3.0 * k,
        Resolved::Key(text) => keycap_w(fonts, text, k),
    }
}

fn shoulder_w(fonts: &Fonts, k: f64) -> f64 {
    fonts.measure("L1", W::SemiBold, 10.0 * k) as f64 + 10.0 * k
}

fn keycap_w(fonts: &Fonts, text: &str, k: f64) -> f64 {
    fonts.measure(text, W::SemiBold, 11.0 * k) as f64 + 14.0 * k
}

/// A hint key resolved against the glyph style.
enum Resolved {
    /// A face-button badge: the letter (Letters) or shape index (Shapes).
    Badge(Face),
    Shoulders,
    Adjust,
    Key(&'static str),
}

#[derive(Clone, Copy)]
enum Face {
    A,
    B,
    X,
    Y,
}

fn resolved(key: HintKey, style: GlyphStyle) -> Resolved {
    if style == GlyphStyle::Keyboard {
        return match key {
            HintKey::Confirm => Resolved::Key("Enter"),
            HintKey::Back => Resolved::Key("Esc"),
            HintKey::Secondary => Resolved::Key("Y"),
            HintKey::Tertiary => Resolved::Key("X"),
            HintKey::Shoulders => Resolved::Key("PgUp/PgDn"),
            HintKey::Adjust => Resolved::Adjust,
            HintKey::Key(t) => Resolved::Key(t),
        };
    }
    match key {
        HintKey::Confirm => Resolved::Badge(Face::A),
        HintKey::Back => Resolved::Badge(Face::B),
        HintKey::Tertiary => Resolved::Badge(Face::X),
        HintKey::Secondary => Resolved::Badge(Face::Y),
        HintKey::Shoulders => Resolved::Shoulders,
        HintKey::Adjust => Resolved::Adjust,
        HintKey::Key(t) => Resolved::Key(t),
    }
}

/// Draw one glyph with its LEFT edge at `x`, vertically centered on `cy`.
fn draw_glyph(
    canvas: &Canvas,
    fonts: &Fonts,
    key: HintKey,
    style: GlyphStyle,
    x: f64,
    cy: f64,
    k: f64,
) {
    match resolved(key, style) {
        Resolved::Badge(face) => {
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &Paint::new(fg(0.10), None));
            let mut ring = Paint::new(fg(0.32), None);
            ring.set_style(skia_safe::PaintStyle::Stroke);
            ring.set_stroke_width((1.2 * k) as f32);
            ring.set_anti_alias(true);
            canvas.draw_circle(center, r as f32, &ring);
            if style == GlyphStyle::Shapes {
                draw_ps_shape(canvas, face, center, (4.6 * k) as f32, (1.7 * k) as f32);
            } else {
                let letter = match face {
                    Face::A => "A",
                    Face::B => "B",
                    Face::X => "X",
                    Face::Y => "Y",
                };
                let size = 12.0 * k;
                let w = fonts.measure(letter, W::SemiBold, size) as f64;
                fonts.draw(
                    canvas,
                    letter,
                    x + r - w / 2.0,
                    cy + size * 0.36,
                    W::SemiBold,
                    size,
                    fg(0.92),
                );
            }
        }
        Resolved::Shoulders => {
            let mut pen = x;
            for label in ["L1", "R1"] {
                let w = shoulder_w(fonts, k);
                let h = 15.0 * k;
                let rect = Rect::from_xywh(pen as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
                canvas.draw_rrect(
                    RRect::new_rect_xy(rect, (4.0 * k) as f32, (4.0 * k) as f32),
                    &Paint::new(fg(0.10), None),
                );
                let size = 10.0 * k;
                let tw = fonts.measure(label, W::SemiBold, size) as f64;
                fonts.draw(
                    canvas,
                    label,
                    pen + (w - tw) / 2.0,
                    cy + size * 0.36,
                    W::SemiBold,
                    size,
                    fg(0.92),
                );
                pen += w + 3.0 * k;
            }
        }
        Resolved::Adjust => {
            // ◀ ▶ — two small solid triangles.
            let r = BADGE_D * k / 2.0;
            let (cx, cyf) = ((x + r) as f32, cy as f32);
            let (tw, th) = ((4.5 * k) as f32, (5.5 * k) as f32);
            let gap = (2.6 * k) as f32;
            let paint = Paint::new(fg(0.85), None);
            let mut left = Path::new();
            left.move_to((cx - gap, cyf - th));
            left.line_to((cx - gap - tw, cyf));
            left.line_to((cx - gap, cyf + th));
            left.close();
            canvas.draw_path(&left, &paint);
            let mut right = Path::new();
            right.move_to((cx + gap, cyf - th));
            right.line_to((cx + gap + tw, cyf));
            right.line_to((cx + gap, cyf + th));
            right.close();
            canvas.draw_path(&right, &paint);
        }
        Resolved::Key(text) => {
            let w = keycap_w(fonts, text, k);
            let h = 18.0 * k;
            let rect = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &Paint::new(fg(0.10), None),
            );
            let mut ring = Paint::new(fg(0.28), None);
            ring.set_style(skia_safe::PaintStyle::Stroke);
            ring.set_stroke_width(1.0);
            ring.set_anti_alias(true);
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &ring,
            );
            let size = 11.0 * k;
            let tw = fonts.measure(text, W::SemiBold, size) as f64;
            fonts.draw(
                canvas,
                text,
                x + (w - tw) / 2.0,
                cy + size * 0.36,
                W::SemiBold,
                size,
                fg(0.92),
            );
        }
    }
}

/// The PlayStation face shapes, stroked inside the badge: Confirm=✕, Back=○, X-position
/// =□, Y-position=△ (the DualSense's physical layout).
fn draw_ps_shape(canvas: &Canvas, face: Face, center: Point, r: f32, stroke: f32) {
    let mut p = Paint::new(fg(0.92), None);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(stroke);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_anti_alias(true);
    let (cx, cy) = (center.x, center.y);
    match face {
        Face::A => {
            // ✕
            canvas.draw_line((cx - r, cy - r), (cx + r, cy + r), &p);
            canvas.draw_line((cx - r, cy + r), (cx + r, cy - r), &p);
        }
        Face::B => {
            // ○
            canvas.draw_circle(center, r * 1.1, &p);
        }
        Face::X => {
            // □
            canvas.draw_rect(Rect::from_xywh(cx - r, cy - r, 2.0 * r, 2.0 * r), &p);
        }
        Face::Y => {
            // △
            let mut tri = Path::new();
            tri.move_to((cx, cy - r * 1.2));
            tri.line_to((cx + r * 1.15, cy + r * 0.85));
            tri.line_to((cx - r * 1.15, cy + r * 0.85));
            tri.close();
            canvas.draw_path(&tri, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_follows_pad_kind() {
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::DualSense)),
            GlyphStyle::Shapes
        );
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SteamDeck)),
            GlyphStyle::Letters
        );
        assert_eq!(GlyphStyle::from_pref(None), GlyphStyle::Keyboard);
    }
}
