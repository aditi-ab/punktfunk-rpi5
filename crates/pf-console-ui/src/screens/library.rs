//! The game-library screen: the coverflow carousel (spring-chased cursor, perspective
//! card tilt, poster art streaming in) — the original console library, now one screen
//! on the shell's stack. B pops back to the host list; A launches the focused title in
//! the same window. The shell owns the aurora, chrome, and the connecting overlay.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, grid_col_hint, grid_step, initials, step_cursor, store_label, GridDir, GridShape,
    LibraryGame, LibraryPhase, LibraryShared, LibraryView, StepResult, BUMP_C, BUMP_K, BUMP_PX,
    ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, FOCUS_GAP, GRID_GAP, GRID_H, GRID_W, JUMP,
    PERSPECTIVE, POSTER_H, POSTER_W, RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C,
    SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, art_sampling, fg, fill, Fonts, W};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Matrix, Point, RRect, Rect, TileMode, M44};
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
///
/// These are RASTER images now, not encoded bytes with a promise to decode: at the cached
/// size (see [`ART_CACHE_W`]) that is ~0.7 MB each at Deck scale, so this number is also the
/// screen's memory ceiling and not only its re-decode policy.
const ART_BUDGET: usize = 160;
/// The box a poster is KEPT in, in design units — twice the grid cell, scaled by the live
/// `k` at `LibraryScreen::cache_art`.
///
/// The host sends Steam's 600×900 capsule (SteamGridDB portraits run to 1000×1500) and
/// nothing here ever draws one at that size: the largest destination in the crate is the
/// shelf's focused card, `POSTER_W · k`. What the draw actually samples is the mip chain
/// [`crate::theme::art_sampling`] asks for — at a 150 dp grid cell the 150-wide level, at a
/// 220 dp shelf card the levels either side of it — so a cache base of twice the grid cell
/// carries exactly the levels both arrangements read today, out of a quarter of the pixels.
/// Larger would only feed a level 0 no draw reaches; smaller would put the shelf on a
/// MAGNIFIED level, which is the softness this must not trade the stutter for.
const ART_CACHE_W: f64 = GRID_W * 2.0;
const ART_CACHE_H: f64 = GRID_H * 2.0;
/// How many arrived posters are decoded per frame.
///
/// The decode is real work — a 600×900 JPEG is ~10 ms on a Deck core — and it has to happen
/// somewhere; this is the bound on how much of it lands in one frame. Two per frame is 120
/// posters a second, comfortably ahead of what the fetch pool's three workers deliver over a
/// LAN, so the art still streams in as fast as it arrives while no single frame pays more
/// than one dropped frame's worth of it.
const ART_DECODES_PER_FRAME: usize = 2;

/// The size a fetched poster is cached at: fitted into the [`ART_CACHE_W`]×[`ART_CACHE_H`]
/// box at scale `k`, aspect preserved, never enlarged.
///
/// The aspect is the SOURCE's, not the card's. A title with no portrait capsule falls back
/// to its 460×215 header ([`pf_client_core::library::Artwork::poster_candidates`]), and
/// squeezing that into the cards' 2:3 would stretch an image the draw's own centre-crop is
/// already there to handle.
fn art_cache_size(src: (i32, i32), k: f64) -> (i32, i32) {
    let (iw, ih) = (f64::from(src.0), f64::from(src.1));
    if iw <= 0.0 || ih <= 0.0 {
        return src;
    }
    // Never UP: the source is the ceiling on how sharp any of this can be, and storing a
    // small poster large only spends memory to magnify it sooner.
    let s = (ART_CACHE_W * k / iw).min(ART_CACHE_H * k / ih).min(1.0);
    (
        (iw * s).round().max(1.0) as i32,
        (ih * s).round().max(1.0) as i32,
    )
}

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
        // is off-screen by definition, and dropping it costs nothing that was on show. It
        // does not come back, though — the encoded bytes were consumed by the decode — so
        // this is a trim of the coldest, never a cache that refills itself.
        .map(|id| (seen.get(id).copied().unwrap_or(0), id))
        .collect();
    by_age.sort_unstable();
    by_age
        .into_iter()
        .take(live.len() - ART_BUDGET)
        .map(|(_, id)| id.clone())
        .collect()
}

