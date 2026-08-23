//! The console shell's modal overlays (connecting / waking / toast / full-screen takeover).

use crate::anim::{approach, springs};
use crate::glyphs::{hint_bar, Hint, HintKey};
use crate::theme::{fg, fill, Fonts, PanelStroke, W};
use skia_safe::{gradient, Canvas, Color4f, PathBuilder, Point, Rect, TileMode};

use super::{Shell, ToastMark, BOTTOM_BAND};

/// The toast's leading mark, centred on `(cx, cy)` in a ~13 dp box.
fn draw_toast_mark(canvas: &Canvas, mark: ToastMark, cx: f64, cy: f64, k: f64, ink: Color4f) {
    let mut p = fill(ink);
    match mark {
        ToastMark::Dot => {
            canvas.draw_circle((cx as f32, cy as f32), (3.4 * k) as f32, &p);
        }
        ToastMark::Check => {
            p.set_style(skia_safe::PaintStyle::Stroke);
            p.set_stroke_width((1.9 * k) as f32);
            p.set_stroke_cap(skia_safe::PaintCap::Round);
            p.set_stroke_join(skia_safe::PaintJoin::Round);
            let r = 5.0 * k;
            let mut path = PathBuilder::new();
            path.move_to(((cx - r) as f32, cy as f32));
            path.line_to(((cx - r * 0.25) as f32, (cy + r * 0.7) as f32));
            path.line_to(((cx + r) as f32, (cy - r * 0.7) as f32));
            canvas.draw_path(&path.detach(), &p);
        }
        ToastMark::Bang => {
            // A stem and a dot rather than a glyph: at 0.75× k an "!" from the text font
            // would be two pixels of stem and disappear.
            let r = 5.4 * k;
            let w = 2.0 * k;
            canvas.draw_rrect(
                skia_safe::RRect::new_rect_xy(
                    Rect::from_xywh(
                        (cx - w / 2.0) as f32,
                        (cy - r) as f32,
                        w as f32,
                        (r * 1.35) as f32,
                    ),
                    (w / 2.0) as f32,
                    (w / 2.0) as f32,
                ),
                &p,
            );
            canvas.draw_circle((cx as f32, (cy + r * 0.72) as f32), (w * 0.62) as f32, &p);
        }
    }
}

