//! The game-library screen: the coverflow carousel (spring-chased cursor, perspective
//! card tilt, poster art streaming in) — the original console library, now one screen
//! on the shell's stack. B pops back to the host list; A launches the focused title in
//! the same window. The shell owns the aurora, chrome, and the connecting overlay.

use crate::anim::Spring;
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, initials, step_cursor, store_label, LibraryGame, LibraryPhase, LibraryShared,
    StepResult, BUMP_C, BUMP_K, BUMP_PX, FOCUS_GAP, JUMP, PERSPECTIVE, POSTER_H, POSTER_W,
    RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C, SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::screens::{ConnectIntent, Ctx, Outbox};
use crate::theme::{white, Fonts, DIM, W, WHITE};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Paint, Point, RRect, Rect, M44};
use std::collections::HashMap;

pub(crate) struct LibraryScreen {
    host_name: String,
    addr: String,
    port: u16,
    fp_hex: String,
    mgmt: u16,
    shared: Option<LibraryShared>,
    // Synced snapshot of the shared model (re-pulled when the generation bumps).
    generation: u64,
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    // Navigation: the integer cursor is the authority; the eased position chases it.
    cursor: i32,
    anim: Spring,
    bump: Spring,
    /// Decoded posters by game id (decode once; Skia uploads lazily on first draw).
    art: HashMap<String, Image>,
}

impl LibraryScreen {
    pub(crate) fn new(host: &HostRow) -> LibraryScreen {
        LibraryScreen {
            host_name: host.name.clone(),
            addr: host.addr.clone(),
            port: host.port,
            fp_hex: host.fp_hex.clone(),
            mgmt: host.mgmt_port,
            shared: None, // adopted from Ctx on the first render (the shell owns it)
            generation: u64::MAX,
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            art: HashMap::new(),
        }
    }

    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    fn fetch_cmd(&self) -> ConsoleCmd {
        ConsoleCmd::FetchLibrary {
            addr: self.addr.clone(),
            mgmt: self.mgmt,
            fp_hex: self.fp_hex.clone(),
        }
    }

