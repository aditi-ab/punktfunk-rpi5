//! The application shell: window, navigation, and top-level glue. The trust/pairing
//! dialogs live in `ui_trust`, session launch in `launch`, CLI entry paths in `cli`, the
//! hosts grid in `ui_hosts`.

use crate::trust::Settings;
use crate::ui_hosts::{ConnectRequest, HostsCallbacks, HostsUi};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref};
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "io.unom.Punktfunk";

/// Custom styles on top of libadwaita for the host cards: status pills, presence pips,
/// the most-recent accent bar, dashed discovered cards. Colours come from the adwaita
/// named palette so dark mode just works.
const CSS: &str = "
.pf-host-card { padding: 16px; }
.pf-pill { font-size: 0.72em; font-weight: bold; padding: 2px 10px; border-radius: 999px;
           color: alpha(currentColor, 0.8); background: alpha(currentColor, 0.1); }
.pf-pill.pf-green { color: @success_color; background: alpha(@success_color, 0.15); }
.pf-pill.pf-accent { color: @accent_color; background: alpha(@accent_color, 0.15); }
.pf-pill.pf-neutral { color: alpha(currentColor, 0.75); background: alpha(currentColor, 0.12); }
.pf-pip { min-width: 8px; min-height: 8px; border-radius: 999px;
          background: alpha(currentColor, 0.35); }
.pf-pip.pf-online { background: @success_color; }
/* Most-recent host: a full accent ring drawn as an inset outline so it follows the card's
   rounded corners (an `inset` box-shadow bar gets eaten by the 12px corner clip) and leaves
   the card's own elevation shadow intact. */
.pf-recent { outline: 2px solid @accent_color; outline-offset: -2px; }
.pf-discovered { border: 1px dashed alpha(currentColor, 0.35); }
.pf-poster { border-radius: 10px; background: alpha(currentColor, 0.08); }
.pf-poster-monogram { font-size: 2.4em; font-weight: bold; color: alpha(currentColor, 0.45); }
.pf-store-badge { color: white; background: rgba(0, 0, 0, 0.55); }
/* Gaming-Mode launches: gamescope displays the window fullscreen but never ACKs the
   xdg_toplevel fullscreen state, so GTK keeps the floating-CSD styling — libadwaita's
   rounded corners + shadow margin stay visible over the stream. Flatten them outright. */
window.pf-chromeless { border-radius: 0; box-shadow: none; }
/* The gamepad library launcher (`--browse`, ui_gamepad_library) — always-dark console
   chrome over the aurora, independent of the desktop theme. */
.pf-gl-page { background: black; color: white; }
.pf-gl-host { font-size: 1.15em; font-weight: bold; color: rgba(255, 255, 255, 0.9); }
.pf-gl-chip { font-size: 0.8em; color: rgba(255, 255, 255, 0.7);
              background: rgba(255, 255, 255, 0.08);
              border: 1px solid rgba(255, 255, 255, 0.12);
              border-radius: 999px; padding: 4px 12px; }
/* Solid face, not glass: coverflow side cards OVERLAP — a translucent card would bleed
   the stack through the one on top. */
.pf-gl-poster { border-radius: 16px; background: rgb(30, 30, 37);
                border: 1px solid rgba(255, 255, 255, 0.07); }
.pf-gl-dim { background: black; border-radius: 16px; }
.pf-gl-detail-title { font-size: 1.7em; font-weight: bold; color: white; }
.pf-gl-detail-store { font-size: 0.75em; font-weight: 600; letter-spacing: 2px;
                      color: rgba(255, 255, 255, 0.5); }
.pf-gl-glyph { font-size: 0.85em; font-weight: bold; color: white;
               background: rgba(255, 255, 255, 0.14);
               border-radius: 999px; min-width: 26px; min-height: 26px; padding: 2px 8px; }
