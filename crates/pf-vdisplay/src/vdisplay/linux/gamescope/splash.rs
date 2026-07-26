//! The bare-spawn **splash client** — the reason a fresh headless gamescope delivers frames at all.
//!
//! gamescope only composites (and only on a composite pushes a buffer to its PipeWire capture
//! node) when a client paints. A dedicated Steam launch paints NOTHING for the whole Steam
//! bootstrap (no window maps until the gamepad UI's first frame) — so a fresh
//! spawn's capture negotiated a format, reached `Streaming`, and then starved: zero buffers, the
//! 10 s first-frame timeout, teardown — and every native-plane retry killed the half-booted Steam
//! and started from zero, while the GameStream plane died on its single wait (the "fresh gamescope
//! output never delivers frames" field failure; root-caused on-box 2026-07-27 with a raw pw_stream
//! probe: `sleep infinity` nested → 0 buffers ever, `vkcube` nested → 60/s immediately).
//!
//! This client runs INSIDE the spawned gamescope (the nested-command wrapper backgrounds it before
//! exec'ing the real app) and guarantees damage from the first second: a dark window with a subtle
//! breathing bar painted at ~2.5 Hz. Two gamescope focus facts, both live-proven on c31743d:
//!
//! * In `--steam` mode gamescope composites ONLY windows whose Steam appid is listed in the root
//!   window's `GAMESCOPECTRL_BASELAYER_APPID` property (a plain mapped+painting window — even
//!   vkcube — gets zero composites). So the splash declares itself as the Steam client UI
//!   (`STEAM_GAME=769`) and seeds the baselayer with that appid iff nobody has written it yet.
//!   When Steam finishes booting it rewrites the baselayer as part of launching the game, which
//!   hands composite focus to the real game window with no action on our side (verified: 2.5 Hz
//!   splash cadence → 60 Hz game cadence on the same capture stream, no gap).
//! * In plain (non-`--steam`) mode any mapped painting window composites; the atoms are inert.
//!
//! The splash never exits on its own while the session lives: after the game takes focus it keeps
//! painting unfocused (damage on an unfocused window schedules no composite — harmless), so if the
//! game's window briefly goes away (Steam updater closed, level-load window swap) and gamescope
//! re-focuses the splash, liveness returns instead of a frozen last frame. It dies with the session:
//! gamescope's reaper kills it at teardown, and any X error (the connection dropping) exits cleanly.

use anyhow::{Context, Result};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ConnectionExt, CreateGCAux, CreateWindowAux, PropMode, Rectangle,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The Steam client UI's appid — the identity the splash borrows so `--steam`-mode gamescope
/// treats it as focusable before (and beneath) any real game.
const STEAM_UI_APPID: u32 = 769;

/// Window/bar palette: near-black background, greys the bar breathes through.
const BG: u32 = 0x0d0d0d;

/// How often the splash repaints (each paint is the damage that makes gamescope push a capture
/// buffer). 400 ms ≈ 2.5 fps — far inside every first-frame budget, invisible in CPU terms.
const TICK: Duration = Duration::from_millis(400);

/// Entry point for the hidden `gamescope-splash` host subcommand. Blocks for the session's
/// lifetime; returns (or errors) only when the X connection is gone or never came up.
pub(crate) fn run() -> Result<()> {
    // gamescope execs its nested command only once Xwayland is ready and DISPLAY is set, so the
    // first attempt normally lands — the retry covers a slow Xwayland under driver-init load.
    let (conn, screen_num) = connect_with_retry()?;
    let screen = &conn.setup().roots[screen_num];
    let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(BG),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"Punktfunk",
    )?;
    // STEAM_GAME on the window + GAMESCOPECTRL_BASELAYER_APPID on the root — the pair that makes a
    // `--steam` gamescope composite us. Seed the baselayer only while unset: once Steam is up it
    // owns that property (its rewrite at game launch is exactly the handover we want).
    let steam_game = conn.intern_atom(false, b"STEAM_GAME")?.reply()?.atom;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        steam_game,
        AtomEnum::CARDINAL,
        &[STEAM_UI_APPID],
    )?;
    let baselayer = conn
        .intern_atom(false, b"GAMESCOPECTRL_BASELAYER_APPID")?
        .reply()?
        .atom;
    let existing = conn
        .get_property(false, screen.root, baselayer, AtomEnum::CARDINAL, 0, 4)?
        .reply()?;
    if existing.value_len == 0 {
        conn.change_property32(
            PropMode::REPLACE,
            screen.root,
            baselayer,
            AtomEnum::CARDINAL,
            &[STEAM_UI_APPID],
        )?;
    }
    conn.map_window(win)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(BG))?;
    conn.flush()?;
    tracing::info!(
        w,
        h,
        seeded_baselayer = existing.value_len == 0,
        "gamescope splash: mapped"
    );

    // The breathing bar. Server-side fills generate the damage; the background pixel handles
    // exposures, so no event loop is needed. Any request failing = the session (or its Xwayland)
    // is gone = a clean exit.
    let bar_w = 120i16;
    let bar_h = 8i16;
    let bar = Rectangle {
        x: (w as i16) / 2 - bar_w / 2,
        y: (h as i16) / 2 - bar_h / 2,
        width: bar_w as u16,
        height: bar_h as u16,
    };
    let mut t: u32 = 0;
    loop {
        // 8-step triangle pulse through dark greys (0x1a…0x40 per channel).
        let phase = (t % 8) as i32;
        let tri = if phase < 4 { phase } else { 8 - phase };
        let grey = 0x1a + 0x0c * tri as u32;
        let colour = (grey << 16) | (grey << 8) | grey;
        conn.change_gc(gc, &ChangeGCAux::new().foreground(colour))?;
        conn.poly_fill_rectangle(win, gc, &[bar])?;
        conn.flush()
            .context("gamescope splash: X connection lost")?;
        t = t.wrapping_add(1);
        std::thread::sleep(TICK);
    }
}

/// Connect to the session's `DISPLAY`, retrying briefly — gamescope sets the variable before
/// exec'ing the nested command, but a slow Xwayland under cold driver init gets a grace window.
fn connect_with_retry() -> Result<(RustConnection, usize)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match x11rb::connect(None) {
            Ok(ok) => return Ok(ok),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(e).context("gamescope splash: could not connect to the session DISPLAY")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}
