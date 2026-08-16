//! The game-library screen: the coverflow carousel (spring-chased cursor, perspective
//! card tilt, poster art streaming in) — the original console library, now one screen
//! on the shell's stack. B pops back to the host list; A launches the focused title in
//! the same window. The shell owns the aurora, chrome, and the connecting overlay.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, initials, step_cursor, store_label, LibraryGame, LibraryPhase, LibraryShared,
    StepResult, BUMP_C, BUMP_K, BUMP_PX, ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, FOCUS_GAP, JUMP,
    PERSPECTIVE, POSTER_H, POSTER_W, RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C,
    SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow, ProfileChip};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox};
use crate::theme::{accent, fg, Fonts, W};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Paint, Point, RRect, Rect, M44};
use std::collections::HashMap;

pub(crate) struct LibraryScreen {
    host_name: String,
    addr: String,
    port: u16,
    fp_hex: String,
    mgmt: u16,
    /// `Some` when this library was opened from a PINNED host+profile card (§5.2a) rather
    /// than the host's primary tile: every launch off this shelf is that card's connect
    /// with a title attached, so it carries the same one-off profile the card's plain
    /// A-press would. `None` = the primary tile, where the host's binding decides.
    pin: Option<ProfileChip>,
    shared: Option<LibraryShared>,
    // Synced snapshot of the shared model (re-pulled when the generation bumps).
    generation: u64,
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    // Navigation: the integer cursor is the authority; the eased position chases it.
    cursor: i32,
    /// Each card's rect as last drawn (axis-aligned, scale applied — the perspective tilt
    /// is a few degrees and well inside a finger's slop), empty for culled cards.
    geom: Vec<Rect>,
    anim: Spring,
    bump: Spring,
    /// Decoded posters by game id (decode once; Skia uploads lazily on first draw).
    art: HashMap<String, Image>,
    /// The shelf's entrance, and when `Ready` first landed. Unlike the home carousel this
    /// one waits for CONTENT: the choreography exists to show off artwork, and fanning open
    /// a rank of grey placeholder faces is worse than not animating at all. See
    /// [`Self::arm_entrance`].
    entrance: Option<Entrance>,
    entrance_armed: bool,
    ready_at: Option<f64>,
}

impl LibraryScreen {
    pub(crate) fn new(host: &HostRow) -> LibraryScreen {
        LibraryScreen {
            host_name: host.name.clone(),
            addr: host.addr.clone(),
            port: host.port,
            fp_hex: host.fp_hex.clone(),
            mgmt: host.mgmt_port,
            pin: host.pin.clone(),
            shared: None, // adopted from Ctx on the first render (the shell owns it)
            generation: u64::MAX,
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            cursor: 0,
            geom: Vec::new(),
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            art: HashMap::new(),
            entrance: None,
            entrance_armed: false,
            ready_at: None,
        }
    }

