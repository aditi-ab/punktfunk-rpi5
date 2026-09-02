//! Shared look for the console shell: palette ink, embedded Geist, glass
//! panels, and the paint constructors every fill in this crate goes through.
//!
//! Skia's `Paint` defaults anti-alias off. [`fill`], [`stroke`], [`shaded`],
//! and [`layer`] are the only constructors; the shell test fails a stray
//! `Paint::new` outside this file. [`Ink`] is per-frame palette state,
//! published on a thread-local by [`crate::shell::Shell::render`]. Form
//! screens share the aurora mesh at `calm = 1`. No static backdrop here.
//!
//! Recede evidence: `recede_matrix_is_identity_at_the_focus` and neighbours.

use anyhow::{anyhow, Result};
use skia_safe::textlayout::{
    FontCollection, Paragraph, ParagraphBuilder, ParagraphStyle, TextAlign, TextStyle,
    TypefaceFontProvider,
};
use skia_safe::{
    gradient, Canvas, Color4f, Font, FontMgr, FontStyle, MaskFilter, Paint, PathEffect, Point,
    RRect, Rect, TileMode, Typeface,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Anti-aliased fill. Skia's `Paint::new` defaults `fAntiAlias` off, so a
/// bare constructor hard-steps every round-rect and glyph.
pub(crate) fn fill(color: Color4f) -> Paint {
    let mut p = Paint::new(color, None);
    p.set_anti_alias(true);
    p
}

/// Stroke of `width` device pixels. Callers that scale by `k` pass `width * k`.
pub(crate) fn stroke(color: Color4f, width: f32) -> Paint {
    let mut p = fill(color);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// Shader paint, opaque by construction. Skia multiplies shader output by
/// the paint's alpha, so an alpha-0 placeholder draws nothing.
pub(crate) fn shaded() -> Paint {
    fill(Color4f::new(0.0, 0.0, 0.0, 1.0))
}

/// [`shaded`] as a stroke. Opaque, or Skia scales the gradient's alpha away.
pub(crate) fn shaded_stroke(width: f32) -> Paint {
    let mut p = shaded();
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// Radial shade under the in-stream ring so the discs lift off the picture.
/// `alpha` tracks the ring's opening.
pub(crate) fn ring_scrim(cx: f32, cy: f32, r: f32, alpha: f32) -> Paint {
    let mut p = shaded();
    let colors = [
        Color4f::new(0.0, 0.0, 0.0, 0.36 * alpha),
        Color4f::new(0.0, 0.0, 0.0, 0.14 * alpha),
    ];
    p.set_shader(gradient::shaders::radial_gradient(
        (Point::new(cx, cy), r),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    p
}

/// Blurred black under a disc or card. Lifts translucent glass off a moving picture.
pub(crate) fn soft_shadow(alpha: f32, sigma: f32) -> Paint {
    let mut p = fill(Color4f::new(0.0, 0.0, 0.0, alpha));
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// Blurred white stroke. Drawn under the crisp ring so the glow bleeds past the edge.
pub(crate) fn glow_ring(alpha: f32, width: f32, sigma: f32) -> Paint {
    let mut p = stroke(Color4f::new(1.0, 1.0, 1.0, alpha), width);
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// Top-edge hairline. The stream cannot backdrop-blur, so this is the glass edge.
pub(crate) fn rim_light(top: f32, bottom: f32, alpha: f32, width: f32) -> Paint {
    let mut p = shaded_stroke(width);
    let colors = [
        Color4f::new(1.0, 1.0, 1.0, alpha),
        Color4f::new(1.0, 1.0, 1.0, 0.0),
    ];
    p.set_shader(gradient::shaders::linear_gradient(
        (Point::new(0.0, top), Point::new(0.0, bottom)),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    p
}

/// `save_layer` paint: filters only, no geometry, so AA is a no-op. Own constructor
/// so the AA guard can tell a compositing paint from a drawing one.
pub(crate) fn layer() -> Paint {
    Paint::default()
}

/// Linear + linear mipmap. `draw_image_rect` defaults to nearest with no mipmaps;
/// a poster shrunk into a cell then drops whole source rows.
pub(crate) fn art_sampling() -> skia_safe::SamplingOptions {
    skia_safe::SamplingOptions::new(skia_safe::FilterMode::Linear, skia_safe::MipmapMode::Linear)
}

/// Status red (GTK `#ff938a`). A warning must not follow the wallpaper.
pub(crate) const ERROR: Color4f = Color4f::new(1.0, 0.576, 0.541, 1.0);
pub(crate) const ONLINE_GREEN: Color4f = Color4f::new(0.20, 0.84, 0.29, 1.0);

/// Palette-derived fg, accent, glass, and scrim. Pale fields need dark text;
/// a brand-violet wash on a copper field clashes.
#[derive(Clone, Copy)]
pub(crate) struct Ink {
    fg: Color4f,
    accent: Color4f,
    glass: Color4f,
    /// Ground the vignette leans toward (black on dark, white on pale) and how
    /// hard (`a`). Mixing toward white at dark-field strength bleaches chroma.
    pub(crate) scrim: Color4f,
}

/// Shipped dark look, and the fallback before any palette is applied.
const DARK_INK: Ink = Ink {
    fg: Color4f::new(1.0, 1.0, 1.0, 1.0),
    // Brand violet, dark-appearance `#8678F5`.
    accent: Color4f::new(0.525, 0.471, 0.961, 1.0),
    glass: Color4f::new(0.086, 0.086, 0.125, 0.62),
    scrim: Color4f::new(0.0, 0.0, 0.0, 1.0),
};

impl Ink {
    /// Palette ink. Pale fields get near-black fg tinted toward the ground (a
    /// foreign grey reads as a second palette) and white-frost glass.
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
            // 0.66 vs dark's 0.62: white frost over a bright gradient has less to separate it.
            glass: Color4f::new(1.0, 1.0, 1.0, 0.66),
            scrim: Color4f::new(1.0, 1.0, 1.0, 0.45),
        }
    }

    /// OS-theme ink. The accent is already lifted by [`crate::os_theme::readable_accent`];
    /// an arbitrary OS colour is not contrast-safe as focus. Fg is the theme's own.
    pub(crate) fn of_os(t: &crate::os_theme::OsTheme) -> Ink {
        let c = |(r, g, b): (f64, f64, f64), a: f32| Color4f::new(r as f32, g as f32, b as f32, a);
        let accent = c(crate::os_theme::readable_accent(t), 1.0);
        if !t.light {
            return Ink {
                fg: c(t.foreground, 1.0),
                accent,
                // Theme field, not brand violet-grey: a panel sits a shade above the ground it covers.
                glass: c(crate::os_theme::mix(t.background, t.foreground, 0.10), 0.62),
                scrim: Color4f::new(0.0, 0.0, 0.0, 1.0),
            };
        }
        Ink {
            fg: c(t.foreground, 1.0),
            accent,
            glass: Color4f::new(1.0, 1.0, 1.0, 0.66),
            scrim: Color4f::new(1.0, 1.0, 1.0, 0.45),
        }
    }
}

thread_local! {
    /// Per-frame ink. Thread-local: one render thread owns the shell. Set by
    /// [`crate::shell::Shell::render`].
    static INK: std::cell::Cell<Ink> = const { std::cell::Cell::new(DARK_INK) };

    /// Per-frame `trust::Settings::reduce_motion`. Same thread-local as [`INK`].
    /// Set by [`crate::shell::Shell::render`].
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

/// Travel suppressed this frame. Callers keep the state change and drop the glide.
/// Never used to skip a haptic: the pulse replaces the motion.
pub(crate) fn reduce_motion() -> bool {
    REDUCE_MOTION.with(std::cell::Cell::get)
}

pub(crate) fn fg(alpha: f32) -> Color4f {
    let c = ink().fg;
    Color4f::new(c.r, c.g, c.b, alpha)
}

pub(crate) fn accent(alpha: f32) -> Color4f {
    let c = ink().accent;
    Color4f::new(c.r, c.g, c.b, alpha)
}

/// Text wash. `alpha` is dark-field strength; multiplied by [`Ink::scrim`].a so a
/// pale field does not bleach.
pub(crate) fn shade(alpha: f32) -> Color4f {
    let s = ink().scrim;
    Color4f::new(s.r, s.g, s.b, alpha * s.a)
}

/// Opaque coverless-card face, `tint` of the way from the field's ground toward accent.
/// Coverflow sides overlap, so glass would show the neighbour. Face and [`fg`] move
/// opposite ways with the palette; a fixed near-black face under pale `fg` fails contrast.
pub(crate) fn card_face(tint: f32) -> Color4f {
    let a = ink().accent;
    let base = if ink().scrim.r > 0.5 { 1.0 } else { 0.0 };
    let mix = |c: f32| c * tint + base * (1.0 - tint);
    Color4f::new(mix(a.r), mix(a.g), mix(a.b), 1.0)
}

/// Black or white on the accent, by luminance not `light`: an accent is picked
/// against the glass, not the field.
pub(crate) fn on_accent() -> Color4f {
    let a = ink().accent;
    let luma = 0.2126 * a.r + 0.7152 * a.g + 0.0722 * a.b;
    if luma > 0.55 {
        Color4f::new(0.0, 0.0, 0.0, 1.0)
    } else {
        Color4f::new(1.0, 1.0, 1.0, 1.0)
    }
}

pub(crate) enum PanelStroke {
    /// Hairline at this alpha (rows, pills).
    Plain(f32),
    /// Fg 0.22 → 0.04 top→bottom.
    Gradient,
    /// [`Gradient`] dashed `[6, 5]` design units.
    GradientDashed,
    /// Accent hairline (the field being edited).
    Brand(f32),
}

/// Glass panel. `corner` and the dash pattern are design units; the caller's `k` scales them.
pub(crate) fn panel(
    canvas: &Canvas,
    rect: Rect,
    corner: f32,
    tint: Option<Color4f>,
    stroke: PanelStroke,
    k: f32,
) {
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.draw_rrect(rr, &fill(ink().glass));
    if let Some(tint) = tint {
        canvas.draw_rrect(rr, &fill(tint));
    }
    // Opaque: Plain/Brand overwrite colour; a shader's output is scaled by this paint's alpha.
    let mut sp = shaded_stroke(1.0);
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

/// 4×5 row-major colour matrix: neighbours lose saturation and brightness with
/// `d` (0 = focused, 1 = fully receded). Rec. 709 sat mix, then lerp toward the
/// ground: `out = (1 − b)·sat_mix(c) + ground·b`.
pub(crate) fn recede_matrix(d: f64) -> [f32; 20] {
    let d = d.clamp(0.0, 1.0);
    let sat = (1.0 - RECEDE_SATURATION * d) as f32;
    // Toward the ground, not darker. A darkened tile on a pale field gains contrast.
    let toward_light = ink().scrim.r > 0.5;
    // Fraction of the way to the ground, not a level offset. An additive
    // darken clips a dark placeholder to #000; a lerp cannot clip at either pole.
    let b = (RECEDE_BRIGHTNESS * d) as f32;
    let ground = if toward_light { 1.0f32 } else { 0.0 };
    let keep = 1.0 - b;
    let offset = ground * b;
    const LR: f32 = 0.2126;
    const LG: f32 = 0.7152;
    const LB: f32 = 0.0722;
    let (ir, ig, ib) = (LR * (1.0 - sat), LG * (1.0 - sat), LB * (1.0 - sat));
    [
        keep * (ir + sat),
        keep * ig,
        keep * ib,
        0.0,
        offset,
        keep * ir,
        keep * (ig + sat),
        keep * ib,
        0.0,
        offset,
        keep * ir,
        keep * ig,
        keep * (ib + sat),
        0.0,
        offset,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// Saturation drain at full recede. Floor is 0.125 while brightness is 0.20:
/// channel spread must drop 30 %, and spread scales as `(1 − b)·sat`.
const RECEDE_SATURATION: f64 = 0.34;
/// Fraction of the way to the ground. An additive term clips; a dark card clips to nothing.
const RECEDE_BRIGHTNESS: f64 = 0.20;

/// Inner 1 px top-edge highlight over the top 40 % of the panel. Separate from
/// [`panel`] so resting rows skip the extra stroke.
pub(crate) fn panel_highlight(canvas: &Canvas, rect: Rect, corner: f32, k: f32) {
    let inset = rect.with_inset((0.5 * k, 0.5 * k));
    let mut p = shaded_stroke(k.max(1.0));
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
    // Inset 0.5: radius must drop by 0.5 too or the lit edge crosses the panel's corner arc.
    let r = ((corner - 0.5) * k).max(0.0);
    canvas.draw_rrect(RRect::new_rect_xy(inset, r, r), &p);
}

/// [`focus_halo`] growth past the card, design units. Applied to both rect and corner radius.
const HALO_OUTSET: f32 = 4.0;

/// Accent glow under the focused card. Drawn behind [`drop_shadow`].
pub(crate) fn focus_halo(canvas: &Canvas, rect: Rect, corner: f32, k: f32, f: f32) {
    if f <= 0.01 {
        return;
    }
    // Pale accents are dark, so a blurred accent on a pale field is a smudge.
    // Mix halfway to the scrim (white) to keep the hue; 0.24 vs 0.20 so the
    // lightened mark still registers.
    let (a, s) = (ink().accent, ink().scrim);
    let (c, alpha) = if s.r > 0.5 {
        let mix = |x: f32, y: f32| x + (y - x) * 0.5;
        (
            Color4f::new(mix(a.r, s.r), mix(a.g, s.g), mix(a.b, s.b), 1.0),
            0.24 * f,
        )
    } else {
        (a, 0.20 * f)
    };
    let mut p = fill(Color4f::new(c.r, c.g, c.b, alpha));
    // Outer, not Normal: Normal keeps the interior, so the halo fills the card
    // at full accent and shows through translucent glass (α 0.62 / 0.66).
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Outer,
        10.0 * k,
        None,
    ));
    // Outset, not offset: a halo spills every side; the shadow falls one way.
    // Reach is outset + 3σ and must stay inside the neighbour gap.
    let spread = rect.with_outset((HALO_OUTSET * k, HALO_OUTSET * k));
    // Radius grows by the same `d` as the rect or the arcs do not share a centre
    // and the halo reads squarer than the card at the corners.
    let r = (corner + HALO_OUTSET) * k;
    canvas.draw_rrect(RRect::new_rect_xy(spread, r, r), &p);
}

pub(crate) fn drop_shadow(canvas: &Canvas, rect: Rect, corner: f32, k: f32, alpha: f32) {
    // Scale 0.40 at the pale pole so the caller's alpha stays dark-field strength.
    let alpha = if ink().scrim.r > 0.5 {
        alpha * 0.40
    } else {
        alpha
    };
    let mut p = fill(Color4f::new(0.0, 0.0, 0.0, alpha));
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Normal,
        10.0 * k,
        None,
    ));
    let shifted = rect.with_offset((0.0, 10.0 * k));
    canvas.draw_rrect(RRect::new_rect_xy(shifted, corner * k, corner * k), &p);
}

/// Loading spinner. `t` is the shell clock.
pub(crate) fn spinner(canvas: &Canvas, cx: f64, cy: f64, r: f64, t: f64) {
    let start = (t * 300.0) % 360.0;
    let mut paint = stroke(fg(0.85), (r / 5.0) as f32);
    paint.set_stroke_cap(skia_safe::PaintCap::Round);
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

/// Chrome inset from the screen edge, design units. 24 matches Apple `.horizontal, 24`
/// and Android `ConsoleEdgeInset`. Not the legend's 18 (a pill edge). Screen inset,
/// not content margin: rows and coverflow are centred columns.
pub(crate) const EDGE_INSET: f64 = 24.0;

/// Geist weights, matching the Apple client's `.geist(size, weight)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum W {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

/// Embedded Geist plus a paragraph collection with system fallback. Titles can be
/// CJK, and `draw_str` cannot shape those.
pub(crate) struct Fonts {
    regular: Typeface,
    medium: Typeface,
    semibold: Typeface,
    bold: Typeface,
    collection: FontCollection,
    /// Shaped paragraphs keyed by [`ParaKey`]. Position is not in the key (`paint`
    /// takes it) so a scrolling shelf reuses the same layout. `RefCell`: every draw
    /// path is `&self` and the shell is one render thread.
    paragraphs: RefCell<HashMap<ParaKey, Cached>>,
    /// Liveness clock, bumped by [`Fonts::begin_frame`].
    frame: Cell<u64>,
}

/// Paragraph shape. A tag, not a loose `(TextAlign, Option<usize>)` pair: those two
/// are not independent, and this is half of a hash key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Para {
    Centered,
    Leading,
    /// Left-aligned, one ellipsized line.
    Heading,
}

impl Para {
    fn style(self) -> (TextAlign, Option<usize>) {
        match self {
            Para::Centered => (TextAlign::Center, None),
            Para::Leading => (TextAlign::Left, None),
            Para::Heading => (TextAlign::Left, Some(1)),
        }
    }
}

/// Everything that changes the shaped result. Floats ride as bits: sizes are
/// `k`-scaled so never whole, and `f32` is not `Hash`. Same `k` → same bits.
#[derive(PartialEq, Eq, Hash)]
struct ParaKey {
    text: String,
    kind: Para,
    weight: W,
    size: u64,
    max_w: u32,
    /// ARGB, as `[a, r, g, b]`.
    color: [u8; 4],
}

struct Cached {
    para: Paragraph,
    used: u64,
}

/// Cap before cold entries drop. A screen is well under this; the library can page
/// thousands of titles and would otherwise keep every one.
const PARA_CACHE_MAX: usize = 512;

/// Shape one paragraph. Free function: the cache hands a `&ParaKey` borrowed from
/// the map being inserted into, which rules out `&self` across the call.
fn shape(collection: &FontCollection, key: &ParaKey) -> Paragraph {
    let (align, clamp) = key.kind.style();
    let mut style = ParagraphStyle::new();
    style.set_text_align(align);
    if let Some(lines) = clamp {
        style.set_max_lines(lines);
        style.set_ellipsis("\u{2026}");
    }
    let mut ts = TextStyle::new();
    ts.set_font_families(&["Geist"]);
    ts.set_font_size(f64::from_bits(key.size) as f32);
    let [a, r, g, b] = key.color;
    ts.set_color(skia_safe::Color::from_argb(a, r, g, b));
    ts.set_font_style(match key.weight {
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
    let mut builder = ParagraphBuilder::new(&style, collection.clone());
    builder.add_text(&key.text);
    let mut p = builder.build();
    p.layout(f32::from_bits(key.max_w));
    p
}

/// Embedded so a bare gamescope session with no font packages still looks right.
/// fontconfig still serves CJK fallback where it exists.
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

    // Asset provider first: all four weights under one "Geist" alias so style matching
    // picks the face; system manager is fallback.
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
        paragraphs: RefCell::new(HashMap::new()),
        frame: Cell::new(0),
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

    /// `draw_str` at a baseline, not a top edge. Returns the advance so callers can chain runs.
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
            &fill(color),
        );
        font.measure_str(text, None).0
    }

    /// Letter-spaced run. Skia's `draw_str` has no tracking, so each char is placed.
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
        let paint = fill(color);
        let mut pen = x as f32;
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            let s = ch.encode_utf8(&mut buf);
            canvas.draw_str(&*s, Point::new(pen, baseline as f32), &font, &paint);
            pen += font.measure_str(&*s, None).0 + tracking as f32;
        }
    }

    /// Bump the paragraph-cache clock. Entries not drawn this frame or the last
    /// are eviction candidates. The shell calls this once per `render_in`.
    pub(crate) fn begin_frame(&self) {
        self.frame.set(self.frame.get().wrapping_add(1));
    }

    /// Draw a cached paragraph. `at` is top-left and is not part of the key.
    #[allow(clippy::too_many_arguments)]
    fn draw_paragraph(
        &self,
        canvas: &Canvas,
        text: &str,
        kind: Para,
        w: W,
        size: f64,
        color: Color4f,
        max_w: f64,
        at: Point,
    ) {
        let frame = self.frame.get();
        // Owned key: a hit still allocates a `String`. Cheap next to a reshape.
        let key = ParaKey {
            text: text.to_owned(),
            kind,
            weight: w,
            size: size.to_bits(),
            max_w: (max_w as f32).to_bits(),
            color: {
                // 8-bit ARGB the paragraph bakes, not the `Color4f`: two floats that
                // round to the same pixel share an entry.
                let c = color.to_color();
                [c.a(), c.r(), c.g(), c.b()]
            },
        };
        let mut cache = self.paragraphs.borrow_mut();
        let entry = cache.entry(key).or_insert_with_key(|k| Cached {
            para: shape(&self.collection, k),
            used: frame,
        });
        entry.used = frame;
        entry.para.paint(canvas, at);
        // Reap entries not drawn this frame or the last (`used + 1 >= frame`).
        if cache.len() > PARA_CACHE_MAX {
            cache.retain(|_, c| c.used + 1 >= frame);
        }
    }

    /// Centered wrapping paragraph; `y` is the top edge (CJK fallback).
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
        let at = Point::new((cx - max_w / 2.0) as f32, y as f32);
        self.draw_paragraph(canvas, text, Para::Centered, w, size, color, max_w, at);
    }

    /// Left-aligned twin of [`centered`](Self::centered). Same paragraph path (CJK
    /// fallback); chrome cannot use `draw`/`draw_clipped` instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn leading(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        x: f64,
        y: f64,
        max_w: f64,
    ) {
        let at = Point::new(x as f32, y as f32);
        self.draw_paragraph(canvas, text, Para::Leading, w, size, color, max_w, at);
    }

    /// Left-aligned heading at `(x, y)`, one ellipsized line at `max_w`. A wrap would
    /// run under the controller chip and push a second line into the content.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn heading(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        x: f64,
        y: f64,
        max_w: f64,
    ) {
        let at = Point::new(x as f32, y as f32);
        self.draw_paragraph(canvas, text, Para::Heading, w, size, color, max_w, at);
    }

    /// One line, ellipsized to `max_w`, at a baseline. For titles that exceed their tile.
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
        // Measure from a stack buffer, not a `String` per character: this runs
        // every over-long title, every frame.
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            let cw = font.measure_str(&*ch.encode_utf8(&mut buf), None).0;
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

/// First family that matches. Linux fontconfig resolves generic aliases
/// ("sans-serif"); Windows DirectWrite does not, so the list includes concrete names.
#[cfg(feature = "vulkan-overlay")]
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

    /// A bad embedded face takes out every console screen at init.
    #[test]
    fn embedded_fonts_load() {
        let fonts = build_fonts().expect("Geist faces load");
        for w in [W::Regular, W::Medium, W::SemiBold, W::Bold] {
            assert!(fonts.measure("Punktfunk", w, 16.0) > 0.0);
        }
        // Heavier faces are wider at the same size: four distinct faces, not one reused.
        assert!(
            fonts.measure("Punktfunk", W::Bold, 16.0)
                > fonts.measure("Punktfunk", W::Regular, 16.0)
        );
    }

    /// Skia's 4×5 row-major apply, without the clamp, so a channel below zero is visible.
    fn apply(m: &[f32; 20], c: [f32; 4]) -> [f32; 4] {
        core::array::from_fn(|row| {
            let o = row * 5;
            m[o] * c[0] + m[o + 1] * c[1] + m[o + 2] * c[2] + m[o + 3] * c[3] + m[o + 4]
        })
    }

    /// Focused card is identity, so every card can share one code path. A half-percent
    /// tint at d=0 is invisible on glass and wrong in every screenshot.
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
        let c = [0.9f32, 0.2, 0.15, 1.0]; // high chroma: spread drain is the assertion
        let out = apply(&m, c);
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(
            spread(out) < spread(c) * 0.7,
            "colour did not drain: {out:?}"
        );
        let lum = |v: [f32; 4]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        assert!(lum(out) < lum(c), "it did not darken: {out:?}");
        // Alpha is untouched: coverflow sides overlap, so a recede that reached alpha
        // would show neighbours through each other.
        assert_eq!(out[3], 1.0);
    }

    /// Recede moves toward the ground, the opposite direction on a pale palette.
    /// Dark-palette screenshots miss this: a darkened neighbour gains contrast.
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
        // Saturation has no handedness: both poles drain colour.
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(spread(dark_side) < spread(card));
        assert!(spread(pale_side) < spread(card));

        set_ink(DARK_INK);
    }

    /// Recede must not clamp. The other tests are relative and never feed a dark
    /// input. `apply` omits Skia's clamp, so a channel below zero is the clip.
    #[test]
    fn a_dark_card_face_survives_a_full_recede() {
        // Coverless card at the quieter placeholder tint: the darkest face the shelf has.
        for p in crate::library::PALETTES.iter().filter(|p| !p.light) {
            set_ink(Ink::of(p));
            let f = card_face(0.20);
            let out = apply(&recede_matrix(1.0), [f.r, f.g, f.b, 1.0]);
            for c in &out[..3] {
                assert!(
                    *c > 0.05,
                    "the recede crushed {}'s card face to black: {out:?}",
                    p.id
                );
            }
        }
        set_ink(DARK_INK);
    }
}
