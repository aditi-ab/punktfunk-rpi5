//! The library's collections: one tile per group, and the sort that orders them all.
//!
//! This is the drill-in the whole of Part C exists for — group by platform, walk the
//! platforms, pick PS3, see its games. It deliberately borrows the home carousel's tile
//! language rather than inventing a third one: a collection is a place you go, exactly like
//! a host is, and the console should have one idea of what "a place you go" looks like.
//!
//! It owns no model. The groups come from [`crate::collate`] over the library screen's own
//! snapshot, and opening one pushes a `LibraryScreen` with a FILTER rather than a filtered
//! copy of the library — so the art pump, the fetch flow and the shared model never learn
//! that collections exist.
//!
//! It is reached two ways, and the difference is one flag ([`CollectionsScreen::root`]).
//! From a shelf's Y there is a shelf underneath it, holding the covers and waiting for the
//! B. Opened as the host's library — the "Start in collections" setting, where the shelf
//! hands over the moment it knows there is more than one collection
//! (`LibraryScreen::collections_upgrade`) — there is nothing underneath, so this screen
//! feeds itself from the model's poster queue and offers Y as the way to the whole library.

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

// Home's tile metrics, to the pixel. The module doc claims a collection is "a place you go,
// exactly like a host is" — that claim is only checkable if the two tiles are the SAME tile,
// so these are not "close to" home.rs:20-23, they are it.
const TILE_W: f64 = 340.0;
const TILE_H: f64 = 224.0;
const TILE_GAP: f64 = 30.0;
const TILE_CORNER: f64 = 26.0;
/// How many poster covers the deck goes deep. Three is enough to say "this is a shelf of
/// covers" and few enough that they stay recognisable at tile size.
const FAN: usize = 3;

// The box the deck is laid out in, from the tile's padded top-left. Reserved whether or not
// a single cover decoded, so the top-right caption keeps a clear corner and the title below
// keeps a clear rail — `the_deck_stays_clear_of_the_tiles_own_text` holds it to both.
const FAN_W: f64 = 120.0;
const FAN_H: f64 = 130.0;
// The FRONT cover; the other two are this one, smaller. 2:3, the same as the shelf's posters
// and the grid's cells, so a cover here is the shape of the cover it opens.
const COVER_H: f64 = 118.0;
const COVER_W: f64 = COVER_H * 2.0 / 3.0;
const COVER_CORNER: f64 = 11.0;
// How the deck splays: each card further back is smaller, higher, further right and turned a
// few degrees more. Four cues rather than one because any single one is ambiguous at couch
// distance — a smaller card could be a smaller cover, a lifted one a taller cover — but
// smaller AND lifted AND turned is unmistakably "behind".
const FAN_SCALE_STEP: f64 = 0.07;
const FAN_DX: f64 = 18.0;
const FAN_DY: f64 = -7.0;
const FAN_ROT_DEG: f64 = 6.0;
// The contact shadow under each card: a HARD round-rect, grown on every side and dropped
// down-right, never a blur. A blurred shadow per cover would be three `MaskFilter`s per tile
// and fifteen across a live strip, where the whole crate has three blur call sites; at
// 1280x800 from a sofa a 3 px hard band and a 3 px soft one are the same mark.
const PLATE_OUTSET: f64 = 3.0;
const PLATE_DX: f64 = 1.5;
const PLATE_DY: f64 = 2.0;
const PLATE_ALPHA: f32 = 0.38;

pub(crate) struct CollectionsScreen {
    /// Everything needed to build the filtered library this screen pushes into. Copied at
    /// construction from the shelf that opened it, so a collection inherits its host AND
    /// its pinned profile — a drill-in off a pinned card's shelf must still launch the way
    /// that card does.
    host: HostRow,
    cursor: i32,
    anim: Spring,
    bump: Spring,
    sort: SortKey,
    sort_tabs: TabStrip,
    /// Group labels/keys/counts as of the last sync, and the poster ids that fan on each.
    groups: Vec<GroupTile>,
    generation: u64,
    /// Decoded posters, borrowed by id from whatever the library screen already fetched —
    /// this screen never asks for art of its own. A group whose covers have not arrived
    /// yet simply fans nothing and shows its monogram, which is also what a platform of
    /// art-less ROM entries looks like permanently.
    art: HashMap<String, Image>,
    /// The scale `art` was decoded for — the last frame's `k`, published by `render` before
    /// it syncs, exactly as the shelf publishes its own.
    art_k: f64,
    /// This screen stands where the library's SHELF would have stood: it was opened as the
    /// host's library ("Start in collections"), not from a shelf's Y. Two things follow, and
    /// both are wrong the other way round — it feeds itself from the model's poster queue
    /// (there is no shelf beneath it to do that), and it offers the way to the whole library
    /// (there is no shelf beneath it to go back to).
    root: bool,
    geom: Vec<Rect>,
    entrance: Option<Entrance>,
    entrance_armed: bool,
}

