//! Gamescope overlay input mask. Tells [`crate::gamepad::GamepadService::set_masked`]
//! to stop forwarding the real pad while Steam's menu or QAM owns it. SDL's
//! unfocused-window drop cannot fire here: gamescope focuses per Xwayland ctx,
//! the overlay lives in the root ctx, and the client sits alone in its own.
//!
//! Signal: `GAMESCOPE_FOCUSED_APP` vs `GAMESCOPE_FOCUSED_APP_GFX` on the root
//! ctx's root window (`gamescope -e` only). Equal in play; they diverge when
//! something else has taken input. Compare inequality, not "app is Steam" —
//! a non-Steam shortcut's appid is assigned at creation.
//!
//! Gaming Mode uses two Xwaylands; atoms live on the first, `$DISPLAY` is
//! the second. Walk `$DISPLAY` then `/tmp/.X11-unix` and keep the first root
//! that carries both. No cookie: gamescope Xwayland accepts local connections.
//!
//! Best-effort: no gamescope, no X, or a sandbox that cannot see the socket
//! all mean "no signal" and fail open (forward as before).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// After X drops. Gaming Mode recreates Xwayland on session restart; a hot
/// retry loop is not worth it.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Overlay-owns-input flag from the watcher thread. Relaxed load: the
/// presenter polls each frame and talks to the gamepad service on an edge.
pub struct OverlayFocus {
    open: Arc<AtomicBool>,
}

impl OverlayFocus {
    /// `None` when this is not a gamescope Steam session, or when
    /// `PUNKTFUNK_OVERLAY_MASK=0`. The caller then keeps its window-focus path,
    /// which is the right signal everywhere the compositor actually moves focus.
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

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

/// The only place this signal exists. Same env checks the shells use for
/// Gaming Mode.
pub fn gamescope_session() -> bool {
    crate::gamescope::under_gamescope()
        || std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.eq_ignore_ascii_case("gamescope"))
}

/// `$DISPLAY` first (single-server gamescope publishes on the app's display),
/// then every socket in `/tmp/.X11-unix`. Do not probe `:0..:N` — that would
/// connect to displays that are not there.
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

/// `None` if this display is not the root ctx. `only_if_exists` so we do not
/// intern the names into an unrelated X server.
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

/// gamescope writes length 0 when the appid is 0 (`focusedAppId != 0 ? 1 : 0`).
/// Empty must be `None`, not `Some(0)` — that would differ from every real id.
fn read_appid(conn: &RustConnection, root: Window, atom: Atom) -> Option<u32> {
    let reply = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    // Edition 2024: tail-expression temporaries drop before the block's locals,
    // so the iterator borrowing `reply` no longer outlives it.
    reply.value32()?.next()
}

/// Overlay iff both appids are known and differ. Absence is never an overlay:
/// a latched mask would kill the pad for the rest of the session.
fn overlay_open_from(app: Option<u32>, gfx: Option<u32>) -> bool {
    matches!((app, gfx), (Some(a), Some(g)) if a != g)
}

fn overlay_open(conn: &RustConnection, root: Window, app: Atom, gfx: Atom) -> bool {
    overlay_open_from(read_appid(conn, root, app), read_appid(conn, root, gfx))
}

/// Block on PropertyNotify for the two atoms. Any X error returns so the
/// outer loop can rebuild after a session restart.
fn watch(flag: &Arc<AtomicBool>) {
    loop {
        if let Some((conn, root, app, gfx)) = connect() {
            // Seed before the first event: the overlay may already be up.
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
            // Unmask: a restart mid-overlay would otherwise leave the pad dead.
            flag.store(false, Ordering::Relaxed);
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

fn connect() -> Option<(RustConnection, Window, Atom, Atom)> {
    for dpy in candidate_displays() {
        // `dpy`, not `display`: tracing's value helper would steal a field named
        // `display` inside the macro.
        let Ok((conn, screen_num)) = RustConnection::connect(Some(&dpy)) else {
            continue;
        };
        let Some((app, gfx)) = gamescope_atoms(&conn) else {
            continue;
        };
        let root = conn.setup().roots[screen_num].root;
        // Interned names are not enough: a second gamescope Xwayland knows the
        // strings; only the root ctx publishes values.
        if read_appid(&conn, root, gfx).is_none() {
            continue;
        }
        // Check the event mask applied. A silent fail would block forever on a
        // display that never speaks.
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

    #[test]
    fn divergent_appids_are_an_overlay() {
        assert!(!overlay_open_from(Some(3856846079), Some(3856846079)));
        assert!(overlay_open_from(Some(769), Some(3856846079)));
    }

    #[test]
    fn a_missing_appid_is_never_an_overlay() {
        assert!(!overlay_open_from(None, Some(3856846079)));
        assert!(!overlay_open_from(Some(769), None));
        assert!(!overlay_open_from(None, None));
    }

    #[test]
    fn absence_fails_open_not_closed() {
        assert!(!overlay_open_from(None, None));
    }
}
