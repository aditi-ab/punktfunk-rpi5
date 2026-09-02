//! XFixes cursor source for the gamescope capture path.
//!
//! gamescope paints the pointer on a DRM cursor plane and omits it from the
//! PipeWire frame, so `SPA_META_Cursor` never arrives. This module reads the
//! nested Xwayland pointer via X11 and publishes a [`CursorOverlay`] into the
//! same slot the portal path uses.
//!
//! Connect to every nested Xwayland (`--xwayland-count`); the pointer lives
//! on the focused display only. Follow [`GS_CURSOR_FEEDBACK`] on the root —
//! gamescope hides by warping the pointer to `(w-1, h-1)` and leaving the last
//! opaque cursor, so motion and `XFixesGetCursorImage` both report the parked
//! arrow. Poll position at [`POLL`]; refresh shape on `CursorNotify`; re-read
//! the atom on `PropertyNotify` (and [`FEEDBACK_RESYNC`] if a notify is missed).
//!
//! Pin: `pick_active` and [`scale_to_frame`] tests in this module.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
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
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::GamescopeCursorTargets;

const MIT_MAGIC_COOKIE_1: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// Adopt Xwaylands spawned after start and retry dead connections. 2 s is
/// far below a missing-pointer notice; one socket probe per known display.
const REDISCOVER: Duration = Duration::from_secs(2);

/// 4 ms ≈ 250 Hz, matching the Windows GDI poller. The polled position is
/// the composited position and must out-run a 240 fps session.
const POLL: Duration = Duration::from_millis(4);

/// gamescope's pointer verdict on every nested root: `1` on the drawing
/// server, `0` elsewhere and on all of them once hidden. Motion cannot
/// answer this — see the module docs.
const GS_CURSOR_FEEDBACK: &str = "GAMESCOPE_CURSOR_VISIBLE_FEEDBACK";

/// Covers a missed `PropertyNotify` and an atom published after connect.
/// One `GetProperty` per display per interval vs the 250 Hz pointer poll.
const FEEDBACK_RESYNC: Duration = Duration::from_millis(250);

