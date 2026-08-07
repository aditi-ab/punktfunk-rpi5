//! The console shell's modal overlays (connecting / waking / toast / full-screen takeover).

use crate::anim::{approach, ease_out_cubic};
use crate::glyphs::{hint_bar, Hint, HintKey};
use crate::theme::{fg, Fonts, PanelStroke, W};
use skia_safe::{gradient_shader, Canvas, Paint, Point, Rect, TileMode};

use super::{Shell, BOTTOM_BAND};

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
                if c.canceling {
                    Some((
                        c.appear,
                        true,
                        "Canceling…".to_string(),
                        String::new(),
                        vec![],
                    ))
                } else if c.request_access {
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

        // The toast: a transient pill above the hint bar; slides in, fades out.
        if self.toast.as_ref().is_some_and(|toast| t - toast.at > 4.0) {
            self.toast = None;
        }
        if let Some(toast) = &self.toast {
            let age = t - toast.at;
            {
                let slide = ease_out_cubic((age / 0.25).min(1.0));
                let fade = if age > 3.4 {
                    (1.0 - (age - 3.4) / 0.6).max(0.0)
                } else {
                    1.0
                };
                let alpha = (slide * fade) as f32;
                let size = 13.0 * k;
                let tw = f64::from(fonts.measure(&toast.text, W::Medium, size));
                let (pad_x, bh) = (16.0 * k, 34.0 * k);
                let bw = tw + 2.0 * pad_x;
                let bx = (w - bw) / 2.0;
                let by = h - BOTTOM_BAND * k - bh - 8.0 * k + (1.0 - slide) * 12.0 * k;
                canvas.save_layer_alpha_f(None, alpha);
                let rect = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32);
                canvas.draw_rrect(
                    skia_safe::RRect::new_rect_xy(rect, (bh / 2.0) as f32, (bh / 2.0) as f32),
                    &Paint::new(crate::theme::shade(0.6), None),
                );
                crate::theme::panel(
                    canvas,
                    rect,
                    (bh / 2.0 / k) as f32,
                    None,
                    PanelStroke::Plain(0.14),
                    k as f32,
                );
                fonts.draw(
                    canvas,
                    &toast.text,
                    bx + pad_x,
                    by + bh / 2.0 + size * 0.36,
                    W::Medium,
                    size,
                    fg(0.92),
                );
                canvas.restore();
            }
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
        let mut vignette = Paint::default();
        vignette.set_shader(gradient_shader::radial(
            Point::new(cx as f32, (h / 2.0) as f32),
            (w.max(h) * 0.42) as f32,
            gradient_shader::GradientShaderColors::Colors(&[
                crate::theme::shade(0.5).to_color(),
                crate::theme::shade(0.0).to_color(),
            ]),
            None,
            TileMode::Clamp,
            None,
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
                cx - probe.0 / 2.0,
                h - 34.0 * k,
                k,
            );
        }
        canvas.restore();
    }
}
