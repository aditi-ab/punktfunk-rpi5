//! One tile per library group, and the sort that orders them.
//!
//! Owns no model. [`crate::collate`] groups the library snapshot; opening a tile
//! pushes a `LibraryScreen` with a filter rather than a filtered copy, so the art
//! pump, fetch, and shared model never learn this screen exists. Tile metrics match
//! the home carousel pixel-for-pixel.
//!
//! Two entry paths, one flag ([`CollectionsScreen::root`]). From a shelf's Y the
//! shelf stays underneath. As the host's library ("Start in collections",
//! `LibraryScreen::collections_upgrade`) nothing is underneath, so this screen
//! pumps the model's poster queue and Y opens the unfiltered list.
//!
//! Pin: `as_the_librarys_root_it_takes_only_the_covers_it_fans`,
//! `a_drill_in_from_a_shelf_leaves_the_queue_alone`,
//! `the_way_to_all_titles_exists_only_where_there_is_no_shelf`.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::collate::{collate, GroupBy, GroupKey, SortKey};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    initials, step_cursor, LibraryGame, LibraryShared, StepResult, BUMP_C, BUMP_K, BUMP_PX,
    ENTER_RISE, ENTER_SCALE, SPRING_C, SPRING_K,
};
use crate::model::HostRow;
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{accent, art_sampling, fg, fill, stroke, Fonts, PanelStroke, EDGE_INSET, W};
use crate::widgets::{TabStrip, TAB_STRIP_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Image, Matrix, Point, RRect, Rect, TileMode};
use std::collections::HashMap;

// Same numbers as home.rs — a collection tile is a host tile.
const TILE_W: f64 = 340.0;
const TILE_H: f64 = 224.0;
const TILE_GAP: f64 = 30.0;
const TILE_CORNER: f64 = 26.0;
/// Deck depth. Three reads as a shelf; more and the back cards vanish at tile size.
const FAN: usize = 3;

// Reserved even with no covers, so the caption corner and the title rail stay clear.
const FAN_W: f64 = 120.0;
const FAN_H: f64 = 130.0;
// Front cover; the rest are this rect, smaller. 2:3, matching the shelf posters it opens.
const COVER_H: f64 = 118.0;
const COVER_W: f64 = COVER_H * 2.0 / 3.0;
const COVER_CORNER: f64 = 11.0;
// Back cards: smaller, higher, further right, and turned. One cue alone is a smaller cover.
const FAN_SCALE_STEP: f64 = 0.07;
const FAN_DX: f64 = 18.0;
const FAN_DY: f64 = -7.0;
const FAN_ROT_DEG: f64 = 6.0;
// Hard round-rect, not a blur: three `MaskFilter`s per tile, fifteen on a live strip.
const PLATE_OUTSET: f64 = 3.0;
const PLATE_DX: f64 = 1.5;
const PLATE_DY: f64 = 2.0;
const PLATE_ALPHA: f32 = 0.38;

pub(crate) struct CollectionsScreen {
    /// Host and pinned profile from the shelf that opened this. A drill-in off a
    /// pinned card must still launch the way that card does.
    host: HostRow,
    cursor: i32,
    anim: Spring,
    bump: Spring,
    sort: SortKey,
    sort_tabs: TabStrip,
    groups: Vec<GroupTile>,
    generation: u64,
    /// Posters borrowed by id from the library screen. This screen never fetches art;
    /// a group with none yet (or ever — art-less ROMs) fans a monogram.
    art: HashMap<String, Image>,
    /// Decode scale: last frame's `k`, published by `render` before `sync`.
    art_k: f64,
    /// Opened as the host's library, not from a shelf's Y. Pumps the poster queue
    /// itself and offers Y as the way to the whole library — there is no shelf beneath.
    root: bool,
    geom: Vec<Rect>,
    entrance: Option<Entrance>,
    entrance_armed: bool,
}

struct GroupTile {
    key: GroupKey,
    label: String,
    count: usize,
    fan: Vec<FanCard>,
}

/// Id plus launcher icon — enough to draw the cover without holding the title.
///
/// A launcher has no poster; its cover is a brand mark from `icon`. Ids alone
/// fan empty ghosts for every launcher group.
struct FanCard {
    id: String,
    /// Launcher icon key (`steam`, …); empty for a game. A path built to the card
    /// rect, which is not known at collate time.
    icon: String,
}

