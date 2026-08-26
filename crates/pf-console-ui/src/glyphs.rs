//! Controller button glyphs and the hint bar — the "controls legend" pill every console
//! screen pins bottom-leading (the Apple client resolves real SF glyphs per pad via
//! `sfSymbolsName`; here the shapes are drawn). The style follows WHAT IS DRIVING the
//! console (see `Shell::glyph_style`): PlayStation controllers read ✕/○/□/△, Nintendo
//! pads read their own letter positions, everything else reads ABXY letters — and when
//! the last input came from keys, the legend swaps to keyboard keycaps on the desktop or
//! to TV-remote marks (OK, the back arrow, the D-pad) on Android, where key-driven input
//! IS a remote. The console stays fully drivable in every one of them.

use crate::theme::{fg, fill, stroke, Fonts, W};
use punktfunk_core::config::GamepadPref;
use skia_safe::{Canvas, PathBuilder, Point, RRect, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GlyphStyle {
    /// ABXY letter badges (Xbox / Steam Deck / generic).
    Letters,
    /// PlayStation face shapes (DualSense / DualShock 4).
    Shapes,
    /// Nintendo letter badges: the same positional buttons, labelled the way the pad in
    /// the user's hands is — south reads B, east A, west Y, north X. Without this a
    /// Switch pad's legend says "A Select" over the button engraved B.
    Nintendo,
    /// Keys drive, on a desktop — keyboard keycaps.
    Keyboard,
    /// Keys drive, on Android — a TV remote: OK, the back arrow, and the D-pad. A remote
    /// has no Y/X and no shoulders, so hints that need them resolve to nothing and the
    /// section hint points at the D-pad path instead.
    Remote,
}

impl GlyphStyle {
    /// The style a PAD speaks in, from the family its `Auto` virtual pad resolves to
    /// ([`PadInfo::pref`](pf_client_core::menu_nav::PadInfo) — DualSense stays DualSense,
    /// Switch Pro stays Switch Pro, everything else lands on an Xbox class). The keys-drive
    /// styles are picked by the shell, which knows the platform; `None` (no pad) falls to
    /// keycaps as the neutral default.
    pub(crate) fn from_pref(pref: Option<GamepadPref>) -> GlyphStyle {
        match pref {
            Some(GamepadPref::DualSense | GamepadPref::DualSenseEdge | GamepadPref::DualShock4) => {
                GlyphStyle::Shapes
            }
            Some(GamepadPref::SwitchPro) => GlyphStyle::Nintendo,
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
    if style == GlyphStyle::Remote {
        // A remote: a slim upright wand with its select ring near the top. Outlined like
        // the keycap — the filled marks are for things with a body to fill.
        let rw = w * 0.42;
        let rh = w * 0.98;
        let body = Rect::from_xywh(
            (x + (w - rw) / 2.0) as f32,
            (cy - rh / 2.0) as f32,
            rw as f32,
            rh as f32,
        );
        p.set_style(skia_safe::PaintStyle::Stroke);
        p.set_stroke_width((1.3 * k) as f32);
        canvas.draw_rrect(
            RRect::new_rect_xy(body, (rw / 2.2) as f32, (rw / 2.2) as f32),
            &p,
        );
        canvas.draw_circle(
            ((x + w / 2.0) as f32, (cy - rh * 0.22) as f32),
            (rw * 0.30) as f32,
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
    b: pf_client_core::menu_nav::PadBattery,
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
    /// ▼ — the home carousel's other spare direction, which opens Settings. Advertised in
    /// place of [`HintKey::Tertiary`] where no pad is attached, because that is exactly the
    /// device that has no X to press: a TV remote is a D-pad, OK and Back.
    Down,
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
    // Hints with no honest glyph in this style (a remote's missing Y/X) are dropped
    // here, before layout — they take no width, draw nothing and get no hit box.
    let shown: Vec<&Hint> = hints
        .iter()
        .filter(|h| resolved(h.key, style).is_some())
        .collect();
    if shown.is_empty() {
        return HintBar {
            size: (0.0, 0.0),
            rects: Vec::new(),
        };
    }
    let widths: Vec<(f64, f64)> = shown
        .iter()
        .map(|h| {
            (
                glyph_width(fonts, h.key, style, k).expect("filtered to resolvable"),
                fonts.measure(&h.label, W::SemiBold, LABEL_SIZE * k) as f64,
            )
        })
        .collect();
    let content_w: f64 = widths.iter().map(|(g, l)| g + gap_glyph + l).sum::<f64>()
        + gap_hint * (shown.len() - 1) as f64;
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
    let mut rects = Vec::with_capacity(shown.len());
    for (hint, (gw, lw)) in shown.iter().zip(&widths) {
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

/// `None` = the hint resolves to nothing in this style and takes no space (see [`resolved`]).
fn glyph_width(fonts: &Fonts, key: HintKey, style: GlyphStyle, k: f64) -> Option<f64> {
    Some(match resolved(key, style)? {
        Resolved::Badge(_) | Resolved::Adjust => BADGE_D * k,
        Resolved::Shoulders => 2.0 * shoulder_w(fonts, k) + 3.0 * k,
        Resolved::Up | Resolved::Down | Resolved::Ok | Resolved::BackArrow => BADGE_D * k,
        Resolved::Key(text) => keycap_w(fonts, text, k),
    })
}

fn shoulder_w(fonts: &Fonts, k: f64) -> f64 {
    fonts.measure("L1", W::SemiBold, 10.0 * k) as f64 + 10.0 * k
}

fn keycap_w(fonts: &Fonts, text: &str, k: f64) -> f64 {
    fonts.measure(text, W::SemiBold, 11.0 * k) as f64 + 14.0 * k
}

/// A hint key resolved against the glyph style.
enum Resolved {
    /// A face-button badge: the letter (Letters/Nintendo) or shape (Shapes).
    Badge(Face),
    Shoulders,
    Adjust,
    /// The d-pad's up — drawn the same in every style, because it is a direction rather
    /// than a button whose label changes with the pad.
    Up,
    /// The d-pad's down — the same triangle stood on its head, and style-free for the
    /// same reason [`Resolved::Up`] is.
    Down,
    /// A TV remote's select — a round badge that simply says OK.
    Ok,
    /// A TV remote's back — the ↩ return arrow in a badge.
    BackArrow,
    Key(&'static str),
}

#[derive(Clone, Copy)]
enum Face {
    A,
    B,
    X,
    Y,
}

/// `None` = this hint has no honest glyph in this style and is not drawn at all: a TV
/// remote has no Y/X, and advertising a button the device cannot press is worse than
/// silence. (The touch path loses those two bar buttons in Remote style with it —
/// acceptable: Remote only rules while KEYS drove last, and every such action still has
/// an on-screen path.)
fn resolved(key: HintKey, style: GlyphStyle) -> Option<Resolved> {
    if style == GlyphStyle::Keyboard {
        return Some(match key {
            HintKey::Confirm => Resolved::Key("Enter"),
            HintKey::Back => Resolved::Key("Esc"),
            HintKey::Secondary => Resolved::Key("Y"),
            HintKey::Tertiary => Resolved::Key("X"),
            // Tab is the key a keyboard reaches for to change section; PgUp/PgDn still
            // work, but naming both here makes the legend wider than the hint is worth.
            HintKey::Shoulders => Resolved::Key("Tab"),
            HintKey::Adjust => Resolved::Adjust,
            HintKey::Up => Resolved::Up,
            HintKey::Down => Resolved::Down,
            HintKey::Key(t) => Resolved::Key(t),
        });
    }
    if style == GlyphStyle::Remote {
        return match key {
            HintKey::Confirm => Some(Resolved::Ok),
            HintKey::Back => Some(Resolved::BackArrow),
            // A remote has no Y and no X. The screens' Y/X features stay reachable the
            // ways their screens already provide; the legend just stops naming buttons
            // that are not in the user's hand.
            HintKey::Secondary | HintKey::Tertiary => None,
            // No shoulders either — the D-pad path to the strip (Up from the top row) is
            // the section switcher a remote actually has, so the hint points up.
            HintKey::Shoulders => Some(Resolved::Up),
            HintKey::Adjust => Some(Resolved::Adjust),
            HintKey::Up => Some(Resolved::Up),
            HintKey::Down => Some(Resolved::Down),
            HintKey::Key(t) => Some(Resolved::Key(t)),
        };
    }
    Some(match key {
        HintKey::Confirm => Resolved::Badge(Face::A),
        HintKey::Back => Resolved::Badge(Face::B),
        HintKey::Tertiary => Resolved::Badge(Face::X),
        HintKey::Secondary => Resolved::Badge(Face::Y),
        HintKey::Shoulders => Resolved::Shoulders,
        HintKey::Adjust => Resolved::Adjust,
        HintKey::Down => Resolved::Down,
        HintKey::Up => Resolved::Up,
        HintKey::Key(t) => Resolved::Key(t),
    })
}

/// The letter a face badge shows: positional buttons, labelled the way the ACTIVE pad
/// is engraved. Nintendo swaps both pairs — its south is B and its east is A.
fn face_letter(face: Face, style: GlyphStyle) -> &'static str {
    match (style, face) {
        (GlyphStyle::Nintendo, Face::A) => "B",
        (GlyphStyle::Nintendo, Face::B) => "A",
        (GlyphStyle::Nintendo, Face::X) => "Y",
        (GlyphStyle::Nintendo, Face::Y) => "X",
        (_, Face::A) => "A",
        (_, Face::B) => "B",
        (_, Face::X) => "X",
        (_, Face::Y) => "Y",
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
    let Some(resolved) = resolved(key, style) else {
        return;
    };
    match resolved {
        Resolved::Badge(face) => {
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(fg(0.10)));
            canvas.draw_circle(center, r as f32, &stroke(fg(0.32), (1.2 * k) as f32));
            if style == GlyphStyle::Shapes {
                draw_ps_shape(canvas, face, center, (4.6 * k) as f32, (1.7 * k) as f32);
            } else {
                let letter = face_letter(face, style);
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
        Resolved::Ok => {
            // The remote's select: the same badge as a face button, saying OK — the word
            // printed on the remote itself.
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(fg(0.10)));
            canvas.draw_circle(center, r as f32, &stroke(fg(0.32), (1.2 * k) as f32));
            let size = 9.5 * k;
            let w = fonts.measure("OK", W::SemiBold, size) as f64;
            fonts.draw(
                canvas,
                "OK",
                x + r - w / 2.0,
                cy + size * 0.36,
                W::SemiBold,
                size,
                fg(0.92),
            );
        }
        Resolved::BackArrow => {
            // The remote's back: the ↩ return arrow in the same badge — a shaft curving
            // home with an arrowhead at its left end.
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(fg(0.10)));
            canvas.draw_circle(center, r as f32, &stroke(fg(0.32), (1.2 * k) as f32));
            let (cx, cyf) = (center.x, center.y);
            let (half_w, rise) = ((4.6 * k) as f32, (3.2 * k) as f32);
            let mut p = stroke(fg(0.92), (1.7 * k) as f32);
            p.set_stroke_cap(skia_safe::PaintCap::Round);
            p.set_stroke_join(skia_safe::PaintJoin::Round);
            let mut path = PathBuilder::new();
            path.move_to((cx + half_w, cyf - rise)); // the hook, up on the right…
            path.line_to((cx + half_w, cyf + rise * 0.2)); // …dropping to the shaft…
            path.line_to((cx - half_w, cyf + rise * 0.2)); // …running left toward the head.
            canvas.draw_path(&path.detach(), &p);
            let head = (2.6 * k) as f32;
            let tip = cx - half_w;
            let mut arrow = PathBuilder::new();
            arrow.move_to((tip + head, cyf + rise * 0.2 - head));
            arrow.line_to((tip, cyf + rise * 0.2));
            arrow.line_to((tip + head, cyf + rise * 0.2 + head));
            canvas.draw_path(&arrow.detach(), &p);
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
        g @ (Resolved::Up | Resolved::Down) => {
            // ▲ / ▼ — one solid triangle in a badge-sized slot, the same triangle either
            // way up: apex toward the direction it names, base at the other end.
            let r = BADGE_D * k / 2.0;
            let (cx, cyf) = ((x + r) as f32, cy as f32);
            let (tw, th) = ((5.5 * k) as f32, (4.5 * k) as f32);
            let (apex, base) = if matches!(g, Resolved::Down) {
                (cyf + th, cyf - th)
            } else {
                (cyf - th, cyf + th)
            };
            let mut tri = PathBuilder::new();
            tri.move_to((cx, apex));
            tri.line_to((cx - tw, base));
            tri.line_to((cx + tw, base));
            tri.close();
            canvas.draw_path(&tri.detach(), &fill(fg(0.85)));
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
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SwitchPro)),
            GlyphStyle::Nintendo
        );
        assert_eq!(GlyphStyle::from_pref(None), GlyphStyle::Keyboard);
    }

    /// Nintendo's badges carry the pad's OWN engravings: the positional confirm (south) is
    /// the button a Switch pad labels B. Everything non-Nintendo keeps the Xbox letters.
    #[test]
    fn nintendo_badges_read_the_pads_own_letters() {
        assert_eq!(face_letter(Face::A, GlyphStyle::Nintendo), "B");
        assert_eq!(face_letter(Face::B, GlyphStyle::Nintendo), "A");
        assert_eq!(face_letter(Face::X, GlyphStyle::Nintendo), "Y");
        assert_eq!(face_letter(Face::Y, GlyphStyle::Nintendo), "X");
        assert_eq!(face_letter(Face::A, GlyphStyle::Letters), "A");
    }

    /// A remote has no Y/X, so those hints resolve to nothing — the legend must not
    /// advertise a button the device in the user's hand cannot press. Confirm and Back
    /// resolve to the remote's own marks, and the section hint points at the D-pad path.
    #[test]
    fn remote_hides_the_buttons_a_remote_does_not_have() {
        assert!(resolved(HintKey::Secondary, GlyphStyle::Remote).is_none());
        assert!(resolved(HintKey::Tertiary, GlyphStyle::Remote).is_none());
        assert!(matches!(
            resolved(HintKey::Confirm, GlyphStyle::Remote),
            Some(Resolved::Ok)
        ));
        assert!(matches!(
            resolved(HintKey::Back, GlyphStyle::Remote),
            Some(Resolved::BackArrow)
        ));
        assert!(matches!(
            resolved(HintKey::Shoulders, GlyphStyle::Remote),
            Some(Resolved::Up)
        ));
        // Every other style resolves every hint — nothing else went silent.
        for style in [
            GlyphStyle::Letters,
            GlyphStyle::Shapes,
            GlyphStyle::Nintendo,
            GlyphStyle::Keyboard,
        ] {
            for key in [
                HintKey::Confirm,
                HintKey::Back,
                HintKey::Secondary,
                HintKey::Tertiary,
                HintKey::Shoulders,
                HintKey::Adjust,
                HintKey::Up,
                HintKey::Down,
            ] {
                assert!(resolved(key, style).is_some(), "{style:?} lost a hint");
            }
        }
    }
}
