//! The console shell's per-frame screen compose/transition render path.

use crate::anim::approach;
use crate::glyphs::{hint_bar, GlyphStyle};
use crate::library::LibraryShared;
use crate::model::HostRow;
use crate::screens::{Bg, Ctx, Screen};
use crate::theme::{fg, Fonts, PanelStroke, EDGE_INSET, W};
use pf_client_core::gamepad::PadInfo;
use pf_client_core::trust;
use skia_safe::{Canvas, Rect};
use std::time::Instant;

use super::{
    Motion, NavKind, Shell, BOTTOM_BAND, NAV_ENTER_SCALE, NAV_EXIT_SCALE, NAV_REVEAL_ALPHA,
    NAV_SLIDE_DP, TOP_BAND,
};

impl Shell {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        fonts: &Fonts,
        pad: Option<&str>,
        pad_pref: Option<punktfunk_core::config::GamepadPref>,
        pads: &[PadInfo],
    ) {
        let now = Instant::now();
        let dt = self
            .last_frame
            .replace(now)
            .map_or(1.0 / 60.0, |t| (now - t).as_secs_f64().clamp(0.0, 0.05));
        self.sync();
        // Publish the palette's ink before ANYTHING draws — every widget, glyph and panel in
        // the crate reads it (see `theme::set_ink`), so a frame that skipped this would paint
        // the previous palette's text over the new palette's field.
        crate::theme::set_ink(self.ink);
        // Same contract as the ink: published once, before anything draws, so every widget
        // that has a choice to make about travel this frame reads one answer. Also kept as
        // a local — `LayerEnv` borrows `self.settings` mutably below, so the transition
        // arms can no longer reach the field itself.
        let reduce = self.settings.reduce_motion;
        crate::theme::set_reduce_motion(reduce);
        self.pads = pads.to_vec();
        self.glyphs = GlyphStyle::from_pref(pad_pref);
        self.chip = Some(pad.map_or_else(
            || "No controller — keyboard works too".to_string(),
            str::to_owned,
        ));

        let (w, h) = (f64::from(width), f64::from(height));
        let k = (h / 800.0).clamp(0.75, 3.0);
        let t = self.t();

        // Advance the transition. `None` means "settled" — which is also what makes the
        // hint-rect invariant below still hold: with a spring, "settled" is
        // `Motion::None`, exactly as it was with a timer. A reversed push takes its screen
        // back off the stack here; a completed pop drops the one it was carrying.
        let motion_p = self.advance_nav(dt);

        // The backdrop settles into (or out of) calm with the screen transition. It is the
        // SAME living field either way — a form screen quiets it, it doesn't replace it —
        // so this is one shader pass with a chased uniform, not two stacked backdrops.
        let bg_target = match self.stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
        self.bg_mix = approach(self.bg_mix, bg_target, dt, 0.12);
        if (self.bg_mix - bg_target).abs() < 0.005 {
            self.bg_mix = bg_target;
        }
        self.draw_aurora(canvas, w, h, t, self.bg_mix);

        // The screens, through the transition choreography.
        let content = Rect::from_ltrb(
            0.0,
            (TOP_BAND * k) as f32,
            w as f32,
            (h - BOTTOM_BAND * k) as f32,
        );
        // How much room the heading has before it reaches the controller chip. The chip is
        // painted last so it sits above every layer, but its geometry is known now — `chip`
        // and `pads` are both set above — and the heading needs it: centred, the title had
        // the whole width to spread symmetrically into, where left-aligned it runs AT the
        // chip. The 12 is the gap Apple keeps between the two; the floor keeps a
        // pathologically long chip string from squeezing the title to nothing.
        let title_max_w = {
            let chip_w = self.chip.as_ref().map_or(0.0, |c| {
                chip_width(
                    fonts,
                    c,
                    self.pads.first().is_some_and(|p| p.battery.is_some()),
                    k,
                )
            });
            (w - 2.0 * EDGE_INSET * k - chip_w - 12.0 * k).max(w * 0.35)
        };
        // One paint recipe per layer: (alpha, slide, scale). Everything below borrows
        // disjoint fields of `self` per call, so the borrow checker stays happy.
        let mut env = LayerEnv {
            canvas,
            w,
            h,
            content,
            k,
            title_max_w,
            dt,
            fonts,
            hosts: &self.hosts,
            library: &self.library,
            settings: &mut self.settings,
            pads: &self.pads,
            deck: self.deck,
            device_name: &self.device_name,
            t,
            glyphs: self.glyphs,
            // A modal card owns B/A while it's up — the screen's legend would lie.
            show_hints: self.connecting.is_none() && self.wake.is_none(),
        };
        // Only a SETTLED top screen publishes clickable hint boxes. Mid-transition every
        // layer is slid and scaled inside a `save_layer`, so the rects a `paint` reports
        // aren't where the pixels are — and the shell drops pointer input during a
        // transition anyway, exactly as it drops menu events.
        self.hint_rects.clear();
        // Reduced motion keeps the CROSSFADE — the stack has to stay legible, and an
        // instant swap loses the only spatial cue a console shell has — and drops the
        // travel: no slide, no scale.
        let slide = |dy: f64| if reduce { 0.0 } else { dy };
        let zoom = |s: f64| if reduce { 1.0 } else { s };
        // The geometry below is UNCHANGED from the tween: same 36 dp slide, same
        // 0.985/0.96 scales, same 0.4 reveal alpha. Only the time-course differs — `p` is
        // now the spring's position where it used to be `ease_out_cubic(elapsed)`.
        match (&mut self.motion, motion_p) {
            (
                Motion::Nav {
                    kind: NavKind::Push,
                    leaving,
                    ..
                },
                Some(p),
            ) => {
                let n = self.stack.len();
                let enter_scale = zoom(NAV_ENTER_SCALE + (1.0 - NAV_ENTER_SCALE) * p);
                let enter_slide = slide(NAV_SLIDE_DP * k * (1.0 - p));
                let recede = zoom(1.0 - (1.0 - NAV_EXIT_SCALE) * p);
                // Outgoing recedes underneath…
                if let Some(replaced) = leaving.as_mut() {
                    // A REPLACE carries the screen it swapped out, because that screen is no
                    // longer on the stack to be found under the incoming one. Painting the
                    // stack's own n-2 here would recede the replaced screen's PARENT, which
                    // is how choosing "Edit…" in a host menu used to flash the host list.
                    env.paint(replaced.as_mut(), 1.0 - p, 0.0, recede);
                    env.paint(&mut self.stack[n - 1], p, enter_slide, enter_scale);
                } else if n >= 2 {
                    let (below, top) = self.stack.split_at_mut(n - 1);
                    env.paint(&mut below[n - 2], 1.0 - p, 0.0, recede);
                    // …while the incoming slides up out of a fade.
                    env.paint(&mut top[0], p, enter_slide, enter_scale);
                } else {
                    env.paint(&mut self.stack[0], p, enter_slide, enter_scale);
                }
            }
            (
                Motion::Nav {
                    kind: NavKind::Pop,
                    leaving: Some(leaving),
                    ..
                },
                Some(p),
            ) => {
                // The revealed screen grows back in…
                let n = self.stack.len();
                env.paint(
                    &mut self.stack[n - 1],
                    NAV_REVEAL_ALPHA + (1.0 - NAV_REVEAL_ALPHA) * p,
                    0.0,
                    zoom(NAV_EXIT_SCALE + (1.0 - NAV_EXIT_SCALE) * p),
                );
                // …while the leaving one slides down into a fade.
                env.paint(leaving.as_mut(), 1.0 - p, slide(NAV_SLIDE_DP * k * p), 1.0);
            }
            _ => {
                let n = self.stack.len();
                self.hint_rects = env.paint(&mut self.stack[n - 1], 1.0, 0.0, 1.0);
            }
        }

        // Persistent chrome: the controller chip (top-right, above every layer). Reads
        // left-to-right as kind · name · charge — a mark for what is connected, its name,
        // and how long it has left.
        if let Some(chip) = &self.chip {
            let size = 12.0 * k;
            let tw = f64::from(fonts.measure(chip, W::Medium, size));
            let (bh, pad_x, gap) = (24.0 * k, 12.0 * k, 8.0 * k);
            let mark_w = 15.0 * k;
            let battery = self.pads.first().and_then(|p| p.battery);
            let bw = chip_width(fonts, chip, battery.is_some(), k);
            let bx = w - EDGE_INSET * k - bw;
            let top = 18.0 * k;
            let rect = Rect::from_xywh(bx as f32, top as f32, bw as f32, bh as f32);
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.12),
                k as f32,
            );
            let cy = top + bh / 2.0;
            crate::glyphs::pad_mark(canvas, self.glyphs, bx + pad_x, cy, mark_w, k, fg(0.7));
            fonts.draw(
                canvas,
                chip,
                bx + pad_x + mark_w + gap,
                cy + size * 0.36,
                W::Medium,
                size,
                fg(0.7),
            );
            if let Some(b) = battery {
                crate::glyphs::battery_pip(
                    canvas,
                    bx + pad_x + mark_w + gap + tw + gap,
                    cy,
                    22.0 * k,
                    k,
                    b,
                );
            }
        }

        self.draw_overlays(canvas, w, h, k, dt, t, fonts);
    }
}

