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
//! * [`settings`] — persisted preferences · [`licenses`] — the license notices screen ·
//!   [`help`] — the in-stream keyboard-shortcuts reference (reached from the host list)
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
mod help;
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
use crate::trust::{KnownHosts, Settings};
use hosts::HostsProps;
use punktfunk_core::client::NativeClient;
use speed::{SpeedProps, SpeedState};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stream::StreamProps;
use windows_reactor::*;

#[derive(Clone, PartialEq)]
pub(crate) enum Screen {
    Hosts,
    Connecting,
    /// The no-PIN "request access" wait: an identified connect is in flight, parked by the host
    /// until the operator approves this device in its console. Cancelable.
    RequestAccess,
    /// Wake-on-LAN "wait until up": a magic packet was sent to an offline saved host and we're
    /// polling mDNS for it to reappear (re-sending periodically) before dialing. Cancelable.
    Waking,
    Stream,
    Settings,
    /// Open-source / third-party license notices (reached from Settings).
    Licenses,
    /// In-stream keyboard-shortcuts reference + capture help (reached from the host list's Help
    /// button).
    Help,
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
    /// Wake-on-LAN MAC(s) for this host (from the saved store or the live advert) — used to send a
    /// magic packet before connecting to an offline host. Empty when none is known.
    pub(crate) mac: Vec<String>,
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

/// Cross-thread handoff from the session pump (off-thread) to the stream page (UI thread):
/// the connector (input sends), the decoded-frame channel (render thread), and the session's
/// stop flag (the disconnect shortcut trips it).
#[derive(Default)]
pub(crate) struct Shared {
    #[allow(clippy::type_complexity)]
    pub(crate) handoff:
        Mutex<Option<(Arc<NativeClient>, crate::session::FrameRx, Arc<AtomicBool>)>>,
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
    apply_window_icon_when_ready();
    App::new()
        .title("Punktfunk")
        .inner_size(1000.0, 720.0)
        .backdrop(Backdrop::Mica)
        .render(move |cx| root(cx, &ctx))
}

/// Stamp the embedded app icon (build.rs, resource ordinal 1) onto the top-level window once it
/// exists: `WM_SETICON` drives the title bar and Alt-Tab (plus the taskbar for unpackaged runs;
/// the MSIX taskbar/Start icons come from the package assets). windows-reactor creates its
/// window icon-less and exposes no handle before `App::render` blocks, so a short background
/// poll finds our own window by its (unique) title.
fn apply_window_icon_when_ready() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };
    let _ = std::thread::Builder::new()
        .name("pf-window-icon".into())
        .spawn(|| unsafe {
            for _ in 0..100 {
                if let Ok(hwnd) = FindWindowW(None, windows::core::w!("Punktfunk")) {
                    let Ok(module) = GetModuleHandleW(None) else {
                        return;
                    };
                    // Small (title bar) and big (Alt-Tab) at their native metrics, both from
                    // the multi-size .ico so nothing is scaled at draw time.
                    for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
                        let px = GetSystemMetrics(metric);
                        if let Ok(icon) = LoadImageW(
                            Some(module.into()),
                            windows::core::PCWSTR(1 as *const u16),
                            IMAGE_ICON,
                            px,
                            px,
                            LR_DEFAULTCOLOR,
                        ) {
                            SendMessageW(
                                hwnd,
                                WM_SETICON,
                                Some(WPARAM(which as usize)),
                                Some(LPARAM(icon.0 as isize)),
                            );
                        }
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
}

fn root(cx: &mut RenderCx, ctx: &Arc<AppCtx>) -> Element {
    let (screen, set_screen) = cx.use_async_state(Screen::Hosts);
    let (hosts, set_hosts) = cx.use_async_state(Vec::<DiscoveredHost>::new());
    let (status, set_status) = cx.use_async_state(String::new());
    let (hud, set_hud) = cx.use_async_state(stream::HudSample::default());
    let (speed, set_speed) = cx.use_async_state(SpeedState::Running);
    // Per-host action state for the hosts page. Root, not page-local: the "…" overflow is a WinUI
    // MenuFlyout whose item clicks are wired straight in the reactor backend, bypassing the normal
    // event-dispatch flush — a sync page-local setter marks state dirty but never re-renders. See
    // `hosts::HostsProps`.
    let (forget, set_forget) = cx.use_async_state(Option::<(String, String)>::None);
    let (rename, set_rename) = cx.use_async_state(Option::<(String, String)>::None);
    let (show_add, set_show_add) = cx.use_async_state(false);
    // Hovered host tile (its stable id), driving the WinUI-style card hover fill. Root state for
    // the same reason as `forget`/`rename`: pointer enter/exit handlers are wired straight in the
    // reactor backend, so only a root `AsyncSetState` reliably re-renders the page.
    let (hover, set_hover) = cx.use_async_state(Option::<String>::None);
    // Which Settings section the NavigationView shows (persists across visits this run).
    let (settings_nav, set_settings_nav) = cx.use_async_state("display".to_string());
    // Saved-host reachability, keyed by `fp_hex`, refreshed by the probe sweep below. Root state
    // (thread-driven → must be root to re-render — see the module docs), passed to the hosts page.
    let (probed, set_probed) = cx.use_async_state(HashMap::<String, bool>::new());

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
                        visible: crate::input::hud_visible(),
                        present: crate::render::present_stats(),
                    });
                })
                .ok();
        }
    });

    // Periodic reachability sweep (spawned once): a saved host reached only over a routed network
    // (Tailscale/VPN) never advertises on mDNS, so presence can't come from the discovery stream
    // alone. This probes every saved host (bounded, trust-agnostic QUIC handshake — one thread each
    // so a slow/unreachable host doesn't delay the rest) and mirrors the results into root state so
    // the tiles get them as a prop; the pip then reads `advertising OR probed-reachable` — the
    // display-side companion to the dial-first connect fix. The compare in `call` makes idle free.
    cx.use_effect((), {
        let set_probed = set_probed.clone();
        move || {
            std::thread::Builder::new()
                .name("pf-probe".into())
                .spawn(move || loop {
                    let handles: Vec<_> = KnownHosts::load()
                        .hosts
                        .into_iter()
                        .filter(|h| !h.addr.is_empty())
                        .map(|h| {
                            std::thread::spawn(move || {
                                (h.fp_hex, NativeClient::probe(&h.addr, h.port, Duration::from_millis(2500)))
                            })
                        })
                        .collect();
                    let map: HashMap<String, bool> =
                        handles.into_iter().filter_map(|h| h.join().ok()).collect();
                    set_probed.call(map);
                    std::thread::sleep(Duration::from_secs(12));
                })
                .ok();
        }
    });

    // Screen-entrance animation: each navigation slides the new screen up a few px while fading it
    // in (the Windows-Settings drill-in). It's a manual tween, not a composition animation, because
    // reactor's DSL exposes no static transform/translation setter and its one-shot animations run
    // from the visual's CURRENT value (a shown element is already at opacity 1, so nothing to fade
    // from). So a worker thread steps a 0 → 1 `progress` after each navigation; the wrapper maps it
    // to opacity (= progress) and a top margin (= (1-progress)·offset). The page components are
    // memoised on unchanged props, so each step is just a cheap root re-render updating two props.
    // A generation guard (bumped per navigation) stops a superseded tween so rapid nav can't fight.
    let anim_gen = cx.use_ref(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let (anim, set_anim) = cx.use_async_state((Option::<Screen>::None, 1.0f64));
    cx.use_effect(screen.clone(), {
        let (s, set_anim, gen) = (screen.clone(), set_anim.clone(), anim_gen.borrow().clone());
        move || {
            use std::sync::atomic::Ordering::SeqCst;
            let mine = gen.fetch_add(1, SeqCst) + 1;
            std::thread::spawn(move || {
                const STEPS: u32 = 14;
                for i in 0..=STEPS {
                    if gen.load(SeqCst) != mine {
                        return; // a newer navigation superseded this tween
                    }
                    let p = f64::from(i) / f64::from(STEPS);
                    let eased = 1.0 - (1.0 - p).powi(3); // ease-out cubic
                    set_anim.call((Some(s.clone()), eased));
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
            });
        }
    });
    // Progress for THIS screen: 0 until the tween for it starts (fresh navigation starts hidden +
    // offset, no flash), 1 once settled. A stale value for another screen reads as 0.
    let progress = if anim.0.as_ref() == Some(&screen) {
        anim.1
    } else {
        0.0
    };

    // Settings-section entrance: the same tween again, keyed on the selected section, so
    // switching panes slides the CONTENT column up (the sidebar stays put — this must not wrap
    // the NavigationView, so it can't ride the screen-level tween above). Entering Settings
    // fresh leaves it settled at 1 (only the screen tween plays; no double animation).
    let nav_gen = cx.use_ref(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let (nav_anim, set_nav_anim) = cx.use_async_state((String::new(), 1.0f64));
    cx.use_effect(settings_nav.clone(), {
        let (s, set_nav_anim, gen) = (
            settings_nav.clone(),
            set_nav_anim.clone(),
            nav_gen.borrow().clone(),
        );
        move || {
            use std::sync::atomic::Ordering::SeqCst;
            let mine = gen.fetch_add(1, SeqCst) + 1;
            std::thread::spawn(move || {
                const STEPS: u32 = 14;
                for i in 0..=STEPS {
                    if gen.load(SeqCst) != mine {
                        return; // a newer section switch superseded this tween
                    }
                    let p = f64::from(i) / f64::from(STEPS);
                    let eased = 1.0 - (1.0 - p).powi(3);
                    set_nav_anim.call((s.clone(), eased));
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
            });
        }
    });
    let nav_progress = if nav_anim.0 == settings_nav {
        nav_anim.1
    } else {
        0.0
    };

    // "Add host" modal entrance: the same manual tween as the screen navigation (see above for
    // why it can't be a composition animation), stepping 0 → 1 when the modal opens. The hosts
    // page maps it to the modal's opacity + a downward start offset (the slide-up) and the
    // scrim's fade. Closing resets to 0 instantly — the modal unmounts, nothing to animate.
    let add_gen = cx.use_ref(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let (add_anim, set_add_anim) = cx.use_async_state(0.0f64);
    cx.use_effect(show_add, {
        let (set_add_anim, gen) = (set_add_anim.clone(), add_gen.borrow().clone());
        move || {
            use std::sync::atomic::Ordering::SeqCst;
            let mine = gen.fetch_add(1, SeqCst) + 1;
            if !show_add {
                set_add_anim.call(0.0);
                return;
            }
            std::thread::spawn(move || {
                const STEPS: u32 = 12;
                for i in 0..=STEPS {
                    if gen.load(SeqCst) != mine {
                        return; // reopened/closed mid-tween — a newer run owns the value
                    }
                    let p = f64::from(i) / f64::from(STEPS);
                    set_add_anim.call(1.0 - (1.0 - p).powi(3)); // ease-out cubic
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
            });
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
    let body = match &screen {
        Screen::Hosts => component(
            hosts::hosts_page,
            HostsProps {
                svc,
                hosts,
                probed,
                status,
                forget,
                rename,
                show_add,
                add_anim,
                hover,
                set_forget,
                set_rename,
                set_show_add,
                set_hover,
            },
        ),
        // connecting_page / request_access_page / waking_page / settings_page / licenses_page /
        // help_page use no hooks (they never touch `cx`), so calling them inline is sound.
        Screen::Connecting => connect::connecting_page(ctx, &status),
        Screen::RequestAccess => connect::request_access_page(ctx, &set_screen),
        Screen::Waking => connect::waking_page(ctx, &set_screen),
        Screen::Settings => settings::settings_page(
            ctx,
            &set_screen,
            &settings_nav,
            &set_settings_nav,
            nav_progress,
        ),
        Screen::Licenses => licenses::licenses_page(&set_screen),
        Screen::Help => help::help_page(&set_screen),
        Screen::Pair => component(pair::pair_page, svc),
        Screen::SpeedTest => component(speed::speed_page, SpeedProps { svc, state: speed }),
        Screen::Stream => component(stream::stream_page, StreamProps { svc, hud }),
    };

    // The Stream screen owns the SwapChainPanel + per-frame present; never wrap it in an animated
    // opacity/offset layer. Everything else slides + fades in on navigation.
    if matches!(screen, Screen::Stream) {
        return body;
    }
    let offset = (1.0 - progress) * 22.0;
    border(body)
        .opacity(progress)
        .margin(Thickness {
            left: 0.0,
            top: offset,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}
