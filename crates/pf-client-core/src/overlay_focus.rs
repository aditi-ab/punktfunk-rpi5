//! "A system overlay owns the controller right now" — the gamescope half of the input mask.
//!
//! On a Steam Deck in Gaming Mode the Steam menu and the QAM are drawn by Steam and driven by
//! the *same physical controller* the client is forwarding. Steam does not mask us the way it
//! masks a normal game: masking happens on Steam Input's virtual pad, and the client
//! deliberately forwards the REAL pad instead (28DE:1205 — the virtual one has no gyro,
//! trackpads or paddles). So while the QAM is up, one thumbstick drives Steam's UI *and* the
//! game on the host. This watcher is what tells [`crate::gamepad::GamepadService::set_masked`]
//! to stop that.
//!
//! **Why the free mechanism can't do it.** SDL already drops gamepad presses while the process
//! has windows but no keyboard focus (`SDL_PrivateJoystickShouldIgnoreEvent`, on by default —
//! we never set `SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS`), and on a desktop that fires. It cannot
//! fire here: gamescope resolves focus **per Xwayland ctx** (`determine_and_apply_focus` scans
//! only that ctx's window list), the Steam overlay lives in the root ctx, and the client sits
//! alone in its own. Measured on a Deck 2026-08-08: with the QAM open, X input focus inside the
//! client's ctx never moved off its window, so no `FocusOut` is ever generated. Hence an
//! explicit signal.
//!
//! **The signal.** gamescope publishes two CARDINALs on the ROOT ctx's root window (Steam mode
//! only, i.e. `gamescope -e` — which is what Gaming Mode runs):
//!
//! * `GAMESCOPE_FOCUSED_APP` — appid of the window holding **input** focus
//! * `GAMESCOPE_FOCUSED_APP_GFX` — appid of the window being **displayed**
//!
//! They are equal in normal play and diverge exactly while something else has taken input over
//! the running app. Measured, both for the Steam menu and for the QAM:
//!
//! ```text
//! app=3856846079 gfx=3856846079   ← streaming, we own input
//! app=769        gfx=3856846079   ← overlay open (769 = Steam)
//! ```
//!
//! Note `app != gfx` rather than "app is Steam": anything that takes input away from the
//! displayed app is a thing we should stop forwarding through, and comparing to our own appid
//! would need us to know it (a non-Steam shortcut's appid is assigned by Steam at creation).
//!
//! **Which display.** Not necessarily ours. Gaming Mode runs `gamescope --xwayland-count 2`:
//! Steam and the atoms live on the first server, the app is given the second, and the client's
//! own `$DISPLAY` therefore has none of these properties. So discovery walks candidates — our
//! `$DISPLAY` first (correct for a single-server gamescope), then every socket in
//! `/tmp/.X11-unix` — and keeps the first whose root actually carries both atoms. gamescope's
//! Xwayland accepts unauthenticated local connections (verified: `xprop` against it succeeds
//! with no `.Xauthority` at all), so no cookie plumbing is needed.
//!
//! Everything here is best-effort by construction: no gamescope, no X, a sandbox that cannot
//! see the other socket, or a session that restarts underneath us all end in "no signal", which
//! degrades to exactly the behaviour that shipped before this module existed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// How long to wait before rebuilding everything after the X connection drops. Gaming Mode
/// recreates its Xwayland servers across a session restart, so "gone" is not permanent — but it
/// is also not worth a hot retry loop.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Live "an overlay owns input" flag, updated by a background thread.
///
/// Cheap to poll (one relaxed atomic load), which is what the presenter's event loop wants — it
/// checks once per iteration and only talks to the gamepad service on an edge.
pub struct OverlayFocus {
    open: Arc<AtomicBool>,
}

impl OverlayFocus {
    /// Start watching, or return `None` when this isn't a gamescope Steam session (the common
    /// case — every desktop client) or the user opted out with `PUNKTFUNK_OVERLAY_MASK=0`.
    ///
    /// Returning `None` is not a failure: the caller keeps its window-focus path, which is the
    /// right signal everywhere the compositor actually moves focus.
    pub fn start() -> Option<OverlayFocus> {
        if std::env::var("PUNKTFUNK_OVERLAY_MASK").is_ok_and(|v| v == "0" || v == "false") {
            tracing::info!("overlay input mask disabled by PUNKTFUNK_OVERLAY_MASK");
            return None;
        }
        if !gamescope_session() {
            return None;
        }
        let open = Arc::new(AtomicBool::new(false));
        let flag = open.clone();
        std::thread::Builder::new()
            .name("punktfunk-overlay-focus".into())
            .spawn(move || watch(&flag))
            .map_err(|e| tracing::warn!(error = %e, "overlay focus watcher failed to start"))
            .ok()?;
        Some(OverlayFocus { open })
    }

    /// Does something other than the displayed app own input right now?
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

/// Gaming Mode / any gamescope session — the only place this signal exists. Mirrors the same
/// env checks the shells already use to detect Gaming Mode.
fn gamescope_session() -> bool {
    std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
        || std::env::var_os("SteamDeck").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.eq_ignore_ascii_case("gamescope"))
}