/// Running XFixes cursor reader. Drop stops the worker and waits — bounded —
/// for it to release the X connections.
pub(super) struct XFixesCursorSource {
    stop: Arc<AtomicBool>,
    /// Signalled just before the worker returns so `Drop` can bound its wait.
    done: std::sync::mpsc::Receiver<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl XFixesCursorSource {
    /// Publish overlays into `slot` from the nested Xwayland gamescope is drawing.
    /// `targets` is re-run every [`REDISCOVER`]; `frame_size` is `(w << 32) | h`,
    /// `0` until the first negotiation (see [`scale_to_frame`]).
    ///
    /// `None` only if the thread cannot spawn. An empty target list is not a
    /// failure: the worker idles and retries so a stream that starts before the
    /// game still converges.
    pub(super) fn spawn(
        targets: GamescopeCursorTargets,
        slot: Arc<Mutex<Option<CursorOverlay>>>,
        frame_size: Arc<AtomicU64>,
    ) -> Option<Self> {
        // Connect here so the log below is the live set, not "starting…".
        let mut displays = Vec::new();
        rediscover(&mut displays, &targets, true);
        let names: Vec<&str> = displays.iter().map(|d| d.name.as_str()).collect();
        let feedback = displays.iter().any(|d| d.gs_visible.is_some());
        if displays.is_empty() {
            tracing::warn!(
                "gamescope cursor: no usable nested Xwayland yet — retrying every {}s (a game's \
                 Xwayland appears when it launches)",
                REDISCOVER.as_secs()
            );
        } else {
            tracing::info!(
                displays = ?names,
                cursor_feedback = feedback,
                "gamescope cursor: XFixes source live — following the Xwayland gamescope draws the \
                 pointer on (cursor_feedback=false ⇒ this gamescope publishes no \
                 GAMESCOPE_CURSOR_VISIBLE_FEEDBACK, degrading to the pointer-motion heuristic)"
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let join = std::thread::Builder::new()
            .name("pf-gs-cursor".into())
            .spawn(move || {
                run(displays, slot, stop_worker, targets, frame_size);
                let _ = done_tx.send(());
            })
            .ok()?;
        Some(XFixesCursorSource {
            stop,
            done: done_rx,
            join: Some(join),
        })
    }
}

impl Drop for XFixesCursorSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The worker blocks in `RustConnection` replies with no read timeout.
        // A hung Xwayland would hang session-path teardown. On timeout detach:
        // the thread only touches its own X connections and an `Arc`'d slot,
        // and exits the moment `stop` is visible and the reply lands.
        let joinable = self.done.recv_timeout(Duration::from_millis(250)).is_ok();
        if let Some(j) = self.join.take() {
            if joinable {
                let _ = j.join(); // `done` already fired
            } else {
                tracing::warn!(
                    "gamescope cursor: worker did not stop within 250ms (blocked on an X reply?) — \
                     detaching it"
                );
            }
        }
    }
}

/// Connect new targets and retry dead ones. Leave healthy displays so their
/// shape cache and last position survive.
fn rediscover(displays: &mut Vec<XDisplay>, targets: &GamescopeCursorTargets, first: bool) {
    for (dpy, xauth) in targets() {
        let existing = displays.iter().position(|d| d.name == dpy);
        if existing.is_some_and(|i| !displays[i].dead) {
            continue;
        }
        match connect(&dpy, xauth.as_deref()) {
            Ok((conn, root, root_size, feedback)) => {
                let d = XDisplay::new(dpy.clone(), conn, root, root_size, feedback);
                match existing {
                    Some(i) => {
                        tracing::info!(dpy = %dpy, "gamescope cursor: reconnected a nested Xwayland");
                        displays[i] = d;
                    }
                    None => {
                        if !first {
                            tracing::info!(dpy = %dpy, "gamescope cursor: adopted a new nested Xwayland");
                        }
                        displays.push(d);
                    }
                }
            }
            // Debug, not warn: a 2 s retry would flood for every advertised
            // Xwayland that never comes up.
            Err(e) if first => tracing::warn!(
                dpy = %dpy, error = %e,
                "gamescope cursor: skipping a nested Xwayland we can't use (will retry)"
            ),
            Err(e) => tracing::debug!(dpy = %dpy, error = %e, "gamescope cursor: retry failed"),
        }
    }
}

/// Connection, root, root pixel size (for [`scale_to_frame`]), and this
/// display's initial [`GS_CURSOR_FEEDBACK`] reading. Value is `None` when
/// gamescope publishes no such property here.
type Connected = (RustConnection, Window, (u16, u16), (Atom, Option<bool>));

fn connect(dpy: &str, xauthority: Option<&str>) -> Result<Connected, String> {
    let (conn, screen_num) = connect_conn(dpy, xauthority)?;

    // XFixes ≥ 1 is GetCursorImage / SelectCursorInput; ask 5.0, take what we get.
    conn.xfixes_query_version(5, 0)
        .map_err(ReplyError::from)
        .and_then(|c| c.reply())
        .map_err(|e| format!("XFixes unavailable: {e}"))?;

    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| format!("no X screen {screen_num}"))?;
    let root = screen.root;
    // Nested root can differ from the PipeWire node (`-w/-h` vs `-W/-H`).
    // `QueryPointer` answers in this space. Setup reply is already parsed.
    let root_size = (screen.width_in_pixels, screen.height_in_pixels);

    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
        .map_err(ReplyError::from)
        .and_then(|c| c.check())
        .map_err(|e| format!("SelectCursorInput: {e}"))?;

    // Intern with `only_if_exists=false` so we have an id if gamescope sets
    // the property later. PROPERTY_CHANGE failure is not fatal: resync still
    // tracks it at [`FEEDBACK_RESYNC`].
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
    Ok((conn, root, root_size, (feedback_atom, feedback)))
}