    /// Arm the shelf entrance once the coverflow has something worth showing: the cards
    /// around the cursor have posters, or 400 ms have gone by and they clearly aren't
    /// coming. Art streams in per title after the list lands, so without the wait the
    /// entrance would reliably play over placeholders — the fetch is the slow part, not
    /// the list.
    ///
    /// The deadline matters as much as the gate: a library of art-less custom entries
    /// still gets its entrance, just 400 ms later.
    fn arm_entrance(&mut self, t: f64) {
        if self.entrance_armed || !matches!(self.phase, LibraryPhase::Ready) {
            return;
        }
        let since = *self.ready_at.get_or_insert(t);
        let cursor = self.cursor.max(0) as usize;
        // The cards actually on screen at rest — the ones the fan opens around.
        let lo = cursor.saturating_sub(2);
        let hi = (cursor + 3).min(self.games.len());
        let have_art = self.games[lo..hi]
            .iter()
            .any(|g| self.art.contains_key(&g.id));
        if have_art || t - since >= 0.4 {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(entrances::CARDS, cursor, t));
        }
    }

    /// The screen's title: the host, and — when this shelf belongs to a pinned card — the
    /// profile every launch off it will use, in the card's own `host · profile` shape.
    pub(crate) fn title(&self) -> String {
        match &self.pin {
            Some(p) => format!("{} \u{b7} {}", self.host_name, p.name),
            None => self.host_name.clone(),
        }
    }

    /// One title's self-emitted link: this shelf's host, this shelf's pinned profile (so a
    /// link taken off a pinned card's shelf streams the way that card does), and the game.
    fn game_link(&self, id: &str) -> Option<String> {
        crate::screens::saved_host_link(
            &self.fp_hex,
            &self.addr,
            self.port,
            self.pin.as_ref().map(|p| p.id.as_str()),
            Some(id),
        )
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
                // A different set of titles is a different shelf, so it gets its own
                // arrival. This is also the FIRST one: the screen mounts on `Loading` with
                // no games, and the list landing is the moment the coverflow appears.
                self.entrance = None;
                self.entrance_armed = false;
                self.ready_at = None;
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
                        // A pinned card's shelf says which profile it is launching with,
                        // the same way its tile and this screen's title do.
                        title: match &self.pin {
                            Some(p) => format!("{} \u{b7} {}", g.title, p.name),
                            None => g.title.clone(),
                        },
                        request_access: false,
                        // A game launch off a PINNED card's shelf is that card's connect
                        // with a title attached — it carries the card's profile as the
                        // one-off. Off the primary tile there is none, and the host's
                        // default binding decides.
                        profile: self.pin.as_ref().map(|p| p.id.clone()),
                    });
                    Some(MenuPulse::Confirm)
                }
                // X copies the focused title's own `punktfunk://` link — the same
                // self-emitted URL a host tile's "Copy link" hands out, plus this game's
                // `launch=` id, so pasting it into Playnite or a Stream Deck macro boots
                // straight into the title. Direct rather than behind an options screen:
                // it is the only per-game action there is, and a menu holding one row is
                // a press the user pays for nothing.
                MenuEvent::Tertiary => {
                    let g = self.games.get(self.cursor as usize)?;
                    match self.game_link(&g.id) {
                        Some(url) => {
                            fx.copy = Some(url);
                            fx.toast = Some("Link copied".into());
                        }
                        // Only if the host left the store while the shelf was open.
                        None => fx.toast = Some("This host isn't saved any more".into()),
                    }
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                MenuEvent::Move(_) | MenuEvent::Secondary => None,
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

    /// Mouse/touch on the coverflow. Same rule as the home carousel: the centre card
    /// launches, any other one only comes to the front.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match p.kind {
            PointerKind::Scroll { up } => {
                self.step(if up { -1 } else { 1 }, false);
                true
            }
            PointerKind::Press => {
                // The cards OVERLAP, and the ones nearest the cursor are drawn on top —
                // so among the rects a press falls in, the topmost is the nearest. Picking
                // the first by index would hand the press to a card buried underneath.
                let hit = self
                    .geom
                    .iter()
                    .enumerate()
                    // The geometry is a frame old; a library refresh can shorten the shelf
                    // between the render that recorded it and this press.
                    .filter(|(i, r)| *i < self.games.len() && p.hits(**r))
                    .min_by_key(|(i, _)| (*i as i32 - self.cursor).abs())
                    .map(|(i, _)| i);
                match hit {
                    Some(i) if i == self.cursor as usize => {
                        self.menu(MenuEvent::Confirm, ctx, fx);
                        true
                    }
                    Some(i) => {
                        self.cursor = i as i32;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
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

    /// How many launcher entries lead the shelf — [`LibraryShared::set_games`] groups them at the
    /// front, so the launcher group is always the prefix `0..launcher_count()`.
    fn launcher_count(&self) -> usize {
        self.games.iter().take_while(|g| g.launcher).count()
    }

    /// Is the focused entry a launcher? (Drives the confirm hint: you *open* Steam, you *play* a
    /// game.)
    fn focused_is_launcher(&self) -> bool {
        self.games
            .get(self.cursor as usize)
            .is_some_and(|g| g.launcher)
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        match &self.phase {
            LibraryPhase::Ready => vec![
                Hint::new(
                    HintKey::Confirm,
                    if self.focused_is_launcher() {
                        "Open"
                    } else {
                        "Play"
                    },
                ),
                Hint::new(HintKey::Tertiary, "Copy link"),
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
                // See the twin in `home.rs`: the recoil's haptic survives, its travel does not.
                if crate::theme::reduce_motion() {
                    self.bump = Spring::rest(0.0);
                }
                self.arm_entrance(ctx.t);
                if self.entrance.is_some_and(|e| e.done(ctx.t)) {
                    self.entrance = None;
                }
                self.draw_carousel(canvas, rect, k, fonts, ctx.t);
            }
            LibraryPhase::Loading => {
                crate::theme::spinner(canvas, cx, cy_all - 24.0 * k, 16.0 * k, ctx.t);
                fonts.centered(
                    canvas,
                    "Loading library…",
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
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
                    fg(1.0),
                    cx,
                    cy_all - 20.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    "Install Steam titles or add custom entries in the host's web console.",
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
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
                    fg(1.0),
                    cx,
                    cy_all - 32.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    &body,
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
                    cx,
                    cy_all + 4.0 * k,
                    (600.0 * k).min(w * 0.85),
                );
            }
        }
    }

    fn draw_carousel(&mut self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
        let (card_w, card_h) = (POSTER_W * k, POSTER_H * k);
        let w = f64::from(rect.width());
        // The strip rides slightly above center; the detail block gets the band below.
        let cy = f64::from(rect.top) + f64::from(rect.height()) * 0.44;
        let pos = self.anim.pos;
        let bump = self.bump.pos * k;

        // Group heading. The model groups launcher entries at the front (design D4), and a
        // coverflow is one-dimensional — so instead of a second focus rail (a new up/down nav
        // model, in three renderers, for two or three tiles) the heading names the group the
        // cursor is in and changes as it crosses the boundary. Drawn only when the shelf
        // actually has both groups, so a library without launchers looks exactly as before.
        let launchers = self.launcher_count();
        if launchers > 0 && launchers < self.games.len() {
            let heading = if (self.cursor as usize) < launchers {
                "LAUNCHERS"
            } else {
                "GAMES"
            };
            fonts.centered(
                canvas,
                heading,
                W::SemiBold,
                12.0 * k,
                // 0.45 is the SECTION-HEADER weight, the same one `MenuList` gives its own
                // group headers. This sat at 0.5 and read a shade louder than the identical
                // role two screens over.
                fg(0.45),
                f64::from(rect.left) + w / 2.0,
                cy - card_h / 2.0 - 22.0 * k,
                w * 0.5,
            );
        }

        // Paint order = draw order: farthest from the (integer) cursor first, so the
        // dense side stacks overlap toward the focus.
        let mut order: Vec<usize> = (0..self.games.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse((i as i32 - self.cursor).abs()));
        self.geom.clear();
        self.geom.resize(self.games.len(), Rect::new_empty());

        for i in order {
            let d = i as f64 - pos;
            let a = d.abs();
            if a > VISIBLE_RANGE {
                continue;
            }
            let prox = a.min(1.0);
            // The entrance rides the card's REAL Y-rotation — the whole reason Apple had to
            // fake its turn with a cos-squeeze is that SwiftUI can't snapshot a rotated
            // layer to glass, and Skia has no such constraint here. Cards turn away in the
            // direction they sit from the anchor, so the strip fans open like a book.
            let ent = self.entrance.map_or(EntranceAt::SETTLED, |e| e.at(i, t));
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let turn = (1.0 - ent.travel)
                * ENTER_TURN_DEG
                * if (i as i32) < self.cursor { -1.0 } else { 1.0 };
            let scale = (1.0 - prox * RECEDE_SCALE) * arrive;
            let angle = -d.clamp(-1.0, 1.0) * ROTATE_DEG + turn;
            let offset = if a <= 1.0 {
                d * FOCUS_GAP * k
            } else {
                d.signum() * (FOCUS_GAP + (a - 1.0) * SIDE_SPACING) * k
            };
            let ccx = f64::from(rect.left) + w / 2.0 + offset + bump;
            let cy = cy + (1.0 - ent.travel) * ENTER_RISE * k;
            self.geom[i] = Rect::from_xywh(
                (ccx - card_w * scale / 2.0) as f32,
                (cy - card_h * scale / 2.0) as f32,
                (card_w * scale) as f32,
                (card_h * scale) as f32,
            );
            let m = card_matrix(ccx, cy, angle, scale, card_w, card_h, PERSPECTIVE * k);

            let game = &self.games[i];
            // The focused card's glow, drawn in SCREEN space before the card's own
            // transform: it is light spilling AROUND the card, so it cannot live inside the
            // rounded rect the card clips itself to. Fades with the sprung proximity rather
            // than snapping on the integer cursor, so it travels with the strip. The few
            // degrees of perspective it misses are invisible on an 18 dp blur.
            crate::theme::focus_halo(canvas, self.geom[i], 16.0, k as f32, (1.0 - prox) as f32);
            canvas.save();
            canvas.concat_44(&M44::row_major(&m));
            let crect = Rect::from_wh(card_w as f32, card_h as f32);
            let rr = RRect::new_rect_xy(crect, 16.0 * k as f32, 16.0 * k as f32);
            canvas.clip_rrect(rr, None, true);
            // ONE layer carrying both the entrance fade and the colour recede, raised only
            // when a card needs either — the focused, settled card still pays nothing.
            // Raised after the clip so `None` bounds mean the CARD and not the screen: a
            // full-screen layer per card is the one way this could cost real time on a
            // Deck. This is the plan's named O(visible cards) cost, and the thing to watch
            // on a Deck frame graph if the shelf ever feels heavy.
            let fading = ent.fade < 1.0;
            let layered = fading || prox > 0.001;
            if layered {
                let mut lp = Paint::default();
                lp.set_alpha_f(ent.fade as f32);
                if prox > 0.001 {
                    lp.set_color_filter(skia_safe::color_filters::matrix_row_major(
                        &crate::theme::recede_matrix(prox),
                        None,
                    ));
                }
                canvas.save_layer(&skia_safe::canvas::SaveLayerRec::default().paint(&lp));
            }
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
                    //
                    // A launcher tile usually has no poster, and an art-less launcher drawn like
                    // an art-less game reads as "a game whose cover failed to load". So it gets
                    // the brand-tinted face and names its launcher, instead of a title monogram.
                    let face = if game.launcher {
                        Color4f::new(0.153, 0.137, 0.267, 1.0)
                    } else {
                        Color4f::new(0.118, 0.118, 0.145, 1.0)
                    };
                    canvas.draw_rect(crect, &Paint::new(face, None));
                    // The launcher's brand mark IS the poster when we ship one for it. Inset to
                    // ~44% of the card so it reads as a mark on a face rather than a cropped
                    // cover; `launcher_mark` letterboxes inside that box, so a non-square master
                    // (Steam 496x512, Playnite 1024x1024) keeps its proportions.
                    let mark = (!game.icon.is_empty())
                        .then(|| {
                            let side = (card_w.min(card_h) as f32) * 0.44;
                            crate::launcher_icons::launcher_mark(
                                &game.icon,
                                Rect::from_xywh(
                                    (card_w as f32 - side) / 2.0,
                                    (card_h as f32 - side) / 2.0,
                                    side,
                                    side,
                                ),
                            )
                        })
                        .flatten();
                    if let Some(path) = mark {
                        canvas.draw_path(&path, &Paint::new(fg(0.85), None));
                    } else {
                        let (glyph, size, ink) = if game.launcher {
                            (store_label(&game.store).to_string(), 22.0 * k, fg(0.85))
                        } else {
                            (initials(&game.title), 38.0 * k, fg(0.45))
                        };
                        let font = fonts.font(W::Bold, size);
                        let tw = font.measure_str(&glyph, None).0;
                        canvas.draw_str(
                            &glyph,
                            Point::new(
                                (card_w as f32 - tw) / 2.0,
                                card_h as f32 / 2.0 + 13.0 * k as f32,
                            ),
                            &font,
                            &Paint::new(ink, None),
                        );
                    }
                }
            }
            // Store badge, top-left.
            {
                let label = store_label(&game.store);
                let size = 11.0 * k;
                let tw = fonts.measure(label, W::SemiBold, size) as f64;
                let (px, py) = (8.0 * k, 8.0 * k);
                let (bw, bh) = (tw + 16.0 * k, 20.0 * k);
                // Brand-filled for a launcher, smoked glass for a game — the one cue that
                // survives being three cards deep in the recede.
                let pill = if game.launcher {
                    accent(0.85)
                } else {
                    crate::theme::shade(0.55)
                };
                canvas.draw_rrect(
                    RRect::new_rect_xy(
                        Rect::from_xywh(px as f32, py as f32, bw as f32, bh as f32),
                        (bh / 2.0) as f32,
                        (bh / 2.0) as f32,
                    ),
                    &Paint::new(pill, None),
                );
                fonts.draw(
                    canvas,
                    label,
                    px + 8.0 * k,
                    py + 14.0 * k,
                    W::SemiBold,
                    size,
                    fg(1.0),
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
            if layered {
                canvas.restore(); // the entrance/recede layer
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
                fg(1.0),
                cx,
                f64::from(rect.bottom) - 64.0 * k,
                w * 0.8,
            );
            let sub = if g.launcher {
                format!("{} · LAUNCHER", store_label(&g.store).to_uppercase())
            } else {
                store_label(&g.store).to_uppercase()
            };
            fonts.centered(
                canvas,
                &sub,
                W::Regular,
                12.0 * k,
                // The subtitle rung of the 0.55 / 0.7 / 0.85 ladder every other detail line
                // in the crate already sits on.
                fg(0.55),
                cx,
                f64::from(rect.bottom) - 30.0 * k,
                w * 0.5,
            );
        }
    }
}
