//! The game-library screen: the coverflow carousel (spring-chased cursor, perspective
//! card tilt, poster art streaming in) — the original console library, now one screen
//! on the shell's stack. B pops back to the host list; A launches the focused title in
//! the same window. The shell owns the aurora, chrome, and the connecting overlay.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, initials, step_cursor, store_label, GridDir, LibraryGame, LibraryPhase,
    LibraryShared, LibraryView, StepResult, BUMP_C, BUMP_K, BUMP_PX, ENTER_RISE, ENTER_SCALE,
    ENTER_TURN_DEG, FOCUS_GAP, GRID_GAP, GRID_H, GRID_W, JUMP, PERSPECTIVE, POSTER_H, POSTER_W,
    RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C, SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, fg, Fonts, W};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Paint, Point, RRect, Rect, M44};
use std::collections::HashMap;

/// How wide a margin the grid keeps at each side of the field.
const GRID_MARGIN: f64 = 48.0;
/// Air under each grid row (the cell's own label space — the title itself lives in the
/// shared detail band, so this is breathing room rather than text).
const GRID_LABEL: f64 = 10.0;
/// The band a grid group heading occupies.
const GRID_HEADING: f64 = 30.0;
/// The band the focused title's name and store occupy under either arrangement.
const DETAIL_BAND: f64 = 84.0;
/// How many decoded posters stay resident. Roughly nine grid rows' worth — enough that
/// paging back and forth over the same stretch never re-decodes, small enough that a
/// 400-title library cannot accumulate all of it. See `LibraryScreen::evict_art`.
const ART_BUDGET: usize = 160;

/// Which decoded posters to drop, coldest first, once more than [`ART_BUDGET`] are live.
///
/// Split out from the screen so the POLICY can be tested without decoding images: what
/// matters here is that the covers currently on screen are the last to go, and that is a
/// statement about stamps, not about Skia.
fn art_to_evict(live: &[String], seen: &HashMap<String, u64>) -> Vec<String> {
    if live.len() <= ART_BUDGET {
        return Vec::new();
    }
    let mut by_age: Vec<(u64, &String)> = live
        .iter()
        // A poster decoded but never yet DRAWN stamps 0 and goes first. That is right: it
        // is off-screen by definition, and re-decoding costs one frame of grey.
        .map(|id| (seen.get(id).copied().unwrap_or(0), id))
        .collect();
    by_age.sort_unstable();
    by_age
        .into_iter()
        .take(live.len() - ART_BUDGET)
        .map(|(_, id)| id.clone())
        .collect()
}

/// The loading state: placeholder cards in the arrangement the real ones will arrive in,
/// with a sheen travelling across them.
///
/// One animated linear gradient over the whole run rather than a shader or a per-card
/// animation — the sweep is the only thing that says "still working", and it costs one
/// paint. Frozen under reduced motion, where a still skeleton says the same thing.
fn draw_skeleton(canvas: &Canvas, rect: Rect, k: f64, t: f64, view: LibraryView, cols: usize) {
    let w = f64::from(rect.width());
    let cy = f64::from(rect.top) + (f64::from(rect.height()) - DETAIL_BAND * k) / 2.0;
    let mut cells: Vec<Rect> = Vec::new();
    match view {
        LibraryView::Shelf => {
            // The coverflow's arrangement, without its perspective: a big centre card and
            // two receded neighbours each side.
            let (cw, ch) = (POSTER_W * k, POSTER_H * k);
            for d in -2i32..=2 {
                let s = 1.0 - f64::from(d.abs()).min(1.0) * RECEDE_SCALE;
                let off = if d.abs() <= 1 {
                    f64::from(d) * FOCUS_GAP * k
                } else {
                    f64::from(d.signum()) * (FOCUS_GAP + SIDE_SPACING) * k
                };
                let cx = f64::from(rect.left) + w / 2.0 + off;
                cells.push(Rect::from_xywh(
                    (cx - cw * s / 2.0) as f32,
                    (cy - ch * s / 2.0) as f32,
                    (cw * s) as f32,
                    (ch * s) as f32,
                ));
            }
        }
        LibraryView::Grid => {
            let (cw, ch) = (GRID_W * k, GRID_H * k);
            let pitch_x = cw + GRID_GAP * k;
            let grid_w = cols as f64 * pitch_x - GRID_GAP * k;
            let x0 = f64::from(rect.left) + (w - grid_w) / 2.0;
            let rows = 2;
            let y0 = cy - (rows as f64 * (ch + GRID_GAP * k) - GRID_GAP * k) / 2.0;
            for r in 0..rows {
                for c in 0..cols {
                    cells.push(Rect::from_xywh(
                        (x0 + c as f64 * pitch_x) as f32,
                        (y0 + r as f64 * (ch + GRID_GAP * k)) as f32,
                        cw as f32,
                        ch as f32,
                    ));
                }
            }
        }
    }
    for cell in &cells {
        canvas.draw_rrect(
            RRect::new_rect_xy(*cell, (14.0 * k) as f32, (14.0 * k) as f32),
            &Paint::new(fg(0.06), None),
        );
    }
    // The sheen: a narrow bright band travelling left to right on a 1.6 s cycle.
    let phase = if crate::theme::reduce_motion() {
        0.35
    } else {
        (t / 1.6).fract()
    };
    let span = w * 0.45;
    let head = f64::from(rect.left) - span + (w + 2.0 * span) * phase;
    let mut sheen = Paint::default();
    let stops = [fg(0.0), fg(0.09), fg(0.0)];
    sheen.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            Point::new(head as f32, rect.top),
            Point::new((head + span) as f32, rect.bottom),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &stops,
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    for cell in &cells {
        canvas.draw_rrect(
            RRect::new_rect_xy(*cell, (14.0 * k) as f32, (14.0 * k) as f32),
            &sheen,
        );
    }
}

