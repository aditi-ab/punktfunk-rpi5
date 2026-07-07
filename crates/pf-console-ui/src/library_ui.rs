//! The console library's Skia side: navigation state, scene selection, and rendering —
//! the GTK launcher (`ui_gamepad_library.rs`) re-homed onto the presenter surface. The
//! mesh-gradient background runs as an SkSL shader at full rate (the 30 Hz CPU path is
//! gone), the coverflow is `concat_44` perspective with paint order = draw order (the
//! restack hack is gone), and every state renders in-scene (gamescope maps no dialogs).

use crate::library::{
    card_matrix, initials, mesh_sksl, spring_advance, step_cursor, store_label,
    LibraryGame, LibraryPhase, LibraryShared, StepResult, BUMP_C, BUMP_K, BUMP_PX, FOCUS_GAP,
    JUMP, PERSPECTIVE, POSTER_H, POSTER_W, RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING,
    SPRING_C, SPRING_K, VISIBLE_RANGE,
};
use anyhow::{anyhow, Result};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use pf_presenter::overlay::OverlayAction;
use skia_safe::textlayout::{FontCollection, ParagraphBuilder, ParagraphStyle, TextAlign, TextStyle};
use skia_safe::{
    Canvas, Color4f, Data, Font, FontStyle, Image, Paint, Point, RRect, Rect, RuntimeEffect,
    Typeface, M44,
};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub(crate) struct LibraryUi {
    shared: LibraryShared,
    host_label: String,
    // Synced snapshot of the shared model (re-pulled when the generation bumps).
    generation: u64,
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    // Navigation: the integer cursor is the authority; the eased position chases it.
    cursor: i32,
    anim_pos: f64,
    anim_vel: f64,
    bump: f64,
    bump_vel: f64,
    last_frame: Option<Instant>,
    t0: Instant,
    /// Decoded posters by game id (decode once; Skia uploads lazily on first draw).
    art: HashMap<String, Image>,
    /// The animated mesh-gradient background (compiled once; drawn first each frame).
    mesh: RuntimeEffect,
    /// A launch is in flight — menu input parks, the hint bar says Connecting….
    connecting: bool,
    /// A session is on screen — the library doesn't render (stream chrome does).
    pub(crate) in_stream: bool,
    /// Transient error strip on the carousel (connect failures land here).
    status: Option<String>,
    actions: VecDeque<OverlayAction>,
}

impl LibraryUi {
    pub(crate) fn new(shared: LibraryShared, host_label: String) -> Result<LibraryUi> {
        let mesh = RuntimeEffect::make_for_shader(mesh_sksl(), None)
            .map_err(|e| anyhow!("mesh-gradient SkSL: {e}"))?;
        Ok(LibraryUi {
            shared,
            host_label,
            generation: u64::MAX, // force the first sync
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            cursor: 0,
            anim_pos: 0.0,
            anim_vel: 0.0,
            bump: 0.0,
            bump_vel: 0.0,
            last_frame: None,
            t0: Instant::now(),
            art: HashMap::new(),
            mesh,
            connecting: false,
            in_stream: false,
            status: None,
            actions: VecDeque::new(),
        })
    }

    /// Pull the shared model when it changed; decode any newly arrived poster bytes.
    pub(crate) fn sync(&mut self) {
        if self.shared.generation() != self.generation {
            let (phase, games, generation) = self.shared.snapshot();
            let fresh_games = self.games.len() != games.len()
                || self
                    .games
                    .iter()
                    .zip(&games)
                    .any(|(a, b)| a.id != b.id);
            self.phase = phase;
            self.games = games;
            self.generation = generation;
            if fresh_games {
                // Fresh library: snap the sprung position onto the (reset) cursor.
                self.cursor = 0;
                self.anim_pos = 0.0;
                self.anim_vel = 0.0;
                self.bump = 0.0;
                self.bump_vel = 0.0;
            }
            self.cursor = self.cursor.clamp(0, (self.games.len() as i32 - 1).max(0));
        }
        for (id, bytes) in self.shared.drain_art() {
            match Image::from_encoded(Data::new_copy(&bytes)) {
                Some(img) => {
                    self.art.insert(id, img);
                }
                None => tracing::debug!(%id, "undecodable poster"),
            }
        }
    }

