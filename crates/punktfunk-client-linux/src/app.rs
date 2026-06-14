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
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    // GTK doesn't see our argv (`--connect` is handled in `build_ui`); an empty argv also
    // keeps GApplication from rejecting unknown options.
    app.run_with_args(&[] as &[&str])
}

/// `--connect host[:port]` — skip the hosts page and start a session immediately
/// (scripting + headless testing; trust follows the same known-hosts/TOFU rules).
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
        pair_required: false,
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

    if let Some(req) = cli_connect_request() {
        initiate_connect(app, req);
    }
}

/// The trust gate in front of every connect. Discovered hosts carry their fingerprint in
/// the mDNS advert, so trust is decided *before* any traffic: known → pinned connect;
/// unknown → TOFU prompt (or straight to pairing when the host requires it). Manual
/// entries have no advance fingerprint: trust on first use, pin from then on.
fn initiate_connect(app: Rc<App>, req: ConnectRequest) {
    if app.busy.get() {
        return;
    }
    let known = KnownHosts::load();
    match &req.fp_hex {
        Some(fp_hex) => {
            if known.find_by_fp(fp_hex).is_some() {
                start_session(app, req.clone(), crate::trust::parse_hex32(fp_hex));
            } else if req.pair_required {
                // TOFU alone won't pass the host's gate — go straight to the ceremony.
                pin_dialog(app, req);
            } else {
                tofu_dialog(app, req);
            }
        }
        None => {
            let pin = known
                .find_by_addr(&req.addr, req.port)
                .and_then(|k| crate::trust::parse_hex32(&k.fp_hex));
            start_session(app, req, pin);
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
                0,
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
        let monitor = app
            .window
            .surface()
            .zip(gdk::Display::default())
            .and_then(|(surf, d)| d.monitor_at_surface(&surf));
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

fn start_session(app: Rc<App>, req: ConnectRequest, pin: Option<[u8; 32]>) {
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
        pin,
        identity: app.identity.clone(),
    };
    let inhibit = s.inhibit_shortcuts;
    drop(s);
    let tofu = pin.is_none();

    let mut handle = crate::session::start(params);
    let frames = std::mem::replace(&mut handle.frames, async_channel::bounded(1).1);
    glib::spawn_future_local(async move {
        let mut frames = Some(frames);
        let mut page: Option<crate::ui_stream::StreamPage> = None;
        while let Ok(event) = handle.events.recv().await {
            match event {
                SessionEvent::Connected {
                    connector,
                    mode,
                    fingerprint,
                } => {
                    // A TOFU connect just observed the real fingerprint — pin it from now on.
                    if tofu {
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
                        handle.stop.clone(),
                        inhibit,
                        &title,
                    );
                    app.nav.push(&p.page);
                    page = Some(p);
                }
                SessionEvent::Stats(s) => {
                    if let Some(p) = &page {
                        p.update_stats(s);
                    }
                }
                SessionEvent::Failed(msg) => {
                    tracing::warn!(%msg, "connect failed");
                    app.toast(&msg);
                    app.busy.set(false);
                    break;
                }
                SessionEvent::Ended(err) => {
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
