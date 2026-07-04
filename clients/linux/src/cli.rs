//! Command-line entry paths: argv helpers, headless pairing, `--connect`, and the CI
//! screenshot scenes.

use crate::app::App;
use crate::ui_hosts::ConnectRequest;
use gtk::glib;
use gtk::prelude::*;
use std::rc::Rc;

/// The value following `flag` in argv, if present (`--flag value`).
pub fn arg_value(flag: &str) -> Option<String> {
    std::env::args()
        .skip_while(|a| a != flag)
        .nth(1)
        .filter(|v| !v.starts_with("--"))
}

/// True if argv contains `flag` (a valueless switch).
fn arg_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

/// Run the stream fullscreen with no window chrome — the Steam Deck / Gaming-Mode launch path.
/// The Decky wrapper passes `--fullscreen`; we also honor the Deck/gamescope env as a fallback
/// so a manual launch under Gaming Mode does the right thing too.
pub fn fullscreen_mode() -> bool {
    arg_flag("--fullscreen")
        || std::env::var_os("SteamDeck").is_some()
        || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
}

/// Split `host[:port]`: no colon defaults the port to 9777; a colon with an unparsable
/// port yields `None` for it (callers decide whether to default or bail).
fn parse_host_port(target: &str) -> (String, Option<u16>) {
    match target.rsplit_once(':') {
        Some((a, p)) => (a.to_string(), p.parse().ok()),
        None => (target.to_string(), Some(9777)),
    }
}

/// Run the SPAKE2 PIN ceremony without a GTK window and persist the verified host to the
/// known-hosts store as paired, so a later `--connect` connects silently. Same identity
/// store the streaming path uses (same binary), so pairing here makes the stream work.
/// Prints a one-line `paired <addr>:<port> fp=<hex>` on success; exits non-zero on failure.
pub fn headless_pair(pin: &str) -> glib::ExitCode {
    let Some(target) = arg_value("--connect") else {
        eprintln!("--pair requires --connect host[:port]");
        return glib::ExitCode::FAILURE;
    };
    let (addr, port) = parse_host_port(&target);
    let port = port.unwrap_or(9777);
    // The label the HOST stores this client under (its paired-devices list).
    let name = arg_value("--name").unwrap_or_else(|| "Steam Deck".to_string());

    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    match crate::trust::pair_with_host(&addr, port, &identity, pin, &name) {
        Ok(fp) => {
            let fp_hex = crate::trust::hex(&fp);
            crate::trust::persist_host(
                &arg_value("--host-label").unwrap_or_else(|| addr.clone()),
                &addr,
                port,
                &fp_hex,
                true,
            );
            println!("paired {addr}:{port} fp={fp_hex}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pairing failed: {e:?} (wrong PIN, or pairing not armed on the host?)");
            glib::ExitCode::FAILURE
        }
    }
}

/// `--connect host[:port]` — skip the hosts page and start a session immediately
/// (scripting + headless testing). Trust follows the same rules as a manual entry: a host
/// already pinned at this address connects silently on its stored pin; an unknown host is
/// routed to the PIN ceremony (never a silent TOFU connect — `fp_hex`/`pair_optional` are
/// unset, so `initiate_connect`'s manual arm mandates pairing).
///
/// `--launch <id>` asks the host to launch that library title (store-qualified id from
/// `--library`, e.g. `steam:570` — the Decky wrapper's `PF_LAUNCH`); the raw id doubles
/// as the stream title (best-effort — no extra fetch just for a prettier label).
pub fn cli_connect_request() -> Option<ConnectRequest> {
    if arg_value("--browse").is_some() {
        return None; // browse mode owns the session lifecycle (precedence over --connect)
    }
    let target = std::env::args().skip_while(|a| a != "--connect").nth(1)?;
    let (addr, port) = parse_host_port(&target);
    // An unparsable port (`host:notaport`) used to make the whole request `None` → the app
    // silently landed on the hosts page with no session and no message. Fall back to the
    // native default like the add-host dialog, and say so, instead of doing nothing.
    let port = port.unwrap_or_else(|| {
        eprintln!("--connect: unparsable port in '{target}', using default 9777");
        9777
    });
    Some(ConnectRequest {
        name: addr.clone(),
        addr,
        port,
        fp_hex: None,
        pair_optional: false,
        launch: arg_value("--launch").map(|id| (id.clone(), id)),
    })
}