    /// Menu-mode navigation (gamepad; the keyboard fallback funnels in here too).
    pub(crate) fn menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        if self.connecting {
            return None; // a connect is in flight — input is parked
        }
        match &self.phase {
            LibraryPhase::Ready => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                MenuEvent::JumpBack => self.step(-JUMP, true),
                MenuEvent::JumpForward => self.step(JUMP, true),
                MenuEvent::Confirm => {
                    let g = self.games.get(self.cursor as usize)?;
                    self.actions.push_back(OverlayAction::Launch {
                        id: g.id.clone(),
                        title: g.title.clone(),
                    });
                    self.status = None;
                    self.connecting = true;
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    self.actions.push_back(OverlayAction::Quit);
                    None
                }
                MenuEvent::Move(_) | MenuEvent::Secondary | MenuEvent::Tertiary => None,
            },
            LibraryPhase::Error { can_retry, .. } => match ev {
                MenuEvent::Confirm if *can_retry => {
                    self.phase = LibraryPhase::Loading; // local; the fetch re-syncs it
                    self.actions.push_back(OverlayAction::Retry);
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    self.actions.push_back(OverlayAction::Quit);
                    None
                }
                _ => None,
            },
            LibraryPhase::Loading | LibraryPhase::Empty | LibraryPhase::PairFirst => {
                if ev == MenuEvent::Back {
                    self.actions.push_back(OverlayAction::Quit);
                }
                None
            }
        }
    }

    /// Keyboard fallback (arrows/Enter/Esc/PageUp/PageDown) — the launcher is fully
    /// drivable with no pad. Returns true when consumed.
    pub(crate) fn key(&mut self, sc: sdl3::keyboard::Scancode, repeat: bool) -> bool {
        use sdl3::keyboard::Scancode as S;
        let ev = match sc {
            S::Left => MenuEvent::Move(MenuDir::Left),
            S::Right => MenuEvent::Move(MenuDir::Right),
            S::Up => MenuEvent::Move(MenuDir::Up),
            S::Down => MenuEvent::Move(MenuDir::Down),
            S::Return | S::KpEnter | S::Space if !repeat => MenuEvent::Confirm,
            S::Escape | S::Backspace if !repeat => MenuEvent::Back,
            S::PageUp if !repeat => MenuEvent::JumpBack,
            S::PageDown if !repeat => MenuEvent::JumpForward,
            _ => return false,
        };
        self.menu(ev); // no pad to pulse
        true
    }

    pub(crate) fn take_action(&mut self) -> Option<OverlayAction> {
        self.actions.pop_front()
    }

    pub(crate) fn set_connecting(&mut self, on: bool) {
        self.connecting = on;
        if on {
            self.status = None;
        }
    }

    /// A launch that didn't stick (connect failed / session ended with a reason):
    /// carousel-visible errors land on the transient strip, anything else becomes the
    /// error scene (no retry — the library itself is fine).
    pub(crate) fn session_error(&mut self, msg: &str) {
        self.connecting = false;
        self.in_stream = false;
        if self.phase == LibraryPhase::Ready {
            self.status = Some(format!("Couldn't connect — {msg}"));
        } else {
            self.phase = LibraryPhase::Error {
                title: "Couldn't connect".into(),
                body: msg.to_string(),
                can_retry: false,
            };
        }
    }

    fn step(&mut self, delta: i32, clamp: bool) -> Option<MenuPulse> {
        match step_cursor(self.cursor, self.games.len(), delta, clamp) {
            StepResult::Moved(to) => {
                self.cursor = to;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                // Recoil against the push; the stiff bump spring wobbles it back.
                self.bump = -BUMP_PX * f64::from(delta.signum());
                self.bump_vel = 0.0;
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// Render the whole library scene. Always a full repaint — the aurora animates
    /// every frame (that's the point of the GPU port).
    pub(crate) fn render(&mut self, canvas: &Canvas, w: u32, h: u32, fonts: &Fonts) {
        let (wf, hf) = (w as f64, h as f64);
        // Uniform scale off the Deck's 800p design height; fonts and geometry follow.
        let k = (hf / 800.0).clamp(0.75, 3.0);

        // Springs (Ready only — other scenes have no strip).
        let now = Instant::now();
        let dt = self
            .last_frame
            .replace(now)
            .map_or(1.0 / 60.0, |t| (now - t).as_secs_f64().clamp(0.0, 0.05));
        if self.phase == LibraryPhase::Ready {
            let target = f64::from(self.cursor);
            (self.anim_pos, self.anim_vel) = spring_advance(
                self.anim_pos,
                self.anim_vel,
                target,
                SPRING_K,
                SPRING_C,
                dt,
            );
            if (target - self.anim_pos).abs() < 0.001 && self.anim_vel.abs() < 0.01 {
                self.anim_pos = target;
                self.anim_vel = 0.0;
            }
            (self.bump, self.bump_vel) =
                spring_advance(self.bump, self.bump_vel, 0.0, BUMP_K, BUMP_C, dt);
            if self.bump.abs() < 0.3 && self.bump_vel.abs() < 4.0 {
                self.bump = 0.0;
                self.bump_vel = 0.0;
            }
        }

        self.draw_background(canvas, wf, hf);

        match self.phase.clone() {
            LibraryPhase::Ready => self.draw_carousel(canvas, wf, hf, k, fonts),
            LibraryPhase::Loading => {
                self.draw_spinner(canvas, wf / 2.0, hf / 2.0 - 24.0 * k, 16.0 * k);
                fonts.centered(canvas, "Loading library…", 14.0 * k, DIM_TEXT, wf / 2.0, hf / 2.0 + 16.0 * k, wf * 0.8);
            }
            LibraryPhase::PairFirst => {
                fonts.centered_bold(canvas, "Not paired with this host", 22.0 * k, WHITE, wf / 2.0, hf / 2.0 - 20.0 * k, wf * 0.8);
                fonts.centered(canvas, "Pair from the Punktfunk plugin first.", 14.0 * k, DIM_TEXT, wf / 2.0, hf / 2.0 + 12.0 * k, wf * 0.8);
            }
            LibraryPhase::Empty => {
                fonts.centered_bold(canvas, "No games found", 22.0 * k, WHITE, wf / 2.0, hf / 2.0 - 20.0 * k, wf * 0.8);
                fonts.centered(canvas, "Install Steam titles or add custom entries in the host's web console.", 14.0 * k, DIM_TEXT, wf / 2.0, hf / 2.0 + 12.0 * k, wf * 0.8);
            }
            LibraryPhase::Error { title, body, .. } => {
                fonts.centered_bold(canvas, &title, 22.0 * k, WHITE, wf / 2.0, hf / 2.0 - 32.0 * k, wf * 0.8);
                fonts.centered(canvas, &body, 14.0 * k, DIM_TEXT, wf / 2.0, hf / 2.0 + 4.0 * k, (600.0 * k).min(wf * 0.85));
            }
        }

        self.draw_chrome(canvas, wf, hf, k, fonts);
    }

    fn draw_background(&self, canvas: &Canvas, w: f64, h: f64) {
        let t = self.t0.elapsed().as_secs_f64();
        // Uniform layout: float2 u_res, float u_t (declared order, no padding needed).
        let uniforms: [f32; 3] = [w as f32, h as f32, t as f32];
        let data = Data::new_copy(bytemuck_bytes(&uniforms));
        match self.mesh.make_shader(data, &[], None) {
            Some(shader) => {
                let mut paint = Paint::default();
                paint.set_shader(shader);
                canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
            }
            None => {
                canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
            }
        }
    }

    fn draw_carousel(&mut self, canvas: &Canvas, w: f64, h: f64, k: f64, fonts: &Fonts) {
        let (card_w, card_h) = (POSTER_W * k, POSTER_H * k);
        // The strip's vertical center: the space between the top bar and the detail
        // block (the GTK vexpand'ed scroller, approximated).
        let cy = h * 0.44;
        let pos = self.anim_pos;
        let bump = self.bump * k;

        // Paint order = draw order: farthest from the (integer) cursor first, so the
        // dense side stacks overlap toward the focus. Stable → the equidistant
        // neighbors keep a deterministic order.
        let mut order: Vec<usize> = (0..self.games.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse((i as i32 - self.cursor).abs()));

        for i in order {
            let d = i as f64 - pos;
            let a = d.abs();
            if a > VISIBLE_RANGE {
                continue;
            }
            let prox = a.min(1.0);
            let scale = 1.0 - prox * RECEDE_SCALE;
            let angle = -d.clamp(-1.0, 1.0) * ROTATE_DEG;
            // Piecewise strip: a full FOCUS_GAP around the focus, then the dense side
            // stacks (the classic coverflow shelf).
            let offset = if a <= 1.0 {
                d * FOCUS_GAP * k
            } else {
                d.signum() * (FOCUS_GAP + (a - 1.0) * SIDE_SPACING) * k
            };
            let cx = w / 2.0 + offset + bump;
            let m = card_matrix(cx, cy, angle, scale, card_w, card_h, PERSPECTIVE * k);

            let game = &self.games[i];
            canvas.save();
            canvas.concat_44(&M44::row_major(&m));
            let rect = Rect::from_wh(card_w as f32, card_h as f32);
            let rr = RRect::new_rect_xy(rect, 16.0 * k as f32, 16.0 * k as f32);
            canvas.clip_rrect(rr, None, true);
            match self.art.get(&game.id) {
                Some(img) => {
                    // Cover-fit: center-crop the source to the card's 2:3.
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let card_aspect = rect.width() / rect.height();
                    let src = if iw / ih > card_aspect {
                        let sw = ih * card_aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / card_aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    canvas.draw_image_rect(img, Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)), rect, &Paint::default());
                }
                None => {
                    // Solid face, not glass: the side cards OVERLAP (GTK CSS note).
                    canvas.draw_rect(rect, &Paint::new(Color4f::new(0.118, 0.118, 0.145, 1.0), None));
                    let mono = initials(&game.title);
                    let font = fonts.sans_bold(38.0 * k);
                    let tw = font.measure_str(&mono, None).0;
                    canvas.draw_str(
                        &mono,
                        Point::new((card_w as f32 - tw) / 2.0, card_h as f32 / 2.0 + 13.0 * k as f32),
                        &font,
                        &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.45), None),
                    );
                }
            }
            // Store badge, top-left.
            {
                let label = store_label(&game.store);
                let font = fonts.sans_bold(11.0 * k);
                let tw = font.measure_str(label, None).0;
                let (px, py) = (8.0 * k as f32, 8.0 * k as f32);
                let (bw, bh) = (tw + 16.0 * k as f32, 20.0 * k as f32);
                canvas.draw_rrect(
                    RRect::new_rect_xy(Rect::from_xywh(px, py, bw, bh), bh / 2.0, bh / 2.0),
                    &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.55), None),
                );
                canvas.draw_str(
                    label,
                    Point::new(px + 8.0 * k as f32, py + 14.0 * k as f32),
                    &font,
                    &Paint::new(Color4f::new(1.0, 1.0, 1.0, 1.0), None),
                );
            }
            // The brightness recede: an opaque-black veil, never whole-card alpha.
            if prox > 0.0 {
                canvas.draw_rect(
                    rect,
                    &Paint::new(Color4f::new(0.0, 0.0, 0.0, (prox * RECEDE_DIM) as f32), None),
                );
            }
            canvas.restore();
        }

        // Detail block: focused title + store, centered between strip and hints.
        if let Some(g) = self.games.get(self.cursor as usize) {
            fonts.centered_bold(canvas, &g.title, 27.0 * k, WHITE, w / 2.0, h - 96.0 * k, w * 0.8);
            fonts.centered(
                canvas,
                &store_label(&g.store).to_uppercase(),
                12.0 * k,
                Color4f::new(1.0, 1.0, 1.0, 0.5),
                w / 2.0,
                h - 66.0 * k,
                w * 0.5,
            );
        }
        if let Some(status) = &self.status {
            fonts.centered(
                canvas,
                status,
                13.0 * k,
                Color4f::new(1.0, 0.576, 0.541, 1.0), // the GTK #ff938a
                w / 2.0,
                h - 44.0 * k,
                w * 0.85,
            );
        }
    }

    /// Top bar (host + controller chip) and the bottom hint bar — per-scene affordances.
    fn draw_chrome(&self, canvas: &Canvas, w: f64, h: f64, k: f64, fonts: &Fonts) {
        let font = fonts.sans_bold(18.0 * k);
        canvas.draw_str(
            &self.host_label,
            Point::new(24.0 * k as f32, 32.0 * k as f32),
            &font,
            &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.9), None),
        );
        if let Some(chip) = &fonts.chip_text {
            let cf = fonts.sans(12.0 * k);
            let tw = cf.measure_str(chip, None).0;
            let (bh, pad) = (24.0 * k as f32, 12.0 * k as f32);
            let bx = w as f32 - 24.0 * k as f32 - tw - 2.0 * pad;
            canvas.draw_rrect(
                RRect::new_rect_xy(
                    Rect::from_xywh(bx, 18.0 * k as f32, tw + 2.0 * pad, bh),
                    bh / 2.0,
                    bh / 2.0,
                ),
                &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.08), None),
            );
            canvas.draw_str(
                chip,
                Point::new(bx + pad, 18.0 * k as f32 + 16.0 * k as f32),
                &cf,
                &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.7), None),
            );
        }

        let hint = if self.connecting {
            "Connecting…".to_string()
        } else {
            match &self.phase {
                LibraryPhase::Ready => "A  Play    B  Quit    L1 / R1  Jump".to_string(),
                LibraryPhase::Error { can_retry: true, .. } => "A  Retry    B  Quit".to_string(),
                _ => "B  Quit".to_string(),
            }
        };
        fonts.left(
            canvas,
            &hint,
            13.0 * k,
            Color4f::new(1.0, 1.0, 1.0, 0.85),
            24.0 * k,
            h - 20.0 * k,
        );
    }

    /// The loading spinner: a rotating arc off the aurora clock.
    fn draw_spinner(&self, canvas: &Canvas, cx: f64, cy: f64, r: f64) {
        let t = self.t0.elapsed().as_secs_f64();
        let start = (t * 300.0) % 360.0;
        let mut paint = Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.85), None);
        paint.set_style(skia_safe::PaintStyle::Stroke);
        paint.set_stroke_width(3.0);
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
}

