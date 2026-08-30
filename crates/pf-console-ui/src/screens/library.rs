//! The game-library screen: the coverflow carousel (spring-chased cursor, perspective
//! card tilt, poster art streaming in) — the original console library, now one screen
//! on the shell's stack. B pops back to the host list; A launches the focused title in
//! the same window. The shell owns the aurora, chrome, and the connecting overlay.

use crate::anim::{approach, entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, grid_col_hint, grid_step, initials, step_cursor, store_label, GridDir, GridShape,
    LibraryGame, LibraryPhase, LibraryShared, LibraryView, Stale, StepResult, BUMP_C, BUMP_K,
    BUMP_PX, ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, FOCUS_GAP, GRID_GAP, GRID_H, GRID_W, JUMP,
    PERSPECTIVE, POSTER_H, POSTER_W, RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C,
    SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, art_sampling, fg, fill, Fonts, EDGE_INSET, W};
use crate::widgets::{TabStrip, TAB_PILL_H, TAB_PILL_TOP, TAB_STRIP_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Matrix, Point, RRect, Rect, TileMode, M44};
use std::collections::HashMap;

/// How wide a margin the grid keeps at each side of the field.
const GRID_MARGIN: f64 = 48.0;
/// Air under each grid row (the cell's own label space — the title itself lives in the
/// shared detail band, so this is breathing room rather than text).
const GRID_LABEL: f64 = 10.0;
/// The band a grid group heading occupies.
const GRID_HEADING: f64 = 30.0;
/// The band the focused title's name occupies under either arrangement. Shrunk from 84 when
/// the store/platform subtitle left: the cover badge already carries that answer, and on a
/// phone the 20 units bought most of a grid row back.
const DETAIL_BAND: f64 = 64.0;
/// The corner on the view/sort bar's own glass, once it has focus.
const BAR_CORNER: f64 = 14.0;
/// Air between the bar and the field under it. The shelf centres its cards and would never
/// notice; the GRID starts its first row at the very top of what it is given, and a rank of
/// covers flush against a band of chrome reads as one clipped object rather than two.
const BAR_GAP: f64 = 12.0;
/// Air between a caption and the pills it names, and between the two groups.
const CAPTION_GAP: f64 = 10.0;
/// How many decoded posters stay resident. Roughly nine grid rows' worth — enough that
/// paging back and forth over the same stretch never re-decodes, small enough that a
/// 400-title library cannot accumulate all of it. See `LibraryScreen::evict_art`.
///
/// These are RASTER images now, not encoded bytes with a promise to decode: at the cached
/// size (see [`ART_CACHE_W`]) that is ~0.7 MB each at Deck scale, so this number is also the
/// screen's memory ceiling and not only its re-decode policy.
const ART_BUDGET: usize = 160;
/// The box a poster is KEPT in, in design units — twice the grid cell, scaled by the live
/// `k` at [`decode_poster`].
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
pub(super) const ART_DECODES_PER_FRAME: usize = 2;

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

/// One arrived poster, decoded and reduced to what the console will actually sample.
///
/// Forces the decode HERE — the whole point (see [`LibraryScreen`]'s `art` field) — and does
/// it at [`art_cache_size`], which is a quarter of the pixels a Steam capsule arrives with
/// and the difference between a screenful of posters fitting Skia's GPU budget and thrashing
/// it. The mip chain is baked at the same time, so even a purge under some other screen's
/// pressure costs an upload rather than a decode or a regeneration pass.
///
/// A free function rather than the shelf's own, because the collections screen decodes the
/// covers it fans when it is the library's root, and two answers to "how big is a cached
/// poster" would be one cache that draws soft and one that thrashes.
pub(super) fn decode_poster(bytes: &[u8], k: f64) -> Option<Image> {
    let img = Image::from_encoded(Data::new_copy(bytes))?;
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

/// "This one is already up on the host" — the Resume affordance, in `rect`'s top-RIGHT corner.
///
/// A badge rather than a changed action label because these tiles have no labels to change: the
/// poster *is* the control. It says `RESUME` rather than `RUNNING` on purpose — the player does not
/// need a status report, they need to know what pressing A will do.
///
/// Top-right because the coverflow's store chip owns top-left; the grid draws no store chip at all,
/// so this is the only mark on its cells and the corner is free either way. `ONLINE_GREEN` is the
/// same semantic green the host presence pip uses — this is a state the host reports, not a
/// Punktfunk surface, and it has to stay distinguishable from the launcher chip, which already owns
/// the brand fill one corner away.
///
/// Opaque fill, like [`StoreBadge(solid:)`] on Apple and for the same reason: the coverflow
/// composites its cards into an offscreen layer, where a translucent pill has no backdrop to sample
/// and would simply reveal the neighbour behind it.
fn draw_running_badge(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64) {
    const LABEL: &str = "RESUME";
    let size = 11.0 * k;
    let tw = fonts.measure(LABEL, W::SemiBold, size) as f64;
    let (bw, bh) = (tw + 16.0 * k, 20.0 * k);
    let pad = 8.0 * k;
    let x = f64::from(rect.right) - pad - bw;
    let y = f64::from(rect.top) + pad;
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(x as f32, y as f32, bw as f32, bh as f32),
            (bh / 2.0) as f32,
            (bh / 2.0) as f32,
        ),
        &fill(crate::theme::ONLINE_GREEN),
    );
    // Near-black on the green rather than the palette's ink: this pill is a fixed colour on
    // every palette, so taking `fg()` would put white text on it under the six pale ones.
    fonts.draw(
        canvas,
        LABEL,
        x + 8.0 * k,
        y + 14.0 * k,
        W::SemiBold,
        size,
        Color4f::new(0.04, 0.10, 0.05, 1.0),
    );
}

/// Persist the library's sort, from wherever it was chosen.
///
/// Deliberately writes the SETTING and nothing else. Every screen that shows the library
/// re-reads `library_sort` each frame (`LibraryScreen::adopt_settings`,
/// `CollectionsScreen::sync`), so one key is the state and the screens are mirrors of it —
/// which is what makes a sort chosen on the shelf true on the collections behind it, and
/// what stops the console from having two answers to "what is the sort".
pub(super) fn store_sort(sort: crate::collate::SortKey, ctx: &mut Ctx) {
    ctx.settings.library_sort = sort.id().to_string();
    ctx.store.save(ctx.settings);
}

/// Persist the library's arrangement. The Settings → Interface row writes this same key, so
/// the row and the bar cannot drift: whichever moved last is what both read next frame.
fn store_view(view: LibraryView, ctx: &mut Ctx) {
    ctx.settings.library_view = view.id().to_string();
    ctx.store.save(ctx.settings);
}

/// The caption over a strip of pills, drawn at `x` on the pill row's own centre line, and
/// the width it took.
///
/// A row reading "Default · A–Z · Platform · Store" only answers "what is the sort" once
/// something names it, and the console now shows that row on two screens. Tracked caps at
/// the section-header rung, so it is the same kind of mark as the shelf's own LAUNCHERS /
/// GAMES headings rather than a label of its own invention.
///
/// The caller adds the returned width to `x` and hands the REST of the band to
/// [`TabStrip::render`], which insets itself from whatever rect it is given — so neither
/// side has to be told how wide the other came out.
/// How wide [`strip_caption`] will draw `label`.
///
/// Separate from the draw because a group anchored to the TRAILING edge has to know its own
/// width before it can decide where to start — and measuring it a second way, by hand, is how
/// the measured edge ends up somewhere the drawn edge is not.
pub(super) fn caption_width(fonts: &Fonts, label: &str, k: f64) -> f64 {
    let size = 11.0 * k;
    // `draw_tracked` adds tracking after every character including the last, so the ink ends
    // one gap short of the pen.
    f64::from(fonts.measure(label, W::SemiBold, size))
        + 1.4 * k * (label.chars().count().saturating_sub(1)) as f64
}

