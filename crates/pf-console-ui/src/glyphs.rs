//! Controller button glyphs and the hint bar — the "controls legend" pill every console
//! screen pins bottom-leading (the Apple client resolves real SF glyphs per pad via
//! `sfSymbolsName`; here the shapes are drawn). The style follows the ACTIVE pad:
//! PlayStation controllers read ✕/○/□/△, everything else reads ABXY letters, and with
//! no pad at all the legend swaps to keyboard keycaps — the console stays fully
//! drivable either way.

use crate::theme::{fg, fill, stroke, Fonts, W};
use punktfunk_core::config::GamepadPref;
use skia_safe::{Canvas, PathBuilder, Point, RRect, Rect};

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

/// A compact mark for WHAT is driving the console, drawn from `(x, cy)` across `w`: a
/// controller silhouette, or a keycap when there is no pad and the keyboard is doing the
/// work. Says at a glance which glyph set the legend below is speaking in.
pub(crate) fn pad_mark(
    canvas: &Canvas,
    style: GlyphStyle,
    x: f64,
    cy: f64,
    w: f64,
    k: f64,
    ink: skia_safe::Color4f,
) {
    let mut p = fill(ink);
    if style == GlyphStyle::Keyboard {
        // A keycap: the same shape the hint bar draws for a key, at chip size.
        let h = w * 0.72;
        let r = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
        p.set_style(skia_safe::PaintStyle::Stroke);
        p.set_stroke_width((1.3 * k) as f32);
        canvas.draw_rrect(
            RRect::new_rect_xy(r, (3.0 * k) as f32, (3.0 * k) as f32),
            &p,
        );
        return;
    }
    // A gamepad: a wide rounded body with a grip under each end. Detail beyond the
    // silhouette is invisible at 15 dp, so there is none — the outline IS the glyph.
    let h = w * 0.52;
    let body = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(body, (h / 2.2) as f32, (h / 2.2) as f32),
        &p,
    );
    let grip = (w * 0.17) as f32;
    canvas.draw_circle(((x + w * 0.2) as f32, (cy + h * 0.36) as f32), grip, &p);
    canvas.draw_circle(((x + w * 0.8) as f32, (cy + h * 0.36) as f32), grip, &p);
}

