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

/// Serializes the `XAUTHORITY` env swap of the LEGACY connect fallback (the var is process-global).
///
/// The fallback is a last resort now — see [`connect_conn`]. It serialises this source against
/// itself and nothing else: `getenv` needs no lock to be racy, so every OTHER thread's read (libspa
/// plugin load, EGL/CUDA init — concurrent by construction, since `attach_gamescope_cursor` runs
/// while the PipeWire thread is starting) could still observe the swapped value or a torn
/// environ. That is why the primary path parses the cookie itself and never touches the
/// environment.
static XAUTH_LOCK: Mutex<()> = Mutex::new(());

/// The `MIT-MAGIC-COOKIE-1` auth-protocol name, as it appears in an `.Xauthority` entry.
const MIT_MAGIC_COOKIE_1: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// Re-run the targets provider this often: adopt Xwaylands that appeared after the stream started
/// (gamescope spawns the game's on launch and advertises only the first in any child's environ) and
/// retry ones whose connection died. Two seconds is far below a human's tolerance for a missing
/// pointer and costs one cheap socket probe per known display.
const REDISCOVER: Duration = Duration::from_secs(2);

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

/// A running XFixes cursor reader. Dropping it stops the worker thread and waits — bounded — for it
/// to release the X connections, so it lives (at most) as long as the capturer that owns it.
pub(super) struct XFixesCursorSource {
    stop: Arc<AtomicBool>,
    /// Signalled by the worker just before it returns — lets `Drop` bound its wait (see there).
    done: std::sync::mpsc::Receiver<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl XFixesCursorSource {
    /// Start publishing cursor overlays into `slot` from whichever of gamescope's nested Xwaylands
    /// it is drawing the pointer on. `targets` is re-run every [`REDISCOVER`] to adopt Xwaylands
    /// that appear later and retry dead ones; `frame_size` is the negotiated capture size, packed
    /// `(w << 32) | h`, `0` until the first negotiation (see [`scale_to_frame`]).
    ///
    /// Returns `None` only if the thread cannot be spawned. An empty or entirely-unusable target
    /// list is NOT a failure: the worker idles and keeps re-running the provider, so a stream that
    /// starts before the game converges instead of being cursorless for the session.
    pub(super) fn spawn(
        targets: GamescopeCursorTargets,
        slot: Arc<Mutex<Option<CursorOverlay>>>,
        frame_size: Arc<AtomicU64>,
    ) -> Option<Self> {
        // First pass on the caller's thread: the common case connects here, so the log line below
        // reports the real state instead of "starting…".
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
        // BOUNDED join. The worker blocks in `RustConnection` replies with no read timeout, so a
        // peer that stops answering while keeping its socket open (a hung Xwayland) would hang
        // capturer teardown — and teardown runs on the session path. On timeout we detach: the
        // thread only ever touches its own X connections and an `Arc`'d slot, so leaving it to
        // finish on its own is safe (it observes `stop` and exits the moment its reply lands).
        let joinable = self.done.recv_timeout(Duration::from_millis(250)).is_ok();
        if let Some(j) = self.join.take() {
            if joinable {
                let _ = j.join(); // returns at once: `done` already fired
            } else {
                tracing::warn!(
                    "gamescope cursor: worker did not stop within 250ms (blocked on an X reply?) — \
                     detaching it"
                );
            }
        }
    }
}

/// Re-run the targets provider and reconcile `displays` with it: connect to any target we do not
/// track yet, and re-connect one whose display died. Existing healthy displays are left alone, so
/// their shape cache and last position survive.
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
            // Debug, not warn: with a 2 s retry a warn would flood for every Xwayland a
            // `--xwayland-count` reports but that never comes up.
            Err(e) if first => tracing::warn!(
                dpy = %dpy, error = %e,
                "gamescope cursor: skipping a nested Xwayland we can't use (will retry)"
            ),
            Err(e) => tracing::debug!(dpy = %dpy, error = %e, "gamescope cursor: retry failed"),
        }
    }
}