/// The library's ONE waiting state: the shell's spinner with a line of text under it,
/// centred in the field.
///
/// There are two waits here and the user is meant to read them as one. The list is in flight
/// (`LibraryPhase::Loading`), and then the shelf holds — for up to 400 ms — for the first
/// posters, because the entrance exists to show artwork off and fanning open a rank of grey
/// faces is worse than not animating at all (see [`LibraryScreen::arm_entrance`]). The second
/// wait used to draw the FINISHED shelf, which is what made the cards appear, blink out and
/// animate in again. One continuous spinner covers both, so nothing appears until it is the
/// entrance's first frame.
///
/// This replaced a skeleton shelf — placeholder cells in the arrangement the real cards would
/// arrive in, under a travelling sheen. The shape argument for it only ever answered the FIRST
/// wait, and a rank of grey rectangles that then has to be swept away is a second thing to
/// dismiss rather than a preview of the first.
fn draw_loading(canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
    let w = f64::from(rect.width());
    let cx = f64::from(rect.left) + w / 2.0;
    let cy = f64::from(rect.top) + f64::from(rect.height()) / 2.0;
    // The connect takeover's arc, at its size, over its own gap to the line beneath
    // (`shell::overlays::draw_takeover`): the console waits in one shape everywhere. It turns
    // under reduced motion like the other three spinners in the crate do — a frozen arc reads
    // as a hung screen, which is the one thing this must not say.
    fonts.centered(
        canvas,
        "Loading library…",
        W::Regular,
        14.0 * k,
        fg(0.55),
        cx,
        cy + 26.0 * k,
        w * 0.8,
    );
    crate::theme::spinner(canvas, cx, cy - 26.0 * k, 22.0 * k, t);
}

/// How much accent a coverless card's face carries — see [`crate::theme::card_face`], which
/// mixes it into the ground side of the field so the face stays OPAQUE.
///
/// A launcher gets the louder one. Its face is the cue that survives being three cards deep
/// in the coverflow's recede, and it is the whole reason the two faces differ: a launcher
/// without a poster is not a game whose cover failed to load.
const FACE_TINT: f32 = 0.20;
const LAUNCHER_FACE_TINT: f32 = 0.38;

/// The face a coverless card sits on. Its own function because the palette contrast test is
/// an assertion about THIS colour, and a test that re-derived it would only prove itself.
fn placeholder_face(launcher: bool) -> Color4f {
    crate::theme::card_face(if launcher {
        LAUNCHER_FACE_TINT
    } else {
        FACE_TINT
    })
}