pub(super) fn strip_caption(
    canvas: &Canvas,
    fonts: &Fonts,
    label: &str,
    band: Rect,
    x: f64,
    k: f64,
) -> f64 {
    let size = 11.0 * k;
    let track = 1.4 * k;
    // The pill row's centre, which `TabStrip` seats 2 dp into the band and draws 30 dp tall.
    // The BAND's centre would put the caption half a pill below the labels beside it.
    let baseline = f64::from(band.top) + 17.0 * k + size * 0.36;
    fonts.draw_tracked(
        canvas,
        label,
        x,
        baseline,
        W::SemiBold,
        size,
        track,
        fg(0.45),
    );
    // `draw_tracked` adds tracking after every character including the last, so the ink ends
    // one gap short of the pen.
    f64::from(fonts.measure(label, W::SemiBold, size))
        + track * (label.chars().count().saturating_sub(1)) as f64
}

/// The view/sort bar's own state.
///
/// One struct rather than three fields on the screen because it is one control: the focus is
/// the WHOLE bar, not a cell inside it — while it is up, ◀ ▶ step the sort and the shoulders
/// pick the arrangement — and the two strips own nothing but their sprung highlight, since
/// the persisted settings are the state and these only draw it and hit-test a click.
struct LibraryBar {
    focus: bool,
    /// How present the bar is, 0–1. The bar only APPEARS while it holds the pad (▲ from the
    /// field, "Sort & view" in the legend) — the Apple client's behaviour, adopted after the
    /// always-on band proved too expensive on a phone: it taxed every library visit a strip
    /// of field height to answer a question ("what is the sort") that only matters in the
    /// moment of changing it. Chased toward `focus` each frame; the field takes the band's
    /// room back as this falls.
    reveal: f64,
    sort_tabs: TabStrip,
    view_tabs: TabStrip,
}

