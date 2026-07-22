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
//! inactive display's pointer is frozen. So the source connects to ALL of them and each tick
//! follows the one whose pointer actually moved (gamescope routes input to the focused surface, so
//! exactly one moves at a time). It reads that display's shape too, since each Xwayland has its own
//! current cursor. This is why a single-display read froze the pointer the moment a game on the
//! OTHER Xwayland took focus.
//!
//! Two X sources per display, split by cost (Sunshine's split):
//! * **Position** — core `QueryPointer` on the root, polled fast. Cheap (a few-byte reply, no
//!   bitmap), so it can out-pace the stream fps and keep the composited pointer smooth. It also
//!   doubles as the focus signal (the display whose pointer moves is the active one).
//! * **Shape / hotspot / visibility** — `XFixesGetCursorImage`, refreshed only after an XFixes
//!   `CursorNotify` (a real cursor change). A game hiding the pointer IS a cursor change → the
//!   image comes back fully transparent → `visible: false`, which the encode loop strips before
//!   any blend path draws it (so a grabbed pointer shows nothing, matching native gamescope).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use pf_frame::CursorOverlay;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xfixes::{self, ConnectionExt as _, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt as _, QueryPointerReply, Window};
use x11rb::rust_connection::RustConnection;

/// Serializes the brief `XAUTHORITY` env swap around a connect (the var is process-global). Only
/// ever contended if two gamescope sessions start at once — rare, and the swap is microseconds.
static XAUTH_LOCK: Mutex<()> = Mutex::new(());

/// Position out-paces the stream fps (`POLL`); shape rides `CursorNotify` events drained each tick.
/// 4 ms ≈ 250 Hz matches the Windows GDI poller — the polled position IS the composited position
/// and must out-run a 240 fps session or the pointer stutters.
const POLL: Duration = Duration::from_millis(4);

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
                Ok((conn, root)) => displays.push(XDisplay::new(dpy, conn, root)),
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
        tracing::info!(
            displays = ?names,
            "gamescope cursor: XFixes source live — following the focused Xwayland's pointer"
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

/// Open the X connection, negotiate XFixes, and select cursor-change events — returning the
/// connection + root window. `RustConnection` reads `XAUTHORITY` from the env at connect time only,
/// so set it under the lock (the host isn't a gamescope child), connect, then restore.
fn connect(dpy: &str, xauthority: Option<&str>) -> Result<(RustConnection, Window), String> {
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
    let _ = conn.flush();
    Ok((conn, root))
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
    /// The X connection died (game/Xwayland exited) — skip it.
    dead: bool,
}

impl XDisplay {
    fn new(name: String, conn: RustConnection, root: Window) -> Self {
        XDisplay {
            name,
            conn,
            root,
            last_pos: None,
            shape: Shape::default(),
            need_shape: true,
            dead: false,
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

    while !stop.load(Ordering::Relaxed) {
        // 1) Poll every display's pointer; note which moved since last tick (the focus signal).
        let mut active_moved = false;
        let mut other_moved: Option<usize> = None;
        for (i, d) in displays.iter_mut().enumerate() {
            if d.dead {
                continue;
            }
            // Drain pending events; the only ones selected are CursorNotify, so ANY event means
            // "re-read this display's shape". poll_for_event never blocks.
            loop {
                match d.conn.poll_for_event() {
                    Ok(Some(_)) => d.need_shape = true,
                    Ok(None) => break,
                    Err(_) => {
                        d.dead = true;
                        break;
                    }
                }
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

        // 2) Switch focus: sticky to the active display while it moves (no flapping); otherwise
        //    follow another display that moved. If the active one died, fall to any live display.
        if !active_moved {
            if let Some(j) = other_moved {
                active = j;
            }
        }
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
        let d = &displays[active];
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
                    visible: d.shape.visible,
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
/// `ReplyError` which is `From<ConnectionError>`), so `?` collapses both.
fn fetch_cursor_image(conn: &RustConnection) -> Result<GetCursorImageReply, ReplyError> {
    Ok(conn.xfixes_get_cursor_image()?.reply()?)
}

fn fetch_pointer(conn: &RustConnection, root: Window) -> Result<QueryPointerReply, ReplyError> {
    Ok(conn.query_pointer(root)?.reply()?)
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
