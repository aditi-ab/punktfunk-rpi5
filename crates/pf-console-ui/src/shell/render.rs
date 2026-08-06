//! The console shell's per-frame screen compose/transition render path.

use crate::anim::{approach, ease_out_cubic};
use crate::glyphs::{hint_bar, GlyphStyle};
use crate::library::LibraryShared;
use crate::model::HostRow;
use crate::screens::{Bg, Ctx, Screen};
use crate::theme::{fg, Fonts, PanelStroke, W};
use pf_client_core::gamepad::PadInfo;
use pf_client_core::trust;
use skia_safe::{Canvas, Rect};
use std::time::Instant;

use super::{Motion, Shell, BOTTOM_BAND, TOP_BAND};

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
        self.pads = pads.to_vec();
        self.glyphs = GlyphStyle::from_pref(pad_pref);
        self.chip = Some(pad.map_or_else(
            || "No controller — keyboard works too".to_string(),
            str::to_owned,
        ));

        let (w, h) = (f64::from(width), f64::from(height));
        let k = (h / 800.0).clamp(0.75, 3.0);
        let t = self.t();

        // Advance the transition; a finished pop finally drops its leaving screen.
        let motion_p = match &mut self.motion {
            Motion::None => None,
            Motion::Push(p) => {
                p.advance(dt);
                let v = p.value();
                if p.done() {
                    self.motion = Motion::None;
                    None
                } else {
                    Some(v)
                }
            }
            Motion::Pop { t, .. } => {
                t.advance(dt);
                let v = t.value();
                if t.done() {
                    self.motion = Motion::None;
                    None
                } else {
                    Some(v)
                }
            }
        };

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
        // One paint recipe per layer: (alpha, slide, scale). Everything below borrows
        // disjoint fields of `self` per call, so the borrow checker stays happy.
        let mut env = LayerEnv {
            canvas,
            w,
            h,
            content,
            k,
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
        match (&mut self.motion, motion_p) {
            (Motion::Push(_), Some(raw)) => {
                let p = ease_out_cubic(raw);
                let n = self.stack.len();
                // Outgoing recedes underneath…
                if n >= 2 {
                    let (below, top) = self.stack.split_at_mut(n - 1);
                    env.paint(&mut below[n - 2], 1.0 - p, 0.0, 1.0 - 0.04 * p);
                    // …while the incoming slides up out of a fade.
                    env.paint(&mut top[0], p, 36.0 * k * (1.0 - p), 0.985 + 0.015 * p);
                } else {
                    env.paint(
                        &mut self.stack[0],
                        p,
                        36.0 * k * (1.0 - p),
                        0.985 + 0.015 * p,
                    );
                }
            }
            (Motion::Pop { leaving, .. }, Some(raw)) => {
                let p = ease_out_cubic(raw);
                // The revealed screen grows back in…
                let n = self.stack.len();
                env.paint(&mut self.stack[n - 1], 0.4 + 0.6 * p, 0.0, 0.96 + 0.04 * p);
                // …while the leaving one slides down into a fade.
                env.paint(leaving.as_mut(), 1.0 - p, 36.0 * k * p, 1.0);
            }
            _ => {
                let n = self.stack.len();
                env.paint(&mut self.stack[n - 1], 1.0, 0.0, 1.0);
            }
        }

        // Persistent chrome: the controller chip (top-right, above every layer).
        if let Some(chip) = &self.chip {
            let size = 12.0 * k;
            let tw = f64::from(fonts.measure(chip, W::Medium, size));
            let (bh, pad_x) = (24.0 * k, 12.0 * k);
            let bx = w - 24.0 * k - tw - 2.0 * pad_x;
            let rect = Rect::from_xywh(
                bx as f32,
                (18.0 * k) as f32,
                (tw + 2.0 * pad_x) as f32,
                bh as f32,
            );
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.12),
                k as f32,
            );
            fonts.draw(
                canvas,
                chip,
                bx + pad_x,
                18.0 * k + 16.0 * k,
                W::Medium,
                size,
                fg(0.7),
            );
        }

        self.draw_overlays(canvas, w, h, k, dt, t, fonts);
    }
}

/// Everything one screen layer needs to paint — bundled so the transition arms stay
/// readable and each `paint` call borrows `Shell` fields disjointly.
struct LayerEnv<'a> {
    canvas: &'a Canvas,
    w: f64,
    h: f64,
    content: Rect,
    k: f64,
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
    /// so chrome travels with content through a transition.
    fn paint(&mut self, screen: &mut Screen, alpha: f64, dy: f64, scale: f64) {
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
        self.fonts.centered(
            canvas,
            &screen.title(&ctx),
            W::Bold,
            30.0 * self.k,
            fg(1.0),
            self.w / 2.0,
            18.0 * self.k,
            self.w * 0.7,
        );
        screen.render(canvas, self.content, self.k, self.dt, self.fonts, &mut ctx);
        if self.show_hints {
            let hints = screen.hints(&ctx);
            hint_bar(
                canvas,
                self.fonts,
                &hints,
                self.glyphs,
                18.0 * self.k,
                self.h - 18.0 * self.k,
                self.k,
            );
        }
        canvas.restore();
    }
}