/// Open the X connection, negotiate XFixes, and select cursor-change + root-property events —
/// returning the connection, root window, the root's pixel size (for [`scale_to_frame`]) and this
/// display's initial [`GAMESCOPE_CURSOR_FEEDBACK`](GS_CURSOR_FEEDBACK) reading (`(atom, value)`;
/// the value is `None` when gamescope publishes no such property here).
type Connected = (RustConnection, Window, (u16, u16), (Atom, Option<bool>));

fn connect(dpy: &str, xauthority: Option<&str>) -> Result<Connected, String> {
    let (conn, screen_num) = connect_conn(dpy, xauthority)?;

    // XFixes ≥ 1 gives GetCursorImage / SelectCursorInput; ask for a modern minor, take what we get.
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
    // gamescope's nested root can be a DIFFERENT size from the output/PipeWire node it publishes
    // (`-w/-h` vs `-W/-H` are independent knobs), and this is the space `QueryPointer` answers in.
    // Free here — the setup reply is already parsed.
    let root_size = (screen.width_in_pixels, screen.height_in_pixels);

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
    Ok((conn, root, root_size, (feedback_atom, feedback)))
}

/// Establish the X connection for `dpy` using `xauthority`'s cookie, WITHOUT touching this process's
/// environment.
///
/// `RustConnection::connect` reads `XAUTHORITY` from the env, so the original implementation
/// `set_var`'d it around each connect under [`XAUTH_LOCK`]. That is unsound from a live
/// multithreaded host: the lock serialises this source against itself, but `getenv` takes no lock,
/// so any concurrent reader (libspa's plugin load, EGL/CUDA init — running at exactly this moment,
/// since the PipeWire thread is starting up) could read the swapped value or race the environ
/// rewrite outright. The project already has a process-wide env-lock discipline elsewhere, but
/// sharing it would be the wrong layer AND would still not fix `getenv`.
///
/// So: parse the MIT-MAGIC-COOKIE-1 entry out of the file ourselves and hand it to
/// `connect_to_stream_with_auth_info`, which is what `RustConnection::connect` does internally with
/// the cookie IT found. The env swap survives only as a fallback for a file we cannot parse (an
/// unexpected layout, or an auth family whose entry we decline to guess at).
fn connect_conn(dpy: &str, xauthority: Option<&str>) -> Result<(RustConnection, usize), String> {
    let Some(path) = xauthority else {
        // No per-display cookie file to inject: the ambient environment is already what this
        // connect should use, so there is nothing to swap and nothing to parse.
        return RustConnection::connect(Some(dpy)).map_err(|e| format!("connect: {e}"));
    };
    match mit_magic_cookie(path, dpy) {
        Some((name, data)) => match connect_with_cookie(dpy, name, data) {
            Ok(v) => return Ok(v),
            Err(e) => tracing::debug!(
                dpy = %dpy, xauthority = %path, error = %e,
                "gamescope cursor: cookie connect failed — falling back to the XAUTHORITY env swap"
            ),
        },
        None => tracing::debug!(
            dpy = %dpy, xauthority = %path,
            "gamescope cursor: no MIT-MAGIC-COOKIE-1 entry for this display — falling back to the \
             XAUTHORITY env swap"
        ),
    }
    connect_via_env_swap(dpy, path)
}

/// Connect to `dpy` and complete the setup handshake with an explicit cookie — the same two steps
/// `RustConnection::connect` performs, minus its env-derived auth lookup.
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

/// LEGACY fallback (see [`connect_conn`]): swap `XAUTHORITY`, connect, restore. Serialised against
/// this source's own concurrent connects, but NOT against other threads' `getenv` — which is why it
/// is a fallback and not the path taken.
fn connect_via_env_swap(dpy: &str, xauthority: &str) -> Result<(RustConnection, usize), String> {
    let _g = XAUTH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("XAUTHORITY");
    std::env::set_var("XAUTHORITY", xauthority);
    let out = RustConnection::connect(Some(dpy));
    match prev {
        Some(p) => std::env::set_var("XAUTHORITY", p),
        None => std::env::remove_var("XAUTHORITY"),
    }
    out.map_err(|e| format!("connect: {e}"))
}