const WHITE: Color4f = Color4f::new(1.0, 1.0, 1.0, 1.0);
const DIM_TEXT: Color4f = Color4f::new(1.0, 1.0, 1.0, 0.7);

fn bytemuck_bytes(v: &[f32; 3]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), 12) }
}

/// The text toolkit shared by every scene: typefaces + a paragraph-based centered-text
/// helper (shaping + font fallback — poster titles can be CJK; `draw_str` can't).
pub(crate) struct Fonts {
    pub sans: Typeface,
    pub collection: FontCollection,
    /// The controller chip's current text (fed per frame by the overlay).
    pub chip_text: Option<String>,
}

impl Fonts {
    fn sans(&self, size: f64) -> Font {
        Font::new(self.sans.clone(), size as f32)
    }

    fn sans_bold(&self, size: f64) -> Font {
        let mut f = Font::new(self.sans.clone(), size as f32);
        f.set_embolden(true);
        f
    }

    fn paragraph(
        &self,
        text: &str,
        size: f64,
        color: Color4f,
        bold: bool,
        align: TextAlign,
        max_w: f64,
    ) -> skia_safe::textlayout::Paragraph {
        let mut style = ParagraphStyle::new();
        style.set_text_align(align);
        let mut ts = TextStyle::new();
        ts.set_font_size(size as f32);
        ts.set_color(color.to_color());
        ts.set_font_style(if bold {
            FontStyle::bold()
        } else {
            FontStyle::normal()
        });
        style.set_text_style(&ts);
        let mut b = ParagraphBuilder::new(&style, self.collection.clone());
        b.add_text(text);
        let mut p = b.build();
        p.layout(max_w as f32);
        p
    }

