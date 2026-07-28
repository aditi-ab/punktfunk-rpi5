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
/* The FlowBoxChild draws the hover/selection highlight AROUND the card (it wraps it
   with its own padding), so its corners must run concentric with the card's 12px —
   radius = card radius + the child's padding ring. */
.pf-host-grid > flowboxchild { border-radius: 15px; }
.pf-pill { font-size: 0.72em; font-weight: bold; padding: 2px 10px; border-radius: 999px;
           color: alpha(currentColor, 0.8); background: alpha(currentColor, 0.1); }
.pf-pill.pf-green { color: @success_color; background: alpha(@success_color, 0.15); }
.pf-pill.pf-accent { color: @accent_color; background: alpha(@accent_color, 0.15); }
.pf-pill.pf-neutral { color: alpha(currentColor, 0.75); background: alpha(currentColor, 0.12); }
.pf-pip { min-width: 8px; min-height: 8px; border-radius: 999px;
          background: alpha(currentColor, 0.35); }
.pf-pip.pf-online { background: @success_color; }
/* An overridden row in profile scope: an accent dot in the prefix, so which settings this
   profile changes is legible at a glance without reading every value. (Plain string literal
   -- a quote in here would end it.) */
.pf-override-dot { min-width: 8px; min-height: 8px; border-radius: 999px;
                   background: @accent_color; }
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
    /// Device lists for the settings pickers (GPUs via `punktfunk-session
    /// --list-adapters` — the shell deliberately links no Vulkan itself — and audio
    /// endpoints via the PipeWire registry), probed once at startup on a worker thread.
    /// Empty until the probe lands — empty lists simply hide their pickers.
    pub probes: Rc<RefCell<crate::ui_settings::DeviceProbes>>,
    hosts: Controller<HostsPage>,
    /// One session child at a time — connects while one runs are ignored.
    busy: bool,
    /// Armed by [`AppMsg::WakeConnect`] (a dial to a host that isn't advertising but has a
    /// known MAC): if THAT dial's child exits with a connect failure, `SessionExited` falls
    /// back into the visible wake-and-wait instead of an error. Consumed on the next exit and
    /// matched against the exiting request, so it can never redirect an unrelated failure.
    wake_fallback: Option<ConnectRequest>,
    /// The request-access "waiting for approval" dialog, closed on the first child
    /// event. A shared slot (not a message): dialogs are main-thread GTK objects and
    /// `AppMsg` must stay `Send` for the session child's reader thread.
    waiting: Rc<RefCell<Option<adw::AlertDialog>>>,
}

#[derive(Debug)]
pub enum AppMsg {
    /// A `punktfunk://` URL arrived (scheme handler, a shortcut, or a second invocation
    /// forwarded to this instance by GApplication) — design/client-deep-links.md §4.1.
    DeepLink(String),
    /// The trust gate in front of every connect (rules 1–3, see `update`).
    Connect(ConnectRequest),
    /// Connect to a saved host that isn't advertising but has a known MAC: fire a wake
    /// packet and DIAL IMMEDIATELY (mDNS absence ≠ unreachable — routed/Tailscale hosts
    /// never advertise here); only a failed dial falls into the visible wake-and-wait.
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
    /// Re-open Preferences editing a specific layer — the settings scope switcher's
    /// destination (design/client-settings-profiles.md §5.1).
    ShowPreferencesScoped(crate::ui_settings::Scope),
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
        // Device lists for the settings pickers: probe in the background, ready long
        // before the dialog opens. A missing session binary or absent PipeWire just
        // leaves the corresponding list empty (and its picker hidden).
        let probes: Rc<RefCell<crate::ui_settings::DeviceProbes>> = Rc::default();
        {
            let (tx, rx) = async_channel::bounded::<crate::ui_settings::DeviceProbes>(1);
            std::thread::spawn(move || {
                let adapters: Vec<String> =
                    std::process::Command::new(crate::spawn::session_binary())
                        .arg("--list-adapters")
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                let (speakers, mics) = pf_client_core::audio::devices().unwrap_or_default();
                let _ = tx.send_blocking(crate::ui_settings::DeviceProbes {
                    adapters,
                    speakers,
                    mics,
                });
            });
            let probes = probes.clone();
            glib::spawn_future_local(async move {
                if let Ok(found) = rx.recv().await {
                    *probes.borrow_mut() = found;
                }
            });
        }
        // Re-apply the persisted forwarded-controller pin (stable key; the service
        // matches it whenever such a pad connects).
        {
            let forward = settings.borrow().forward_pad.clone();
            if !forward.is_empty() {
                init.gamepad.set_pinned(Some(forward));
            }
        }