/// The `MIT-MAGIC-COOKIE-1` `(name, data)` for `dpy` from the `.Xauthority`-format file at `path`.
///
/// Entry layout, all integers big-endian: `u16 family`, then four length-prefixed byte strings —
/// `address`, `number` (the display number in ASCII), `name`, `data`. An entry with an empty
/// `number` matches any display.
///
/// Deliberately does NOT match on family/address, unlike libxcb's own lookup. A gamescope Xwayland
/// gets its own single-entry cookie file, so there is nothing to disambiguate; and a wrong pick
/// cannot do damage — the server rejects the cookie, and [`connect_conn`] falls back to the env
/// path, which does the full match. Guessing the peer address for a `LOCAL`-family unix socket, on
/// the other hand, is exactly the sort of detail that would rot.
fn mit_magic_cookie(path: &str, dpy: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let want = display_number(dpy);
    // An entry whose `number` is empty matches any display — only used if no exact match is found.
    let mut wildcard = None;
    let mut p = 0usize;
    'entries: while p + 2 <= bytes.len() {
        p += 2; // family
        let mut fields: [&[u8]; 4] = [&[]; 4];
        for f in fields.iter_mut() {
            let Some(lb) = bytes.get(p..p + 2) else {
                break 'entries; // truncated — keep whatever we already found
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

/// The display NUMBER of an X display string as ASCII bytes: `":2"`, `"host:2"` and `":2.0"` all
/// yield `b"2"`. `None` when there is no numeric display component to match on.
fn display_number(dpy: &str) -> Option<Vec<u8>> {
    let num = dpy.rsplit_once(':')?.1.split('.').next()?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())).then(|| num.as_bytes().to_vec())
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
    /// The nested root's size in pixels — the space `QueryPointer` reports in, which is NOT
    /// necessarily the captured frame's (see [`scale_to_frame`]).
    root_size: (u16, u16),
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

/// Map a root-space pointer position into FRAME space.
///
/// `QueryPointer` answers in the nested root's coordinates, but `CursorOverlay::x/y`'s contract is
/// FRAME pixels — and gamescope's `-w/-h` (nested root) and `-W/-H` (output + PipeWire node) are
/// independent knobs. Measured on this box with gamescope 3.16.23 at `-W 1280 -H 720 -w 640 -h 360`
/// the two spaces differ by 2×, so publishing root coordinates verbatim drew the pointer at a
/// fraction of its real position. `frame` is `(0, 0)` until the PipeWire format negotiates, in which
/// case the position passes through unscaled — the same as before, and correct for the common case
/// where the two spaces agree.
///
/// The cursor BITMAP is not scaled: it stays at the shape's native size, so on a mismatched session
/// the pointer lands in the right place but is drawn at root scale.
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
        // Adopt Xwaylands that appeared since we started (a game's, above all) and retry dead ones.
        if last_discover.elapsed() >= REDISCOVER {
            last_discover = std::time::Instant::now();
            rediscover(&mut displays, &targets, false);
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
        // Root space → frame space (see `scale_to_frame`). The negotiated size arrives from the
        // PipeWire thread's `param_changed`, packed `(w << 32) | h`; `0` = not negotiated yet.
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
    use super::{
        display_number, mit_magic_cookie, pick_active, scale_to_frame, MIT_MAGIC_COOKIE_1,
    };

    /// One `.Xauthority` entry in wire form: `u16 family` + four length-prefixed byte strings.
    fn entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = family.to_be_bytes().to_vec();
        for f in [address, number, name, data] {
            v.extend_from_slice(&(f.len() as u16).to_be_bytes());
            v.extend_from_slice(f);
        }
        v
    }

    fn write_xauth(bytes: &[u8]) -> std::path::PathBuf {
        // The scratch file lives beside the test binary's temp dir; unique per call via the address
        // of a local (no rand dependency, and the tests do not run concurrently on one path).
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
        // Nothing to match on — the caller then accepts only a wildcard entry.
        assert_eq!(display_number("bogus"), None);
        assert_eq!(display_number(":"), None);
        assert_eq!(display_number(":abc"), None);
    }

    /// The cookie reader is the one PARSER in this file fed a file we did not write, so it gets the
    /// hostile-input treatment: exact-match, wildcard fallback, skipping other auth protocols, and
    /// truncation.
    #[test]
    fn mit_magic_cookie_picks_the_matching_entry() {
        let mut file = Vec::new();
        // A different protocol on the display we want — must be skipped, not returned.
        file.extend(entry(256, b"host", b"2", b"XDM-AUTHORIZATION-1", b"nope"));
        // Another display's cookie.
        file.extend(entry(
            256,
            b"host",
            b"7",
            MIT_MAGIC_COOKIE_1,
            b"other-display",
        ));
        // Ours.
        file.extend(entry(256, b"host", b"2", MIT_MAGIC_COOKIE_1, b"the-cookie"));
        let p = write_xauth(&file);
        let got = mit_magic_cookie(p.to_str().unwrap(), ":2");
        assert_eq!(
            got,
            Some((MIT_MAGIC_COOKIE_1.to_vec(), b"the-cookie".to_vec()))
        );
        // A display with no entry at all yields nothing (⇒ the caller falls back to the env path).
        assert_eq!(mit_magic_cookie(p.to_str().unwrap(), ":9"), None);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn mit_magic_cookie_falls_back_to_a_wildcard_entry() {
        // An empty `number` matches any display — but an exact match still wins.
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
        // A second entry cut off mid-length-prefix.
        file.extend(entry(256, b"host", b"5", MIT_MAGIC_COOKIE_1, b"truncated"));
        file.truncate(file.len() - 4);
        let p = write_xauth(&file);
        // No panic, no over-read: the wildcard found before the truncation still serves.
        assert_eq!(
            mit_magic_cookie(p.to_str().unwrap(), ":5").map(|(_, d)| d),
            Some(b"wildcard".to_vec())
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_garbage_xauthority_is_declined_not_fatal() {
        // A length prefix that claims far more than the file holds.
        let p = write_xauth(&[0, 0, 0xff, 0xff, 1, 2, 3]);
        assert_eq!(mit_magic_cookie(p.to_str().unwrap(), ":0"), None);
        let _ = std::fs::remove_file(p);
        // A missing file is simply "no cookie".
        assert_eq!(mit_magic_cookie("/nonexistent/pf-xauth", ":0"), None);
    }

    /// L6: gamescope's `-w/-h` (nested root, the space `QueryPointer` answers in) and `-W/-H`
    /// (output + PipeWire node, the space `CursorOverlay` is contracted in) are independent.
    #[test]
    fn pointer_positions_are_mapped_into_frame_space() {
        // The measured case: root 640x360 inside a 1280x720 output — 2x.
        assert_eq!(
            scale_to_frame((320, 180), (640, 360), (1280, 720)),
            (640, 360)
        );
        assert_eq!(scale_to_frame((0, 0), (640, 360), (1280, 720)), (0, 0));
        // Down-scaling works the same way.
        assert_eq!(
            scale_to_frame((1280, 720), (1280, 720), (640, 360)),
            (640, 360)
        );
        // Equal spaces are a pass-through (the common case — no rounding drift).
        assert_eq!(scale_to_frame((7, 9), (1920, 1080), (1920, 1080)), (7, 9));
        // Not negotiated yet, or a degenerate root: pass through rather than divide by zero.
        assert_eq!(scale_to_frame((7, 9), (1920, 1080), (0, 0)), (7, 9));
        assert_eq!(scale_to_frame((7, 9), (0, 0), (1920, 1080)), (7, 9));
    }

    /// A 5K frame times a 5K coordinate overflows `i32` — the scale must be computed wide.
    #[test]
    fn scaling_does_not_overflow_at_5k() {
        assert_eq!(
            scale_to_frame((2879, 1619), (2880, 1620), (5120, 2880)),
            (5118, 2878)
        );
    }

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