impl LibraryBar {
    fn new() -> LibraryBar {
        LibraryBar {
            focus: false,
            reveal: 0.0,
            sort_tabs: TabStrip::new(),
            view_tabs: TabStrip::new(),
        }
    }
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
    /// Whether these titles came off the disk cache rather than off the host, and what to say
    /// about it. Never an error state: a cached library is a working library, and this is
    /// precisely the screen a player wants while a sleeping box boots.
    stale: Stale,
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
    /// This shelf was reached FROM the collections screen — as one group, or as its "All
    /// titles" way back out to the whole library. It is what makes Y refuse: a drill-in from
    /// a drill-in would collate a set that is already one group, and from the unfiltered way
    /// out it would bounce the user between the two screens for ever.
    ///
    /// A superset of `filter.is_some()`, which used to carry this on its own and could not
    /// speak for the unfiltered case.
    drilled: bool,
    /// This shelf has not yet decided whether it should have been the collections screen —
    /// see [`Self::collections_upgrade`]. Consumed exactly once, whichever way the answer
    /// goes, because a rescan that later grows the library to two collections must not yank
    /// the user off a shelf they are already browsing.
    pending_collections: bool,
    /// The model's fetch epoch when this shelf was PUSHED.
    ///
    /// Between the push and the fetch reaching the service thread, the shared model still holds
    /// the PREVIOUS host's titles — ready and all — and deciding the collections entry on those
    /// would open one host's library on another's collections. Comparing the epoch against this
    /// answers "the list I am looking at is mine" exactly.
    ///
    /// 🛑 This used to be a `fetch_seen: bool`, flipped from the render loop on the first frame
    /// that saw a non-`Ready` phase. That only ever worked because a fetch's first act was to
    /// block on the network, so `Loading` stood for hundreds of milliseconds. Once the catalog
    /// cache landed, a warm cache published `Ready` about a millisecond later — inside a single
    /// frame — so the shelf went from the previous host's `Ready` to its own without ever
    /// observing `Loading`, the flag stayed false, and "Start in collections" silently stopped
    /// working for exactly the hosts you had visited before. Do not go back to inferring this
    /// from a phase: a counter cannot be missed the way a passing state can.
    entry_epoch: u64,
    // Navigation: the integer cursor is the authority; the eased position chases it.
    cursor: i32,
    /// Each card's rect as last drawn (axis-aligned, scale applied — the perspective tilt
    /// is a few degrees and well inside a finger's slop), empty for culled cards.
    geom: Vec<Rect>,
    anim: Spring,
    bump: Spring,
    /// Which arrangement is drawing (persisted as `library_view`).
    view_mode: LibraryView,
    /// The view/sort bar over the field. Boxed because `Screen` is moved by value on every
    /// push and pop and the library is already its largest variant: of everything this screen
    /// holds, the bar is the part that is pure chrome, so it is the part that pays for an
    /// indirection rather than the cursor, the display order or the poster cache.
    bar: Box<LibraryBar>,
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
    /// they are drawn at, with their mip chain already built ([`decode_poster`]).
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
    /// `entry_epoch` is [`LibraryShared::fetch_epoch`] read at the moment of the push — BEFORE
    /// the `FetchLibrary` command is queued, which is what makes the comparison exact.
    pub(crate) fn new(host: &HostRow, entry_epoch: u64) -> LibraryScreen {
        LibraryScreen {
            host: host.clone(),
            shared: None, // adopted from Ctx on the first render (the shell owns it)
            generation: u64::MAX,
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            stale: Stale::No,
            view: Vec::new(),
            sort: crate::collate::SortKey::default(),
            filter: None,
            filter_label: None,
            drilled: false,
            pending_collections: true,
            entry_epoch,
            cursor: 0,
            geom: Vec::new(),
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            view_mode: LibraryView::default(),
            bar: Box::new(LibraryBar::new()),
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
    /// new sort, a new filter. The cursor is only CLAMPED here, never followed by identity: on
    /// a user-chosen re-sort "keep the same index" and "keep the same title" are both arbitrary,
    /// and the cheap one at least never points off the end.
    ///
    /// [`Self::sync`] is the exception, and it re-anchors after calling this: a list that moved
    /// on its own (the running-first order arriving with a `/status` read) is not something the
    /// player asked for, so the cursor has to stay on the title it was standing on.
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

    // Which host this shelf belongs to, for the shell's `RefreshRunning` on a stream ending.
    // Three narrow readers rather than exposing `host`: the shell needs to address the host's
    // management API and nothing else, and a `&HostRow` would hand it the pin, the profile and
    // the presence state as well.
    pub(crate) fn host_addr(&self) -> &str {
        &self.host.addr
    }

    pub(crate) fn host_mgmt_port(&self) -> u16 {
        self.host.mgmt_port
    }

    pub(crate) fn host_fp_hex(&self) -> &str {
        &self.host.fp_hex
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
        self.drilled = true;
        self.recollate();
    }

    /// Show the WHOLE library, reached from the collections screen — its "All titles" way
    /// out, and the only shelf that is drilled without being filtered.
    pub(crate) fn all_titles(&mut self) {
        self.drilled = true;
    }

    /// Become the collections screen, if that is what this library was opened as.
    ///
    /// Nothing here can be decided where the screen is pushed: the list arrives from a
    /// worker thread long afterwards, and "has this library more than one collection?" is
    /// only answerable once it has. Nor can it be decided in `render`, which has no way to
    /// navigate. So the shell asks this once a frame, beside the pairing pop it already
    /// makes for the same reason — `Some` means "put this screen where this shelf stands".
    ///
    /// Keeping the shelf as the screen that is PUSHED, and letting it hand over only once it
    /// knows, is what leaves Loading, Empty and the error card's Retry in the one place that
    /// has ever had them.
    ///
    /// No caller yet: the one that belongs here is `Shell::sync`, which asks
    /// the top screen once a frame and swaps the returned screen in where the shelf stands —
    /// guarded on a settled transition, or a swap can land on a push the user has already
    /// reversed with B and put them on the collections instead of home.
    pub(crate) fn collections_upgrade(
        &mut self,
        library: &LibraryShared,
        settings: &pf_client_core::trust::Settings,
    ) -> Option<super::collections::CollectionsScreen> {
        // FIRST, and O(1). This is called from a per-frame sync for the life of the screen,
        // and everything below it collates the whole library — 400 titles, at 60 Hz, for
        // ever, if the decided case is ever allowed to fall through.
        if !self.pending_collections {
            return None;
        }
        if !settings.library_collections || self.drilled {
            self.pending_collections = false;
            return None;
        }
        // Adopt the model NOW rather than at render time: the decision is about the list,
        // and this is the frame it may have landed on.
        self.sync(library);
        // Loading, Empty and Error decide nothing. A fetch that failed keeps this screen and
        // its Retry — and a retry that lands still upgrades, because the pending bit is only
        // spent on a ready library.
        if !matches!(self.phase, LibraryPhase::Ready) {
            return None;
        }
        // Ready on a fetch epoch this shelf was pushed at is the PREVIOUS host's library, still
        // sitting in the model — wait for one that belongs to this shelf.
        if library.fetch_epoch() == self.entry_epoch {
            return None;
        }
        self.pending_collections = false;
        // The same predicate Y and the legend ask, so the three can never disagree about
        // whether there is anything to collect. One collection is not worth a screen: that
        // library opens on its shelf, exactly as it does with the setting off.
        if !crate::collate::worth_browsing(&self.games) {
            return None;
        }
        // From the SETTING, not from `self.sort`: `adopt_settings` runs from `menu` and
        // `render`, and this can fire before either, so the field may still hold the default
        // while the user's stored sort says A–Z.
        let mut screen = super::collections::CollectionsScreen::new(
            &self.host,
            crate::collate::SortKey::parse(&settings.library_sort),
        );
        // It stands in for this shelf now: whatever posters arrived while the list was in
        // flight go with it, and it takes over feeding itself from the model's queue.
        screen.adopt_art(std::mem::take(&mut self.art));
        screen.own_library();
        Some(screen)
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
            let snap = shared.snapshot();
            let (phase, games, generation) = (snap.phase, snap.games, snap.generation);
            // A different SET of titles — a different library. Deliberately not "a different
            // sequence": the shelf re-sorts in place now (a `/status` read moving the running
            // titles to the front), and reading that as a new library would clear every decoded
            // poster and throw the cursor back to the start while the player was standing on a
            // tile. Compared as a multiset of ids, so an ORDER change alone is not freshness.
            let fresh = self.games.len() != games.len() || {
                let mut before: Vec<&str> = self.games.iter().map(|g| g.id.as_str()).collect();
                let mut after: Vec<&str> = games.iter().map(|g| g.id.as_str()).collect();
                before.sort_unstable();
                after.sort_unstable();
                before != after
            };
            // The title under the cursor, so a re-sort can put the cursor back on it rather
            // than on whatever slid into that index. The same "position is a title, not an
            // offset" rule the Apple client remembers a scroll position by, applied where this
            // shell actually needs it: its screen stack survives a stream, so the cursor is
            // never lost to a re-mount — only to the list moving under it.
            let anchor = (!fresh)
                .then(|| self.focused().map(|g| g.id.clone()))
                .flatten();
            self.stale = snap.stale;
            // Whether this is the shelf's FIRST list or a later one — the difference between
            // "the screen just mounted" and "the library moved under it", which `fresh`
            // alone cannot tell apart because a screen that holds no games is different from
            // every list there is.
            let was_empty = self.games.is_empty();
            self.phase = phase;
            self.games = games;
            self.generation = generation;
            if fresh {
                self.cursor = 0;
                self.anim = Spring::rest(0.0);
                self.bump = Spring::rest(0.0);
                // Everything else here is reset on the first list too, but the posters are
                // INHERITED on it: a shelf pushed by the collections screen is handed the
                // covers that screen already decoded, and the queue those bytes came from
                // was emptied long before. Clearing on arrival made the hand-over a silent
                // no-op and left every drill-in on monograms for good. A LATER list is a
                // different library, and does clear.
                if !was_empty {
                    self.art.clear();
                    self.art_seen.clear();
                }
                // A different set of titles is a different shelf, so it gets its own
                // arrival. This is also the FIRST one: the screen mounts on `Loading` with
                // no games, and the list landing is the moment the coverflow appears.
                self.entrance = None;
                self.entrance_armed = false;
                self.ready_at = None;
                // The bar is drawn over a field, so focus cannot be left up there while the
                // field goes back to a spinner — a rescan would otherwise strand the pad on
                // furniture nothing is drawing.
                self.bar.focus = false;
            }
            self.recollate();
            // Put the cursor back on the title it was on. After `recollate`, because the
            // display order is what the cursor indexes — and only when the anchor is still in
            // the (possibly filtered) view, so a collection drill-in that no longer contains it
            // keeps the clamped index `recollate` already chose.
            if let Some(id) = anchor {
                if let Some(i) = self.view.iter().position(|&i| self.games[i].id == id) {
                    self.cursor = i as i32;
                    self.seat_grid_col();
                }
            }
        }
        // A bounded handful per frame: this is where the decode happens now, and a burst of
        // arrivals must not become one long frame (see `ART_DECODES_PER_FRAME`).
        let k = self.art_k;
        for (id, bytes) in shared.drain_art(ART_DECODES_PER_FRAME) {
            match decode_poster(&bytes, k) {
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
        // The bar answers first, ABOVE the phase match: while it holds the pad, Confirm must
        // not be able to reach `ready_action` and start a session on a press the user spent
        // on a sort.
        if self.bar.focus {
            if self.bar_shown() {
                return self.bar_menu(ev, ctx);
            }
            self.bar.focus = false; // the field went away under it
        }
        match &self.phase {
            // The GRID reads the same events through 2-D arithmetic. The shelf is a line, so
            // it has up/down to spare and shoulders that jump along it; the grid spends
            // up/down on rows and gives the shoulders whole pages instead.
            LibraryPhase::Ready if self.view_mode == LibraryView::Grid => match ev {
                MenuEvent::Move(MenuDir::Left) => self.grid_move(GridDir::Left),
                MenuEvent::Move(MenuDir::Right) => self.grid_move(GridDir::Right),
                // Up out of the TOP row reaches the bar — asked of the same shape the
                // renderer laid the field out with, so this is exactly the press that used
                // to do nothing but thud, and never one from the middle of the field.
                MenuEvent::Move(MenuDir::Up) => match self.grid_shape() {
                    Some(shape) if shape.cell_of(self.cursor.max(0) as usize).0 == 0 => {
                        self.focus_bar()
                    }
                    _ => self.grid_move(GridDir::Up),
                },
                MenuEvent::Move(MenuDir::Down) => self.grid_move(GridDir::Down),
                MenuEvent::JumpBack => self.grid_move(GridDir::PageBack),
                MenuEvent::JumpForward => self.grid_move(GridDir::PageForward),
                _ => self.ready_action(ev, fx),
            },
            LibraryPhase::Ready => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                // A coverflow is a line, so up is the direction it has spare — and the bar
                // is drawn directly above it, which is what makes this a move rather than a
                // shortcut worth a hint of its own.
                MenuEvent::Move(MenuDir::Up) => self.focus_bar(),
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

    /// Is the view/sort bar on screen? Only over a drawn field: under `Loading`, an error or
    /// the entrance's art wait the screen is a spinner, and a sort control over nothing is a
    /// lie the pointer would still answer for. One condition, read by the draw, the pad, the
    /// mouse and the legend, so the four cannot disagree about whether the bar is there.
    fn bar_shown(&self) -> bool {
        matches!(self.phase, LibraryPhase::Ready) && self.entrance_armed
    }

    /// Move focus up into the bar.
    fn focus_bar(&mut self) -> Option<MenuPulse> {
        if !self.bar_shown() {
            return None;
        }
        self.bar.focus = true;
        Some(MenuPulse::Move)
    }

    /// What a press means while the bar holds the pad.
    ///
    /// Both controls apply IMMEDIATELY, and both apply by writing the SETTING:
    /// [`Self::adopt_settings`] re-reads it at the top of the very next call — this frame's
    /// `render` — and re-collates or re-seats the scroll from there. So the field re-sorts
    /// and re-arranges under the strip while it is still up, which is the whole point of
    /// putting the controls here rather than on a pushed screen. Assigning `self.sort` or
    /// `self.view_mode` directly would be reverted a frame later AND would leave the
    /// Settings row saying something the screen disagrees with.
    fn bar_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        match ev {
            // ◀ ▶ steps the sort, CLAMPED — the settings screen's own grammar for a value,
            // and the reason a held direction stops at the end rather than wrapping through
            // the whole-file settings writer six times a second.
            MenuEvent::Move(MenuDir::Left) => self.step_sort(-1, ctx),
            MenuEvent::Move(MenuDir::Right) => self.step_sort(1, ctx),
            // The shoulders pick the arrangement outright: there are two of them and two
            // arrangements, so L1 is the shelf and R1 is the grid. They do NOT wrap the way
            // the console's other shoulder pairs do (settings' sections, the collections'
            // sort) — over two values wrapping would make both buttons the same button, and
            // an auto-repeating one would flip the field back and forth while it was held.
            // Jumping the cards is meaningless while the thumb is not steering them.
            MenuEvent::JumpBack => self.step_view(-1, ctx),
            MenuEvent::JumpForward => self.step_view(1, ctx),
            // Down, B and A all mean "done". The value is live the moment it changes, so
            // there is nothing here to confirm and nothing to cancel — and B must land here
            // rather than popping the screen, or leaving the bar would leave the library.
            MenuEvent::Move(MenuDir::Down) | MenuEvent::Back | MenuEvent::Confirm => {
                self.bar.focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
            MenuEvent::Secondary | MenuEvent::Tertiary | MenuEvent::Sector(_) => None,
        }
    }

    fn step_sort(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = crate::collate::SortKey::ALL;
        let at = all.iter().position(|s| *s == self.sort);
        match super::settings::step_option(at, all.len(), delta, false) {
            Some(i) => {
                store_sort(all[i], ctx);
                Some(MenuPulse::Move)
            }
            None => Some(MenuPulse::Boundary),
        }
    }

    fn step_view(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = LibraryView::ALL;
        let at = all.iter().position(|v| *v == self.view_mode);
        match super::settings::step_option(at, all.len(), delta, false) {
            Some(i) => {
                store_view(all[i], ctx);
                Some(MenuPulse::Move)
            }
            None => Some(MenuPulse::Boundary),
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
            // X raises the focused title's OPTIONS — the console's one context menu, the
            // same screen a host tile opens, with the subject swapped. "Copy link" is a row
            // in it rather than this button's whole meaning.
            //
            // It used to copy the link outright, on the argument that a menu holding one row
            // is a press the user pays for nothing. True of a menu that will only ever hold
            // one row; this one is where a title's next action lands, and a console that
            // copies a link on X here while asking for a menu on ▲ two screens away has two
            // idioms for the same verb. The press buys the pattern.
            //
            // X and not ▲: the grid arrangement spends Up on row navigation, so ▲ would open
            // a menu in the shelf and move the cursor in the grid — one gesture meaning two
            // things inside one screen.
            MenuEvent::Tertiary => {
                let g = self.focused()?;
                fx.options(super::options::OptionsScreen::for_game(&self.host, g));
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
            // Reached FROM the collections screen, Y is refused too: a drill-in from a
            // drill-in would collate a set that is already one group, and from the "All
            // titles" way out — which is not filtered, and so cannot be recognised by its
            // filter — it would bounce the user between the two screens for ever.
            MenuEvent::Secondary => {
                if self.drilled || !crate::collate::worth_browsing(&self.games) {
                    return Some(MenuPulse::Boundary);
                }
                let mut screen = super::collections::CollectionsScreen::new(&self.host, self.sort);
                // Hand over the posters already decoded here, so the group tiles fan real
                // covers instead of re-fetching art this screen is holding anyway.
                screen.adopt_art(self.art.clone());
                fx.push(Screen::Collections(screen));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Move(_)
            | MenuEvent::Sector(_)
            | MenuEvent::JumpBack
            | MenuEvent::JumpForward => None,
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
                // The bar's pills are checked BEFORE the field, the order the Collections
                // screen already uses: they sit over it, and a press on a pill is never
                // meant for a card. Only while the bar is drawn — a `TabStrip` keeps the
                // pills it last drew, and a shelf that has gone back to a spinner must not
                // answer for furniture that is no longer on screen.
                //
                // This is also the pointer's only route to a VALUE. Clicking the legend's ▲
                // reaches the bar (the shell turns that hint into a `Move(Up)`), but focus is
                // not a choice — a mouse picks a sort by pressing the pill it wants, which is
                // why both strips hit-test themselves rather than leaning on the legend.
                // …and only while the bar is actually PRESENT: it appears on focus now, so
                // an unfocused library has no pills on screen and none to hit. The `TabStrip`s
                // keep the geometry they last drew, and a press must not land on furniture
                // that has faded out.
                let (sort_hit, view_hit) = if self.bar_shown() && self.bar.focus {
                    (
                        self.bar
                            .sort_tabs
                            .pointer(p)
                            .and_then(|i| crate::collate::SortKey::ALL.get(i).copied()),
                        self.bar
                            .view_tabs
                            .pointer(p)
                            .and_then(|i| LibraryView::ALL.get(i).copied()),
                    )
                } else {
                    (None, None)
                };
                if let Some(sort) = sort_hit {
                    store_sort(sort, ctx);
                    return true;
                }
                if let Some(view) = view_hit {
                    store_view(view, ctx);
                    return true;
                }
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
        // The bar's legend REPLACES the field's rather than extending it: while it holds the
        // pad, Play and Copy link would be advertising presses that go nowhere. Three
        // entries, every one of them true.
        if self.bar.focus && self.bar_shown() {
            return vec![
                Hint::new(HintKey::Adjust, "Sort"),
                Hint::new(HintKey::Shoulders, "View"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        match &self.phase {
            LibraryPhase::Ready => {
                // "Resume" outranks both: what A does to a title that is already up is get you
                // back into it, and that is worth saying before whether it is a launcher —
                // resuming Steam Big Picture is still resuming.
                let mut hints = vec![Hint::new(
                    HintKey::Confirm,
                    match (
                        self.focused().is_some_and(|g| g.running),
                        self.focused_is_launcher(),
                    ) {
                        (true, _) => "Resume",
                        (false, true) => "Open",
                        (false, false) => "Play",
                    },
                )];
                // Only offered when there is something to browse, and never on a shelf the
                // collections screen itself opened — the same two conditions the button
                // checks, so the legend never advertises a press that would only thud.
                if !self.drilled && crate::collate::worth_browsing(&self.games) {
                    hints.push(Hint::new(HintKey::Secondary, "Collections"));
                }
                hints.push(Hint::new(HintKey::Tertiary, "Options"));
                hints.push(Hint::new(HintKey::Shoulders, "Jump"));
                // Named where it leads rather than by the gesture: the bar is already on
                // screen showing both values, and this says which press reaches it. Gated on
                // the bar being drawn, so hint and press agree while the field is a spinner.
                if self.bar_shown() {
                    hints.push(Hint::new(HintKey::Up, "Sort & view"));
                }
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
                // The bar only takes its band off the TOP of the field while it is present
                // (see [`LibraryBar::reveal`]) — hidden, the field keeps the whole rect. The
                // detail band keeps the full rect either way — it is anchored to the bottom —
                // and so does the loading path above, which is centred in a field the bar is
                // not part of.
                let bar_target = if self.bar.focus { 1.0 } else { 0.0 };
                self.bar.reveal = if crate::theme::reduce_motion() {
                    bar_target
                } else {
                    approach(self.bar.reveal, bar_target, dt, 0.10)
                };
                if (self.bar.reveal - bar_target).abs() < 0.005 {
                    self.bar.reveal = bar_target;
                }
                let reveal = self.bar.reveal;
                let bar = Rect::from_ltrb(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.top + (TAB_STRIP_H * k) as f32,
                );
                let field = Rect::from_ltrb(
                    rect.left,
                    rect.top + ((TAB_STRIP_H + BAR_GAP) * k * reveal) as f32,
                    rect.right,
                    rect.bottom,
                );
                match self.view_mode {
                    LibraryView::Shelf => self.draw_carousel(canvas, field, k, fonts, ctx.t),
                    LibraryView::Grid => self.draw_grid(canvas, field, k, fonts, ctx.t),
                }
                // After the cards, like the detail band: it is the screen's readout, and a
                // short window must not let an arriving cover paint over the answer.
                // Faded as a unit while arriving/leaving, with a small rise — the crate's
                // transition grammar. Bounded layer: unbounded would allocate a surface-sized
                // offscreen for a strip of pills (see the twin warning in home.rs).
                if reveal > 0.01 {
                    let bounds = Rect::from_ltrb(
                        bar.left,
                        bar.top - (12.0 * k) as f32,
                        bar.right,
                        bar.bottom + (12.0 * k) as f32,
                    );
                    canvas.save_layer_alpha_f(bounds, reveal as f32);
                    canvas.translate((0.0f32, (-(1.0 - reveal) * 10.0 * k) as f32));
                    self.draw_bar(canvas, bar, k, fonts, dt);
                    canvas.restore();
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

    /// The bar over the field: what this library is sorted by, what it is arranged as, and
    /// the control for both.
    ///
    /// Drawn only while it holds the pad ([`LibraryBar::reveal`]): the field's legend keeps
    /// "▲ Sort & view" up permanently, so the ANSWER is one press away instead of one strip
    /// of always-spent field height — the Apple client's behaviour, adopted for the small
    /// screens where that strip priced out a full grid row. A coverflow under `Platform` and
    /// one under `A–Z` are still the same screen with the cards in a different order; this
    /// band is still the only place that names it (the Collections screen is never offered
    /// to a single-store library at all — [`crate::collate::worth_browsing`]).
    fn draw_bar(&mut self, canvas: &Canvas, bar: Rect, k: f64, fonts: &Fonts, dt: f64) {
        // Focused, the WHOLE band takes an accent WASH — the two groups are one control here
        // (◀ ▶ step the sort, the shoulders pick the arrangement), so a ring around one pill
        // row would name a focus the input model does not have.
        //
        // A wash and nothing else: no border, no halo, no glass. This was a glass panel with
        // a brand hairline and a halo behind it, which is the console's recipe for a floating
        // SURFACE — and a strip of chrome sitting flat in the field is not one. It read as a
        // dark slab with a line round it, worse on the pale palettes where the glass turns to
        // frost over an already-light field. The accent is palette-derived, so at 14 % it is
        // legible at both poles without ever becoming an object.
        if self.bar.focus {
            // Over the PILL ROW's extent, not the band's. `TabStrip` seats its pills
            // `TAB_PILL_TOP` into the band and draws them `TAB_PILL_H` tall, so the content
            // occupies 34 of the band's 46 and the remaining 12 is air below it — air that
            // belongs to the gap before the field, not to this control. Washing the whole band
            // put 2 dp above the pills and 14 below them: a backdrop its own content visibly
            // sat high inside. Padded symmetrically instead, so the wash is centred on the
            // thing it is highlighting at any scale.
            let wash_h = (TAB_PILL_TOP * 2.0 + TAB_PILL_H) * k;
            let wash = Rect::from_ltrb(bar.left, bar.top, bar.right, bar.top + wash_h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(wash, (BAR_CORNER * k) as f32, (BAR_CORNER * k) as f32),
                &fill(accent(0.14)),
            );
        }
        // Two groups, each anchored to the edge it belongs to: the sort LEADS at the same
        // inset as the heading above it, the arrangement TRAILS at the same inset as the
        // controller chip above it — and the shoulders that change it are the trailing pair
        // of buttons, so the hand and the eye agree.
        //
        // The arrangement used to START at the band's midpoint, which is neither centred nor
        // trailing: it read as a control that had been put down wherever there was room. An
        // anchored pair is the same structure the top band already has, so the two rows line
        // up as one piece of chrome rather than three.
        let sorts: Vec<&str> = crate::collate::SortKey::ALL
            .iter()
            .map(|s| s.label())
            .collect();
        let sort_at = crate::collate::SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
        let views: Vec<&str> = LibraryView::ALL.iter().map(|v| v.label()).collect();

        // Each group is caption + gap + pills, measured whole. The gap is spelled out for
        // BOTH rather than left to `TabStrip`'s own leading inset, which only appears when the
        // rect it is handed has slack to spend: the trailing group's rect is exactly as wide
        // as its pills, so it had no slack, no inset, and its caption ended up touching the
        // first pill while the leading group — handed a wide rect — kept a gap by accident.
        let gap = CAPTION_GAP * k;
        let sort_cap = caption_width(fonts, "SORT", k);
        let view_cap = caption_width(fonts, "VIEW", k);
        let sort_pills = crate::widgets::TabStrip::width(&sorts, fonts, k);
        let view_pills = crate::widgets::TabStrip::width(&views, fonts, k);

        let sort_x = f64::from(bar.left) + EDGE_INSET * k;
        // Clamped so a narrow window degrades by crowding the sort's own column rather than
        // sliding off the leading edge: the console's scale follows the window's HEIGHT alone,
        // so a tall narrow window can put these two groups closer than either likes.
        let view_x = (f64::from(bar.right) - EDGE_INSET * k - view_cap - gap - view_pills)
            .max(sort_x + sort_cap + gap + sort_pills + gap);

        strip_caption(canvas, fonts, "SORT", bar, sort_x, k);
        strip_caption(canvas, fonts, "VIEW", bar, view_x, k);
        let sort_left = sort_x + sort_cap + gap;
        let view_left = view_x + view_cap + gap;

        // Both strips are handed a rect exactly as wide as their pills, so neither depends on
        // that inset for its position — the caller places them.
        self.bar.sort_tabs.render(
            canvas,
            Rect::from_ltrb(
                sort_left as f32,
                bar.top,
                (sort_left + sort_pills) as f32,
                bar.bottom,
            ),
            &sorts,
            sort_at,
            false,
            fonts,
            k,
            dt,
        );

        let view_at = LibraryView::ALL
            .iter()
            .position(|v| *v == self.view_mode)
            .unwrap_or(0);
        self.bar.view_tabs.render(
            canvas,
            Rect::from_ltrb(
                view_left as f32,
                bar.top,
                (view_left + view_pills) as f32,
                bar.bottom,
            ),
            &views,
            view_at,
            false,
            fonts,
            k,
            dt,
        );
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
        // `grid_cols` clamps at two columns, so on a narrow-enough viewport (a high-density
        // phone in portrait, where the density floor raises `k` past what the panel width
        // covers) two full-size covers plus margins can overflow the rect and clip at the
        // edges. The covers shrink to fit instead — only ever downward, and only the CELLS:
        // headings and labels keep the design scale, and geometry stays self-consistent
        // because everything below draws and records from these same metrics.
        let fit = ((f64::from(rect.width()) - 2.0 * GRID_MARGIN * k)
            / ((cols as f64 * (GRID_W + GRID_GAP) - GRID_GAP) * k))
            .clamp(0.25, 1.0);
        let (cw, ch) = (GRID_W * k * fit, GRID_H * k * fit);
        let pitch_x = cw + GRID_GAP * k * fit;
        let pitch_y = ch + GRID_GAP * k + GRID_LABEL * k;
        // The launcher prefix keeps its own band, which is how design D4 reads in two
        // dimensions: the shelf says it with a heading that changes as the cursor crosses,
        // a grid says it with a gap and a heading over each half.
        let split_row = (shape.split > 0).then(|| shape.split_row());
        let heading_h = GRID_HEADING * k;
        // The grid's top inset is UNCONDITIONAL, and that is the fix rather than the tidy-up
        // it looks like. It used to be spelled `if split_row.is_some() { heading_h }`, so the
        // one band was doing two jobs — the LAUNCHERS heading, and the air the first row needs
        // above it. A collection filtered to a single platform has no launcher prefix, so the
        // term went to zero and row 0 sat flush against the viewport's clip: the focus scale,
        // the entrance lift and the halo all got sliced off along the top edge. Reported from a
        // Deck as the cards being cut off, and only inside a collection, which is exactly the
        // shape a condition on `split_row` would give it.
        //
        // The second band, before GAMES, stays conditional — that one really is a heading.
        let row_top = |row: usize| -> f64 {
            let section_gap = match split_row {
                Some(s) if row >= s => heading_h,
                _ => 0.0,
            };
            row as f64 * pitch_y + heading_h + section_gap
        };

        // …and the SAME inset again at the end, which is the other half of the same defect.
        // The content used to stop at the last row's card bottom, so at maximum scroll that
        // card sat exactly flush with the viewport's clip and lost its focus scale, its
        // shadow and its label off the bottom edge — reported from a Deck as the grid being
        // cut off at the bottom, once the top had air and the asymmetry became visible. The
        // scroll range is what carries this: `content_h` is the only thing the clamp knows
        // about, so the air has to be part of the content rather than part of the viewport.
        let content_h = row_top(shape.rows().saturating_sub(1)) + ch + heading_h;
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

        let grid_w = cols as f64 * pitch_x - GRID_GAP * k * fit;
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
            // Read out now rather than re-borrowing below: the art stamp at the end of this
            // body needs `&mut self`, so nothing may still be holding a `&LibraryGame` by then.
            let running = game.running;

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
            // The grid draws no store chip, so this is the only mark on a cell — it still goes
            // top-right, because the same badge in two different corners on two arrangements of
            // the same shelf would read as two different badges.
            if running {
                draw_running_badge(canvas, fonts, cell, k);
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
            // Opposite corner from the store chip, so the two never meet in the middle of a
            // card. Inside the recede layer with everything else: a receded neighbour's badge
            // should fade with the card it belongs to, not float over the shelf.
            if game.running {
                draw_running_badge(canvas, fonts, crect, k);
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
        // Says the titles above are remembered rather than observed — shown only while that is
        // true, and never as an error. Leading-aligned on the subtitle's own baseline rather
        // than centred over the title: it describes the SHELF, and a line stacked above the
        // focused title would read as part of it.
        if let Some(note) = self.stale.note() {
            fonts.draw(
                canvas,
                note,
                f64::from(rect.left) + EDGE_INSET * k,
                f64::from(rect.bottom) - 12.0 * k,
                W::Regular,
                12.0 * k,
                fg(0.55),
            );
        }
        let Some(g) = self.focused() else { return };
        let w = f64::from(rect.width());
        let cx = f64::from(rect.left) + w / 2.0;
        // The title alone. The store/platform subtitle that sat under it is gone: the cover
        // badge already names the store, so the line said everything twice and cost the band
        // 20 units of field height on every library visit.
        fonts.centered(
            canvas,
            &g.title,
            W::Bold,
            27.0 * k,
            fg(1.0),
            cx,
            f64::from(rect.bottom) - 34.0 * k,
            w * 0.8,
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
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_profile: None,
        }
    }

    /// A shelf on a LIVE shared model, with its entrance already over.
    ///
    /// Both halves matter for the input tests: `menu` re-syncs from the model on every press,
    /// and a shelf built by hand would be wiped by the first one; the bar exists only over a
    /// drawn field, which off-screen means "the entrance has armed".
    ///
    /// The titles are deliberately not in alphabetical order, so a sort is something the
    /// display order can be asked about rather than a setting that happens to change nothing.
    fn live_shelf() -> (LibraryScreen, LibraryShared) {
        // The bar persists on every step, and `Settings::save` rewrites the whole console
        // settings file — so the store is pointed at a throwaway HOME before any of these
        // tests can reach it. Shared with the settings screen's tests, which need it for the
        // same reason and own the one write to the environment.
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        library.set_games(
            ["Zeta", "Alpha", "Nimbus", "Bravo", "Kilo", "Delta"]
                .iter()
                .enumerate()
                .map(|(i, t)| LibraryGame {
                    id: format!("g{i}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: None,
                    running: false,
                })
                .collect(),
        );
        let mut s = LibraryScreen::new(&host(), 0);
        s.sync(&library);
        s.entrance_armed = true;
        (s, library)
    }

    fn ctx<'a>(
        library: &'a LibraryShared,
        settings: &'a mut pf_client_core::trust::Settings,
    ) -> Ctx<'a> {
        Ctx {
            hosts: &[],
            library,
            settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &[],
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        }
    }

    /// One press, with the settings the test owns standing in for the console's.
    fn press(
        s: &mut LibraryScreen,
        library: &LibraryShared,
        settings: &mut pf_client_core::trust::Settings,
        ev: MenuEvent,
    ) -> (Option<MenuPulse>, Outbox) {
        let mut fx = Outbox::default();
        let pulse = s.menu(ev, &mut ctx(library, settings), &mut fx);
        (pulse, fx)
    }

    fn hint_keys(
        s: &LibraryScreen,
        library: &LibraryShared,
        settings: &mut pf_client_core::trust::Settings,
    ) -> Vec<HintKey> {
        s.hints(&ctx(library, settings))
            .iter()
            .map(|h| h.key)
            .collect()
    }

    /// The bar takes the pad while it is up, and the press that matters is A.
    ///
    /// A Confirm that reached `ready_action` from here would START A SESSION on a button the
    /// user spent choosing a sort — the one mistake this bar could make that costs anything,
    /// and the reason its branch sits above the phase match rather than inside it.
    #[test]
    fn the_bar_owns_the_pad_and_confirm_cannot_launch_from_it() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert!(s.bar.focus, "up from the shelf reaches the bar");

        let cursor = s.cursor;
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(fx.connect.is_none(), "A in the bar launched a game");
        assert!(fx.nav.is_none(), "and pushed a screen");
        assert!(!s.bar.focus, "A means done");
        assert_eq!(s.cursor, cursor, "and the field never moved under it");
    }

    /// Left and right belong to the bar while it has focus — the cards must not walk along
    /// underneath the value the thumb is choosing.
    #[test]
    fn the_field_does_not_move_while_the_bar_is_up() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Right),
        );
        assert_eq!(
            s.cursor, 0,
            "the shelf stepped on a press meant for the sort"
        );
    }

    /// The bar changes the SETTING, never the screen — which is the entire mechanism.
    ///
    /// Every surface that shows the library re-reads `library_sort` each frame, so writing
    /// the key is what makes the field re-order live under the strip AND what stops the
    /// Settings screen from disagreeing with it. Assigning `self.sort` here would look right
    /// for exactly one frame and would then be reverted by the next `adopt_settings`.
    #[test]
    fn stepping_the_bar_writes_the_setting_and_the_field_follows() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Right),
        );
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::Title.id(),
            "the sort is persisted, not held on the screen"
        );

        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.sort, crate::collate::SortKey::Title);
        assert_eq!(
            s.game(0).map(|g| g.title.as_str()),
            Some("Alpha"),
            "the display order did not follow the sort"
        );
        // …and the cursor still indexes the DISPLAY order, so the shelf is showing the
        // first title of the new order rather than a stale index into the old one.
        assert_eq!(s.focused().map(|g| g.title.as_str()), Some("Alpha"));

        // Left goes back, and the first sort is the end of the run: it thuds rather than
        // wrapping round to Store, exactly as a settings value row does.
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Left),
        );
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::HostOrder.id()
        );
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Left),
        );
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::HostOrder.id()
        );
    }

    /// The shoulders pick the arrangement outright, and stop rather than wrap: over two
    /// values a wrapping pair is one button pressed twice, and a held one would flip the
    /// whole field back and forth.
    #[test]
    fn the_shoulders_swap_the_arrangement_and_stop_at_the_ends() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let (pulse, _) = press(&mut s, &library, &mut settings, MenuEvent::JumpForward);
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert_eq!(settings.library_view, LibraryView::Grid.id());

        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.view_mode, LibraryView::Grid);
        assert!(
            s.snap_scroll,
            "the two arrangements do not share a scroll, so the new one seats"
        );

