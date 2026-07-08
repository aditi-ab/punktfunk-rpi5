//! `--browse host[:port]` — the console game library (phase 4b of the plan): the Skia
//! coverflow idles in the session window, A launches the focused title as a stream in
//! the SAME window (no gamescope window handoff — the whole point of one process), the
//! session's end returns to the library, B quits to Gaming Mode.
//!
//! The host must already be paired (the stored pin fetches the library and connects
//! silently; no ceremony can run under gamescope) — an unpaired target renders the
//! pair-first scene. `PUNKTFUNK_FAKE_LIBRARY=<file.json>` feeds canned entries with no
//! host (portrait paths starting with `/` load from disk), the GPU-only dev path.

use crate::session_main::{
    arg_flag, arg_value, fullscreen_mode, parse_host_port, session_params, window_pos,
};
use pf_client_core::{library, trust};
use pf_console_ui::{LibraryGame, LibraryPhase, LibraryShared, SkiaOverlay};
use pf_presenter::overlay::OverlayAction;
use pf_presenter::ActionOutcome;
use std::collections::VecDeque;

pub fn run(target: &str) -> u8 {
    let (addr, port) = parse_host_port(target);
    let known = trust::KnownHosts::load();
    let k = known
        .hosts
        .iter()
        .find(|h| h.addr == addr && h.port == port);
    let host_label = k.map_or_else(|| addr.clone(), |h| h.name.clone());
    let paired = k.is_some_and(|h| h.paired);
    let pin = k.and_then(|h| trust::parse_hex32(&h.fp_hex));
    let mgmt = arg_value("--mgmt")
        .and_then(|p| p.parse().ok())
        .unwrap_or(library::DEFAULT_MGMT_PORT);

    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return crate::session_main::EXIT_CONNECT_FAILED;
        }
    };
    let settings = trust::Settings::load();

    let (overlay, shared) = match SkiaOverlay::with_library(host_label.clone()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("console UI: {e:#}");
            return crate::session_main::EXIT_PRESENTER_FAILED;
        }
    };

    // The library fetch — paired hosts only (the fake-library hook exists precisely for
    // host-less/pairing-less UI work).
    let fake = std::env::var_os("PUNKTFUNK_FAKE_LIBRARY").is_some();
    if paired || fake {
        spawn_fetch(shared.clone(), addr.clone(), mgmt, identity.clone(), pin);
    } else {
        shared.set_phase(LibraryPhase::PairFirst);
    }

    // `--json-status`: a shell parent is reading stdout (the WinUI shell hides itself on
    // `{"ready":true}` and restores on exit) — plain CLI/gamescope runs stay silent.
    let json_status = arg_flag("--json-status");
    let opts = pf_presenter::SessionOpts {
        window_title: format!("Punktfunk · {host_label}"),
        fullscreen: fullscreen_mode(),
        window_pos: window_pos(),
        print_stats: settings.show_stats || arg_flag("--stats"),
        json_status,
        on_connected: Some(Box::new(|fingerprint: [u8; 32]| {
            trust::touch_last_used(&trust::hex(&fingerprint));
        })),
        overlay: Some(Box::new(overlay)),
    };

    let result =
        pf_presenter::run_browse(opts, |action, gamepad, native, force_software, vulkan| {
            match action {
                OverlayAction::Launch { id, title } => {
                    // The carousel only renders for a paired host, so the pin exists; the
                    // guard keeps a logic slip from turning into a pinless connect.
                    let Some(pin) = pin else {
                        tracing::warn!("launch without a stored pin — refusing");
                        return ActionOutcome::Handled;
                    };
                    tracing::info!(%id, %title, "launching from the library");
                    ActionOutcome::Start(Box::new(session_params(
                        &settings,
                        addr.clone(),
                        port,
                        pin,
                        identity.clone(),
                        Some(id),
                        gamepad,
                        native,
                        force_software,
                        vulkan,
                    )))
                }
                OverlayAction::Retry => {
                    spawn_fetch(shared.clone(), addr.clone(), mgmt, identity.clone(), pin);
                    ActionOutcome::Handled
                }
                OverlayAction::Quit => ActionOutcome::Quit,
            }
        });

    match result {
        Ok(()) => 0,
        Err(e) => {
            // The shell contract's terminal line (a clean quit needs none — stdout EOF
            // already routes the shell back to its host list silently).
            if json_status {
                crate::session_main::json_line("error", &format!("{e:#}"), Some(false));
            }
            eprintln!("browse: {e:#}");
            crate::session_main::EXIT_PRESENTER_FAILED
        }
    }
}

/// Fetch the library off the main thread, then stream poster art into the shared model
/// as results land (the GTK launcher's `load` + `load_art`, minus the main-loop hops —
/// the renderer drains `push_art` per frame).
fn spawn_fetch(
    shared: LibraryShared,
    addr: String,
    mgmt: u16,
    identity: (String, String),
    pin: Option<[u8; 32]>,
) {
    shared.set_phase(LibraryPhase::Loading);
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            if let Ok(path) = std::env::var("PUNKTFUNK_FAKE_LIBRARY") {
                load_fake(&shared, &path);
                return;
            }
            match library::fetch_games(&addr, mgmt, &identity, pin) {
                Ok(games) => {
                    let base = library::base_url(&addr, mgmt);
                    let jobs: VecDeque<(String, Vec<String>)> = games
                        .iter()
                        .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
                        .filter(|(_, candidates)| !candidates.is_empty())
                        .collect();
                    shared.set_games(
                        games
                            .iter()
                            .map(|g| LibraryGame {
                                id: g.id.clone(),
                                title: g.title.clone(),
                                store: g.store.clone(),
                            })
                            .collect(),
                    );
                    if !jobs.is_empty() {
                        let rx = library::spawn_art_fetch(base, identity, pin, jobs);
                        while let Ok((id, bytes)) = rx.recv_blocking() {
                            shared.push_art(id, bytes);
                        }
                    }
                }
                Err(e) => shared.set_phase(LibraryPhase::Error {
                    title: "Couldn't load the library".into(),
                    body: e.to_string(),
                    can_retry: true,
                }),
            }
        })
        .ok();
}

/// Dev hook: entries from a JSON file; portrait paths starting with `/` load from disk.
fn load_fake(shared: &LibraryShared, path: &str) {
    let games: Vec<library::GameEntry> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for g in &games {
        if let Some(p) = g.art.portrait.as_deref().filter(|p| p.starts_with('/')) {
            if let Ok(bytes) = std::fs::read(p) {
                shared.push_art(g.id.clone(), bytes);
            }
        }
    }
    shared.set_games(
        games
            .iter()
            .map(|g| LibraryGame {
                id: g.id.clone(),
                title: g.title.clone(),
                store: g.store.clone(),
            })
            .collect(),
    );
}