/// `--browse host[:port]` — open the gamepad library launcher for that host instead of
/// connecting (the Decky wrapper's `PF_BROWSE`; native port, default 9777). The host must
/// already be paired: the stored pin is what lets the launcher fetch the library and
/// connect silently — no dialog can run under gamescope, so an unpaired target renders
/// the launcher's pair-first scene. Returns the request (name + stored fingerprint from
/// the known-hosts store), whether it's paired, and the mgmt port (`--mgmt <port>`, the
/// wrapper's `PF_MGMT`; default 47990 — browse mode runs no mDNS to learn it).
pub fn cli_browse_request() -> Option<(ConnectRequest, bool, u16)> {
    let target = arg_value("--browse")?;
    let (addr, port) = parse_host_port(&target);
    let port = port.unwrap_or(9777);
    let known = crate::trust::KnownHosts::load();
    let k = known
        .hosts
        .iter()
        .find(|h| h.addr == addr && h.port == port);
    let mgmt = arg_value("--mgmt")
        .and_then(|p| p.parse().ok())
        .unwrap_or(crate::library::DEFAULT_MGMT_PORT);
    Some((
        ConnectRequest {
            name: k.map_or_else(|| addr.clone(), |k| k.name.clone()),
            addr,
            port,
            fp_hex: k.map(|k| k.fp_hex.clone()),
            pair_optional: false,
            launch: None,
        },
        k.is_some_and(|k| k.paired),
        mgmt,
    ))
}

