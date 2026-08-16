//! The console shell's shared look: the brand palette, the embedded Geist typography
//! (the same OFL faces the Apple client bundles — one brand voice on every platform),
//! the dark-glass panel primitive standing in for Liquid Glass (translucent fill +
//! hairline stroke; the backdrops here are soft gradients, so a real backdrop blur
//! would be indistinguishable and costs Deck GPU), and the quiet form backdrop the
//! settings/add-host/pair screens sit on.

use anyhow::{anyhow, Result};
use skia_safe::textlayout::{
    FontCollection, ParagraphBuilder, ParagraphStyle, TextAlign, TextStyle, TypefaceFontProvider,
};
use skia_safe::{
    gradient, Canvas, Color4f, Font, FontMgr, FontStyle, MaskFilter, Paint, PathEffect, Point,
    RRect, Rect, TileMode, Typeface,
};

// --- Ink ----------------------------------------------------------------------------------

/// The error/status red (the GTK client's #ff938a). Fixed: a warning must not change meaning
/// with the wallpaper.
pub(crate) const ERROR: Color4f = Color4f::new(1.0, 0.576, 0.541, 1.0);
pub(crate) const ONLINE_GREEN: Color4f = Color4f::new(0.20, 0.84, 0.29, 1.0);

/// Everything about the console's look that follows the chosen background palette: which way
/// the text runs, what the glass is made of, and the accent that marks focus.
///
/// The console UI was white-on-dark throughout, with the brand violet hardcoded as the accent.
/// Both had to become palette-derived at once: a pale field needs dark text or it is
/// unreadable, and a violet focus wash on a copper field is the clash this exists to fix.
#[derive(Clone, Copy)]
pub(crate) struct Ink {
    /// Primary text/glyph colour, opaque.
    fg: Color4f,
    /// Focus wash, selected pill, caret — the palette's own accent.
    accent: Color4f,
    /// The base fill every glass panel starts from.
    glass: Color4f,
    /// What the vignette and legibility scrims tend toward — black under a dark field, white
    /// under a pale one (darkening a pastel field would strand the dark text on it) — with the
    /// alpha carrying HOW HARD. A pale field needs far less: mixing toward white at the dark
    /// field's strength bleaches the chroma straight out of the gradient.
    pub(crate) scrim: Color4f,
}

/// The shipped dark look — also what a test or a preview gets before any palette is applied.
const DARK_INK: Ink = Ink {
    fg: Color4f::new(1.0, 1.0, 1.0, 1.0),
    // The punktfunk brand violet, DARK-appearance value (#8678F5).
    accent: Color4f::new(0.525, 0.471, 0.961, 1.0),
    glass: Color4f::new(0.086, 0.086, 0.125, 0.62),
    scrim: Color4f::new(0.0, 0.0, 0.0, 1.0),
};

impl Ink {
    /// The ink a palette calls for. On a pale field the text goes near-black (tinted toward the
    /// palette's own ground so it doesn't read as a foreign grey) and the glass turns to white
    /// frost, which is what keeps a row legible over a bright gradient.
    pub(crate) fn of(p: &crate::library::Palette) -> Ink {
        let accent = Color4f::new(p.accent.0 as f32, p.accent.1 as f32, p.accent.2 as f32, 1.0);
        if !p.light {
            return Ink { accent, ..DARK_INK };
        }
        let g = p.ground;
        Ink {
            fg: Color4f::new(
                (g.0 * 0.16) as f32,
                (g.1 * 0.14) as f32,
                (g.2 * 0.20) as f32,
                1.0,
            ),
            accent,
            // More body than the dark glass carries: white frost over a bright gradient has
            // far less to separate it from its backdrop than dark glass over a dark one.
            glass: Color4f::new(1.0, 1.0, 1.0, 0.66),
            scrim: Color4f::new(1.0, 1.0, 1.0, 0.45),
        }
    }
}