        let hosts =
            HostsPage::builder()
                .launch(settings.clone())
                .forward(sender.input_sender(), |out| match out {
                    HostsOutput::Connect(req) => AppMsg::Connect(req),
                    HostsOutput::WakeConnect(req) => AppMsg::WakeConnect(req),
                    HostsOutput::Pair(req) => AppMsg::Pair(req),
                    HostsOutput::SpeedTest(req) => AppMsg::SpeedTest(req),
                    HostsOutput::Library(req, mgmt) => AppMsg::OpenLibrary(req, mgmt),
                    HostsOutput::Toast(msg) => AppMsg::Toast(msg),
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
            probes,
            hosts,
            busy: false,
            wake_fallback: None,
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

        // The deep-link seam is live from here: anything GApplication delivered during a cold
        // start has been parked, and everything from now on arrives as a message.
        LINK_TX.with_borrow_mut(|tx| *tx = Some(sender.input_sender().clone()));
        for url in PENDING_LINKS.with_borrow_mut(std::mem::take) {
            sender.input(AppMsg::DeepLink(url));
        }

        ComponentParts {
            model,
            widgets: AppWidgets {},
        }
    }

    fn update(&mut self, msg: AppMsg, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::DeepLink(url) => self.open_deep_link(&url, &sender),
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
                            crate::ui_trust::pin_dialog(
                                &self.window,
                                &sender,
                                self.identity.clone(),
                                req,
                            );
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
                        match known
                            .find_by_addr(&req.addr, req.port)
                            .map(|k| k.fp_hex.clone())
                        {
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
                    // DIAL FIRST — no mDNS advert does NOT mean unreachable: a host reached over
                    // a routed network (Tailscale/VPN/another subnet) is mDNS-blind forever, and
                    // gating the dial on presence bricked exactly those reconnects. Fire the magic
                    // packet now (fire-and-forget — harmless if it's awake) so a genuinely-asleep
                    // box is already booting while the dial times out, arm the wake-wait fallback
                    // for THIS request, and connect immediately.
                    //
                    // Auto-wake OFF (the Settings toggle, for VPN hosts that look offline when
                    // they aren't): no packet and no wake-and-wait fallback — the dial either
                    // succeeds or fails with the normal error. The host-card menu's explicit
                    // "Wake host" is deliberately not gated.
                    if self.settings.borrow().auto_wake {
                        crate::wol::wake(&req.mac, req.addr.parse().ok());
                        self.wake_fallback = Some(req.clone());
                    }
                    sender.input(AppMsg::Connect(req));
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
                self.hosts
                    .emit(HostsMsg::SetConnecting(Some(req.card_key())));
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
                // The dial-first wake fallback (armed by `WakeConnect`, consumed on every exit):
                // a failed dial to the non-advertising host it was armed for falls into the
                // visible wake-and-wait instead of an error alert. Matched by fingerprint (else
                // address) so a stale armed request can never redirect another host's failure.
                let wake_fb =
                    self.wake_fallback
                        .take()
                        .filter(|fb| match (&fb.fp_hex, &req.fp_hex) {
                            (Some(a), Some(b)) => a == b,
                            _ => fb.addr == req.addr && fb.port == req.port,
                        });
                match (code, error, ended) {
                    (0, _, None) => {} // clean end — back on the hosts page, no noise
                    (0, _, Some(reason)) => self.hosts.emit(HostsMsg::ShowError(reason)),
                    (_, Some((_, true)), _) if !tofu => {
                        // The stored pin no longer matches (rotated cert or impostor). The host
                        // ANSWERED — never the wake fallback.
                        self.toast("Host fingerprint changed — re-pair with a PIN to continue");
                        crate::ui_trust::pin_dialog(
                            &self.window,
                            &sender,
                            self.identity.clone(),
                            req,
                        );
                    }
                    // A fingerprint mismatch means the host ANSWERED — reachable, so the plain
                    // error arms below handle it; only a genuine connect failure wakes.
                    (_, Some((_, false)), _) if wake_fb.is_some() => {
                        crate::ui_trust::wake_and_connect(&self.window, &sender, req)
                    }
                    (_, Some((msg, _)), _) => self
                        .hosts
                        .emit(HostsMsg::ShowError(format!("Couldn't connect — {msg}"))),
                    (-1, None, _) => {} // killed (request-access cancel) — already handled
                    (_, None, _) if wake_fb.is_some() => {
                        crate::ui_trust::wake_and_connect(&self.window, &sender, req)
                    }
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
            AppMsg::ShowPreferences => sender.input(AppMsg::ShowPreferencesScoped(
                crate::ui_settings::Scope::Defaults,
            )),
            AppMsg::ShowPreferencesScoped(scope) => {
                let hosts = self.hosts.sender().clone();
                let reopen = sender.clone();
                crate::ui_settings::show_scoped(
                    &self.window,
                    self.settings.clone(),
                    &self.gamepad,
                    &self.probes.borrow(),
                    scope,
                    // The switcher closes the dialog to commit the layer it was editing, then
                    // asks for it back in the new scope — so the app owns the re-open and the
                    // dialog stays a pure view.
                    move |next| reopen.input(AppMsg::ShowPreferencesScoped(next)),
                    move || {
                        // The library toggle changes the saved cards' menu, and a profile edit
                        // changes the chips — re-render either way.
                        let _ = hosts.send(HostsMsg::Refresh);
                    },
                );
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

    /// Route a `punktfunk://` URL (design/client-deep-links.md §4.1). Parsing, host/profile
    /// resolution and every refusal rule live in the shared brain (`plan_from_link`); this is
    /// only the GTK end of it — turn the outcome into the same messages a card click raises,
    /// so a link gets the identical wake, trust and error surfaces and NOT a second connect
    /// path of its own.
    fn open_deep_link(&mut self, url: &str, sender: &ComponentSender<AppModel>) {
        use pf_client_core::deeplink;
        use pf_client_core::orchestrate::{plan_from_link, PlanOutcome};
        use pf_client_core::profiles::ProfilesFile;

        tracing::debug!(%url, "deep link");
        let link = match deeplink::parse(url) {
            Ok(l) => l,
            Err(e) => return self.toast(&e.message()),
        };
        let known = trust::KnownHosts::load();
        let outcome = plan_from_link(
            &link,
            &known,
            &ProfilesFile::load(),
            &self.settings.borrow(),
        );
        match outcome {
            Ok(PlanOutcome::Connect(plan)) => {
                // Rule 2 of §3: never preempt a live session. Only this layer knows one is
                // running, which is why the brain leaves the check here.
                if self.busy {
                    return self.toast("A session is already running — end it first.");
                }
                let req = ConnectRequest {
                    name: plan.host.name.clone(),
                    addr: plan.host.addr.clone(),
                    port: plan.host.port,
                    fp_hex: plan.host.fp_hex.clone(),
                    pair_optional: false,
                    launch: plan.launch.clone().map(|id| (id.clone(), id)),
                    mac: plan.host.mac.clone(),
                    // `profile=` in a URL is a one-off, exactly like "Connect with ▸": it
                    // shapes this session and leaves the host's binding alone.
                    profile: plan.profile_override.clone(),
                };
                // A link is a launch like any other: with a MAC it takes the dial-first wake
                // path, so a sleeping host wakes instead of erroring.
                sender.input(if plan.wake {
                    AppMsg::WakeConnect(req)
                } else {
                    AppMsg::Connect(req)
                });
            }
            Ok(PlanOutcome::ConfirmUnknown(unknown)) => {
                // Known-but-unpinned, or not known at all: the link may not pair and may not
                // trust on its own, so it opens the ordinary ceremony under the user's eyes —
                // the PIN dialog, seeded with what the link claimed.
                if self.busy {
                    return self.toast("A session is already running — end it first.");
                }
                let req = ConnectRequest {
                    name: unknown.name.clone().unwrap_or_else(|| unknown.addr.clone()),
                    addr: unknown.addr.clone(),
                    port: unknown.port,
                    fp_hex: unknown.fp.clone(),
                    pair_optional: false,
                    launch: unknown.launch.clone().map(|id| (id.clone(), id)),
                    mac: Vec::new(),
                    profile: None,
                };
                self.toast(&format!(
                    "{} isn't paired with this device yet — pair it to continue.",
                    req.name
                ));
                crate::ui_trust::pin_dialog(&self.window, sender, self.identity.clone(), req);
            }
            Ok(PlanOutcome::Unsupported(route)) => self.toast(&format!(
                "Punktfunk can't open “{}” links yet.",
                route.as_str()
            )),
            Err(e) => self.toast(&e.message()),
        }
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
        // Where a measured bitrate belongs is "the layer this host actually resolves bitrate
        // from" (design/client-settings-profiles.md §5.3) — the long-standing wrong answer was
        // always the global, so measuring the slow retro box downstairs re-tuned the desktop
        // too. The target depends only on the host, so it is known before the result lands and
        // the button can say where it will write.
        let target = SpeedTestTarget::resolve(&req);
        match &target {
            SpeedTestTarget::Global => {
                dialog.add_responses(&[("close", "Close"), ("apply", "Apply")]);
            }
            SpeedTestTarget::Profile(p) => {
                dialog.add_responses(&[
                    ("close", "Close"),
                    ("apply", &format!("Apply to “{}”", p.name)),
                ]);
            }
            // A bound host whose profile doesn't override bitrate could legitimately mean
            // either: the user gets both, rather than us guessing which layer they meant.
            SpeedTestTarget::Ask(p) => {
                dialog.add_responses(&[
                    ("close", "Close"),
                    ("apply-global", "Set as default"),
                    ("apply", &format!("Set in “{}”", p.name)),
                ]);
                dialog.set_response_enabled("apply-global", false);
            }
        }
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
                    0,                                // bitrate_kbps (host default)
                    0,                                // video_caps: probe connect, nothing presents
                    2,                                // audio_channels: stereo
                    crate::video::decodable_codecs(), // codecs (unused by the probe, but honest)
                    0,                                // preferred_codec: no preference
                    None, // display_hdr: probe connect, nothing presents
                    0,    // client_caps: probe connect, nothing renders a cursor
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
                    if matches!(target, SpeedTestTarget::Ask(_)) {
                        dialog.set_response_enabled("apply-global", true);
                    }
                    let mbit = f64::from(recommended_kbps) / 1000.0;
                    {
                        let (settings, toasts) = (settings.clone(), toasts.clone());
                        dialog.connect_response(Some("apply"), move |_, _| {
                            let where_to = match &target {
                                SpeedTestTarget::Global => {
                                    let mut s = settings.borrow_mut();
                                    s.bitrate_kbps = recommended_kbps;
                                    s.save();
                                    "the default bitrate".to_string()
                                }
                                SpeedTestTarget::Profile(p) | SpeedTestTarget::Ask(p) => {
                                    write_profile_bitrate(&p.id, recommended_kbps);
                                    format!("“{}”", p.name)
                                }
                            };
                            toasts.add_toast(adw::Toast::new(&format!(
                                "{mbit:.0} Mbit/s set in {where_to}"
                            )));
                        });
                    }
                    dialog.connect_response(Some("apply-global"), move |_, _| {
                        let mut s = settings.borrow_mut();
                        s.bitrate_kbps = recommended_kbps;
                        s.save();
                        toasts.add_toast(adw::Toast::new(&format!(
                            "{mbit:.0} Mbit/s set in the default bitrate"
                        )));
                    });
                }
                Ok(Err(msg)) => status.set_text(&msg),
                Err(_) => {}
            }
        });
    }
}

/// Which layer a measured bitrate should land in for the host that was tested
/// (design/client-settings-profiles.md §5.3).
enum SpeedTestTarget {
    /// No profile bound — the global default, i.e. what has always happened.
    Global,
    /// The bound profile already overrides bitrate, so that override is what this host reads.
    Profile(pf_client_core::profiles::StreamProfile),
    /// Bound, but the profile inherits bitrate: writing either layer is defensible, so ask.
    Ask(pf_client_core::profiles::StreamProfile),
}

impl SpeedTestTarget {
    fn resolve(req: &crate::ui_hosts::ConnectRequest) -> SpeedTestTarget {
        // Resolved exactly the way a connect resolves it: the one-off pick this test was
        // started with (a pinned card carries one), else the host's binding.
        let bound = trust::KnownHosts::load()
            .hosts
            .iter()
            .find(|h| h.addr == req.addr && h.port == req.port)
            .and_then(|h| h.profile_id.clone());
        let reference = match req.profile.as_deref() {
            Some("") => return SpeedTestTarget::Global,
            Some(id) => Some(id.to_string()),
            None => bound,
        };
        let Some(reference) = reference else {
            return SpeedTestTarget::Global;
        };
        let catalog = pf_client_core::profiles::ProfilesFile::load();
        match catalog.resolve(&reference).0 {
            Some(p) if p.overrides.bitrate_kbps.is_some() => SpeedTestTarget::Profile(p.clone()),
            Some(p) => SpeedTestTarget::Ask(p.clone()),
            // A dangling binding resolves as no profile everywhere else; here too.
            None => SpeedTestTarget::Global,
        }
    }
}

/// Write a measured bitrate into one profile's overlay, leaving everything else alone.
fn write_profile_bitrate(id: &str, kbps: u32) {
    let mut catalog = pf_client_core::profiles::ProfilesFile::load();
    let Some(p) = catalog.profiles.iter_mut().find(|p| p.id == id) else {
        return; // deleted while the test ran — the toast still tells the truth about the test
    };
    p.overrides.bitrate_kbps = Some(kbps);
    if let Err(e) = catalog.save() {
        tracing::warn!(error = %format!("{e:#}"), "saving the measured bitrate");
    }
}

thread_local! {
    /// Where a delivered URL goes once the window exists. Both ends of this live on the GTK
    /// main thread: `connect_open` fires there, and so does the model's `init`.
    static LINK_TX: std::cell::RefCell<Option<relm4::Sender<AppMsg>>> =
        const { std::cell::RefCell::new(None) };
    /// URLs that arrived before the model existed — the cold-start case, where GApplication
    /// runs `open` before `activate` builds the window. A dropped URL is the one outcome a
    /// link must never have, so they wait here instead.
    static PENDING_LINKS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Hand a URL to the running app, or park it until there is one.
fn deliver_deep_link(url: String) {
    let queued = LINK_TX.with_borrow(|tx| match tx {
        Some(tx) => {
            let _ = tx.send(AppMsg::DeepLink(url.clone()));
            false
        }
        None => true,
    });
    if queued {
        PENDING_LINKS.with_borrow_mut(|q| q.push(url));
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
    // Headless known-hosts management (list/add/edit/forget/reset) + reachability probes —
    // the shared store the Decky plugin drives; returns None when argv names none of them.
    if let Some(code) = crate::cli::headless_host_command() {
        return code;
    }
    // Streams and the console library live in the session binary now — exec it,
    // forwarding the relevant argv (the Decky wrapper keeps working through the shell
    // until it's repointed).
    // `--browse` may be bare now (the console home — hosts, pairing, settings), so the
    // gate is the flag, not a value after it.
    if crate::cli::arg_value("--connect").is_some() || crate::cli::arg_flag("--browse") {
        return crate::cli::exec_session();
    }

    // HANDLES_OPEN is what makes `Exec=punktfunk-client %u` work: GApplication turns the URI
    // into an `open` call, and — this is the part that matters — a SECOND invocation forwards
    // its URI to the already-running instance over D-Bus and exits, so clicking a link with
    // Punktfunk open reuses the window instead of racing a new one.
    let mut builder = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN);
    // Screenshot mode launches the app once per scene back-to-back; NON_UNIQUE keeps
    // each launch its own primary instance.
    if crate::cli::shot_scene().is_some() {
        builder =
            builder.flags(gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_OPEN);
    }
    let adw_app = builder.build();
    adw_app.connect_open(|app, files, _hint| {
        for f in files {
            deliver_deep_link(f.uri().to_string());
        }
        // `open` does not raise a window on its own; the model's activate handler does.
        app.activate();
    });
    // One SDL context for the whole process, started while single-threaded.
    let gamepad = crate::gamepad::GamepadService::start();
    // argv stays withheld from GApplication — except for a positional URL, which is exactly
    // what GIO's single-instance forwarding is for. Passing it through means the FIRST
    // instance's `open` fires locally and a later one's is delivered to the primary, with no
    // IPC of our own.
    let args: Vec<String> = match crate::cli::deep_link_arg() {
        Some(url) => vec![
            std::env::args()
                .next()
                .unwrap_or_else(|| "punktfunk-client".into()),
            url,
        ],
        None => Vec::new(),
    };
    let app = relm4::RelmApp::from_app(adw_app).with_args(args);
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
                <property name="accelerator">F11 &lt;Alt&gt;Return</property>
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
                <property name="title">Cycle the statistics overlay (off · compact · normal · detailed)</property>
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