        let (pulse, _) = press(&mut s, &library, &mut settings, MenuEvent::JumpForward);
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "already the grid"
        );
        press(&mut s, &library, &mut settings, MenuEvent::JumpBack);
        assert_eq!(settings.library_view, LibraryView::Shelf.id());
    }

    /// B leaves the bar before it leaves the library. Standard sub-focus behaviour, and the
    /// thing that lets the bar be a place rather than a mode you escape twice by accident.
    #[test]
    fn back_leaves_the_bar_before_it_leaves_the_library() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
        assert!(fx.nav.is_none(), "B in the bar popped the screen");
        assert!(!s.bar.focus);
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Pop)),
            "…and B on the field still leaves"
        );
    }

    /// In the grid, up reaches the bar from the TOP DRAWN row and only from there — anywhere
    /// else it is row navigation, which is the press it must not steal. Asked of the same
    /// [`GridShape`] the renderer lays the field out with, so the launcher split cannot make
    /// the two disagree about which row a cover is on.
    #[test]
    fn up_reaches_the_bar_from_the_grids_top_row_only() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Grid.id().to_string(),
            ..Default::default()
        };
        // The grid's shape is what the last frame DREW, and no test draws one.
        s.grid_cols_last = Some(3);
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Down),
        );
        assert_eq!(s.view_mode, LibraryView::Grid);
        assert_eq!(s.cursor, 3, "down moved a row");

        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert!(!s.bar.focus, "up out of the second row is a row move");
        assert_eq!(s.cursor, 0);

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(s.bar.focus, "…and from the top row it reaches the bar");
    }

    /// Legend and press agree in BOTH focus states. The bar's legend replaces the field's
    /// rather than extending it — while it is up, Play and Copy link would be advertising
    /// presses that go nowhere.
    #[test]
    fn the_legend_swaps_with_the_focus() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        let closed = hint_keys(&s, &library, &mut settings);
        assert!(closed.contains(&HintKey::Up), "nothing leads to the bar");
        assert!(closed.contains(&HintKey::Confirm) && closed.contains(&HintKey::Tertiary));

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let open = hint_keys(&s, &library, &mut settings);
        assert!(open.contains(&HintKey::Adjust) && open.contains(&HintKey::Shoulders));
        assert!(
            !open.contains(&HintKey::Confirm) && !open.contains(&HintKey::Tertiary),
            "the bar's legend still offered the field's actions"
        );
        assert!(
            open.len() <= 4,
            "the legend is read from a sofa: {} entries",
            open.len()
        );
    }

    /// No bar over a spinner. The screen is `Ready` for the whole of the entrance's art
    /// wait, and a sort control over a field that has not appeared is a lie the legend would
    /// advertise and the pointer would answer for.
    #[test]
    fn there_is_no_bar_until_there_is_a_field() {
        let (mut s, library) = live_shelf();
        s.entrance_armed = false;
        let mut settings = pf_client_core::trust::Settings::default();
        assert!(!hint_keys(&s, &library, &mut settings).contains(&HintKey::Up));
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(pulse.is_none(), "the shelf's dead press became a live one");
        assert!(!s.bar.focus);
    }

    /// A shelf whose list has landed but whose art has not — the state the fix is about.
    fn waiting_shelf() -> LibraryScreen {
        let mut s = LibraryScreen::new(&host(), 0);
        s.phase = LibraryPhase::Ready;
        s.games = (0..6)
            .map(|i| LibraryGame {
                id: format!("g{i}"),
                title: format!("Game {i}"),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: None,
                running: false,
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
        // Four bytes a pixel, and a third again for the mip chain `decode_poster` bakes in.
        let bytes = |(w, h): (i32, i32)| (w as usize) * (h as usize) * 4 * 4 / 3;
        // Up to a 1440p panel; `shell::DEFAULT_GPU_CACHE_BYTES` names the one case past
        // it (4K fed 1000×1500 art) that the budget does not claim to cover.
        for k in [0.75, 1.0, 1.35, 1.8] {
            for src in [(600, 900), (1000, 1500)] {
                let covers = bytes(art_cache_size(src, k)) * SCREENFUL;
                // The console holds two render targets of the panel's own size as well.
                let targets = 2 * (1280.0 * k) as usize * (800.0 * k) as usize * 4;
                let budget = crate::shell::DEFAULT_GPU_CACHE_BYTES;
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

    /// Titles by name and platform; a platform-less Steam title collates as its STORE, so
    /// `[None, None]` is one collection and `[Some, None]` is two.
    fn games(spec: &[(&str, Option<&str>)]) -> Vec<LibraryGame> {
        spec.iter()
            .enumerate()
            .map(|(i, (title, platform))| LibraryGame {
                id: format!("g{i}"),
                title: (*title).to_string(),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: platform.map(str::to_string),
                running: false,
            })
            .collect()
    }

    fn setting_on() -> pf_client_core::trust::Settings {
        pf_client_core::trust::Settings {
            library_collections: true,
            ..Default::default()
        }
    }

    /// A freshly pushed shelf that has watched its own fetch go out — the state every entry
    /// is in a frame or two after the push, and the only one the entry decision is taken in.
    /// The model starts `Loading`, which is exactly what a queued fetch puts it back to.
    fn pushed_shelf(
        library: &LibraryShared,
        settings: &pf_client_core::trust::Settings,
    ) -> LibraryScreen {
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        library.begin_fetch(); // the queued fetch reaches the service thread
        s.collections_upgrade(library, settings);
        s
    }

    /// The entry decision, in one place: the setting says yes AND the library has more than
    /// one collection. Either half missing and the shelf stays a shelf — which is what makes
    /// "collections enabled, one collection" a no-op rather than a screen holding one tile.
    ///
    /// One-shot in both directions. A shelf that decided against upgrading must not be asked
    /// again — that answer costs a full collate — and a shelf that decided FOR it must not
    /// hand over twice.
    #[test]
    fn the_shelf_hands_over_only_when_the_setting_and_the_library_agree() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let one = games(&[("Ico", None), ("Journey", None)]);

        let off = pf_client_core::trust::Settings::default();
        let library = LibraryShared::default();
        library.set_games(mixed.clone());
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(
            s.collections_upgrade(&library, &off).is_none(),
            "setting off"
        );
        assert!(!s.pending_collections, "and it is not asked again");

        let library = LibraryShared::default();
        let mut s = pushed_shelf(&library, &setting_on());
        library.set_games(one);
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "one collection is not worth a screen"
        );
        assert!(!s.pending_collections);
        assert!(!s.drilled, "…and the shelf it stayed still offers Y");

        let library = LibraryShared::default();
        let mut s = pushed_shelf(&library, &setting_on());
        library.set_games(mixed);
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "handed over twice"
        );
    }

    /// The model still holds the host the user just backed out of, and it is READY — for the
    /// frames between this shelf being pushed and the fetch it queued reaching the service
    /// thread. Deciding there opens one host's library on another host's collections, and on
    /// the common path (browse a host, back, open the next) it is the normal case, not a
    /// race that needs losing.
    #[test]
    fn a_shelf_never_hands_over_on_the_library_it_inherited() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.set_games(mixed.clone()); // the host the user was just on
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        assert!(
            s.pending_collections,
            "the decision was deferred, not spent"
        );

        library.begin_fetch(); // …and now this shelf's own fetch goes out
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        library.set_games(mixed);
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
    }

    /// 🛑 REGRESSION (C7): a WARM CATALOG CACHE hands the shelf a ready library without any
    /// frame ever seeing `Loading`, and the hand-over must still happen.
    ///
    /// This is the exact sequence a second visit to a host produces once the catalog is cached
    /// on disk: the model holds the previous host's `Ready` list, `begin_fetch` flips it to
    /// `Loading`, and the fetch thread's very first act — a file read, about a millisecond —
    /// publishes the cached catalog as `Ready` again. All of that lands between two 60 Hz
    /// frames, so the render loop observes `Ready` before and `Ready` after and nothing in
    /// between. The old `fetch_seen` flag was set only by *catching* a non-`Ready` phase, so it
    /// stayed false and "Start in collections" silently stopped working — for precisely the
    /// hosts you had opened before, which is everyone's own host.
    ///
    /// Note there is no `collections_upgrade` call between `begin_fetch` and the cached list:
    /// that absence IS the test. Adding one would let a phase-observing implementation pass.
    #[test]
    fn a_warm_cache_still_hands_over_though_no_frame_ever_saw_loading() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.set_games(mixed.clone()); // the previous host, still on screen
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());

        library.begin_fetch();
        library.set_games_cached(mixed); // the disk cache, before the next frame
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_some(),
            "a cached catalog is this shelf's own list and must upgrade"
        );
    }

    /// The other half of that guarantee: an epoch the shelf was pushed at is NOT its own fetch,
    /// however ready the model looks. Without this the fix would just be "always upgrade".
    #[test]
    fn a_cached_list_from_before_the_push_is_still_refused() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.begin_fetch();
        library.set_games_cached(mixed); // the PREVIOUS host, served from ITS cache
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "same epoch as the push — this list belongs to the host we came from"
        );
        assert!(s.pending_collections, "and the decision is still pending");
    }

    /// A fetch that failed keeps the shelf — the error card and its Retry live here and
    /// nowhere else — and the decision is deferred rather than spent, so the retry that
    /// lands still opens on the collections.
    #[test]
    fn a_failed_fetch_keeps_the_shelf_and_still_hands_over_on_retry() {
        let library = LibraryShared::default();
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());

        // The shelf's own fetch goes out and fails.
        library.begin_fetch();
        library.set_phase(LibraryPhase::Error {
            title: "Couldn't load the library".into(),
            body: "refused".into(),
            can_retry: true,
        });
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        assert!(
            s.pending_collections,
            "the decision was deferred, not spent"
        );
        assert!(
            matches!(s.phase, LibraryPhase::Error { .. }),
            "the retry is still here"
        );

        // Retry — a second fetch, so a second epoch — and this one lands.
        library.begin_fetch();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
    }

    /// A shelf the collections screen opened never offers the collections back — filtered to
    /// one group it would collate a set that is already one group, and unfiltered ("All
    /// titles") it would bounce the user between the two screens for ever.
    #[test]
    fn a_drilled_shelf_neither_hands_over_nor_offers_y() {
        let library = LibraryShared::default();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        let mut settings = setting_on();
        for drill in [0, 1] {
            let mut s = LibraryScreen::new(&host(), 0);
            if drill == 0 {
                s.set_filter(
                    crate::collate::GroupKey::Platform("PS2".into()),
                    "PS2".into(),
                );
            } else {
                s.all_titles();
            }
            assert!(s.collections_upgrade(&library, &settings).is_none());
            let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Secondary);
            assert!(matches!(pulse, Some(MenuPulse::Boundary)), "Y was answered");
            assert!(fx.nav.is_none(), "…and pushed a screen");
            assert!(
                !hint_keys(&s, &library, &mut settings).contains(&HintKey::Secondary),
                "the legend offered a press that only thuds"
            );
        }
    }

    /// Posters handed to a shelf survive its FIRST list and are dropped by any later one.
    ///
    /// The hand-over is the only way a drill-in ever has covers: the encoded bytes are
    /// pushed once per fetch and the queue they came from was emptied by the screen above.
    /// Clearing on the first sync — which is always "fresh", because a new screen holds no
    /// games — made every hand-over a silent no-op and left the drill-in on monograms.
    #[test]
    fn handed_over_posters_survive_the_first_list_and_only_that_one() {
        let poster = || {
            skia_safe::surfaces::raster_n32_premul((4, 6))
                .expect("a raster surface")
                .image_snapshot()
        };
        let library = LibraryShared::default();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        let mut s = LibraryScreen::new(&host(), 0);
        s.adopt_art(HashMap::from([("g0".to_string(), poster())]));
        s.sync(&library);
        assert_eq!(s.art.len(), 1, "the hand-over was wiped by the first list");

        library.set_games(games(&[("Rez", Some("PS2"))]));
        s.sync(&library);
        assert!(s.art.is_empty(), "a different library kept the old covers");
    }
}
