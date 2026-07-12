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
use ashpd::zbus;
use futures_util::StreamExt;
use punktfunk_core::input::{InputEvent, InputKind};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// `code` value marking a horizontal scroll event (mirrors `gamestream::input`).
const SCROLL_HORIZONTAL: u32 = 1;

/// Where to find the EIS server.
#[derive(Clone, Debug)]
pub enum EiSource {
    /// `org.freedesktop.portal.RemoteDesktop` via `ashpd` (KWin — a pre-seeded grant avoids the
    /// approval dialog).
    Portal,
    /// Mutter's *direct* `org.gnome.Mutter.RemoteDesktop` EIS (GNOME). Unlike the xdg portal, this
    /// needs no interactive "Allow remote control?" approval — which a headless host can't answer,
    /// so the portal's `Start()` would just time out. Mirrors how the Mutter *video* backend uses
    /// the same direct API.
    MutterEis,
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
    let (_keepalive, context, mut events) = match tokio::time::timeout(
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
    // Watchdog: a healthy EIS server adds + resumes an input device within a beat of the handshake.
    // If none has resumed by this deadline, the connection is dead-on-arrival (stale/half-ready
    // gamescope socket the handshake passed but no real server is behind) — exit so the next
    // inject() fails and InjectorService reopens against a fresh socket, instead of silently
    // swallowing every event for the whole session.
    let resume_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(resume_deadline);
    let mut resumed_once = false;
    loop {
        tokio::select! {
            ei = events.next() => match ei {
                Some(Ok(ev)) => {
                    state.handle_ei(ev, &context);
                    if !resumed_once && state.devices.iter().any(|d| d.resumed) {
                        resumed_once = true;
                    }
                }
                Some(Err(e)) => { tracing::warn!(error = %e, "libei: event stream error"); break; }
                None => { tracing::info!("libei: EIS disconnected"); break; }
            },
            msg = rx.recv() => match msg {
                Some(input) => state.inject(&input, &context),
                None => { tracing::info!("libei: injector closed — ending session"); break; }
            },
            _ = &mut resume_deadline, if !resumed_once => {
                tracing::warn!(
                    "libei: no input device resumed within 5s of connecting — treating the EIS \
                     connection as dead and reopening (stale or half-ready compositor socket)"
                );
                break;
            }
        }
    }
    // A client that vanished mid-press must not leave keys/buttons latched in the
    // compositor — Mutter keeps the implicit grab of a destroyed device's button and the
    // focused app stops taking clicks until it is restarted. Release everything still
    // held before the EIS connection (and its devices) go away.
    state.release_all(&context);
}

/// Tie down the verbose tuple the connect step returns. The keep-alive must stay alive for the
/// whole session — dropping the portal/Mutter session closes the EIS connection; for the
/// direct-socket path it's `Box::new(())`.
type Connected = (
    Box<dyn Send>,
    ei::Context,
    reis::tokio::EiConvertEventStream,
);

/// Reach an EIS server per `source` and run the EI sender handshake.
async fn connect(source: EiSource) -> Result<Connected> {
    let (keepalive, stream): (Box<dyn Send>, UnixStream) = match source {
        EiSource::Portal => {
            let (rd, session, fd) = connect_portal().await?;
            (Box::new((rd, session)), UnixStream::from(fd))
        }
        EiSource::MutterEis => {
            let (keepalive, fd) = connect_mutter().await?;
            (keepalive, UnixStream::from(fd))
        }
        EiSource::SocketPathFile(file) => (Box::new(()), connect_socket_file(&file).await?),
    };
    let context = ei::Context::new(stream).map_err(|e| anyhow!("reis EI context: {e}"))?;
    // Bound the handshake. `UnixStream::connect` to a socket *file* succeeds the moment the path
    // exists, but a stale/half-ready gamescope (its socket created early in startup, or left behind
    // by a SIGKILLed prior session) may never drive the EI handshake — which would otherwise hang
    // this worker forever. A bounded handshake lets the worker error out so InjectorService reopens.
    let (_conn, events) = tokio::time::timeout(
        Duration::from_secs(8),
        context.handshake_tokio("punktfunk-host", ei::handshake::ContextType::Sender),
    )
    .await
    .map_err(|_| {
        anyhow!("EI handshake timed out (EIS server not responding — stale/half-ready socket?)")
    })?
    .map_err(|e| anyhow!("EI handshake: {e}"))?;
    Ok((keepalive, context, events))
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
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer | DeviceType::Touchscreen)
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

/// GNOME path: get the EIS socket fd from Mutter's *direct* `org.gnome.Mutter.RemoteDesktop` API
/// (`CreateSession` → `Start` → `ConnectToEIS`). No xdg portal is involved, so there is no
/// interactive "Allow remote control?" approval to satisfy — exactly why [`connect_portal`] times
/// out on a headless GNOME host. (Same direct API the Mutter *video* backend uses.) The returned
/// keep-alive owns the D-Bus connection + session; dropping it tears the Mutter session down and
/// closes the EIS connection (Mutter sessions die with their D-Bus connection).
async fn connect_mutter() -> Result<(Box<dyn Send>, std::os::fd::OwnedFd)> {
    use zbus::zvariant::{OwnedObjectPath, Value};
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| anyhow!("connect session D-Bus (Mutter RemoteDesktop): {e}"))?;
    let rd = zbus::Proxy::new(
        &conn,
        "org.gnome.Mutter.RemoteDesktop",
        "/org/gnome/Mutter/RemoteDesktop",
        "org.gnome.Mutter.RemoteDesktop",
    )
    .await
    .map_err(|e| anyhow!("Mutter RemoteDesktop proxy (is gnome-shell running?): {e}"))?;
    let session_path: OwnedObjectPath = rd
        .call("CreateSession", &())
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.CreateSession: {e}"))?;
    let session = zbus::Proxy::new(
        &conn,
        "org.gnome.Mutter.RemoteDesktop",
        session_path,
        "org.gnome.Mutter.RemoteDesktop.Session",
    )
    .await
    .map_err(|e| anyhow!("Mutter RemoteDesktop.Session proxy: {e}"))?;
    session
        .call_method("Start", &())
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.Session.Start: {e}"))?;
    let options: HashMap<&str, Value> = HashMap::new();
    let fd: zbus::zvariant::OwnedFd = session
        .call("ConnectToEIS", &(options,))
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.Session.ConnectToEIS: {e}"))?;
    tracing::info!("libei: connected to Mutter's direct RemoteDesktop EIS (no portal approval)");
    Ok((Box::new((conn, session)), std::os::fd::OwnedFd::from(fd)))
}