struct GroupTile {
    key: GroupKey,
    label: String,
    count: usize,
    /// Up to [`FAN`] titles whose covers fan across the tile.
    fan: Vec<FanCard>,
}

/// One card in a tile's deck — what it takes to DRAW that title's cover without holding the
/// title.
///
/// The id alone was not enough, and a launcher group is where that showed: a launcher has no
/// poster, its cover IS its brand mark, and the mark is drawn from `icon` rather than looked
/// up in the art map. Fanning ids only, the deck found no image for any of them and drew a
/// rank of empty ghosts — reported from a Deck as the Launchers collection showing covers
/// with no logos in them.
struct FanCard {
    id: String,
    /// The launcher's icon key (`steam`, `playnite`, …), empty for an ordinary game. Carried
    /// rather than resolved at collate time because the mark is a PATH built to fit the rect
    /// it is drawn into, and the tile's card rect is not known here.
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
            // Seated at the design scale; the first frame publishes the real one, and it
            // does so before the first `sync` can decode anything against it.
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

    /// Re-derive the tiles when the library moved under us (a rescan, a late fetch) or the
    /// sort changed. Cheap enough to do on a generation bump: collation is one pass and
    /// the tiles hold labels and ids, not games.
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
        // Outside the generation guard: the list settles in one bump and the art streams in
        // for seconds afterwards.
        if self.root {
            self.pump_art(library);
        }
    }

    /// Decode the covers this screen FANS, and only those.
    ///
    /// Only as the library's root, and even then only by name. The shelf drains the model's
    /// queue wholesale because it eventually draws everything in it; this screen draws three
    /// covers per group. The bytes are pushed once per fetch and never re-sent, so a
    /// wholesale drain here would take four hundred posters, keep a dozen, and leave the
    /// shelf a tile opens with monograms for the rest of the session.
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

    /// Stand in for the shelf this screen was opened INSTEAD of — see [`Self::root`].
    pub(crate) fn own_library(&mut self) {
        self.root = true;
    }

    /// Adopt whatever posters the library screen has already decoded. Reading the shared
    /// model's art queue would be wrong on the drill-in path — that queue is drained by the
    /// shelf this screen is standing on — so this takes a snapshot handed down at
    /// construction time instead. As the library's own root there is no shelf to read it
    /// from either, and [`Self::pump_art`] takes over from here.
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

    /// Step the sort and persist it. The shelf reads `library_sort` every frame, so the
    /// collection tiles AND the shelf waiting behind this screen both re-order at once —
    /// which is the point of putting the pills here rather than in a dialog.
    fn step_sort(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = SortKey::ALL;
        let at = all.iter().position(|s| *s == self.sort).unwrap_or(0);
        let next = (at as i32 + delta).rem_euclid(all.len() as i32) as usize;
        self.sort = all[next];
        super::library::store_sort(self.sort, ctx);
        // Force a re-collate: the sort changed, not the library.
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
                // The CURRENT epoch: a drill-in raises no fetch of its own — it filters the
                // list already in the model — so the titles it shows are this epoch's by
                // definition. (`set_filter` marks it drilled, which refuses a second
                // collections hand-over regardless; this just keeps the epoch honest rather
                // than leaning on that.)
                let mut shelf =
                    super::library::LibraryScreen::new(&self.host, ctx.library.fetch_epoch());
                shelf.set_filter(g.key.clone(), g.label.clone());
                // Hand the decoded posters DOWN as well. These are the very covers this
                // screen just fanned on the tile the user pressed; without this the drill-in
                // waits out the art deadline and then shows monograms forever, because the
                // shared queue that fed them was drained by the shelf above.
                shelf.adopt_art(self.art.clone());
                fx.push(Screen::Library(shelf));
                Some(MenuPulse::Confirm)
            }
            // Y is the way to the whole library — and it exists only when this screen IS the
            // library, because then there is no shelf underneath and one alphabetical list
            // of everything would otherwise be reachable only by turning the setting off.
            // Opened from a shelf's own Y, that shelf is one B away and this would be a
            // second, deeper copy of it.
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
                    // The carousel's rule: a press brings a tile to the centre, and a press
                    // on the CENTRED tile opens it.
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
        // Offered on exactly the condition the press answers to, so the legend never
        // advertises a button that would do nothing.
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
        // Published before the sync that reads it: a poster is cached against the scale it
        // will be drawn at, and a decode is not something this can redo.
        self.art_k = k;
        self.sync(ctx.library);
        let labels: Vec<&str> = SortKey::ALL.iter().map(|s| s.label()).collect();
        let selected = SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
        let strip = Rect::from_xywh(rect.left, rect.top, rect.width(), (TAB_STRIP_H * k) as f32);
        // Captioned, the way the library's own bar captions the identical row: four pills
        // reading "Default · A–Z · Platform · Store" are a sort only once something says so,
        // and this screen and the shelf behind it are showing the SAME key.
        let cap_x = f64::from(strip.left) + EDGE_INSET * k;
        let pills = cap_x + super::library::strip_caption(canvas, fonts, "SORT", strip, cap_x, k);
        self.sort_tabs.render(
            canvas,
            Rect::from_ltrb(pills as f32, strip.top, strip.right, strip.bottom),
            &labels,
            selected,
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
            // Raised only when it does something, and bounded to the tile when it does.
            // Unbounded, `save_layer` allocates a SCREEN-SIZED offscreen — five of them a
            // frame here, one of which was pure ceremony: the focused settled tile is at
            // alpha 1.0 with no recede, so its layer composited a full screen to arrive
            // back where it started. The deck below draws a dozen round-rects per tile and
            // every one of them would have been a draw into that offscreen. Same guard the
            // coverflow already uses (screens/library.rs:1119); the outset covers
            // `focus_halo`'s 4 + 3·10 reach and `drop_shadow`'s +10 drop.
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
                canvas.restore(); // layer
            }
            canvas.restore(); // transform
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

        // The deck. It sits in the top-left corner at a FIXED size, and the text below it
        // runs the tile's full inner width — the same composition as a host tile, and the
        // end of a two-column split that let the covers decide where the title started. A
        // group whose posters had decoded got 76 px of title and clipped "PlayStation 3" to
        // "Play…", while the group beside it that had none got twice that, so the strip's
        // text rail was ragged and moved as art streamed in.
        //
        // Scaled by the TILE rather than by `k` alone so that a squeezed tile takes the
        // deck down with it instead of letting it run into the title.
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
        // What each slot actually draws. A launcher has no poster and never will — its cover
        // is its brand mark — so it is a face in its own right rather than a slot still
        // waiting for art. Compacted like the posters: a gap in the middle would read as a
        // rendering fault.
        let have: Vec<Face<'_>> = g
            .fan
            .iter()
            .filter_map(|c| match self.art.get(&c.id) {
                Some(img) => Some(Face::Poster(img)),
                None if !c.icon.is_empty() => Some(Face::Launcher(&c.icon)),
                None => None,
            })
            .collect();
        // Never deeper than the group has titles: a one-game platform must not pretend to
        // be a stack of three. `have` compacts (a missing poster in the middle would read
        // as a rendering bug), so covers fill from the front and ghosts trail behind.
        let slots = FAN.min(g.count.max(1));
        // Never thinner than a device pixel, the same floor `panel_highlight` keeps: a
        // hairline that is the whole separation between two covers cannot be allowed to
        // fade to a grey smear on a 720p panel.
        let hair = fk.max(1.0) as f32;
        // BACK TO FRONT, so `fan[0]` — the sort-first game, the one that represents the
        // group — lands on top and nearest the title. Ascending order buried it at the back
        // and put the sort's LAST pick in the hero position.
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
                    // Back cards move TOWARD THE GROUND — `shade` is black on a dark
                    // palette and white on a pale one, the same rule `recede_matrix`
                    // follows. A colour filter would read better still and costs a
                    // save_layer per cover, which is the one thing this deck must not do.
                    if n > 0 {
                        canvas.draw_rrect(rr, &fill(crate::theme::shade(0.14 * n as f32)));
                    }
                    // The plate under the card and an ink hairline on top of it: a lit edge
                    // over a shadow on a dark palette, a drawn line over one on a pale
                    // palette, and an edge either way. Nothing drew a boundary at all
                    // before, so dark art overlapping dark art merged into one black blob.
                    // Back cards get the stronger rim; they need the separation more.
                    canvas.draw_rrect(
                        rr.with_inset((hair / 2.0, hair / 2.0)),
                        &stroke(fg(if n == 0 { 0.18 } else { 0.28 }), hair),
                    );
                }
                // Slot zero with nothing decoded is the monogram — a platform of art-less
                // ROM entries looks like this permanently, so it has to be a finished thing
                // rather than a gap.
                None if n == 0 => {
                    plate(canvas, rr, fk);
                    draw_monogram(canvas, fonts, &g.label, front, rr, hair);
                }
                // A ghost: the silhouette of a card that has not arrived. No plate — an
                // empty slot casts no shadow — but the deck keeps its depth, so a tile does
                // not change shape as its posters stream in.
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

        // Title and count on the bottom rail at full inner width, exactly where the host
        // tile puts its name and address (home.rs:523-532). 300 px at 1280x800, identical
        // on every tile in the strip whatever decoded.
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
        // What KIND of collection this is, so "Steam" as a platform bucket and "Steam" as
        // a store read as the same thing they are. Top-right, where the host tile keeps its
        // status cluster — the corner the deck deliberately leaves clear.
        let kind = match &g.key {
            GroupKey::Launchers => "LAUNCHERS",
            GroupKey::Platform(_) => "PLATFORM",
            GroupKey::Store(_) => "STORE",
        };
        // `draw_tracked` adds tracking after EVERY character including the last, so the ink
        // ends one gap short of the pen — hence `n - 1` when hanging it off the right edge.
        let track = 1.4 * k;
        let kind_w = f64::from(fonts.measure(kind, W::SemiBold, 11.0 * k))
            + track * (kind.chars().count().saturating_sub(1)) as f64;
        // …and never left of the corner the deck reserves: the tile narrows with the window
        // (`tile_w` is capped at 80 % of the field), and on a narrow one a right-aligned
        // caption would otherwise walk back into the covers.
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