/// A cover we have no art for, drawn into `rect`.
///
/// A LAUNCHER without a poster is not a game whose cover failed to load, and must not read
/// like one: it gets the brand-tinted face and its launcher's mark (or name), where a game
/// gets the darker face and a title monogram. `None` is the skeleton case — a cell that is
/// still waiting for the library itself.
fn draw_poster_placeholder(
    canvas: &Canvas,
    fonts: &Fonts,
    game: Option<&LibraryGame>,
    rect: Rect,
    k: f64,
) {
    // Solid, never glass: coverflow side cards OVERLAP, so a translucent face would show
    // its neighbour through it.
    let face = match game {
        Some(g) if g.launcher => Color4f::new(0.153, 0.137, 0.267, 1.0),
        _ => Color4f::new(0.118, 0.118, 0.145, 1.0),
    };
    canvas.draw_rect(rect, &Paint::new(face, None));
    let Some(game) = game else { return };
    // The launcher's brand mark IS the poster when we ship one for it. Inset to ~44 % of
    // the card so it reads as a mark on a face rather than a cropped cover; `launcher_mark`
    // letterboxes inside that box, so a non-square master (Steam 496×512, Playnite
    // 1024×1024) keeps its proportions.
    let mark = (!game.icon.is_empty())
        .then(|| {
            let side = rect.width().min(rect.height()) * 0.44;
            crate::launcher_icons::launcher_mark(
                &game.icon,
                Rect::from_xywh(
                    rect.left + (rect.width() - side) / 2.0,
                    rect.top + (rect.height() - side) / 2.0,
                    side,
                    side,
                ),
            )
        })
        .flatten();
    if let Some(path) = mark {
        canvas.draw_path(&path, &Paint::new(fg(0.85), None));
        return;
    }
    // Sized off the CARD rather than off `k`: the grid's cells are two-thirds the shelf's,
    // and a monogram scaled for a poster would not fit one.
    let (glyph, size, ink) = if game.launcher {
        (
            store_label(&game.store).to_string(),
            f64::from(rect.height()) * 0.067,
            fg(0.85),
        )
    } else {
        (
            initials(&game.title),
            f64::from(rect.height()) * 0.115,
            fg(0.45),
        )
    };
    let font = fonts.font(W::Bold, size.max(9.0 * k));
    let tw = font.measure_str(&glyph, None).0;
    canvas.draw_str(
        &glyph,
        Point::new(
            rect.left + (rect.width() - tw) / 2.0,
            rect.center_y() + (size * 0.36) as f32,
        ),
        &font,
        &Paint::new(ink, None),
    );
}