/// Poll `file` for the EIS socket path (the gamescope backend relays `LIBEI_SOCKET` there once
/// the nested app launches), then connect. A bare name is resolved against `XDG_RUNTIME_DIR`,
/// mirroring libei's own `LIBEI_SOCKET` semantics.
async fn connect_socket_file(file: &std::path::Path) -> Result<UnixStream> {
    // The relay file is rewritten each session with the CURRENT gamescope's `LIBEI_SOCKET`, and the
    // socket may not be `listen()`ing the instant its name appears — or the file may briefly still
    // hold a prior, now-dead session's name (the host-lifetime injector reconnecting between
    // sessions). So poll: RE-READ the file and RETRY the connect, treating "refused"/"missing" as
    // not-ready-yet (the exact "Connection refused" we saw when a stale socket lingered). Bounded so
    // a genuinely wedged setup still surfaces an error.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut logged = String::new();
    loop {
        // Defense-in-depth: never follow a symlinked relay file. It lives under `$XDG_RUNTIME_DIR`
        // (per-user 0700) so a cross-user plant is already blocked, but refuse a symlink outright
        // rather than read through one to an attacker-chosen target (a rogue EIS server would
        // keylog/deny the session's input; security-review 2026-06-28 #6).
        if std::fs::symlink_metadata(file)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "EIS relay file {} is a symlink — refusing to follow it",
                file.display()
            ));
        }
        if let Ok(s) = std::fs::read_to_string(file) {
            let name = s.trim();
            if !name.is_empty() {
                let full = if name.starts_with('/') {
                    std::path::PathBuf::from(name)
                } else {
                    let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
                        anyhow!("XDG_RUNTIME_DIR unset (needed to resolve EIS socket '{name}')")
                    })?;
                    std::path::Path::new(&runtime).join(name)
                };
                if logged != name {
                    tracing::info!(socket = %full.display(), "libei: connecting to EIS socket");
                    logged = name.to_string();
                }
                match UnixStream::connect(&full) {
                    Ok(stream) => return Ok(stream),
                    // Refused = socket file exists but no listener yet (or a dead session);
                    // NotFound = path not created yet. Both heal once the live gamescope's EIS is
                    // up — retry. Anything else (e.g. permission) is a real failure.
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(e) => return Err(anyhow!("connect EIS socket {}: {e}", full.display())),
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "EIS socket from {} never became connectable (gamescope not up, or its EIS crashed)",
                file.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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
    /// Total inject() calls — used only to throttle diagnostic logging.
    injected: u64,
    /// Bitmask of [`InputKind`]s already logged once (diagnostics: surface the FIRST of each
    /// kind a client sends + whether it emitted, so an unexpected client — e.g. a touch-only
    /// tablet hitting a compositor without ei_touchscreen — is immediately diagnosable).
    seen_kinds: u32,
    /// Wire codes currently held down (keys = VK, buttons = GameStream ids, touches = ids)
    /// — synthesized back up at session end ([`EiState::release_all`]). A client that
    /// vanishes mid-press must not leave the compositor with a latched key or an implicit
    /// pointer grab: observed on Mutter, a button held by a destroyed EIS device wedges
    /// click delivery to the focused app until that app is restarted.
    held_keys: Vec<u32>,
    held_buttons: Vec<u32>,
    held_touches: Vec<u32>,
}

