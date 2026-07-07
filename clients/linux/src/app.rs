//! The application shell as a relm4 component tree (phase 5 of punktfunk-planning
//! `linux-client-rearchitecture.md`): [`AppModel`] owns the window, navigation, trust
//! gate, and the spawned session child's lifecycle; the hosts page is a child component
//! ([`crate::ui_hosts`]); dialogs (trust, settings, library) are plain GTK invoked from
//! `update`. Every stream runs in the `punktfunk-session` Vulkan binary — the shell
//! never touches video.

use crate::spawn::{self, SpawnOpts};
use crate::trust::{self, Settings};
use crate::ui_hosts::{ConnectRequest, HostsMsg, HostsOutput, HostsPage};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref};
use relm4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

pub const APP_ID: &str = "io.unom.Punktfunk";

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
";

/// Everything the shell shares below the component tree.
pub struct AppModel {
    pub window: adw::ApplicationWindow,
    pub nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    pub settings: Rc<RefCell<Settings>>,
    pub identity: (String, String),
    /// App-lifetime SDL gamepad service (Settings' controller list + pinning). Streams
    /// run in the session binary, which has its own.
    pub gamepad: crate::gamepad::GamepadService,
    hosts: Controller<HostsPage>,
    /// One session child at a time — connects while one runs are ignored.
    busy: bool,
    /// The request-access "waiting for approval" dialog, closed on the first child
    /// event. A shared slot (not a message): dialogs are main-thread GTK objects and
    /// `AppMsg` must stay `Send` for the session child's reader thread.
    waiting: Rc<RefCell<Option<adw::AlertDialog>>>,
}

#[derive(Debug)]
pub enum AppMsg {
    /// The trust gate in front of every connect (rules 1–3, see `update`).
    Connect(ConnectRequest),
    /// Wake an offline saved host, poll until it advertises, then `Connect`.
    WakeConnect(ConnectRequest),
    /// The SPAKE2 PIN ceremony dialog.
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    /// The desktop library page (mgmt port from the live advert when known).
    OpenLibrary(ConnectRequest, Option<u16>),
    /// Spawn the session child now (trust already decided; `tofu` = persist the
    /// fingerprint once the child proves it).
    StartSession {
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        opts: SpawnOpts,
    },
    /// The child presented its first frame.
    SessionReady {
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        persist_paired: bool,
    },
    /// The child exited (the session is over, or the connect failed).
    SessionExited {
        req: ConnectRequest,
        code: i32,
        error: Option<(String, bool)>,
        ended: Option<String>,
        tofu: bool,
    },
    /// Request-access Cancel: the child was killed; release busy quietly.
    CancelPending,
    /// The speed-test dialog resolved (either way) — release `busy`.
    SpeedTestDone,
    ShowPreferences,
    ShowShortcuts,
    ShowAbout,
    ShowAddHost,
    Toast(String),
}

pub struct AppInit {
    pub gamepad: crate::gamepad::GamepadService,
}

pub struct AppWidgets {}