pub(crate) struct LibraryScreen {
    /// The host this shelf belongs to, kept WHOLE rather than unpacked into scalars: the
    /// Collections drill-in builds a second `LibraryScreen` from it, and two partial copies
    /// of the same host is the state that goes stale first.
    ///
    /// Its `pin` is load-bearing. `Some` = this library was opened from a PINNED
    /// host+profile card (§5.2a) rather than the host's primary tile, so every launch off
    /// this shelf is that card's connect with a title attached and carries the same one-off
    /// profile. `None` = the primary tile, where the host's binding decides.
    host: HostRow,
    shared: Option<LibraryShared>,
    // Synced snapshot of the shared model (re-pulled when the generation bumps).
    generation: u64,
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Display order: indices into `games`, as [`crate::collate`] arranged them. The cursor
    /// indexes THIS, not `games` — which is what lets the plain shelf, a chosen sort and a
    /// collection drill-in all be the same screen with the same cursor arithmetic, and what
    /// keeps the art cache keyed on the model's own identities rather than on positions.
    view: Vec<usize>,
    sort: crate::collate::SortKey,
    /// `Some` = this shelf shows ONE collated group (the Collections drill-in). The shared
    /// model is untouched by it: filtering is index-level, so the art pump and the fetch
    /// flow never learn that a filter exists.
    filter: Option<crate::collate::GroupKey>,
    /// The filter's display name, for the title breadcrumb. Held beside the key rather than
    /// re-derived, because `GroupKey::Store("Steam")` and `GroupKey::Platform("Steam")` both
    /// read "Steam" and the collation that produced the label is gone by then.
    filter_label: Option<String>,
    // Navigation: the integer cursor is the authority; the eased position chases it.
    cursor: i32,
    /// Each card's rect as last drawn (axis-aligned, scale applied — the perspective tilt
    /// is a few degrees and well inside a finger's slop), empty for culled cards.
    geom: Vec<Rect>,
    anim: Spring,
    bump: Spring,
    /// Which arrangement is drawing (persisted as `library_view`).
    view_mode: LibraryView,
    /// The grid's vertical scroll, in device px, chasing the focused row.
    scroll: Spring,
    /// Seat the scroll instead of chasing it on the next frame — used when the arrangement
    /// changes, where "gliding from the old position" is meaningless.
    snap_scroll: bool,
    /// Columns the last frame actually drew. Navigation reads THIS rather than recomputing,
    /// so the cursor arithmetic and the layout can never disagree about the grid's shape.
    grid_cols_last: usize,
    /// Decoded posters by game id (decode once; Skia uploads lazily on first draw).
    art: HashMap<String, Image>,
    /// Frame stamp of each decoded poster's last DRAW, and the frame counter behind it.
    ///
    /// The grid is what forces this. A 400-title library paged through in the shelf touched
    /// a dozen decodes; paged through in the grid it touches all of them, and `art` had no
    /// eviction at all — every cover ever scrolled past stayed resident for the life of the
    /// screen. Trimming the coldest costs a frame of grey on re-entry and nothing else:
    /// the ENCODED bytes are not kept here either way, so the fetch pipeline is untouched.
    art_seen: HashMap<String, u64>,
    frame: u64,
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
            host: host.clone(),
            shared: None, // adopted from Ctx on the first render (the shell owns it)
            generation: u64::MAX,
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            view: Vec::new(),
            sort: crate::collate::SortKey::default(),
            filter: None,
            filter_label: None,
            cursor: 0,
            geom: Vec::new(),
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            view_mode: LibraryView::default(),
            scroll: Spring::rest(0.0),
            snap_scroll: true,
            grid_cols_last: 4,
            art: HashMap::new(),
            art_seen: HashMap::new(),
            frame: 0,
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
        let hi = (cursor + 3).min(self.len());
        let have_art = (lo..hi)
            .filter_map(|i| self.game(i))
            .any(|g| self.art.contains_key(&g.id));
        if have_art || t - since >= 0.4 {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(entrances::CARDS, cursor, t));
        }
    }

    /// Adopt the persisted presentation settings. Read every frame rather than at
    /// construction because they can be changed while this screen is on the stack, and a
    /// shelf that only picked them up on re-entry would look broken for one visit.
    fn adopt_settings(&mut self, ctx: &Ctx) {
        let sort = crate::collate::SortKey::parse(&ctx.settings.library_sort);
        if sort != self.sort {
            self.sort = sort;
            self.recollate();
        }
        let view = LibraryView::parse(&ctx.settings.library_view);
        if view != self.view_mode {
            self.view_mode = view;
            // The two arrangements do not share a scroll — the shelf's is a horizontal
            // spring through a corridor of cards, the grid's is vertical rows — so the new
            // one is SEATED on the cursor next frame rather than gliding in from wherever
            // the other happened to be parked.
            self.snap_scroll = true;
        }
    }

    /// Columns that fit `rect` at scale `k`. Recomputed per frame rather than stored: the
    /// window resizes mid-stream, and a stale column count would put the cursor arithmetic
    /// and the drawn layout on different grids — which reads as the focus ring landing on
    /// the wrong cover.
    fn grid_cols(&self, rect: Rect, k: f64) -> usize {
        let avail = f64::from(rect.width()) - 2.0 * GRID_MARGIN * k;
        let pitch = (GRID_W + GRID_GAP) * k;
        // `+ GRID_GAP` because the last column needs no trailing gap.
        (((avail + GRID_GAP * k) / pitch).floor() as i64).clamp(2, 8) as usize
    }

    /// Rebuild the display order after anything that could change it: a new game list, a
    /// new sort, a new filter. The cursor is clamped rather than followed by identity — a
    /// re-sort moves everything, so "keep the same index" and "keep the same title" are
    /// both arbitrary, and the cheap one at least never points off the end.
    fn recollate(&mut self) {
        self.view = crate::collate::filtered(&self.games, self.sort, self.filter.as_ref());
        self.cursor = self.cursor.clamp(0, (self.view.len() as i32 - 1).max(0));
    }

    /// The game at a DISPLAY index (`None` past the end, or if the order went stale).
    fn game(&self, i: usize) -> Option<&LibraryGame> {
        self.games.get(*self.view.get(i)?)
    }

    fn focused(&self) -> Option<&LibraryGame> {
        self.game(self.cursor.max(0) as usize)
    }

    /// Tiles on the shelf — the FILTERED count, not the library's.
    fn len(&self) -> usize {
        self.view.len()
    }

    /// How many tiles this shelf is showing, for the shell's collection-flow test — which
    /// lives in another module and so cannot reach [`Self::len`].
    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.len()
    }

    /// The screen's title: the host, and — when this shelf belongs to a pinned card — the
    /// profile every launch off it will use, in the card's own `host · profile` shape.
    pub(crate) fn title(&self) -> String {
        // host · profile · collection, each part only when it applies — the same
        // breadcrumb shape a pinned card's shelf already used, extended by one.
        let mut t = match &self.host.pin {
            Some(p) => format!("{} \u{b7} {}", self.host.name, p.name),
            None => self.host.name.clone(),
        };
        if let Some(label) = &self.filter_label {
            t.push_str(" \u{b7} ");
            t.push_str(label);
        }
        t
    }

    /// Show ONE collated group. Set before the screen is first rendered (the Collections
    /// drill-in builds the shelf and pushes it in the same breath), so the shelf never
    /// flashes the whole library on its way to a collection.
    pub(crate) fn set_filter(&mut self, key: crate::collate::GroupKey, label: String) {
        self.filter = Some(key);
        self.filter_label = Some(label);
        self.recollate();
    }

    /// One title's self-emitted link: this shelf's host, this shelf's pinned profile (so a
    /// link taken off a pinned card's shelf streams the way that card does), and the game.
    fn game_link(&self, id: &str) -> Option<String> {
        crate::screens::saved_host_link(
            &self.host.fp_hex,
            &self.host.addr,
            self.host.port,
            self.host.pin.as_ref().map(|p| p.id.as_str()),
            Some(id),
        )
    }

    fn fetch_cmd(&self) -> ConsoleCmd {
        ConsoleCmd::FetchLibrary {
            addr: self.host.addr.clone(),
            mgmt: self.host.mgmt_port,
            fp_hex: self.host.fp_hex.clone(),
        }
    }

    /// Pull the shared model when it changed; decode newly arrived poster bytes.
    fn sync(&mut self, library: &LibraryShared) {
        if self.shared.is_none() {
            self.shared = Some(library.clone());
        }
        // Cloned rather than borrowed: `LibraryShared` is an `Arc` handle, so this costs a
        // refcount, and holding a borrow of `self.shared` across the body would forbid the
        // `&mut self` work below (re-collating the display order) for no benefit.
        let Some(shared) = self.shared.clone() else {
            return;
        };
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
                self.art_seen.clear();
                // A different set of titles is a different shelf, so it gets its own
                // arrival. This is also the FIRST one: the screen mounts on `Loading` with
                // no games, and the list landing is the moment the coverflow appears.
                self.entrance = None;
                self.entrance_armed = false;
                self.ready_at = None;
            }
            self.recollate();
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
        self.adopt_settings(ctx);
        match &self.phase {
            // The GRID reads the same events through 2-D arithmetic. The shelf is a line, so
            // it has up/down to spare and shoulders that jump along it; the grid spends
            // up/down on rows and gives the shoulders whole pages instead.
            LibraryPhase::Ready if self.view_mode == LibraryView::Grid => match ev {
                MenuEvent::Move(MenuDir::Left) => self.grid_move(GridDir::Left),
                MenuEvent::Move(MenuDir::Right) => self.grid_move(GridDir::Right),
                MenuEvent::Move(MenuDir::Up) => self.grid_move(GridDir::Up),
                MenuEvent::Move(MenuDir::Down) => self.grid_move(GridDir::Down),
                MenuEvent::JumpBack => self.grid_move(GridDir::PageBack),
                MenuEvent::JumpForward => self.grid_move(GridDir::PageForward),
                _ => self.ready_action(ev, fx),
            },
            LibraryPhase::Ready => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                MenuEvent::JumpBack => self.step(-JUMP, true),
                MenuEvent::JumpForward => self.step(JUMP, true),
                _ => self.ready_action(ev, fx),
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

    /// Move the grid cursor, seating the boundary recoil the same way the shelf does.
    fn grid_move(&mut self, dir: GridDir) -> Option<MenuPulse> {
        // The column count is a RENDER fact (it depends on the window), so navigation reads
        // the last frame's. One frame of staleness after a resize is invisible; deriving it
        // twice from different widths would not be.
        match crate::library::grid_step(self.cursor, self.len(), self.grid_cols_last, dir) {
            StepResult::Moved(c) => {
                self.cursor = c;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: -BUMP_PX * f64::from(matches!(dir, GridDir::Right | GridDir::Down) as i8),
                    vel: 0.0,
                };
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// What A, X and B do on a ready shelf — identical in both arrangements, because they
    /// act on the FOCUSED TITLE and neither view changes what that means.
    fn ready_action(&mut self, ev: MenuEvent, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Confirm => {
                let g = self.focused()?;
                fx.connect = Some(ConnectIntent {
                    addr: self.host.addr.clone(),
                    port: self.host.port,
                    fp_hex: self.host.fp_hex.clone(),
                    launch: Some(g.id.clone()),
                    // A pinned card's shelf says which profile it is launching with,
                    // the same way its tile and this screen's title do.
                    title: match &self.host.pin {
                        Some(p) => format!("{} \u{b7} {}", g.title, p.name),
                        None => g.title.clone(),
                    },
                    request_access: false,
                    // A game launch off a PINNED card's shelf is that card's connect
                    // with a title attached — it carries the card's profile as the
                    // one-off. Off the primary tile there is none, and the host's
                    // default binding decides.
                    profile: self.host.pin.as_ref().map(|p| p.id.clone()),
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
                let g = self.focused()?;
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
            // Y opens the collections. Refused — with a boundary pulse rather than
            // silence — when there is nothing to collect, which is the same condition that
            // keeps the hint off the legend, so the button and its label never disagree.
            //
            // Already inside a collection, Y is refused too: a drill-in from a drill-in
            // would collate a set that is already one group and open onto a single tile.
            MenuEvent::Secondary => {
                if self.filter.is_some() || !crate::collate::worth_browsing(&self.games) {
                    return Some(MenuPulse::Boundary);
                }
                let mut screen = super::collections::CollectionsScreen::new(&self.host, self.sort);
                // Hand over the posters already decoded here, so the group tiles fan real
                // covers instead of re-fetching art this screen is holding anyway.
                screen.adopt_art(self.art.clone());
                fx.push(Screen::Collections(screen));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Move(_) | MenuEvent::JumpBack | MenuEvent::JumpForward => None,
        }
    }

    /// Mouse/touch on the coverflow. Same rule as the home carousel: the centre card
    /// launches, any other one only comes to the front.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match p.kind {
            PointerKind::Scroll { up } => {
                // A wheel notch moves what the eye expects it to: one card along the
                // shelf, one ROW down the grid.
                if self.view_mode == LibraryView::Grid {
                    self.grid_move(if up { GridDir::Up } else { GridDir::Down });
                } else {
                    self.step(if up { -1 } else { 1 }, false);
                }
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
                    .filter(|(i, r)| *i < self.len() && p.hits(**r))
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
        match step_cursor(self.cursor, self.len(), delta, clamp) {
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
        self.view
            .iter()
            .map_while(|&i| self.games.get(i))
            .take_while(|g| g.launcher)
            .count()
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
            LibraryPhase::Ready => {
                let mut hints = vec![Hint::new(
                    HintKey::Confirm,
                    if self.focused_is_launcher() {
                        "Open"
                    } else {
                        "Play"
                    },
                )];
                // Only offered when there is something to browse, and never from inside a
                // collection — the same two conditions the button itself checks, so the
                // legend never advertises a press that would only thud.
                if self.filter.is_none() && crate::collate::worth_browsing(&self.games) {
                    hints.push(Hint::new(HintKey::Secondary, "Collections"));
                }
                hints.push(Hint::new(HintKey::Tertiary, "Copy link"));
                hints.push(Hint::new(HintKey::Shoulders, "Jump"));
                hints.push(Hint::new(HintKey::Back, "Back"));
                hints
            }
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
        self.adopt_settings(ctx);
        self.frame = self.frame.wrapping_add(1);
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
                match self.view_mode {
                    LibraryView::Shelf => self.draw_carousel(canvas, rect, k, fonts, ctx.t),
                    LibraryView::Grid => self.draw_grid(canvas, rect, k, fonts, ctx.t),
                }
                self.draw_detail_band(canvas, rect, k, fonts);
                self.evict_art();
            }
            // A skeleton shelf rather than a spinner and a line of text. The wait is for a
            // LIST, and its shape is known before its contents are — so showing that shape
            // is both more honest and less startling than a blank field that suddenly
            // becomes a coverflow.
            LibraryPhase::Loading => {
                draw_skeleton(
                    canvas,
                    rect,
                    k,
                    ctx.t,
                    self.view_mode,
                    self.grid_cols(rect, k),
                );
                fonts.centered(
                    canvas,
                    "Loading library…",
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
                    cx,
                    f64::from(rect.bottom) - 40.0 * k,
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

    /// Drop the coldest decoded posters once too many are resident. Called once per frame,
    /// after drawing, so "coldest" means "longest since it was actually on screen" rather
    /// than "longest since it arrived" — a cover the cursor keeps returning to survives a
    /// sweep through the rest of the library.
    fn evict_art(&mut self) {
        let live: Vec<String> = self.art.keys().cloned().collect();
        for id in art_to_evict(&live, &self.art_seen) {
            self.art.remove(&id);
            self.art_seen.remove(&id);
        }
    }

    /// The grid: a scrolling field of posters, `cols` wide.
    ///
    /// Shares everything that makes the shelf the shelf — the same cursor, the same
    /// collated order, the same detail band underneath, the same art cache — and differs
    /// only in where it puts the cards. That is deliberate: a second arrangement that
    /// forked any of those would be a second screen pretending to be a view.
    fn draw_grid(&mut self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
        let cols = self.grid_cols(rect, k);
        self.grid_cols_last = cols;
        let (cw, ch) = (GRID_W * k, GRID_H * k);
        let pitch_x = cw + GRID_GAP * k;
        let pitch_y = ch + GRID_GAP * k + GRID_LABEL * k;
        let launchers = self.launcher_count();
        // The launcher prefix keeps its own band, which is how design D4 reads in two
        // dimensions: the shelf says it with a heading that changes as the cursor crosses,
        // a grid says it with a gap and a heading over each half.
        let split_row = if launchers > 0 && launchers < self.len() {
            Some(launchers.div_ceil(cols))
        } else {
            None
        };
        let heading_h = GRID_HEADING * k;
        let row_top = |row: usize| -> f64 {
            let extra = match split_row {
                Some(s) if row >= s => heading_h,
                _ => 0.0,
            };
            row as f64 * pitch_y + extra + if split_row.is_some() { heading_h } else { 0.0 }
        };
        // Launchers occupy whole rows of their own, so a game never shares a row with one.
        let cell_of = |i: usize| -> (usize, usize) {
            match split_row {
                Some(s) if i >= launchers => {
                    let j = i - launchers;
                    (s + j / cols, j % cols)
                }
                _ => (i / cols, i % cols),
            }
        };

        let rows_total = match split_row {
            Some(s) => s + (self.len() - launchers).div_ceil(cols),
            None => self.len().div_ceil(cols),
        };
        let content_h = row_top(rows_total.saturating_sub(1)) + ch;
        // The detail band keeps its place at the bottom; the grid scrolls above it.
        let view_h = f64::from(rect.height()) - DETAIL_BAND * k;
        let (focus_row, _) = cell_of(self.cursor.max(0) as usize);
        // The focused row rides the upper-middle band rather than an edge, so there is
        // always a row of context above and below it.
        let want = (row_top(focus_row) - view_h * 0.34).clamp(0.0, (content_h - view_h).max(0.0));
        if std::mem::take(&mut self.snap_scroll) || crate::theme::reduce_motion() {
            self.scroll = Spring::rest(want);
        } else {
            self.scroll
                .step_spec(want, crate::anim::springs::FOCUS, 1.0 / 60.0);
            self.scroll.settle(want, 0.05, 0.5);
        }

        let grid_w = cols as f64 * pitch_x - GRID_GAP * k;
        let x0 = f64::from(rect.left) + (f64::from(rect.width()) - grid_w) / 2.0;
        let y0 = f64::from(rect.top);
        let viewport = Rect::from_xywh(rect.left, rect.top, rect.width(), (view_h.max(0.0)) as f32);

        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());
        canvas.save();
        canvas.clip_rect(viewport, None, true);
        if let Some(_s) = split_row {
            let head = |canvas: &Canvas, label: &str, y: f64| {
                fonts.draw_tracked(
                    canvas,
                    label,
                    x0,
                    y,
                    W::SemiBold,
                    12.0 * k,
                    1.4 * k,
                    fg(0.45),
                );
            };
            head(canvas, "LAUNCHERS", y0 + heading_h * 0.62 - self.scroll.pos);
            head(
                canvas,
                "GAMES",
                y0 + row_top(split_row.expect("checked")) - heading_h * 0.38 - self.scroll.pos,
            );
        }

        for i in 0..self.len() {
            let (row, col) = cell_of(i);
            let top = y0 + row_top(row) - self.scroll.pos;
            if top + ch < f64::from(rect.top) - ch || top > y0 + view_h + ch {
                continue; // off-screen: not drawn, and deliberately not art-stamped either
            }
            let ent = self.entrance.map_or(EntranceAt::SETTLED, |e| e.at(i, t));
            let f = if i == self.cursor.max(0) as usize {
                1.0
            } else {
                0.0
            };
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            // The focused cell pops; everything else sits flat. No coverflow recede here —
            // a grid's whole job is that every cell is equally readable.
            let scale = (1.0 + 0.06 * f) * arrive;
            let cx = x0 + col as f64 * pitch_x + cw / 2.0;
            let cy = top + ch / 2.0 + (1.0 - ent.travel) * ENTER_RISE * k;
            let cell = Rect::from_xywh(
                (cx - cw * scale / 2.0) as f32,
                (cy - ch * scale / 2.0) as f32,
                (cw * scale) as f32,
                (ch * scale) as f32,
            );
            self.geom[i] = cell;
            let Some(game) = self.game(i) else { continue };
            let id = game.id.clone();

            crate::theme::focus_halo(canvas, cell, 12.0, k as f32, f as f32);
            let fading = ent.fade < 1.0;
            if fading {
                canvas.save_layer_alpha_f(cell, ent.fade as f32);
            }
            let rr = RRect::new_rect_xy(cell, (12.0 * k) as f32, (12.0 * k) as f32);
            canvas.save();
            canvas.clip_rrect(rr, None, true);
            match self.art.get(&id) {
                Some(img) => {
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let aspect = cell.width() / cell.height();
                    let src = if iw / ih > aspect {
                        let sw = ih * aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    canvas.draw_image_rect(
                        img,
                        Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                        cell,
                        &Paint::default(),
                    );
                }
                None => draw_poster_placeholder(canvas, fonts, self.game(i), cell, k),
            }
            canvas.restore();
            // The focus ring goes OUTSIDE the clip, so it reads as a ring around the cover
            // rather than a border painted onto it.
            if f > 0.0 {
                crate::theme::panel(
                    canvas,
                    cell,
                    12.0,
                    None,
                    crate::theme::PanelStroke::Brand(0.9),
                    k as f32,
                );
            }
            if fading {
                canvas.restore();
            }
            self.art_seen.insert(id, self.frame);
        }
        canvas.restore();
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
        if launchers > 0 && launchers < self.len() {
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
        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse((i as i32 - self.cursor).abs()));
        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());

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

            let Some(game) = self.game(i) else { continue };
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
            let drawn_id = game.id.clone();
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
                None => draw_poster_placeholder(canvas, fonts, Some(game), crect, k),
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
            // Stamp AFTER drawing, so "coldest" means "longest off screen" — the shelf has
            // to stamp too, or an eviction sweep would drop the very covers on display.
            self.art_seen.insert(drawn_id, self.frame);
        }
    }

    /// The focused title's name and provenance, in the band under whichever arrangement is
    /// drawing. Shared deliberately: the shelf and the grid differ in how they show you the
    /// LIBRARY, not in how they describe one title, and a second copy of this would be a
    /// second place for the platform line to go stale.
    fn draw_detail_band(&self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts) {
        let Some(g) = self.focused() else { return };
        let w = f64::from(rect.width());
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
        // Store, and the PLATFORM when the host named one — the reason `platform` was
        // plumbed at all is that "Shadow of the Colossus" means something rather different
        // with "PS2" under it.
        let store = store_label(&g.store).to_uppercase();
        let sub = match (&g.platform, g.launcher) {
            (_, true) => format!("{store} · LAUNCHER"),
            (Some(p), _) if !p.trim().is_empty() => format!("{store} · {}", p.to_uppercase()),
            _ => store,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Eviction must never take a cover that is ON SCREEN. That is the whole point of
    /// stamping after the draw rather than on arrival: a 400-title library paged end to end
    /// touches every decode, and an LRU keyed on arrival would happily drop the six the
    /// cursor is sitting among.
    #[test]
    fn eviction_drops_the_coldest_and_keeps_the_focused_neighbourhood() {
        let live: Vec<String> = (0..ART_BUDGET + 40).map(|i| format!("g{i}")).collect();
        let mut seen = HashMap::new();
        // Everything was drawn long ago…
        for id in &live {
            seen.insert(id.clone(), 10u64);
        }
        // …except the neighbourhood around the cursor, drawn this frame.
        let hot: Vec<String> = (100..112).map(|i| format!("g{i}")).collect();
        for id in &hot {
            seen.insert(id.clone(), 9_000);
        }
        let dropped = art_to_evict(&live, &seen);
        assert_eq!(dropped.len(), 40, "trimmed back to exactly the budget");
        for id in &hot {
            assert!(!dropped.contains(id), "{id} was on screen and got evicted");
        }
    }

    #[test]
    fn eviction_does_nothing_under_the_budget() {
        let live: Vec<String> = (0..ART_BUDGET).map(|i| format!("g{i}")).collect();
        assert!(art_to_evict(&live, &HashMap::new()).is_empty());
    }

    /// A poster decoded but never drawn is the FIRST to go — it is off-screen by
    /// definition, whatever else the stamps say.
    #[test]
    fn never_drawn_posters_are_evicted_before_merely_old_ones() {
        let live: Vec<String> = (0..ART_BUDGET + 2).map(|i| format!("g{i}")).collect();
        let mut seen: HashMap<String, u64> = live.iter().map(|id| (id.clone(), 5)).collect();
        seen.remove("g7");
        seen.remove("g9");
        let dropped = art_to_evict(&live, &seen);
        assert_eq!(dropped.len(), 2);
        assert!(dropped.contains(&"g7".to_string()));
        assert!(dropped.contains(&"g9".to_string()));
    }
}
