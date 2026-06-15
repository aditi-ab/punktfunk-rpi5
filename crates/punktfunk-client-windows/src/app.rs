//! The WinUI 3 (windows-reactor) application shell — host list, settings, PIN/TOFU pairing, and
//! the stream page (a `SwapChainPanel` bound to the D3D11 composition swapchain in
//! [`crate::present`], driven by reactor's per-frame `on_rendering`).
//!
//! Declarative React-like model: a single root component routes on a `Screen` value held in
//! `use_async_state` so background threads (discovery, the session pump) can drive navigation.
//! The present + decoded-frame handoff crosses to the UI thread through a `Mutex` side-channel
//! and thread-locals (the windows-reactor SwapChainPanel sample's pattern), since the per-frame
//! present must not go through state/rerender.

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
        .inner_size(1100.0, 720.0)
        .render(move |cx| root(cx, &ctx))
}

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
            text_block("Connecting…").font_size(20.0),
            text_block(status.clone()),
        ))
        .spacing(12.0)
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

    let mut rows: Vec<Element> = Vec::new();
    rows.push(text_block("Punktfunk").font_size(28.0).bold().into());

    // Saved (trusted/paired) hosts.
    if !known.hosts.is_empty() {
        rows.push(text_block("Saved hosts").font_size(16.0).bold().into());
        for k in &known.hosts {
            let t = Target {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                pair_optional: false,
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            rows.push(
                button(format!(
                    "{}  ·  {}:{}  ·  {}",
                    k.name,
                    k.addr,
                    k.port,
                    if k.paired { "paired" } else { "trusted" }
                ))
                .on_click(move || initiate(&ctx2, t.clone(), &ss, &st))
                .into(),
            );
        }
    }

    // Discovered hosts.
    rows.push(
        text_block("Hosts on this network")
            .font_size(16.0)
            .bold()
            .into(),
    );
    if hosts.is_empty() {
        rows.push(text_block("Searching the LAN…").into());
    }
    for h in hosts {
        let t = Target {
            name: h.name.clone(),
            addr: h.addr.clone(),
            port: h.port,
            fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
            pair_optional: h.pair == "optional",
        };
        let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
        rows.push(
            button(format!(
                "{}  ·  {}:{}  ·  pairing {}",
                h.name,
                h.addr,
                h.port,
                if h.pair.is_empty() {
                    "optional"
                } else {
                    &h.pair
                }
            ))
            .on_click(move || initiate(&ctx2, t.clone(), &ss, &st))
            .into(),
        );
    }

    // Manual connection.
    rows.push(
        text_block("Manual connection")
            .font_size(16.0)
            .bold()
            .into(),
    );
    rows.push(
        text_box(manual.clone())
            .placeholder("host:port")
            .on_changed(move |s| set_manual.call(s))
            .into(),
    );
    {
        let (ctx2, ss, st, text) = (ctx.clone(), set_screen.clone(), set_status.clone(), manual);
        rows.push(
            button("Connect")
                .accent()
                .on_click(move || {
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
                })
                .into(),
        );
    }

    {
        let ss = set_screen.clone();
        rows.push(
            button("Settings")
                .on_click(move || ss.call(Screen::Settings))
                .into(),
        );
    }
    if !status.is_empty() {
        rows.push(text_block(status.to_string()).into());
    }

    vstack(rows).spacing(8.0).into()
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

    let (ctx2, ss, st, code2, target2) = (
        ctx.clone(),
        set_screen.clone(),
        set_status.clone(),
        code.clone(),
        target.clone(),
    );
    let pair_btn = button("Pair & Connect").accent().on_click(move || {
        let pin = code2.trim().to_string();
        let (ctx3, ss, st, target3) = (ctx2.clone(), ss.clone(), st.clone(), target2.clone());
        std::thread::spawn(move || {
            let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows-client".into());
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
    });
    let back = {
        let ss = set_screen.clone();
        button("Cancel").on_click(move || ss.call(Screen::Hosts))
    };

    vstack((
        text_block(format!("Pair with {}", target.name))
            .font_size(22.0)
            .bold(),
        text_block("Arm pairing on the host (console or web UI), then enter the 4-digit PIN."),
        text_box(code)
            .placeholder("PIN")
            .on_changed(move |s| set_code.call(s)),
        hstack((pair_btn, back)).spacing(8.0),
    ))
    .spacing(12.0)
    .into()
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
                format!("{w} × {h}")
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
    let back = {
        let ss = set_screen.clone();
        button("Back")
            .accent()
            .on_click(move || ss.call(Screen::Hosts))
    };

    vstack((
        text_block("Settings").font_size(28.0).bold(),
        res_combo,
        hz_combo,
        mic_toggle,
        back,
    ))
    .spacing(12.0)
    .into()
}

fn present_newest(ctx: &mut PresentCtx) {
    let mut newest = None;
    while let Ok(f) = ctx.frames.try_recv() {
        newest = Some(f);
    }
    let cpu = newest.as_ref().map(|DecodedFrame::Cpu(c)| c);
    ctx.presenter.present(cpu);
}

fn stream_page(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
    // Take the connector + frames handoff once on mount; keep the connector alive (and for
    // input once that lands) in a use_ref, stash frames for `on_ready`.
    let connector_ref = cx.use_ref::<Option<Arc<NativeClient>>>(None);
    cx.use_effect((), {
        let shared = ctx.shared.clone();
        let connector_ref = connector_ref.clone();
        move || {
            if let Some((connector, frames)) = shared.handoff.lock().unwrap().take() {
                connector_ref.set(Some(connector));
                PENDING_FRAMES.with(|c| *c.borrow_mut() = Some(frames));
            }
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