impl SimpleComponent for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("Punktfunk")
            .default_width(1200)
            .default_height(780)
            .build()
    }

    fn init(
        init: Self::Init,
        window: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!("client identity: {e:#}");
                std::process::exit(1);
            }
        };
        load_css();
        // Screenshot scenes must capture settled frames: kill every GTK/libadwaita
        // animation (a headless session may starve the frame clock and leave a
        // transition frozen mid-flight in the capture).
        if crate::cli::shot_scene().is_some() {
            if let Some(s) = gtk::Settings::default() {
                s.set_gtk_enable_animations(false);
            }
        }

        let settings = Rc::new(RefCell::new(Settings::load()));
        // Re-apply the persisted forwarded-controller pin (stable key; the service
        // matches it whenever such a pad connects).
        {
            let forward = settings.borrow().forward_pad.clone();
            if !forward.is_empty() {
                init.gamepad.set_pinned(Some(forward));
            }
        }

        let hosts = HostsPage::builder()
            .launch(settings.clone())
            .forward(sender.input_sender(), |out| match out {
                HostsOutput::Connect(req) => AppMsg::Connect(req),
                HostsOutput::WakeConnect(req) => AppMsg::WakeConnect(req),
                HostsOutput::Pair(req) => AppMsg::Pair(req),
                HostsOutput::SpeedTest(req) => AppMsg::SpeedTest(req),
                HostsOutput::Library(req, mgmt) => AppMsg::OpenLibrary(req, mgmt),
            });

        let nav = adw::NavigationView::new();
        nav.add(hosts.widget());
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        window.set_content(Some(&toasts));
        // Gaming-mode fallback (a bare launch under gamescope): fullscreen the shell.
        if crate::cli::fullscreen_mode() {
            window.fullscreen();
        }

        let model = AppModel {
            window: window.clone(),
            nav,
            toasts,
            settings,
            identity,
            gamepad: init.gamepad,
            hosts,
            busy: false,
            waiting: Rc::new(RefCell::new(None)),
        };
        install_actions(&model.window, &sender);

        // CI screenshot mode: dispatch the scripted scene once the window is actually
        // mapped (AdwDialogs need a live window; relm4 maps it after `init` returns, so
        // this can't run inline like the pre-relm4 `activate` path did).
        if let Some(scene) = crate::cli::shot_scene() {
            let ctx = crate::cli::ShotCtx {
                window: model.window.clone(),
                nav: model.nav.clone(),
                hosts: model.hosts.sender().clone(),
                settings: model.settings.clone(),
                gamepad: model.gamepad.clone(),
                identity: model.identity.clone(),
                sender: sender.clone(),
            };
            let fired = std::cell::Cell::new(false);
            model.window.connect_map(move |_| {
                if fired.replace(true) {
                    return; // map can fire more than once; the scene runs on the first
                }
                crate::cli::run_shot(&ctx, &scene);
            });
        }
        window.present();

        ComponentParts {
            model,
            widgets: AppWidgets {},
        }
    }

    fn update(&mut self, msg: AppMsg, sender: ComponentSender<Self>) {
        match msg {
            // The trust gate (the host is the policy authority — it advertises
            // `pair=optional` only when it accepts unpaired clients):
            //   1. PINNED RECONNECT — a stored fingerprint connects silently.
            //   2. FINGERPRINT CHANGED — known address, different fp: the impostor
            //      signal; force the PIN ceremony.
            //   3a. NEW + pair=optional — offer TOFU alongside PIN.
            //   3b. NEW otherwise — delegated approval (request access) or PIN.
            AppMsg::Connect(req) => {
                if self.busy {
                    return;
                }
                let known = trust::KnownHosts::load();
                match &req.fp_hex {
                    Some(fp_hex) => {
                        if known.find_by_fp(fp_hex).is_some() {
                            let fp_hex = fp_hex.clone();
                            sender.input(AppMsg::StartSession {
                                req,
                                fp_hex,
                                tofu: false,
                                opts: SpawnOpts::default(),
                            });
                        } else if known.find_by_addr(&req.addr, req.port).is_some() {
                            self.toast("Host fingerprint changed — re-pair with a PIN to continue");
                            crate::ui_trust::pin_dialog(&self.window, &sender, self.identity.clone(), req);
                        } else if req.pair_optional {
                            crate::ui_trust::tofu_dialog(&self.window, &sender, req);
                        } else {
                            crate::ui_trust::approval_dialog(
                                &self.window,
                                &sender,
                                self.waiting.clone(),
                                req,
                            );
                        }
                    }
                    None => {
                        // Manual entry: a known address connects on its stored pin;
                        // an unknown one must pair — never silent TOFU.
                        match known.find_by_addr(&req.addr, req.port).map(|k| k.fp_hex.clone()) {
                            Some(fp_hex) => sender.input(AppMsg::StartSession {
                                req,
                                fp_hex,
                                tofu: false,
                                opts: SpawnOpts::default(),
                            }),
                            None => crate::ui_trust::approval_dialog(
                                &self.window,
                                &sender,
                                self.waiting.clone(),
                                req,
                            ),
                        }
                    }
                }
            }
            AppMsg::WakeConnect(req) => {
                if !self.busy {
                    crate::ui_trust::wake_and_connect(&self.window, &sender, req);
                }
            }
            AppMsg::Pair(req) => {
                if !self.busy {
                    crate::ui_trust::pin_dialog(&self.window, &sender, self.identity.clone(), req);
                }
            }
            AppMsg::SpeedTest(req) => self.speed_test(req, &sender),
            AppMsg::SpeedTestDone => self.busy = false,
            AppMsg::OpenLibrary(req, mgmt_port) => {
                crate::ui_library::open(self, &sender, req, mgmt_port);
            }
            AppMsg::StartSession {
                req,
                fp_hex,
                tofu,
                opts,
            } => {
                if std::mem::replace(&mut self.busy, true) {
                    return;
                }
                self.hosts.emit(HostsMsg::ClearError);
                self.hosts.emit(HostsMsg::SetConnecting(Some(req.card_key())));
                let fullscreen = self.settings.borrow().fullscreen_on_stream;
                if let Err(e) = spawn::spawn_session(
                    sender.input_sender().clone(),
                    req,
                    fp_hex,
                    tofu,
                    fullscreen,
                    opts,
                ) {
                    self.busy = false;
                    self.hosts.emit(HostsMsg::SetConnecting(None));
                    self.hosts.emit(HostsMsg::ShowError(e));
                }
            }
            AppMsg::SessionReady {
                req,
                fp_hex,
                tofu,
                persist_paired,
            } => {
                self.close_waiting();
                self.hosts.emit(HostsMsg::SetConnecting(None));
                if persist_paired {
                    // Request-access: the operator approved this device — a trusted
                    // PAIRED host from now on, like after a PIN ceremony.
                    trust::persist_host(&req.name, &req.addr, req.port, &fp_hex, true);
                    self.toast("Approved — connected");
                } else if tofu {
                    // The advertised fingerprint proved itself on a real connect.
                    trust::persist_host(&req.name, &req.addr, req.port, &fp_hex, false);
                    self.toast(&format!(
                        "Trusted on first use — fingerprint {}…",
                        &fp_hex[..16.min(fp_hex.len())]
                    ));
                }
                self.hosts.emit(HostsMsg::Refresh);
            }
            AppMsg::SessionExited {
                req,
                code,
                error,
                ended,
                tofu,
            } => {
                self.close_waiting();
                self.busy = false;
                self.hosts.emit(HostsMsg::SetConnecting(None));
                match (code, error, ended) {
                    (0, _, None) => {} // clean end — back on the hosts page, no noise
                    (0, _, Some(reason)) => self.hosts.emit(HostsMsg::ShowError(reason)),
                    (_, Some((_, true)), _) if !tofu => {
                        // The stored pin no longer matches (rotated cert or impostor).
                        self.toast("Host fingerprint changed — re-pair with a PIN to continue");
                        crate::ui_trust::pin_dialog(&self.window, &sender, self.identity.clone(), req);
                    }
                    (_, Some((msg, _)), _) => self
                        .hosts
                        .emit(HostsMsg::ShowError(format!("Couldn't connect — {msg}"))),
                    (-1, None, _) => {} // killed (request-access cancel) — already handled
                    (code, None, _) => self.hosts.emit(HostsMsg::ShowError(format!(
                        "Stream session failed (punktfunk-session exit {code})"
                    ))),
                }
            }
            AppMsg::CancelPending => {
                self.close_waiting();
                self.busy = false;
                self.hosts.emit(HostsMsg::SetConnecting(None));
                self.toast("Cancelled — the request may still be pending on the host.");
            }
            AppMsg::ShowPreferences => {
                let hosts = self.hosts.sender().clone();
                crate::ui_settings::show(&self.window, self.settings.clone(), &self.gamepad, move || {
                    // The library toggle changes the saved cards' menu — re-render.
                    let _ = hosts.send(HostsMsg::Refresh);
                });
            }
            AppMsg::ShowShortcuts => shortcuts_window(&self.window).present(),
            AppMsg::ShowAbout => crate::ui_settings::show_about(&self.window),
            AppMsg::ShowAddHost => self.hosts.emit(HostsMsg::ShowAddHost),
            AppMsg::Toast(msg) => self.toast(&msg),
        }
    }
}