thread_local! {
    /// The ink the CURRENT frame draws with. A thread-local rather than a parameter because
    /// every widget, glyph and panel in the crate reads it and the console renders on exactly
    /// one thread — threading an `Ink` through ~90 call sites would be all cost and no safety.
    /// [`crate::shell::Shell::render`] sets it once per frame, before anything draws.
    static INK: std::cell::Cell<Ink> = const { std::cell::Cell::new(DARK_INK) };

    /// Whether this frame draws in reduced-motion mode (`trust::Settings::reduce_motion`).
    /// Published here for the same reason the ink is: motion is decided in ~15 places
    /// scattered across widgets, screens and the shell, and every one of them already reads
    /// a thread-local to know how to paint. Also set once per frame by
    /// [`crate::shell::Shell::render`].
    static REDUCE_MOTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn set_ink(ink: Ink) {
    INK.with(|i| i.set(ink));
}

pub(crate) fn ink() -> Ink {
    INK.with(std::cell::Cell::get)
}

pub(crate) fn set_reduce_motion(on: bool) {
    REDUCE_MOTION.with(|r| r.set(on));
}

/// Is travel suppressed this frame? Callers keep the STATE change and drop the journey:
/// a focused row is still focused, a stepped value still stepped — they simply arrive
/// instead of gliding. Never used to skip a haptic; the pulse is the feedback that
/// replaces the motion, not another thing to take away.
pub(crate) fn reduce_motion() -> bool {
    REDUCE_MOTION.with(std::cell::Cell::get)
}

/// The foreground at `alpha` — white on a dark palette, near-black on a pale one.
pub(crate) fn fg(alpha: f32) -> Color4f {
    let c = ink().fg;
    Color4f::new(c.r, c.g, c.b, alpha)
}

/// The palette's accent at `alpha`.
pub(crate) fn accent(alpha: f32) -> Color4f {
    let c = ink().accent;
    Color4f::new(c.r, c.g, c.b, alpha)
}

/// A wash laid UNDER text to seat it against the field — black on a dark palette, white on a
/// pale one. `alpha` is the dark-field strength; a pale field needs less (see [`Ink::scrim`]),
/// so it is scaled the same way the backdrop's own scrims are.
pub(crate) fn shade(alpha: f32) -> Color4f {
    let s = ink().scrim;
    Color4f::new(s.r, s.g, s.b, alpha * s.a)
}

/// Ink that reads ON the accent (a filled key, a selected pill): whichever of black or white
/// the accent has more room for. Chosen by luminance rather than by `light`, because an accent
/// is picked for contrast against the GLASS, not against the field.
pub(crate) fn on_accent() -> Color4f {
    let a = ink().accent;
    let luma = 0.2126 * a.r + 0.7152 * a.g + 0.0722 * a.b;
    if luma > 0.55 {
        Color4f::new(0.0, 0.0, 0.0, 1.0)
    } else {
        Color4f::new(1.0, 1.0, 1.0, 1.0)
    }
}

// --- Panels (the Liquid Glass stand-in) --------------------------------------------------

pub(crate) enum PanelStroke {
    /// Hairline white at this alpha (rows, pills).
    Plain(f32),
    /// White .22 → .04 top→bottom (the host tiles' gradient edge).
    Gradient,
    /// The gradient edge dashed `[6,5]` (discovered / Add-Host tiles).
    GradientDashed,
    /// Brand-colored hairline (the actively edited field).
    Brand(f32),
}

/// One glass panel: base fill, optional tint wash, hairline stroke. `corner` and the
/// dash pattern are DESIGN units — the caller's `k` scales them.
pub(crate) fn panel(
    canvas: &Canvas,
    rect: Rect,
    corner: f32,
    tint: Option<Color4f>,
    stroke: PanelStroke,
    k: f32,
) {
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.draw_rrect(rr, &Paint::new(ink().glass, None));
    if let Some(tint) = tint {
        canvas.draw_rrect(rr, &Paint::new(tint, None));
    }
    let mut sp = Paint::default();
    sp.set_style(skia_safe::PaintStyle::Stroke);
    sp.set_stroke_width(1.0);
    sp.set_anti_alias(true);
    match stroke {
        PanelStroke::Plain(alpha) => {
            sp.set_color4f(fg(alpha), None);
        }
        PanelStroke::Brand(alpha) => {
            sp.set_color4f(accent(alpha), None);
        }
        PanelStroke::Gradient | PanelStroke::GradientDashed => {
            let colors = [fg(0.22), fg(0.04)];
            sp.set_shader(gradient::shaders::linear_gradient(
                (
                    Point::new(rect.left, rect.top),
                    Point::new(rect.left, rect.bottom),
                ),
                &gradient::Gradient::new(
                    gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
                    gradient::Interpolation::default(),
                ),
                None,
            ));
            if matches!(stroke, PanelStroke::GradientDashed) {
                sp.set_path_effect(PathEffect::dash(&[6.0 * k, 5.0 * k], 0.0));
            }
        }
    }
    canvas.draw_rrect(rr, &sp);
}

/// The soft drop shadow under a focused tile — a blurred black round-rect behind it.
/// The colour half of the focus recede: neighbours lose SATURATION and BRIGHTNESS with
/// distance `d` (0 = focused, 1 = fully receded), as one 4×5 row-major matrix.
///
/// This is what the flat black veil could never do. A veil only darkens, so a receded card
/// stays as colourful as the focused one and the eye keeps reading it as a competing
/// subject; draining the colour is what makes it read as DEPTH. The veil survives at half
/// its old strength, doing the job it is actually good at — separating overlapping cards.
///
/// Row-major `[r…, g…, b…, a…]`, each row `[R G B A offset]`. The RGB rows are a standard
/// luminance-weighted saturation matrix (Rec. 709 weights) scaled by `sat`, with the
/// brightness shift in the offset column — SwiftUI's `.brightness()` is additive, and
/// matching it keeps the two codebases' recede comparable by eye.
pub(crate) fn recede_matrix(d: f64) -> [f32; 20] {
    let d = d.clamp(0.0, 1.0);
    let sat = (1.0 - RECEDE_SATURATION * d) as f32;
    // Toward the GROUND, not simply darker. Apple's `.brightness(-0.24·d)` is dark-mode
    // arithmetic: on a dark field, down is away. On one of this crate's six PALE palettes
    // it is exactly backwards — a darkened tile gains contrast against a light ground and
    // the UNFOCUSED card becomes the heaviest thing on screen. The scrim already knows
    // which way the field leans (it tends to black on a dark palette, white on a pale
    // one), so the recede borrows its direction.
    let toward_light = ink().scrim.r > 0.5;
    let bright = (RECEDE_BRIGHTNESS * d) as f32 * if toward_light { 1.0 } else { -1.0 };
    const LR: f32 = 0.2126;
    const LG: f32 = 0.7152;
    const LB: f32 = 0.0722;
    let (ir, ig, ib) = (LR * (1.0 - sat), LG * (1.0 - sat), LB * (1.0 - sat));
    [
        ir + sat,
        ig,
        ib,
        0.0,
        bright,
        ir,
        ig + sat,
        ib,
        0.0,
        bright,
        ir,
        ig,
        ib + sat,
        0.0,
        bright,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// How much colour a fully receded neighbour loses…
const RECEDE_SATURATION: f64 = 0.42;
/// …and how much light. Both ported from the Apple gamepad UI's focus recede.
const RECEDE_BRIGHTNESS: f64 = 0.24;

/// The lit top edge that makes glass read as a material rather than as a tinted rectangle:
/// a 1 px inner stroke fading from `fg(0.10)` to nothing over the top 40 % of the panel,
/// so the highlight sits on the top arc and dies away down the sides.
///
/// Deliberately a separate call rather than a flag on [`panel`]: it is worth drawing on
/// tiles and on the ONE focused row, and not worth it on the dozens of resting rows a
/// settings screen paints every frame. Making the caller ask keeps that discipline visible
/// instead of hiding it behind a default.
pub(crate) fn panel_highlight(canvas: &Canvas, rect: Rect, corner: f32, k: f32) {
    let inset = rect.with_inset((0.5 * k, 0.5 * k));
    let mut p = Paint::default();
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(k.max(1.0));
    p.set_anti_alias(true);
    let colors = [fg(0.10), fg(0.0)];
    p.set_shader(gradient::shaders::linear_gradient(
        (
            Point::new(rect.left, rect.top),
            Point::new(rect.left, rect.top + rect.height() * 0.4),
        ),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rrect(RRect::new_rect_xy(inset, corner * k, corner * k), &p);
}

/// An accent-tinted glow under the focused card — the palette-aware mark that says "this
/// one" from across a room, where a 2 % scale difference says nothing at all. Drawn behind
/// [`drop_shadow`], and only ever for the ONE focused tile, so it costs a single extra
/// blurred round-rect per frame.
pub(crate) fn focus_halo(canvas: &Canvas, rect: Rect, corner: f32, k: f32, f: f32) {
    if f <= 0.01 {
        return;
    }
    let mut p = Paint::new(accent(0.28 * f), None);
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Normal,
        18.0 * k,
        None,
    ));
    // Grown slightly rather than offset: a halo is light spilling out of the card on every
    // side, where the shadow below it is the card's weight falling in one direction.
    let spread = rect.with_outset((6.0 * k, 6.0 * k));
    canvas.draw_rrect(RRect::new_rect_xy(spread, corner * k, corner * k), &p);
}

pub(crate) fn drop_shadow(canvas: &Canvas, rect: Rect, corner: f32, k: f32, alpha: f32) {
    let mut p = Paint::new(Color4f::new(0.0, 0.0, 0.0, alpha), None);
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Normal,
        10.0 * k,
        None,
    ));
    let shifted = rect.with_offset((0.0, 10.0 * k));
    canvas.draw_rrect(RRect::new_rect_xy(shifted, corner * k, corner * k), &p);
}

// --- The form backdrop (settings / add-host / pair) --------------------------------------
//
// There isn't one any more. The form screens used to sit on a STATIC deep-indigo field
// drawn here, crossfaded over the launcher's aurora; they now wear the same living mesh at
// `calm = 1` (see `library::mesh_sksl` and `Shell::draw_aurora`), which keeps the glass rows
// on real colour, keeps the console's one backdrop palette-themed everywhere, and means no
// screen in the gamepad UI is ever backed by a still image.

/// The loading/connecting spinner: a rotating 270° arc driven by the shell clock.
pub(crate) fn spinner(canvas: &Canvas, cx: f64, cy: f64, r: f64, t: f64) {
    let start = (t * 300.0) % 360.0;
    let mut paint = Paint::new(fg(0.85), None);
    paint.set_style(skia_safe::PaintStyle::Stroke);
    paint.set_stroke_width((r / 5.0) as f32);
    paint.set_stroke_cap(skia_safe::PaintCap::Round);
    paint.set_anti_alias(true);
    canvas.draw_arc(
        Rect::from_xywh(
            (cx - r) as f32,
            (cy - r) as f32,
            2.0 * r as f32,
            2.0 * r as f32,
        ),
        start as f32,
        270.0,
        false,
        &paint,
    );
}

// --- Typography ---------------------------------------------------------------------------

/// Geist weights the console uses (matching the Apple client's `.geist(size, weight)`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum W {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

/// The text toolkit shared by every screen: the four embedded Geist faces for direct
/// `draw_str` work, plus a paragraph collection ("Geist" with system fallback — game
/// titles can be CJK; `draw_str` can't shape those).
pub(crate) struct Fonts {
    regular: Typeface,
    medium: Typeface,
    semibold: Typeface,
    bold: Typeface,
    collection: FontCollection,
}

/// The Geist faces ride in the binary — the console must look right on a bare gamescope
/// session with no font packages to lean on (fontconfig still serves CJK fallback where
/// it exists).
const GEIST_REGULAR: &[u8] = include_bytes!("../assets/fonts/Geist-Regular.otf");
const GEIST_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Geist-Medium.otf");
const GEIST_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.otf");
const GEIST_BOLD: &[u8] = include_bytes!("../assets/fonts/Geist-Bold.otf");

pub(crate) fn build_fonts() -> Result<Fonts> {
    let mgr = FontMgr::new();
    let load = |bytes: &[u8], which: &str| {
        mgr.new_from_data(bytes, None)
            .ok_or_else(|| anyhow!("embedded Geist face rejected: {which}"))
    };
    let regular = load(GEIST_REGULAR, "Regular")?;
    let medium = load(GEIST_MEDIUM, "Medium")?;
    let semibold = load(GEIST_SEMIBOLD, "SemiBold")?;
    let bold = load(GEIST_BOLD, "Bold")?;

    // Paragraphs resolve "Geist" from the asset provider first (all four weights under
    // one alias — style matching picks the face), then fall back to the system manager.
    let mut provider = TypefaceFontProvider::new();
    for tf in [&regular, &medium, &semibold, &bold] {
        provider.register_typeface(tf.clone(), Some("Geist"));
    }
    let mut collection = FontCollection::new();
    collection.set_asset_font_manager(Some(provider.into()));
    collection.set_default_font_manager(FontMgr::new(), None);
    Ok(Fonts {
        regular,
        medium,
        semibold,
        bold,
        collection,
    })
}

impl Fonts {
    pub(crate) fn font(&self, w: W, size: f64) -> Font {
        let tf = match w {
            W::Regular => &self.regular,
            W::Medium => &self.medium,
            W::SemiBold => &self.semibold,
            W::Bold => &self.bold,
        };
        let mut f = Font::new(tf.clone(), size as f32);
        f.set_subpixel(true);
        f
    }

    pub(crate) fn measure(&self, text: &str, w: W, size: f64) -> f32 {
        self.font(w, size).measure_str(text, None).0
    }

    /// `draw_str` at a BASELINE. Returns the advance so callers can chain runs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        color: Color4f,
    ) -> f32 {
        let font = self.font(w, size);
        canvas.draw_str(
            text,
            Point::new(x as f32, baseline as f32),
            &font,
            &Paint::new(color, None),
        );
        font.measure_str(text, None).0
    }

    /// Letter-spaced small caps (the section headers' `tracking(1.4)` look) — Skia's
    /// `draw_str` has no tracking, so the run is placed char by char.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_tracked(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        tracking: f64,
        color: Color4f,
    ) {
        let font = self.font(w, size);
        let paint = Paint::new(color, None);
        let mut pen = x as f32;
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            let s = ch.encode_utf8(&mut buf);
            canvas.draw_str(&*s, Point::new(pen, baseline as f32), &font, &paint);
            pen += font.measure_str(&*s, None).0 + tracking as f32;
        }
    }

    fn paragraph(
        &self,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        align: TextAlign,
        max_w: f64,
    ) -> skia_safe::textlayout::Paragraph {
        let mut style = ParagraphStyle::new();
        style.set_text_align(align);
        let mut ts = TextStyle::new();
        ts.set_font_families(&["Geist"]);
        ts.set_font_size(size as f32);
        ts.set_color(color.to_color());
        ts.set_font_style(match w {
            W::Regular => FontStyle::normal(),
            W::Medium => FontStyle::new(
                skia_safe::font_style::Weight::MEDIUM,
                skia_safe::font_style::Width::NORMAL,
                skia_safe::font_style::Slant::Upright,
            ),
            W::SemiBold => FontStyle::new(
                skia_safe::font_style::Weight::SEMI_BOLD,
                skia_safe::font_style::Width::NORMAL,
                skia_safe::font_style::Slant::Upright,
            ),
            W::Bold => FontStyle::bold(),
        });
        style.set_text_style(&ts);
        let mut b = ParagraphBuilder::new(&style, self.collection.clone());
        b.add_text(text);
        let mut p = b.build();
        p.layout(max_w as f32);
        p
    }

    /// Centered, wrapping paragraph with `y` as its TOP edge (shaping + CJK fallback).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn centered(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        cx: f64,
        y: f64,
        max_w: f64,
    ) {
        let p = self.paragraph(text, w, size, color, TextAlign::Center, max_w);
        p.paint(canvas, Point::new((cx - max_w / 2.0) as f32, y as f32));
    }

    /// A single shaped line, middle-ellipsized to `max_w`, drawn at a baseline. For
    /// host/game titles that may exceed their tile.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_clipped(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        color: Color4f,
        max_w: f64,
    ) {
        let font = self.font(w, size);
        if font.measure_str(text, None).0 <= max_w as f32 {
            self.draw(canvas, text, x, baseline, w, size, color);
            return;
        }
        let ell = "…";
        let ell_w = font.measure_str(ell, None).0;
        let mut fitted = String::new();
        let mut used = 0.0f32;
        for ch in text.chars() {
            let cw = font.measure_str(ch.to_string().as_str(), None).0;
            if used + cw + ell_w > max_w as f32 {
                break;
            }
            fitted.push(ch);
            used += cw;
        }
        fitted.push_str(ell);
        self.draw(canvas, &fitted, x, baseline, w, size, color);
    }
}

