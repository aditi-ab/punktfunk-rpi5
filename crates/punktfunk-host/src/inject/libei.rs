//! libei input injection — the portable EI-sender path.
//!
//! Two ways to reach an EIS server ([`EiSource`]):
//! * **Portal** — `org.freedesktop.portal.RemoteDesktop` via `ashpd` (KWin, GNOME/Mutter),
//!   which hands us the EIS socket fd after the session grant.
//! * **Socket** — connect directly to a compositor's own EIS socket. gamescope runs an EIS
//!   server and exports its path to its children as `LIBEI_SOCKET`; our gamescope backend
//!   relays that path through a file so the injector can connect (no portal involved).
//!
//! Either way, `reis` drives the connection as an EI *sender*: bind the seat's
//! pointer/keyboard/scroll/button capabilities and, per device, `start_emulating` → emit →
//! `frame`. The session and the EIS connection must stay alive and the event stream must be
//! polled continuously (resume/pause/ping/modifier traffic), so the whole thing runs on a
//! dedicated thread with its own tokio runtime; the synchronous control thread reaches it
//! through an unbounded channel and [`LibeiInjector::inject`] merely enqueues.
//!
//! Keyboard codes are Linux evdev (the same space our VK→evdev table produces) and the
//! compositor supplies the keymap, so — unlike the wlr path — there is no keymap to upload and
//! no modifier mask to serialize: pressing the modifier *keys* (which Moonlight sends as normal
//! key events) is enough.

use super::{gs_button_to_evdev, vk_to_evdev, InputInjector};
use anyhow::{anyhow, Result};
use ashpd::desktop::{
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
    CreateSessionOptions, PersistMode,
};
use futures_util::StreamExt;
use punktfunk_core::input::{InputEvent, InputKind};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// `code` value marking a horizontal scroll event (mirrors `gamestream::input`).
const SCROLL_HORIZONTAL: u32 = 1;

/// Where to find the EIS server.
#[derive(Clone, Debug)]
pub enum EiSource {
    /// `org.freedesktop.portal.RemoteDesktop` (KWin, GNOME/Mutter).
    Portal,
    /// A file containing the EIS socket path/name (gamescope's relayed `LIBEI_SOCKET`); polled
    /// until it appears, since the compositor may still be starting.
    SocketPathFile(std::path::PathBuf),
}

/// Handle held by the control thread; forwards events to the libei worker thread.
pub struct LibeiInjector {
    tx: UnboundedSender<InputEvent>,
}

impl LibeiInjector {
    pub fn open() -> Result<Self> {
        Self::open_with(EiSource::Portal)
    }

    pub fn open_with(source: EiSource) -> Result<Self> {
        let (tx, rx) = unbounded_channel::<InputEvent>();
        std::thread::Builder::new()
            .name("punktfunk-libei".into())
            .spawn(move || worker(rx, source))
            .map_err(|e| anyhow!("spawn libei worker thread: {e}"))?;
        // Return immediately — the portal/socket handshake must NOT run on the caller's
        // (control) thread, or a slow/denied setup would freeze the ENet control stream and
        // drop the client. The worker establishes the session asynchronously and logs its
        // status; events enqueue until devices resume (a few startup events may be dropped).
        Ok(Self { tx })
    }
}

impl InputInjector for LibeiInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        self.tx
            .send(*event)
            .map_err(|_| anyhow!("libei worker thread has exited"))
    }
}

/// Worker thread entry: build a tokio runtime and run the session to completion.
fn worker(rx: UnboundedReceiver<InputEvent>, source: EiSource) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "libei: build tokio runtime failed");
            return;
        }
    };
    rt.block_on(session_main(rx, source));
}

/// Open the portal/socket + EIS (bounded), then pump events until disconnect or shutdown.
async fn session_main(mut rx: UnboundedReceiver<InputEvent>, source: EiSource) {
    // Keep `_rd`/`_session` bound for the whole loop — dropping the portal session closes the
    // EIS connection. Bound the setup so a headless approval dialog (un-bypassed grant) can't
    // hang the worker forever.
    let (_portal, context, mut events) = match tokio::time::timeout(
        Duration::from_secs(30),
        connect(source),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            tracing::error!(error = %format!("{e:#}"), "libei: portal/EIS setup failed");
            return;
        }
        Err(_) => {
            tracing::error!(
                    "libei: EIS setup timed out (headless approval needed / kde-authorized grant not seeded / gamescope socket never appeared)"
                );
            return;
        }
    };
    tracing::info!("libei: EIS connected — awaiting devices");

    let mut state = EiState::new();
    loop {
        tokio::select! {
            ei = events.next() => match ei {
                Some(Ok(ev)) => state.handle_ei(ev, &context),
                Some(Err(e)) => { tracing::warn!(error = %e, "libei: event stream error"); break; }
                None => { tracing::info!("libei: EIS disconnected"); break; }
            },
            msg = rx.recv() => match msg {
                Some(input) => state.inject(&input, &context),
                None => { tracing::info!("libei: injector closed — ending session"); break; }
            },
        }
    }
}