/// Stable small index per [`InputKind`] for the `seen_kinds` bitmask.
fn kind_bit(kind: InputKind) -> u32 {
    let i = match kind {
        InputKind::MouseMove => 0,
        InputKind::MouseMoveAbs => 1,
        InputKind::MouseButtonDown => 2,
        InputKind::MouseButtonUp => 3,
        InputKind::MouseScroll => 4,
        InputKind::KeyDown => 5,
        InputKind::KeyUp => 6,
        InputKind::TouchDown => 7,
        InputKind::TouchMove => 8,
        InputKind::TouchUp => 9,
        InputKind::GamepadButton => 10,
        InputKind::GamepadAxis => 11,
        InputKind::GamepadState => 12,
        InputKind::GamepadRemove => 13,
        InputKind::GamepadArrival => 14,
    };
    1 << i
}

impl EiState {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            last_serial: 0,
            sequence: 0,
            start: Instant::now(),
            injected: 0,
            seen_kinds: 0,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            held_touches: Vec::new(),
        }
    }

    /// Release everything the remote client still holds — called when the session ends
    /// (client gone, EIS closing). Synthesizes wire-level release events through the
    /// normal [`EiState::inject`] path so the compositor sees proper key-up / button-up /
    /// touch-up frames before the devices disappear.
    fn release_all(&mut self, ctx: &ei::Context) {
        let (keys, buttons, touches) = (
            std::mem::take(&mut self.held_keys),
            std::mem::take(&mut self.held_buttons),
            std::mem::take(&mut self.held_touches),
        );
        if keys.is_empty() && buttons.is_empty() && touches.is_empty() {
            return;
        }
        tracing::info!(
            keys = keys.len(),
            buttons = buttons.len(),
            touches = touches.len(),
            "libei: releasing input still held at session end"
        );
        let release = |kind: InputKind, code: u32| InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        };
        for code in buttons {
            self.inject(&release(InputKind::MouseButtonUp, code), ctx);
        }
        for code in keys {
            self.inject(&release(InputKind::KeyUp, code), ctx);
        }
        for id in touches {
            self.inject(&release(InputKind::TouchUp, id), ctx);
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
                        | DeviceCapability::Button
                        | DeviceCapability::Touch,
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
                let dev = &e.device;
                tracing::info!(
                    name = ?dev.name(),
                    pointer = dev.has_capability(DeviceCapability::Pointer),
                    pointer_abs = dev.has_capability(DeviceCapability::PointerAbsolute),
                    keyboard = dev.has_capability(DeviceCapability::Keyboard),
                    button = dev.has_capability(DeviceCapability::Button),
                    scroll = dev.has_capability(DeviceCapability::Scroll),
                    "libei: device RESUMED (now emittable)"
                );
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
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {
                DeviceCapability::Touch
            }
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => return, // uinput path (later)
        };
        self.injected += 1;
        let n = self.injected;
        // Log the first of each kind always (diagnostics), then occasionally.
        let bit = kind_bit(ev.kind);
        let first = self.seen_kinds & bit == 0;
        self.seen_kinds |= bit;
        let loud = first || n <= 5 || n % 600 == 0;
        let Some(idx) = self.device_for(cap) else {
            if loud {
                tracing::warn!(
                    n,
                    kind = ?ev.kind,
                    ?cap,
                    devices = self.devices.len(),
                    resumed = self.devices.iter().filter(|d| d.resumed).count(),
                    "libei: DROP — no resumed device exposes this capability"
                );
            }
            // No resumed device with this capability yet. For touch this is usually permanent on
            // this compositor — the RemoteDesktop portal may grant the Touchscreen *device type*
            // while the EIS server never creates a touchscreen *device* (observed on headless
            // KWin). Surface it once so touch silently going nowhere is diagnosable.
            if matches!(
                ev.kind,
                InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp
            ) {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        "touch received but the compositor's EIS exposed no touchscreen device — \
                         touch is dropped (KWin's libei may not implement ei_touchscreen yet; \
                         gamescope / a newer compositor may)"
                    );
                }
            }
            return;
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
                    // Wire deltas are WHEEL_DELTA(120)-scaled in `x`. Emit BOTH ei scroll axes
                    // from it: `scroll_discrete` (120-per-detent — drives line/page scrolling)
                    // AND the continuous `scroll` axis in logical px (≈15 px/detent). Without
                    // the continuous axis Mutter floors a sub-detent delta (trackpad / precise
                    // wheel / fractional smooth scroll) to zero whole clicks, so small scrolls
                    // never register and you have to spin the wheel a lot — emitting the pixel
                    // axis too makes every delta move proportionally (matches the wlr backend's
                    // 15 px/notch). Positive wire = up (vertical, negated on the ei axis) /
                    // RIGHT (horizontal, already positive — moonlight-qt/Sunshine pass it
                    // through unnegated); only the vertical axis flips.
                    const PX_PER_DETENT: f32 = 15.0;
                    let px = ev.x as f32 / 120.0 * PX_PER_DETENT;
                    if ev.code == SCROLL_HORIZONTAL {
                        s.scroll_discrete(ev.x, 0);
                        s.scroll(px, 0.0);
                    } else {
                        s.scroll_discrete(0, -ev.x);
                        s.scroll(0.0, -px);
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
            // Touch: `code` is the touch id, `x`/`y` are client pixels and `flags` packs the
            // client surface w/h — mapped into the device's region exactly like MouseMoveAbs.
            // One InputEvent = one frame, which satisfies the ei_touchscreen rule that a down /
            // motion / up must not share a frame.
            InputKind::TouchDown | InputKind::TouchMove => {
                let w = ((ev.flags >> 16) & 0xffff) as f32;
                let h = (ev.flags & 0xffff) as f32;
                match (slot.interface::<ei::Touchscreen>(), slot.regions().first()) {
                    (Some(t), Some(region)) if w > 0.0 && h > 0.0 => {
                        let nx = (ev.x as f32 / w).clamp(0.0, 1.0);
                        let ny = (ev.y as f32 / h).clamp(0.0, 1.0);
                        let x = region.x as f32 + nx * region.width as f32;
                        let y = region.y as f32 + ny * region.height as f32;
                        if ev.kind == InputKind::TouchDown {
                            t.down(ev.code, x, y);
                        } else {
                            t.motion(ev.code, x, y);
                        }
                    }
                    _ => emitted = false,
                }
            }
            InputKind::TouchUp => match slot.interface::<ei::Touchscreen>() {
                Some(t) => t.up(ev.code),
                None => emitted = false,
            },
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => emitted = false,
        }

        if emitted {
            // Track held state on the wire codes so `release_all` can undo it at
            // session end (vanished clients must not leave anything latched).
            match ev.kind {
                InputKind::KeyDown if !self.held_keys.contains(&ev.code) => {
                    self.held_keys.push(ev.code);
                }
                InputKind::KeyUp => self.held_keys.retain(|&c| c != ev.code),
                InputKind::MouseButtonDown if !self.held_buttons.contains(&ev.code) => {
                    self.held_buttons.push(ev.code);
                }
                InputKind::MouseButtonUp => self.held_buttons.retain(|&c| c != ev.code),
                InputKind::TouchDown if !self.held_touches.contains(&ev.code) => {
                    self.held_touches.push(ev.code);
                }
                InputKind::TouchUp => self.held_touches.retain(|&c| c != ev.code),
                _ => {}
            }
            dev.frame(self.last_serial, self.now_us());
        }
        if let Err(e) = ctx.flush() {
            tracing::warn!(error = %e, "libei: ctx.flush failed");
        }
        if loud {
            tracing::info!(n, kind = ?ev.kind, idx, emitted, "libei: emitted");
        }
    }
}