.pf-gl-hint { color: rgba(255, 255, 255, 0.85); }
.pf-gl-status { font-size: 0.85em; color: #ff938a; }
.pf-gl-error-title { font-size: 1.4em; font-weight: bold; color: white; }
";

pub struct App {
    pub window: adw::ApplicationWindow,
    pub nav: adw::NavigationView,
    pub toasts: adw::ToastOverlay,
    pub settings: Rc<RefCell<Settings>>,
    pub identity: (String, String),
    /// App-lifetime SDL gamepad service: Settings list + per-session capture/feedback.
    pub gamepad: crate::gamepad::GamepadService,
    /// One session at a time — ignore connects while one is starting/running.
    pub busy: std::cell::Cell<bool>,
    /// Steam Deck / Gaming-Mode launch: fullscreen the window (chrome-less) when a stream starts.
    pub fullscreen: bool,
    /// Quit when the session ends (Gaming-Mode `--connect` launch): the app IS the stream —
    /// exiting ends the Steam "game" so the Deck returns to Gaming Mode instead of stranding
    /// the user on the client's own hosts page.
    pub quit_on_session_end: bool,
    /// The hosts page handle (banner + per-card connecting spinner), set right after the
    /// page is built — `None` only during construction.
    pub hosts: RefCell<Option<Rc<HostsUi>>>,
    /// The gamepad library launcher — `Some` only under `--browse`, where it replaces the
    /// hosts page as the root (and session end returns here instead of quitting).
    pub browse: RefCell<Option<Rc<crate::ui_gamepad_library::LauncherUi>>>,
}

impl App {
    pub fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    pub fn hosts_ui(&self) -> Option<Rc<HostsUi>> {
        self.hosts.borrow().clone()
    }

    pub fn browse_ui(&self) -> Option<Rc<crate::ui_gamepad_library::LauncherUi>> {
        self.browse.borrow().clone()
    }

    /// Surface a connect failure: the launcher in browse mode, else the hosts page banner
    /// (toast fallback pre-build).
    pub fn connect_error(&self, msg: &str) {
        match (self.browse_ui(), self.hosts_ui()) {
            (Some(l), _) => l.show_error(msg),
            (_, Some(h)) => h.show_error(msg),
            _ => self.toast(msg),
        }
    }
}

pub fn run() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // Steam launches its shortcuts with SDL_GAMECONTROLLER_IGNORE_DEVICES naming every
    // physical pad Steam Input has virtualized — SDL then hides the real device so games
    // only see the virtual X360 pad. Right for games, wrong for us: capturing the Deck's
    // built-in controller (trackpads/paddles/gyro, 28DE:1205) needs SDL's HIDAPI driver
    // to enumerate the REAL device, and the built-in pad can never leave Steam Input
    // ("Steam Controller" is always-required), so this filter is the only off switch we
    // get. Clear it while still single-threaded (the gamepad worker starts with the UI);
    // we dedupe the virtual pad ourselves (`gamepad.rs` `active_id` skips steam_virtual).
    for var in [
        "SDL_GAMECONTROLLER_IGNORE_DEVICES",
        "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT",
    ] {
        if let Ok(v) = std::env::var(var) {
            tracing::info!(var, value = %v, "clearing Steam's SDL device filter");
            std::env::remove_var(var);
        }
    }
    // Headless pairing path (no GTK window): `--pair <PIN> --connect host[:port] [--name N]`.
    // Used by the Decky plugin (a GTK dialog can't pop under gamescope) and for scripting.
    if let Some(pin) = crate::cli::arg_value("--pair") {
        return crate::cli::headless_pair(&pin);
    }
    // Headless library fetch (no GTK window): `--library host[:mgmt_port] [--fp HEX]`.
    if let Some(target) = crate::cli::arg_value("--library") {
        return crate::cli::headless_library(&target);
    }
    // Headless Wake-on-LAN (no GTK window): `--wake host[:port]`. The Decky wrapper calls this
    // before the stream launch so a sleeping host is up by the time `--connect` runs.
    if crate::cli::arg_value("--wake").is_some() {
        return crate::cli::cli_wake();
    }
    let mut builder = adw::Application::builder().application_id(APP_ID);
    // Screenshot mode launches the app once per scene back-to-back; NON_UNIQUE keeps each
    // launch its own primary instance instead of forwarding to a still-registered name.
    if crate::cli::shot_scene().is_some() {
        builder = builder.flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
    }
    let app = builder.build();
    // One SDL context for the whole process: `activate` fires again on every subsequent
    // launch forwarded to this already-running singleton (another `--connect`, the desktop
    // icon clicked twice, …). SDL only ever lets the FIRST thread that calls `sdl3::init()`
    // hold the "main thread" — a second `GamepadService::start()` from a later `activate`
    // would spawn a new thread that fails that check forever. Starting it once here and
    // cloning it into each `build_ui` keeps the worker thread (and its pad state) shared
    // across every window instead.
    let gamepad = crate::gamepad::GamepadService::start();
    app.connect_activate(move |gtk_app| build_ui(gtk_app, gamepad.clone()));
    // GTK doesn't see our argv (`--connect` is handled in `build_ui`); an empty argv also
    // keeps GApplication from rejecting unknown options.
    app.run_with_args(&[] as &[&str])
}

