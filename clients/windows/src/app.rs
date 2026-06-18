//! The WinUI 3 (windows-reactor) application shell — host list, settings, PIN/TOFU pairing, and
//! the stream page (a `SwapChainPanel` bound to the D3D11 composition swapchain in
//! [`crate::present`], driven by reactor's per-frame `on_rendering`).
//!
//! Declarative React-like model: a single root component routes on a `Screen` value held in
//! `use_async_state` so background threads (discovery, the session pump) can drive navigation.
//! The present + decoded-frame handoff crosses to the UI thread through a `Mutex` side-channel
//! and thread-locals (the windows-reactor SwapChainPanel sample's pattern), since the per-frame
//! present must not go through state/rerender.
//!
//! The chrome follows the windows-reactor gallery's look: Mica backdrop, a centred max-width
//! column, theme brushes (`ThemeRef`), and rounded `border` cards.

use crate::discovery::{self, DiscoveredHost};
use crate::gamepad::GamepadService;
use crate::present::Presenter;
use crate::session::{self, SessionEvent, SessionParams, Stats};
use crate::trust::{self, KnownHost, KnownHosts, Settings};
use crate::video::{DecodedFrame, DecoderPref};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use windows_reactor::*;

const RESOLUTIONS: &[(u32, u32)] = &[
    (0, 0),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Decode backend presets: `(stored value, display label)`.
const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatic (GPU, fall back to CPU)"),
    ("hardware", "Hardware (GPU / D3D11VA)"),
    ("software", "Software (CPU)"),
];
/// Bitrate presets in Mb/s; `0` = host default.
const BITRATES_MBPS: &[u32] = &[0, 10, 20, 30, 50, 80, 150];

#[derive(Clone, PartialEq)]
enum Screen {
    Hosts,
    Connecting,
    Stream,
    Settings,
    Pair,
}

/// The host we're about to connect to / pair with (carried into the Pair screen).
#[derive(Clone, Default)]
struct Target {
    name: String,
    addr: String,
    port: u16,
    fp_hex: Option<String>,
    pair_optional: bool,
}

/// Stable app services handed to the page components as props. Each routed screen that uses
/// hooks (`hosts_page`/`pair_page`/`stream_page`) is mounted as its own `component(...)`, so
/// its hooks live in an isolated slot list — calling them on the shared parent `cx` would
/// change the hook order whenever the screen changes (reactor's Rules-of-Hooks guard aborts).
///
/// `Svc` compares equal by `ctx` identity (it never meaningfully changes across renders), so a
/// page whose props are just `Svc` re-renders only via its own state hooks, never spuriously
/// from the parent.
#[derive(Clone)]
struct Svc {
    ctx: Arc<AppCtx>,
    set_screen: AsyncSetState<Screen>,
    set_status: AsyncSetState<String>,
}

impl PartialEq for Svc {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
    }
}

/// Props for the hosts page: the services plus the changing discovery/status data that must
/// drive its re-render (compared by value, so a new host list or error refreshes the page).
#[derive(Clone)]
struct HostsProps {
    svc: Svc,
    hosts: Vec<DiscoveredHost>,
    status: String,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.hosts == other.hosts && self.status == other.status
    }
}

/// Props for the stream page: the services plus the live stats that drive the HUD overlay
/// (compared by value, so each new sample re-renders the overlay).
#[derive(Clone)]
struct StreamProps {
    svc: Svc,
    stats: Stats,
}

impl PartialEq for StreamProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.stats == other.stats
    }
}

/// UI-thread-only present context: the D3D11 presenter plus the decoded-frame receiver.
struct PresentCtx {
    presenter: Presenter,
    frames: async_channel::Receiver<DecodedFrame>,
}

thread_local! {
    static PRESENT: RefCell<Option<PresentCtx>> = const { RefCell::new(None) };
    static PENDING_FRAMES: RefCell<Option<async_channel::Receiver<DecodedFrame>>> =
        const { RefCell::new(None) };
}

