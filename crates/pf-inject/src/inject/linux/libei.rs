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
use crate::AbsoluteAnchor;
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
    let (_keepalive, context, mut events, output_hint) = match tokio::time::timeout(
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
    state.output_hint = output_hint;
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
    // The compositor's output size ("WxH" relay-file hint) — the scale target for absolute
    // coordinates when the EIS advertises only a degenerate region (gamescope). `None` for
    // the portal/Mutter paths, whose regions are real.
    Option<(u32, u32)>,
);

/// Reach an EIS server per `source` and run the EI sender handshake.
async fn connect(source: EiSource) -> Result<Connected> {
    let (keepalive, stream, output_hint): (Box<dyn Send>, UnixStream, Option<(u32, u32)>) =
        match source {
            EiSource::Portal => {
                let (rd, session, fd) = connect_portal().await?;
                (Box::new((rd, session)), UnixStream::from(fd), None)
            }
            EiSource::MutterEis => {
                let (keepalive, fd) = connect_mutter().await?;
                (keepalive, UnixStream::from(fd), None)
            }
            EiSource::SocketPathFile(file) => {
                let (stream, hint) = connect_socket_file(&file).await?;
                (Box::new(()), stream, hint)
            }
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
    Ok((keepalive, context, events, output_hint))
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
/// mirroring libei's own `LIBEI_SOCKET` semantics. Line 2 of the relay file, when present,
/// carries the compositor's output size as `WxH` — returned as the absolute-coordinate scale
/// hint (gamescope's EIS region is degenerate, so the geometry can't come from the protocol).
async fn connect_socket_file(file: &std::path::Path) -> Result<(UnixStream, Option<(u32, u32)>)> {
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
            let mut file_lines = s.lines();
            let name = file_lines.next().unwrap_or("").trim();
            let hint = file_lines.next().and_then(|l| {
                let (w, h) = l.trim().split_once('x')?;
                Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
            });
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
                    Ok(stream) => return Ok((stream, hint)),
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
/// Pick the region to map absolute coordinates into. The device advertises one region per logical
/// monitor and blindly taking `first()` next to a physical monitor put the pointer — and every
/// click — on whichever output the compositor happened to announce first (on-glass: GNOME with a
/// dummy HDMI beside the virtual primary; the seat cursor never entered the streamed monitor, so
/// neither embedded nor metadata cursor capture could see it).
///
/// The ladder, most identifying first:
///
/// 1. **`mapping_id`** from the session's [`AbsoluteAnchor`] — the protocol's own key for
///    correlating a region with a video stream.
/// 2. **origin** from the anchor — two outputs can share a size, never a top-left. This is what
///    makes a *mirrored physical monitor* land correctly (`design/per-monitor-portal-capture.md`
///    §7.2): its region is not the client's size, so the size rung can't find it.
/// 3. **size** — the streamed mode. Correct for a client-sized virtual output, ambiguous the
///    moment two heads share a mode, which is exactly why the rungs above exist.
/// 4. `first()`.
///
/// An anchor that matches nothing falls through rather than failing: the region set is the truth
/// and the anchor is our belief about it. The caller logs that miss ([`anchor_missed`]).
fn region_for_mode<'a>(
    regions: &'a [reis::event::Region],
    w: f32,
    h: f32,
    anchor: Option<&AbsoluteAnchor>,
) -> Option<&'a reis::event::Region> {
    if let Some(a) = anchor {
        if let Some(id) = a.mapping_id.as_deref() {
            if let Some(r) = regions.iter().find(|r| r.mapping_id.as_deref() == Some(id)) {
                return Some(r);
            }
        }
        if let Some((x, y)) = a.origin {
            // EI region offsets are unsigned; a compositor places every output at a non-negative
            // origin in its own global space, so a negative anchor simply matches nothing.
            if x >= 0 && y >= 0 {
                if let Some(r) = regions.iter().find(|r| r.x == x as u32 && r.y == y as u32) {
                    return Some(r);
                }
            }
        }
    }
    regions
        .iter()
        .find(|r| r.width as f32 == w && r.height as f32 == h)
        .or_else(|| regions.first())
}

/// Report which region absolute coordinates actually landed in, once per distinct answer.
///
/// The ladder above is only *observable* through where the pointer ends up, which on a two-head box
/// is precisely the thing that is hard to see and easy to get wrong — the anchor exists because it
/// already resolved wrong on-glass once, silently. `warn_anchor_miss` covers the anchor naming
/// nothing; this covers the other half, an anchor that matched *something*, by saying which. Once
/// per distinct region so a live session logs one line, not one per motion event.
fn note_abs_region(region: &reis::event::Region, anchor: Option<&AbsoluteAnchor>) {
    static LAST: std::sync::Mutex<Option<(u32, u32, u32, u32)>> = std::sync::Mutex::new(None);
    let key = (region.x, region.y, region.width, region.height);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == Some(key) {
        return;
    }
    *last = Some(key);
    tracing::info!(
        region = %format!("{}x{}+{}+{}", region.width, region.height, region.x, region.y),
        mapping_id = ?region.mapping_id,
        anchor_origin = ?anchor.and_then(|a| a.origin),
        anchor_mapping_id = ?anchor.and_then(|a| a.mapping_id.clone()),
        "libei: absolute input maps into this output"
    );
}

/// Did an anchor name an output this region set doesn't have? Drives the one-shot warning — a
/// silently mis-mapped pointer is the failure this whole ladder exists to prevent, so when the
/// anchor can't be honored that has to be visible in the log rather than inferred from "clicks land
/// on the wrong screen".
fn anchor_missed(regions: &[reis::event::Region], anchor: Option<&AbsoluteAnchor>) -> bool {
    let Some(a) = anchor else {
        return false;
    };
    if let Some(id) = a.mapping_id.as_deref() {
        if regions.iter().any(|r| r.mapping_id.as_deref() == Some(id)) {
            return false;
        }
    }
    if let Some((x, y)) = a.origin {
        if x >= 0 && y >= 0 && regions.iter().any(|r| r.x == x as u32 && r.y == y as u32) {
            return false;
        }
    }
    true
}

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
    /// The touch id currently degraded to the absolute pointer ([`EiState::degrade_touch`]) —
    /// the "primary finger" while the EIS has no touchscreen device. `None` between touches.
    degraded_touch: Option<u32>,
    /// The compositor's output size (relay-file "WxH" hint) — scale target for absolute
    /// coordinates when the device's region is degenerate/absent (gamescope). Without it the
    /// fallback is raw client pixels, correct only when the stream runs at the output's size.
    output_hint: Option<(u32, u32)>,
}