/// The controller chip's drawn width, device px. Its own function because two things need
/// it — the chip itself, and the heading, which is left-aligned now and so has to stop short
/// of it. A second copy of this arithmetic is a title that slides under the chip the day
/// someone adds a field to it.
///
/// The battery only takes room when there IS one: a wired pad, a Steam virtual pad and "no
/// controller" all report nothing, and the chip must not carry a gap where their charge
/// would have been.
fn chip_width(fonts: &Fonts, chip: &str, has_battery: bool, k: f64) -> f64 {
    let tw = f64::from(fonts.measure(chip, W::Medium, 12.0 * k));
    let (pad_x, gap, mark_w) = (12.0 * k, 8.0 * k, 15.0 * k);
    let pip_w = if has_battery { 22.0 * k + gap } else { 0.0 };
    pad_x + mark_w + gap + tw + pip_w + pad_x
}

/// Everything one screen layer needs to paint — bundled so the transition arms stay
/// readable and each `paint` call borrows `Shell` fields disjointly.
struct LayerEnv<'a> {
    canvas: &'a Canvas,
    w: f64,
    h: f64,
    content: Rect,
    k: f64,
    /// The heading's width budget — everything left of the controller chip. See `Shell::render`.
    title_max_w: f64,
    dt: f64,
    fonts: &'a Fonts,
    hosts: &'a [HostRow],
    library: &'a LibraryShared,
    settings: &'a mut trust::Settings,
    pads: &'a [PadInfo],
    deck: bool,
    device_name: &'a str,
    t: f64,
    glyphs: GlyphStyle,
    show_hints: bool,
}

