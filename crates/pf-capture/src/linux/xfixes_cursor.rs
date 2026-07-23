//! XFixes cursor source for the gamescope capture path (remote-desktop-sweep Phase C).
//!
//! gamescope draws the pointer on a DRM hardware-cursor plane and its `paint_pipewire()`
//! deliberately excludes the cursor from the frame it feeds its built-in PipeWire node — so
//! `SPA_META_Cursor` never arrives and the ordinary [`cursor_live`](super::PortalCapturer) slot
//! stays empty (a KWin/GNOME session gets its cursor from that meta; gamescope can't embed one
//! either, its `set_hw_cursor` is inert). We instead read the pointer from gamescope's nested
//! Xwayland via X11 — the trick Sunshine uses — and publish a [`CursorOverlay`] into that same
//! slot, so the encoder blend composites the pointer into the video exactly like the portal path.
//!
//! **Multiple Xwaylands.** gamescope runs one Xwayland per `--xwayland-count` (Steam Gaming Mode
//! uses 2: one for Big Picture, one for the game). The pointer lives on whichever is FOCUSED — an
//! inactive display's pointer is frozen. So the source connects to ALL of them and publishes from
//! the one gamescope is actually drawing the pointer on; it reads that display's shape too, since
//! each Xwayland has its own current cursor. This is why a single-display read froze the pointer
//! the moment a game on the OTHER Xwayland took focus.
//!
//! Three X sources per display, split by cost (Sunshine's split, plus gamescope's own verdict):
//! * **Position** — core `QueryPointer` on the root, polled fast. Cheap (a few-byte reply, no
//!   bitmap), so it can out-pace the stream fps and keep the composited pointer smooth.
//! * **Shape / hotspot** — `XFixesGetCursorImage`, refreshed only after an XFixes `CursorNotify`
//!   (a real cursor change). A fully-transparent image reads as hidden.
//! * **Visibility + focus** — [`GAMESCOPE_CURSOR_VISIBLE_FEEDBACK`](GS_CURSOR_FEEDBACK) on the
//!   root, read at connect and re-read on its `PropertyNotify`.
//!
//! **Why the feedback atom and not pointer motion.** gamescope hides its pointer by WARPING the X
//! pointer to the root's bottom-right corner pixel — it does NOT swap in a transparent X cursor, so
//! `XFixesGetCursorImage` keeps handing back the last opaque arrow. The original "follow whichever
//! display's pointer moved" heuristic therefore stuck to the parked display (a parked pointer never
//! moves again) and composited that arrow at `(w-1, h-1)`: a sliver of cursor welded to the corner
//! of the stream for the rest of the session, while the real pointer went undrawn. Reported
//! on-glass as "part of cursor shows up on bottom right … isn't where it really is", in every game
//! (a game grabs the pointer, so the hide is permanent). Measured on a live 1920x1080 Gaming Mode
//! session: real motion ⇒ pointer live + feedback `1`; 3 s idle (`--hide-cursor-delay 3000`) ⇒
//! pointer `(1919, 1079)` + feedback `0`. The atom answers BOTH questions correctly for a static
//! pointer, which motion cannot: which Xwayland owns it, and whether to draw it at all — so
//! honouring it also gives the stream gamescope's own idle auto-hide, which this source never had.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use pf_frame::CursorOverlay;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xfixes::{self, ConnectionExt as _, GetCursorImageReply};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, QueryPointerReply,
    Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// Serializes the brief `XAUTHORITY` env swap around a connect (the var is process-global). Only
/// ever contended if two gamescope sessions start at once — rare, and the swap is microseconds.
static XAUTH_LOCK: Mutex<()> = Mutex::new(());

/// Position out-paces the stream fps (`POLL`); shape rides `CursorNotify` events drained each tick.
/// 4 ms ≈ 250 Hz matches the Windows GDI poller — the polled position IS the composited position
/// and must out-run a 240 fps session or the pointer stutters.
const POLL: Duration = Duration::from_millis(4);

