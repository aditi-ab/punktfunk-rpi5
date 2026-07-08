//! Command-line entry paths: argv helpers, the headless flows (pair/wake/library), the
//! exec handoff to `punktfunk-session` for `--connect`/`--browse`, and the CI screenshot
//! scenes.

use crate::app::AppModel;
use crate::trust::{KnownHost, KnownHosts};
use crate::ui_hosts::{ConnectRequest, HostsMsg};
use gtk::glib;
use gtk::prelude::*;
use punktfunk_core::client::NativeClient;
use relm4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// The handles `run_shot` needs — cloned out of `AppModel` before it moves into the
/// component parts, so the scene can be dispatched from the window's `map` callback.
pub struct ShotCtx {
    pub window: adw::ApplicationWindow,
    pub nav: adw::NavigationView,
    pub hosts: relm4::Sender<HostsMsg>,
    pub settings: Rc<RefCell<crate::trust::Settings>>,
    pub gamepad: crate::gamepad::GamepadService,
    pub identity: (String, String),
    pub sender: ComponentSender<AppModel>,
}

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

/// Fullscreen the shell — the Gaming-Mode fallback for a bare launch (streams and the
/// console library exec the session binary, which handles its own fullscreen).
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

/// `--connect` / `--browse`: streams and the console library live in the
/// `punktfunk-session` Vulkan binary — replace this process with it, forwarding the
/// relevant argv verbatim. This keeps the Decky wrapper (which launches the SHELL with
/// these flags) working unchanged until it's repointed at the session binary.
pub fn exec_session() -> glib::ExitCode {
    use std::os::unix::process::CommandExt as _;
    let forward = [
        "--connect",
        "--browse",
        "--fp",
        "--launch",
        "--mgmt",
        "--connect-timeout",
    ];
    let mut cmd = std::process::Command::new(crate::spawn::session_binary());
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        if a == "--fullscreen" || a == "--stats" {
            cmd.arg(a);
        } else if forward.contains(&a.as_str()) {
            cmd.arg(&a);
            if let Some(v) = args.peek() {
                if !v.starts_with("--") {
                    cmd.arg(args.next().unwrap());
                }
            }
        }
    }
    let err = cmd.exec(); // only returns on failure
    eprintln!("exec punktfunk-session: {err}");
    glib::ExitCode::FAILURE
}

/// Run the SPAKE2 PIN ceremony without a GTK window and persist the verified host to the
/// known-hosts store as paired, so a later `--connect` connects silently. Same identity
/// store the streaming path uses, so pairing here makes the stream work.
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
            // A host manually added via `--add-host` (no fingerprint yet) is stored as an
            // addr-keyed placeholder; now that the ceremony yielded the real fingerprint,
            // `persist_host` created the fp-keyed entry — drop the placeholder so the list
            // shows this host once, not twice.
            forget_placeholder(&addr, port);
            println!("paired {addr}:{port} fp={fp_hex}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pairing failed: {e:?} (wrong PIN, or pairing not armed on the host?)");
            glib::ExitCode::FAILURE
        }
    }
}

/// `--wake host[:port]` — send a Wake-on-LAN magic packet to a saved host and exit,
/// without opening a window. The MAC comes from the known-hosts store (learned from the
/// host's mDNS `mac` TXT while it was online); exits non-zero if none is known yet.
pub fn cli_wake() -> glib::ExitCode {
    let Some(target) = arg_value("--wake") else {
        eprintln!("--wake requires host[:port]");
        return glib::ExitCode::FAILURE;
    };
    let (addr, port) = parse_host_port(&target);
    let port = port.unwrap_or(9777);
    let mac = crate::trust::KnownHosts::load()
        .hosts
        .iter()
        .find(|h| h.addr == addr && h.port == port)
        .map(|h| h.mac.clone())
        .unwrap_or_default();
    if mac.is_empty() {
        eprintln!(
            "--wake: no MAC known for {addr}:{port} — connect once while the host is awake so its \
             advertised MAC is learned"
        );
        return glib::ExitCode::FAILURE;
    }
    crate::wol::wake(&mac, addr.parse().ok());
    println!("woke {addr}:{port} ({} MAC(s) targeted)", mac.len());
    glib::ExitCode::SUCCESS
}

/// `--library host[:mgmt_port]` — fetch and print the host's game library over the real
/// mTLS + pinned-fingerprint client, no GTK window. The pin comes from `--fp HEX` when
/// given, else the known-hosts store (matched by address), else none (TOFU-accept).
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