/// Open `dpy` with `xauthority`'s cookie without touching this process's
/// environment.
///
/// `RustConnection::connect` reads `XAUTHORITY` from the env. Swapping it
/// around each connect is unsound in a live multithreaded host: `getenv`
/// takes no lock. Parse the MIT-MAGIC-COOKIE-1 entry and pass it to
/// `connect_to_stream_with_auth_info`. If nothing usable, connect with an
/// empty token ([`connect_unauthenticated`]) rather than rewriting `environ`.
fn connect_conn(dpy: &str, xauthority: Option<&str>) -> Result<(RustConnection, usize), String> {
    let Some(path) = xauthority else {
        // Ambient environment is already what this connect should use.
        return RustConnection::connect(Some(dpy)).map_err(|e| format!("connect: {e}"));
    };
    match mit_magic_cookie(path, dpy) {
        Some((name, data)) => match connect_with_cookie(dpy, name, data) {
            Ok(v) => return Ok(v),
            Err(e) => tracing::debug!(
                dpy = %dpy, xauthority = %path, error = %e,
                "gamescope cursor: cookie connect failed — retrying unauthenticated"
            ),
        },
        None => tracing::debug!(
            dpy = %dpy, xauthority = %path,
            "gamescope cursor: no MIT-MAGIC-COOKIE-1 entry for this display — connecting \
             unauthenticated"
        ),
    }
    connect_unauthenticated(dpy)
}

/// Same two steps as `RustConnection::connect`, minus its env-derived auth lookup.
fn connect_with_cookie(
    dpy: &str,
    auth_name: Vec<u8>,
    auth_data: Vec<u8>,
) -> Result<(RustConnection, usize), String> {
    let parsed = x11rb::reexports::x11rb_protocol::parse_display::parse_display(Some(dpy))
        .map_err(|e| format!("parse display {dpy}: {e}"))?;
    let screen = usize::from(parsed.screen);
    let mut stream = None;
    let mut last_err = None;
    for addr in parsed.connect_instruction() {
        match DefaultStream::connect(&addr) {
            Ok((s, _peer)) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| match last_err {
        Some(e) => format!("connect: {e}"),
        None => "connect: no usable address".to_string(),
    })?;
    RustConnection::connect_to_stream_with_auth_info(stream, screen, auth_name, auth_data)
        .map(|c| (c, screen))
        .map_err(|e| format!("setup: {e}"))
}

/// Connect with an explicitly empty auth token. Do not `setenv` `XAUTHORITY`:
/// glibc rewrites process-global `environ` and `getenv` takes no lock.
///
/// Reached only when [`mit_magic_cookie`] found no usable entry. x11rb's
/// lookup also matches family/address, so it would find nothing too. A
/// gamescope Xwayland writes a single-entry MIT-MAGIC-COOKIE-1 file.
fn connect_unauthenticated(dpy: &str) -> Result<(RustConnection, usize), String> {
    connect_with_cookie(dpy, Vec::new(), Vec::new())
}

/// MIT-MAGIC-COOKIE-1 `(name, data)` for `dpy` from the `.Xauthority` file.
///
/// Wire: big-endian `u16 family`, then four length-prefixed byte strings —
/// `address`, `number` (display number in ASCII), `name`, `data`. Empty
/// `number` matches any display.
///
/// Does not match family/address (unlike libxcb). A gamescope Xwayland has
/// a single-entry cookie file; a wrong pick is rejected by the server and
/// [`connect_conn`] falls back. Guessing a `LOCAL` unix peer address would rot.
fn mit_magic_cookie(path: &str, dpy: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let want = display_number(dpy);
    let mut wildcard = None;
    let mut p = 0usize;
    'entries: while p + 2 <= bytes.len() {
        p += 2; // skip family (we do not match on it)
        let mut fields: [&[u8]; 4] = [&[]; 4];
        for f in fields.iter_mut() {
            let Some(lb) = bytes.get(p..p + 2) else {
                break 'entries; // truncated: keep whatever we already found
            };
            let len = usize::from(u16::from_be_bytes([lb[0], lb[1]]));
            p += 2;
            let Some(v) = bytes.get(p..p + len) else {
                break 'entries;
            };
            *f = v;
            p += len;
        }
        let [_address, number, name, data] = fields;
        if name != MIT_MAGIC_COOKIE_1 {
            continue;
        }
        if want.as_deref().is_some_and(|w| number == w) {
            return Some((name.to_vec(), data.to_vec()));
        }
        if number.is_empty() && wildcard.is_none() {
            wildcard = Some((name.to_vec(), data.to_vec()));
        }
    }
    wildcard
}

