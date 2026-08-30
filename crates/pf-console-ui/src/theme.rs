//! The console shell's shared look: the brand palette, the embedded Geist typography
//! (the same OFL faces the Apple client bundles — one brand voice on every platform),
//! the dark-glass panel primitive standing in for Liquid Glass (translucent fill +
//! hairline stroke; the backdrops here are soft gradients, so a real backdrop blur
//! would be indistinguishable and costs Deck GPU), and the quiet form backdrop the
//! settings/add-host/pair screens sit on.

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

// --- Paint ----------------------------------------------------------------------------------

/// A filled paint, ANTI-ALIASED. Build every fill in this crate here.
///
/// Skia's `SkPaint` defaults `fAntiAlias` to **false**, and `Paint::new(colour, None)` is that
/// default constructor with a colour on it — so the natural, terse way to write a Skia draw call
/// (`canvas.draw_rrect(rr, &Paint::new(c, None))`) silently produces HARD-STEPPED geometry. That
/// is the wrong default for this crate twice over: the console draws almost nothing axis-aligned
/// (round-rects, circles, arcs, D-pad and stick paths), and it is read from a couch on a 1280×800
/// Deck panel, where a stair-stepped 16 px glyph circle reads as a visible octagon.
///
/// The bug this exists to prevent is specific and it already happened: paints that got MUTATED
/// for some other reason (a stroke style, a width) collected a `set_anti_alias(true)` along the
/// way, while every inline `&Paint::new(…)` argument did not. The console therefore shipped
/// smooth 1 px rings drawn on top of jagged fills — which is worse-looking than no ring at all,
/// because the smooth edge gives the eye a reference for how wrong the fill is.
///
/// `theme::fill`/[`stroke`]/[`layer`] are the only sanctioned constructors, and
/// `shell::tests::paints_are_built_by_the_theme_constructors` fails the build if a bare
/// `Paint::new`/`Paint::default` reappears anywhere outside this file.
pub(crate) fn fill(color: Color4f) -> Paint {
    let mut p = Paint::new(color, None);
    p.set_anti_alias(true);
    p
}