/// Where card `n` of the deck sits, as a transform on the FRONT card's rect.
///
/// A pure function rather than four `canvas` calls inline so the geometry can be asserted
/// without a GPU: the one thing that can go wrong here is the deck growing past the box it
/// reserves and colliding with the title under it.
fn fan_matrix(front: Rect, n: usize, k: f64) -> Matrix {
    let n = n as f64;
    let s = (1.0 - FAN_SCALE_STEP * n) as f32;
    let pivot = Point::new(front.center_x(), front.center_y());
    let mut m = Matrix::translate(((n * FAN_DX * k) as f32, (n * FAN_DY * k) as f32));
    m.pre_rotate((FAN_ROT_DEG * n) as f32, pivot);
    m.pre_scale((s, s), pivot);
    m
}

/// The contact shadow under one card — the depth cue that survives any cover art, because
/// it is not made of cover art.
fn plate(canvas: &Canvas, rr: RRect, k: f64) {
    // Black under a card is WEIGHT on a dark field and DIRT on a pale one, where it is the
    // heaviest mark on the screen. `theme::drop_shadow` scales itself back at the pale pole
    // for exactly that reason; a shadow that ignored the palette would put a grey-brown
    // smear between every pair of covers on `dawn` and `bloom`.
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

/// One cover, centre-cropped to the card's 2:3 and drawn as a single shader-filled
/// round-rect.
///
/// What one slot of the deck draws.
enum Face<'a> {
    /// A decoded poster, adopted from the shelf that opened this screen.
    Poster(&'a Image),
    /// A launcher's brand mark, drawn from its icon key. Not a missing poster — a launcher
    /// entry has no cover art and never will, so this is its finished appearance.
    Launcher(&'a str),
}

/// A launcher's card: the brand face the library's own placeholder uses, with its mark
/// centred on it. Kept in step with `screens::library::draw_poster_placeholder` on purpose —
/// the same launcher appears as a card in the shelf and as a cover in this deck, and two
/// recipes would have it two colours.
fn draw_launcher_face(canvas: &Canvas, icon: &str, front: Rect, rr: RRect) {
    canvas.draw_rrect(rr, &fill(crate::theme::card_face(0.38)));
    // ~44 % of the card, letterboxed by `launcher_mark` itself, so a non-square master keeps
    // its proportions rather than stretching to the 2:3.
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

/// Not `clip_rrect` + `draw_image_rect`, which is what this replaced: it is a save, a clip
/// and a draw where this is one draw, and — the load-bearing part — a rotated round-rect
/// FILL stays on Skia's analytic path, where a rotated round-rect CLIP is no longer
/// device-axis-aligned and falls back to a clip mask. At three covers across five live
/// tiles that is fifteen mask allocations a frame, which would have made the deck's turn
/// unaffordable. If this ever goes back to a clip, `FAN_ROT_DEG` must go to zero with it.
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
    // OPAQUE by construction — Skia modulates a shader by the paint's alpha, so a
    // transparent placeholder here would draw literally nothing (theme.rs:52-64).
    let mut p = crate::theme::shaded();
    p.set_shader(shader);
    canvas.draw_rrect(rr, &p);
}

/// The front card when the group has no art at all: its initials on an accent-tinted face.
fn draw_monogram(canvas: &Canvas, fonts: &Fonts, label: &str, front: Rect, rr: RRect, hair: f32) {
    // The face FOLLOWS THE PALETTE. It used to be a fixed near-black (0.118, 0.118, 0.145)
    // carrying an `fg()` glyph, and on the six pale palettes `fg()` is itself a near-black
    // tinted toward the ground — on `mint` the two composited to 1.03:1, so the monogram was
    // not dim there, it was absent, and had been on every pale palette since they shipped.
    // An accent tint moves with the field and the ink moves the opposite way, so the pair
    // separates at both poles by construction rather than by luck.
    canvas.draw_rrect(rr, &fill(accent(0.20)));
    canvas.draw_rrect(
        rr.with_inset((hair / 2.0, hair / 2.0)),
        &stroke(accent(0.5), hair),
    );
    let mono = initials(label);
    // Off the CARD, not off `k`: the deck's cards are smaller than a shelf poster and a
    // monogram scaled for one would not fit the other.
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
            pin: None,
            bound_profile: None,
        }
    }

    /// Two platforms of four titles each — more per group than the deck's [`FAN`] three, so
    /// some covers belong to no tile and the queue has something to be left with.
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

    /// As the library's root this screen feeds itself — and takes ONLY the covers it fans.
    ///
    /// The poster bytes are pushed once per fetch and never re-sent, so every one taken by a
    /// screen that cannot draw it is a monogram on the shelf a tile opens. A wholesale drain
    /// here would look right on this screen and gut the next one.
    #[test]
    fn as_the_librarys_root_it_takes_only_the_covers_it_fans() {
        let library = two_platforms();
        let mut s = CollectionsScreen::new(&host(), SortKey::HostOrder);
        s.own_library();
        // Bounded per frame, like the shelf's own pump: several frames to take six covers.
        for _ in 0..8 {
            s.sync(&library);
        }
        // Only the poster-backed slots: a launcher's card is drawn from its icon and must
        // never be fetched, which is the other half of what this test is holding.
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

    /// Opened from a shelf's Y, that shelf is still under this screen and still expects the
    /// queue it has been draining all along. Touching it here would starve it.
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

    /// The way to the whole library exists exactly where the whole library is otherwise
    /// unreachable — as the root, with no shelf beneath. Opened from a shelf's Y it would be
    /// a second, deeper copy of the screen one B away.
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

    /// The tile at `k`, laid out the way [`CollectionsScreen::render`] lays it out.
    fn tile(k: f64) -> Rect {
        Rect::from_xywh(100.0, 60.0, (TILE_W * k) as f32, (TILE_H * k) as f32)
    }

    /// The front card of the deck on that tile — [`CollectionsScreen::draw_tile`]'s own
    /// arithmetic, and the only thing the deck's geometry is a function of.
    fn front_card(rect: Rect, k: f64) -> Rect {
        let pad = 20.0 * k;
        Rect::from_xywh(
            (f64::from(rect.left) + pad) as f32,
            (f64::from(rect.top) + pad + (FAN_H - COVER_H) * k) as f32,
            (COVER_W * k) as f32,
            (COVER_H * k) as f32,
        )
    }

    /// The deck's whole footprint, every slot, however many decoded.
    fn deck_bounds(rect: Rect, k: f64) -> Rect {
        let front = front_card(rect, k);
        let mut b = Rect::new_empty();
        for n in 0..FAN {
            b.join(fan_matrix(front, n, k).map_rect(front).0);
        }
        b
    }

    /// The deck turns and lifts, so its footprint is NOT the front card's — the property
    /// that matters is that however far it splays it stays inside the box it reserves and
    /// off the title underneath. Getting this wrong is a collision no screenshot of one
    /// palette would catch and every long group label would.
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
            // Bold 23 sits on `sub_base - 22`; no face's ascent exceeds its em size, so a
            // full em above the baseline is a bound on where the title's ink can start.
            let title_top = f64::from(rect.bottom) - pad - 22.0 * k - 23.0 * k;
            assert!(
                f64::from(deck.bottom) < title_top,
                "the deck reaches the title at k={k}: {} vs {title_top}",
                deck.bottom
            );
        }
    }

    /// A launcher's slot is a FACE, not a slot still waiting for a poster.
    ///
    /// Reported from a Deck: the Launchers collection fanned covers "which don't actually
    /// contain their logos" — blank cards. A launcher entry carries no poster and never will;
    /// its cover is a brand mark drawn from its icon key. The deck only ever looked in the art
    /// map, found nothing for any of them, and drew the ghost that means "not arrived yet".
    ///
    /// Asserted on what the tile CARRIES rather than on pixels: the defect was the fan losing
    /// the icon on its way out of collation, and a rendered frame cannot tell a mark that was
    /// never asked for from one that failed to draw.
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
        // …and it is never queued for a fetch: a launcher has no poster to wait for, so
        // asking for one would be a request that can only ever fail.
        assert!(
            !s.art.contains_key("steam"),
            "a launcher's cover is drawn, not fetched"
        );
    }

    /// Every card further back must be smaller, higher AND further right. One cue alone is
    /// ambiguous at couch distance — a smaller card could be a smaller cover — so the test
    /// is that all three agree, and that none of them ever reverses.
    ///
    /// Measured on the card's OWN top edge, not on `map_rect`'s bounding box. The deck turns
    /// each card a few degrees, and a rotated rectangle's axis-aligned bounds are WIDER than
    /// the rectangle: at slot 1 the card shrinks 78.7 → 73.2 while its bounds grow to 84.2. A
    /// test written against the bounds therefore reads a shrinking deck as a growing one, and
    /// says "did not shrink" about geometry that is doing exactly what it should.
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
            // The centre survives the rotation (it is the pivot), so the bounding box is a
            // fair witness for these two.
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

    /// WCAG's own arithmetic: sRGB to linear, then Rec. 709 relative luminance.
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

    /// A group with no decoded art shows its initials, and that had to be readable on ALL
    /// THIRTEEN palettes rather than on the one anybody screenshots.
    ///
    /// The face used to be a hardcoded near-black under an `fg()` glyph. On the six pale
    /// palettes `fg()` is ITSELF a near-black tinted toward the ground, so the two landed on
    /// top of each other: `mint` measured 1.03:1 — not a dim monogram, an absent one — and
    /// it had been that way since the pale palettes shipped. Only an assertion that fails
    /// before and passes after is worth writing, and this is that assertion.
    #[test]
    fn the_monogram_reads_on_every_palette() {
        for p in &crate::library::PALETTES {
            crate::theme::set_ink(crate::theme::Ink::of(p));
            let ground = Color4f::new(p.ground.0 as f32, p.ground.1 as f32, p.ground.2 as f32, 1.0);
            // The stack the badge actually sits in: the tile's accent tint over the field,
            // then the badge's own face, then the glyph. The glass between the first two is
            // left out deliberately — it is white frost on a pale field and near-black on a
            // dark one, so it pushes the backdrop AWAY from the ink at both poles. Omitting
            // it is the harder case (measured: it costs every palette some margin).
            let panel = over(accent(0.16), ground);
            let face = over(accent(0.20), panel);
            let glyph = over(fg(0.85), face);
            let c = contrast(glyph, face);
            assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
        }
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
    }
}