/// A four-segment charge pip, drawn from `(x, cy)` across `w`. Filled segments are the
/// charge; the outline is always the full cell, so "one bar" and "four bars" occupy the
/// same width and the chip never reflows as the pad drains.
///
/// Three states, in priority order. CHARGING takes the palette's accent and outranks the
/// low warning, because a pad at 4 % on the cable is not the problem a pad at 4 % off it
/// is. Otherwise under 20 % goes red — a fixed red, not the accent, for the same reason the
/// error toast uses one: on a `moss` or `mint` field the accent is a colour that means
/// "fine". Everything else is plain foreground.
pub(crate) fn battery_pip(
    canvas: &Canvas,
    x: f64,
    cy: f64,
    w: f64,
    k: f64,
    b: pf_client_core::gamepad::PadBattery,
) {
    let h = w * 0.5;
    let cell = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, (w * 0.86) as f32, h as f32);
    let ink = if b.charging {
        crate::theme::accent(1.0)
    } else if b.percent < 20 {
        skia_safe::Color4f::new(0.93, 0.31, 0.28, 1.0)
    } else {
        crate::theme::fg(0.7)
    };
    let outline = stroke(ink, (1.2 * k) as f32);
    let r = (2.0 * k) as f32;
    canvas.draw_rrect(RRect::new_rect_xy(cell, r, r), &outline);
    // The terminal nub, so the cell reads as a battery and not as a text field.
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(
                cell.right + (1.5 * k) as f32,
                (cy - h * 0.22) as f32,
                (1.8 * k) as f32,
                (h * 0.44) as f32,
            ),
            r,
            r,
        ),
        &fill(ink),
    );
    // Four segments, rounded UP so a pad with any charge left always shows at least one —
    // an empty-looking cell on a pad that still works reads as broken.
    let filled = ((f32::from(b.percent) / 100.0) * 4.0)
        .ceil()
        .clamp(0.0, 4.0) as i32;
    let pad = (1.6 * k) as f32;
    let seg_w = (cell.width() - 2.0 * pad) / 4.0;
    for i in 0..filled {
        let sx = cell.left + pad + i as f32 * seg_w;
        canvas.draw_rect(
            Rect::from_xywh(
                sx + 0.4 * k as f32,
                cell.top + pad,
                seg_w - 0.8 * k as f32,
                cell.height() - 2.0 * pad,
            ),
            &fill(ink),
        );
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
    /// ▲ — up raises the focused item's context menu, on a screen with up to spare. Where
    /// there isn't (the library grid spends up on rows) the same menu hangs off
    /// [`HintKey::Tertiary`] instead; the button differs, the word "Options" does not.
    Up,
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

/// What a drawn hint bar left behind.
pub(crate) struct HintBar {
    /// The pill's `(width, height)`.
    pub size: (f64, f64),
    /// One hit box per hint, in the order they were given. The legend is also the console's
    /// only on-screen list of what the face buttons do, so for a pointer — which has no
    /// face buttons — it doubles as the button bar itself.
    pub rects: Vec<(HintKey, Rect)>,
}

/// The hint bar pill, anchored at its BOTTOM-LEFT corner.
pub(crate) fn hint_bar(
    canvas: &Canvas,
    fonts: &Fonts,
    hints: &[Hint],
    style: GlyphStyle,
    x: f64,
    bottom: f64,
    k: f64,
) -> HintBar {
    if hints.is_empty() {
        return HintBar {
            size: (0.0, 0.0),
            rects: Vec::new(),
        };
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
    // A scrim under the glass, then the SHARED glass recipe — the legend used to mix its
    // own (a flat `fg(0.06)` wash and a hand-rolled stroke), which meant it was the one
    // floating surface in the console that didn't pick up the palette's glass. The scrim
    // stays because this pill sits over the aurora at full contrast, where glass alone has
    // little to separate it from the field. Same construction as the toast.
    let corner = (h / 2.0 / k) as f32;
    canvas.draw_rrect(
        RRect::new_rect_xy(rect, (h / 2.0) as f32, (h / 2.0) as f32),
        &fill(crate::theme::shade(0.30)),
    );
    crate::theme::panel(
        canvas,
        rect,
        corner,
        None,
        crate::theme::PanelStroke::Plain(0.12),
        k as f32,
    );
    // One lit edge per frame for the whole legend — the cost discipline the highlight is
    // rationed by counts ROWS, and this is chrome.
    crate::theme::panel_highlight(canvas, rect, corner, k as f32);

    let cy = bottom - h / 2.0;
    let mut pen = x + pad;
    let mut rects = Vec::with_capacity(hints.len());
    for (hint, (gw, lw)) in hints.iter().zip(&widths) {
        // Glyph + label + half the gap to the next hint, full pill height: a comfortable
        // target without stealing the neighbour's.
        rects.push((
            hint.key,
            Rect::from_xywh(
                (pen - gap_glyph / 2.0) as f32,
                (bottom - h) as f32,
                (gw + gap_glyph + lw + gap_hint / 2.0) as f32,
                h as f32,
            ),
        ));
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
    HintBar {
        size: (w, h),
        rects,
    }
}

fn glyph_width(fonts: &Fonts, key: HintKey, style: GlyphStyle, k: f64) -> f64 {
    match resolved(key, style) {
        Resolved::Badge(_) | Resolved::Adjust => BADGE_D * k,
        Resolved::Shoulders => 2.0 * shoulder_w(fonts, k) + 3.0 * k,
        Resolved::Up => BADGE_D * k,
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
    /// The d-pad's up — drawn the same in every style, because it is a direction rather
    /// than a button whose label changes with the pad.
    Up,
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
            // Tab is the key a keyboard reaches for to change section; PgUp/PgDn still
            // work, but naming both here makes the legend wider than the hint is worth.
            HintKey::Shoulders => Resolved::Key("Tab"),
            HintKey::Adjust => Resolved::Adjust,
            HintKey::Up => Resolved::Up,
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
        HintKey::Up => Resolved::Up,
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
            canvas.draw_circle(center, r as f32, &fill(fg(0.10)));
            canvas.draw_circle(center, r as f32, &stroke(fg(0.32), (1.2 * k) as f32));
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
                    &fill(fg(0.10)),
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
        Resolved::Up => {
            // ▲ — one solid triangle in a badge-sized slot.
            let r = BADGE_D * k / 2.0;
            let (cx, cyf) = ((x + r) as f32, cy as f32);
            let (tw, th) = ((5.5 * k) as f32, (4.5 * k) as f32);
            let mut up = PathBuilder::new();
            up.move_to((cx, cyf - th));
            up.line_to((cx - tw, cyf + th));
            up.line_to((cx + tw, cyf + th));
            up.close();
            canvas.draw_path(&up.detach(), &fill(fg(0.85)));
        }
        Resolved::Adjust => {
            // ◀ ▶ — two small solid triangles.
            let r = BADGE_D * k / 2.0;
            let (cx, cyf) = ((x + r) as f32, cy as f32);
            let (tw, th) = ((4.5 * k) as f32, (5.5 * k) as f32);
            let gap = (2.6 * k) as f32;
            let paint = fill(fg(0.85));
            let mut left = PathBuilder::new();
            left.move_to((cx - gap, cyf - th));
            left.line_to((cx - gap - tw, cyf));
            left.line_to((cx - gap, cyf + th));
            left.close();
            canvas.draw_path(&left.detach(), &paint);
            let mut right = PathBuilder::new();
            right.move_to((cx + gap, cyf - th));
            right.line_to((cx + gap + tw, cyf));
            right.line_to((cx + gap, cyf + th));
            right.close();
            canvas.draw_path(&right.detach(), &paint);
        }
        Resolved::Key(text) => {
            let w = keycap_w(fonts, text, k);
            let h = 18.0 * k;
            let rect = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &fill(fg(0.10)),
            );
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &stroke(fg(0.28), 1.0),
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
fn draw_ps_shape(canvas: &Canvas, face: Face, center: Point, r: f32, width: f32) {
    let mut p = stroke(fg(0.92), width);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
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
            let mut tri = PathBuilder::new();
            tri.move_to((cx, cy - r * 1.2));
            tri.line_to((cx + r * 1.15, cy + r * 0.85));
            tri.line_to((cx - r * 1.15, cy + r * 0.85));
            tri.close();
            canvas.draw_path(&tri.detach(), &p);
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