/// A cover we have no art for, drawn into `rect`.
///
/// A LAUNCHER without a poster is not a game whose cover failed to load, and must not read
/// like one: it gets the brand-tinted face and its launcher's mark (or name), where a game
/// gets the quieter face and a title monogram. `None` is the stale-order case — a cell whose
/// index no longer names a title.
fn draw_poster_placeholder(
    canvas: &Canvas,
    fonts: &Fonts,
    game: Option<&LibraryGame>,
    rect: Rect,
    k: f64,
) {
    // Solid, never glass: coverflow side cards OVERLAP, so a translucent face would show
    // its neighbour through it. `card_face` gets the accent tint without spending alpha on
    // it, which a plain `accent(0.20)` could not.
    let launcher = matches!(game, Some(g) if g.launcher);
    canvas.draw_rect(rect, &fill(placeholder_face(launcher)));
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
        canvas.draw_path(&path, &fill(fg(0.85)));
        return;
    }
    // Sized off the CARD rather than off `k`: the grid's cells are two-thirds the shelf's,
    // and a monogram scaled for a poster would not fit one.
    //
    // ONE rung for both, the same `fg(0.85)` on the same accent-tinted face the collections
    // deck's monogram now uses — a game's fallback and a group's fallback sit a screen apart
    // and must not read as two different ideas. A game's monogram used to sit at 0.45, which
    // measured 2.5:1 against this face on the pale palettes: what separates a game from a
    // launcher here is the face and the glyph's SIZE, neither of which the palette can take
    // away.
    let (glyph, size) = if game.launcher {
        (
            store_label(&game.store).to_string(),
            f64::from(rect.height()) * 0.067,
        )
    } else {
        (initials(&game.title), f64::from(rect.height()) * 0.115)
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
        &fill(fg(0.85)),
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
    /// Columns the last frame actually drew, `None` until the grid has drawn a frame.
    /// Navigation reads THIS rather than recomputing, so the cursor arithmetic and the
    /// layout can never disagree about the grid's shape.
    ///
    /// Honest about not knowing, because the alternative is worse than a swallowed press:
    /// this used to be seeded at four while a Deck draws seven, so the first press after
    /// entering the grid — before its first frame — moved by a column count nothing had
    /// drawn.
    grid_cols_last: Option<usize>,
    /// The column the user last CHOSE, carried across vertical moves (see
    /// [`crate::library::grid_col_hint`]). Without it, one pass through the two-wide
    /// launcher row would pin the cursor to column 1 for the rest of the library.
    grid_col: usize,
    /// Which axis the boundary recoil deflects along — the grid refuses in two dimensions,
    /// and a vertical thud that nudged the field sideways would read as a mis-move rather
    /// than as an edge.
    bump_vertical: bool,
    /// Posters by game id: RASTER images, decoded once on arrival and reduced to the size
    /// they are drawn at, with their mip chain already built (`Self::cache_art`).
    ///
    /// Every part of that sentence is load-bearing, and the shelf shipped with none of it.
    /// `Image::from_encoded` does not decode — skia-safe's own doc says it defers "until the
    /// image is actually used", and that a purge makes "the next draw of the image have to
    /// re-decode". A grid full of full-resolution posters overruns Skia's GPU resource
    /// budget, so that purge came round every frame and most of the covers on screen were
    /// re-decoded from JPEG, on the render thread, per frame. That was the grid's slideshow,
    /// and it is why it only appeared once the screen was FULL: a half-filled grid fits.
    art: HashMap<String, Image>,
    /// The scale the posters in `art` were decoded for — the last frame's `k`, published by
    /// `render` before it syncs. Kept because the cache is sized against what will be DRAWN,
    /// and `sync` (which also runs from `menu`) has no window of its own to ask.
    ///
    /// A window that GROWS leaves the posters already cached at the smaller size: the
    /// encoded bytes are gone by then, and there is nothing to decode again from. That costs
    /// a re-entry to the shelf to put right, and only after a resolution change.
    art_k: f64,
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
    /// The index the live entrance fans out from. [`Entrance`] keeps its own copy and asks
    /// for a distance in ITEMS; the grid measures that distance in CELLS instead (see
    /// [`Self::entrance_at`]), so it needs the anchor back.
    entrance_anchor: usize,
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
            grid_cols_last: None,
            grid_col: 0,
            bump_vertical: false,
            art: HashMap::new(),
            // Seated at the design scale; the first frame publishes the real one, and it
            // does so before the first `sync` can decode anything against it.
            art_k: 1.0,
            art_seen: HashMap::new(),
            frame: 0,
            entrance: None,
            entrance_armed: false,
            entrance_anchor: 0,
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
        // An EMPTY shelf — a collection whose filter matches nothing — has no art coming and
        // nothing to fan, so it must not spend the deadline waiting for either. Without this
        // it holds the spinner for 400 ms over a shelf that was never going to have a card.
        if have_art || self.len() == 0 || t - since >= 0.4 {
            self.entrance_armed = true;
            self.entrance_anchor = cursor;
            self.entrance = Some(Entrance::new(entrances::CARDS, cursor, t));
        }
    }

    /// One item's place in the entrance, `steps` of fan away from the anchor.
    ///
    /// [`Entrance`] measures the fan as the distance between two INDICES, which is the right
    /// unit for a strip and the wrong one for a field: the stagger caps five steps out, so a
    /// seven-wide grid indexed linearly exhausts the whole fan inside its first row and every
    /// cell after that starts at once. The grid passes a distance in CELLS instead and the
    /// entrance opens diagonally out from the focused cover.
    ///
    /// THREE states in two variables, and reading two of them as one is what made the shelf
    /// flash: `entrance == None` means "already over" only once the entrance has been ARMED.
    /// Before that it means "not begun", and answering `SETTLED` there drew the finished shelf
    /// for as long as the arming waited on art — then the entrance armed and every card blinked
    /// out to play it. Nothing draws while un-armed today ([`Self::render`] shows the spinner
    /// instead), and this is the same answer stated where the state actually lives.
    fn entrance_at(&self, steps: usize, t: f64) -> EntranceAt {
        match self.entrance {
            Some(e) => e.at(self.entrance_anchor + steps, t),
            None if self.entrance_armed => EntranceAt::SETTLED,
            None => EntranceAt {
                travel: 0.0,
                fade: 0.0,
            },
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

    /// The grid as the last frame drew it — the shape navigation and the renderer share, and
    /// the reason they can no longer disagree about which cover the cursor is on. `None`
    /// until the grid has drawn once: the column count is a fact about the window, and
    /// inventing one is exactly the divergence this exists to prevent.
    fn grid_shape(&self) -> Option<GridShape> {
        Some(GridShape::new(
            self.len(),
            self.grid_cols_last?,
            self.launcher_count(),
        ))
    }

    /// Re-seat the remembered column on wherever the cursor now is. For everything that
    /// moves the cursor without CHOOSING a column — a re-sort, a press, a resize — where it
    /// landed is the only honest answer to "which column would Up and Down return to".
    fn seat_grid_col(&mut self) {
        if let Some(shape) = self.grid_shape() {
            self.grid_col = shape.cell_of(self.cursor.max(0) as usize).1;
        }
    }

    /// Rebuild the display order after anything that could change it: a new game list, a
    /// new sort, a new filter. The cursor is clamped rather than followed by identity — a
    /// re-sort moves everything, so "keep the same index" and "keep the same title" are
    /// both arbitrary, and the cheap one at least never points off the end.
    fn recollate(&mut self) {
        self.view = crate::collate::filtered(&self.games, self.sort, self.filter.as_ref());
        self.cursor = self.cursor.clamp(0, (self.view.len() as i32 - 1).max(0));
        self.seat_grid_col();
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

    /// Inherit posters that are already decoded, from the screen that pushed this one.
    ///
    /// The counterpart to `CollectionsScreen::adopt_art`, and the same reasoning running the
    /// other way. The shared model's art queue is DRAINED by whoever reads it first, so by the
    /// time a collection drill-in builds its shelf the bytes are long gone — the drill-in would
    /// sit out the entrance's whole art deadline and then show monogram cards permanently, for
    /// covers the console had decoded two screens ago and is still holding.
    pub(crate) fn adopt_art(&mut self, art: HashMap<String, Image>) {
        self.art = art;
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
        // A bounded handful per frame: this is where the decode happens now, and a burst of
        // arrivals must not become one long frame (see `ART_DECODES_PER_FRAME`).
        let k = self.art_k;
        for (id, bytes) in shared.drain_art(ART_DECODES_PER_FRAME) {
            let poster = Image::from_encoded(Data::new_copy(&bytes))
                .and_then(|img| Self::cache_art(&img, k));
            match poster {
                Some(img) => {
                    self.art.insert(id, img);
                }
                None => tracing::debug!(%id, "undecodable poster"),
            }
        }
    }

    /// One arrived poster, reduced to what the console will actually sample.
    ///
    /// Forces the decode HERE — the whole point (see the `art` field) — and does it at
    /// `art_cache_size`, which is a quarter of the pixels a Steam capsule arrives with and
    /// the difference between a screenful of posters fitting Skia's GPU budget and thrashing
    /// it. The mip chain is baked at the same time, so even a purge under some other screen's
    /// pressure costs an upload rather than a decode or a regeneration pass.
    fn cache_art(img: &Image, k: f64) -> Option<Image> {
        let want = art_cache_size((img.width(), img.height()), k);
        let scaled = if want == (img.width(), img.height()) {
            None
        } else {
            // The destination's own info: the console's render targets are `new_n32_premul`
            // with no colour space (`skia_overlay::ensure_slot`), so this is the format the
            // poster is headed for and no conversion happens at draw time either way.
            let info = skia_safe::ImageInfo::new_n32_premul(want, None);
            img.make_scaled(&info, art_sampling())
        };
        // A scale Skia refused leaves the poster at full size — the old behaviour for one
        // cover, rather than no cover.
        let out = scaled.unwrap_or_else(|| img.clone());
        let mipped = out.with_default_mipmaps();
        Some(mipped.unwrap_or(out))
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
        // The grid's shape is a RENDER fact (it depends on the window), so navigation reads
        // the last frame's. One frame of staleness after a resize is invisible; deriving it
        // twice from two widths would not be — and before the first grid frame there is no
        // grid to move in, so the press is dropped rather than answered by a guess.
        let shape = self.grid_shape()?;
        match grid_step(self.cursor, shape, self.grid_col, dir) {
            StepResult::Moved(c) => {
                self.grid_col = grid_col_hint(shape, self.grid_col, dir, c);
                self.cursor = c;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                // Against the push, along the axis it was pushed on. The old sign expression
                // deflected only Right and Down and left every other refusal with a recoil of
                // exactly zero — a refused press that moves nothing on screen is what makes an
                // edge read as a dropped input.
                let forward = matches!(dir, GridDir::Right | GridDir::Down | GridDir::PageForward);
                self.bump = Spring {
                    pos: -BUMP_PX * if forward { 1.0 } else { -1.0 },
                    vel: 0.0,
                };
                self.bump_vertical = !matches!(dir, GridDir::Left | GridDir::Right);
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
                        // A press CHOOSES a cell, and with it the column Up and Down will
                        // return to — the alternative is a grid that walks back to wherever
                        // the stick last was.
                        self.seat_grid_col();
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
                self.bump_vertical = false; // the shelf is a line, and it runs across
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
    ///
    /// Through [`Self::focused`], because the cursor indexes the DISPLAY order: reading
    /// `games` with it agrees only under the default sort, and told you "Open" over a game
    /// under any other one.
    fn focused_is_launcher(&self) -> bool {
        self.focused().is_some_and(|g| g.launcher)
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
        // Published before the sync that reads it: the poster cache is sized against the
        // scale its covers will be drawn at, and a decode is not something this can redo.
        self.art_k = k;
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
                if !self.entrance_armed {
                    // Still holding for the first posters, so the shelf has NOT appeared yet.
                    // Drawing it settled here is what made the cards flash: the finished
                    // coverflow sat on glass for up to 400 ms, blinked out the frame the
                    // entrance armed, and only then animated in. Same spinner as the list
                    // wait, so the two waits have no seam between them.
                    //
                    // The geometry has to go with them — `pointer` reads `geom` a frame late,
                    // and a press must not land on a card nothing drew. `grid_cols_last` is
                    // deliberately left alone: it is what the last frame DREW, no frame has
                    // drawn a grid yet, and `grid_move` already declines rather than guessing.
                    self.geom.clear();
                    draw_loading(canvas, rect, k, fonts, ctx.t);
                    return;
                }
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
            // The same spinner the Ready arm shows while it holds for art — one wait, drawn
            // one way, so the list landing does not change what the screen is saying.
            LibraryPhase::Loading => draw_loading(canvas, rect, k, fonts, ctx.t),
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
        // Publishing the shape is what navigation waits for. A resize is a DIFFERENT grid, so
        // the column the cursor remembers belonged to the old one and is re-seated on where
        // the cursor actually is — which is also how the very first grid frame seats it.
        if self.grid_cols_last != Some(cols) {
            self.grid_cols_last = Some(cols);
            self.seat_grid_col();
        }
        // The same shape `grid_move` reads. Two copies of this arithmetic — one here, one in
        // the cursor — is what put the focus ring in a different column from the cover the
        // scroll had just brought up.
        let shape = GridShape::new(self.len(), cols, self.launcher_count());
        let (cw, ch) = (GRID_W * k, GRID_H * k);
        let pitch_x = cw + GRID_GAP * k;
        let pitch_y = ch + GRID_GAP * k + GRID_LABEL * k;
        // The launcher prefix keeps its own band, which is how design D4 reads in two
        // dimensions: the shelf says it with a heading that changes as the cursor crosses,
        // a grid says it with a gap and a heading over each half.
        let split_row = (shape.split > 0).then(|| shape.split_row());
        let heading_h = GRID_HEADING * k;
        let row_top = |row: usize| -> f64 {
            let extra = match split_row {
                Some(s) if row >= s => heading_h,
                _ => 0.0,
            };
            row as f64 * pitch_y + extra + if split_row.is_some() { heading_h } else { 0.0 }
        };

        let content_h = row_top(shape.rows().saturating_sub(1)) + ch;
        // The detail band keeps its place at the bottom; the grid scrolls above it.
        let view_h = f64::from(rect.height()) - DETAIL_BAND * k;
        let (focus_row, _) = shape.cell_of(self.cursor.max(0) as usize);
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

        // The boundary recoil, deflected along the axis the refused press pushed on.
        // `draw_carousel` has always drawn this; the grid computed it, integrated it and then
        // drew nothing with it — which is what makes an edge in the field read as a swallowed
        // press rather than as an edge.
        let bump = self.bump.pos * k;
        let (bump_x, scroll) = if self.bump_vertical {
            (0.0, self.scroll.pos - bump)
        } else {
            (bump, self.scroll.pos)
        };

        let grid_w = cols as f64 * pitch_x - GRID_GAP * k;
        let x0 = f64::from(rect.left) + (f64::from(rect.width()) - grid_w) / 2.0 + bump_x;
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
            head(canvas, "LAUNCHERS", y0 + heading_h * 0.62 - scroll);
            head(
                canvas,
                "GAMES",
                y0 + row_top(split_row.expect("checked")) - heading_h * 0.38 - scroll,
            );
        }

        // What a cell can reach BEYOND its berth, and so how far outside the viewport one
        // still has to be drawn: the entrance carries a card up from below its berth, and
        // the focused cell swells 6 %. Only while the entrance is running — once it is over
        // a card sits exactly on its berth, and the cull can be as tight as the clip.
        //
        // It used to keep a whole extra card height at BOTH edges unconditionally, which is
        // a third more posters resident than the screen ever shows. That is a third more of
        // the one resource this screen is short of.
        let reach = if self.entrance.is_some() {
            ENTER_RISE * k + 0.06 * ch
        } else {
            0.0
        };
        // Where the entrance fans FROM, as a cell — the anchor is an index, and a field's
        // idea of "next" is not its list's. See `entrance_at`.
        let (anchor_row, anchor_col) =
            shape.cell_of(self.entrance_anchor.min(self.len().saturating_sub(1)));
        for i in 0..self.len() {
            let (row, col) = shape.cell_of(i);
            let top = y0 + row_top(row) - scroll;
            if top + ch + reach < f64::from(rect.top) || top > y0 + view_h {
                continue; // off-screen: not drawn, and deliberately not art-stamped either
            }
            let ent = self.entrance_at(anchor_row.abs_diff(row) + anchor_col.abs_diff(col), t);
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
            let art = self.art.get(&id);
            let rr = RRect::new_rect_xy(cell, (12.0 * k) as f32, (12.0 * k) as f32);
            // The entrance fade goes through the PAINT wherever a cell is a single draw, and
            // raises a layer only where it has several overlapping pieces to fade as one: a
            // placeholder's face and monogram, or the focused cell's ring and glass (which
            // `theme::panel` mixes itself and has no alpha for). A layer per visible card was
            // thirty-odd offscreen allocations a frame, paid for exactly the second the
            // screen is arriving in.
            let layered = ent.fade < 1.0 && (art.is_none() || f > 0.0);
            if layered {
                canvas.save_layer_alpha_f(cell, ent.fade as f32);
            }
            match art {
                Some(img) => {
                    // Cover-fit: center-crop the source to the cell's 2:3.
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let aspect = cell.width() / cell.height();
                    let src = if iw / ih > aspect {
                        let sw = ih * aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    // One shader-filled round-rect, not a clip plus an image draw — the same
                    // picture out of one draw, with the corner getting real coverage AA
                    // instead of an AA clip, and no clip-stack element per card across a
                    // screenful of them. `collections::draw_cover` is the same move.
                    let (sx, sy) = (cell.width() / src.width(), cell.height() / src.height());
                    let mut local = Matrix::scale((sx, sy));
                    local.post_translate((cell.left - src.left * sx, cell.top - src.top * sy));
                    if let Some(shader) = img.to_shader(
                        (TileMode::Clamp, TileMode::Clamp),
                        art_sampling(),
                        Some(&local),
                    ) {
                        // OPAQUE by construction — Skia modulates a shader by the paint's
                        // alpha, so a transparent placeholder here would draw literally
                        // nothing (theme.rs:52-64). Which is also what makes the fade below
                        // work: the cover is exactly what the paint's alpha scales.
                        let mut p = crate::theme::shaded();
                        p.set_shader(shader);
                        if !layered {
                            p.set_alpha_f(ent.fade as f32);
                        }
                        canvas.draw_rrect(rr, &p);
                    }
                }
                // The placeholder paints a square face and a monogram over it, so it still
                // needs the rounded clip to come out a card.
                None => {
                    canvas.save();
                    canvas.clip_rrect(rr, None, true);
                    draw_poster_placeholder(canvas, fonts, self.game(i), cell, k);
                    canvas.restore();
                }
            }
            // The focus ring goes OUTSIDE the cover, so it reads as a ring around it rather
            // than a border painted onto it.
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
            if layered {
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
            // A strip's fan IS its index distance, so the steps are the distance to the
            // anchor — the same number `e.at(i, t)` used to work out for itself. Through
            // `entrance_at` all the same, so there is one place that knows what an absent
            // entrance means and the coverflow cannot drift from the grid on it.
            let ent = self.entrance_at(i.abs_diff(self.entrance_anchor), t);
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
            // degrees of perspective it misses are invisible on a 10 dp blur.
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
                let mut lp = crate::theme::layer();
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
                    canvas.draw_image_rect_with_sampling_options(
                        img,
                        Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                        crect,
                        art_sampling(),
                        &fill(fg(1.0)),
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
                    &fill(pill),
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
            // The separating veil: an opaque wash toward the GROUND, never whole-card alpha.
            // Hardcoded black cancelled itself out on the six pale palettes — it greyed back
            // the lift `recede_matrix` had just applied and left a receded card muddier than
            // the focused one — so it goes through the scrim, which is black on a dark field
            // and white on a pale one and so pushes the same direction the matrix does.
            if prox > 0.0 {
                canvas.draw_rect(
                    crect,
                    &fill(crate::theme::shade((prox * RECEDE_DIM) as f32)),
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
            last_used: None,
            os: String::new(),
            pin: None,
            bound_profile: None,
        }
    }

    /// A shelf whose list has landed but whose art has not — the state the fix is about.
    fn waiting_shelf() -> LibraryScreen {
        let mut s = LibraryScreen::new(&host());
        s.phase = LibraryPhase::Ready;
        s.games = (0..6)
            .map(|i| LibraryGame {
                id: format!("g{i}"),
                title: format!("Game {i}"),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: None,
            })
            .collect();
        s.recollate();
        s
    }

    /// The shelf must never be drawn ARRIVED before it has arrived.
    ///
    /// `entrance == None` is two states, not one — "not begun" and "already over" — and the
    /// draw code read both as settled. Since the arming deliberately waits for art or 400 ms,
    /// that put the finished coverflow on screen, blinked it out and then played its entrance.
    /// The property is that the first thing a card is ever drawn at is frame one of its own
    /// entrance: invisible, and never full opacity before it.
    #[test]
    fn a_shelf_is_hidden_until_its_entrance_begins_not_settled() {
        let mut s = waiting_shelf();
        // No art, no deadline yet: it declines to arm, and every card reads as absent.
        s.arm_entrance(10.0);
        assert!(!s.entrance_armed, "armed with nothing to show");
        for steps in 0..s.len() {
            let at = s.entrance_at(steps, 10.0);
            assert_eq!(
                (at.travel, at.fade),
                (0.0, 0.0),
                "card {steps} was drawn before the entrance began"
            );
        }
        s.arm_entrance(10.39);
        assert!(!s.entrance_armed, "the deadline is 400 ms");

        // The deadline: it arms, and the very frame it arms on is still invisible.
        s.arm_entrance(10.4);
        assert!(s.entrance_armed);
        assert_eq!(s.entrance_at(0, 10.4).fade, 0.0, "the shelf flashed");

        // …and once it is over, `None` means settled again.
        s.entrance = None;
        assert_eq!(s.entrance_at(3, 99.0), EntranceAt::SETTLED);
    }

    /// Art on any card in the neighbourhood arms it immediately — the deadline is the
    /// fallback, not the path.
    #[test]
    fn decoded_art_arms_the_entrance_without_waiting_out_the_deadline() {
        let mut s = waiting_shelf();
        s.arm_entrance(4.0);
        assert!(!s.entrance_armed);
        let mut surface = skia_safe::surfaces::raster_n32_premul((2, 2)).expect("2×2 raster");
        s.art.insert("g1".into(), surface.image_snapshot());
        s.arm_entrance(4.05);
        assert!(s.entrance_armed, "a decoded poster is the whole point");
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

    /// A title with no cover shows its initials, and that has to hold on ALL THIRTEEN
    /// palettes rather than on the one anybody screenshots.
    ///
    /// The face was a hardcoded near-black under `fg()` ink. On the six PALE palettes `fg()`
    /// is itself a near-black tinted toward the ground, so the two landed on top of each
    /// other — `mint` measured 1.03:1, which is an absent monogram, not a dim one — and it hit
    /// every coverless card in both the grid and the coverflow. Only an assertion that fails
    /// before and passes after is worth writing, and this is that assertion.
    #[test]
    fn a_coverless_card_reads_on_every_palette() {
        for p in &crate::library::PALETTES {
            crate::theme::set_ink(crate::theme::Ink::of(p));
            for launcher in [false, true] {
                let face = placeholder_face(launcher);
                // OPAQUE, and load-bearing: the coverflow's side cards overlap, so any alpha
                // here would show the neighbour through the card in front of it.
                assert_eq!(face.a, 1.0, "{} face is translucent", p.id);
                let c = contrast(over(fg(0.85), face), face);
                assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
            }
        }
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
    }

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

    /// The cache is sized against what is DRAWN, in both directions.
    ///
    /// Too small and a cached poster is being MAGNIFIED onto the shelf's focused card — the
    /// soft, "low-resolution" look the sampling work exists to have removed, and the one way
    /// a memory fix could quietly trade one artefact for another. Too large and it is feeding
    /// a mip level no draw reaches, which is what put a screenful of covers over Skia's GPU
    /// budget in the first place. The source is the ceiling over both: nothing is ever
    /// enlarged, and the aspect is the source's, so a title whose only art is a 460×215
    /// header is not stretched into the cards' 2:3 on its way in.
    #[test]
    fn a_cached_poster_is_bounded_by_the_size_it_is_drawn_at() {
        for k in [0.75, 1.0, 1.25, 1.5, 1.8, 2.7, 3.0] {
            for src in [(600, 900), (1000, 1500), (460, 215), (300, 450), (64, 64)] {
                let (w, h) = art_cache_size(src, k);
                assert!(
                    w <= src.0 && h <= src.1,
                    "{src:?} at k={k} was enlarged to {w}×{h}"
                );
                // The shelf's focused card is the largest a poster is ever drawn.
                let drawn = (POSTER_W * k).min(f64::from(src.0));
                assert!(
                    f64::from(w) >= drawn - 1.0,
                    "{src:?} at k={k} cached {w} px wide, under the {drawn:.0} px drawn"
                );
                assert!(
                    f64::from(w) <= ART_CACHE_W * k + 1.0 && f64::from(h) <= ART_CACHE_H * k + 1.0,
                    "{src:?} at k={k} cached {w}×{h}, outside the cache box"
                );
                let (want, got) = (
                    f64::from(src.0) / f64::from(src.1),
                    f64::from(w) / f64::from(h),
                );
                assert!(
                    (want - got).abs() < 0.02,
                    "{src:?} at k={k} came back {w}×{h} — aspect {got:.3} for a {want:.3} source"
                );
            }
        }
    }

    /// A screenful of covers has to FIT Skia's GPU resource budget. When it doesn't, the
    /// cache evicts a third of them on every submit and the next frame re-decodes them from
    /// JPEG on the render thread — which is not a slow frame, it is a slideshow, and it
    /// arrives as a cliff the moment the grid fills rather than as a gradual sag.
    #[test]
    fn a_screenful_of_covers_fits_the_gpu_budget() {
        // The most cells the grid can put on screen: `grid_cols` clamps at eight columns, and
        // the cull keeps a little over three rows at any scale — `k` is the panel's height
        // over 800, so the row COUNT barely moves with resolution. Six rows is generous.
        const SCREENFUL: usize = 8 * 6;
        // Four bytes a pixel, and a third again for the mip chain `cache_art` bakes in.
        let bytes = |(w, h): (i32, i32)| (w as usize) * (h as usize) * 4 * 4 / 3;
        // Up to a 1440p panel; `skia_overlay::RESOURCE_CACHE_BYTES` names the one case past
        // it (4K fed 1000×1500 art) that the budget does not claim to cover.
        for k in [0.75, 1.0, 1.35, 1.8] {
            for src in [(600, 900), (1000, 1500)] {
                let covers = bytes(art_cache_size(src, k)) * SCREENFUL;
                // The console holds two render targets of the panel's own size as well.
                let targets = 2 * (1280.0 * k) as usize * (800.0 * k) as usize * 4;
                let budget = crate::skia_overlay::RESOURCE_CACHE_BYTES;
                assert!(
                    covers + targets < budget,
                    "{src:?} art at k={k}: {} MB of covers + {} MB of targets over a {} MB budget",
                    covers >> 20,
                    targets >> 20,
                    budget >> 20
                );
            }
        }
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
