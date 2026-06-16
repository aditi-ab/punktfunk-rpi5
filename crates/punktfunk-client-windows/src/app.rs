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
use crate::session::{self, SessionEvent, SessionParams};
use crate::trust::{self, KnownHost, KnownHosts, Settings};
use crate::video::DecodedFrame;
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

/// A clickable host row: name + address/badge + chevron.
fn host_card(name: &str, sub: &str, badge: &str, on_tap: impl Fn() + 'static) -> Element {
    card(
        grid((
            vstack((
                text_block(name).font_size(15.0).semibold(),
                text_block(sub)
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
            text_block(badge)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .margin(edges(0.0, 0.0, 12.0, 0.0)),
            text_block("\u{203A}")
                .font_size(18.0)
                .foreground(ThemeRef::SecondaryText)
                .grid_column(2)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::Star(1.0), GridLength::Auto, GridLength::Auto]),
    )
    .on_tapped(on_tap)
    .into()
}

// --- screens ------------------------------------------------------------------------------

fn root(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
    let (screen, set_screen) = cx.use_async_state(Screen::Hosts);
    let (hosts, set_hosts) = cx.use_async_state(Vec::<DiscoveredHost>::new());
    let (status, set_status) = cx.use_async_state(String::new());

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

    match screen {
        Screen::Hosts => hosts_page(cx, ctx, &hosts, &status, &set_screen, &set_status),
        Screen::Connecting => vstack((
            ProgressRing::indeterminate()
                .width(48.0)
                .height(48.0)
                .horizontal_alignment(HorizontalAlignment::Center),
            text_block("Connecting\u{2026}")
                .font_size(16.0)
                .horizontal_alignment(HorizontalAlignment::Center),
            text_block(status.clone())
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center),
        ))
        .spacing(16.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into(),
        Screen::Settings => settings_page(ctx, &set_screen),
        Screen::Pair => pair_page(cx, ctx, &set_screen, &set_status),
        Screen::Stream => stream_page(cx, ctx),
    }
}

fn hosts_page(
    cx: &mut RenderCx,
    ctx: &Arc<AppCtx>,
    hosts: &[DiscoveredHost],
    status: &str,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) -> Element {
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
        body.push(card(text_block(status.to_string()).foreground(ThemeRef::SystemCritical)).into());
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
            Ok(SessionEvent::Stats(_)) => {}
            Err(_) => {
                gamepad.detach();
                ss.call(Screen::Hosts);
                break;
            }
        }
    });
}

fn pair_page(
    cx: &mut RenderCx,
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) -> Element {
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
        button("Pair & Connect").accent().on_click(move || {
            let pin = code2.trim().to_string();
            let (ctx3, ss, st, target3) = (ctx2.clone(), ss.clone(), st.clone(), target2.clone());
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
        button("Cancel").on_click(move || ss.call(Screen::Hosts))
    };

    let content = card(vstack((
        text_block(format!("Pair with {}", target.name))
            .font_size(20.0)
            .semibold(),
        text_block(
            "Arm pairing on the host (its console or web console), then enter the 4-digit PIN it \
             shows.",
        )
        .foreground(ThemeRef::SecondaryText)
        .max_width(440.0),
        text_box(code)
            .placeholder("PIN")
            .on_changed(move |s| set_code.call(s)),
        hstack((pair_btn, cancel_btn)).spacing(8.0),
    ))
    .spacing(14.0))
    .max_width(480.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .margin(edges(0.0, 80.0, 0.0, 0.0));

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
    let mic_toggle = {
        let ctx = ctx.clone();
        check_box(s.mic_enabled)
            .label("Stream microphone to the host")
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
            text_block("Stream").font_size(15.0).semibold(),
            text_block("The host creates a virtual display at exactly this mode.")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            res_combo,
            hz_combo,
        ))
        .spacing(10.0),
    );

    let audio_card =
        card(vstack((text_block("Audio").font_size(15.0).semibold(), mic_toggle)).spacing(10.0));

    page(vec![
        header.into(),
        section("STREAM"),
        stream_card.into(),
        section("AUDIO"),
        audio_card.into(),
    ])
}

// --- stream page --------------------------------------------------------------------------

fn present_newest(ctx: &mut PresentCtx) {
    let mut newest = None;
    while let Ok(f) = ctx.frames.try_recv() {
        newest = Some(f);
    }
    let cpu = newest.as_ref().map(|DecodedFrame::Cpu(c)| c);
    ctx.presenter.present(cpu);
}

fn stream_page(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
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
            if let Ok(r) = on_rendering(|| {
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
        })
        .into()
}