/// A stroking paint of `width` DEVICE pixels, anti-aliased. Callers scaling by `k` pass
/// `width * k` — nothing here knows about design units.
pub(crate) fn stroke(color: Color4f, width: f32) -> Paint {
    let mut p = fill(color);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// A paint whose colour comes from a SHADER — a gradient, or the aurora's runtime effect.
/// Anti-aliased, and OPAQUE by construction, which is the whole point of it existing.
///
/// The colour channel is unused once a shader is attached, so the obvious thing is to build
/// one of these from a transparent placeholder and let the shader supply everything. That is
/// a trap: Skia modulates a shader's output by the PAINT'S ALPHA, so an alpha-0 placeholder
/// draws nothing whatever the shader says. It is a silent, total failure — the element simply
/// is not there — and it is invisible to a test that only asserts a frame renders without
/// panicking. `Paint::default` happened to be opaque black and so never showed the problem;
/// anything replacing it has to be deliberately opaque, so that is what this is.
pub(crate) fn shaded() -> Paint {
    fill(Color4f::new(0.0, 0.0, 0.0, 1.0))
}

/// [`shaded`]'s stroking twin — a gradient hairline, opaque so the gradient survives.
pub(crate) fn shaded_stroke(width: f32) -> Paint {
    let mut p = shaded();
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// The in-stream ring's scrim: a pool of shade around the ring's centre, thinning to a light
/// veil by `r` and flat beyond it — the discs sit in the shade, the stream past them stays
/// legible. `alpha` scales the whole thing with the ring's opening.
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

/// A soft drop shadow under a disc or a card: black at `alpha`, blurred by `sigma` — what
/// lifts a translucent shape off a moving picture.
pub(crate) fn soft_shadow(alpha: f32, sigma: f32) -> Paint {
    let mut p = fill(Color4f::new(0.0, 0.0, 0.0, alpha));
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// The ring's highlight halo: a white stroke `width` wide at `alpha`, blurred by `sigma` so
/// the light bleeds past the disc's edge. Drawn under the crisp ring.
pub(crate) fn glow_ring(alpha: f32, width: f32, sigma: f32) -> Paint {
    let mut p = stroke(Color4f::new(1.0, 1.0, 1.0, alpha), width);
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// The light catching a disc's top edge — glass's one cheap tell over a stream the console
/// cannot blur: a hairline whose white fades from `alpha` at `top` to nothing by `bottom`.
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

/// The paint for a `save_layer` — it carries alpha and colour filters, and never any geometry
/// of its own, so anti-aliasing has nothing to act on. Its own constructor so the AA guard (and
/// a reader) can tell a compositing paint from a drawing one without reading the call site.
pub(crate) fn layer() -> Paint {
    Paint::default()
}

/// How the console samples bitmap art (poster/cover images, launcher icons).
///
/// `Canvas::draw_image_rect`'s default is `SamplingOptions::default()` — `FilterMode::Nearest`
/// with `MipmapMode::None`, i.e. NO filtering at all. Every cover in the library is minified
/// hard (a 600×900 poster into a ~180×270 Deck cell), and nearest-neighbour minification drops
/// whole rows and columns of source pixels: box-art lettering breaks up, edges crawl as the
/// shelf scrolls, and the result reads as "low resolution" no matter what the panel is. Linear
/// with a linear mipmap chain is the fix — the mip level does the bulk of the reduction, so the
/// filter is never asked to shrink by more than 2×, which is the one thing bilinear does well.
pub(crate) fn art_sampling() -> skia_safe::SamplingOptions {
    skia_safe::SamplingOptions::new(skia_safe::FilterMode::Linear, skia_safe::MipmapMode::Linear)
}

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

/// An OPAQUE card face, `tint` of the way from the ground side of the field toward the
/// palette's accent — the backdrop for a cover we have no art for.
///
/// Opaque is the constraint that rules the alternatives out: coverflow side cards OVERLAP, so
/// a glass face would show its neighbour through it, and `accent(0.20)` over nothing is
/// exactly that. Mixing the same tint into a base the field's own lean chooses (black under a
/// dark palette, white under a pale one, which is what [`Ink::scrim`] already knows) gets the
/// accent tint with no alpha spent.
///
/// The pairing with [`fg`] is the point of it. A fixed near-black face carrying `fg()` ink was
/// legible on the seven dark palettes and ABSENT on the six pale ones, where `fg()` is itself a
/// near-black tinted toward the ground — the two composited to 1.03:1. Face and ink now move in
/// opposite directions with the palette, so they separate at both poles by construction.
pub(crate) fn card_face(tint: f32) -> Color4f {
    let a = ink().accent;
    let base = if ink().scrim.r > 0.5 { 1.0 } else { 0.0 };
    let mix = |c: f32| c * tint + base * (1.0 - tint);
    Color4f::new(mix(a.r), mix(a.g), mix(a.b), 1.0)
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
    canvas.draw_rrect(rr, &fill(ink().glass));
    if let Some(tint) = tint {
        canvas.draw_rrect(rr, &fill(tint));
    }
    // Opaque to start with: the Plain/Brand arms overwrite the colour outright, and the
    // gradient arms attach a shader whose output this paint's alpha would otherwise scale
    // away to nothing.
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

/// The colour half of the focus recede: neighbours lose SATURATION and BRIGHTNESS with
/// distance `d` (0 = focused, 1 = fully receded), as one 4×5 row-major matrix.
///
/// This is what the flat black veil could never do. A veil only darkens, so a receded card
/// stays as colourful as the focused one and the eye keeps reading it as a competing
/// subject; draining the colour is what makes it read as DEPTH. The veil survives at half
/// its old strength, doing the job it is actually good at — separating overlapping cards.
///
/// Row-major `[r…, g…, b…, a…]`, each row `[R G B A offset]`. The RGB rows are a standard
/// luminance-weighted saturation matrix (Rec. 709 weights) scaled by `sat`, the whole of it
/// then LERPED toward the ground: `out = (1 − b)·sat_mix(c) + ground·b`.
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
    // A FRACTION of the way to the ground, not a level offset. SwiftUI's `.brightness()` is
    // additive and this matched it, which meant −0.24 was −61/255 on every channel and
    // Skia's colour matrix clamps: the coverflow's own #1E1E25 placeholder came out at
    // literal #000000, the whole side stack a single black slab with no depth in it and no
    // cover-art detail left to see. A lerp cannot clip at either pole, and it is also the
    // arithmetic "receding into the field" actually means — a fixed subtraction is a
    // different amount of recede for every card and total annihilation for a dark one.
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

/// How much colour a fully receded neighbour loses. Gentle enough that a side card still
/// reads as the artwork it is — drain more and the shelf looks like a filter was applied to
/// it rather than like the cards are standing further away. Cannot go below 0.125 while the
/// brightness term is 0.20: `recede_matrix_drains_colour_and_light_but_never_alpha` wants
/// the channel spread cut by 30 %, and spread scales exactly as `(1 − b)·sat`.
const RECEDE_SATURATION: f64 = 0.34;
/// …and how far it travels toward the ground, as a FRACTION of the distance — never as a
/// level offset, whatever the Apple gamepad UI's `.brightness()` does. See
/// [`recede_matrix`]: an additive term clips, and a card dark enough clips to nothing.
const RECEDE_BRIGHTNESS: f64 = 0.20;

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
    // Concentric, the same rule the halo states: pulled in by half a unit, so the radius
    // comes in by half a unit too or the lit edge crosses the panel's own corner arc.
    let r = ((corner - 0.5) * k).max(0.0);
    canvas.draw_rrect(RRect::new_rect_xy(inset, r, r), &p);
}

/// How far [`focus_halo`] is grown past the card on every side, in design units. Both the
/// rect AND the corner radius take it — see the draw there.
const HALO_OUTSET: f32 = 4.0;

/// An accent-tinted glow under the focused card — the palette-aware mark that says "this
/// one" from across a room, where a 2 % scale difference says nothing at all. Drawn behind
/// [`drop_shadow`], and only ever for the ONE focused tile, so it costs a single extra
/// blurred round-rect per frame.
pub(crate) fn focus_halo(canvas: &Canvas, rect: Rect, corner: f32, k: f32, f: f32) {
    if f <= 0.01 {
        return;
    }
    // Every pale palette's accent is DARK — mint 0.34 luma, sunset 0.26, opal 0.33 — so a
    // blurred accent on a pale field is a smudge, and the focused tile came out the dirtiest
    // thing in the row while its unfocused neighbours stayed clean and light: the focus mark
    // inverted. Mixing halfway to the scrim (white there) keeps the palette's own hue while
    // making the mark read as light. It needs a little more body to register once lightened.
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
    // Outer, not Normal: Normal keeps the blurred shape's INTERIOR, so the halo also filled
    // the card's own footprint at full accent. On the home and collections tiles the panel
    // glass over it is translucent (α 0.62 dark, 0.66 pale), so a third of that came through
    // the face and the focused card read as a lit blob rather than as a card with light
    // spilling around it.
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Outer,
        10.0 * k,
        None,
    ));
    // Grown slightly rather than offset: a halo is light spilling out of the card on every
    // side, where the shadow below it is the card's weight falling in one direction. The
    // reach is outset + 3σ and it has to stay INSIDE the gap to the next card: at 6 + 3·18
    // it overran the coverflow's 58 dp focused-to-neighbour gap, and since the strip paints
    // farthest-first the focused card's corona landed on top of its neighbours — which is
    // what made every card look like it was glowing.
    let spread = rect.with_outset((HALO_OUTSET * k, HALO_OUTSET * k));
    // Concentric: a shape grown by `d` on every side keeps its corners parallel to the
    // original's only if its radius grows by `d` too (the two arcs then share a centre).
    // Reusing the card's own radius left the halo squarer than the card it sits under, so
    // it read as a misaligned outline at the four corners and a clean glow along the edges.
    let r = (corner + HALO_OUTSET) * k;
    canvas.draw_rrect(RRect::new_rect_xy(spread, r, r), &p);
}

pub(crate) fn drop_shadow(canvas: &Canvas, rect: Rect, corner: f32, k: f32, alpha: f32) {
    // Black under a tile is WEIGHT on a dark field and DIRT on a pale one, where it is the
    // heaviest mark on the screen: on `holo` and `sunset` the focused tile sat in a muddy
    // grey-brown ring while every unfocused tile stayed clean. Scaled back at the pale pole
    // the same way the scrim already scales itself, so the caller's alpha keeps meaning
    // "dark-field strength" and no call site has to know which palette is up.
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

// --- Layout -------------------------------------------------------------------------------

/// How far in from a screen's edge its CHROME sits, design units — the heading, the section
/// strip under it and the controller chip on the right all share this one column.
///
/// The number is the other clients' verbatim: Apple pads every gamepad heading and its tab
/// strip `.horizontal, 24`, Android names it `ConsoleEdgeInset = 24.dp`. It is deliberately
/// NOT the legend's 18 — that is a PILL's edge, whose first glyph lands at 31, so matching it
/// would misalign the very thing it was copied from.
///
/// It is a screen inset, not a content margin: the rows, the carousel and the coverflow are
/// all CENTRED columns, so aligning a heading to one would mean tracking `(width − column)/2`,
/// which is an artefact of the window size rather than a margin anyone chose.
pub(crate) const EDGE_INSET: f64 = 24.0;

// --- Typography ---------------------------------------------------------------------------

/// Geist weights the console uses (matching the Apple client's `.geist(size, weight)`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Shaped paragraphs, keyed by everything that shapes one ([`ParaKey`]).
    ///
    /// `Paragraph::layout` runs the whole shaper — HarfBuzz, line breaking, font fallback —
    /// and the shell re-built every paragraph on screen from scratch EVERY frame, which on a
    /// TV box is the largest CPU cost in the frame. Position is deliberately not part of the
    /// key (`paint` takes it), so one shaped paragraph serves a string wherever it moves to:
    /// a scrolling shelf and a screen transition both re-use it rather than re-shaping.
    ///
    /// `RefCell` because every draw path here takes `&self` and the console's shell is
    /// single-threaded by construction (one render thread owns it on all three ABIs).
    paragraphs: RefCell<HashMap<ParaKey, Cached>>,
    /// The frame counter [`Fonts::begin_frame`] bumps — the cache's liveness clock.
    frame: Cell<u64>,
}

/// The three paragraph shapes the console draws. A single tag rather than a loose
/// `(TextAlign, Option<usize>)` pair because it is half of a hash key, and because those two
/// were never independent — every call site picks one of these three.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Para {
    /// Centred, wrapping freely.
    Centered,
    /// Left-aligned, wrapping freely.
    Leading,
    /// Left-aligned, clamped to one ellipsized line.
    Heading,
}

impl Para {
    /// The paragraph style this shape asks for: alignment, and the line clamp if it has one.
    fn style(self) -> (TextAlign, Option<usize>) {
        match self {
            Para::Centered => (TextAlign::Center, None),
            Para::Leading => (TextAlign::Left, None),
            Para::Heading => (TextAlign::Left, Some(1)),
        }
    }
}

/// Everything [`shape`] bakes into a laid-out `Paragraph` — change any of it and the shaped
/// result differs, so all of it is in the key.
///
/// The floats ride as bits: the sizes and widths are all `k`-scaled, so they are never whole
/// numbers, and `f64`/`f32` are not `Hash`. Bit equality is the right test anyway — the same
/// `k` produces the same bits, and a different `k` must re-shape.
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

/// One shaped paragraph and the frame it was last drawn on.
struct Cached {
    para: Paragraph,
    used: u64,
}

/// How many shaped paragraphs stay resident before the cold ones are dropped. A screen draws
/// well under this; the ceiling exists for the library, where paging a large catalogue walks
/// through thousands of titles and every one of them would otherwise be kept forever.
const PARA_CACHE_MAX: usize = 512;

/// Build and lay out one paragraph — the shaping [`Fonts::draw_paragraph`]'s cache exists to
/// do exactly once per distinct key.
///
/// A free function rather than a method because the cache hands it a `&ParaKey` borrowed out
/// of the map it is inserting into, which rules out holding `&self` across the call.
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
            &fill(color),
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
        let paint = fill(color);
        let mut pen = x as f32;
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            let s = ch.encode_utf8(&mut buf);
            canvas.draw_str(&*s, Point::new(pen, baseline as f32), &font, &paint);
            pen += font.measure_str(&*s, None).0 + tracking as f32;
        }
    }

    /// Start a frame — the paragraph cache's clock. Anything not drawn on this frame or the
    /// one before it becomes a candidate for eviction, so the live set is exactly "what the
    /// last two frames drew". The shell calls this once per `render_in`.
    pub(crate) fn begin_frame(&self) {
        self.frame.set(self.frame.get().wrapping_add(1));
    }

    /// Draw a shaped paragraph, building and laying it out only the first time this exact
    /// (text, shape, weight, size, width, colour) is asked for — see [`Fonts::paragraphs`].
    /// `at` is the paragraph's TOP-LEFT, and is deliberately not part of the key.
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
        // ponytail: the key owns its text, so a HIT still costs one small `String` allocation
        // where a borrowed-key lookup would cost none. Deliberate — it is a rounding error
        // against the shape it replaces, and the alternatives (hash-only keys, `hashbrown`'s
        // raw entry) trade a real collision risk or a dependency for it. Revisit only if a
        // profile ever puts this line on the board.
        let key = ParaKey {
            text: text.to_owned(),
            kind,
            weight: w,
            size: size.to_bits(),
            max_w: (max_w as f32).to_bits(),
            color: {
                // The 8-bit ARGB the paragraph actually bakes, not the `Color4f` it came
                // from — two float colours that round to the same pixel share an entry.
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
        // Drop what the last two frames did not draw. Every entry still on screen is
        // re-stamped above on the frame it appears in, so this only reaps strings that left.
        if cache.len() > PARA_CACHE_MAX {
            cache.retain(|_, c| c.used + 1 >= frame);
        }
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
        let at = Point::new((cx - max_w / 2.0) as f32, y as f32);
        self.draw_paragraph(canvas, text, Para::Centered, w, size, color, max_w, at);
    }

    /// [`centered`](Self::centered)'s LEFT-ALIGNED twin: `x` is the text's left edge, `y` its
    /// top. Same paragraph path, so it shapes and falls back for CJK exactly as `centered`
    /// does — which is why the screen chrome cannot use `draw`/`draw_clipped` instead.
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

    /// A screen's heading: left-aligned at `x`, top edge at `y`, clamped to ONE ellipsized
    /// line at `max_w`.
    ///
    /// Every punktfunk client anchors its console heading to the leading edge — Apple's
    /// carries the note that a centred one "read as a floating label" rather than as a
    /// section heading, Android's `ConsoleHeader` pins it to `ConsoleEdgeInset`. The single
    /// line is not cosmetic either: left-aligned, a long host name would otherwise wrap under
    /// the controller chip and push a second line into the content.
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
        // The char goes onto the stack to be measured, not into a fresh `String` per character:
        // this runs for every over-long title on screen, every frame, and the allocation was
        // the bulk of it. `encode_utf8` writes the same bytes `to_string` would have.
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

/// Resolve the first available family. Generic aliases ("sans-serif", "monospace")
/// resolve through fontconfig on Linux; Windows' DirectWrite-backed FontMgr has no
/// generic aliases, so the list falls through to concrete family names there.
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

    /// The recede must never CLAMP. The other three assertions here are all relative and
    /// none of them feeds the matrix a dark input, which is exactly how a brightness term
    /// that subtracted 61/255 in the offset column shipped: the coverflow's own placeholder
    /// face came out at literal #000000 on every card a slot or more from the focus, so the
    /// side stack was one black slab with no depth and no cover-art detail in it. `apply`
    /// omits Skia's clamp deliberately, so a channel below zero here IS the shipped bug.
    #[test]
    fn a_dark_card_face_survives_a_full_recede() {
        // Every dark palette's coverless card, at the quieter of the two tints
        // `screens::library::draw_poster_placeholder` draws — the darkest face the shelf has.
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