fn build_ui(gtk_app: &adw::Application, gamepad: crate::gamepad::GamepadService) {
    let identity = match crate::trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("client identity: {e:#}");
            std::process::exit(1);
        }
    };
    load_css();
    // Screenshot scenes must capture settled frames: kill every GTK/libadwaita animation
    // (nav-push slides especially — a headless session may starve the frame clock and
    // leave a transition frozen mid-flight in the capture).
    if crate::cli::shot_scene().is_some() {
        if let Some(s) = gtk::Settings::default() {
            s.set_gtk_enable_animations(false);
        }
    }

    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));
    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("Punktfunk")
        .default_width(1200)
        .default_height(780)
        .content(&toasts)
        .build();

    let fullscreen = crate::cli::fullscreen_mode();
    if fullscreen {
        // Chrome-less shell: no CSD rounding/shadow (see CSS — gamescope never ACKs the
        // fullscreen state, so GTK would keep them), and ask for fullscreen up front.
        window.add_css_class("pf-chromeless");
        window.fullscreen();
    }

    let app = Rc::new(App {
        window: window.clone(),
        nav: nav.clone(),
        toasts,
        settings: Rc::new(RefCell::new(Settings::load())),
        identity,
        gamepad,
        busy: std::cell::Cell::new(false),
        fullscreen,
        // (`--browse` makes cli_connect_request None — browse mode returns to the
        // launcher on session end instead of quitting.)
        quit_on_session_end: fullscreen && crate::cli::cli_connect_request().is_some(),
        hosts: RefCell::new(None),
        browse: RefCell::new(None),
    });

    // Re-apply the persisted forwarded-controller pin (stable key; the service matches it
    // whenever such a pad connects) — without this the pin silently resets to Automatic on
    // every launch, and Automatic may resolve to a gyro-less pad (Steam's virtual gamepad).
    {
        let forward = app.settings.borrow().forward_pad.clone();
        if !forward.is_empty() {
            app.gamepad.set_pinned(Some(forward));
        }
    }

    // Browse mode (`--browse host`): the app IS the gamepad library launcher — it becomes
    // the ONE root page. No hosts page (whose construction starts the mDNS browse), no
    // header-menu actions; `Settings::library_enabled` is deliberately ignored (the flag
    // gates the desktop menu item — asking to browse IS the opt-in here).
    if let Some((req, paired, mgmt_port)) = crate::cli::cli_browse_request() {
        let launcher = crate::ui_gamepad_library::open(app.clone(), req, paired, mgmt_port);
        nav.add(&launcher.page);
        *app.browse.borrow_mut() = Some(launcher);
        window.present();
        return;
    }

    let hosts_ui = Rc::new(crate::ui_hosts::new(
        app.settings.clone(),
        HostsCallbacks {
            on_connect: {
                let app = app.clone();
                Rc::new(move |req| crate::ui_trust::initiate_connect(app.clone(), req))
            },
            on_wake_connect: {
                let app = app.clone();
                Rc::new(move |req| crate::ui_trust::wake_and_connect(app.clone(), req))
            },
            on_speed_test: {
                let app = app.clone();
                Rc::new(move |req| speed_test(app.clone(), req))
            },
            on_pair: {
                let app = app.clone();
                Rc::new(move |req| {
                    if !app.busy.get() {
                        crate::ui_trust::pin_dialog(app.clone(), req);
                    }
                })
            },
            on_library: {
                let app = app.clone();
                Rc::new(move |req| crate::ui_library::open(app.clone(), req))
            },
        },
    ));
    *app.hosts.borrow_mut() = Some(hosts_ui.clone());
    install_actions(&app, &hosts_ui);
    nav.add(&hosts_ui.page);
    window.present();

    // CI screenshot mode: render one scripted, host-free scene and signal readiness
    // (clients/linux/tools/screenshots.sh). Mutually exclusive with a real connect.
    if let Some(scene) = crate::cli::shot_scene() {
        crate::cli::run_shot(app, &scene);
        return;
    }

    if let Some(req) = crate::cli::cli_connect_request() {
        crate::ui_trust::initiate_connect(app, req);
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Window actions behind the hosts page's header: the primary (hamburger) menu entries
/// plus the "+" add-host button and the empty state's call to action.
fn install_actions(app: &Rc<App>, hosts: &Rc<HostsUi>) {
    let add = |name: &str, f: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| f());
        app.window.add_action(&action);
    };
    {
        let app = app.clone();
        add(
            "preferences",
            Box::new(move || {
                let refresh = {
                    let app = app.clone();
                    // The library toggle changes the saved cards' menu — re-render on close.
                    move || {
                        if let Some(h) = app.hosts_ui() {
                            h.refresh();
                        }
                    }
                };
                crate::ui_settings::show(&app.window, app.settings.clone(), &app.gamepad, refresh)
            }),
        );
    }
    {
        let window = app.window.clone();
        add(
            "shortcuts",
            Box::new(move || shortcuts_window(&window).present()),
        );
    }
    {
        let window = app.window.clone();
        add(
            "about",
            Box::new(move || crate::ui_settings::show_about(&window)),
        );
    }
    {
        let hosts = hosts.clone();
        add("add-host", Box::new(move || hosts.show_add_host()));
    }
}

