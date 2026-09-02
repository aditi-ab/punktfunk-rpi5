//! Modal overlays: connecting, waking, toast, full-screen takeover.

use crate::anim::{approach, springs};
use crate::glyphs::{hint_bar, Hint, HintKey};
use crate::theme::{fg, fill, Fonts, PanelStroke, W};
use skia_safe::{gradient, Canvas, Color4f, PathBuilder, Point, Rect, TileMode};

use super::{Shell, ToastMark, BOTTOM_BAND};

/// Kind mark in a 13 dp box.
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
            // Stem + dot, not a text "!": at 0.75× k a font bang is two pixels and vanishes.
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
        // Connect and wake share one full-screen shape. A connect can follow a wake
        // (`sync`) so they share the backdrop and never blink between them.
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
                // Service-driven: already settled, no fade-in.
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
                        // Wake-only offers "Stop Waiting"; wake-then-connect is "Cancel".
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
            // Seat springs the slide. Fade stays linear: dismissal is a 4 s deadline, not a gesture.
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
            // Kind mark, then text. Pad is 13 dp (shy of `pad_x`) because the 13 dp mark
            // never fills its box; equal pad leaves the pill left-heavy.
            let (mark_pad, mark_w, gap) = (13.0 * k, 13.0 * k, 9.0 * k);
            let lead = mark_pad + mark_w + gap;
            let bw = lead + tw + pad_x;
            let bx = (w - bw) / 2.0;
            let by = h - BOTTOM_BAND * k - bh - 8.0 * k + (1.0 - slide) * 12.0 * k;
            let rect = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32);
            // Bound the fade layer to the pill. Unbounded `save_layer` is a full-surface
            // offscreen every frame. 12 k outset is stroke slack (no blur to reach further).
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
    /// Full-screen connect/wake takeover: aurora, optional spinner, title, one
    /// detail line, own hint row. `appear` = 1.0 when a wake hands off to a connect
    /// so the two never blink. Not a centered card.
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
        // Opaque aurora — the home field, so this reads as the console taking over.
        self.draw_aurora(canvas, w, h, t, 0.0);
        // Shade pool under the centre so title/body separate from a bright aurora.
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
