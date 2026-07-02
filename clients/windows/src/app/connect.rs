//! The trust gate and session lifecycle glue: `initiate` routes a connect through the trust
//! rules (pinned → silent, `pair=optional` → TOFU, otherwise → PIN), `connect_with` starts the
//! session worker and drives navigation from its events, and the "request access"
//! (delegated-approval) flow parks an identified connect until the operator approves it.

use super::style::*;
use super::{AppCtx, Screen, Svc, Target};
use crate::session::{self, SessionEvent, SessionParams, Stats};
use crate::trust::{self, KnownHost, KnownHosts, Settings};
use crate::video::DecoderPref;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_reactor::*;

/// The trust gate (mirrors the GTK client's `initiate_connect`): pinned fingerprint → silent
/// connect; known address → stored pin; advertised `pair=optional` → TOFU; otherwise → PIN
/// pairing.
pub(crate) fn initiate(
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

/// The mode to request: explicit settings, with `0` fields resolved to the native size/refresh
/// of the display our window is on (mirrors the Linux/Swift clients' native-display default).
pub(crate) fn resolve_mode(s: &Settings) -> Mode {
    let mut mode = Mode {
        width: s.width,
        height: s.height,
        refresh_hz: s.refresh_hz,
    };
    if mode.width == 0 || mode.refresh_hz == 0 {
        if let Some((w, h, hz)) = current_display_mode() {
            if mode.width == 0 {
                (mode.width, mode.height) = (w, h);
            }
            if mode.refresh_hz == 0 {
                mode.refresh_hz = hz;
            }
        }
    }
    // No display info (headless session, RDP oddities) — a sane floor.
    if mode.width == 0 {
        (mode.width, mode.height) = (1920, 1080);
    }
    if mode.refresh_hz == 0 {
        mode.refresh_hz = 60;
    }
    mode
}

/// The current mode (physical pixels + refresh) of the display our window occupies:
/// `MonitorFromWindow` on the foreground window — ours, the user just clicked in it — then
/// `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` on that monitor's device. Defaults to the
/// primary display when we're not foreground (e.g. a scripted connect).
fn current_display_mode() -> Option<(u32, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
        MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe {
        let monitor = MonitorFromWindow(
            GetForegroundWindow(),
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTOPRIMARY,
        );
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(
            monitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
        .as_bool()
        {
            return None;
        }
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
        .as_bool()
        {
            return None;
        }
        // dmDisplayFrequency of 0/1 means "hardware default" — unusable as a mode request.
        (dm.dmPelsWidth > 0 && dm.dmDisplayFrequency > 1).then_some((
            dm.dmPelsWidth,
            dm.dmPelsHeight,
            dm.dmDisplayFrequency,
        ))
    }
}

/// Tunables that differ between the normal connect and the no-PIN "request access" flow.
/// `Default` is the normal connect: short handshake budget, persist *unpaired* on TOFU, and the
/// plain "Connecting" screen.
pub(crate) struct ConnectOpts {
    /// Handshake budget. Request-access uses a long one because the host PARKS the connection
    /// until the operator clicks Approve in its console (see the host's `PENDING_APPROVAL_WAIT`).
    connect_timeout: Duration,
    /// Persist the host as *paired* on a successful connect. Set for request-access, where the
    /// operator's approval IS the pairing, so future connects are silent (rule 1). Normal TOFU
    /// persists the host *unpaired* (pinned, but not PIN/approval-verified).
    persist_paired: bool,
    /// Show the cancelable "waiting for approval" screen instead of "Connecting" (request-access).
    awaiting_approval: bool,
    /// Set by the waiting screen's Cancel button. `NativeClient::connect` is blocking with no
    /// abort, so Cancel returns the UI immediately and leaves the parked connect to resolve/time
    /// out; this request's event loop (which captured the same `Arc` at spawn) then tears down
    /// silently when the parked connect finally resolves — without touching a screen a new
    /// session may already own.
    cancel: Option<Arc<AtomicBool>>,
}

impl Default for ConnectOpts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            persist_paired: false,
            awaiting_approval: false,
            cancel: None,
        }
    }
}

pub(crate) fn connect(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    connect_with(
        ctx,
        target,
        pin,
        set_screen,
        set_status,
        ConnectOpts::default(),
    );
}

