//! The application shell: window, navigation, trust dialogs, session lifecycle.

use crate::session::{SessionEvent, SessionParams};
use crate::trust::{KnownHost, KnownHosts, Settings};
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::{gdk, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref};
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "io.unom.Punktfunk";

struct App {
    window: adw::ApplicationWindow,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    settings: Rc<RefCell<Settings>>,
    identity: (String, String),
    /// App-lifetime SDL gamepad service: Settings list + per-session capture/feedback.
    gamepad: crate::gamepad::GamepadService,
    /// One session at a time — ignore connects while one is starting/running.
    busy: std::cell::Cell<bool>,
    /// Steam Deck / Gaming-Mode launch: fullscreen the window (chrome-less) when a stream starts.
    fullscreen: bool,
}

impl App {
    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

pub fn run() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // Headless pairing path (no GTK window): `--pair <PIN> --connect host[:port] [--name N]`.
    // Used by the Decky plugin (a GTK dialog can't pop under gamescope) and for scripting.
    if let Some(pin) = arg_value("--pair") {
        return headless_pair(&pin);
    }
    let mut builder = adw::Application::builder().application_id(APP_ID);
    // Screenshot mode launches the app once per scene back-to-back; NON_UNIQUE keeps each
    // launch its own primary instance instead of forwarding to a still-registered name.
    if shot_scene().is_some() {
        builder = builder.flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
    }
    let app = builder.build();
    app.connect_activate(build_ui);
    // GTK doesn't see our argv (`--connect` is handled in `build_ui`); an empty argv also
    // keeps GApplication from rejecting unknown options.
    app.run_with_args(&[] as &[&str])
}

/// The value following `flag` in argv, if present (`--flag value`).
fn arg_value(flag: &str) -> Option<String> {
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
fn fullscreen_mode() -> bool {
    arg_flag("--fullscreen")
        || std::env::var_os("SteamDeck").is_some()
        || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
}

/// Run the SPAKE2 PIN ceremony without a GTK window and persist the verified host to the
/// known-hosts store as paired, so a later `--connect` connects silently. Same identity
/// store the streaming path uses (same binary), so pairing here makes the stream work.
/// Prints a one-line `paired <addr>:<port> fp=<hex>` on success; exits non-zero on failure.
fn headless_pair(pin: &str) -> glib::ExitCode {
    let Some(target) = arg_value("--connect") else {
        eprintln!("--pair requires --connect host[:port]");
        return glib::ExitCode::FAILURE;
    };
    let (addr, port) = match target.rsplit_once(':') {
        Some((a, p)) => (a.to_string(), p.parse().unwrap_or(9777)),
        None => (target.clone(), 9777),
    };
    // The label the HOST stores this client under (its paired-devices list).
    let name = arg_value("--name").unwrap_or_else(|| "Steam Deck".to_string());

    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    match NativeClient::pair(
        &addr,
        port,
        (&identity.0, &identity.1),
        pin.trim(),
        &name,
        std::time::Duration::from_secs(90),
    ) {
        Ok(fp) => {
            let fp_hex = crate::trust::hex(&fp);
            let mut known = KnownHosts::load();
            known.upsert(KnownHost {
                name: arg_value("--host-label").unwrap_or_else(|| addr.clone()),
                addr: addr.clone(),
                port,
                fp_hex: fp_hex.clone(),
                paired: true,
            });
            let _ = known.save();
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
fn cli_connect_request() -> Option<ConnectRequest> {
    let args: Vec<String> = std::env::args().collect();
    let target = args
        .iter()
        .skip_while(|a| *a != "--connect")
        .nth(1)?
        .clone();
    let (addr, port) = match target.rsplit_once(':') {
        Some((a, p)) => (a.to_string(), p.parse().ok()?),
        None => (target.clone(), 9777),
    };
    Some(ConnectRequest {
        name: addr.clone(),
        addr,
        port,
        fp_hex: None,
        pair_optional: false,
    })
}

fn build_ui(gtk_app: &adw::Application) {
    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("client identity: {e:#}");
            std::process::exit(1);
        }
    };

    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));
    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("Punktfunk")
        .default_width(1100)
        .default_height(720)
        .content(&toasts)
        .build();

    let app = Rc::new(App {
        window: window.clone(),
        nav: nav.clone(),
        toasts,
        settings: Rc::new(RefCell::new(Settings::load())),
        identity,
        gamepad: crate::gamepad::GamepadService::start(),
        busy: std::cell::Cell::new(false),
        fullscreen: fullscreen_mode(),
    });

    let hosts_page = crate::ui_hosts::new(
        {
            let app = app.clone();
            Rc::new(move |req| initiate_connect(app.clone(), req))
        },
        {
            let app = app.clone();
            Rc::new(move || {
                crate::ui_settings::show(&app.window, app.settings.clone(), &app.gamepad)
            })
        },
        {
            let app = app.clone();
            Rc::new(move |req| speed_test(app.clone(), req))
        },
    );
    nav.add(&hosts_page);
    window.present();

    // CI screenshot mode: render one scripted, host-free scene and signal readiness
    // (clients/linux/tools/screenshots.sh). Mutually exclusive with a real connect.
    if let Some(scene) = shot_scene() {
        run_shot(app, &scene);
        return;
    }

    if let Some(req) = cli_connect_request() {
        initiate_connect(app, req);
    }
}