/// gamescope's own pointer verdict, published on EVERY nested Xwayland's root: `1` on the server
/// whose pointer gamescope is currently drawing, `0` on the others — and `0` on all of them once
/// the pointer is hidden (a game grabbed it, or `--hide-cursor-delay` fired). See the module docs
/// for why this, and not pointer motion, is the signal this source follows.
const GS_CURSOR_FEEDBACK: &str = "GAMESCOPE_CURSOR_VISIBLE_FEEDBACK";

/// Self-heal cadence for the feedback re-read: `PropertyNotify` drives it, this only covers a
/// missed/coalesced event (and a gamescope that publishes the atom after we connected). One
/// `GetProperty` per display per interval is nothing next to the 250 Hz pointer poll.
const FEEDBACK_RESYNC: Duration = Duration::from_millis(250);

/// A running XFixes cursor reader. Dropping it stops the worker thread and joins it, releasing the
/// X connections — so it lives exactly as long as the capturer that owns it.
pub(super) struct XFixesCursorSource {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl XFixesCursorSource {
    /// Connect to every gamescope nested Xwayland in `targets` (`(DISPLAY, XAUTHORITY)`) and start
    /// publishing cursor overlays into `slot`, following the focused display's pointer. Returns
    /// `None` — and logs — if NONE can be used (no X connection / no XFixes), so the caller
    /// degrades to no gamescope cursor (today's behaviour) instead of failing the session.
    pub(super) fn spawn(
        targets: Vec<(String, Option<String>)>,
        slot: Arc<Mutex<Option<CursorOverlay>>>,
    ) -> Option<Self> {
        // Connect on the caller's thread so failures degrade cleanly and the displays are validated
        // before we commit a thread.
        let mut displays = Vec::new();
        for (dpy, xauth) in targets {
            match connect(&dpy, xauth.as_deref()) {
                Ok((conn, root, feedback)) => {
                    displays.push(XDisplay::new(dpy, conn, root, feedback))
                }
                Err(e) => tracing::warn!(
                    dpy = %dpy,
                    error = %e,
                    "gamescope cursor: skipping a nested Xwayland we can't use"
                ),
            }
        }
        if displays.is_empty() {
            tracing::warn!(
                "gamescope cursor: no usable nested Xwayland — no in-video pointer this session \
                 (falls back to today's cursorless gamescope stream)"
            );
            return None;
        }
        let names: Vec<&str> = displays.iter().map(|d| d.name.as_str()).collect();
        let feedback = displays.iter().any(|d| d.gs_visible.is_some());
        tracing::info!(
            displays = ?names,
            cursor_feedback = feedback,
            "gamescope cursor: XFixes source live — following the Xwayland gamescope draws the \
             pointer on (cursor_feedback=false ⇒ this gamescope publishes no \
             GAMESCOPE_CURSOR_VISIBLE_FEEDBACK, degrading to the pointer-motion heuristic)"
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("pf-gs-cursor".into())
            .spawn(move || run(displays, slot, stop_worker))
            .ok()?;
        Some(XFixesCursorSource {
            stop,
            join: Some(join),
        })
    }
}

impl Drop for XFixesCursorSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Open the X connection, negotiate XFixes, and select cursor-change + root-property events —
/// returning the connection, root window and this display's initial
/// [`GAMESCOPE_CURSOR_FEEDBACK`](GS_CURSOR_FEEDBACK) reading (`(atom, value)`; the value is `None`
/// when gamescope publishes no such property here). `RustConnection` reads `XAUTHORITY` from the
/// env at connect time only, so set it under the lock (the host isn't a gamescope child), connect,
/// then restore.
type Connected = (RustConnection, Window, (Atom, Option<bool>));

fn connect(dpy: &str, xauthority: Option<&str>) -> Result<Connected, String> {
    let (conn, screen_num) = {
        let _g = XAUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XAUTHORITY");
        if let Some(x) = xauthority {
            std::env::set_var("XAUTHORITY", x);
        }
        let out = RustConnection::connect(Some(dpy));
        match (&prev, xauthority) {
            (Some(p), _) => std::env::set_var("XAUTHORITY", p),
            (None, Some(_)) => std::env::remove_var("XAUTHORITY"),
            (None, None) => {}
        }
        out.map_err(|e| format!("connect: {e}"))?
    };

    // XFixes ≥ 1 gives GetCursorImage / SelectCursorInput; ask for a modern minor, take what we get.
    conn.xfixes_query_version(5, 0)
        .map_err(ReplyError::from)
        .and_then(|c| c.reply())
        .map_err(|e| format!("XFixes unavailable: {e}"))?;

    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| format!("no X screen {screen_num}"))?
        .root;