    /// Pull the shared model when it changed; decode newly arrived poster bytes.
    fn sync(&mut self, library: &LibraryShared) {
        if self.shared.is_none() {
            self.shared = Some(library.clone());
        }
        let Some(shared) = &self.shared else { return };
        if shared.generation() != self.generation {
            let (phase, games, generation) = shared.snapshot();
            let fresh = self.games.len() != games.len()
                || self.games.iter().zip(&games).any(|(a, b)| a.id != b.id);
            self.phase = phase;
            self.games = games;
            self.generation = generation;
            if fresh {
                self.cursor = 0;
                self.anim = Spring::rest(0.0);
                self.bump = Spring::rest(0.0);
                self.art.clear();
            }
            self.cursor = self.cursor.clamp(0, (self.games.len() as i32 - 1).max(0));
        }
        for (id, bytes) in shared.drain_art() {
            match Image::from_encoded(Data::new_copy(&bytes)) {
                Some(img) => {
                    self.art.insert(id, img);
                }
                None => tracing::debug!(%id, "undecodable poster"),
            }
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.sync(ctx.library);
        match &self.phase {
            LibraryPhase::Ready => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                MenuEvent::JumpBack => self.step(-JUMP, true),
                MenuEvent::JumpForward => self.step(JUMP, true),
                MenuEvent::Confirm => {
                    let g = self.games.get(self.cursor as usize)?;
                    fx.connect = Some(ConnectIntent {
                        addr: self.addr.clone(),
                        port: self.port,
                        fp_hex: self.fp_hex.clone(),
                        launch: Some(g.id.clone()),
                        title: g.title.clone(),
                    });
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                MenuEvent::Move(_) | MenuEvent::Secondary | MenuEvent::Tertiary => None,
            },
            LibraryPhase::Error { can_retry, .. } => match ev {
                MenuEvent::Confirm if *can_retry => {
                    self.phase = LibraryPhase::Loading; // local; the fetch re-syncs it
                    fx.cmds.push(self.fetch_cmd());
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                _ => None,
            },
            LibraryPhase::Loading | LibraryPhase::Empty => {
                if ev == MenuEvent::Back {
                    fx.pop();
                }
                None
            }
        }
    }

    fn step(&mut self, delta: i32, clamp: bool) -> Option<MenuPulse> {
        match step_cursor(self.cursor, self.games.len(), delta, clamp) {
            StepResult::Moved(to) => {
                self.cursor = to;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: -BUMP_PX * f64::from(delta.signum()),
                    vel: 0.0,
                };
                Some(MenuPulse::Boundary)
            }
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        match &self.phase {
            LibraryPhase::Ready => vec![
                Hint::new(HintKey::Confirm, "Play"),
                Hint::new(HintKey::Shoulders, "Jump"),
                Hint::new(HintKey::Back, "Back"),
            ],
            LibraryPhase::Error {
                can_retry: true, ..
            } => vec![
                Hint::new(HintKey::Confirm, "Retry"),
                Hint::new(HintKey::Back, "Back"),
            ],
            _ => vec![Hint::new(HintKey::Back, "Back")],
        }
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.sync(ctx.library);
        let (w, cy_all) = (
            f64::from(rect.width()),
            f64::from(rect.top) + f64::from(rect.height()) / 2.0,
        );
        let cx = f64::from(rect.left) + w / 2.0;
        match self.phase.clone() {
            LibraryPhase::Ready => {
                self.anim
                    .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
                self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
                self.bump.step(0.0, BUMP_K, BUMP_C, dt);
                self.bump.settle(0.0, 0.3, 4.0);
                self.draw_carousel(canvas, rect, k, fonts);
            }
            LibraryPhase::Loading => {
                crate::theme::spinner(canvas, cx, cy_all - 24.0 * k, 16.0 * k, ctx.t);
                fonts.centered(
                    canvas,
                    "Loading library…",
                    W::Regular,
                    14.0 * k,
                    DIM,
                    cx,
                    cy_all + 16.0 * k,
                    w * 0.8,
                );
            }
            LibraryPhase::Empty => {
                fonts.centered(
                    canvas,
                    "No games found",
                    W::Bold,
                    22.0 * k,
                    WHITE,
                    cx,
                    cy_all - 20.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    "Install Steam titles or add custom entries in the host's web console.",
                    W::Regular,
                    14.0 * k,
                    DIM,
                    cx,
                    cy_all + 12.0 * k,
                    w * 0.8,
                );
            }
            LibraryPhase::Error { title, body, .. } => {
                fonts.centered(
                    canvas,
                    &title,
                    W::Bold,
                    22.0 * k,
                    WHITE,
                    cx,
                    cy_all - 32.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    &body,
                    W::Regular,
                    14.0 * k,
                    DIM,
                    cx,
                    cy_all + 4.0 * k,
                    (600.0 * k).min(w * 0.85),
                );
            }
        }
    }

    fn draw_carousel(&mut self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts) {
        let (card_w, card_h) = (POSTER_W * k, POSTER_H * k);
        let w = f64::from(rect.width());
        // The strip rides slightly above center; the detail block gets the band below.
        let cy = f64::from(rect.top) + f64::from(rect.height()) * 0.44;
        let pos = self.anim.pos;
        let bump = self.bump.pos * k;

        // Paint order = draw order: farthest from the (integer) cursor first, so the
        // dense side stacks overlap toward the focus.
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
            let offset = if a <= 1.0 {
                d * FOCUS_GAP * k
            } else {
                d.signum() * (FOCUS_GAP + (a - 1.0) * SIDE_SPACING) * k
            };
            let ccx = f64::from(rect.left) + w / 2.0 + offset + bump;
            let m = card_matrix(ccx, cy, angle, scale, card_w, card_h, PERSPECTIVE * k);

            let game = &self.games[i];
            canvas.save();
            canvas.concat_44(&M44::row_major(&m));
            let crect = Rect::from_wh(card_w as f32, card_h as f32);
            let rr = RRect::new_rect_xy(crect, 16.0 * k as f32, 16.0 * k as f32);
            canvas.clip_rrect(rr, None, true);
            match self.art.get(&game.id) {
                Some(img) => {
                    // Cover-fit: center-crop the source to the card's 2:3.
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let card_aspect = crect.width() / crect.height();
                    let src = if iw / ih > card_aspect {
                        let sw = ih * card_aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / card_aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    canvas.draw_image_rect(
                        img,
                        Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                        crect,
                        &Paint::default(),
                    );
                }
                None => {
                    // Solid face, not glass: the side cards OVERLAP.
                    canvas.draw_rect(
                        crect,
                        &Paint::new(Color4f::new(0.118, 0.118, 0.145, 1.0), None),
                    );
                    let mono = initials(&game.title);
                    let font = fonts.font(W::Bold, 38.0 * k);
                    let tw = font.measure_str(&mono, None).0;
                    canvas.draw_str(
                        &mono,
                        Point::new(
                            (card_w as f32 - tw) / 2.0,
                            card_h as f32 / 2.0 + 13.0 * k as f32,
                        ),
                        &font,
                        &Paint::new(white(0.45), None),
                    );
                }
            }
            // Store badge, top-left.
            {
                let label = store_label(&game.store);
                let size = 11.0 * k;
                let tw = fonts.measure(label, W::SemiBold, size) as f64;
                let (px, py) = (8.0 * k, 8.0 * k);
                let (bw, bh) = (tw + 16.0 * k, 20.0 * k);
                canvas.draw_rrect(
                    RRect::new_rect_xy(
                        Rect::from_xywh(px as f32, py as f32, bw as f32, bh as f32),
                        (bh / 2.0) as f32,
                        (bh / 2.0) as f32,
                    ),
                    &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.55), None),
                );
                fonts.draw(
                    canvas,
                    label,
                    px + 8.0 * k,
                    py + 14.0 * k,
                    W::SemiBold,
                    size,
                    WHITE,
                );
            }
            // The brightness recede: an opaque-black veil, never whole-card alpha.
            if prox > 0.0 {
                canvas.draw_rect(
                    crect,
                    &Paint::new(
                        Color4f::new(0.0, 0.0, 0.0, (prox * RECEDE_DIM) as f32),
                        None,
                    ),
                );
            }
            canvas.restore();
        }

        // Detail block: focused title + store, in the band under the strip.
        if let Some(g) = self.games.get(self.cursor as usize) {
            let cx = f64::from(rect.left) + w / 2.0;
            fonts.centered(
                canvas,
                &g.title,
                W::Bold,
                27.0 * k,
                WHITE,
                cx,
                f64::from(rect.bottom) - 64.0 * k,
                w * 0.8,
            );
            fonts.centered(
                canvas,
                &store_label(&g.store).to_uppercase(),
                W::Regular,
                12.0 * k,
                white(0.5),
                cx,
                f64::from(rect.bottom) - 30.0 * k,
                w * 0.5,
            );
        }
    }
}
