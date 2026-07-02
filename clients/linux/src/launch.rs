//! Session launch: resolve the stream mode, spawn the session worker, and drive its
//! event stream into the UI (trust persistence, stream-page push, teardown).

use crate::app::App;
use crate::session::{SessionEvent, SessionParams, Stats};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use crate::video::DecodedFrame;
use adw::prelude::*;
use gtk::{gdk, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// The mode to request: explicit settings, with `0` fields resolved to the native
/// size/refresh of the monitor the window currently occupies (mirrors the Swift client's
/// native-display default).
fn resolve_mode(app: &App) -> Mode {
    let s = app.settings.borrow();
    let mut mode = Mode {
        width: s.width,
        height: s.height,
        refresh_hz: s.refresh_hz,
    };
    if mode.width == 0 || mode.refresh_hz == 0 {
        // Prefer the monitor the window is on; fall back to the display's first monitor. On a
        // `--connect` launch the window may not be mapped yet when this runs, and without the
        // fallback we'd drop to the 1920×1080 floor below — wrong on the Deck (1280×800).
        let monitor = app
            .window
            .surface()
            .zip(gdk::Display::default())
            .and_then(|(surf, d)| d.monitor_at_surface(&surf))
            .or_else(|| {
                gdk::Display::default()
                    .and_then(|d| d.monitors().item(0))
                    .and_then(|o| o.downcast::<gdk::Monitor>().ok())
            });
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

/// Tunables for a session start that differ between the normal connect and the "request access"
/// (delegated-approval) flow. `Default` is the normal connect.
pub struct StartOpts {
    /// Handshake budget. The request-access flow uses a long one because the host PARKS the
    /// connection until the operator clicks Approve (see the host's `PENDING_APPROVAL_WAIT`).
    pub connect_timeout: std::time::Duration,
    /// Persist the host as *paired* on a successful connect. Set for request-access, where the
    /// operator's approval IS the pairing, so future connects are silent (rule 1). Normal TOFU
    /// persists the host *unpaired* (pinned, but not PIN/approval-verified).
    pub persist_paired: bool,
    /// A "waiting for approval" dialog to dismiss on the first session event (request-access only).
    pub waiting: Option<adw::AlertDialog>,
    /// Set by the waiting dialog's Cancel button. `NativeClient::connect` is a blocking call with
    /// no abort, so Cancel returns the UI immediately (clears busy, closes the dialog) and leaves
    /// the in-flight connect to time out; when it finally resolves, the event loop sees this flag
    /// and tears down silently (drops the connector → closes the connection) without touching the
    /// UI a new session may already own.
    pub cancel: Option<Rc<std::cell::Cell<bool>>>,
}

impl Default for StartOpts {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(15),
            persist_paired: false,
            waiting: None,
            cancel: None,
        }
    }
}

pub fn start_session(app: Rc<App>, req: ConnectRequest, pin: Option<[u8; 32]>) {
    start_session_with(app, req, pin, StartOpts::default());
}

pub fn start_session_with(
    app: Rc<App>,
    req: ConnectRequest,
    pin: Option<[u8; 32]>,
    opts: StartOpts,
) {
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
        audio_channels: s.audio_channels,
        preferred_codec: s.preferred_codec(),
        decoder: s.decoder.clone(),
        launch: req.launch.as_ref().map(|(id, _)| id.clone()),
        pin,
        identity: app.identity.clone(),
        connect_timeout: opts.connect_timeout,
    };
    let inhibit = s.inhibit_shortcuts;
    let show_stats = s.show_stats;
    drop(s);
    let cancel = opts.cancel;

    // Card feedback while the connect is in flight: spinner on the matching hosts card,
    // stale failure banner dismissed. Cleared again on Connected/Failed/Ended.
    if let Some(h) = app.hosts_ui() {
        h.clear_error();
        h.set_connecting(Some(req.card_key()));
    }

    let mut handle = crate::session::start(params);
    let frames = std::mem::replace(&mut handle.frames, async_channel::bounded(1).1);
    let mut ctx = SessionUi {
        stop: handle.stop.clone(),
        app,
        req,
        persist_paired: opts.persist_paired,
        tofu: pin.is_none(),
        inhibit,
        show_stats,
        frames: Some(frames),
        waiting: opts.waiting,
        page: None,
    };
    glib::spawn_future_local(async move {
        while let Ok(event) = handle.events.recv().await {
            // A cancelled request-access connect resolved late: tear down silently. Don't touch
            // app.busy — Cancel already cleared it, and a fresh session may now own it.
            if cancel.as_ref().is_some_and(|c| c.get()) {
                ctx.close_waiting();
                break;
            }
            match event {
                SessionEvent::Connected {
                    connector,
                    mode,
                    fingerprint,
                } => ctx.on_connected(connector, mode, fingerprint),
                SessionEvent::Stats(s) => ctx.on_stats(s),
                SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => {
                    ctx.on_failed(&msg, trust_rejected);
                    break;
                }
                SessionEvent::Ended(err) => {
                    ctx.on_ended(err);
                    break;
                }
            }
        }
    });
}