    // Wake the worker's event drain whenever the cursor shape changes (incl. hide/show).
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
        .map_err(ReplyError::from)
        .and_then(|c| c.check())
        .map_err(|e| format!("SelectCursorInput: {e}"))?;

    // …and whenever gamescope republishes its cursor verdict. Interned with `only_if_exists=false`
    // so we hold a matchable atom id even on a gamescope that sets the property later; a failure to
    // select PROPERTY_CHANGE is NOT fatal — the resync re-read still tracks it, just at 250 ms.
    let feedback_atom = conn
        .intern_atom(false, GS_CURSOR_FEEDBACK.as_bytes())
        .map_err(ReplyError::from)
        .and_then(|c| c.reply())
        .map(|r| r.atom)
        .unwrap_or(0);
    if let Ok(c) = conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    ) {
        let _ = c.check();
    }
    let _ = conn.flush();
    let feedback = read_cursor_feedback(&conn, root, feedback_atom);
    Ok((conn, root, (feedback_atom, feedback)))
}

/// Read `GAMESCOPE_CURSOR_VISIBLE_FEEDBACK` off `root`. `None` = the property is absent or
/// unreadable (not a gamescope that publishes it) — the caller then falls back to the pointer-motion
/// heuristic rather than blanking the cursor, so an older gamescope keeps today's behaviour.
fn read_cursor_feedback(conn: &RustConnection, root: Window, atom: Atom) -> Option<bool> {
    if atom == 0 {
        return None;
    }
    let reply = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let value = reply.value32()?.next()?;
    Some(value != 0)
}

/// One gamescope Xwayland the source tracks.
struct XDisplay {
    name: String,
    conn: RustConnection,
    root: Window,
    /// Last polled pointer position — a change since the previous tick marks this display FOCUSED.
    last_pos: Option<(i32, i32)>,
    /// Cached cursor shape, refreshed only after this display's XFixes `CursorNotify`.
    shape: Shape,
    /// A `CursorNotify` (or first read) is pending — fetch the shape when this display is active.
    need_shape: bool,
    /// Interned [`GS_CURSOR_FEEDBACK`] atom (`0` = intern failed — treated as absent).
    feedback_atom: Atom,
    /// gamescope's verdict for THIS display: `Some(true)` = it is drawing the pointer here,
    /// `Some(false)` = it is not, `None` = this gamescope publishes no verdict at all.
    gs_visible: Option<bool>,
    /// The X connection died (game/Xwayland exited) — skip it.
    dead: bool,
}

impl XDisplay {
    fn new(
        name: String,
        conn: RustConnection,
        root: Window,
        (feedback_atom, gs_visible): (Atom, Option<bool>),
    ) -> Self {
        XDisplay {
            name,
            conn,
            root,
            last_pos: None,
            shape: Shape::default(),
            need_shape: true,
            feedback_atom,
            gs_visible,
            dead: false,
        }
    }

    /// Re-read gamescope's verdict, keeping a previously-seen one if the read fails (a transient
    /// failure must not look like "this gamescope has no feedback" and re-arm the motion heuristic).
    fn resync_feedback(&mut self) {
        let fresh = read_cursor_feedback(&self.conn, self.root, self.feedback_atom);
        if fresh.is_some() || self.gs_visible.is_none() {
            self.gs_visible = fresh;
        }
    }
}