/// `PUNKTFUNK_SHOT_SCENE`, when set, selects a scripted host-free scene for CI screenshots.
fn shot_scene() -> Option<String> {
    std::env::var("PUNKTFUNK_SHOT_SCENE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Render one mock-populated, host-free scene over the already-presented window, then print
/// `PF_SHOT_READY` once it has had a moment to map + settle so the driver knows when to capture.
/// No `NativeClient` or session is created. The stream scene is deliberately absent — its page
/// requires a live connector (`ui_stream::new` takes an `Arc<NativeClient>`).
fn run_shot(app: Rc<App>, scene: &str) {
    // A plausible host for the trust/pair dialogs (fp_hex is 64 hex chars, like a real SHA-256).
    let mock_req = || ConnectRequest {
        name: "Living Room PC".to_string(),
        addr: "192.168.1.42".to_string(),
        port: 9777,
        fp_hex: Some(
            "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00".to_string(),
        ),
        pair_optional: true,
    };

    match scene {
        // The saved-hosts grid reads ~/.config/punktfunk/client-known-hosts.json, which the
        // driver seeds — so the already-shown hosts page is the scene; nothing to do here.
        "hosts" | "02-hosts" => {}
        "settings" | "03-settings" => {
            crate::ui_settings::show(&app.window, app.settings.clone(), &app.gamepad);
        }
        "trust" | "04-trust" => tofu_dialog(app.clone(), mock_req()),
        "pair" | "05-pair" => pin_dialog(app.clone(), mock_req()),
        other => tracing::warn!("unknown PUNKTFUNK_SHOT_SCENE={other:?}; showing hosts only"),
    }

    let settle_ms = std::env::var("PUNKTFUNK_SHOT_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let scene = scene.to_string();
    glib::timeout_add_local_once(std::time::Duration::from_millis(settle_ms), move || {
        use std::io::Write as _;
        println!("PF_SHOT_READY scene={scene}");
        let _ = std::io::stdout().flush();
    });
}

/// The trust gate in front of every connect. The host is the policy authority (it
/// advertises `pair=optional` only when it accepts unpaired clients); the client renders
/// its trust UI from that:
///   1. PINNED RECONNECT — a host already pinned to this exact fingerprint connects silently.
///   2. FINGERPRINT CHANGED — a host we know at this address but whose fingerprint no longer
///      matches is the impostor signal: force re-pairing via the PIN ceremony, regardless of
///      the advertised policy.
///   3. NEW host — TOFU is offered only when the host advertised `pair=optional` (rule 3a);
///      otherwise (pair=required, unknown/empty policy, or a manual entry) PIN pairing is
///      mandatory (rule 3b).
///
/// A new host is never auto-connected without a stored pin or an explicit trust decision.
fn initiate_connect(app: Rc<App>, req: ConnectRequest) {
    if app.busy.get() {
        return;
    }
    let known = KnownHosts::load();
    match &req.fp_hex {
        Some(fp_hex) => {
            if known.find_by_fp(fp_hex).is_some() {
                // Rule 1: pinned fingerprint matches — silent connect.
                start_session(app, req.clone(), crate::trust::parse_hex32(fp_hex));
            } else if known.find_by_addr(&req.addr, req.port).is_some() {
                // Rule 2: we trust a host at this address but the fingerprint changed —
                // the impostor signal. Re-pair via the PIN ceremony (no TOFU shortcut).
                app.toast("Host fingerprint changed — re-pair with a PIN to continue");
                pin_dialog(app, req);
            } else if req.pair_optional {
                // Rule 3a: the host opted into reduced-security TOFU; offer it alongside PIN.
                tofu_dialog(app, req);
            } else {
                // Rule 3b: pair=required or unknown policy — offer no-PIN delegated approval
                // (request access → approve in the console) or the PIN ceremony.
                approval_dialog(app, req);
            }
        }
        None => {
            // Manual entry (no advertised fingerprint). A known address connects silently
            // on its stored pin (rule 1); an unknown one must pair — request access (approve in
            // the console) or use a PIN; never silent TOFU.
            match known
                .find_by_addr(&req.addr, req.port)
                .and_then(|k| crate::trust::parse_hex32(&k.fp_hex))
            {
                Some(pin) => start_session(app, req, Some(pin)),
                None => approval_dialog(app, req), // rule 3b
            }
        }
    }
}

/// First contact with a discovered host: show the advertised fingerprint and let the user
/// trust it (TOFU), run the PIN ceremony instead, or walk away.
fn tofu_dialog(app: Rc<App>, req: ConnectRequest) {
    let fp = req.fp_hex.clone().unwrap_or_default();
    let dialog = adw::AlertDialog::new(
        Some("New Host"),
        Some(&format!(
            "{} at {}:{}\n\nCertificate fingerprint:\n{}\n\nPairing with a PIN verifies it; \
             trusting accepts it as-is.",
            req.name, req.addr, req.port, fp
        )),
    );
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("pair", "Pair with PIN…"),
        ("trust", "Trust & Connect"),
    ]);
    dialog.set_response_appearance("trust", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("trust"));
    dialog.set_close_response("cancel");
    let parent = app.window.clone();
    dialog.connect_response(None, move |_, response| match response {
        "trust" => {
            let mut known = KnownHosts::load();
            known.upsert(KnownHost {
                name: req.name.clone(),
                addr: req.addr.clone(),
                port: req.port,
                fp_hex: fp.clone(),
                paired: false,
            });
            let _ = known.save();
            start_session(app.clone(), req.clone(), crate::trust::parse_hex32(&fp));
        }
        "pair" => pin_dialog(app.clone(), req.clone()),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The SPAKE2 ceremony: the host is armed and displays a 4-digit PIN; proving knowledge
/// of it pins the host's certificate (and registers ours) with no offline-guessable
/// transcript.
fn pin_dialog(app: Rc<App>, req: ConnectRequest) {
    let entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .placeholder_text("4-digit PIN shown by the host")
        .activates_default(true)
        .build();
    let dialog = adw::AlertDialog::new(
        Some("Pair with PIN"),
        Some(&format!(
            "Arm pairing on {} (console or web UI), then enter the PIN it displays.",
            req.name
        )),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("pair", "Pair")]);
    dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("pair"));
    dialog.set_close_response("cancel");
    let parent = app.window.clone();
    dialog.connect_response(Some("pair"), move |_, _| {
        let pin = entry.text().to_string();
        let app = app.clone();
        let req = req.clone();
        let identity = app.identity.clone();
        let (tx, rx) = async_channel::bounded::<Result<[u8; 32], String>>(1);
        let (host, port, name) = (req.addr.clone(), req.port, glib::host_name().to_string());
        std::thread::spawn(move || {
            let result = NativeClient::pair(
                &host,
                port,
                (&identity.0, &identity.1),
                pin.trim(),
                &name,
                std::time::Duration::from_secs(90),
            )
            .map_err(|e| format!("Pairing failed: {e:?} (wrong PIN, or pairing not armed?)"));
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(fp)) => {
                    let fp_hex = crate::trust::hex(&fp);
                    let mut known = KnownHosts::load();
                    known.upsert(KnownHost {
                        name: req.name.clone(),
                        addr: req.addr.clone(),
                        port: req.port,
                        fp_hex,
                        paired: true,
                    });
                    let _ = known.save();
                    app.toast("Paired — connecting…");
                    start_session(app.clone(), req, Some(fp));
                }
                Ok(Err(msg)) => app.toast(&msg),
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(&parent));
}

/// A fresh host that requires pairing: offer the two ways in. "Request access" is the no-PIN
/// path — connect and wait for the operator to click Approve in the host's console/web UI
/// (delegated approval); "Use a PIN instead…" runs the SPAKE2 ceremony.
fn approval_dialog(app: Rc<App>, req: ConnectRequest) {
    let dialog = adw::AlertDialog::new(
        Some("Pairing Required"),
        Some(&format!(
            "{} requires pairing.\n\nRequest access and approve this device in the host's console \
             (or web UI) — no PIN needed. Or pair with the 4-digit PIN it can display.",
            req.name
        )),
    );
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("pin", "Use a PIN instead…"),
        ("request", "Request Access"),
    ]);
    dialog.set_response_appearance("request", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("request"));
    dialog.set_close_response("cancel");
    let parent = app.window.clone();
    dialog.connect_response(None, move |_, response| match response {
        "request" => request_access(app.clone(), req.clone()),
        "pin" => pin_dialog(app.clone(), req.clone()),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The no-PIN "request access" flow: open an identified connect that the host PARKS until the
/// operator approves it in the console, showing a cancelable "waiting" dialog meanwhile. On
/// approval the same connection is admitted (no reconnect) and the host is saved as paired.
fn request_access(app: Rc<App>, req: ConnectRequest) {
    // Pin the advertised certificate for a discovered host (defence against a host impostor while
    // we wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
    let pin = req.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
    let cancel = Rc::new(std::cell::Cell::new(false));

    let waiting = adw::AlertDialog::new(
        Some("Waiting for Approval"),
        Some(&format!(
            "Approve “{}” in {}’s console or web UI.\n\nThis device is waiting to be let in — it \
             connects automatically once you approve it.",
            glib::host_name(),
            req.name
        )),
    );
    waiting.add_responses(&[("cancel", "Cancel")]);
    waiting.set_close_response("cancel");
    {
        let app = app.clone();
        let cancel = cancel.clone();
        waiting.connect_response(Some("cancel"), move |_, _| {
            // Return the UI immediately; the in-flight connect is left to time out and is torn
            // down silently by the event loop (see StartOpts::cancel).
            cancel.set(true);
            app.busy.set(false);
            app.toast("Cancelled — the request may still be pending on the host.");
        });
    }
    waiting.present(Some(&app.window));

    start_session_with(
        app,
        req,
        pin,
        StartOpts {
            // Must exceed the host's approval window (PENDING_APPROVAL_WAIT) so a slow operator
            // approval still lands on this connection rather than timing the client out first.
            connect_timeout: std::time::Duration::from_secs(185),
            persist_paired: true,
            waiting: Some(waiting),
            cancel: Some(cancel),
        },
    );
}

/// Measure the path to a host over the real data plane (Swift's "Test Network Speed…"):
/// connect, have the host burst probe filler for 2 s up to its 3 Gbps ceiling, report
/// goodput · loss · a recommended bitrate (≈70 % of measured), and apply it in one tap.
fn speed_test(app: Rc<App>, req: ConnectRequest) {
    if app.busy.replace(true) {
        return;
    }
    let pin = req.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
    let status = gtk::Label::new(Some("Connecting…"));
    let dialog = adw::AlertDialog::new(Some("Network Speed Test"), Some(&req.name));
    dialog.set_extra_child(Some(&status));
    dialog.add_responses(&[("close", "Close"), ("apply", "Apply")]);
    dialog.set_response_enabled("apply", false);
    dialog.set_close_response("close");
    dialog.present(Some(&app.window));

    let (tx, rx) =
        async_channel::bounded::<Result<punktfunk_core::client::ProbeOutcome, String>>(1);
    let identity = app.identity.clone();
    let (host, port) = (req.addr.clone(), req.port);
    std::thread::spawn(move || {
        let result = (|| {
            let c = NativeClient::connect(
                &host,
                port,
                punktfunk_core::config::Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                CompositorPref::Auto,
                GamepadPref::Auto,
                0,    // bitrate_kbps (host default)
                0,    // video_caps: the Linux client has no 10-bit/HDR present path yet
                2,    // audio_channels: speed-test probe, stereo
                None, // launch: speed-test probe connect, no game
                pin,
                Some(identity),
                std::time::Duration::from_secs(15),
            )
            .map_err(|e| format!("connect: {e:?}"))?;
            c.request_probe(3_000_000, 2_000)
                .map_err(|e| format!("probe: {e:?}"))?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let r = c.probe_result();
                if r.done {
                    // Let the last UDP shards land before tearing down.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    return Ok(c.probe_result());
                }
                if std::time::Instant::now() > deadline {
                    return Err("probe timed out".to_string());
                }
            }
        })();
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        let outcome = rx.recv().await;
        app.busy.set(false);
        match outcome {
            Ok(Ok(r)) => {
                let mbps = f64::from(r.throughput_kbps) / 1000.0;
                let recommended_kbps = r.throughput_kbps / 10 * 7;
                status.set_text(&format!(
                    "{mbps:.0} Mbit/s measured · {:.1} % loss\nRecommended bitrate: {:.0} Mbit/s",
                    r.loss_pct,
                    f64::from(recommended_kbps) / 1000.0,
                ));
                dialog.set_response_enabled("apply", true);
                dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
                let settings = app.settings.clone();
                let toasts = app.toasts.clone();
                dialog.connect_response(Some("apply"), move |_, _| {
                    let mut s = settings.borrow_mut();
                    s.bitrate_kbps = recommended_kbps;
                    s.save();
                    toasts.add_toast(adw::Toast::new(&format!(
                        "Bitrate set to {:.0} Mbit/s",
                        f64::from(recommended_kbps) / 1000.0
                    )));
                });
            }
            Ok(Err(msg)) => status.set_text(&msg),
            Err(_) => {}
        }
    });
}

/// The mode to request: explicit settings, with `0` fields resolved to the native
/// size/refresh of the monitor the window currently occupies (mirrors the Swift client's
/// native-display default).
fn resolve_mode(app: &App) -> punktfunk_core::config::Mode {
    let s = app.settings.borrow();
    let mut mode = punktfunk_core::config::Mode {
        width: s.width,
        height: s.height,
        refresh_hz: s.refresh_hz,
    };
    if mode.width == 0 || mode.refresh_hz == 0 {
        // Prefer the monitor the window is on; fall back to the display's first monitor. On a
        // `--connect` launch the window may not be mapped yet when this runs, and without the
        // fallback we'd drop to the 1920×1080 floor below — wrong on the Deck (1280×800).
        let monitor = app
            .window
            .surface()
            .zip(gdk::Display::default())
            .and_then(|(surf, d)| d.monitor_at_surface(&surf))
            .or_else(|| {
                gdk::Display::default()
                    .and_then(|d| d.monitors().item(0))
                    .and_then(|o| o.downcast::<gdk::Monitor>().ok())
            });
        if let Some(m) = monitor {
            let geo = m.geometry();
            let scale = m.scale_factor().max(1);
            if mode.width == 0 {
                mode.width = (geo.width() * scale) as u32;
                mode.height = (geo.height() * scale) as u32;
            }
            if mode.refresh_hz == 0 {
                mode.refresh_hz = ((m.refresh_rate() + 500) / 1000).max(30) as u32;
            }
        }
    }
    // No monitor info (early call, odd compositor) — a sane floor.
    if mode.width == 0 {
        (mode.width, mode.height) = (1920, 1080);
    }
    if mode.refresh_hz == 0 {
        mode.refresh_hz = 60;
    }
    mode
}

/// Tunables for a session start that differ between the normal connect and the "request access"
/// (delegated-approval) flow. `Default` is the normal connect.
struct StartOpts {
    /// Handshake budget. The request-access flow uses a long one because the host PARKS the
    /// connection until the operator clicks Approve (see the host's `PENDING_APPROVAL_WAIT`).
    connect_timeout: std::time::Duration,
    /// Persist the host as *paired* on a successful connect. Set for request-access, where the
    /// operator's approval IS the pairing, so future connects are silent (rule 1). Normal TOFU
    /// persists the host *unpaired* (pinned, but not PIN/approval-verified).
    persist_paired: bool,
    /// A "waiting for approval" dialog to dismiss on the first session event (request-access only).
    waiting: Option<adw::AlertDialog>,
    /// Set by the waiting dialog's Cancel button. `NativeClient::connect` is a blocking call with
    /// no abort, so Cancel returns the UI immediately (clears busy, closes the dialog) and leaves
    /// the in-flight connect to time out; when it finally resolves, the event loop sees this flag
    /// and tears down silently (drops the connector → closes the connection) without touching the
    /// UI a new session may already own.
    cancel: Option<Rc<std::cell::Cell<bool>>>,
}

impl Default for StartOpts {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(15),
            persist_paired: false,
            waiting: None,
            cancel: None,
        }
    }
}

fn start_session(app: Rc<App>, req: ConnectRequest, pin: Option<[u8; 32]>) {
    start_session_with(app, req, pin, StartOpts::default());
}

fn start_session_with(app: Rc<App>, req: ConnectRequest, pin: Option<[u8; 32]>, opts: StartOpts) {
    if app.busy.replace(true) {
        return;
    }
    let mode = resolve_mode(&app);
    let s = app.settings.borrow();
    let params = SessionParams {
        host: req.addr.clone(),
        port: req.port,
        mode,
        compositor: CompositorPref::from_name(&s.compositor).unwrap_or(CompositorPref::Auto),
        // "Automatic" matches the physical pad (Swift parity); an explicit choice wins.
        gamepad: match GamepadPref::from_name(&s.gamepad) {
            Some(GamepadPref::Auto) | None => app.gamepad.auto_pref(),
            Some(explicit) => explicit,
        },
        bitrate_kbps: s.bitrate_kbps,
        mic_enabled: s.mic_enabled,
        audio_channels: s.audio_channels,
        pin,
        identity: app.identity.clone(),
        connect_timeout: opts.connect_timeout,
    };
    let inhibit = s.inhibit_shortcuts;
    drop(s);
    let tofu = pin.is_none();
    let persist_paired = opts.persist_paired;
    let mut waiting = opts.waiting;
    let cancel = opts.cancel;

    let mut handle = crate::session::start(params);
    let frames = std::mem::replace(&mut handle.frames, async_channel::bounded(1).1);
    glib::spawn_future_local(async move {
        let mut frames = Some(frames);
        let mut page: Option<crate::ui_stream::StreamPage> = None;
        while let Ok(event) = handle.events.recv().await {
            // A cancelled request-access connect resolved late: tear down silently. Don't touch
            // app.busy — Cancel already cleared it, and a fresh session may now own it.
            if cancel.as_ref().is_some_and(|c| c.get()) {
                if let Some(w) = waiting.take() {
                    w.close();
                }
                break;
            }
            match event {
                SessionEvent::Connected {
                    connector,
                    mode,
                    fingerprint,
                } => {
                    // Dismiss the "waiting for approval" dialog (request-access flow), if any.
                    if let Some(w) = waiting.take() {
                        w.close();
                    }
                    if persist_paired {
                        // Request-access: the operator approved this device, so record the host as
                        // a trusted PAIRED host (pinning the fingerprint we observed) — future
                        // connects are then silent (rule 1), exactly like after a PIN ceremony.
                        let fp_hex = crate::trust::hex(&fingerprint);
                        let mut known = KnownHosts::load();
                        known.upsert(KnownHost {
                            name: req.name.clone(),
                            addr: req.addr.clone(),
                            port: req.port,
                            fp_hex,
                            paired: true,
                        });
                        let _ = known.save();
                        app.toast("Approved — connecting…");
                    } else if tofu {
                        // A TOFU connect just observed the real fingerprint — pin it from now on.
                        let fp_hex = crate::trust::hex(&fingerprint);
                        let mut known = KnownHosts::load();
                        known.upsert(KnownHost {
                            name: req.name.clone(),
                            addr: req.addr.clone(),
                            port: req.port,
                            fp_hex: fp_hex.clone(),
                            paired: false,
                        });
                        let _ = known.save();
                        app.toast(&format!(
                            "Trusted on first use — fingerprint {}…",
                            &fp_hex[..16]
                        ));
                    }
                    tracing::debug!(?mode, "connected — pushing stream page");
                    let title = format!(
                        "{} · {}×{}@{}",
                        req.name, mode.width, mode.height, mode.refresh_hz
                    );
                    app.gamepad.attach(connector.clone());
                    let p = crate::ui_stream::new(
                        &app.window,
                        connector,
                        frames.take().expect("Connected delivered once"),
                        app.gamepad.escape_events(),
                        handle.stop.clone(),
                        inhibit,
                        &title,
                    );
                    app.nav.push(&p.page);
                    // Steam Deck / Gaming Mode: gamescope fullscreens the window but GTK doesn't
                    // know it, so its header bar stays drawn. Enter GTK fullscreen explicitly —
                    // the stream page's `connect_fullscreened_notify` then hides all chrome.
                    if app.fullscreen {
                        app.window.fullscreen();
                    }
                    page = Some(p);
                }
                SessionEvent::Stats(s) => {
                    if let Some(p) = &page {
                        p.update_stats(s);
                    }
                }
                SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => {
                    if let Some(w) = waiting.take() {
                        w.close();
                    }
                    tracing::warn!(%msg, trust_rejected, "connect failed");
                    app.busy.set(false);
                    // A pinned connect rejected on trust grounds means the host's cert no
                    // longer matches the stored pin (rotated cert or impostor) — route to
                    // the PIN ceremony to re-establish trust rather than dead-ending.
                    if trust_rejected && !tofu {
                        app.toast("Host fingerprint changed — re-pair with a PIN to continue");
                        pin_dialog(app.clone(), req.clone());
                    } else {
                        app.toast(&msg);
                    }
                    break;
                }
                SessionEvent::Ended(err) => {
                    if let Some(w) = waiting.take() {
                        w.close();
                    }
                    app.gamepad.detach();
                    app.nav.pop_to_tag("hosts");
                    if let Some(e) = err {
                        app.toast(&e);
                    }
                    app.busy.set(false);
                    break;
                }
            }
        }
    });
}