    /// Centered paragraph with `y` as its top edge.
    #[allow(clippy::too_many_arguments)]
    fn centered(
        &self,
        canvas: &Canvas,
        text: &str,
        size: f64,
        color: Color4f,
        cx: f64,
        y: f64,
        max_w: f64,
    ) {
        let p = self.paragraph(text, size, color, false, TextAlign::Center, max_w);
        p.paint(canvas, Point::new((cx - max_w / 2.0) as f32, y as f32));
    }

    #[allow(clippy::too_many_arguments)]
    fn centered_bold(
        &self,
        canvas: &Canvas,
        text: &str,
        size: f64,
        color: Color4f,
        cx: f64,
        y: f64,
        max_w: f64,
    ) {
        let p = self.paragraph(text, size, color, true, TextAlign::Center, max_w);
        p.paint(canvas, Point::new((cx - max_w / 2.0) as f32, y as f32));
    }

    fn left(&self, canvas: &Canvas, text: &str, size: f64, color: Color4f, x: f64, y: f64) {
        canvas.draw_str(
            text,
            Point::new(x as f32, y as f32),
            &self.sans(size),
            &Paint::new(color, None),
        );
    }
}

pub(crate) fn build_fonts() -> Result<Fonts> {
    let mgr = skia_safe::FontMgr::new();
    let sans = mgr
        .match_family_style("sans-serif", FontStyle::normal())
        .ok_or_else(|| anyhow!("no sans-serif typeface via fontconfig"))?;
    let mut collection = FontCollection::new();
    collection.set_default_font_manager(mgr, None);
    Ok(Fonts {
        sans,
        collection,
        chip_text: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated mesh-gradient SkSL must actually compile (Skia's SkSL frontend runs
    /// on the CPU — no GPU needed) — the shape test in `library` can't catch type errors.
    #[test]
    fn mesh_sksl_compiles() {
        RuntimeEffect::make_for_shader(mesh_sksl(), None)
            .unwrap_or_else(|e| panic!("mesh-gradient SkSL rejected:\n{e}"));
    }

    /// Render the background on a CPU raster surface at a few times and dump PNGs — a visual
    /// check of the mesh-gradient look (ignored; run with `--ignored` + PF_MESH_DUMP=<dir>).
    #[test]
    #[ignore]
    fn mesh_dump_png() {
        let dir = std::env::var("PF_MESH_DUMP").expect("set PF_MESH_DUMP to an output dir");
        let effect = RuntimeEffect::make_for_shader(mesh_sksl(), None).unwrap();
        let (w, h) = (1280i32, 800i32);
        for t in [0.0f32, 20.0, 60.0, 300.0] {
            let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
            let uniforms: [f32; 3] = [w as f32, h as f32, t];
            let data = Data::new_copy(bytemuck_bytes(&uniforms));
            let shader = effect.make_shader(data, &[], None).unwrap();
            let mut paint = Paint::default();
            paint.set_shader(shader);
            surface
                .canvas()
                .draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/mesh_t{t}.png"), png.as_bytes()).unwrap();
        }
    }
}