/// Cached cursor shape for one display.
#[derive(Default)]
struct Shape {
    /// Straight-alpha RGBA (`w*h*4`, bytes R,G,B,A); empty before the first image arrives.
    rgba: Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    /// XFixes' own per-display cursor serial — bumps on every shape change.
    serial: u64,
    /// A hidden pointer arrives as an all-transparent image; kept so a position-only tick preserves
    /// the last known visibility.
    visible: bool,
}

fn run(
    mut displays: Vec<XDisplay>,
    slot: Arc<Mutex<Option<CursorOverlay>>>,
    stop: Arc<AtomicBool>,
) {
    let mut active = 0usize;
    // The overlay serial must bump whenever the DRAWN cursor changes — either the active display's
    // shape OR which display is active (per-display XFixes serials aren't comparable across
    // displays, so switching could reuse a number and the encoder would keep the old texture).
    let mut out_serial = 0u64;
    let mut last_key = (usize::MAX, u64::MAX);
    let mut warned_image = false;
    let mut last_resync = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // A missed/coalesced PropertyNotify would otherwise strand the verdict — re-read on a slow
        // cadence so the source always converges (also picks the atom up if gamescope adds it late).
        let resync = last_resync.elapsed() >= FEEDBACK_RESYNC;
        if resync {
            last_resync = std::time::Instant::now();
        }

        // 1) Poll every display's pointer; note which moved since last tick (the fallback focus
        //    signal, used only when this gamescope publishes no cursor verdict).
        let mut active_moved = false;
        let mut other_moved: Option<usize> = None;
        for (i, d) in displays.iter_mut().enumerate() {
            if d.dead {
                continue;
            }
            // Drain pending events. Two kinds are selected: XFixes CursorNotify (the shape
            // changed) and root PropertyNotify (gamescope republished its cursor verdict, among
            // the many other properties it keeps on the root — hence the atom match).
            let mut need_feedback = resync;
            loop {
                match d.conn.poll_for_event() {
                    Ok(Some(Event::XfixesCursorNotify(_))) => d.need_shape = true,
                    Ok(Some(Event::PropertyNotify(ev))) => {
                        need_feedback |= d.feedback_atom != 0 && ev.atom == d.feedback_atom;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {
                        d.dead = true;
                        break;
                    }
                }
            }
            if need_feedback && !d.dead {
                d.resync_feedback();
            }
            match fetch_pointer(&d.conn, d.root) {
                Ok(p) if p.same_screen => {
                    let pos = (i32::from(p.root_x), i32::from(p.root_y));
                    let moved = d.last_pos.is_some_and(|lp| lp != pos);
                    d.last_pos = Some(pos);
                    if moved {
                        if i == active {
                            active_moved = true;
                        } else if other_moved.is_none() {
                            other_moved = Some(i);
                        }
                    }
                }
                Ok(_) => {} // pointer on another screen — keep the last position.
                Err(_) => d.dead = true,
            }
        }

        // 2) Pick the display to publish from, and decide whether a pointer should be drawn at all.
        let states: Vec<(bool, Option<bool>)> =
            displays.iter().map(|d| (d.dead, d.gs_visible)).collect();
        let hidden_by_gamescope;
        (active, hidden_by_gamescope) = pick_active(&states, active, active_moved, other_moved);
        if displays.get(active).is_none_or(|d| d.dead) {
            match displays.iter().position(|d| !d.dead) {
                Some(k) => active = k,
                None => {
                    std::thread::sleep(POLL); // all connections dead — idle until Drop.
                    continue;
                }
            }
        }

        // 3) Fetch the active display's shape if a CursorNotify (or a focus switch) left it stale.
        if displays[active].need_shape {
            match fetch_cursor_image(&displays[active].conn) {
                Ok(img) => {
                    update_shape(&mut displays[active].shape, &img);
                    displays[active].need_shape = false;
                }
                Err(e) => {
                    if !warned_image {
                        warned_image = true;
                        tracing::warn!(error = %e, "gamescope cursor: GetCursorImage failed — retrying");
                    }
                }
            }
        }

        // 4) Publish the ACTIVE display's pointer + shape (or clear the slot when it has no cursor
        //    of its own — so a focus switch never leaves the other display's stale pointer showing).
        //    A pointer gamescope is not drawing is published `visible: false`, NOT dropped: the
        //    encode loop overwrites the frame's overlay from this slot and strips invisible ones, so
        //    a `None` here would leave the last visible overlay standing on repeat frames.
        let d = &displays[active];
        let drawn = d.shape.visible && !hidden_by_gamescope;
        let overlay = match (d.last_pos, d.shape.rgba.is_empty()) {
            (Some((px, py)), false) => {
                let key = (active, d.shape.serial);
                if key != last_key {
                    out_serial += 1;
                    last_key = key;
                }
                Some(CursorOverlay {
                    // Top-left = pointer position − hotspot (the overlay contract).
                    x: px - d.shape.hot_x as i32,
                    y: py - d.shape.hot_y as i32,
                    w: d.shape.w,
                    h: d.shape.h,
                    rgba: Arc::clone(&d.shape.rgba),
                    serial: out_serial,
                    hot_x: d.shape.hot_x,
                    hot_y: d.shape.hot_y,
                    visible: drawn,
                })
            }
            _ => None,
        };
        if let Ok(mut s) = slot.lock() {
            *s = overlay;
        }

        std::thread::sleep(POLL);
    }
}