/// Tie down the verbose tuple the connect step returns. The portal pair must stay alive for
/// the whole session (dropping it closes the EIS connection); `None` for the direct-socket path.
type Connected = (
    Option<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>)>,
    ei::Context,
    reis::tokio::EiConvertEventStream,
);

/// Reach an EIS server per `source` and run the EI sender handshake.
async fn connect(source: EiSource) -> Result<Connected> {
    let (portal, stream) = match source {
        EiSource::Portal => {
            let (rd, session, fd) = connect_portal().await?;
            (Some((rd, session)), UnixStream::from(fd))
        }
        EiSource::SocketPathFile(file) => (None, connect_socket_file(&file).await?),
    };
    let context = ei::Context::new(stream).map_err(|e| anyhow!("reis EI context: {e}"))?;
    let (_conn, events) = context
        .handshake_tokio("punktfunk-host", ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| anyhow!("EI handshake: {e}"))?;
    Ok((portal, context, events))
}

/// Open a RemoteDesktop portal session (pointer + keyboard) and obtain the EIS socket fd.
async fn connect_portal() -> Result<(
    RemoteDesktop,
    ashpd::desktop::Session<RemoteDesktop>,
    std::os::fd::OwnedFd,
)> {
    let rd = RemoteDesktop::new()
        .await
        .map_err(|e| anyhow!("open RemoteDesktop portal (is xdg-desktop-portal-kde/gnome running and XDG_CURRENT_DESKTOP set?): {e}"))?;
    let session = rd
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| anyhow!("create RemoteDesktop session: {e}"))?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::DoNot),
    )
    .await
    .map_err(|e| anyhow!("select_devices: {e}"))?
    .response()
    .map_err(|e| anyhow!("select_devices response: {e}"))?;
    let started = rd
        .start(&session, None, StartOptions::default())
        .await
        .map_err(|e| anyhow!("start RemoteDesktop session: {e}"))?;
    let granted = started
        .response()
        .map_err(|e| anyhow!("RemoteDesktop start denied: {e}"))?;
    tracing::info!(devices = ?granted.devices(), "libei: portal granted devices");

    let fd = rd
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .map_err(|e| anyhow!("connect_to_eis (RemoteDesktop portal version < 2?): {e}"))?;
    Ok((rd, session, fd))
}

/// Poll `file` for the EIS socket path (the gamescope backend relays `LIBEI_SOCKET` there once
/// the nested app launches), then connect. A bare name is resolved against `XDG_RUNTIME_DIR`,
/// mirroring libei's own `LIBEI_SOCKET` semantics.
async fn connect_socket_file(file: &std::path::Path) -> Result<UnixStream> {
    let path = loop {
        match std::fs::read_to_string(file) {
            Ok(s) if !s.trim().is_empty() => break s.trim().to_string(),
            _ => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    };
    let full = if path.starts_with('/') {
        std::path::PathBuf::from(&path)
    } else {
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
            anyhow!("XDG_RUNTIME_DIR unset (needed to resolve EIS socket '{path}')")
        })?;
        std::path::Path::new(&runtime).join(&path)
    };
    tracing::info!(socket = %full.display(), "libei: connecting to EIS socket");
    UnixStream::connect(&full).map_err(|e| anyhow!("connect EIS socket {}: {e}", full.display()))
}

/// One EI device and its emulation state.
struct DeviceSlot {
    device: reis::event::Device,
    /// The device is resumed (allowed to emit). Devices arrive paused and may pause again.
    resumed: bool,
    /// We have issued `start_emulating` since the last resume.
    emulating: bool,
}

/// Tracks bound devices + the serial/sequence/timebase the EI protocol requires.
struct EiState {
    devices: Vec<DeviceSlot>,
    last_serial: u32,
    sequence: u32,
    start: Instant,
}

