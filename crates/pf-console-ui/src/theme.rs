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
    gradient_shader, Canvas, Color4f, Font, FontMgr, FontStyle, MaskFilter, Paint, PathEffect,
    Point, RRect, Rect, TileMode, Typeface,
};

// --- Palette -----------------------------------------------------------------------------

/// The punktfunk brand violet — the DARK-appearance value (#8678F5); the console UI is
/// always dark. (Light surfaces use #6656F2; nothing here is light.)
pub(crate) const BRAND: Color4f = Color4f::new(0.525, 0.471, 0.961, 1.0);
pub(crate) const WHITE: Color4f = Color4f::new(1.0, 1.0, 1.0, 1.0);
pub(crate) const DIM: Color4f = Color4f::new(1.0, 1.0, 1.0, 0.55);
pub(crate) const FAINT: Color4f = Color4f::new(1.0, 1.0, 1.0, 0.35);
/// The error/status red (the GTK client's #ff938a).
pub(crate) const ERROR: Color4f = Color4f::new(1.0, 0.576, 0.541, 1.0);
pub(crate) const ONLINE_GREEN: Color4f = Color4f::new(0.20, 0.84, 0.29, 1.0);

pub(crate) fn white(alpha: f32) -> Color4f {
    Color4f::new(1.0, 1.0, 1.0, alpha)
}

pub(crate) fn brand(alpha: f32) -> Color4f {
    Color4f::new(BRAND.r, BRAND.g, BRAND.b, alpha)
}

/// The dark-glass base fill every panel starts from.
const GLASS_BASE: Color4f = Color4f::new(0.086, 0.086, 0.125, 0.62);

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
    canvas.draw_rrect(rr, &Paint::new(GLASS_BASE, None));
    if let Some(tint) = tint {
        canvas.draw_rrect(rr, &Paint::new(tint, None));
    }
    let mut sp = Paint::default();
    sp.set_style(skia_safe::PaintStyle::Stroke);
    sp.set_stroke_width(1.0);
    sp.set_anti_alias(true);
    match stroke {
        PanelStroke::Plain(alpha) => {
            sp.set_color4f(white(alpha), None);
        }
        PanelStroke::Brand(alpha) => {
            sp.set_color4f(brand(alpha), None);
        }
        PanelStroke::Gradient | PanelStroke::GradientDashed => {
            sp.set_shader(gradient_shader::linear(
                (
                    Point::new(rect.left, rect.top),
                    Point::new(rect.left, rect.bottom),
                ),
                gradient_shader::GradientShaderColors::Colors(&[
                    white(0.22).to_color(),
                    white(0.04).to_color(),
                ]),
                None,
                TileMode::Clamp,
                None,
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
    let mut paint = Paint::new(white(0.85), None);
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
}