/// Which display to publish from, and whether gamescope is drawing NO pointer right now —
/// `(active, hidden)`. `states` is one `(dead, gs_visible)` per display, in `displays` order.
///
/// PREFERRED: gamescope's own verdict. It is authoritative for a STATIC pointer, which is exactly
/// the case motion cannot read — a game grabs the pointer, gamescope parks it in the bottom-right
/// corner, and "follow whichever moved" then never switches again (see the module docs).
///
/// FALLBACK, only when NO live display publishes the verdict: the original motion heuristic —
/// sticky to the active display while its pointer moves (no flapping), else follow another that
/// moved. `hidden` is never asserted on this path, so an older gamescope keeps today's behaviour.
fn pick_active(
    states: &[(bool, Option<bool>)],
    active: usize,
    active_moved: bool,
    other_moved: Option<usize>,
) -> (usize, bool) {
    let live = |&(dead, _): &(bool, Option<bool>)| !dead;
    if states.iter().filter(|s| live(s)).any(|(_, v)| v.is_some()) {
        return match states.iter().position(|s| live(s) && s.1 == Some(true)) {
            // gamescope is drawing the pointer here — publish from it.
            Some(i) => (i, false),
            // Drawing none of them (a game grabbed it, or the idle auto-hide fired). Keep `active`
            // so the shape cache and last position stay warm for the re-show; the caller publishes
            // the overlay `visible: false` instead of dropping it.
            None => (active, true),
        };
    }
    match other_moved {
        Some(j) if !active_moved => (j, false),
        _ => (active, false),
    }
}

/// Update `shape` from a fresh `GetCursorImage` reply. A hidden pointer (all-transparent) keeps the
/// last bitmap (instant re-show) but flips visibility; the serial still bumps so the change shows.
fn update_shape(shape: &mut Shape, img: &GetCursorImageReply) {
    let visible =
        img.width > 0 && img.height > 0 && img.cursor_image.iter().any(|&p| (p >> 24) & 0xff != 0);
    if visible {
        shape.rgba = Arc::new(argb_premul_to_straight_rgba(&img.cursor_image));
        shape.w = u32::from(img.width);
        shape.h = u32::from(img.height);
        shape.hot_x = u32::from(img.xhot);
        shape.hot_y = u32::from(img.yhot);
    }
    shape.visible = visible;
    shape.serial = u64::from(img.cursor_serial);
}

/// One request+reply — x11rb splits errors (the request is `ConnectionError`, `reply()` is
/// `ReplyError` which is `From<ConnectionError>`), so the request `?` converts into the reply error.
fn fetch_cursor_image(conn: &RustConnection) -> Result<GetCursorImageReply, ReplyError> {
    conn.xfixes_get_cursor_image()?.reply()
}