/// Display number as ASCII bytes: `":2"`, `"host:2"`, `":2.0"` → `b"2"`.
/// `None` when there is no numeric component to match.
fn display_number(dpy: &str) -> Option<Vec<u8>> {
    let num = dpy.rsplit_once(':')?.1.split('.').next()?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then(|| num.as_bytes().to_vec())
}

/// `None` = property absent or unreadable — caller falls back to motion
/// rather than blanking, so an older gamescope keeps a cursor.
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

struct XDisplay {
    name: String,
    conn: RustConnection,
    root: Window,
    /// Nested root size — `QueryPointer` space, not necessarily the captured
    /// frame (see [`scale_to_frame`]).
    root_size: (u16, u16),
    /// Last polled position; a change since the previous tick marks focus.
    last_pos: Option<(i32, i32)>,
    shape: Shape,
    /// Dirty after CursorNotify; fetch only while this display is active.
    need_shape: bool,
    /// Interned [`GS_CURSOR_FEEDBACK`] (`0` = intern failed, treated as absent).
    feedback_atom: Atom,
    /// `Some(true)` = drawing here, `Some(false)` = not, `None` = no verdict.
    gs_visible: Option<bool>,
    dead: bool,
}

impl XDisplay {
    fn new(
        name: String,
        conn: RustConnection,
        root: Window,
        root_size: (u16, u16),
        (feedback_atom, gs_visible): (Atom, Option<bool>),
    ) -> Self {
        XDisplay {
            name,
            conn,
            root,
            root_size,
            last_pos: None,
            shape: Shape::default(),
            need_shape: true,
            feedback_atom,
            gs_visible,
            dead: false,
        }
    }

    /// Keep a previously-seen verdict if the read fails: a transient miss
    /// must not look like "no feedback" and re-arm the motion heuristic.
    fn resync_feedback(&mut self) {
        let fresh = read_cursor_feedback(&self.conn, self.root, self.feedback_atom);
        if fresh.is_some() || self.gs_visible.is_none() {
            self.gs_visible = fresh;
        }
    }
}

#[derive(Default)]
struct Shape {
    /// Straight-alpha RGBA (`w*h*4`); empty before the first image.
    rgba: Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    /// XFixes per-display cursor serial; not comparable across displays.
    serial: u64,
    /// All-transparent image = hidden. Kept so a position-only tick preserves it.
    visible: bool,
}

/// Map a root-space pointer into frame pixels.
///
/// `QueryPointer` answers in the nested root; `CursorOverlay::x/y` is frame
/// pixels. gamescope's `-w/-h` and `-W/-H` are independent, so root coords
/// verbatim land at a fraction of the real position. `(0, 0)` frame (not yet
/// negotiated) passes through unscaled.
///
/// The bitmap is not scaled: the pointer lands in the right place at root size.
fn scale_to_frame((x, y): (i32, i32), root: (u16, u16), frame: (u32, u32)) -> (i32, i32) {
    let (rw, rh) = (u32::from(root.0), u32::from(root.1));
    let (fw, fh) = frame;
    if rw == 0 || rh == 0 || fw == 0 || fh == 0 || (rw, rh) == (fw, fh) {
        return (x, y);
    }
    (
        ((i64::from(x) * i64::from(fw)) / i64::from(rw)) as i32,
        ((i64::from(y) * i64::from(fh)) / i64::from(rh)) as i32,
    )
}