/// Resolve the first available family. Generic aliases ("sans-serif", "monospace")
/// resolve through fontconfig on Linux; Windows' DirectWrite-backed FontMgr has no
/// generic aliases, so the list falls through to concrete family names there.
pub(crate) fn match_first_family(
    mgr: &FontMgr,
    families: &[&str],
    style: FontStyle,
) -> Option<Typeface> {
    families
        .iter()
        .find_map(|f| mgr.match_family_style(f, style))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded Geist faces must all parse — a bad asset would take out every
    /// console screen at init.
    #[test]
    fn embedded_fonts_load() {
        let fonts = build_fonts().expect("Geist faces load");
        for w in [W::Regular, W::Medium, W::SemiBold, W::Bold] {
            assert!(fonts.measure("Punktfunk", w, 16.0) > 0.0);
        }
        // Heavier faces render wider at the same size — proves four distinct faces.
        assert!(
            fonts.measure("Punktfunk", W::Bold, 16.0)
                > fonts.measure("Punktfunk", W::Regular, 16.0)
        );
    }

    /// Apply the 4×5 row-major matrix to one unpremultiplied RGBA colour, the way Skia
    /// does — without the clamp, so the maths is testable at the edges.
    fn apply(m: &[f32; 20], c: [f32; 4]) -> [f32; 4] {
        core::array::from_fn(|row| {
            let o = row * 5;
            m[o] * c[0] + m[o + 1] * c[1] + m[o + 2] * c[2] + m[o + 3] * c[3] + m[o + 4]
        })
    }

    /// The focused card must come out EXACTLY as it went in. This is the assertion that
    /// makes it safe to hand every card the same code path — a matrix that tinted the focus
    /// by half a percent would be invisible in review and wrong in every screenshot.
    #[test]
    fn recede_matrix_is_identity_at_the_focus() {
        let m = recede_matrix(0.0);
        for c in [
            [1.0, 0.2, 0.4, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.3, 0.9, 0.1, 0.5],
        ] {
            let out = apply(&m, c);
            for i in 0..4 {
                assert!((out[i] - c[i]).abs() < 1e-5, "{c:?} became {out:?}");
            }
        }
    }

    #[test]
    fn recede_matrix_drains_colour_and_light_but_never_alpha() {
        let m = recede_matrix(1.0);
        let c = [0.9f32, 0.2, 0.15, 1.0]; // a saturated red poster
        let out = apply(&m, c);
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(
            spread(out) < spread(c) * 0.7,
            "colour did not drain: {out:?}"
        );
        let lum = |v: [f32; 4]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        assert!(lum(out) < lum(c), "it did not darken: {out:?}");
        // ALPHA IS UNTOUCHED, and that is load-bearing: coverflow side cards overlap, so a
        // recede that reached alpha would let them show through each other.
        assert_eq!(out[3], 1.0);
    }

    /// The recede must move a card TOWARD ITS GROUND, which is the opposite direction on a
    /// pale palette. Getting this wrong is not subtle and is not caught by any dark-palette
    /// screenshot: on `mint` or `holo` a darkened neighbour gains contrast against the
    /// light field, so the UNFOCUSED tile becomes the loudest thing on screen.
    #[test]
    fn recede_moves_toward_the_ground_on_a_pale_palette() {
        let lum = |v: [f32; 4]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        let card = [0.55f32, 0.55, 0.6, 1.0];

        set_ink(Ink::of(crate::library::palette("violet")));
        assert!(ink().scrim.r < 0.5, "violet is a dark field");
        let dark_side = apply(&recede_matrix(1.0), card);

        set_ink(Ink::of(crate::library::palette("mint")));
        assert!(ink().scrim.r > 0.5, "mint is a pale field");
        let pale_side = apply(&recede_matrix(1.0), card);

        assert!(
            lum(dark_side) < lum(card),
            "on a dark field a receded card sinks"
        );
        assert!(
            lum(pale_side) > lum(card),
            "on a pale field it must LIFT, not sink: {pale_side:?}"
        );
        // Both drain colour, whichever way the light goes — saturation has no handedness.
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(spread(dark_side) < spread(card));
        assert!(spread(pale_side) < spread(card));

        set_ink(DARK_INK);
    }
}