/// UI-side state one session's event loop carries between events.
struct SessionUi {
    app: Rc<App>,
    req: ConnectRequest,
    /// Persist the host as PAIRED on `Connected` (request-access — the approval IS the pairing).
    persist_paired: bool,
    /// This is a TOFU connect (no stored pin): pin the observed fingerprint on `Connected`.
    tofu: bool,
    /// Grab compositor shortcuts while input is captured (Settings).
    inhibit: bool,
    /// Show the stats OSD when the stream page opens (Settings; live-toggled on-page).
    show_stats: bool,
    stop: Arc<AtomicBool>,
    /// Decoded-frame receiver, handed to the stream page once on `Connected`.
    frames: Option<async_channel::Receiver<DecodedFrame>>,
    /// The "waiting for approval" dialog (request-access flow), dismissed on the first event.
    waiting: Option<adw::AlertDialog>,
    page: Option<crate::ui_stream::StreamPage>,
}

impl SessionUi {
    /// Dismiss the "waiting for approval" dialog (request-access flow), if any.
    fn close_waiting(&mut self) {
        if let Some(w) = self.waiting.take() {
            w.close();
        }
    }

    /// `Connected`: record the configured trust decision, attach gamepads, and push the
    /// stream page.
    fn on_connected(&mut self, connector: Arc<NativeClient>, mode: Mode, fingerprint: [u8; 32]) {
        self.close_waiting();
        if let Some(h) = self.app.hosts_ui() {
            h.set_connecting(None);
        }
        if self.persist_paired {
            // Request-access: the operator approved this device, so record the host as
            // a trusted PAIRED host (pinning the fingerprint we observed) — future
            // connects are then silent (rule 1), exactly like after a PIN ceremony.
            let fp_hex = trust::hex(&fingerprint);
            trust::persist_host(&self.req.name, &self.req.addr, self.req.port, &fp_hex, true);
            self.app.toast("Approved — connecting…");
        } else if self.tofu {
            // A TOFU connect just observed the real fingerprint — pin it from now on.
            let fp_hex = trust::hex(&fingerprint);
            trust::persist_host(
                &self.req.name,
                &self.req.addr,
                self.req.port,
                &fp_hex,
                false,
            );
            self.app.toast(&format!(
                "Trusted on first use — fingerprint {}…",
                &fp_hex[..16]
            ));
        }
        // Stamp the successful connect — this host's card carries the accent bar now.
        trust::touch_last_used(&trust::hex(&fingerprint));
        tracing::debug!(?mode, "connected — pushing stream page");
        // A library launch titles the stream with the game, not the host.
        let name = self
            .req
            .launch
            .as_ref()
            .map_or(self.req.name.as_str(), |(_, game)| game.as_str());
        let title = format!(
            "{name} · {}×{}@{}",
            mode.width, mode.height, mode.refresh_hz
        );
        self.app.gamepad.attach(connector.clone());
        let clock_offset_ns = connector.clock_offset_ns;
        let p = crate::ui_stream::new(crate::ui_stream::StreamPageArgs {
            window: self.app.window.clone(),
            connector,
            frames: self.frames.take().expect("Connected delivered once"),
            clock_offset_ns,
            escape_rx: self.app.gamepad.escape_events(),
            disconnect_rx: self.app.gamepad.disconnect_events(),
            stop: self.stop.clone(),
            inhibit_shortcuts: self.inhibit,
            show_stats: self.show_stats,
            title,
        });
        self.app.nav.push(&p.page);
        // Steam Deck / Gaming Mode: gamescope fullscreens the window but GTK doesn't
        // know it, so its header bar stays drawn. Enter GTK fullscreen explicitly —
        // the stream page's `connect_fullscreened_notify` then hides all chrome.
        if self.app.fullscreen {
            self.app.window.fullscreen();
        }
        self.page = Some(p);
    }

    fn on_stats(&self, s: Stats) {
        if let Some(p) = &self.page {
            p.update_stats(s);
        }
    }

    /// `Failed`: surface the error; a trust rejection on a pinned connect routes to re-pairing.
    fn on_failed(&mut self, msg: &str, trust_rejected: bool) {
        self.close_waiting();
        tracing::warn!(%msg, trust_rejected, "connect failed");
        self.app.busy.set(false);
        if let Some(h) = self.app.hosts_ui() {
            h.set_connecting(None);
        }
        // A pinned connect rejected on trust grounds means the host's cert no
        // longer matches the stored pin (rotated cert or impostor) — route to
        // the PIN ceremony to re-establish trust rather than dead-ending.
        if trust_rejected && !self.tofu {
            self.app
                .toast("Host fingerprint changed — re-pair with a PIN to continue");
            crate::ui_trust::pin_dialog(self.app.clone(), self.req.clone());
        } else {
            // Errors land on the hosts page banner, not a transient toast.
            self.app.connect_error(&format!("Couldn't connect — {msg}"));
        }
    }

    /// `Ended`: detach gamepads, pop back to the hosts page, and surface the reason.
    fn on_ended(&mut self, err: Option<String>) {
        self.close_waiting();
        self.app.gamepad.detach();
        self.app.nav.pop_to_tag("hosts");
        if let Some(h) = self.app.hosts_ui() {
            h.set_connecting(None);
        }
        if let Some(e) = err {
            self.app.connect_error(&e);
        }
        self.app.busy.set(false);
    }
}