impl LayerEnv<'_> {
    /// One screen composited as a unit: `alpha` fade, `dy` vertical slide, `scale`
    /// about the screen center — its pinned title and hint bar ride inside the layer,
    /// so chrome travels with content through a transition. Returns the hint bar's hit
    /// boxes, which only the caller can know are worth keeping (see `Shell::render`).
    fn paint(
        &mut self,
        screen: &mut Screen,
        alpha: f64,
        dy: f64,
        scale: f64,
    ) -> Vec<(crate::glyphs::HintKey, Rect)> {
        let canvas = self.canvas;
        canvas.save_layer_alpha_f(None, alpha.clamp(0.0, 1.0) as f32);
        canvas.translate((0.0, dy as f32));
        let (cx, cy) = ((self.w / 2.0) as f32, (self.h / 2.0) as f32);
        canvas.translate((cx, cy));
        canvas.scale((scale as f32, scale as f32));
        canvas.translate((-cx, -cy));

        let mut ctx = Ctx {
            hosts: self.hosts,
            library: self.library,
            settings: self.settings,
            pads: self.pads,
            deck: self.deck,
            device_name: self.device_name,
            t: self.t,
        };
        self.fonts.heading(
            canvas,
            &screen.title(&ctx),
            W::Bold,
            30.0 * self.k,
            fg(1.0),
            EDGE_INSET * self.k,
            18.0 * self.k,
            self.title_max_w,
        );
        screen.render(canvas, self.content, self.k, self.dt, self.fonts, &mut ctx);
        let rects = if self.show_hints {
            let hints = screen.hints(&ctx);
            hint_bar(
                canvas,
                self.fonts,
                &hints,
                self.glyphs,
                18.0 * self.k,
                self.h - 18.0 * self.k,
                self.k,
            )
            .rects
        } else {
            Vec::new()
        };
        canvas.restore();
        rects
    }
}