/// `--library host[:mgmt_port]` — fetch and print the host's game library over the real
/// mTLS + pinned-fingerprint client, no GTK window (scripting, and the live-API proof
/// that the library HTTP path works against a real host). The pin comes from `--fp HEX`
/// when given, else the known-hosts store (matched by address), else none (TOFU-accept).
pub fn headless_library(target: &str) -> glib::ExitCode {
    let (addr, port) = match target.rsplit_once(':') {
        Some((a, p)) if p.parse::<u16>().is_ok() => (a.to_string(), p.parse().unwrap()),
        _ => (target.to_string(), crate::library::DEFAULT_MGMT_PORT),
    };
    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    let pin = arg_value("--fp")
        .as_deref()
        .and_then(crate::trust::parse_hex32)
        .or_else(|| {
            crate::trust::KnownHosts::load()
                .hosts
                .iter()
                .find(|h| h.addr == addr)
                .and_then(|h| crate::trust::parse_hex32(&h.fp_hex))
        });
    match crate::library::fetch_games(&addr, port, &identity, pin) {
        Ok(games) => {
            for g in &games {
                println!("{}\t{}\t{}", g.id, g.store, g.title);
            }
            println!("{} game(s)", games.len());
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("library: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

/// `PUNKTFUNK_SHOT_SCENE`, when set, selects a scripted host-free scene for CI screenshots.
pub fn shot_scene() -> Option<String> {
    std::env::var("PUNKTFUNK_SHOT_SCENE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Render one mock-populated, host-free scene over the already-presented window, then print
/// `PF_SHOT_READY` once it has had a moment to map + settle so the driver knows when to capture.
/// When `PUNKTFUNK_SHOT_OUT=/path.png` is set the app CAPTURES ITSELF first (widget snapshot →
/// gsk render → PNG, see `save_png`) — no Xvfb/ImageMagick needed, and libadwaita dialogs are
/// in-window overlays so they land in the frame. No `NativeClient` or session is created. The
/// stream scene is deliberately absent — its page requires a live connector (`ui_stream::new`
/// takes an `Arc<NativeClient>`).
pub fn run_shot(app: Rc<App>, scene: &str) {
    // A plausible host for the trust/pair dialogs (fp_hex is 64 hex chars, like a real SHA-256).
    let mock_req = || ConnectRequest {
        name: "Living Room PC".to_string(),
        addr: "192.168.1.42".to_string(),
        port: 9777,
        fp_hex: Some(
            "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00".to_string(),
        ),
        pair_optional: true,
        launch: None,
    };
    let mock_advert =
        |key: &str, name: &str, addr: &str, fp: &str| crate::discovery::DiscoveredHost {
            key: key.to_string(),
            fullname: format!("{key}._punktfunk._udp.local."),
            name: name.to_string(),
            addr: addr.to_string(),
            port: 9777,
            fp_hex: fp.to_string(),
            pair: "required".to_string(),
            mgmt_port: None,
        };

    // What the self-capture renders: the main window, except for scenes that open their
    // own toplevel (the shortcuts window).
    let mut target: gtk::Widget = app.window.clone().upcast();
    match scene {
        // The saved-hosts grid reads ~/.config/punktfunk/client-known-hosts.json, which the
        // driver seeds. On top, inject synthetic adverts through the same path the mDNS
        // stream feeds: one matching the seeded saved host (ONLINE pip + dedup out of the
        // discovered grid) and one unknown pair=required host (PIN pill).
        "hosts" | "02-hosts" => {
            if let Some(h) = app.hosts_ui() {
                h.inject_advert(mock_advert(
                    "mock-online",
                    "Living Room PC",
                    "192.168.1.42",
                    "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
                ));
                h.inject_advert(mock_advert(
                    "mock-new",
                    "steamdeck",
                    "192.168.1.77",
                    "00aabbccddeeff112233445566778899a0b1c2d3e4f5061728394a5b6c7d8e9f",
                ));
            }
        }
        "settings" | "03-settings" => {
            crate::ui_settings::show(&app.window, app.settings.clone(), &app.gamepad, || {});
        }
        "trust" | "04-trust" => crate::ui_trust::tofu_dialog(app.clone(), mock_req()),
        "pair" | "05-pair" => crate::ui_trust::pin_dialog(app.clone(), mock_req()),
        "addhost" | "06-addhost" => {
            if let Some(h) = app.hosts_ui() {
                h.show_add_host();
            }
        }
        "shortcuts" | "07-shortcuts" => {
            let w = crate::app::shortcuts_window(&app.window);
            w.present();
            target = w.upcast();
        }
        // The library page with injected entries: mixed stores exercising the badge set,
        // no-art placeholders (monogram tiles), and one solid-color texture standing in
        // for a loaded poster (the real poster path, minus the network).
        "library" | "08-library" => {
            let (games, art) = mock_library();
            crate::ui_library::open_mock(app.clone(), mock_req(), games, art);
        }
        // The gamepad launcher (`--browse`) with the same injected entries — cursor sits
        // at 1 so both recede directions show; aurora + easing render frozen (shot mode).
        "gamepad-library" | "09-gamepad-library" => {
            let (games, art) = mock_library();
            let ui = crate::ui_gamepad_library::open_mock(app.clone(), mock_req(), games, art);
            app.nav.push(&ui.page);
            *app.browse.borrow_mut() = Some(ui);
        }
        other => tracing::warn!("unknown PUNKTFUNK_SHOT_SCENE={other:?}; showing hosts only"),
    }

    let settle_ms = std::env::var("PUNKTFUNK_SHOT_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let scene = scene.to_string();
    glib::timeout_add_local_once(std::time::Duration::from_millis(settle_ms), move || {
        use std::io::Write as _;
        let self_capture = std::env::var("PUNKTFUNK_SHOT_OUT")
            .ok()
            .filter(|p| !p.is_empty());
        if let Some(out) = &self_capture {
            if let Err(e) = save_png(&target, out) {
                eprintln!("PF_SHOT_ERROR scene={scene}: {e:#}");
            }
        }
        println!("PF_SHOT_READY scene={scene}");
        let _ = std::io::stdout().flush();
        // Self-capture mode: the shot is on disk — exit so back-to-back scene runs don't
        // stack windows on a live desktop. (The X11-fallback driver captures externally
        // after READY and kills us itself.)
        if self_capture.is_some() {
            std::process::exit(0);
        }
    });
}

/// The mock game set shared by the `library` and `gamepad-library` scenes: mixed stores
/// exercising the badge set, plus one solid-colour poster texture.
fn mock_library() -> (
    Vec<crate::library::GameEntry>,
    Vec<(String, gtk::gdk::Texture)>,
) {
    let game = |id: &str, store: &str, title: &str| crate::library::GameEntry {
        id: id.to_string(),
        store: store.to_string(),
        title: title.to_string(),
        art: crate::library::Artwork::default(),
    };
    let games = vec![
        game("steam:570", "steam", "Dota 2"),
        game("steam:1091500", "steam", "Cyberpunk 2077"),
        game("custom:emu-1", "custom", "RetroArch"),
        game("heroic:fortnite", "heroic", "Fortnite"),
        game("gog:witcher3", "gog", "The Witcher 3"),
        game("lutris:osu", "lutris", "osu!"),
    ];
    let art = vec![(
        "steam:570".to_string(),
        solid_texture(300, 450, 0x35, 0x84, 0xe4),
    )];
    (games, art)
}

/// A WxH single-colour RGBA texture — the `library` scene's stand-in for a fetched poster.
fn solid_texture(w: i32, h: i32, r: u8, g: u8, b: u8) -> gtk::gdk::Texture {
    let px = [r, g, b, 0xff].repeat((w * h) as usize);
    gtk::gdk::MemoryTexture::new(
        w,
        h,
        gtk::gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(px),
        (w * 4) as usize,
    )
    .upcast()
}

/// Snapshot `widget` (the whole window, dialogs included) into a PNG: WidgetPaintable →
/// `gtk::Snapshot` → the realized native's gsk renderer → `GdkTexture::save_to_png`.
fn save_png(widget: &gtk::Widget, path: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use gtk::prelude::*;
    let (w, h) = (widget.width(), widget.height());
    anyhow::ensure!(w > 0 && h > 0, "widget not laid out yet ({w}x{h})");
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(w), f64::from(h));
    let node = snapshot.to_node().context("empty snapshot")?;
    let renderer = widget
        .native()
        .context("widget not realized")?
        .renderer()
        .context("no gsk renderer")?;
    let texture = renderer.render_texture(node, None);
    texture
        .save_to_png(path)
        .with_context(|| format!("save {path}"))?;
    Ok(())
}