impl CollectionsScreen {
    pub(crate) fn new(host: &HostRow, sort: SortKey) -> CollectionsScreen {
        CollectionsScreen {
            host: host.clone(),
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            sort,
            sort_tabs: TabStrip::new(),
            groups: Vec::new(),
            generation: u64::MAX,
            art: HashMap::new(),
            // Design scale until the first frame publishes the real `k` before `sync`.
            art_k: 1.0,
            root: false,
            geom: Vec::new(),
            entrance: None,
            entrance_armed: false,
        }
    }

    pub(crate) fn title(&self) -> String {
        self.host.name.clone()
    }

    /// Rebuild tiles on a library generation bump or a sort change. Collation is
    /// one pass; tiles hold labels and ids, not games.
    fn sync(&mut self, library: &LibraryShared) {
        if library.generation() != self.generation {
            let snap = library.snapshot();
            let (games, generation) = (snap.games, snap.generation);
            self.generation = generation;
            self.groups = collate(&games, self.sort, Some(GroupBy::Platform))
                .into_iter()
                .map(|g| GroupTile {
                    count: g.games.len(),
                    fan: g
                        .games
                        .iter()
                        .filter_map(|&i| games.get(i))
                        .map(|game: &LibraryGame| FanCard {
                            id: game.id.clone(),
                            icon: game.icon.clone(),
                        })
                        .take(FAN)
                        .collect(),
                    key: g.key,
                    label: g.label,
                })
                .collect();
            self.cursor = self.cursor.clamp(0, (self.groups.len() as i32 - 1).max(0));
        }
        // Art arrives after the list settles; the generation guard would miss it.
        if self.root {
            self.pump_art(library);
        }
    }