/// Cross-thread handoff from the session pump (off-thread) to the stream page (UI thread).
#[derive(Default)]
struct Shared {
    handoff: Mutex<Option<(Arc<NativeClient>, async_channel::Receiver<DecodedFrame>)>>,
    target: Mutex<Target>,
    /// Latest stream stats, written by the session's event loop and mirrored into reactor state
    /// by the stream page's HUD poll thread to drive the overlay.
    stats: Mutex<Stats>,
}

pub struct AppCtx {
    identity: (String, String),
    settings: Mutex<Settings>,
    gamepad: GamepadService,
    shared: Arc<Shared>,
}

pub fn run(identity: (String, String), gamepad: GamepadService) -> windows_reactor::Result<()> {
    let ctx = Arc::new(AppCtx {
        identity,
        settings: Mutex::new(Settings::load()),
        gamepad,
        shared: Arc::new(Shared::default()),
    });
    App::new()
        .title("Punktfunk")
        .inner_size(1000.0, 720.0)
        .backdrop(Backdrop::Mica)
        .render(move |cx| root(cx, &ctx))
}

// --- shared styling -----------------------------------------------------------------------

fn uniform(v: f64) -> Thickness {
    Thickness::uniform(v)
}

fn edges(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

/// A rounded, bordered surface in the theme's card colours.
fn card(child: impl Into<Element>) -> Border {
    border(child.into())
        .background(ThemeRef::CardBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(8.0)
        .padding(uniform(16.0))
}

/// A small all-caps section label above a group of cards.
fn section(label: &str) -> Element {
    text_block(label)
        .font_size(12.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .margin(edges(2.0, 10.0, 0.0, 0.0))
        .into()
}

/// Wrap a screen's children in a scrollable, centred, max-width column.
fn page(children: Vec<Element>) -> Element {
    let col = vstack(children)
        .spacing(10.0)
        .max_width(640.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .margin(edges(24.0, 24.0, 24.0, 40.0));
    scroll_view(col).into()
}

/// A rounded square "monogram" for a host, the first letter on an accent fill — a clean leading
/// visual that avoids depending on an icon font being installed.
fn avatar(name: &str) -> Border {
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    border(
        text_block(initial)
            .font_size(17.0)
            .semibold()
            .foreground(ThemeRef::AccentText)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center),
    )
    .background(ThemeRef::Accent)
    .corner_radius(10.0)
    .width(40.0)
    .height(40.0)
}

/// Pill chip colour intent.
#[derive(Clone, Copy)]
enum Pill {
    Accent,
    Good,
    Neutral,
}

/// A small rounded status chip (paired/PIN/HDR/etc.).
fn pill(text: &str, kind: Pill) -> Border {
    let (bg, fg) = match kind {
        Pill::Accent => (ThemeRef::Accent, ThemeRef::AccentText),
        Pill::Good => (ThemeRef::SystemSuccessBackground, ThemeRef::SystemSuccess),
        Pill::Neutral => (ThemeRef::SubtleFill, ThemeRef::SecondaryText),
    };
    border(text_block(text).font_size(11.0).semibold().foreground(fg))
        .background(bg)
        .corner_radius(10.0)
        .padding(edges(9.0, 3.0, 9.0, 3.0))
}

/// A clickable host row: monogram + name/address + status pill + chevron.
fn host_card(name: &str, sub: &str, badge: &str, on_tap: impl Fn() + 'static) -> Element {
    let kind = match badge {
        "Paired" => Pill::Good,
        "Open" => Pill::Neutral,
        _ => Pill::Accent, // Trusted / PIN
    };
    card(
        grid((
            avatar(name)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack((
                text_block(name).font_size(15.0).semibold(),
                text_block(sub)
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .margin(edges(12.0, 0.0, 0.0, 0.0)),
            pill(badge, kind)
                .grid_column(2)
                .vertical_alignment(VerticalAlignment::Center)
                .margin(edges(0.0, 0.0, 10.0, 0.0)),
            text_block("\u{203A}")
                .font_size(18.0)
                .foreground(ThemeRef::SecondaryText)
                .grid_column(3)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([
            GridLength::Auto,
            GridLength::Star(1.0),
            GridLength::Auto,
            GridLength::Auto,
        ]),
    )
    .on_tapped(on_tap)
    .into()
}

// --- screens ------------------------------------------------------------------------------

fn root(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
    let (screen, set_screen) = cx.use_async_state(Screen::Hosts);
    let (hosts, set_hosts) = cx.use_async_state(Vec::<DiscoveredHost>::new());
    let (status, set_status) = cx.use_async_state(String::new());
    let (stats, set_stats) = cx.use_async_state(Stats::default());

    // Continuous LAN discovery (spawned once).
    cx.use_effect((), {
        let set_hosts = set_hosts.clone();
        move || {
            let rx = discovery::browse();
            std::thread::spawn(move || {
                let mut acc: Vec<DiscoveredHost> = Vec::new();
                while let Ok(h) = rx.recv_blocking() {
                    if let Some(e) = acc.iter_mut().find(|e| e.key == h.key) {
                        *e = h;
                    } else {
                        acc.push(h);
                    }
                    set_hosts.call(acc.clone());
                }
            });
        }
    });

    // HUD stats: the session event loop writes `shared.stats`; this poll thread mirrors it into
    // root state so the stream page gets it as a *prop*. (A child component's own async-state
    // update is pruned when its props are unchanged — only a prop change re-renders it, exactly
    // like discovery → hosts above.)
    cx.use_effect((), {
        let shared = ctx.shared.clone();
        let set_stats = set_stats.clone();
        move || {
            std::thread::Builder::new()
                .name("pf-hud".into())
                .spawn(move || {
                    let mut last = Stats::default();
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        let s = *shared.stats.lock().unwrap();
                        if s != last {
                            last = s;
                            set_stats.call(s);
                        }
                    }
                })
                .ok();
        }
    });

    // Each hook-using screen is mounted as its own component so its hooks are isolated from
    // root's (root's own hooks above stay a stable prefix regardless of which screen renders).
    let svc = Svc {
        ctx: ctx.clone(),
        set_screen: set_screen.clone(),
        set_status: set_status.clone(),
    };
    match screen {
        Screen::Hosts => component(hosts_page, HostsProps { svc, hosts, status }),
        Screen::Connecting => {
            let target_name = ctx.shared.target.lock().unwrap().name.clone();
            let headline = if target_name.is_empty() {
                "Connecting\u{2026}".to_string()
            } else {
                format!("Connecting to {target_name}\u{2026}")
            };
            vstack((
                ProgressRing::indeterminate()
                    .width(48.0)
                    .height(48.0)
                    .horizontal_alignment(HorizontalAlignment::Center),
                text_block(headline)
                    .font_size(18.0)
                    .semibold()
                    .horizontal_alignment(HorizontalAlignment::Center),
                text_block(if status.is_empty() {
                    "Negotiating the session and creating the virtual display\u{2026}".to_string()
                } else {
                    status.clone()
                })
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center),
            ))
            .spacing(16.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
        }
        // settings_page uses no hooks (it never touches `cx`), so calling it inline is sound.
        Screen::Settings => settings_page(ctx, &set_screen),
        Screen::Pair => component(pair_page, svc),
        Screen::Stream => component(stream_page, StreamProps { svc, stats }),
    }
}

fn hosts_page(props: &HostsProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let hosts = props.hosts.as_slice();
    let status = props.status.as_str();
    let set_screen = &props.svc.set_screen;
    let set_status = &props.svc.set_status;
    let (manual, set_manual) = cx.use_state(String::new());
    let known = KnownHosts::load();

    let mut body: Vec<Element> = Vec::new();

    // Header: title block + Settings button.
    body.push(
        grid((
            vstack((
                text_block("Punktfunk").font_size(30.0).bold(),
                text_block("Stream from a host on your network.")
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
            button("Settings")
                .icon(SymbolGlyph::Setting)
                .on_click({
                    let ss = set_screen.clone();
                    move || ss.call(Screen::Settings)
                })
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .margin(edges(0.0, 0.0, 0.0, 6.0))
        .into(),
    );

    if !status.is_empty() {
        body.push(
            InfoBar::new("Couldn't connect")
                .message(status.to_string())
                .error()
                .is_closable(false)
                .into(),
        );
    }

    // Saved (trusted/paired) hosts.
    if !known.hosts.is_empty() {
        body.push(section("SAVED HOSTS"));
        for k in &known.hosts {
            let target = Target {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                pair_optional: false,
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            body.push(host_card(
                &k.name,
                &format!("{}:{}", k.addr, k.port),
                if k.paired { "Paired" } else { "Trusted" },
                move || initiate(&ctx2, target.clone(), &ss, &st),
            ));
        }
    }

    // Discovered hosts.
    body.push(section("ON YOUR NETWORK"));
    if hosts.is_empty() {
        body.push(
            card(
                hstack((
                    ProgressRing::indeterminate().width(18.0).height(18.0),
                    text_block("Searching the LAN\u{2026}").foreground(ThemeRef::SecondaryText),
                ))
                .spacing(12.0),
            )
            .into(),
        );
    } else {
        for h in hosts {
            let target = Target {
                name: h.name.clone(),
                addr: h.addr.clone(),
                port: h.port,
                fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
                pair_optional: h.pair == "optional",
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            let badge = if h.pair == "required" { "PIN" } else { "Open" };
            body.push(host_card(
                &h.name,
                &format!("{}:{}", h.addr, h.port),
                badge,
                move || initiate(&ctx2, target.clone(), &ss, &st),
            ));
        }
    }

    // Manual connection.
    body.push(section("CONNECT MANUALLY"));
    let connect_manual = {
        let (ctx2, ss, st, text) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            manual.clone(),
        );
        move || {
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let (addr, port) = match text.rsplit_once(':') {
                Some((a, p)) => (a.to_string(), p.parse().unwrap_or(9777)),
                None => (text.to_string(), 9777),
            };
            initiate(
                &ctx2,
                Target {
                    name: addr.clone(),
                    addr,
                    port,
                    fp_hex: None,
                    pair_optional: false,
                },
                &ss,
                &st,
            );
        }
    };
    body.push(
        card(
            grid((
                text_box(manual)
                    .placeholder("host or host:port")
                    .on_changed(move |s| set_manual.call(s))
                    .grid_column(0)
                    .vertical_alignment(VerticalAlignment::Center),
                button("Connect")
                    .accent()
                    .icon(SymbolGlyph::Forward)
                    .on_click(connect_manual)
                    .grid_column(1)
                    .margin(edges(8.0, 0.0, 0.0, 0.0)),
            ))
            .columns([GridLength::Star(1.0), GridLength::Auto]),
        )
        .into(),
    );

    page(body)
}

/// The trust gate (mirrors the GTK client's `initiate_connect`): pinned fingerprint → silent
/// connect; known address → stored pin; advertised `pair=optional` → TOFU; otherwise → PIN
/// pairing.
fn initiate(
    ctx: &Arc<AppCtx>,
    target: Target,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let known = KnownHosts::load();
    let pin = target
        .fp_hex
        .as_ref()
        .and_then(|fp| known.find_by_fp(fp).map(|_| fp.clone()))
        .or_else(|| {
            known
                .find_by_addr(&target.addr, target.port)
                .map(|k| k.fp_hex.clone())
        })
        .and_then(|fp| trust::parse_hex32(&fp));

    if let Some(pin) = pin {
        connect(ctx, &target, Some(pin), set_screen, set_status);
    } else if target.pair_optional {
        connect(ctx, &target, None, set_screen, set_status); // TOFU
    } else {
        *ctx.shared.target.lock().unwrap() = target;
        set_screen.call(Screen::Pair);
    }
}

fn connect(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let s = ctx.settings.lock().unwrap().clone();
    let mode = if s.width != 0 && s.refresh_hz != 0 {
        Mode {
            width: s.width,
            height: s.height,
            refresh_hz: s.refresh_hz,
        }
    } else {
        Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }
    };
    let gamepad_pref = match GamepadPref::from_name(&s.gamepad) {
        Some(GamepadPref::Auto) | None => ctx.gamepad.auto_pref(),
        Some(explicit) => explicit,
    };
    let handle = session::start(SessionParams {
        host: target.addr.clone(),
        port: target.port,
        mode,
        compositor: CompositorPref::Auto,
        gamepad: gamepad_pref,
        bitrate_kbps: s.bitrate_kbps,
        mic_enabled: s.mic_enabled,
        hdr_enabled: s.hdr_enabled,
        decoder: DecoderPref::from_name(&s.decoder),
        pin,
        identity: ctx.identity.clone(),
    });
    set_status.call(String::new());
    set_screen.call(Screen::Connecting);

    let tofu = pin.is_none();
    let (shared, gamepad) = (ctx.shared.clone(), ctx.gamepad.clone());
    let (ss, st) = (set_screen.clone(), set_status.clone());
    let target = target.clone();
    std::thread::spawn(move || loop {
        match handle.events.recv_blocking() {
            Ok(SessionEvent::Connected {
                connector,
                fingerprint,
                ..
            }) => {
                if tofu {
                    let mut k = KnownHosts::load();
                    k.upsert(KnownHost {
                        name: target.name.clone(),
                        addr: target.addr.clone(),
                        port: target.port,
                        fp_hex: trust::hex(&fingerprint),
                        paired: false,
                    });
                    let _ = k.save();
                }
                gamepad.attach(connector.clone());
                *shared.stats.lock().unwrap() = Stats::default(); // clear any prior session's numbers
                *shared.handoff.lock().unwrap() = Some((connector, handle.frames.clone()));
                ss.call(Screen::Stream);
            }
            Ok(SessionEvent::Failed {
                msg,
                trust_rejected,
            }) => {
                st.call(msg);
                gamepad.detach();
                if trust_rejected {
                    // Pinned-fingerprint mismatch / pairing required → re-pair via the PIN screen.
                    *shared.target.lock().unwrap() = target.clone();
                    ss.call(Screen::Pair);
                } else {
                    ss.call(Screen::Hosts);
                }
                break;
            }
            Ok(SessionEvent::Ended(err)) => {
                st.call(err.unwrap_or_else(|| "Session ended".into()));
                gamepad.detach();
                ss.call(Screen::Hosts);
                break;
            }
            Ok(SessionEvent::Stats(s)) => *shared.stats.lock().unwrap() = s,
            Err(_) => {
                gamepad.detach();
                ss.call(Screen::Hosts);
                break;
            }
        }
    });
}

fn pair_page(props: &Svc, cx: &mut RenderCx) -> Element {
    let ctx = &props.ctx;
    let set_screen = &props.set_screen;
    let set_status = &props.set_status;
    let (code, set_code) = cx.use_state(String::new());
    let target = ctx.shared.target.lock().unwrap().clone();

    let pair_btn = {
        let (ctx2, ss, st, code2, target2) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            code.clone(),
            target.clone(),
        );
        button("Pair & Connect")
            .accent()
            .icon(SymbolGlyph::Accept)
            .on_click(move || {
                let pin = code2.trim().to_string();
                let (ctx3, ss, st, target3) =
                    (ctx2.clone(), ss.clone(), st.clone(), target2.clone());
                std::thread::spawn(move || {
                    let name =
                        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows-client".into());
                    match NativeClient::pair(
                        &target3.addr,
                        target3.port,
                        (&ctx3.identity.0, &ctx3.identity.1),
                        &pin,
                        &name,
                        std::time::Duration::from_secs(90),
                    ) {
                        Ok(fp) => {
                            let mut k = KnownHosts::load();
                            k.upsert(KnownHost {
                                name: target3.name.clone(),
                                addr: target3.addr.clone(),
                                port: target3.port,
                                fp_hex: trust::hex(&fp),
                                paired: true,
                            });
                            let _ = k.save();
                            connect(&ctx3, &target3, Some(fp), &ss, &st);
                        }
                        Err(e) => {
                            st.call(format!("Pairing failed: {e:?} (wrong PIN, or not armed?)"));
                            ss.call(Screen::Hosts);
                        }
                    }
                });
            })
    };
    let cancel_btn = {
        let ss = set_screen.clone();
        button("Cancel")
            .icon(SymbolGlyph::Cancel)
            .on_click(move || ss.call(Screen::Hosts))
    };

    let content = card(vstack((
        grid((
            avatar(&target.name)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack((
                text_block(format!("Pair with {}", target.name))
                    .font_size(20.0)
                    .semibold(),
                text_block(format!("{}:{}", target.addr, target.port))
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .margin(edges(12.0, 0.0, 0.0, 0.0)),
        ))
        .columns([GridLength::Auto, GridLength::Star(1.0)]),
        InfoBar::new("Arm pairing on the host")
            .message(
                "On the host's console or web console, start pairing — it shows a 4-digit PIN. \
                 Enter it below within 90 seconds.",
            )
            .informational()
            .is_closable(false),
        text_box(code)
            .placeholder("PIN")
            .font_size(28.0)
            .on_changed(move |s| set_code.call(s)),
        hstack((pair_btn, cancel_btn)).spacing(8.0),
    ))
    .spacing(16.0))
    .max_width(480.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .margin(edges(0.0, 60.0, 0.0, 0.0));

    page(vec![content.into()])
}

fn settings_page(ctx: &Arc<AppCtx>, set_screen: &AsyncSetState<Screen>) -> Element {
    let s = ctx.settings.lock().unwrap().clone();
    let res_i = RESOLUTIONS
        .iter()
        .position(|&(w, h)| w == s.width && h == s.height)
        .unwrap_or(0) as i32;
    let hz_i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0) as i32;

    let res_names: Vec<String> = RESOLUTIONS
        .iter()
        .map(|&(w, h)| {
            if w == 0 {
                "Native display".into()
            } else {
                format!("{w} \u{00D7} {h}")
            }
        })
        .collect();
    let hz_names: Vec<String> = REFRESH
        .iter()
        .map(|&r| {
            if r == 0 {
                "Native".into()
            } else {
                format!("{r} Hz")
            }
        })
        .collect();

    let res_combo = {
        let ctx = ctx.clone();
        ComboBox::new(res_names)
            .header("Resolution")
            .selected_index(res_i)
            .on_selection_changed(move |i: i32| {
                let (w, h) = RESOLUTIONS[(i.max(0) as usize).min(RESOLUTIONS.len() - 1)];
                let mut s = ctx.settings.lock().unwrap();
                (s.width, s.height) = (w, h);
                s.save();
            })
    };
    let hz_combo = {
        let ctx = ctx.clone();
        ComboBox::new(hz_names)
            .header("Refresh rate")
            .selected_index(hz_i)
            .on_selection_changed(move |i: i32| {
                let mut s = ctx.settings.lock().unwrap();
                s.refresh_hz = REFRESH[(i.max(0) as usize).min(REFRESH.len() - 1)];
                s.save();
            })
    };
    let dec_i = DECODERS
        .iter()
        .position(|&(v, _)| v == s.decoder)
        .unwrap_or(0) as i32;
    let dec_names: Vec<String> = DECODERS.iter().map(|&(_, l)| l.to_string()).collect();
    let decoder_combo = {
        let ctx = ctx.clone();
        ComboBox::new(dec_names)
            .header("Video decoder")
            .selected_index(dec_i)
            .on_selection_changed(move |i: i32| {
                let (v, _) = DECODERS[(i.max(0) as usize).min(DECODERS.len() - 1)];
                let mut s = ctx.settings.lock().unwrap();
                s.decoder = v.to_string();
                s.save();
            })
    };

    let br_i = BITRATES_MBPS
        .iter()
        .position(|&m| m * 1000 == s.bitrate_kbps)
        .unwrap_or(0) as i32;
    let br_names: Vec<String> = BITRATES_MBPS
        .iter()
        .map(|&m| {
            if m == 0 {
                "Automatic".into()
            } else {
                format!("{m} Mb/s")
            }
        })
        .collect();
    let bitrate_combo = {
        let ctx = ctx.clone();
        ComboBox::new(br_names)
            .header("Bitrate")
            .selected_index(br_i)
            .on_selection_changed(move |i: i32| {
                let m = BITRATES_MBPS[(i.max(0) as usize).min(BITRATES_MBPS.len() - 1)];
                let mut s = ctx.settings.lock().unwrap();
                s.bitrate_kbps = m * 1000;
                s.save();
            })
    };

    let hdr_toggle = {
        let ctx = ctx.clone();
        ToggleSwitch::new(s.hdr_enabled)
            .header("HDR (10-bit, BT.2020 PQ)")
            .on_content("On")
            .off_content("Off")
            .on_changed(move |on: bool| {
                let mut s = ctx.settings.lock().unwrap();
                s.hdr_enabled = on;
                s.save();
            })
    };
    let mic_toggle = {
        let ctx = ctx.clone();
        ToggleSwitch::new(s.mic_enabled)
            .header("Stream microphone to the host")
            .on_content("On")
            .off_content("Off")
            .on_changed(move |on: bool| {
                let mut s = ctx.settings.lock().unwrap();
                s.mic_enabled = on;
                s.save();
            })
    };

    let header = grid((
        text_block("Settings")
            .font_size(30.0)
            .bold()
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
        button("Back")
            .accent()
            .icon(SymbolGlyph::Back)
            .on_click({
                let ss = set_screen.clone();
                move || ss.call(Screen::Hosts)
            })
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center),
    ))
    .columns([GridLength::Star(1.0), GridLength::Auto])
    .margin(edges(0.0, 0.0, 0.0, 6.0));

    let stream_card = card(
        vstack((
            text_block("Display").font_size(15.0).semibold(),
            text_block("The host creates a virtual display at exactly this mode.")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            res_combo,
            hz_combo,
        ))
        .spacing(10.0),
    );

    let video_card = card(
        vstack((
            text_block("Video").font_size(15.0).semibold(),
            text_block(
                "Hardware decode (D3D11VA) is zero-copy and far lighter than software — keep it on \
                 Automatic unless debugging.",
            )
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
            decoder_combo,
            bitrate_combo,
            hdr_toggle,
        ))
        .spacing(10.0),
    );

    let audio_card =
        card(vstack((text_block("Audio").font_size(15.0).semibold(), mic_toggle)).spacing(10.0));

    page(vec![
        header.into(),
        section("DISPLAY"),
        stream_card.into(),
        section("VIDEO"),
        video_card.into(),
        section("AUDIO"),
        audio_card.into(),
    ])
}

// --- stream page --------------------------------------------------------------------------

fn present_newest(ctx: &mut PresentCtx) {
    // Drain to the newest decoded frame (drop any backlog) and hand it to the presenter by value —
    // the GPU zero-copy path retains the decoder surface across re-presents, so ownership matters.
    let mut newest = None;
    while let Ok(f) = ctx.frames.try_recv() {
        newest = Some(f);
    }
    ctx.presenter.present(newest);
}

fn stream_page(props: &StreamProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    // Take the connector + frames handoff once on mount; keep the connector alive (and for input)
    // in a use_ref, stash frames for `on_ready`, install the input hooks (and remove on unmount).
    let connector_ref = cx.use_ref::<Option<Arc<NativeClient>>>(None);
    cx.use_effect_with_cleanup((), {
        let shared = ctx.shared.clone();
        let connector_ref = connector_ref.clone();
        move || {
            if let Some((connector, frames)) = shared.handoff.lock().unwrap().take() {
                let mode = connector.mode();
                connector_ref.set(Some(connector.clone()));
                PENDING_FRAMES.with(|c| *c.borrow_mut() = Some(frames));
                crate::input::install(connector, mode);
            }
            Some(crate::input::uninstall)
        }
    });

    let rendering = cx.use_ref::<Option<Rendering>>(None);
    cx.use_effect((), {
        let rendering = rendering.clone();
        move || {
            if let Ok(r) = on_rendering(move || {
                PRESENT.with(|cell| {
                    if let Some(ctx) = cell.borrow_mut().as_mut() {
                        present_newest(ctx);
                    }
                });
            }) {
                rendering.set(Some(r));
            }
        }
    });

    let mode = connector_ref.borrow().as_ref().map(|c| c.mode());
    grid((
        swap_chain_panel()
            .on_ready(|panel| match Presenter::new(1280, 720) {
                Ok(p) => {
                    if let Err(e) = panel.set_swap_chain(p.swap_chain()) {
                        tracing::error!(error = %e, "set_swap_chain");
                    }
                    if let Some(frames) = PENDING_FRAMES.with(|c| c.borrow_mut().take()) {
                        PRESENT.with(|cell| {
                            *cell.borrow_mut() = Some(PresentCtx {
                                presenter: p,
                                frames,
                            });
                        });
                        tracing::info!("stream presenter bound to SwapChainPanel");
                    }
                }
                Err(e) => tracing::error!(error = %e, "create presenter"),
            })
            .on_resize(|w, h| {
                PRESENT.with(|cell| {
                    if let Some(ctx) = cell.borrow_mut().as_mut() {
                        ctx.presenter.resize(w as u32, h as u32);
                    }
                });
            }),
        hud_overlay(&props.stats, mode),
    ))
    .into()
}

/// A small chip for the dark HUD: coloured text on a translucent dark fill.
fn hud_chip(text: &str, color: Color) -> Border {
    border(
        text_block(text)
            .font_size(11.0)
            .semibold()
            .foreground(color),
    )
    .background(Color::rgb(38, 38, 38))
    .corner_radius(8.0)
    .padding(edges(8.0, 2.0, 8.0, 2.0))
}

/// The streaming HUD overlay (top-right), mirroring the Apple client: a chip row (mode · decode
/// path · HDR), the fps/throughput/latency line, and the release-cursor hint. Layered over the
/// `SwapChainPanel` in the same grid cell.
fn hud_overlay(stats: &Stats, mode: Option<Mode>) -> Element {
    let res = mode
        .map(|m| format!("{}\u{00D7}{}@{}", m.width, m.height, m.refresh_hz))
        .unwrap_or_else(|| "\u{2014}".into());
    let mut chips: Vec<Element> = vec![hud_chip(&res, Color::rgb(235, 235, 235)).into()];
    chips.push(if stats.hardware {
        hud_chip("GPU decode", Color::rgb(120, 220, 150)).into()
    } else {
        hud_chip("CPU decode", Color::rgb(240, 190, 90)).into()
    });
    if stats.hdr {
        chips.push(hud_chip("HDR", Color::rgb(255, 205, 90)).into());
    }
    let line = format!(
        "{:.0} fps \u{00B7} {:.1} Mb/s \u{00B7} {:.1} ms p50 \u{00B7} decode {:.1} ms",
        stats.fps, stats.mbps, stats.latency_ms, stats.decode_ms
    );
    border(
        vstack((
            hstack(chips).spacing(6.0),
            text_block(line)
                .font_size(11.0)
                .foreground(Color::rgb(210, 210, 210)),
            text_block("Ctrl+Alt+Shift+Q releases the mouse")
                .font_size(11.0)
                .foreground(Color::rgb(150, 150, 150)),
        ))
        .spacing(6.0),
    )
    .background(Color::rgb(0, 0, 0))
    .corner_radius(10.0)
    .padding(uniform(10.0))
    .opacity(0.82)
    .horizontal_alignment(HorizontalAlignment::Right)
    .vertical_alignment(VerticalAlignment::Top)
    .margin(uniform(12.0))
    .into()
}