// -----------------------------------------------------------------------------------------
// Headless host-store management — the shared known-hosts store (`client-known-hosts.json`)
// is the SINGLE source of truth for every client on this device (the GTK shell, the Vulkan
// session, and the Decky plugin, which shells out to these modes). Exposing add/edit/forget/
// list/reset here lets the Decky Gaming-Mode UI mutate exactly the store the desktop client
// reads, so a change in one surface shows up in the other. Reachability (`--list-hosts
// --probe`, `--reachable`) answers "is this host online?" WITHOUT mDNS, so a host reached
// over a routed network (Tailscale/VPN/another subnet) no longer reads as offline.
// -----------------------------------------------------------------------------------------

/// The per-probe budget: a cold host on a routed link answers in well under this, and every
/// saved host is probed in parallel so the wall-clock cost is one timeout, not the sum.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

/// Selector for `--set-host`/`--forget-host`: a 64-hex fingerprint pins one entry across IP
/// changes; anything else is treated as `addr[:port]` (manual entries have no fingerprint).
enum Selector {
    Fp(String),
    Addr(String, u16),
}

fn parse_selector(s: &str) -> Selector {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Selector::Fp(s.to_lowercase())
    } else {
        let (addr, port) = parse_host_port(s);
        Selector::Addr(addr, port.unwrap_or(9777))
    }
}

impl Selector {
    fn matches(&self, h: &KnownHost) -> bool {
        match self {
            Selector::Fp(fp) => h.fp_hex.eq_ignore_ascii_case(fp),
            Selector::Addr(addr, port) => h.addr == *addr && h.port == *port,
        }
    }
}

/// Probe every saved host for reachability in parallel (shared with the hosts-page presence pips).
fn probe_all(hosts: &[KnownHost]) -> Vec<bool> {
    crate::trust::probe_reachable_many(
        hosts.iter().map(|h| (h.addr.clone(), h.port)).collect(),
        PROBE_TIMEOUT,
    )
}

/// Drop an fp-less placeholder for `addr:port` (see `headless_pair`). No-op when none exists.
fn forget_placeholder(addr: &str, port: u16) {
    let mut known = KnownHosts::load();
    let before = known.hosts.len();
    known
        .hosts
        .retain(|h| !(h.fp_hex.is_empty() && h.addr == addr && h.port == port));
    if known.hosts.len() != before {
        let _ = known.save();
    }
}