impl EiState {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            last_serial: 0,
            sequence: 0,
            start: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    /// Apply a server event: bind capabilities, track devices, and follow resume/pause.
    fn handle_ei(&mut self, ev: EiEvent, ctx: &ei::Context) {
        match ev {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
                let _ = ctx.flush();
            }
            EiEvent::DeviceAdded(e) => {
                tracing::info!(device = ?e.device.name(), ty = ?e.device.device_type(), "libei: device added");
                self.devices.push(DeviceSlot {
                    device: e.device,
                    resumed: false,
                    emulating: false,
                });
            }
            EiEvent::DeviceRemoved(e) => {
                self.devices.retain(|d| d.device != e.device);
            }
            EiEvent::DeviceResumed(e) => {
                self.last_serial = e.serial;
                if let Some(d) = self.devices.iter_mut().find(|d| d.device == e.device) {
                    d.resumed = true;
                    d.emulating = false; // must re-issue start_emulating after a resume
                }
            }
            EiEvent::DevicePaused(e) => {
                if let Some(d) = self.devices.iter_mut().find(|d| d.device == e.device) {
                    d.resumed = false;
                    d.emulating = false;
                }
            }
            // Informational: the server reports resulting modifier/group state; we don't set it.
            EiEvent::KeyboardModifiers(e) => self.last_serial = e.serial,
            _ => {}
        }
    }

    /// Index of a resumed device exposing `cap`.
    fn device_for(&self, cap: DeviceCapability) -> Option<usize> {
        self.devices
            .iter()
            .position(|d| d.resumed && d.device.has_capability(cap))
    }

    /// Ensure the device at `idx` is in `start_emulating` state before we emit on it.
    fn ensure_emulating(&mut self, idx: usize, dev: &ei::Device) {
        if !self.devices[idx].emulating {
            dev.start_emulating(self.last_serial, self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
            self.devices[idx].emulating = true;
        }
    }

    /// Translate and emit one client input event, committing it as a single `frame`.
    fn inject(&mut self, ev: &InputEvent, ctx: &ei::Context) {
        let cap = match ev.kind {
            InputKind::MouseMove => DeviceCapability::Pointer,
            InputKind::MouseMoveAbs => DeviceCapability::PointerAbsolute,
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => DeviceCapability::Button,
            InputKind::MouseScroll => DeviceCapability::Scroll,
            InputKind::KeyDown | InputKind::KeyUp => DeviceCapability::Keyboard,
            InputKind::GamepadButton | InputKind::GamepadAxis => return, // uinput path (later)
        };
        let Some(idx) = self.device_for(cap) else {
            return; // no resumed device with this capability yet
        };
        let dev = self.devices[idx].device.device().clone();
        self.ensure_emulating(idx, &dev);

        let mut emitted = true;
        let slot = &self.devices[idx].device;
        match ev.kind {
            InputKind::MouseMove => match slot.interface::<ei::Pointer>() {
                Some(p) => p.motion_relative(ev.x as f32, ev.y as f32),
                None => emitted = false,
            },
            InputKind::MouseMoveAbs => {
                let w = ((ev.flags >> 16) & 0xffff) as f32;
                let h = (ev.flags & 0xffff) as f32;
                match (
                    slot.interface::<ei::PointerAbsolute>(),
                    slot.regions().first(),
                ) {
                    (Some(p), Some(region)) if w > 0.0 && h > 0.0 => {
                        // Map the normalized client position into the device's first region.
                        let nx = (ev.x as f32 / w).clamp(0.0, 1.0);
                        let ny = (ev.y as f32 / h).clamp(0.0, 1.0);
                        let x = region.x as f32 + nx * region.width as f32;
                        let y = region.y as f32 + ny * region.height as f32;
                        p.motion_absolute(x, y);
                    }
                    _ => emitted = false,
                }
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                match (slot.interface::<ei::Button>(), gs_button_to_evdev(ev.code)) {
                    (Some(b), Some(btn)) => {
                        let st = if ev.kind == InputKind::MouseButtonDown {
                            ei::button::ButtonState::Press
                        } else {
                            ei::button::ButtonState::Released
                        };
                        b.button(btn, st);
                    }
                    _ => emitted = false,
                }
            }
            InputKind::MouseScroll => match slot.interface::<ei::Scroll>() {
                Some(s) => {
                    // GameStream sends WHEEL_DELTA(120)-scaled deltas in `x`; ei scroll_discrete
                    // uses the same 120-per-detent unit. Positive GameStream = up (vertical),
                    // which is negative on the ei axis, but = RIGHT (horizontal), which is
                    // already positive there (moonlight-qt/Sunshine pass horizontal through
                    // unnegated) — only the vertical axis flips.
                    if ev.code == SCROLL_HORIZONTAL {
                        s.scroll_discrete(ev.x, 0);
                    } else {
                        s.scroll_discrete(0, -ev.x);
                    }
                }
                None => emitted = false,
            },
            InputKind::KeyDown | InputKind::KeyUp => {
                match (slot.interface::<ei::Keyboard>(), vk_to_evdev(ev.code as u8)) {
                    (Some(k), Some(evdev)) => {
                        let st = if ev.kind == InputKind::KeyDown {
                            ei::keyboard::KeyState::Press
                        } else {
                            ei::keyboard::KeyState::Released
                        };
                        k.key(evdev as u32, st);
                    }
                    _ => {
                        emitted = false;
                        tracing::debug!(vk = ev.code, "libei: unmapped VK keycode — dropped");
                    }
                }
            }
            InputKind::GamepadButton | InputKind::GamepadAxis => emitted = false,
        }

        if emitted {
            dev.frame(self.last_serial, self.now_us());
        }
        let _ = ctx.flush();
    }
}