/// The Keyboard Shortcuts window (menu + the shortcuts scene). GtkShortcutsWindow is
/// builder-XML-first, so it's assembled from a snippet rather than widget calls.
pub fn shortcuts_window(parent: &adw::ApplicationWindow) -> gtk::ShortcutsWindow {
    const UI: &str = r#"
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <property name="modal">1</property>
    <child>
      <object class="GtkShortcutsSection">
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Stream</property>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Toggle fullscreen</property>
                <property name="accelerator">F11</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Release captured input (click the stream to capture)</property>
                <property name="accelerator">&lt;Control&gt;&lt;Alt&gt;&lt;Shift&gt;q</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Disconnect</property>
                <property name="accelerator">&lt;Control&gt;&lt;Alt&gt;&lt;Shift&gt;d</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Toggle statistics overlay</property>
                <property name="accelerator">&lt;Control&gt;&lt;Alt&gt;&lt;Shift&gt;s</property>
              </object>
            </child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;
    let builder = gtk::Builder::from_string(UI);
    let window: gtk::ShortcutsWindow = builder
        .object("shortcuts")
        .expect("shortcuts window in builder XML");
    window.set_transient_for(Some(parent));
    window
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
                0,                                // bitrate_kbps (host default)
                0, // video_caps: the Linux client has no 10-bit/HDR present path yet
                2, // audio_channels: speed-test probe, stereo
                crate::video::decodable_codecs(), // codecs (unused by the probe, but honest)
                0, // preferred_codec: no preference for a speed-test probe
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