/// `--list-hosts [--probe]` — the saved known-hosts store as JSON (the store the Decky plugin
/// renders). With `--probe`, each host carries an `online` bool from a live reachability probe
/// (mDNS-independent); without it, `online` is `null` (unknown — the caller falls back to its
/// own mDNS view). Shape: `{"hosts":[{name,addr,port,fp_hex,paired,mac,last_used,online}]}`.
pub fn headless_list_hosts() -> glib::ExitCode {
    let known = KnownHosts::load();
    let online: Option<Vec<bool>> = arg_flag("--probe").then(|| probe_all(&known.hosts));
    let hosts: Vec<serde_json::Value> = known
        .hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            serde_json::json!({
                "name": h.name,
                "addr": h.addr,
                "port": h.port,
                "fp_hex": h.fp_hex,
                "paired": h.paired,
                "mac": h.mac,
                "last_used": h.last_used,
                "online": online.as_ref().map(|v| serde_json::Value::Bool(v[i]))
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    match serde_json::to_string(&serde_json::json!({ "hosts": hosts })) {
        Ok(s) => {
            println!("{s}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("list-hosts: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

/// `--reachable host[:port]` — probe one target and exit 0 (reachable) / 1 (not). A cheap
/// "is it online / test this address" check that never touches mDNS.
pub fn headless_reachable(target: &str) -> glib::ExitCode {
    let (addr, port) = parse_host_port(target);
    let port = port.unwrap_or(9777);
    if NativeClient::probe(&addr, port, PROBE_TIMEOUT) {
        println!("reachable {addr}:{port}");
        glib::ExitCode::SUCCESS
    } else {
        eprintln!("unreachable {addr}:{port}");
        glib::ExitCode::FAILURE
    }
}

/// `--add-host host[:port] [--host-label NAME] [--fp HEX]` — save a host by address so it can
/// be paired/streamed even when mDNS never sees it (a Tailscale/VPN box). Without `--fp` the
/// entry is an unpaired placeholder (keyed by address) the user pairs later — `headless_pair`
/// then replaces it with the fingerprinted entry. With `--fp` (e.g. carried from an advert)
/// it is pinned immediately as trusted-but-not-PIN-paired. Prints `added <addr>:<port>`.
pub fn headless_add_host(target: &str) -> glib::ExitCode {
    let (addr, port) = parse_host_port(target);
    let port = port.unwrap_or(9777);
    let name = arg_value("--host-label")
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| addr.clone());
    if let Some(fp_hex) = arg_value("--fp").filter(|f| crate::trust::parse_hex32(f).is_some()) {
        // Fingerprint known up front: upsert the pinned entry (paired stays false — no PIN
        // ceremony happened; a later `--pair` upgrades it).
        crate::trust::persist_host(&name, &addr, port, &fp_hex.to_lowercase(), false);
        forget_placeholder(&addr, port);
        println!("added {addr}:{port} fp={}", fp_hex.to_lowercase());
        return glib::ExitCode::SUCCESS;
    }
    // No fingerprint yet — an address-keyed placeholder. Refresh the name if it already exists.
    let mut known = KnownHosts::load();
    if let Some(h) = known
        .hosts
        .iter_mut()
        .find(|h| h.addr == addr && h.port == port)
    {
        h.name = name;
    } else {
        known.hosts.push(KnownHost {
            name,
            addr: addr.clone(),
            port,
            fp_hex: String::new(),
            paired: false,
            last_used: None,
            mac: Vec::new(),
        });
    }
    match known.save() {
        Ok(()) => {
            println!("added {addr}:{port}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("add-host: {e:#}");
            glib::ExitCode::FAILURE
        }
    }
}

/// `--set-host <fp|host[:port]> [--host-label NAME] [--addr ADDR] [--port PORT]` — edit a saved
/// host: rename and/or re-point its address. Identified by fingerprint (survives IP changes) or
/// current address. Prints `updated <name>`; fails if nothing matched.
pub fn headless_set_host(selector: &str) -> glib::ExitCode {
    let sel = parse_selector(selector);
    let mut known = KnownHosts::load();
    let Some(h) = known.hosts.iter_mut().find(|h| sel.matches(h)) else {
        eprintln!("set-host: no saved host matches {selector:?}");
        return glib::ExitCode::FAILURE;
    };
    if let Some(name) = arg_value("--host-label").map(|n| n.trim().to_string()) {
        if !name.is_empty() {
            h.name = name;
        }
    }
    if let Some(addr) = arg_value("--addr").map(|a| a.trim().to_string()) {
        if !addr.is_empty() {
            h.addr = addr;
        }
    }
    if let Some(port) = arg_value("--port").and_then(|p| p.trim().parse::<u16>().ok()) {
        h.port = port;
    }
    let label = h.name.clone();
    match known.save() {
        Ok(()) => {
            println!("updated {label}");
            glib::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("set-host: {e:#}");
            glib::ExitCode::FAILURE
        }
    }
}

/// `--forget-host <fp|host[:port]>` — remove a saved host (drops the pinned fingerprint; a later
/// connect must re-pair/trust). Prints `forgot N`; succeeds even if nothing matched (idempotent).
pub fn headless_forget_host(selector: &str) -> glib::ExitCode {
    let sel = parse_selector(selector);
    let mut known = KnownHosts::load();
    let before = known.hosts.len();
    known.hosts.retain(|h| !sel.matches(h));
    let removed = before - known.hosts.len();
    if removed > 0 {
        if let Err(e) = known.save() {
            eprintln!("forget-host: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    }
    println!("forgot {removed}");
    glib::ExitCode::SUCCESS
}

/// `--reset` — clear this device's client state: the saved known-hosts and the stream
/// settings. The persistent IDENTITY (`client-cert.pem`/`client-key.pem`) is deliberately
/// KEPT so the box isn't seen as a brand-new device everywhere (a re-pair still re-adds hosts);
/// a caller wanting a true factory reset removes those separately. Missing files are fine.
pub fn headless_reset() -> glib::ExitCode {
    let Ok(dir) = crate::trust::config_dir() else {
        eprintln!("reset: could not resolve config dir (HOME unset?)");
        return glib::ExitCode::FAILURE;
    };
    let mut ok = true;
    for name in ["client-known-hosts.json", "client-gtk-settings.json"] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("reset: {name}: {e}");
                ok = false;
            }
        }
    }
    if ok {
        println!("reset");
        glib::ExitCode::SUCCESS
    } else {
        glib::ExitCode::FAILURE
    }
}

/// Dispatch the headless host-store modes (returns `None` when argv names none of them, so the
/// caller proceeds to launch the GTK app). Kept in one place so `arg_flag` stays private and the
/// dispatch in `app.rs` is a single line.
pub fn headless_host_command() -> Option<glib::ExitCode> {
    if arg_flag("--list-hosts") {
        return Some(headless_list_hosts());
    }
    if let Some(t) = arg_value("--reachable") {
        return Some(headless_reachable(&t));
    }
    if let Some(t) = arg_value("--add-host") {
        return Some(headless_add_host(&t));
    }
    if let Some(s) = arg_value("--set-host") {
        return Some(headless_set_host(&s));
    }
    if let Some(s) = arg_value("--forget-host") {
        return Some(headless_forget_host(&s));
    }
    if arg_flag("--reset") {
        return Some(headless_reset());
    }
    None
}

/// `PUNKTFUNK_SHOT_SCENE`, when set, selects a scripted host-free scene for CI screenshots.
pub fn shot_scene() -> Option<String> {
    std::env::var("PUNKTFUNK_SHOT_SCENE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Render one mock-populated, host-free scene over the already-presented window, then
/// print `PF_SHOT_READY` once it has settled. When `PUNKTFUNK_SHOT_OUT=/path.png` is set
/// the app CAPTURES ITSELF (widget snapshot → gsk render → PNG) — no Xvfb/ImageMagick
/// needed. The stream and gamepad-library scenes are gone with the pages (both live in
/// the session binary now).
pub fn run_shot(ctx: &ShotCtx, scene: &str) {
    let sender = &ctx.sender;
    // A plausible host for the trust/pair dialogs (fp_hex = 64 hex chars).
    let mock_req = || ConnectRequest {
        name: "Living Room PC".to_string(),
        addr: "192.168.1.42".to_string(),
        port: 9777,
        fp_hex: Some(
            "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00".to_string(),
        ),
        pair_optional: true,
        launch: None,
        mac: Vec::new(),
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
            mac: Vec::new(),
        };

    // What the self-capture renders: the main window, except for scenes that open their
    // own toplevel (the shortcuts window).
    let mut target: gtk::Widget = ctx.window.clone().upcast();
    let hosts = &ctx.hosts;
    match scene {
        // Saved hosts come from the seeded known-hosts store; on top, inject synthetic
        // adverts through the same path the mDNS stream feeds.
        "hosts" | "02-hosts" => {
            let _ = hosts.send(HostsMsg::Advert(mock_advert(
                "mock-online",
                "Living Room PC",
                "192.168.1.42",
                "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
            )));
            let _ = hosts.send(HostsMsg::Advert(mock_advert(
                "mock-new",
                "steamdeck",
                "192.168.1.77",
                "00aabbccddeeff112233445566778899a0b1c2d3e4f5061728394a5b6c7d8e9f",
            )));
        }
        "settings" | "03-settings" => {
            crate::ui_settings::show(&ctx.window, ctx.settings.clone(), &ctx.gamepad, || {});
        }
        "trust" | "04-trust" => crate::ui_trust::tofu_dialog(&ctx.window, sender, mock_req()),
        "pair" | "05-pair" => {
            crate::ui_trust::pin_dialog(&ctx.window, sender, ctx.identity.clone(), mock_req())
        }
        "addhost" | "06-addhost" => {
            let _ = hosts.send(HostsMsg::ShowAddHost);
        }
        "shortcuts" | "07-shortcuts" => {
            let w = crate::app::shortcuts_window(&ctx.window);
            w.present();
            target = w.upcast();
        }
        // The library page with injected entries: mixed stores exercising the badge set,
        // no-art placeholders, and one solid-color texture standing in for a poster.
        "library" | "08-library" => {
            let (games, art) = mock_library();
            crate::ui_library::open_mock(
                &ctx.nav,
                ctx.identity.clone(),
                sender,
                mock_req(),
                games,
                art,
            );
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
        // Self-capture of the dialog scenes (trust/pair/settings/addhost) needs a GL
        // renderer: `WidgetPaintable(window)` under the cairo software renderer doesn't
        // composite the `AdwDialog` overlay layer (the dialog IS presented — the
        // page-content scenes capture fine either way; CI uses GL or the X11 root-grab).
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
        // Self-capture mode: the shot is on disk — exit so back-to-back scene runs
        // don't stack windows on a live desktop.
        if self_capture.is_some() {
            std::process::exit(0);
        }
    });
}

/// The mock game set for the `library` scene: mixed stores exercising the badge set,
/// plus one solid-colour poster texture.
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