/// The anchor whose miss we last warned about, so a persistently unmatchable anchor logs once per
/// *change* instead of once per pointer sample (absolute motion arrives at client frame rate).
static LAST_WARNED_ANCHOR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Warn — once per distinct anchor — that the session's anchor names an output this EIS doesn't
/// advertise, so absolute coordinates fell back to size matching. See [`region_for_mode`].
fn warn_anchor_miss(anchor: &AbsoluteAnchor, regions: &[reis::event::Region]) {
    let key = format!("{anchor:?}");
    let mut last = LAST_WARNED_ANCHOR.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(key.as_str()) {
        return;
    }
    *last = Some(key);
    tracing::warn!(
        ?anchor,
        regions = ?regions
            .iter()
            .map(|r| (r.x, r.y, r.width, r.height, r.mapping_id.clone()))
            .collect::<Vec<_>>(),
        "libei: the session's absolute-coordinate anchor matches no EIS region — falling back to \
         size matching, so the pointer may land on the wrong monitor"
    );
}

/// Is this EIS region a plausible OUTPUT geometry — something to map normalized coordinates
/// into? gamescope advertises a degenerate `(0,0,INT32_MAX,INT32_MAX)` "everything" region on
/// its virtual input device, meaning "absolute coordinates are raw"; normalizing into it
/// explodes a center tap to x≈1e9, which the compositor clamps to the far corner. 16384
/// comfortably covers real multi-monitor layouts while rejecting the sentinel.
fn sane_region(r: &reis::event::Region) -> bool {
    r.width > 0 && r.height > 0 && r.width <= 16_384 && r.height <= 16_384
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
        InputKind::TextInput => 15,
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
            degraded_touch: None,
            output_hint: None,
        }
    }

    /// Release everything the remote client still holds — called when the session ends
    /// (client gone, EIS closing). Synthesizes wire-level release events through the
    /// normal [`EiState::inject`] path so the compositor sees proper key-up / button-up /
    /// touch-up frames before the devices disappear.
    fn release_all(&mut self, ctx: &ei::Context) {
        // A degraded touch is held as a synthesized left button (in `held_buttons`), so the
        // loop below releases it — but the primary-finger latch must clear too, or the NEXT
        // session's first TouchDown reads as a second finger and is ignored.
        self.degraded_touch = None;
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
                    // One region per logical monitor — which one absolute coords map into is
                    // resolved per event (`region_for_mode`); log them so a mis-mapped pointer
                    // (cursor/clicks on the wrong output) is diagnosable from the journal.
                    regions = ?dev
                        .regions()
                        .iter()
                        .map(|r| format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y))
                        .collect::<Vec<_>>(),
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

    /// Degrade touch to a single-finger ABSOLUTE POINTER on compositors whose EIS never
    /// creates a touchscreen device (gamescope's "Gamescope Virtual Input" advertises
    /// pointer/pointer_abs/button but no touch — observed live; headless KWin the same).
    /// Down = abs-move + left press, move = abs-move, up = left release — synthesized through
    /// the normal [`EiState::inject`] Mouse* paths so region mapping, held-state tracking and
    /// [`EiState::release_all`] all apply. Only the FIRST finger drives the pointer; later
    /// fingers are ignored (a pointer has no second contact — a pinch degrades to a drag).
    fn degrade_touch(&mut self, ev: &InputEvent, ctx: &ei::Context) {
        const GS_BUTTON_LEFT: u32 = 1;
        match ev.kind {
            InputKind::TouchDown => {
                if self.degraded_touch.is_some() {
                    return; // secondary finger — single-pointer degradation
                }
                self.degraded_touch = Some(ev.code);
                static NOTED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(
                        "compositor's EIS has no touchscreen device — degrading touch to a \
                         single-finger absolute pointer (tap = left click; multi-touch \
                         gestures unavailable)"
                    );
                }
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseMoveAbs,
                        ..*ev
                    },
                    ctx,
                );
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseButtonDown,
                        code: GS_BUTTON_LEFT,
                        ..*ev
                    },
                    ctx,
                );
            }
            InputKind::TouchMove if self.degraded_touch == Some(ev.code) => {
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseMoveAbs,
                        ..*ev
                    },
                    ctx,
                );
            }
            InputKind::TouchUp if self.degraded_touch == Some(ev.code) => {
                self.degraded_touch = None;
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseButtonUp,
                        code: GS_BUTTON_LEFT,
                        ..*ev
                    },
                    ctx,
                );
            }
            _ => {}
        }
    }

    /// Translate and emit one client input event, committing it as a single `frame`.
    fn inject(&mut self, ev: &InputEvent, ctx: &ei::Context) {
        // No ei_touchscreen device but an absolute pointer exists → degrade rather than drop
        // (the game-mode "touch simply does not work" trap). Checked per event, not latched:
        // a touchscreen device appearing later (compositor restart, capability change) takes
        // over seamlessly on the next touch.
        if matches!(
            ev.kind,
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp
        ) && self.device_for(DeviceCapability::Touch).is_none()
            && self.device_for(DeviceCapability::PointerAbsolute).is_some()
        {
            self.degrade_touch(ev, ctx);
            return;
        }
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
            // libei presses keycodes against the server's negotiated keymap — no committed-text
            // path (the HOST_CAP_TEXT_INPUT cap is not advertised on this backend).
            InputKind::TextInput => return,
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
                    "libei: dropped event — no resumed device exposes this capability"
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
                match slot.interface::<ei::PointerAbsolute>() {
                    Some(p) if w > 0.0 && h > 0.0 => {
                        // Map the normalized client position into the region matching the streamed
                        // output's mode (`region_for_mode` picks the right one on a multi-monitor
                        // EIS). gamescope's "Gamescope Virtual Input" advertises a degenerate
                        // (0,0,INT32_MAX,INT32_MAX) region meaning "coordinates are raw" —
                        // `sane_region` rejects it (normalizing into it explodes a center tap to
                        // x≈1e9, clamped to the far corner), so a non-matching / insane region
                        // falls to the output hint (correct across a resolution mismatch), then
                        // raw client pixels as the last resort.
                        let nx = (ev.x as f32 / w).clamp(0.0, 1.0);
                        let ny = (ev.y as f32 / h).clamp(0.0, 1.0);
                        let anchor = crate::absolute_anchor();
                        if let Some(a) = anchor
                            .as_ref()
                            .filter(|a| anchor_missed(slot.regions(), Some(a)))
                        {
                            warn_anchor_miss(a, slot.regions());
                        }
                        let (x, y) = match region_for_mode(slot.regions(), w, h, anchor.as_ref())
                            .filter(|r| sane_region(r))
                        {
                            Some(region) => {
                                note_abs_region(region, anchor.as_ref());
                                (
                                    region.x as f32 + nx * region.width as f32,
                                    region.y as f32 + ny * region.height as f32,
                                )
                            }
                            // Degenerate/absent region: scale into the relay-file output hint
                            // (correct even when the client streams at a different resolution
                            // than the session runs); raw client pixels as the last resort.
                            None => match self.output_hint {
                                Some((ow, oh)) => (nx * ow as f32, ny * oh as f32),
                                None => (ev.x as f32, ev.y as f32),
                            },
                        };
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
                match slot.interface::<ei::Touchscreen>() {
                    Some(t) if w > 0.0 && h > 0.0 => {
                        let nx = (ev.x as f32 / w).clamp(0.0, 1.0);
                        let ny = (ev.y as f32 / h).clamp(0.0, 1.0);
                        // Same region-selection + degenerate fallback ladder as MouseMoveAbs
                        // (including the session anchor — touch must land on the same monitor the
                        // pointer does, or a mirrored session's taps go to a different screen).
                        let anchor = crate::absolute_anchor();
                        let (x, y) = match region_for_mode(slot.regions(), w, h, anchor.as_ref())
                            .filter(|r| sane_region(r))
                        {
                            Some(region) => {
                                note_abs_region(region, anchor.as_ref());
                                (
                                    region.x as f32 + nx * region.width as f32,
                                    region.y as f32 + ny * region.height as f32,
                                )
                            }
                            None => match self.output_hint {
                                Some((ow, oh)) => (nx * ow as f32, ny * oh as f32),
                                None => (ev.x as f32, ev.y as f32),
                            },
                        };
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
            | InputKind::GamepadArrival
            | InputKind::TextInput => emitted = false,
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
            // In the per-input-event hot path: a dead EIS socket fails flush on every event
            // (mouse-move = 100s/s), so gate the warn behind the same `loud` sampler as its siblings.
            if loud {
                tracing::warn!(error = %e, "libei: ctx.flush failed");
            }
        }
        if loud {
            tracing::debug!(n, kind = ?ev.kind, idx, emitted, "libei: emitted");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: u32, y: u32, w: u32, h: u32, mapping_id: Option<&str>) -> reis::event::Region {
        reis::event::Region {
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            mapping_id: mapping_id.map(str::to_string),
        }
    }

    /// The case the anchor exists for: two heads at the SAME size, so the size rung is a coin
    /// flip. The origin picks the right one.
    #[test]
    fn the_origin_disambiguates_two_same_size_monitors() {
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 1920, 1080, None),
        ];
        let anchor = AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        };
        let picked = region_for_mode(&regions, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (1920, 0));
        // Without the anchor the same call takes the first same-sized region — the old behavior,
        // preserved deliberately for the client-sized virtual-output path.
        let picked = region_for_mode(&regions, 1920.0, 1080.0, None).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0));
    }

    /// `mapping_id` outranks the origin: it is the protocol's own stream↔region correlation, so a
    /// stale/rounded origin can't override it.
    #[test]
    fn mapping_id_outranks_the_origin() {
        let regions = [
            region(0, 0, 1920, 1080, Some("head-a")),
            region(1920, 0, 1920, 1080, Some("head-b")),
        ];
        let anchor = AbsoluteAnchor {
            origin: Some((0, 0)),
            mapping_id: Some("head-b".into()),
        };
        let picked = region_for_mode(&regions, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!(picked.mapping_id.as_deref(), Some("head-b"));
    }

    /// A mirrored monitor's region is NOT the client's streamed size, so the size rung would miss
    /// it entirely — the origin is what makes this land.
    #[test]
    fn the_anchor_finds_a_monitor_the_streamed_size_does_not_match() {
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 3840, 2160, None),
        ];
        // Client streams 1280x720 of a 4K head parked to the right.
        let anchor = AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        };
        let picked = region_for_mode(&regions, 1280.0, 720.0, Some(&anchor)).unwrap();
        assert_eq!((picked.width, picked.height), (3840, 2160));
    }

    /// An anchor that names nothing present must not strand input: fall back down the ladder
    /// (size, then first) — and say so, which `anchor_missed` drives.
    #[test]
    fn an_unmatched_anchor_falls_back_and_is_reported() {
        let regions = [region(0, 0, 1920, 1080, None)];
        let anchor = AbsoluteAnchor {
            origin: Some((5000, 5000)),
            mapping_id: None,
        };
        assert!(anchor_missed(&regions, Some(&anchor)));
        let picked = region_for_mode(&regions, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0), "fell back to the size match");
        // A matched anchor is never reported as missed.
        let ok = AbsoluteAnchor {
            origin: Some((0, 0)),
            mapping_id: None,
        };
        assert!(!anchor_missed(&regions, Some(&ok)));
        assert!(!anchor_missed(&regions, None), "no anchor is not a miss");
    }

    /// EI region offsets are unsigned; a negative anchor origin can only ever match nothing, and
    /// must not be cast into a huge u32 that accidentally matches something.
    #[test]
    fn a_negative_origin_matches_nothing_rather_than_wrapping() {
        let regions = [region(0, 0, 1920, 1080, None)];
        let anchor = AbsoluteAnchor {
            origin: Some((-1920, 0)),
            mapping_id: None,
        };
        assert!(anchor_missed(&regions, Some(&anchor)));
        let picked = region_for_mode(&regions, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0));
    }

    /// An empty anchor is the same as no anchor — so a caller can build one unconditionally and
    /// let the setter drop it.
    #[test]
    fn an_empty_anchor_is_dropped_by_the_setter() {
        crate::set_absolute_anchor(Some(AbsoluteAnchor::default()));
        assert_eq!(crate::absolute_anchor(), None);
        crate::set_absolute_anchor(Some(AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        }));
        assert_eq!(crate::absolute_anchor().unwrap().origin, Some((1920, 0)));
        crate::set_absolute_anchor(None);
        assert_eq!(crate::absolute_anchor(), None);
    }
}