fn run(
    mut displays: Vec<XDisplay>,
    slot: Arc<Mutex<Option<CursorOverlay>>>,
    stop: Arc<AtomicBool>,
    targets: GamescopeCursorTargets,
    frame_size: Arc<AtomicU64>,
) {
    let mut last_discover = std::time::Instant::now();
    let mut warned_scale = false;
    let mut active = 0usize;
    // Overlay serial must bump when the drawn cursor changes — active display
    // or its shape. Per-display XFixes serials are not comparable, so a switch
    // could reuse a number and the encoder would keep the old texture.
    let mut out_serial = 0u64;
    let mut last_key = (usize::MAX, u64::MAX);
    let mut warned_image = false;
    let mut last_resync = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Re-read on a slow cadence so a missed PropertyNotify still converges.
        let resync = last_resync.elapsed() >= FEEDBACK_RESYNC;
        if resync {
            last_resync = std::time::Instant::now();
        }
        if last_discover.elapsed() >= REDISCOVER {
            last_discover = std::time::Instant::now();
            rediscover(&mut displays, &targets, false);
        }

        // Pointer motion is the fallback focus signal when no display publishes
        // a cursor verdict.
        let mut active_moved = false;
        let mut other_moved: Option<usize> = None;
        for (i, d) in displays.iter_mut().enumerate() {
            if d.dead {
                continue;
            }
            // CursorNotify = shape changed. PropertyNotify on the feedback atom
            // = gamescope republished its verdict (the root has many properties).
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
                Ok(_) => {} // other screen — keep the last position.
                Err(_) => d.dead = true,
            }
        }

        let states: Vec<(bool, Option<bool>)> =
            displays.iter().map(|d| (d.dead, d.gs_visible)).collect();
        let hidden_by_gamescope;
        (active, hidden_by_gamescope) = pick_active(&states, active, active_moved, other_moved);
        if displays.get(active).is_none_or(|d| d.dead) {
            match displays.iter().position(|d| !d.dead) {
                Some(k) => active = k,
                None => {
                    std::thread::sleep(POLL); // all dead — idle until Drop.
                    continue;
                }
            }
        }

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

        // Publish `visible: false` rather than `None` for a hidden pointer:
        // the encode loop copies this slot and strips invisible overlays, so
        // `None` would leave the last visible overlay standing on repeat frames.
        let d = &displays[active];
        let drawn = d.shape.visible && !hidden_by_gamescope;
        // Negotiated size from PipeWire `param_changed`, packed `(w << 32) | h`;
        // `0` = not negotiated yet.
        let packed = frame_size.load(Ordering::Relaxed);
        let frame = ((packed >> 32) as u32, packed as u32);
        if !warned_scale
            && frame.0 != 0
            && (u32::from(d.root_size.0), u32::from(d.root_size.1)) != frame
        {
            warned_scale = true;
            tracing::warn!(
                dpy = %d.name,
                root = %format!("{}x{}", d.root_size.0, d.root_size.1),
                negotiated = %format!("{}x{}", frame.0, frame.1),
                "gamescope cursor: the nested root and the captured frame are different sizes \
                 (gamescope -w/-h vs -W/-H) — scaling the pointer POSITION into frame space; the \
                 cursor bitmap stays at root scale"
            );
        }
        let overlay = match (d.last_pos, d.shape.rgba.is_empty()) {
            (Some(pos), false) => {
                let (px, py) = scale_to_frame(pos, d.root_size, frame);
                let key = (active, d.shape.serial);
                if key != last_key {
                    out_serial += 1;
                    last_key = key;
                }
                Some(CursorOverlay {
                    // Overlay top-left is pointer − hotspot.
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

/// `(active, hidden)` from per-display `(dead, gs_visible)`.
///
/// Prefer gamescope's verdict: it is right for a static pointer, which motion
/// cannot read (gamescope parks the pointer at `(w-1, h-1)`). Fallback, only
/// when no live display publishes a verdict: sticky while the active pointer
/// moves, else follow another that moved. `hidden` is never set on fallback.
fn pick_active(
    states: &[(bool, Option<bool>)],
    active: usize,
    active_moved: bool,
    other_moved: Option<usize>,
) -> (usize, bool) {
    let live = |&(dead, _): &(bool, Option<bool>)| !dead;
    if states.iter().filter(|s| live(s)).any(|(_, v)| v.is_some()) {
        return match states.iter().position(|s| live(s) && s.1 == Some(true)) {
            Some(i) => (i, false),
            // Drawing none. Keep `active` so shape + last position stay warm;
            // the caller publishes `visible: false` instead of dropping.
            None => (active, true),
        };
    }
    match other_moved {
        Some(j) if !active_moved => (j, false),
        _ => (active, false),
    }
}

/// A hidden (all-transparent) pointer keeps the last bitmap for instant
/// re-show but flips visibility; the serial still bumps.
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

/// x11rb splits request (`ConnectionError`) and `reply()` (`ReplyError`);
/// `?` on the request converts into the reply error.
fn fetch_cursor_image(conn: &RustConnection) -> Result<GetCursorImageReply, ReplyError> {
    conn.xfixes_get_cursor_image()?.reply()
}

fn fetch_pointer(conn: &RustConnection, root: Window) -> Result<QueryPointerReply, ReplyError> {
    conn.query_pointer(root)?.reply()
}

/// XFixes pixels are packed `0xAARRGGBB` with premultiplied alpha (Xrender /
/// Xcursor). The overlay and both blend paths want straight RGBA, like
/// `SPA_META_Cursor`.
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
    use super::{
        display_number, mit_magic_cookie, pick_active, scale_to_frame, MIT_MAGIC_COOKIE_1,
    };

    fn entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = family.to_be_bytes().to_vec();
        for f in [address, number, name, data] {
            v.extend_from_slice(&(f.len() as u16).to_be_bytes());
            v.extend_from_slice(f);
        }
        v
    }

    fn write_xauth(bytes: &[u8]) -> std::path::PathBuf {
        // Unique per call via the address of a local — no rand, and tests do
        // not share one path concurrently.
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "pf-xauth-test-{}-{:p}",
            std::process::id(),
            bytes as *const [u8]
        );
        p.push(uniq);
        std::fs::write(&p, bytes).expect("write scratch xauth");
        p
    }

    #[test]
    fn display_number_handles_every_display_spelling() {
        assert_eq!(display_number(":2").as_deref(), Some(&b"2"[..]));
        assert_eq!(display_number(":2.0").as_deref(), Some(&b"2"[..]));
        assert_eq!(display_number("host:13.1").as_deref(), Some(&b"13"[..]));
        assert_eq!(display_number("bogus"), None);
        assert_eq!(display_number(":"), None);
        assert_eq!(display_number(":abc"), None);
    }

    #[test]
    fn mit_magic_cookie_picks_the_matching_entry() {
        let mut file = Vec::new();
        file.extend(entry(256, b"host", b"2", b"XDM-AUTHORIZATION-1", b"nope"));
        file.extend(entry(
            256,
            b"host",
            b"7",
            MIT_MAGIC_COOKIE_1,
            b"other-display",
        ));
        file.extend(entry(256, b"host", b"2", MIT_MAGIC_COOKIE_1, b"the-cookie"));
        let p = write_xauth(&file);
        let got = mit_magic_cookie(p.to_str().unwrap(), ":2");
        assert_eq!(
            got,
            Some((MIT_MAGIC_COOKIE_1.to_vec(), b"the-cookie".to_vec()))
        );
        assert_eq!(mit_magic_cookie(p.to_str().unwrap(), ":9"), None);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn mit_magic_cookie_falls_back_to_a_wildcard_entry() {
        let mut file = entry(256, b"host", b"", MIT_MAGIC_COOKIE_1, b"wildcard");
        file.extend(entry(256, b"host", b"3", MIT_MAGIC_COOKIE_1, b"exact"));
        let p = write_xauth(&file);
        let path = p.to_str().unwrap();
        assert_eq!(
            mit_magic_cookie(path, ":3").map(|(_, d)| d),
            Some(b"exact".to_vec())
        );
        assert_eq!(
            mit_magic_cookie(path, ":4").map(|(_, d)| d),
            Some(b"wildcard".to_vec())
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_truncated_xauthority_keeps_what_it_already_parsed() {
        let mut file = entry(256, b"host", b"", MIT_MAGIC_COOKIE_1, b"wildcard");
        file.extend(entry(256, b"host", b"5", MIT_MAGIC_COOKIE_1, b"truncated"));
        file.truncate(file.len() - 4);
        let p = write_xauth(&file);
        assert_eq!(
            mit_magic_cookie(p.to_str().unwrap(), ":5").map(|(_, d)| d),
            Some(b"wildcard".to_vec())
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_garbage_xauthority_is_declined_not_fatal() {
        let p = write_xauth(&[0, 0, 0xff, 0xff, 1, 2, 3]);
        assert_eq!(mit_magic_cookie(p.to_str().unwrap(), ":0"), None);
        let _ = std::fs::remove_file(p);
        assert_eq!(mit_magic_cookie("/nonexistent/pf-xauth", ":0"), None);
    }

    #[test]
    fn pointer_positions_are_mapped_into_frame_space() {
        assert_eq!(
            scale_to_frame((320, 180), (640, 360), (1280, 720)),
            (640, 360)
        );
        assert_eq!(scale_to_frame((0, 0), (640, 360), (1280, 720)), (0, 0));
        assert_eq!(
            scale_to_frame((1280, 720), (1280, 720), (640, 360)),
            (640, 360)
        );
        // Equal spaces: pass-through, no rounding drift.
        assert_eq!(scale_to_frame((7, 9), (1920, 1080), (1920, 1080)), (7, 9));
        // Not negotiated, or a degenerate root: pass through rather than divide by zero.
        assert_eq!(scale_to_frame((7, 9), (1920, 1080), (0, 0)), (7, 9));
        assert_eq!(scale_to_frame((7, 9), (0, 0), (1920, 1080)), (7, 9));
    }

    /// A 5K frame times a 5K coordinate overflows `i32` — scale in `i64`.
    #[test]
    fn scaling_does_not_overflow_at_5k() {
        assert_eq!(
            scale_to_frame((2879, 1619), (2880, 1620), (5120, 2880)),
            (5118, 2878)
        );
    }

    /// Display 0 = Big Picture Xwayland, 1 = the game's.
    const BPM: usize = 0;
    const GAME: usize = 1;

    #[test]
    fn follows_the_display_gamescope_draws_on() {
        // Verdict beats motion, including when only the parked display moved.
        let states = [(false, Some(false)), (false, Some(true))];
        assert_eq!(pick_active(&states, BPM, false, Some(BPM)), (GAME, false));
    }

    #[test]
    fn a_pointer_gamescope_draws_nowhere_is_hidden_not_parked() {
        let states = [(false, Some(false)), (false, Some(false))];
        // Keep `active` (shape + last position stay warm) but hidden.
        assert_eq!(pick_active(&states, GAME, false, None), (GAME, true));
    }

    #[test]
    fn re_show_returns_to_the_drawing_display() {
        let states = [(false, Some(true)), (false, Some(false))];
        assert_eq!(pick_active(&states, GAME, false, None), (BPM, false));
    }

    #[test]
    fn a_dead_displays_verdict_is_ignored() {
        // Stale `Some(true)` on an exited Xwayland must not win.
        let states = [(false, Some(true)), (true, Some(true))];
        assert_eq!(pick_active(&states, GAME, false, None), (BPM, false));
        // A dead display's `Some` must not count as "this gamescope publishes
        // a verdict" — that would blank the cursor forever.
        let states = [(false, None), (true, Some(false))];
        assert_eq!(pick_active(&states, BPM, false, Some(GAME)), (GAME, false));
    }

    #[test]
    fn no_verdict_falls_back_to_the_motion_heuristic() {
        let none = [(false, None), (false, None)];
        assert_eq!(pick_active(&none, BPM, true, Some(GAME)), (BPM, false));
        assert_eq!(pick_active(&none, BPM, false, Some(GAME)), (GAME, false));
        // Never `hidden` on this path (would blank an older gamescope).
        assert_eq!(pick_active(&none, BPM, false, None), (BPM, false));
    }
}
