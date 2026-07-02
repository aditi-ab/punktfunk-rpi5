//! The WinUI 3 (windows-reactor) application shell.
//!
//! Declarative React-like model: this root component routes on a `Screen` value held in
//! `use_async_state` so background threads (discovery, the session pump) can drive navigation.
//! Each screen lives in its own submodule:
//!
//! * [`hosts`] — saved/discovered/manual host list, plus per-host forget + speed test
//! * [`connect`] — the trust gate and session lifecycle glue (connect / request-access flows)
//! * [`pair`] — the SPAKE2 PIN pairing ceremony
//! * [`speed`] — the per-host network speed test (probe burst over the real data plane)
//! * [`settings`] — persisted preferences · [`licenses`] — the license notices screen
//! * [`stream`] — the live stream: `SwapChainPanel` + D3D11 presenter + HUD overlay
//! * [`style`] — the shared look (cards, pills, monograms), following the windows-reactor
//!   gallery: Mica backdrop, a centred max-width column, theme brushes (`ThemeRef`)
//!
//! **Re-render discipline** (reactor's rules): each hook-using screen is mounted as its own
//! `component(...)` so its hooks live in an isolated slot list. A child's *sync* `use_state`
//! marks it dirty and re-renders it; an `AsyncSetState` written from a background thread does
//! NOT (the child is pruned when its props are unchanged) — so everything thread-driven
//! (discovery, HUD stats, speed-test results) is held as *root* state and passed down as props.
//! The present + decoded-frame handoff crosses to the UI thread through a `Mutex` side-channel
//! and thread-locals (the windows-reactor SwapChainPanel sample's pattern), since the per-frame
//! present must not go through state/rerender.

mod connect;
mod hosts;
mod licenses;
mod pair;
mod settings;
mod speed;
mod stream;
mod style;

use crate::discovery::{self, DiscoveredHost};
use crate::gamepad::GamepadService;
use crate::session::Stats;
use crate::trust::Settings;
use crate::video::DecodedFrame;
use hosts::HostsProps;
use punktfunk_core::client::NativeClient;
use speed::{SpeedProps, SpeedState};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use stream::StreamProps;
use windows_reactor::*;

#[derive(Clone, PartialEq)]
pub(crate) enum Screen {
    Hosts,
    Connecting,
    /// The no-PIN "request access" wait: an identified connect is in flight, parked by the host
    /// until the operator approves this device in its console. Cancelable.
    RequestAccess,
    Stream,
    Settings,
    /// Open-source / third-party license notices (reached from Settings).
    Licenses,
    Pair,
    /// Per-host network speed test (probe burst + recommended bitrate).
    SpeedTest,
}

/// The host we're about to connect to / pair with / speed-test (carried into those screens
/// via `Shared::target`).
#[derive(Clone, Default)]
pub(crate) struct Target {
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) port: u16,
    pub(crate) fp_hex: Option<String>,
    pub(crate) pair_optional: bool,
}

/// Stable app services handed to the page components as props. Each routed screen that uses
/// hooks (`hosts_page`/`pair_page`/`stream_page`/`speed_page`) is mounted as its own
/// `component(...)`, so its hooks live in an isolated slot list — calling them on the shared
/// parent `cx` would change the hook order whenever the screen changes (reactor's
/// Rules-of-Hooks guard aborts).
///
/// `Svc` compares equal by `ctx` identity (it never meaningfully changes across renders), so a
/// page whose props are just `Svc` re-renders only via its own state hooks, never spuriously
/// from the parent.
#[derive(Clone)]
pub(crate) struct Svc {
    pub(crate) ctx: Arc<AppCtx>,
    pub(crate) set_screen: AsyncSetState<Screen>,
    pub(crate) set_status: AsyncSetState<String>,
    /// Speed-test lifecycle lives in root state (thread-driven — see the module docs); the hosts
    /// page resets it to `Running` before navigating, the probe worker completes it.
    pub(crate) set_speed: AsyncSetState<SpeedState>,
}

impl PartialEq for Svc {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
    }
}

/// Cross-thread handoff from the session pump (off-thread) to the stream page (UI thread).
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) handoff: Mutex<Option<(Arc<NativeClient>, async_channel::Receiver<DecodedFrame>)>>,
    pub(crate) target: Mutex<Target>,
    /// Latest stream stats, written by the session's event loop and mirrored into reactor state
    /// by the HUD poll thread to drive the overlay.
    pub(crate) stats: Mutex<Stats>,
    /// Cancel flag for the in-flight "request access" connect. A FRESH flag is installed per
    /// request: the waiting screen's Cancel button reads it back from here and sets it, and that
    /// request's event loop (which captured the same `Arc` at spawn) then tears down silently when
    /// the parked connect finally resolves. `None` outside a request-access flow.
    pub(crate) cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Speed-test run generation, bumped by the hosts page when it starts a run. A probe worker
    /// only publishes its outcome while its generation is still current, so a test abandoned
    /// mid-run can't overwrite a newer run's result when it finally resolves.
    pub(crate) speed_gen: std::sync::atomic::AtomicU64,
}

pub struct AppCtx {
    pub(crate) identity: (String, String),
    pub(crate) settings: Mutex<Settings>,
    pub(crate) gamepad: GamepadService,
    pub(crate) shared: Arc<Shared>,
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

fn root(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
    let (screen, set_screen) = cx.use_async_state(Screen::Hosts);
    let (hosts, set_hosts) = cx.use_async_state(Vec::<DiscoveredHost>::new());
    let (status, set_status) = cx.use_async_state(String::new());
    let (hud, set_hud) = cx.use_async_state(stream::HudSample::default());
    let (speed, set_speed) = cx.use_async_state(SpeedState::Running);

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

    // HUD sample: the session event loop writes `shared.stats` and the input hooks track capture
    // state; this poll thread mirrors both into root state so the stream page gets them as a
    // *prop* (thread-driven state must be root state — see the module docs). The compare in
    // `AsyncSetState::call` makes the idle case free.
    cx.use_effect((), {
        let shared = ctx.shared.clone();
        let set_hud = set_hud.clone();
        move || {
            std::thread::Builder::new()
                .name("pf-hud".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    set_hud.call(stream::HudSample {
                        stats: *shared.stats.lock().unwrap(),
                        captured: crate::input::is_captured(),
                    });
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
        set_speed: set_speed.clone(),
    };
    match screen {
        Screen::Hosts => component(hosts::hosts_page, HostsProps { svc, hosts, status }),
        // connecting_page / request_access_page / settings_page / licenses_page use no hooks
        // (they never touch `cx`), so calling them inline is sound.
        Screen::Connecting => connect::connecting_page(ctx, &status),
        Screen::RequestAccess => connect::request_access_page(ctx, &set_screen),
        Screen::Settings => settings::settings_page(ctx, &set_screen),
        Screen::Licenses => licenses::licenses_page(&set_screen),
        Screen::Pair => component(pair::pair_page, svc),
        Screen::SpeedTest => component(speed::speed_page, SpeedProps { svc, state: speed }),
        Screen::Stream => component(stream::stream_page, StreamProps { svc, hud }),
    }
}