fn fetch_pointer(conn: &RustConnection, root: Window) -> Result<QueryPointerReply, ReplyError> {
    conn.query_pointer(root)?.reply()
}

/// XFixes cursor pixels are packed `0xAARRGGBB` with **premultiplied** alpha (the Xrender / Xcursor
/// convention). The overlay + both blend paths want **straight** alpha RGBA (R,G,B,A bytes), like
/// the `SPA_META_Cursor` path — so un-premultiply here. (If on-glass shows over-bright fringes the
/// source wasn't premultiplied after all; drop the divide.)
fn argb_premul_to_straight_rgba(argb: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(argb.len() * 4);
    for &px in argb {
        let a = (px >> 24) & 0xff;
        let r = (px >> 16) & 0xff;
        let g = (px >> 8) & 0xff;
        let b = px & 0xff;
        let (r, g, b) = match a {
            0 => (0, 0, 0),
            255 => (r, g, b),
            a => (
                ((r * 255 + a / 2) / a).min(255),
                ((g * 255 + a / 2) / a).min(255),
                ((b * 255 + a / 2) / a).min(255),
            ),
        };
        out.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::pick_active;

    /// Steam Gaming Mode shape: display 0 = Big Picture's Xwayland, 1 = the game's.
    const BPM: usize = 0;
    const GAME: usize = 1;

    #[test]
    fn follows_the_display_gamescope_draws_on() {
        // Verdict beats motion: gamescope says the game's Xwayland owns the pointer, so we publish
        // from it even though only BPM's (parked) pointer looks like it moved.
        let states = [(false, Some(false)), (false, Some(true))];
        assert_eq!(pick_active(&states, BPM, false, Some(BPM)), (GAME, false));
    }

    /// THE REGRESSION: gamescope hides the pointer by warping it to the root's bottom-right corner
    /// and leaves the opaque arrow as the X cursor. Nothing moves ever again, so the motion
    /// heuristic stayed on the parked display and composited that arrow at (w-1, h-1) — a sliver of
    /// cursor welded to the corner of the stream for the whole session, in every game.
    #[test]
    fn a_pointer_gamescope_draws_nowhere_is_hidden_not_parked() {
        let states = [(false, Some(false)), (false, Some(false))];
        // `active` is kept (shape cache + last position stay warm for the re-show) but hidden.
        assert_eq!(pick_active(&states, GAME, false, None), (GAME, true));
    }

    #[test]
    fn re_show_returns_to_the_drawing_display() {
        let states = [(false, Some(true)), (false, Some(false))];
        assert_eq!(pick_active(&states, GAME, false, None), (BPM, false));
    }

    #[test]
    fn a_dead_displays_verdict_is_ignored() {
        // The game's Xwayland exited mid-session with a stale `Some(true)`; BPM is the live one.
        let states = [(false, Some(true)), (true, Some(true))];
        assert_eq!(pick_active(&states, GAME, false, None), (BPM, false));
        // …and a dead display's `Some` must not count as "this gamescope publishes a verdict",
        // which would blank the cursor forever on a gamescope that publishes none.
        let states = [(false, None), (true, Some(false))];
        assert_eq!(pick_active(&states, BPM, false, Some(GAME)), (GAME, false));
    }

    #[test]
    fn no_verdict_falls_back_to_the_motion_heuristic() {
        let none = [(false, None), (false, None)];
        // Sticky while the active display's own pointer moves…
        assert_eq!(pick_active(&none, BPM, true, Some(GAME)), (BPM, false));
        // …otherwise follow the one that moved…
        assert_eq!(pick_active(&none, BPM, false, Some(GAME)), (GAME, false));
        // …and never assert `hidden` on this path (that would regress an older gamescope to a
        // cursorless stream).
        assert_eq!(pick_active(&none, BPM, false, None), (BPM, false));
    }
}