fn connect_with(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    opts: ConnectOpts,
) {
    let s = ctx.settings.lock().unwrap().clone();
    let gamepad_pref = match GamepadPref::from_name(&s.gamepad) {
        Some(GamepadPref::Auto) | None => ctx.gamepad.auto_pref(),
        Some(explicit) => explicit,
    };
    let handle = session::start(SessionParams {
        host: target.addr.clone(),
        port: target.port,
        mode: resolve_mode(&s),
        compositor: CompositorPref::from_name(&s.compositor).unwrap_or(CompositorPref::Auto),
        gamepad: gamepad_pref,
        bitrate_kbps: s.bitrate_kbps,
        audio_channels: s.audio_channels,
        mic_enabled: s.mic_enabled,
        hdr_enabled: s.hdr_enabled,
        decoder: DecoderPref::from_name(&s.decoder),
        preferred_codec: s.preferred_codec(),
        pin,
        identity: ctx.identity.clone(),
        connect_timeout: opts.connect_timeout,
    });
    set_status.call(String::new());
    set_screen.call(if opts.awaiting_approval {
        Screen::RequestAccess
    } else {
        Screen::Connecting
    });

    let tofu = pin.is_none();
    let persist_paired = opts.persist_paired;
    let cancel = opts.cancel;
    let (shared, gamepad) = (ctx.shared.clone(), ctx.gamepad.clone());
    let (ss, st) = (set_screen.clone(), set_status.clone());
    let target = target.clone();
    std::thread::spawn(move || loop {
        let event = match handle.events.recv_blocking() {
            Ok(e) => e,
            Err(_) => {
                gamepad.detach();
                ss.call(Screen::Hosts);
                break;
            }
        };
        // A cancelled request-access connect that resolved late (the host approved or the park
        // timed out after the user walked away): tear down silently. Cancel already returned the
        // UI to the host list; dropping `event` (and with it any connector) closes the connection
        // without popping a stream or a stray error over the screen a new session may own.
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
            break;
        }
        match event {
            SessionEvent::Connected {
                connector,
                fingerprint,
                ..
            } => {
                if persist_paired || tofu {
                    // Request-access: the operator approved this device, so record the host as a
                    // trusted PAIRED host — future connects are then silent (rule 1), exactly like
                    // after a PIN ceremony. A plain TOFU connect persists it *unpaired* (pinned).
                    let mut k = KnownHosts::load();
                    k.upsert(KnownHost {
                        name: target.name.clone(),
                        addr: target.addr.clone(),
                        port: target.port,
                        fp_hex: trust::hex(&fingerprint),
                        paired: persist_paired,
                    });
                    let _ = k.save();
                }
                gamepad.attach(connector.clone());
                *shared.stats.lock().unwrap() = Stats::default(); // clear any prior session's numbers
                *shared.handoff.lock().unwrap() =
                    Some((connector, handle.frames.clone(), handle.stop.clone()));
                ss.call(Screen::Stream);
            }
            SessionEvent::Failed {
                msg,
                trust_rejected,
            } => {
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
            SessionEvent::Ended(err) => {
                // `None` = the user ended the session themselves (the disconnect shortcut) —
                // return to the host list silently; an error banner would read as a failure.
                st.call(err.unwrap_or_default());
                gamepad.detach();
                ss.call(Screen::Hosts);
                break;
            }
            SessionEvent::Stats(s) => *shared.stats.lock().unwrap() = s,
        }
    });
}

/// The no-PIN "request access" flow: open an identified connect that the host PARKS until the
/// operator approves this device in its console (or web UI), showing a cancelable "waiting"
/// screen meanwhile. On approval the SAME connection is admitted (no reconnect) and the host is
/// saved as paired, so later connects are silent.
pub(crate) fn request_access(props: &Svc, target: &Target) {
    let ctx = &props.ctx;
    // Pin the advertised certificate for a discovered host (defence against a host impostor while
    // we wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
    let pin = target.fp_hex.as_deref().and_then(trust::parse_hex32);
    // A fresh cancel flag per request, installed where the waiting screen's Cancel button can read
    // it back; this request's event loop captures the same `Arc` (via ConnectOpts) below.
    let cancel = Arc::new(AtomicBool::new(false));
    *ctx.shared.cancel.lock().unwrap() = Some(cancel.clone());
    connect_with(
        ctx,
        target,
        pin,
        &props.set_screen,
        &props.set_status,
        ConnectOpts {
            // Must exceed the host's approval window (PENDING_APPROVAL_WAIT) so a slow operator
            // approval still lands on this connection rather than timing the client out first.
            connect_timeout: Duration::from_secs(185),
            persist_paired: true,
            awaiting_approval: true,
            cancel: Some(cancel),
        },
    );
}

/// The plain "Connecting…" screen shown while the session worker handshakes. No hooks.
pub(crate) fn connecting_page(ctx: &Arc<AppCtx>, status: &str) -> Element {
    let target_name = ctx.shared.target.lock().unwrap().name.clone();
    let headline = if target_name.is_empty() {
        "Connecting\u{2026}".to_string()
    } else {
        format!("Connecting to {target_name}\u{2026}")
    };
    let detail = if status.is_empty() {
        "Negotiating the session and creating the virtual display\u{2026}"
    } else {
        status
    };
    busy_page(&headline, detail, Vec::new())
}

/// The cancelable "waiting for approval" screen (request-access flow): a spinner + guidance while
/// the identified connect sits parked on the host, plus a Cancel that returns to the host list and
/// trips the shared cancel flag so the parked connect tears down silently if it resolves after the
/// user has walked away. No hooks.
pub(crate) fn request_access_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
) -> Element {
    let target_name = ctx.shared.target.lock().unwrap().name.clone();
    let headline = if target_name.is_empty() {
        "Waiting for approval\u{2026}".to_string()
    } else {
        format!("Waiting for {target_name} to approve\u{2026}")
    };
    let cancel_btn = {
        let (ctx, ss) = (ctx.clone(), set_screen.clone());
        button("Cancel")
            .icon(Symbol::Cancel)
            .on_click(move || {
                // Return the UI immediately; the parked connect is blocking with no abort, so trip
                // the flag this request's event loop captured — it then tears down silently when
                // the connect finally resolves (see ConnectOpts::cancel).
                if let Some(c) = ctx.shared.cancel.lock().unwrap().as_ref() {
                    c.store(true, Ordering::SeqCst);
                }
                ss.call(Screen::Hosts);
            })
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    busy_page(
        &headline,
        "Approve this device in the host's console or web UI \u{2014} it connects automatically \
         once you approve it. No PIN needed.",
        vec![cancel_btn.into()],
    )
}