impl AppModel {
    pub fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn close_waiting(&mut self) {
        if let Some(w) = self.waiting.borrow_mut().take() {
            w.close();
        }
    }

    /// Measure the path to a host over the real data plane: connect, burst probe filler
    /// for 2 s, report goodput · loss · a recommended bitrate, and apply it in one tap.
    fn speed_test(&mut self, req: ConnectRequest, sender: &ComponentSender<AppModel>) {
        if std::mem::replace(&mut self.busy, true) {
            return;
        }
        let pin = req.fp_hex.as_deref().and_then(trust::parse_hex32);
        let status = gtk::Label::new(Some("Connecting…"));
        let dialog = adw::AlertDialog::new(Some("Network Speed Test"), Some(&req.name));
        dialog.set_extra_child(Some(&status));
        dialog.add_responses(&[("close", "Close"), ("apply", "Apply")]);
        dialog.set_response_enabled("apply", false);
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));

        let (tx, rx) =
            async_channel::bounded::<Result<punktfunk_core::client::ProbeOutcome, String>>(1);
        let identity = self.identity.clone();
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
                    0, // bitrate_kbps (host default)
                    0, // video_caps: probe connect, nothing presents
                    2, // audio_channels: stereo
                    crate::video::decodable_codecs(), // codecs (unused by the probe, but honest)
                    0, // preferred_codec: no preference
                    None, // launch: probe connect, no game
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

        let settings = self.settings.clone();
        let toasts = self.toasts.clone();
        let sender = sender.clone();
        glib::spawn_future_local(async move {
            let outcome = rx.recv().await;
            sender.input(AppMsg::SpeedTestDone);
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
}

pub fn run() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    // Steam launches its shortcuts with SDL_GAMECONTROLLER_IGNORE_DEVICES naming every
    // physical pad Steam Input has virtualized; the Settings controller list needs the
    // real devices (same rationale as the session binary).
    for var in [
        "SDL_GAMECONTROLLER_IGNORE_DEVICES",
        "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT",
    ] {
        if let Ok(v) = std::env::var(var) {
            tracing::info!(var, value = %v, "clearing Steam's SDL device filter");
            std::env::remove_var(var);
        }
    }
    // Headless paths (no GTK window).
    if let Some(pin) = crate::cli::arg_value("--pair") {
        return crate::cli::headless_pair(&pin);
    }
    if let Some(target) = crate::cli::arg_value("--library") {
        return crate::cli::headless_library(&target);
    }
    if crate::cli::arg_value("--wake").is_some() {
        return crate::cli::cli_wake();
    }
    // Streams and the console library live in the session binary now — exec it,
    // forwarding the relevant argv (the Decky wrapper keeps working through the shell
    // until it's repointed).
    if crate::cli::arg_value("--connect").is_some() || crate::cli::arg_value("--browse").is_some()
    {
        return crate::cli::exec_session();
    }

    let mut builder = adw::Application::builder().application_id(APP_ID);
    // Screenshot mode launches the app once per scene back-to-back; NON_UNIQUE keeps
    // each launch its own primary instance.
    if crate::cli::shot_scene().is_some() {
        builder = builder.flags(gio::ApplicationFlags::NON_UNIQUE);
    }
    let adw_app = builder.build();
    // One SDL context for the whole process, started while single-threaded.
    let gamepad = crate::gamepad::GamepadService::start();
    let app = relm4::RelmApp::from_app(adw_app).with_args(Vec::new());
    app.run::<AppModel>(AppInit { gamepad });
    glib::ExitCode::SUCCESS
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

/// Window actions behind the hosts page's header (the primary menu + "+") — thin
/// forwards into the message loop.
fn install_actions(window: &adw::ApplicationWindow, sender: &ComponentSender<AppModel>) {
    let add = |name: &str, msg: fn() -> AppMsg| {
        let action = gio::SimpleAction::new(name, None);
        let sender = sender.clone();
        action.connect_activate(move |_, _| sender.input(msg()));
        action
    };
    window.add_action(&add("preferences", || AppMsg::ShowPreferences));
    window.add_action(&add("shortcuts", || AppMsg::ShowShortcuts));
    window.add_action(&add("about", || AppMsg::ShowAbout));
    window.add_action(&add("add-host", || AppMsg::ShowAddHost));
}

/// The Keyboard Shortcuts window — the SESSION window's keys (the shell itself has
/// none); kept here as discoverable documentation.
pub fn shortcuts_window(parent: &adw::ApplicationWindow) -> gtk::ShortcutsWindow {
    const UI: &str = r#"
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <property name="modal">1</property>
    <child>
      <object class="GtkShortcutsSection">
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Stream (session window)</property>
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