/// Displays worth trying, in order: ours first (a single-server gamescope publishes the atoms on
/// the display the app is already on), then every other socket present. `/tmp/.X11-unix` is
/// listed rather than probing `:0..:N` blindly so we never connect to a display that isn't there.
fn candidate_displays() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("DISPLAY") {
        if !d.is_empty() {
            out.push(d);
        }
    }
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
        let mut found: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let n = name.strip_prefix('X')?;
                n.parse::<u32>().ok().map(|n| format!(":{n}"))
            })
            .collect();
        found.sort();
        for d in found {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// The two atoms on a root that carries them, or `None` for a display that isn't gamescope's
/// root ctx. `only_if_exists` keeps this from interning atoms into unrelated X servers.
fn gamescope_atoms(conn: &RustConnection) -> Option<(Atom, Atom)> {
    let app = conn
        .intern_atom(true, b"GAMESCOPE_FOCUSED_APP")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let gfx = conn
        .intern_atom(true, b"GAMESCOPE_FOCUSED_APP_GFX")
        .ok()?
        .reply()
        .ok()?
        .atom;
    (app != 0 && gfx != 0).then_some((app, gfx))
}

/// Read one CARDINAL appid. gamescope writes these with a length of ZERO when the appid is 0
/// (`focusedAppId != 0 ? 1 : 0`), so "present but empty" is a real state meaning "no app" — it
/// must read as `None`, not as `Some(0)` that would then compare unequal to everything.
fn read_appid(conn: &RustConnection, root: Window, atom: Atom) -> Option<u32> {
    let reply = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    // Inline is sound since edition 2024: tail-expression temporaries now drop BEFORE the
    // block's locals, so the iterator borrowing `reply` no longer outlives it (the 2021 rule
    // forced a `let` binding here).
    reply.value32()?.next()
}

/// The whole decision, separated from X so it can be tested: an overlay is up exactly when
/// input focus and the displayed app are both known and DIFFER.
///
/// Absence is never an overlay. A missing value means "no app focused" (gamescope's zero-length
/// write) or "this display stopped answering" — and a mask that latched on when the signal went
/// away would silently kill the controller for the rest of the session, which is a far worse
/// failure than not masking at all.
fn overlay_open_from(app: Option<u32>, gfx: Option<u32>) -> bool {
    matches!((app, gfx), (Some(a), Some(g)) if a != g)
}

/// True when input focus and the displayed app have diverged — an overlay is up.
fn overlay_open(conn: &RustConnection, root: Window, app: Atom, gfx: Atom) -> bool {
    overlay_open_from(read_appid(conn, root, app), read_appid(conn, root, gfx))
}

/// Connect, find the root ctx, then block on PropertyNotify for the two atoms. Returns on any X
/// error so the outer loop can rebuild after a session restart.
fn watch(flag: &Arc<AtomicBool>) {
    loop {
        if let Some((conn, root, app, gfx)) = connect() {
            // Seed before the first event: the overlay may already be up when we start.
            flag.store(overlay_open(&conn, root, app, gfx), Ordering::Relaxed);
            loop {
                match conn.wait_for_event() {
                    Ok(Event::PropertyNotify(e)) if e.atom == app || e.atom == gfx => {
                        let open = overlay_open(&conn, root, app, gfx);
                        if flag.swap(open, Ordering::Relaxed) != open {
                            tracing::debug!(open, "gamescope overlay focus changed");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::info!(error = %e, "gamescope focus watcher disconnected");
                        break;
                    }
                }
            }
            // A dropped connection tells us nothing about the controller — unmask, or a
            // gamescope restart mid-overlay would leave the pad dead with nothing to revive it.
            flag.store(false, Ordering::Relaxed);
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

/// The first candidate display whose root carries both atoms, with PropertyNotify selected.
fn connect() -> Option<(RustConnection, Window, Atom, Atom)> {
    for dpy in candidate_displays() {
        // `dpy`, not `display`: `display` is one of tracing's own value helpers, and a field
        // named after it resolves to the helper inside the macro rather than to this string.
        let Ok((conn, screen_num)) = RustConnection::connect(Some(&dpy)) else {
            continue;
        };
        let Some((app, gfx)) = gamescope_atoms(&conn) else {
            continue;
        };
        let root = conn.setup().roots[screen_num].root;
        // Both atoms must actually be PRESENT on this root, not merely interned: a second
        // gamescope Xwayland knows the atom names (they are per-server strings) but only the
        // root ctx publishes the values.
        if read_appid(&conn, root, gfx).is_none() {
            continue;
        }
        // Checked rather than fire-and-forget: an event mask that silently failed to apply
        // would leave the watcher blocked forever on a display that never speaks to it.
        let selected = match conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        ) {
            Ok(cookie) => cookie.check().is_ok(),
            Err(_) => false,
        };
        if !selected {
            continue;
        }
        tracing::info!(dpy, "watching gamescope focus for overlay input masking");
        return Some((conn, root, app, gfx));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured Deck states, both directions (2026-08-08, Steam menu and QAM alike):
    /// equal appids while we own input, divergent while the overlay does.
    #[test]
    fn divergent_appids_are_an_overlay() {
        assert!(!overlay_open_from(Some(3856846079), Some(3856846079)));
        assert!(overlay_open_from(Some(769), Some(3856846079)));
    }

    /// gamescope writes these properties with a length of ZERO when the appid is 0, so "no app"
    /// arrives as a missing value rather than `Some(0)`. Reading it as `Some(0)` would make it
    /// differ from every real appid and mask the pad on an empty Gaming Mode home screen.
    #[test]
    fn a_missing_appid_is_never_an_overlay() {
        assert!(!overlay_open_from(None, Some(3856846079)));
        assert!(!overlay_open_from(Some(769), None));
        assert!(!overlay_open_from(None, None));
    }

    /// The safety property that outranks the feature: if the signal is unreadable we forward as
    /// before. A latched mask would leave a streaming session with a dead controller and no way
    /// back short of restarting it.
    #[test]
    fn absence_fails_open_not_closed() {
        assert!(!overlay_open_from(None, None));
    }
}