    /// Decode only the covers this screen fans, and only as the library's root.
    ///
    /// The shelf drains the model's queue wholesale. Bytes are pushed once per fetch
    /// and never re-sent: a wholesale drain here would keep a dozen posters and leave
    /// the next shelf with monograms for the rest of the session.
    fn pump_art(&mut self, library: &LibraryShared) {
        let want: std::collections::HashSet<String> = self
            .groups
            .iter()
            .flat_map(|g| g.fan.iter())
            .filter(|c| c.icon.is_empty() && !self.art.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if want.is_empty() {
            return;
        }
        for (id, bytes) in library.take_art_for(&want, super::library::ART_DECODES_PER_FRAME) {
            match super::library::decode_poster(&bytes, self.art_k) {
                Some(img) => {
                    self.art.insert(id, img);
                }
                None => tracing::debug!(%id, "undecodable poster"),
            }
        }
    }

    pub(crate) fn own_library(&mut self) {
        self.root = true;
    }

    /// Take posters the library screen already decoded. The model's queue is drained
    /// by the shelf this drill-in sits on; as root, [`Self::pump_art`] takes over.
    pub(crate) fn adopt_art(&mut self, art: HashMap<String, Image>) {
        self.art = art;
    }

    fn step(&mut self, delta: i32) -> Option<MenuPulse> {
        match step_cursor(self.cursor, self.groups.len(), delta, false) {
            StepResult::Moved(c) => {
                self.cursor = c;
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

    /// Step the sort and persist it. The shelf reads `library_sort` every frame, so
    /// the tiles here and the shelf behind re-order together.
    fn step_sort(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = SortKey::ALL;
        let at = all.iter().position(|s| *s == self.sort).unwrap_or(0);
        let next = (at as i32 + delta).rem_euclid(all.len() as i32) as usize;
        self.sort = all[next];
        super::library::store_sort(self.sort, ctx);
        // Sort changed, not the library — force a re-collate.
        self.generation = u64::MAX;
        Some(MenuPulse::Move)
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.sync(ctx.library);
        match ev {
            MenuEvent::Move(MenuDir::Left) => self.step(-1),
            MenuEvent::Move(MenuDir::Right) => self.step(1),
            MenuEvent::JumpBack => self.step_sort(-1, ctx),
            MenuEvent::JumpForward => self.step_sort(1, ctx),
            MenuEvent::Confirm => {
                let g = self.groups.get(self.cursor.max(0) as usize)?;
                // This epoch: a drill-in fetches nothing, it filters the list already
                // in the model. `set_filter` also refuses a second collections hand-over.
                let mut shelf =
                    super::library::LibraryScreen::new(&self.host, ctx.library.fetch_epoch());
                shelf.set_filter(g.key.clone(), g.label.clone());
                // Covers this tile just fanned. Without them the drill-in waits out
                // the art deadline and shows monograms: the shared queue was drained above.
                shelf.adopt_art(self.art.clone());
                fx.push(Screen::Library(shelf));
                Some(MenuPulse::Confirm)
            }
            // Whole-library only as root: nothing is underneath. From a shelf's Y
            // that shelf is one Back away; this would push a second copy of it.
            MenuEvent::Secondary if self.root => {
                let mut shelf =
                    super::library::LibraryScreen::new(&self.host, ctx.library.fetch_epoch());
                shelf.all_titles();
                shelf.adopt_art(self.art.clone());
                fx.push(Screen::Library(shelf));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop();
                None
            }
            _ => None,
        }
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        self.sync(ctx.library);
        match p.kind {
            PointerKind::Scroll { up } => {
                self.step(if up { -1 } else { 1 });
                true
            }
            PointerKind::Press => {
                if let Some(i) = self.sort_tabs.pointer(p) {
                    let all = SortKey::ALL;
                    if let Some(&s) = all.get(i) {
                        self.sort = s;
                        super::library::store_sort(s, ctx);
                        self.generation = u64::MAX;
                    }
                    return true;
                }
                match p.pick(&self.geom).filter(|i| *i < self.groups.len()) {
                    // Press centres a tile; press the centred tile to open it.
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

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let mut hints = vec![Hint::new(HintKey::Confirm, "Open")];
        // Same condition the press answers, so the legend never advertises a no-op.
        if self.root {
            hints.push(Hint::new(HintKey::Secondary, "All titles"));
        }
        hints.push(Hint::new(HintKey::Shoulders, "Sort"));
        hints.push(Hint::new(HintKey::Back, "Back"));
        hints
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
        // Cached against the scale it will be drawn at; a decode cannot be redone.
        self.art_k = k;
        self.sync(ctx.library);
        let labels: Vec<&str> = SortKey::ALL.iter().map(|s| s.label()).collect();
        let selected = SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
        let strip = Rect::from_xywh(rect.left, rect.top, rect.width(), (TAB_STRIP_H * k) as f32);
        // Same caption as the library bar: four unlabeled pills are not a sort, and
        // this screen and the shelf behind share the key.
        let cap_x = f64::from(strip.left) + EDGE_INSET * k;
        let pills = cap_x + super::library::strip_caption(canvas, fonts, "SORT", strip, cap_x, k);
        self.sort_tabs.render(
            canvas,
            Rect::from_ltrb(pills as f32, strip.top, strip.right, strip.bottom),
            &labels,
            selected,
            false,
            fonts,
            k,
            dt,
        );

        let field = Rect::from_ltrb(rect.left, strip.bottom, rect.right, rect.bottom);
        self.anim
            .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
        self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        if crate::theme::reduce_motion() {
            self.bump = Spring::rest(0.0);
        }
        if !self.entrance_armed {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(
                entrances::CARDS,
                self.cursor.max(0) as usize,
                ctx.t,
            ));
        }
        if self.entrance.is_some_and(|e| e.done(ctx.t)) {
            self.entrance = None;
        }

        let w = f64::from(field.width());
        let tile_w = (TILE_W * k).min(w * 0.8);
        let tile_h = (TILE_H * k).min(f64::from(field.height()) - 40.0 * k);
        let pitch = tile_w + TILE_GAP * k;
        let cx0 = f64::from(field.left) + w / 2.0 + self.bump.pos * k;
        let cy = f64::from(field.top) + f64::from(field.height()) / 2.0;

        self.geom.clear();
        self.geom.resize(self.groups.len(), Rect::new_empty());
        for i in 0..self.groups.len() {
            let d = i as f64 - self.anim.pos;
            if d.abs() > 2.6 {
                continue;
            }
            let f = 1.0 - d.abs().min(1.0);
            let ent = self
                .entrance
                .map_or(EntranceAt::SETTLED, |e| e.at(i, ctx.t));
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let scale = (0.88 + 0.12 * f) * arrive;
            let alpha = (0.78 + 0.22 * f) * ent.fade;
            let cxx = cx0 + d * pitch;
            let cyy = cy + (1.0 - ent.travel) * ENTER_RISE * k;
            let tile = Rect::from_xywh(
                (cxx - tile_w / 2.0) as f32,
                (cyy - tile_h / 2.0) as f32,
                tile_w as f32,
                tile_h as f32,
            );
            self.geom[i] = Rect::from_xywh(
                (cxx - tile_w * scale / 2.0) as f32,
                (cyy - tile_h * scale / 2.0) as f32,
                (tile_w * scale) as f32,
                (tile_h * scale) as f32,
            );
            canvas.save();
            canvas.translate((cxx as f32, cyy as f32));
            canvas.scale((scale as f32, scale as f32));
            canvas.translate((-cxx as f32, -cyy as f32));
            let recede = 1.0 - f;
            // Layer only when alpha or recede does work, and bound it to the tile.
            // Unbounded, `save_layer` allocates a screen-sized offscreen per tile.
            // Outset covers `focus_halo` (4 + 3·10) and `drop_shadow`'s +10 drop.
            let layered = alpha < 0.999 || recede > 0.001;
            let bounds = tile.with_outset(((36.0 * k) as f32, (36.0 * k) as f32));
            if layered {
                let mut lp = crate::theme::layer();
                lp.set_alpha_f(alpha as f32);
                if recede > 0.001 {
                    lp.set_color_filter(skia_safe::color_filters::matrix_row_major(
                        &crate::theme::recede_matrix(recede),
                        None,
                    ));
                }
                canvas.save_layer(
                    &skia_safe::canvas::SaveLayerRec::default()
                        .bounds(&bounds)
                        .paint(&lp),
                );
            }
            crate::theme::focus_halo(canvas, tile, TILE_CORNER as f32, k as f32, f as f32);
            if f > 0.4 {
                crate::theme::drop_shadow(
                    canvas,
                    tile,
                    TILE_CORNER as f32,
                    k as f32,
                    0.45 * f as f32,
                );
            }
            self.draw_tile(canvas, fonts, i, tile, k);
            if layered {
                canvas.restore();
            }
            canvas.restore();
        }

        if self.groups.is_empty() {
            fonts.centered(
                canvas,
                "Nothing to collect yet — this library is still loading.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                f64::from(field.left) + w / 2.0,
                cy,
                w * 0.7,
            );
        }
    }

    fn draw_tile(&self, canvas: &Canvas, fonts: &Fonts, i: usize, rect: Rect, k: f64) {
        let Some(g) = self.groups.get(i) else { return };
        crate::theme::panel(
            canvas,
            rect,
            TILE_CORNER as f32,
            Some(accent(0.16)),
            PanelStroke::Gradient,
            k as f32,
        );
        crate::theme::panel_highlight(canvas, rect, TILE_CORNER as f32, k as f32);

        let pad = 20.0 * k;
        let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);

        // Fixed-size deck, top-left; title runs the full inner width. Scale by the
        // tile, not `k` alone, or a squeezed tile lets the deck hit the title.
        let fk = (f64::from(rect.height()) / TILE_H).min(k);
        let front = Rect::from_xywh(
            l as f32,
            (t + (FAN_H - COVER_H) * fk) as f32,
            (COVER_W * fk) as f32,
            (COVER_H * fk) as f32,
        );
        let rr = RRect::new_rect_xy(
            front,
            (COVER_CORNER * fk) as f32,
            (COVER_CORNER * fk) as f32,
        );
        // A launcher's cover is its brand mark, not a missing poster. Compact gaps:
        // a hole in the middle reads as a draw fault.
        let have: Vec<Face<'_>> = g
            .fan
            .iter()
            .filter_map(|c| match self.art.get(&c.id) {
                Some(img) => Some(Face::Poster(img)),
                None if !c.icon.is_empty() => Some(Face::Launcher(&c.icon)),
                None => None,
            })
            .collect();
        // Never deeper than the group has titles. `have` is compact, so covers fill
        // from the front and ghosts trail.
        let slots = FAN.min(g.count.max(1));
        // Device-pixel floor, same as `panel_highlight`: a sub-pixel hairline smears.
        let hair = fk.max(1.0) as f32;
        // Back to front so `fan[0]` (sort-first, the group's face) lands on top.
        // Ascending order buried it and put the sort's last pick in front.
        for n in (0..slots).rev() {
            canvas.save();
            canvas.concat(&fan_matrix(front, n, fk));
            match have.get(n) {
                Some(face) => {
                    plate(canvas, rr, fk);
                    match face {
                        Face::Poster(img) => draw_cover(canvas, img, front, rr),
                        Face::Launcher(icon) => draw_launcher_face(canvas, icon, front, rr),
                    }
                    // Recede toward the ground (`shade` tracks the palette). A colour
                    // filter would cost a `save_layer` per cover — the deck must not.
                    if n > 0 {
                        canvas.draw_rrect(rr, &fill(crate::theme::shade(0.14 * n as f32)));
                    }
                    // Plate under, ink hairline on top: an edge on both palettes.
                    // Stronger rim on back cards; they need the separation more.
                    canvas.draw_rrect(
                        rr.with_inset((hair / 2.0, hair / 2.0)),
                        &stroke(fg(if n == 0 { 0.18 } else { 0.28 }), hair),
                    );
                }
                // Front slot with no art: finished monogram, not a gap. Art-less ROMs stay this.
                None if n == 0 => {
                    plate(canvas, rr, fk);
                    draw_monogram(canvas, fonts, &g.label, front, rr, hair);
                }
                // Empty silhouette, no plate (nothing to cast a shadow). Keeps deck depth
                // so the tile does not change shape as posters arrive.
                None => {
                    canvas.draw_rrect(rr, &fill(crate::theme::shade(0.10)));
                    canvas.draw_rrect(
                        rr.with_inset((hair / 2.0, hair / 2.0)),
                        &stroke(fg(0.12), hair),
                    );
                }
            }
            canvas.restore();
        }

        // Bottom rail, full inner width — same place the host tile puts name and address.
        let max_w = f64::from(rect.width()) - 2.0 * pad;
        let sub_base = f64::from(rect.bottom) - pad;
        let count = if g.count == 1 {
            "1 title".to_string()
        } else {
            format!("{} titles", g.count)
        };
        fonts.draw_clipped(
            canvas,
            &count,
            l,
            sub_base,
            W::Regular,
            13.0 * k,
            fg(0.55),
            max_w,
        );
        fonts.draw_clipped(
            canvas,
            &g.label,
            l,
            sub_base - 22.0 * k,
            W::Bold,
            23.0 * k,
            fg(1.0),
            max_w,
        );
        // Platform vs store, so two "Steam" buckets stay distinct. Top-right, the
        // corner the deck leaves clear.
        let kind = match &g.key {
            GroupKey::Launchers => "LAUNCHERS",
            GroupKey::Platform(_) => "PLATFORM",
            GroupKey::Store(_) => "STORE",
        };
        // `draw_tracked` tracks after every character including the last, so the ink
        // ends one gap short of the pen — `n - 1` when hanging off the right edge.
        let track = 1.4 * k;
        let kind_w = f64::from(fonts.measure(kind, W::SemiBold, 11.0 * k))
            + track * (kind.chars().count().saturating_sub(1)) as f64;
        // Never left of the deck's reserved corner: a narrow tile would otherwise
        // walk the caption back into the covers.
        let kind_x = (f64::from(rect.right) - pad - kind_w).max(l + (FAN_W + 10.0) * fk);
        fonts.draw_tracked(
            canvas,
            kind,
            kind_x,
            t + 12.0 * k,
            W::SemiBold,
            11.0 * k,
            track,
            fg(0.45),
        );
    }
}

/// Transform that places card `n` relative to the front card's rect.
///
/// Pure so the geometry can be asserted without a GPU. The trap is the deck
/// growing past the box it reserves and hitting the title under it.
fn fan_matrix(front: Rect, n: usize, k: f64) -> Matrix {
    let n = n as f64;
    let s = (1.0 - FAN_SCALE_STEP * n) as f32;
    let pivot = Point::new(front.center_x(), front.center_y());
    let mut m = Matrix::translate(((n * FAN_DX * k) as f32, (n * FAN_DY * k) as f32));
    m.pre_rotate((FAN_ROT_DEG * n) as f32, pivot);
    m.pre_scale((s, s), pivot);
    m
}

/// Contact shadow under one card. Drawn, not sampled from the cover.
fn plate(canvas: &Canvas, rr: RRect, k: f64) {
    // Black is weight on a dark field and dirt on a pale one. `theme::drop_shadow`
    // scales back at the pale pole; ignoring that smears every pair of covers.
    let alpha = if crate::theme::ink().scrim.r > 0.5 {
        PLATE_ALPHA * 0.40
    } else {
        PLATE_ALPHA
    };
    canvas.draw_rrect(
        rr.with_outset(((PLATE_OUTSET * k) as f32, (PLATE_OUTSET * k) as f32))
            .with_offset(((PLATE_DX * k) as f32, (PLATE_DY * k) as f32)),
        &fill(Color4f::new(0.0, 0.0, 0.0, alpha)),
    );
}

/// What one slot of the deck draws.
enum Face<'a> {
    /// Decoded poster, adopted from the shelf that opened this screen.
    Poster(&'a Image),
    /// Brand mark from the icon key. Not a missing poster — a launcher has no cover art.
    Launcher(&'a str),
}

/// Launcher card: the library placeholder's brand face, mark centred. Same recipe
/// as `screens::library::draw_poster_placeholder` so one launcher is not two colours.
fn draw_launcher_face(canvas: &Canvas, icon: &str, front: Rect, rr: RRect) {
    canvas.draw_rrect(rr, &fill(crate::theme::card_face(0.38)));
    // ~44 % of the card; `launcher_mark` letterboxes, so a non-square master is not stretched to 2:3.
    let side = front.width().min(front.height()) * 0.44;
    let box_ = Rect::from_xywh(
        front.left + (front.width() - side) / 2.0,
        front.top + (front.height() - side) / 2.0,
        side,
        side,
    );
    if let Some(path) = crate::launcher_icons::launcher_mark(icon, box_) {
        canvas.draw_path(&path, &fill(fg(0.85)));
    }
}

/// Centre-crop to the card's 2:3 and fill the round-rect with one shader.
///
/// Not `clip_rrect` + `draw_image_rect`: a rotated round-rect clip is no longer
/// axis-aligned and falls back to a clip mask (three covers × five tiles = fifteen
/// masks a frame). If this goes back to a clip, `FAN_ROT_DEG` must go to zero with it.
fn draw_cover(canvas: &Canvas, img: &Image, front: Rect, rr: RRect) {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let aspect = front.width() / front.height();
    let src = if iw / ih > aspect {
        let sw = ih * aspect;
        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
    } else {
        let sh = iw / aspect;
        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
    };
    let (sx, sy) = (front.width() / src.width(), front.height() / src.height());
    let mut local = Matrix::scale((sx, sy));
    local.post_translate((front.left - src.left * sx, front.top - src.top * sy));
    let Some(shader) = img.to_shader(
        (TileMode::Clamp, TileMode::Clamp),
        art_sampling(),
        Some(&local),
    ) else {
        return;
    };
    // Opaque: Skia modulates a shader by the paint's alpha, so a transparent
    // placeholder here draws nothing.
    let mut p = crate::theme::shaded();
    p.set_shader(shader);
    canvas.draw_rrect(rr, &p);
}

/// Front card when the group has no art: initials on an accent-tinted face.
fn draw_monogram(canvas: &Canvas, fonts: &Fonts, label: &str, front: Rect, rr: RRect, hair: f32) {
    // Accent-tinted face, not a fixed near-black: `fg()` is itself near-black on
    // pale palettes, so a dark face and a dark glyph vanish into each other.
    canvas.draw_rrect(rr, &fill(accent(0.20)));
    canvas.draw_rrect(
        rr.with_inset((hair / 2.0, hair / 2.0)),
        &stroke(accent(0.5), hair),
    );
    let mono = initials(label);
    // Sized off the card, not `k`: deck cards are smaller than a shelf poster.
    let size = f64::from(front.height()) * 0.30;
    let font = fonts.font(W::Bold, size);
    let tw = font.measure_str(&mono, None).0;
    canvas.draw_str(
        &mono,
        Point::new(
            front.center_x() - tw / 2.0,
            front.center_y() + (size * 0.36) as f32,
        ),
        &font,
        &fill(fg(0.85)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostRow {
        HostRow {
            key: "aa".into(),
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 9778,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_profile: None,
        }
    }

    /// Two platforms of four titles — more per group than [`FAN`], so the queue
    /// keeps covers no tile draws.
    fn two_platforms() -> LibraryShared {
        let library = LibraryShared::default();
        library.set_games(
            (0..8)
                .map(|i| LibraryGame {
                    id: format!("g{i}"),
                    title: format!("Game {i}"),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: Some(if i < 4 { "PS2".into() } else { "PS3".into() }),
                    running: false,
                })
                .collect(),
        );
        let bytes = {
            let mut surface =
                skia_safe::surfaces::raster_n32_premul((6, 9)).expect("a raster surface");
            surface.canvas().clear(Color4f::new(0.2, 0.4, 0.6, 1.0));
            surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .expect("a PNG encoder")
                .as_bytes()
                .to_vec()
        };
        for i in 0..8 {
            library.push_art(format!("g{i}"), bytes.clone());
        }
        library
    }

    /// As the library's root this screen feeds itself — and takes only the covers it fans.
    ///
    /// Poster bytes are pushed once per fetch and never re-sent. A wholesale drain
    /// would look right here and leave the next shelf with monograms.
    #[test]
    fn as_the_librarys_root_it_takes_only_the_covers_it_fans() {
        let library = two_platforms();
        let mut s = CollectionsScreen::new(&host(), SortKey::HostOrder);
        s.own_library();
        // Bounded per frame, like the shelf's pump: several frames to take six covers.
        for _ in 0..8 {
            s.sync(&library);
        }
        // Poster-backed slots only: a launcher's card is drawn from its icon, never fetched.
        let mut fanned: Vec<String> = s
            .groups
            .iter()
            .flat_map(|g| g.fan.iter())
            .filter(|c| c.icon.is_empty())
            .map(|c| c.id.clone())
            .collect();
        fanned.sort();
        assert_eq!(
            fanned.len(),
            2 * FAN,
            "the fixture stopped exercising the deck"
        );
        let mut decoded: Vec<String> = s.art.keys().cloned().collect();
        decoded.sort();
        assert_eq!(decoded, fanned, "it decoded something it never draws");
        let left: Vec<String> = library
            .drain_art(99)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            left,
            ["g3", "g7"],
            "the covers no tile fans must survive for the shelf"
        );
    }

    /// From a shelf's Y that shelf is still underneath and still draining the queue.
    /// Touching it here would starve it.
    #[test]
    fn a_drill_in_from_a_shelf_leaves_the_queue_alone() {
        let library = two_platforms();
        let mut s = CollectionsScreen::new(&host(), SortKey::HostOrder);
        for _ in 0..8 {
            s.sync(&library);
        }
        assert!(s.art.is_empty(), "it took art the shelf below is fed by");
        assert_eq!(
            library.drain_art(99).len(),
            8,
            "the whole queue is still there"
        );
    }

    /// Y opens the whole library only as root, where that list is otherwise unreachable.
    /// From a shelf's Y it would be a second copy of the screen one Back away.
    #[test]
    fn the_way_to_all_titles_exists_only_where_there_is_no_shelf() {
        let library = two_platforms();
        let mut settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &[],
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        };
        let mut drilled = CollectionsScreen::new(&host(), SortKey::HostOrder);
        let mut fx = Outbox::default();
        assert!(drilled
            .menu(MenuEvent::Secondary, &mut ctx, &mut fx)
            .is_none());
        assert!(fx.nav.is_none(), "Y pushed a shelf that was already below");
        assert!(!drilled
            .hints(&ctx)
            .iter()
            .any(|h| h.key == HintKey::Secondary));

        let mut root = CollectionsScreen::new(&host(), SortKey::HostOrder);
        root.own_library();
        let mut fx = Outbox::default();
        assert!(root.menu(MenuEvent::Secondary, &mut ctx, &mut fx).is_some());
        assert!(
            matches!(&fx.nav, Some(crate::screens::Nav::Push(s)) if matches!(**s, Screen::Library(_))),
            "Y did not open the whole library"
        );
        assert!(root.hints(&ctx).iter().any(|h| h.key == HintKey::Secondary));
    }

    /// Tile at `k`, matching [`CollectionsScreen::render`].
    fn tile(k: f64) -> Rect {
        Rect::from_xywh(100.0, 60.0, (TILE_W * k) as f32, (TILE_H * k) as f32)
    }

    /// Front card of the deck — [`CollectionsScreen::draw_tile`]'s arithmetic.
    fn front_card(rect: Rect, k: f64) -> Rect {
        let pad = 20.0 * k;
        Rect::from_xywh(
            (f64::from(rect.left) + pad) as f32,
            (f64::from(rect.top) + pad + (FAN_H - COVER_H) * k) as f32,
            (COVER_W * k) as f32,
            (COVER_H * k) as f32,
        )
    }

    fn deck_bounds(rect: Rect, k: f64) -> Rect {
        let front = front_card(rect, k);
        let mut b = Rect::new_empty();
        for n in 0..FAN {
            b.join(fan_matrix(front, n, k).map_rect(front).0);
        }
        b
    }

    /// The deck turns and lifts, so its footprint is not the front card. However far
    /// it splays it must stay inside the reserved box and off the title underneath.
    #[test]
    fn the_deck_stays_clear_of_the_tiles_own_text() {
        for k in [0.75, 1.0, 1.5, 2.0, 3.0] {
            let rect = tile(k);
            let (pad, deck) = (20.0 * k, deck_bounds(rect, k));
            let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
            let slack = 0.05;
            assert!(f64::from(deck.left) >= l - slack, "k={k}: {deck:?}");
            assert!(f64::from(deck.top) >= t - slack, "k={k}: {deck:?}");
            assert!(
                f64::from(deck.right) <= l + FAN_W * k + slack,
                "the deck splays past the corner it leaves for the caption at k={k}: {deck:?}"
            );
            assert!(
                f64::from(deck.bottom) <= t + FAN_H * k + slack,
                "k={k}: {deck:?}"
            );
            // Bold 23 sits on `sub_base - 22`; a full em above the baseline bounds the title's ink.
            let title_top = f64::from(rect.bottom) - pad - 22.0 * k - 23.0 * k;
            assert!(
                f64::from(deck.bottom) < title_top,
                "the deck reaches the title at k={k}: {} vs {title_top}",
                deck.bottom
            );
        }
    }

    /// A launcher's slot is a face, not a poster still on its way.
    ///
    /// A launcher has no poster; its cover is a brand mark from the icon key.
    /// Asserted on what the tile carries: a rendered frame cannot tell a mark
    /// that was never asked for from one that failed to draw.
    #[test]
    fn a_launcher_fans_its_mark_and_is_never_fetched() {
        let library = LibraryShared::default();
        library.set_games(vec![
            LibraryGame {
                id: "steam".into(),
                title: "Steam".into(),
                store: "steam".into(),
                launcher: true,
                icon: "steam".into(),
                platform: Some("Launchers".into()),
                running: false,
            },
            LibraryGame {
                id: "g0".into(),
                title: "Game 0".into(),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: Some("PS3".into()),
                running: false,
            },
        ]);
        let mut s = CollectionsScreen::new(&host(), SortKey::HostOrder);
        s.own_library();
        s.sync(&library);

        let launchers = s
            .groups
            .iter()
            .find(|g| g.label == "Launchers")
            .expect("the launcher group");
        let card = launchers.fan.first().expect("one card");
        assert_eq!(
            card.icon, "steam",
            "the fan must carry the icon, or the deck has no way to draw the mark"
        );
        assert!(
            crate::launcher_icons::launcher_mark(&card.icon, Rect::from_xywh(0.0, 0.0, 40.0, 40.0))
                .is_some(),
            "and the icon it carries must actually resolve to a mark"
        );
        // Never fetched: a launcher has no poster, so the request can only fail.
        assert!(
            !s.art.contains_key("steam"),
            "a launcher's cover is drawn, not fetched"
        );
    }

    /// Every back card is smaller, higher, and further right. One cue alone is a
    /// smaller cover.
    ///
    /// Measured on the card's own top edge, not `map_rect`'s bounds. Rotation
    /// makes the axis-aligned box *wider* than the card, so a shrinking deck
    /// reads as a growing one against the bounds.
    #[test]
    fn the_deck_recedes_in_every_cue_at_once() {
        let front = front_card(tile(1.0), 1.0);
        // Top-left and top-right, mapped: their distance is the card's real width.
        let edge = |n: usize| {
            let m = fan_matrix(front, n, 1.0);
            let src = [
                Point::new(front.left, front.top),
                Point::new(front.right, front.top),
            ];
            let mut dst = [Point::new(0.0, 0.0); 2];
            m.map_points(&mut dst, &src);
            let (dx, dy) = (dst[1].x - dst[0].x, dst[1].y - dst[0].y);
            (f64::from(dx).hypot(f64::from(dy)), m.map_rect(front).0)
        };
        let (mut prev_w, mut prev_box) = edge(0);
        for n in 1..FAN {
            let (w, bounds) = edge(n);
            assert!(w < prev_w, "slot {n} did not shrink: {w} vs {prev_w}");
            // The centre is the pivot, so the bounding box is a fair witness for lift/right.
            assert!(
                bounds.center_y() < prev_box.center_y(),
                "slot {n} did not lift"
            );
            assert!(
                bounds.center_x() > prev_box.center_x(),
                "slot {n} did not step right"
            );
            prev_w = w;
            prev_box = bounds;
        }
    }

    fn over(src: Color4f, dst: Color4f) -> Color4f {
        let m = |s: f32, d: f32| s * src.a + d * (1.0 - src.a);
        Color4f::new(m(src.r, dst.r), m(src.g, dst.g), m(src.b, dst.b), 1.0)
    }

    /// WCAG contrast: sRGB to linear, then Rec. 709 relative luminance.
    fn contrast(a: Color4f, b: Color4f) -> f32 {
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let lum = |c: Color4f| 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Initials of a group with no art must read on every palette, not just the dark one.
    ///
    /// A hardcoded near-black face under `fg()` vanishes on pale palettes, where
    /// `fg()` is itself near-black.
    #[test]
    fn the_monogram_reads_on_every_palette() {
        for p in &crate::library::PALETTES {
            crate::theme::set_ink(crate::theme::Ink::of(p));
            let ground = Color4f::new(p.ground.0 as f32, p.ground.1 as f32, p.ground.2 as f32, 1.0);
            // Tile accent over the field, then the badge face, then the glyph. Glass
            // between the first two is omitted: it pushes the backdrop away from the
            // ink at both poles, so this is the harder case.
            let panel = over(accent(0.16), ground);
            let face = over(accent(0.20), panel);
            let glyph = over(fg(0.85), face);
            let c = contrast(glyph, face);
            assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
        }
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
    }
}