impl Shell {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::shell) fn draw_overlays(
        &mut self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        dt: f64,
        t: f64,
        fonts: &Fonts,
    ) {
        // Resolve the connect/wake takeover — the two phases of reaching a host — into one
        // full-screen shape (spinner, title, one detail line, its own hints). Connecting flows
        // straight out of a wake (see `sync`) so they share the same backdrop and never blink
        // between them. Mirrors the Android client's unified `ConnectOverlay`.
        let takeover: Option<(f64, bool, String, String, Vec<Hint>)> =
            if let Some(c) = &mut self.connecting {
                c.appear = approach(c.appear, 1.0, dt, 0.07);
                if c.request_access {
                    Some((
                        c.appear,
                        true,
                        "Waiting for approval…".to_string(),
                        format!(
                            "Approve this device in {}'s console or web UI — no PIN needed.",
                            c.title
                        ),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                } else {
                    Some((
                        c.appear,
                        true,
                        format!("Connecting to {}…", c.title),
                        "Starting the stream in this window.".to_string(),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                }
            } else if let Some(wk) = &self.wake {
                // Service-driven, so it appears settled (no fade-in).
                if wk.timed_out {
                    Some((
                        1.0,
                        false,
                        format!("{} didn't wake", wk.name),
                        "Check its power settings, or wake it manually and try again.".to_string(),
                        vec![
                            Hint::new(HintKey::Confirm, "Try Again"),
                            Hint::new(HintKey::Back, "Cancel"),
                        ],
                    ))
                } else {
                    Some((
                        1.0,
                        true,
                        format!("Waking {}…", wk.name),
                        format!("Waiting for it to come online · {} s", wk.seconds),
                        // A wake-only wait (no dial after) offers "Stop Waiting"; a wake-&-connect
                        // is a plain "Cancel".
                        vec![Hint::new(
                            HintKey::Back,
                            if wk.then_connect {
                                "Cancel"
                            } else {
                                "Stop Waiting"
                            },
                        )],
                    ))
                }
            } else {
                None
            };
        if let Some((appear, spinner, title, body, hints)) = takeover {
            self.draw_takeover(
                canvas, w, h, k, appear, t, fonts, spinner, &title, &body, &hints,
            );
        }

        // The toast: a transient pill above the hint bar; springs in, fades out.
        if self.toast.as_ref().is_some_and(|toast| t - toast.at > 4.0) {
            self.toast = None;
        }
        if let Some(toast) = &mut self.toast {
            let age = t - toast.at;
            if crate::theme::reduce_motion() {
                toast.seat = crate::anim::Spring::rest(1.0);
            } else {
                toast.seat.step_spec(1.0, springs::INDICATOR, dt);
                toast.seat.settle(1.0, 0.001, 0.01);
            }
            // The seat drives the SLIDE; the fade is deliberately still linear-in-time,
            // because a dismissal is a deadline (4 s) and not a gesture.
            let slide = toast.seat.pos.clamp(0.0, 1.0);
            let fade = if age > 3.4 {
                (1.0 - (age - 3.4) / 0.6).max(0.0)
            } else {
                1.0
            };
            let alpha = (slide * fade) as f32;
            let (tint, mark) = toast.kind.look();
            let size = 13.0 * k;
            let tw = f64::from(fonts.measure(&toast.text, W::Medium, size));
            let (pad_x, bh) = (16.0 * k, 34.0 * k);
            // Leading run: the kind mark, air, then the text. The mark carries the kind by
            // itself — a hairline in the same tint used to stand beside it, saying the same
            // thing twice and reading as a rendering seam rather than as meaning. Its pad is
            // shy of `pad_x` because nothing drawn in the 13 dp box fills it, and an equal pad
            // leaves the pill visibly left-heavy in air.
            let (mark_pad, mark_w, gap) = (13.0 * k, 13.0 * k, 9.0 * k);
            let lead = mark_pad + mark_w + gap;
            let bw = lead + tw + pad_x;
            let bx = (w - bw) / 2.0;
            let by = h - BOTTOM_BAND * k - bh - 8.0 * k + (1.0 - slide) * 12.0 * k;
            let rect = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32);
            // BOUNDED to the pill. Unbounded, `save_layer` allocates an offscreen the size of
            // the whole SURFACE and composites it back — on a 4K TV that is a 33 MB render
            // target raised and torn down every frame, for four seconds, to fade a 34 dp pill
            // (and on a box whose whole Skia budget is 64 MB, it evicts real work to do it).
            //
            // Everything drawn inside is inside `rect`: the pill fill, `theme::panel`'s
            // hairline ON that rect, the kind mark centred in it, and text that ends a `pad_x`
            // short of its right edge. There is no blur to reach further, so the outset is
            // slack for the stroke rather than a computed reach — `screens::home` needs 36 k
            // for the same layer only because it wraps a σ = 10 k halo.
            let bounds = rect.with_outset((12.0 * k as f32, 12.0 * k as f32));
            canvas.save_layer_alpha_f(Some(bounds), alpha);
            canvas.draw_rrect(
                skia_safe::RRect::new_rect_xy(rect, (bh / 2.0) as f32, (bh / 2.0) as f32),
                &fill(crate::theme::shade(0.6)),
            );
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.14),
                k as f32,
            );
            let cy = by + bh / 2.0;
            // Tinted by kind, so a toast is readable from a couch without reading the words.
            draw_toast_mark(canvas, mark, bx + mark_pad + mark_w / 2.0, cy, k, tint);
            fonts.draw(
                canvas,
                &toast.text,
                bx + lead,
                cy + size * 0.36,
                W::Medium,
                size,
                fg(0.92),
            );
            canvas.restore();
        }
    }
    /// A full-screen connect/wake takeover: a fresh aurora over everything (so the carousel and
    /// chrome fall away), a centered spinner (or none, when a wake has timed out), a title, one
    /// detail line, and its own bottom hint row. `appear` fades the whole thing in over the home;
    /// a wake that hands off to a connect passes 1.0 so the two never blink between them. The
    /// console counterpart of the Android/Apple `ConnectOverlay` — one full-screen shape, not a
    /// centered modal card.
    #[allow(clippy::too_many_arguments)]
    fn draw_takeover(
        &self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        appear: f64,
        t: f64,
        fonts: &Fonts,
        spinner: bool,
        title: &str,
        body: &str,
        hints: &[Hint],
    ) {
        let cx = w / 2.0;
        canvas.save_layer_alpha_f(None, appear as f32);
        // Opaque aurora — the same living backdrop the home wears, so the takeover reads as the
        // console taking over rather than a card popping up.
        self.draw_aurora(canvas, w, h, t, 0.0);
        // A soft pool of shade under the centre seats the text against a bright field —
        // dark on a dark palette, light on a pale one, so it always separates.
        let mut vignette = crate::theme::shaded();
        let shades = [crate::theme::shade(0.5), crate::theme::shade(0.0)];
        vignette.set_shader(gradient::shaders::radial_gradient(
            (
                Point::new(cx as f32, (h / 2.0) as f32),
                (w.max(h) * 0.42) as f32,
            ),
            &gradient::Gradient::new(
                gradient::Colors::new_evenly_spaced(&shades, TileMode::Clamp, None),
                gradient::Interpolation::default(),
            ),
            None,
        ));
        canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &vignette);

        // Centre the spinner + title + detail as a group around the middle of the screen.
        let title_y = h / 2.0 + if spinner { 14.0 * k } else { 0.0 };
        if spinner {
            crate::theme::spinner(canvas, cx, title_y - 52.0 * k, 22.0 * k, t);
        }
        fonts.centered(
            canvas,
            title,
            W::SemiBold,
            23.0 * k,
            fg(1.0),
            cx,
            title_y,
            w * 0.82,
        );
        if !body.is_empty() {
            fonts.centered(
                canvas,
                body,
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                title_y + 32.0 * k,
                w * 0.66,
            );
        }
        if !hints.is_empty() {
            // Centered near the bottom, where every console screen's legend sits.
            let probe = hint_bar(canvas, fonts, hints, self.glyphs, -10_000.0, -10_000.0, k);
            hint_bar(
                canvas,
                fonts,
                hints,
                self.glyphs,
                cx - probe.size.0 / 2.0,
                h - 34.0 * k,
                k,
            );
        }
        canvas.restore();
    }
}
