//! The GameStream control stream: an ENet host on UDP 47999. Moonlight connects this
//! BEFORE the video stream starts (`STAGE_CONTROL_STREAM_START` precedes
//! `STAGE_VIDEO_STREAM_START`), so it must be up or the whole connection aborts. It carries
//! input (mouse/keyboard/gamepad), keepalives, and QoS feedback.
//!
//! Sunshine-mode hosts (we advertise `state=SUNSHINE_SERVER_FREE`) make Moonlight encrypt the
//! control stream with AES-128-GCM under the `/launch` `rikey`, even though we negotiate no
//! media encryption. Wire framing (all little-endian):
//!
//! ```text
//!   u16 encType = 0x0001 | u16 length | u32 seq | [16-byte GCM tag] | ciphertext
//!   length = sizeof(seq) + 16 (tag) + plaintext
//! ```
//!
//! The GCM nonce depends on what Moonlight negotiated (`encryptControlMessage` in
//! moonlight-common-c). For `SS_ENC_CONTROL_V2` it is a 12-byte nonce with `seq` (LE) in bytes
//! [0..4] and `b"CC"` (client→host) at [10..12]. For the legacy path — which we hit, since we
//! advertise no encryption — it is a 16-byte nonce with only `iv[0] = seq & 0xff` and the rest
//! zero. The tag is prepended to the ciphertext; there is no AAD; the key is the forward
//! `hex::decode(rikey)`. We auto-detect the exact scheme via [`decrypt_control`] on the first
//! packet that authenticates, since GCM gives no partial credit.
//!
//! Runs on its own native thread — but only while at least one client is paired. `rusty_enet`
//! is a c2rust-style transpile of C ENet (raw-pointer arithmetic, manual allocation), and its
//! fragment reassembly / peer state machine run BEFORE the AES-GCM decrypt below — the host's
//! only pre-authentication unsafe surface (rust-safety WP0). Pairing itself never touches
//! 47999 (the PIN ceremony is HTTPS on nvhttp), so [`sync`] keeps the port closed until the
//! first pairing lands and tears it down when the last one is removed.

use super::{AppState, LaunchSession, CONTROL_PORT};
use crate::inject::gamepad::GamepadManager;
use anyhow::{anyhow, Context, Result};
use punktfunk_core::input::{GamepadEvent, InputEvent};
use punktfunk_core::quic::{classify, GrantClass, HdrMeta, GRANT_ALL};
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lifecycle gate for the control port (rust-safety WP0): binds 47999 only while the
/// paired-client list is non-empty, so a never-paired host exposes no ENet at all.
pub(crate) struct Gate {
    /// Set once by `serve` when the GameStream planes are enabled (`--gamestream`). Without it
    /// [`sync`] is a no-op — the management API's unpair endpoint also runs on native-only
    /// hosts, and those must never bind a GameStream port.
    enabled: AtomicBool,
    /// The live listener; `None` = port closed. The mutex serializes concurrent reconciles
    /// (two pairings in quick succession must not double-bind), and the bind/teardown
    /// decision re-reads the paired list inside it so a pair racing an unpair cannot leave
    /// the port in the wrong state.
    running: Mutex<Option<Running>>,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate {
            enabled: AtomicBool::new(false),
            running: Mutex::new(None),
        }
    }

    /// Arm the gate — [`sync`] stays a no-op until this is called (from `serve`'s
    /// GameStream branch, the single existing source of truth for "compat planes on").
    pub(crate) fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }
}

/// A bound control port being serviced: the stop signal plus the thread observing it.
struct Running {
    /// Tells the service thread to say goodbye to a connected peer and exit (closing the
    /// socket with it).
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// The live session's per-client access (design/per-client-access.md §8, WP13), resolved from
/// the launch owner's cert fingerprint against the shared grants registry
/// ([`AppState::access`]). The control thread owns it single-threadedly, so a plain `u32`
/// stands where the native plane's `Arc<AtomicU32>` does — the idiom is otherwise WP4's:
/// resolve at session start, fold console edits in via the watch (within one 2 ms tick), one
/// mask test per event, and the wall-clock deadline cuts the session.
struct SessionAccess {
    /// The launch owner's fingerprint (lowercase hex) this state was resolved for — a
    /// different owner (steal/new session) re-resolves from scratch.
    fp_hex: String,
    /// Live edits from the console arrive here (`NativePairing::subscribe`); polled per tick.
    /// `None` when no registry is wired (tests) — then the mask stays ungoverned-full forever.
    rx: Option<tokio::sync::watch::Receiver<crate::native_pairing::AccessState>>,
    /// The effective grant mask input is filtered against.
    mask: u32,
    /// Absolute expiry, host wall clock, unix seconds; `None` = permanent. Checked each tick.
    deadline: Option<i64>,
}

impl SessionAccess {
    /// Resolve a session owner's access: subscribe FIRST, then fold the channel's current
    /// value, so a console edit racing this resolution lands either in the borrow or as the
    /// first change notification — never in a gap between the two (the WP3 admission order).
    fn resolve(
        registry: Option<&Arc<crate::native_pairing::NativePairing>>,
        fp_hex: String,
    ) -> SessionAccess {
        let mut access = SessionAccess {
            fp_hex,
            rx: None,
            mask: GRANT_ALL,
            deadline: None,
        };
        if let Some(np) = registry {
            let rx = np.subscribe(&access.fp_hex);
            let st = *rx.borrow();
            access.rx = Some(rx);
            access.fold(st);
        }
        access
    }

    /// Fold one watch state in — with the Moonlight reading of `revoked` (design §8): a
    /// fingerprint with no grants record is *ungoverned* (full control), because this plane's
    /// pairing authority is the GameStream cert list, whose unpair ends the session through
    /// the mgmt endpoint, not through this watch. A record that exists governs as on the
    /// native plane: its mask applies and its deadline (checked per tick) cuts the session.
    fn fold(&mut self, st: crate::native_pairing::AccessState) {
        if st.revoked {
            self.mask = GRANT_ALL;
            self.deadline = None;
        } else {
            self.mask = st.grants;
            self.deadline = st.deadline_unix;
        }
    }

    /// Fold any pending watch edit (non-blocking; the control thread is not async).
    fn poll(&mut self) {
        if let Some(rx) = self.rx.as_mut() {
            if rx.has_changed().unwrap_or(false) {
                let st = *rx.borrow_and_update();
                self.fold(st);
            }
        }
    }

    /// Whether the deadline has passed at `now` (the deadline second itself is expired — the
    /// same evaluation as the trust store's `effective`). An "expire now" console edit is just
    /// a deadline in the past arriving through the watch, so it lands here too.
    fn expired(&self, now_unix: i64) -> bool {
        self.deadline.is_some_and(|d| now_unix >= d)
    }
}

/// Quiet per-(session, grant-class) enforcement-drop accounting — the GameStream twin of the
/// native plane's `GrantDrops` (design §5.5): one counter and ONE `warn!` per class for the
/// whole session (per-event logging is the DoS), totals surfaced once at session end. Plain
/// integers, not atomics: the control thread is the only writer and reader.
struct GrantDrops {
    counts: [u64; 6],
    warned: [bool; 6],
}

impl GrantDrops {
    fn new() -> GrantDrops {
        GrantDrops {
            counts: [0; 6],
            warned: [false; 6],
        }
    }

    /// A class's slot in the fixed tables — the bit position of its grant, so the layout can
    /// never drift from the wire vocabulary.
    fn idx(class: GrantClass) -> usize {
        class.bit().trailing_zeros() as usize
    }

    /// Count one dropped item; log only the FIRST drop of each class — the support signal for
    /// "my keyboard does nothing" from a Moonlight client, which has no grants UX at all
    /// (silent enforcement is protocol-inherent here, design §8).
    fn note(&mut self, class: GrantClass) {
        let i = Self::idx(class);
        self.counts[i] += 1;
        if !self.warned[i] {
            self.warned[i] = true;
            tracing::warn!(
                class = ?class,
                "gamestream: dropping client input this session's access grants don't cover — \
                 counted; further drops of this class are silent until the session-end totals"
            );
        }
    }

    /// Log the session's drop totals (if any) and reset for the next session. Called from
    /// every per-session teardown arm — disconnect, host-side end, thread stop.
    fn end_of_session(&mut self) {
        use std::fmt::Write;
        let mut out = String::new();
        for class in [
            GrantClass::Gamepad,
            GrantClass::Pointer,
            GrantClass::Keyboard,
            GrantClass::Clipboard,
            GrantClass::Mic,
            GrantClass::Launch,
        ] {
            let n = self.counts[Self::idx(class)];
            if n != 0 {
                if !out.is_empty() {
                    out.push(' ');
                }
                let _ = write!(out, "{class:?}={n}");
            }
        }
        if !out.is_empty() {
            tracing::info!(drops = %out, "gamestream: access-grant drop totals for the session");
        }
        *self = GrantDrops::new();
    }
}

/// The one mask test standing between a decoded event class and its injector (design §5.3):
/// `true` = inject; `false` = counted and dropped. Kept a free function so the filter the
/// session actually runs is the thing the tests exercise.
fn permitted(mask: u32, class: GrantClass, drops: &mut GrantDrops) -> bool {
    if mask & class.bit() != 0 {
        return true;
    }
    drops.note(class);
    false
}

/// The virtual Xbox pad a Moonlight session presents, and the one place this plane decides which
/// backend builds it.
///
/// On Windows there are two, and they are not interchangeable to a game: the XUSB companion
/// registers only `GUID_DEVINTERFACE_XUSB` and exposes no HID collection, so Steam's hidapi
/// enumeration, SDL, RawInput, DirectInput, `joy.cpl` and WGI/GameInput cannot see it at all —
/// only classic `XInputGetState` can. The native plane made the HID pad its default on
/// 2026-08-09 for exactly that reason; this plane kept constructing
/// [`GamepadManager`](crate::inject::gamepad::GamepadManager) directly and so kept handing
/// Moonlight clients a pad most games cannot enumerate. Both planes now read the same knob —
/// `native::gamepad::windows_xbox_hid` (not an intra-doc link: it is `cfg(windows)`, so the link
/// would not resolve on any other target) — so `PUNKTFUNK_XBOX_BACKEND=xusb` reverts both
/// together and neither can drift again.
///
/// Everywhere else the choice does not exist: Linux has one uinput X-Box pad, and the stub
/// backend on other platforms drops events.
enum SessionPads {
    /// Linux uinput / the Windows XUSB companion — `crate::inject::gamepad`.
    Xusb(GamepadManager),
    /// The Windows UMDF HID Xbox pad, what the native plane builds by default.
    #[cfg(target_os = "windows")]
    Hid(crate::inject::xbox_windows::XboxWindowsManager),
}

impl SessionPads {
    /// Build this session's pad manager, honoring the shared Windows backend knob.
    fn new() -> SessionPads {
        #[cfg(target_os = "windows")]
        if crate::native::gamepad::windows_xbox_hid() {
            return SessionPads::Hid(crate::inject::xbox_windows::XboxWindowsManager::new());
        }
        SessionPads::Xusb(GamepadManager::new())
    }

    /// Apply one decoded controller event (create/destroy by mask, then state).
    fn handle(&mut self, ev: &GamepadEvent) {
        match self {
            SessionPads::Xusb(m) => m.handle(ev),
            #[cfg(target_os = "windows")]
            SessionPads::Hid(m) => m.handle(ev),
        }
    }

    /// Service the pads' feedback protocol and relay changed rumble levels. Games block inside the
    /// kernel/driver handshake until answered, so call this every tick.
    ///
    /// The HID pad's rich-feedback plane is discarded rather than plumbed: an Xbox pad has no
    /// lightbar or adaptive triggers to report, and GameStream has no vocabulary for one either —
    /// its rumble message (`0x010B`, [`super::gamepad::rumble_plaintext`]) carries the two handle
    /// motors and nothing else, which is also why the trigger levels are dropped at the call site.
    fn pump_rumble(&mut self, rumble: impl FnMut(u16, u16, u16, u16, u16)) {
        match self {
            SessionPads::Xusb(m) => m.pump_rumble(rumble),
            #[cfg(target_os = "windows")]
            SessionPads::Hid(m) => m.pump(rumble, |_| {}),
        }
    }
}

/// Reconcile the control port to the paired-client list: bound while at least one pairing
/// exists, closed when none remain. Idempotent and race-free (see [`Gate::running`]); call it
/// wherever the paired list changes — startup, pairing phase 4, unpair.
pub(crate) fn sync(state: &Arc<AppState>) -> Result<()> {
    let gate = &state.control_gate;
    if !gate.enabled.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut slot = gate.running.lock().unwrap_or_else(|e| e.into_inner());
    // Reap a listener whose thread died (a panic would otherwise leave a Running that
    // serves nobody and blocks every future rebind).
    if slot.as_ref().is_some_and(|r| r.thread.is_finished()) {
        if let Some(r) = slot.take() {
            let _ = r.thread.join();
        }
    }
    let want = !state
        .paired
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    match (slot.is_some(), want) {
        (false, true) => {
            *slot = Some(spawn(state.clone())?);
            Ok(())
        }
        (true, false) => {
            let r = slot.take().expect("slot checked non-empty");
            r.stop.store(true, Ordering::SeqCst);
            // Join before returning: it guarantees the socket is closed before a re-pair can
            // ask for a rebind. Bounded — the loop ticks every 2 ms, plus a ~100 ms farewell
            // flush when a client was connected — and unpair-all is a rare operator action.
            let _ = r.thread.join();
            tracing::info!(
                port = CONTROL_PORT,
                "ENet control torn down — no paired clients remain"
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A [`rusty_enet::Socket`] that drops datagrams whose source IP is not the launch owner's.
///
/// `rusty_enet` 0.4.0 exposes no setter for `maximum_waiting_data` (the C default of 32 MiB of
/// per-peer reassembly), so an off-path LAN peer that connects on 47999 can pin ~32 MiB × the
/// `peer_limit` and occupy peer slots without ever authenticating — and the same unfiltered path
/// lets an on-path attacker spoof the owner's source to feed the tracked peer. Once the owner IS
/// known, filtering at the socket drops those datagrams before ENet allocates any per-peer state.
///
/// The owner is read live from `launch` on each receive, so this covers only the window where a
/// launch is recorded: before `/launch` (owner `None`) the filter passes everything, and what
/// keeps an unauthenticated peer from squatting a slot through that whole idle window is
/// [`accept_connect`] resetting the peer in the `Event::Connect` arm below.
/// security-review 2026-08-15 findings 2 and 13; 2026-08-25 finding 1.
struct OwnerFilteredSocket {
    inner: UdpSocket,
    state: Arc<AppState>,
}

impl rusty_enet::Socket for OwnerFilteredSocket {
    type Address = std::net::SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, opts: rusty_enet::SocketOptions) -> Result<(), std::io::Error> {
        rusty_enet::Socket::init(&mut self.inner, opts)
    }

    fn send(&mut self, address: Self::Address, buffer: &[u8]) -> Result<usize, std::io::Error> {
        rusty_enet::Socket::send(&mut self.inner, address, buffer)
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; rusty_enet::MTU_MAX],
    ) -> Result<Option<(Self::Address, rusty_enet::PacketReceived)>, std::io::Error> {
        // Loop so a dropped non-owner datagram doesn't starve a following owner datagram in the
        // same drain; the inner socket is non-blocking, so this returns `Ok(None)` on WouldBlock.
        loop {
            match rusty_enet::Socket::receive(&mut self.inner, buffer)? {
                Some((addr, received)) => {
                    let owner = self.state.launch.lock().unwrap().and_then(|s| s.peer_ip);
                    if owner.is_some_and(|ip| ip != addr.ip()) {
                        continue;
                    }
                    return Ok(Some((addr, received)));
                }
                None => return Ok(None),
            }
        }
    }
}

/// Whether a fresh ENet connect may be admitted at all. A live `/launch` is the floor —
/// Moonlight connects the control stream only after the RTSP handshake, so there is no such
/// thing as a legitimate connect without one — and when the launching IP was captured on both
/// sides it must match, the same source-IP bind the RTSP/media plane applies
/// (`rtsp::authorized_launch`). Kept a free function so the gate the session actually runs is
/// the thing the tests exercise.
fn accept_connect(launch: Option<LaunchSession>, from: Option<IpAddr>) -> bool {
    match (launch.map(|l| l.peer_ip), from) {
        // No live `/launch`: nothing on this port is legitimate yet.
        (None, _) => false,
        // Launching IP known on both sides but mismatched → not the owner.
        (Some(Some(want)), Some(got)) => want == got,
        // The address couldn't be captured on one side → launch-present only.
        _ => true,
    }
}

/// Bind the ENet control host on 47999 and service it on a dedicated thread until `stop`.
fn spawn(state: Arc<AppState>) -> Result<Running> {
    let socket = UdpSocket::bind(("0.0.0.0", CONTROL_PORT)).context("bind control UDP")?;
    socket
        .set_nonblocking(true)
        .context("control socket nonblocking")?;
    let mut host = Host::new(
        OwnerFilteredSocket {
            inner: socket,
            state: state.clone(),
        },
        HostSettings {
            peer_limit: 4,
            // Moonlight connects with CTRL_CHANNEL_COUNT (0x30) channels and sends gamepad
            // input on channel 0x10+n — a smaller limit silently discards controller input.
            channel_limit: 0x30,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("ENet host init: {e:?}"))?;
    tracing::info!(port = CONTROL_PORT, "ENet control listening");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_seen = stop.clone();
    let thread = std::thread::Builder::new()
        .name("punktfunk-control".into())
        .spawn(move || {
            // GCM scheme detected from the first authenticating packet; reused thereafter.
            let mut detected: Option<Scheme> = None;
            // Consecutive control-decrypt failures for this peer — throttles the warn log so a
            // junk-packet flood can't spam unbounded lines (security-review 2026-06-28 #10).
            let mut decrypt_fails: u64 = 0;
            // Decoded keyboard/mouse is forwarded to a dedicated host-lifetime injector thread —
            // NEVER injected inline, so a slow Wayland/libei/SendInput call can't head-block ENet
            // keepalive/retransmit servicing on this thread. The injector owns non-Send compositor
            // state and lives on its own thread (see crate::inject::InjectorService); the held
            // `inj_tx` clone keeps it alive for the control thread's lifetime.
            let inj_tx = crate::inject::InjectorService::start().sender();
            // Virtual gamepads (uinput). ONE monotonic host→client control sequence counter, shared
            // by every outbound message (rumble + the HDR-mode signal): the GCM nonce is derived
            // from `seq`, so a per-message-type counter would reuse (key, nonce) pairs across
            // message types in the host direction.
            let mut pads = SessionPads::new();
            // Pen/touch translator (SS_PEN/SS_TOUCH → virtual tablet / wire touch). Sent only
            // by clients that saw our SS_FF_PEN_TOUCH_EVENTS feature flag (rtsp.rs).
            let mut pointer = super::pen::GsPointer::new();
            let mut host_seq: u32 = 0;
            // One-shot latch for the HDR-mode control message (0x010e); re-armed on Disconnect.
            let mut hdr_sent = false;
            let mut peer: Option<PeerID> = None;
            // The session's GCM key, remembered from the last tick it was live. Ending a session
            // clears `launch` — and the key lives there — so without this copy the one message that
            // has to go out *because* the session ended could no longer be sealed.
            let mut last_key: Option<[u8; 16]> = None;
            // Per-client access (WP13): the live session's grant mask + deadline, resolved
            // from the launch owner's fingerprint; `None` while no session is live. `drops`
            // is the session's quiet enforcement accounting (counters, never per-event logs).
            let mut access: Option<SessionAccess> = None;
            let mut drops = GrantDrops::new();
            loop {
                // WP0 teardown: the last pairing was removed while we were live. Tell a
                // connected client the session is over — termination + disconnect, the same
                // farewell the host-side session end below uses — rather than vanish on it,
                // flush briefly so the disconnect actually reaches the wire, then end the
                // session and exit. Dropping `host` closes the socket.
                if stop_seen.load(Ordering::SeqCst) {
                    if let Some(pid) = peer {
                        if let (Some(scheme), Some(key)) = (detected, last_key) {
                            let pt = termination_plaintext();
                            let wire = encrypt_control(&key, &scheme, host_seq, &pt);
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control: termination send failed");
                            }
                        }
                        host.peer_mut(pid).disconnect_later(0);
                        // Bounded flush: enough ticks for ENet to emit the termination and
                        // the disconnect handshake; we are exiting either way.
                        for _ in 0..50 {
                            while matches!(host.service(), Ok(Some(_))) {}
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    drops.end_of_session();
                    state.end_session("control stream stopped — last pairing removed");
                    tracing::info!(port = CONTROL_PORT, "control: stopped (no paired clients)");
                    return;
                }
                // Track the live session's access each tick (2 ms): resolve on a new owner,
                // fold any console edit in (one watch poll — cheap version check), and cut the
                // session the tick its deadline passes. Events serviced below read the folded
                // mask, so an edit reaches enforcement within one tick of the watch publish.
                let owner_fp = state.launch.lock().unwrap().and_then(|s| s.owner_fp);
                match owner_fp {
                    None => access = None,
                    Some(fp) => {
                        let fp_hex = hex::encode(fp);
                        if access.as_ref().is_none_or(|a| a.fp_hex != fp_hex) {
                            access = Some(SessionAccess::resolve(state.access.get(), fp_hex));
                        } else if let Some(a) = access.as_mut() {
                            a.poll();
                        }
                        if access
                            .as_ref()
                            .is_some_and(|a| a.expired(super::wall_unix_now()))
                        {
                            // Expiry (or an "expire now" edit) ends the session as a decision
                            // — like the mgmt unpair, not like a network drop. `quit_session`
                            // clears `launch`, and the host-side-ended arm below then sends
                            // the TERMINATION + disconnect: GameStream has no AccessUpdate
                            // vocabulary, so that close IS the whole message (design §8). The
                            // nvhttp gates keep the expired record from re-launching.
                            tracing::info!(
                                "gamestream: session access expired — ending the session"
                            );
                            state.quit_session("gamestream access expired");
                            access = None;
                        }
                    }
                }
                loop {
                    match host.service() {
                        Ok(Some(event)) => match event {
                            Event::Connect { peer: p, .. } => {
                                // Admit only the launch owner ([`accept_connect`]): the tracked
                                // peer's disconnect ENDS the session, so an unauthenticated LAN
                                // peer that connects+disconnects on 47999 must not be able to
                                // steal the slot and tear a live session down (security-review
                                // #4). A refused peer is RESET, not merely left untracked — the
                                // port is open the whole idle life of a paired host, and a peer
                                // ENet keeps alive with its own pings would otherwise hold one of
                                // four slots (plus its 32 MiB reassembly budget) indefinitely and
                                // starve every later session (security-review 2026-08-25 #1).
                                // `disconnect_now` frees the slot before the next tick and emits
                                // no `Disconnect` event for the arm below to read as a session end.
                                let launch = *state.launch.lock().unwrap();
                                let from = p.address().map(|a| a.ip());
                                if accept_connect(launch, from) {
                                    tracing::info!("control: client connected");
                                    peer = Some(p.id());
                                } else {
                                    tracing::warn!(
                                        ?from,
                                        "control: peer connected without a matching /launch — refusing"
                                    );
                                    p.disconnect_now(0);
                                }
                            }
                            Event::Disconnect { peer: p, .. } => {
                                // Gate on the TRACKED session peer: a stray probe peer (or the
                                // OLD peer's late timeout after a fast reconnect replaced it in
                                // the Connect arm) must neither clobber the live session's input
                                // state nor end its session.
                                if peer != Some(p.id()) {
                                    tracing::debug!("control: non-session peer disconnected");
                                    continue;
                                }
                                tracing::info!("control: client disconnected");
                                detected = None;
                                decrypt_fails = 0;
                                peer = None;
                                // Re-arm the HDR-mode signal for the next connection.
                                hdr_sent = false;
                                // Unplug the session's virtual pads + tablet (destroying the
                                // uinput pen releases any held tool/tip kernel-side).
                                pads = SessionPads::new();
                                pointer = super::pen::GsPointer::new();
                                // Surface the session's enforcement-drop totals (WP13).
                                drops.end_of_session();
                                // The control stream is the session's liveness anchor — Moonlight
                                // holds it for the whole stream, and ENet detects a vanished peer
                                // via its reliable-ping timeout (~5–30 s), which ALSO lands here.
                                // End the session: without this, a client that disconnects without
                                // an explicit RTSP TEARDOWN / nvhttp `/cancel` (a network drop,
                                // sleep, crash — or just a plain Moonlight quit, which sends
                                // neither) left the media threads streaming at the dead endpoint
                                // forever (a UDP send only errors on an ICMP port-unreachable) and
                                // the stale launch/streaming state wedged every reconnect.
                                state.end_session("control stream disconnected");
                            }
                            Event::Receive {
                                peer: p,
                                channel_id,
                                packet,
                            } => {
                                // Only the tracked session peer's input is honored. The owner-IP
                                // socket filter already drops non-owner datagrams once a launch is
                                // recorded; this is defense-in-depth for the window before the
                                // owner is captured (and mirrors the `Disconnect` arm's gate) so a
                                // peer that connected while `owner_ip` was `None` still cannot
                                // inject keyboard/mouse/gamepad after another peer became the
                                // session. security-review 2026-08-15 finding 2.
                                if peer != Some(p.id()) {
                                    continue;
                                }

                                // The mask a missing SessionAccess stands in for is FULL:
                                // input only decrypts under the /launch key, so a decryptable
                                // event with no resolved access can only be the ≤2 ms sliver
                                // between `/launch` landing and the next tick's resolve — and
                                // an ungoverned (recordless) session is full-control anyway.
                                on_receive(
                                    &state,
                                    channel_id,
                                    packet.data(),
                                    &mut detected,
                                    &mut decrypt_fails,
                                    &inj_tx,
                                    &mut pads,
                                    &mut pointer,
                                    access.as_ref().map(|a| a.mask).unwrap_or(GRANT_ALL),
                                    &mut drops,
                                );
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = ?e, "control: service error");
                            break;
                        }
                    }
                }
                // A session can also end from the HOST side: the launched game exited, the operator
                // stopped it, or `/cancel` arrived on another connection. Tearing the media threads
                // down is *silence*, not a signal — Moonlight holds this control stream for the
                // whole session, so with no word from us it sits on its last frame until its own
                // timeout eventually fires. The client froze instead of ending (on-glass, .173).
                //
                // Disconnecting the peer is how it learns. Moonlight treats a control-stream
                // disconnect as the session being over and returns to its app list — the same place
                // it lands when the user quits, which is exactly where "the game exited" should
                // leave them.
                //
                // Merely dropping the peer is not enough either: the client reports that as a
                // connection error (`-1` on glass, .173), because an ENet disconnect on its own is
                // indistinguishable from the host falling over. The protocol has a word for this —
                // a TERMINATION control message — so send that first and let the disconnect follow.
                //
                // `end_session` clears `launch`, so a tracked peer with no launch behind it *is*
                // the ended session. Clearing `peer` first makes this fire once; the real
                // `Disconnect` event that follows then takes the non-session-peer branch, which is
                // why this arm repeats that branch's per-connection cleanup.
                if let Some(pid) = peer {
                    let ended = state
                        .launch
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none();
                    if ended {
                        // Only sealable once the client's scheme is known; without it, fall through
                        // to the bare disconnect rather than send a packet it can't read.
                        if let (Some(scheme), Some(key)) = (detected, last_key) {
                            let pt = termination_plaintext();
                            let wire = encrypt_control(&key, &scheme, host_seq, &pt);
                            host_seq = host_seq.wrapping_add(1);
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control: termination send failed");
                            }
                        }
                        tracing::info!("control: the session ended — telling the client");
                        // `disconnect_later` flushes the queued termination first; a plain
                        // `disconnect` would race it off the wire and land us back at "-1".
                        host.peer_mut(pid).disconnect_later(0);
                        peer = None;
                        detected = None;
                        decrypt_fails = 0;
                        hdr_sent = false;
                        pads = SessionPads::new();
                        pointer = super::pen::GsPointer::new();
                        drops.end_of_session();
                    }
                }
                // Service the pads' force-feedback protocol every tick (games block inside
                // EVIOCSFF until answered) and relay mixed rumble levels to the client.
                //
                // SECURITY NOTE (audit #5, legacy GCM nonce reuse): on the LEGACY control scheme
                // (`NonceKind::Legacy*`, which we hit because we advertise no encryption) the nonce is
                // just the per-direction `seq` (`iv[0]=seq&0xff`, rest zero) with NO direction byte —
                // so host control messages (this `host_seq`, shared by rumble + the HDR-mode signal)
                // and client input (its own seq) share the same (key, nonce) space when their seqs
                // collide. This is INHERENT to Nvidia's old-style
                // GameStream control encryption (Apollo/moonlight-common-c are identical: only the V2
                // scheme adds `iv[10..12] = 'H','C'` to separate the host direction). It can't be fixed
                // on the legacy wire without breaking Moonlight; the GCM key is the client-supplied
                // `rikey` (so only a passive eavesdropper who missed the HTTPS /launch is the
                // adversary). The real fix is V2 control-encryption negotiation; for untrusted networks
                // use the native punktfunk/1 plane (correct per-direction nonces + seq-as-AAD).
                if let (Some(pid), Some(scheme)) = (peer, detected) {
                    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
                    // Remember it for the teardown message (see `last_key`).
                    if key.is_some() {
                        last_key = key;
                    }
                    if let Some(key) = key {
                        let mut out: Vec<Vec<u8>> = Vec::new();
                        // One-shot HDR-mode signal (type 0x010e / Sunshine `IDX_HDR_MODE`) once the
                        // control stream is live. Stock Moonlight clients only flip the TV into HDR
                        // picture mode when they receive this async message — the video is already
                        // BT.2020 PQ, but without the cue the display stays in SDR mode (the exact
                        // symptom aurora-tv PR #53 worked around client-side). Sent before rumble so
                        // the client sees HDR as early as possible.
                        if !hdr_sent {
                            // `state.stream` is populated by the RTSP ANNOUNCE, which precedes the
                            // control stream — but guard the race: only commit the one-shot decision
                            // once a config exists, so a not-yet-set stream can't latch us into never
                            // signaling HDR. A non-HDR session latches too (it never needs the msg).
                            if let Some(hdr) = state.stream.lock().unwrap().map(|s| s.hdr) {
                                if hdr {
                                    let pt =
                                        hdr_mode_plaintext(true, &pf_frame::hdr::generic_hdr10());
                                    out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                                    host_seq = host_seq.wrapping_add(1);
                                    tracing::info!(
                                        "control: signaled HDR mode ON to client (0x010e)"
                                    );
                                }
                                hdr_sent = true;
                            }
                        }
                        // The GameStream leg carries the handle motors only: Moonlight's
                        // trigger-rumble message (`ConnListenerRumbleTriggers`) is a separate
                        // control-stream id we have not read out of moonlight-common-c yet, and
                        // `low`/`high` here are already what `rumble_plaintext` (0x010B) encodes.
                        // The uinput backend cannot source triggers anyway (evdev `FF_RUMBLE` has
                        // two fields), so nothing is dropped today.
                        pads.pump_rumble(|index, low, high, _lt, _rt| {
                            let pt = super::gamepad::rumble_plaintext(index, low, high);
                            out.push(encrypt_control(&key, &scheme, host_seq, &pt));
                            host_seq = host_seq.wrapping_add(1);
                        });
                        for wire in out {
                            if let Err(e) = host.peer_mut(pid).send(0, &Packet::reliable(&wire[..]))
                            {
                                tracing::warn!(error = ?e, "control send failed");
                            }
                        }
                    }
                } else {
                    // No client/scheme yet: still answer FF uploads so games don't block.
                    pads.pump_rumble(|_, _, _, _, _| {});
                }
                // ENet needs frequent servicing for handshake/keepalive/retransmit.
                std::thread::sleep(Duration::from_millis(2));
            }
        })
        .context("spawn control thread")?;
    Ok(Running { stop, thread })
}

/// Decode the lost-frame range from an invalidate-reference-frames (0x0301) control message: two
/// little-endian `i64` (firstFrame, lastFrame) after the 4-byte `[u16 type][u16 length]` header,
/// matching Sunshine/Apollo's `IDX_INVALIDATE_REF_FRAMES`. Returns `None` when the body is too
/// short or the range is nonsensical, in which case the caller falls back to a full IDR.
fn decode_rfi_range(pt: &[u8]) -> Option<(i64, i64)> {
    if pt.len() < 20 {
        return None;
    }
    let first = i64::from_le_bytes(pt[4..12].try_into().ok()?);
    let last = i64::from_le_bytes(pt[12..20].try_into().ok()?);
    (first >= 0 && last >= first).then_some((first, last))
}

/// Handle one received control packet: decrypt it (learning the GCM scheme on the first one),
/// decode any input event, classify it against the session's grant mask (WP13 — drops are
/// counted, not logged), and inject what the grants cover into the host session.
#[allow(clippy::too_many_arguments)]
fn on_receive(
    state: &AppState,
    _channel_id: u8,
    d: &[u8],
    detected: &mut Option<Scheme>,
    decrypt_fails: &mut u64,
    inj_tx: &Sender<InputEvent>,
    pads: &mut SessionPads,
    pointer: &mut super::pen::GsPointer,
    grants: u32,
    drops: &mut GrantDrops,
) {
    let Some(key) = state.launch.lock().unwrap().map(|s| s.gcm_key) else {
        return; // control traffic before /launch — no key yet
    };
    // Encrypted control packets begin with u16 LE encType = 0x0001 and an 8-byte header.
    if d.len() < 8 || d[0] != 0x01 || d[1] != 0x00 {
        return;
    }

    let pt = match decrypt_control(&key, d, detected) {
        Some((scheme, pt)) => {
            if detected.is_none() {
                tracing::info!(?scheme, "control: GCM scheme locked in");
            }
            *detected = Some(scheme);
            *decrypt_fails = 0;
            pt
        }
        None => {
            // Throttle: a junk-packet flood must not spam one warn line per packet. Log the first
            // failure, then only at exponentially-spaced counts (1, 2, 4, 8, …).
            *decrypt_fails += 1;
            if decrypt_fails.is_power_of_two() {
                tracing::warn!(
                    len = d.len(),
                    fails = *decrypt_fails,
                    "control: GCM decrypt failed"
                );
            }
            return;
        }
    };

    // Recovery requests after loss. Invalidate-reference-frames (0x0301, Gen7) carries the lost
    // frame range (two LE i64 after the [type][len] header, like Sunshine/Apollo's
    // IDX_INVALIDATE_REF_FRAMES) — route it to the encoder, which invalidates those refs instead of
    // a full IDR when it can (NVENC RFI). Request-IDR (0x0302 / 0x0305) and a malformed 0x0301 force
    // a keyframe. The video thread drains rfi_range/force_idr and resyncs without a multi-second stall.
    if pt.len() >= 2 {
        let inner = u16::from_le_bytes([pt[0], pt[1]]);
        if inner == 0x0301 {
            if let Some((first, last)) = decode_rfi_range(&pt) {
                *state.rfi_range.lock().unwrap() = Some((first, last));
                tracing::debug!(first, last, "control: RFI request → invalidate ref frames");
            } else {
                state
                    .force_idr
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::debug!("control: RFI request (no range) → keyframe");
            }
            return;
        }
        if matches!(inner, 0x0302 | 0x0305) {
            state
                .force_idr
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(
                ty = %format_args!("{inner:#06x}"),
                "control: IDR request → keyframe"
            );
            return;
        }
    }

    // Controller events go to the uinput virtual pads (created on demand per the mask) —
    // gated BEFORE the manager sees them, which is also the deny-at-setup (WP4's idiom): a
    // session without the GAMEPAD grant never creates a uinput node or a pad-audio streamer,
    // because the creating event never arrives.
    if let Some(gp) = super::gamepad::decode(&pt) {
        crate::sleep_inhibit::note_input();
        if permitted(grants, GrantClass::Gamepad, drops) {
            pads.handle(&gp);
        }
        return;
    }

    // Pen/touch extension events (Moonlight sends them only after seeing our feature flag):
    // pen drives this session's virtual tablet; touch forwards as ordinary wire touches.
    // Pointer-class by construction (the plane tag decides, like the native pen plane).
    if let Some(p) = super::input::decode_pointer(&pt) {
        crate::sleep_inhibit::note_input();
        if permitted(grants, GrantClass::Pointer, drops) {
            pointer.apply(&p, |ev| {
                let _ = inj_tx.send(ev);
            });
        }
        return;
    } else if super::input::is_pointer_magic(&pt) {
        // A pointer magic that failed the body parse — a layout mismatch against this
        // client, exactly what an on-glass "touch/pen does nothing" needs surfaced. The
        // first few dump their bytes so the mismatch is diagnosable from the log alone.
        static HEX_DUMPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if HEX_DUMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
            let hex: String = pt.iter().map(|b| format!("{b:02x}")).collect();
            tracing::warn!(
                len = pt.len(),
                bytes = %hex,
                "gamestream: SS_TOUCH/SS_PEN packet failed to decode (malformed/unexpected layout)"
            );
        } else {
            tracing::warn!(
                len = pt.len(),
                "gamestream: SS_TOUCH/SS_PEN packet failed to decode (malformed/unexpected layout)"
            );
        }
        return;
    }

    let events = super::input::decode(&pt);
    if events.is_empty() {
        return; // keepalive / QoS / unhandled input kind
    }
    // A Moonlight guest is driving the box — drop any standing suspend veto so their own "Sleep"
    // reaches logind (see `sleep_inhibit`). Past the `is_empty` gate on purpose: a keepalive is
    // the one thing a passive viewer DOES send, and it must not read as someone being there.
    crate::sleep_inhibit::note_input();

    // Forward to the dedicated injector thread (it opens the backend on the first event and
    // coalesces redundant motion) — each event past one mask test against the exhaustive
    // classifier (design §5.3), so a Controller-only Moonlight guest's keyboard/mouse is inert
    // before injection, exactly like the native datagram dispatch. A closed channel means the
    // injector thread died at startup — input is lossy, so drop silently rather than spam.
    for ev in events {
        if permitted(grants, classify(ev.kind), drops) {
            let _ = inj_tx.send(ev);
        }
    }
}

/// How a control packet's nonce is built — Moonlight picks one based on the negotiated flags.
#[derive(Clone, Copy, Debug)]
enum NonceKind {
    /// `SS_ENC_CONTROL_V2`: 12-byte nonce, `seq` in [0..4], marker bytes at [10..12].
    V2 { seq_be: bool, marker: [u8; 2] },
    /// Legacy: 16-byte nonce, only `iv[0] = seq & 0xff` (the rest zero).
    LegacyLowByte,
    /// Legacy variant: 16-byte nonce, full `seq` in [0..4] (the rest zero).
    Legacy16Seq { seq_be: bool },
}

impl NonceKind {
    fn nonce(&self, seq: u32) -> Vec<u8> {
        let seq_bytes = |be: bool| {
            if be {
                seq.to_be_bytes()
            } else {
                seq.to_le_bytes()
            }
        };
        match *self {
            NonceKind::V2 { seq_be, marker } => {
                let mut iv = vec![0u8; 12];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv[10] = marker[0];
                iv[11] = marker[1];
                iv
            }
            NonceKind::LegacyLowByte => {
                let mut iv = vec![0u8; 16];
                iv[0] = (seq & 0xff) as u8;
                iv
            }
            NonceKind::Legacy16Seq { seq_be } => {
                let mut iv = vec![0u8; 16];
                iv[0..4].copy_from_slice(&seq_bytes(seq_be));
                iv
            }
        }
    }
}

/// The byte-exact GCM scheme that opened a control packet. Determined empirically once per
/// connection (AES-GCM gives no partial credit, so an authenticating combination is proof).
#[derive(Clone, Copy, Debug)]
struct Scheme {
    /// `gcm_key` is byte-reversed before use (defensive; Sunshine's net effect is forward).
    key_rev: bool,
    nonce: NonceKind,
    /// GCM tag sits before the ciphertext (vs after).
    tag_first: bool,
    aad: Aad,
}

#[derive(Clone, Copy, Debug)]
enum Aad {
    None,
    /// The 4-byte cleartext header prefix (encType + length), `d[0..4]`.
    Header4,
}

impl Scheme {
    fn key(&self, base: &[u8; 16]) -> [u8; 16] {
        let mut k = *base;
        if self.key_rev {
            k.reverse();
        }
        k
    }
}

/// Open an encrypted control packet `d` (8-byte cleartext header + `[tag?][ciphertext]`). If
/// `detected` is set only that scheme is tried (fast path); otherwise the full cross-product
/// of plausible schemes (nonce construction × key byte-order × tag position × AAD) is swept
/// and the combination whose GCM tag authenticates is returned.
fn decrypt_control(
    key: &[u8; 16],
    d: &[u8],
    detected: &Option<Scheme>,
) -> Option<(Scheme, Vec<u8>)> {
    let seq = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
    let payload = &d[8..];
    if payload.len() < 16 {
        return None;
    }

    let attempt = |s: Scheme| -> Option<Vec<u8>> {
        // aes-gcm wants `ciphertext || tag`; reassemble from whichever wire order this is.
        let (ct, tag) = if s.tag_first {
            (&payload[16..], &payload[..16])
        } else {
            (
                &payload[..payload.len() - 16],
                &payload[payload.len() - 16..],
            )
        };
        let mut ct_tag = Vec::with_capacity(ct.len() + 16);
        ct_tag.extend_from_slice(ct);
        ct_tag.extend_from_slice(tag);
        let aad: &[u8] = match s.aad {
            Aad::None => &[],
            Aad::Header4 => &d[0..4],
        };
        gcm_open(&s.key(key), &s.nonce.nonce(seq), &ct_tag, aad)
    };

    if let Some(s) = *detected {
        return attempt(s).map(|pt| (s, pt));
    }

    // Candidate nonce constructions, most-likely first.
    const MARKERS: [[u8; 2]; 3] = [*b"CC", *b"HC", *b"CH"];
    let mut kinds: Vec<NonceKind> = vec![NonceKind::LegacyLowByte];
    for seq_be in [false, true] {
        for marker in MARKERS {
            kinds.push(NonceKind::V2 { seq_be, marker });
        }
        kinds.push(NonceKind::Legacy16Seq { seq_be });
    }

    for &nonce in &kinds {
        for key_rev in [false, true] {
            for tag_first in [true, false] {
                for aad in [Aad::None, Aad::Header4] {
                    let s = Scheme {
                        key_rev,
                        nonce,
                        tag_first,
                        aad,
                    };
                    if let Some(pt) = attempt(s) {
                        return Some((s, pt));
                    }
                }
            }
        }
    }
    None
}

/// Serialize an [`HdrMeta`] into Moonlight's `SS_HDR_METADATA` control-message layout: 26 bytes,
/// all **little-endian** (moonlight-common-c parses it with `BYTE_ORDER_LITTLE`), primaries in
/// **R, G, B** order — note [`HdrMeta`]/ST.2086 stores them G, B, R, so we reorder. Luminance is
/// re-scaled to the wire units the client reads: `maxDisplayLuminance`/`maxFullFrameLuminance` in
/// whole nits, `minDisplayLuminance` in 1/10000-nit, content light levels already in nits. There is
/// no separate full-frame luminance in our metadata, so it mirrors the mastering peak.
fn ss_hdr_metadata(m: &HdrMeta) -> [u8; 26] {
    let max_display_nits = (m.max_display_mastering_luminance / 10_000).min(u16::MAX as u32) as u16;
    let min_display = m.min_display_mastering_luminance.min(u16::MAX as u32) as u16;
    let mut b = [0u8; 26];
    let mut o = 0;
    let mut put = |v: u16| {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        o += 2;
    };
    // displayPrimaries[3] in R, G, B order (HdrMeta is G, B, R).
    for p in [
        m.display_primaries[2],
        m.display_primaries[0],
        m.display_primaries[1],
    ] {
        put(p[0]);
        put(p[1]);
    }
    put(m.white_point[0]);
    put(m.white_point[1]);
    put(max_display_nits); // maxDisplayLuminance (nits)
    put(min_display); // minDisplayLuminance (1/10000 nit)
    put(m.max_cll); // maxContentLightLevel (nits)
    put(m.max_fall); // maxFrameAverageLightLevel (nits)
    put(max_display_nits); // maxFullFrameLuminance (nits) — no separate value; mirror the peak
    debug_assert_eq!(o, 26);
    b
}

/// Build the host→client HDR-mode control plaintext (type `0x010e` / Sunshine `IDX_HDR_MODE`):
/// `[u16 type][u16 length][u8 enabled][SS_HDR_METADATA]`, all little-endian, `length` counting the
/// enable byte + metadata (mirrors [`super::gamepad::rumble_plaintext`]). Moonlight flips the
/// TV/decoder into HDR picture mode on `enabled != 0` (`ConnListenerSetHdrMode`). We advertise a
/// Sunshine server, so the client (`IS_SUNSHINE()`) reads the full 26-byte metadata block.
fn hdr_mode_plaintext(enabled: bool, m: &HdrMeta) -> Vec<u8> {
    let meta = ss_hdr_metadata(m);
    let mut pt = Vec::with_capacity(4 + 1 + meta.len());
    pt.extend_from_slice(&0x010eu16.to_le_bytes()); // type
    pt.extend_from_slice(&((1 + meta.len()) as u16).to_le_bytes()); // length = enable + metadata
    pt.push(enabled as u8);
    pt.extend_from_slice(&meta);
    pt
}

/// Build the host→client TERMINATION control plaintext — "the session is over, on purpose".
///
/// Without it, ending a session host-side is invisible on the wire: the media threads simply stop,
/// which the client cannot tell from a host that fell over, so it either sits on its last frame or
/// (once we disconnect the peer) reports a connection error. This is the message that makes it a
/// clean end, and the client returns to its app list.
///
/// Verified against moonlight-common-c `ControlStream.c` (master, 2026-07-26):
///
/// * **Type `0x0109`**, from `packetTypesGen7Enc[IDX_TERMINATION]`. Which table the client reads is
///   decided by ONE thing — the version we advertise:
///   `encryptedControlStream = APP_VERSION_AT_LEAST(7, 1, 431)`, and
///   [`super::APP_VERSION`] is exactly `7.1.431`. So every client is on the encrypted table, where
///   termination is `0x0109`; the plain table's `0x0100` would be ignored.
///
///   This is *not* the same axis as [`NonceKind`], which only describes how the GCM nonce is built.
///   Deriving the type from the nonce scheme looks reasonable and is wrong — it sent `0x0100` to a
///   client reading the encrypted table, which ignored it silently and reported the session as `-1`
///   (on glass, .173). The HDR message could never have caught this: `0x010e` in both tables.
/// * **Payload** is a big-endian `u32` reason (the ≥6-byte "extended" branch; the short branch is a
///   little-endian `u16`, which is GFE's older shape).
/// * **`0x80030023`** is `NVST_DISCONN_SERVER_TERMINATED_CLOSED`, which the client maps to
///   `ML_ERROR_GRACEFUL_TERMINATION` *provided it has seen a frame* — true for any real session,
///   and the reason this reads as "the app quit" rather than an error.
fn termination_plaintext() -> Vec<u8> {
    /// `packetTypesGen7Enc[IDX_TERMINATION]` — see the version gate above.
    const TERMINATION: u16 = 0x0109;
    /// `NVST_DISCONN_SERVER_TERMINATED_CLOSED` — a deliberate, graceful host-side end.
    const GRACEFUL: u32 = 0x8003_0023;
    let mut pt = Vec::with_capacity(8);
    pt.extend_from_slice(&TERMINATION.to_le_bytes());
    pt.extend_from_slice(&4u16.to_le_bytes()); // length = the reason that follows
    pt.extend_from_slice(&GRACEFUL.to_be_bytes()); // big-endian: the extended branch
    pt
}

/// Seal a host→client control message, mirroring the client's `detected` scheme with the
/// direction flipped: V2 nonces use marker `H?` (host-originated) instead of `C?`; legacy
/// nonces keep their construction with our own independent `seq` counter. Wire layout matches
/// what the client sends us: `[0x0001][length][seq][tag|ct per scheme.tag_first]`.
fn encrypt_control(key: &[u8; 16], scheme: &Scheme, seq: u32, pt: &[u8]) -> Vec<u8> {
    let nonce_kind = match scheme.nonce {
        NonceKind::V2 { seq_be, marker } => NonceKind::V2 {
            seq_be,
            marker: [b'H', marker[1]],
        },
        other => other,
    };
    let length = (4 + 16 + pt.len()) as u16;
    let mut wire = Vec::with_capacity(8 + 16 + pt.len());
    wire.extend_from_slice(&0x0001u16.to_le_bytes());
    wire.extend_from_slice(&length.to_le_bytes());
    wire.extend_from_slice(&seq.to_le_bytes());
    let aad: Vec<u8> = match scheme.aad {
        Aad::None => Vec::new(),
        Aad::Header4 => wire[0..4].to_vec(),
    };
    let ct_tag = gcm_seal(&scheme.key(key), &nonce_kind.nonce(seq), pt, &aad);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    if scheme.tag_first {
        wire.extend_from_slice(tag);
        wire.extend_from_slice(ct);
    } else {
        wire.extend_from_slice(ct);
        wire.extend_from_slice(tag);
    }
    wire
}

/// AES-128-GCM seal (companion to [`gcm_open`]); returns `ciphertext || tag`.
fn gcm_seal(key: &[u8; 16], nonce: &[u8], pt: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: pt, aad };
    // Each arm's `try_into` is guarded by the length it matched on.
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("12-byte nonce"), p)
            .expect("GCM seal"),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .unwrap()
            .encrypt(nonce.try_into().expect("16-byte nonce"), p)
            .expect("GCM seal"),
        _ => unreachable!("nonce length"),
    }
}

/// AES-128-GCM open with a 12- or 16-byte nonce and explicit AAD. Returns the plaintext iff
/// the tag authenticates. `ct_tag` is `ciphertext || tag` (aes-gcm's expected order).
fn gcm_open(key: &[u8; 16], nonce: &[u8], ct_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::consts::{U12, U16};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{aes::Aes128, AesGcm};

    let p = Payload { msg: ct_tag, aad };
    match nonce.len() {
        12 => AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        16 => AesGcm::<Aes128, U16>::new_from_slice(key)
            .ok()?
            .decrypt(nonce.try_into().ok()?, p)
            .ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_rfi_range;

    /// Build a 0x0301 invalidate-ref-frames plaintext: `[type LE][len LE][firstFrame i64 LE][last i64 LE]`.
    fn rfi_msg(first: i64, last: i64) -> Vec<u8> {
        let mut v = vec![0x01, 0x03, 0x10, 0x00]; // type 0x0301, length 16
        v.extend_from_slice(&first.to_le_bytes());
        v.extend_from_slice(&last.to_le_bytes());
        v
    }

    /// A live `/launch` whose owner IP is `peer_ip`.
    fn launched(peer_ip: Option<std::net::IpAddr>) -> Option<super::LaunchSession> {
        Some(super::LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip,
            owner_fp: None,
        })
    }

    /// The ENet connect gate: 47999 is open the whole idle life of a paired host, so a connect
    /// with no live `/launch` behind it is refused (it would otherwise squat one of four peer
    /// slots until every later session fails), and a launched session admits only the owner's
    /// IP — falling back to launch-present-only when either side's address is unknown, like the
    /// RTSP plane's `authorized_launch`.
    #[test]
    fn connects_are_admitted_only_behind_a_matching_launch() {
        let owner: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let other: std::net::IpAddr = "192.168.1.99".parse().unwrap();
        // Idle host (no /launch): every connect is refused, address known or not.
        assert!(!super::accept_connect(None, Some(owner)));
        assert!(!super::accept_connect(None, None));
        // Launched with a captured owner IP: only that IP is the session peer.
        assert!(super::accept_connect(launched(Some(owner)), Some(owner)));
        assert!(!super::accept_connect(launched(Some(owner)), Some(other)));
        // Address unknown on one side → launch-present only (the pre-existing fallback).
        assert!(super::accept_connect(launched(Some(owner)), None));
        assert!(super::accept_connect(launched(None), Some(other)));
    }

    #[test]
    fn decodes_a_valid_rfi_range() {
        assert_eq!(decode_rfi_range(&rfi_msg(40, 47)), Some((40, 47)));
        assert_eq!(decode_rfi_range(&rfi_msg(5, 5)), Some((5, 5))); // single frame
    }

    #[test]
    fn rejects_short_or_nonsensical_ranges() {
        assert_eq!(decode_rfi_range(&[0x01, 0x03, 0x00, 0x00]), None); // header only, no body
        assert_eq!(decode_rfi_range(&rfi_msg(-1, 9)), None); // negative first
        assert_eq!(decode_rfi_range(&rfi_msg(9, 4)), None); // last < first
    }

    /// The termination plaintext is what turns "the host went quiet" into "the app quit" on the
    /// client. Getting the type from the wrong table, or the reason's byte order wrong, is silently
    /// ignored by the client — which reads on glass as a frozen stream or a `-1` error, not as a
    /// failed test. Pinned against moonlight-common-c `ControlStream.c`.
    #[test]
    fn termination_plaintext_wire_layout() {
        let pt = super::termination_plaintext();
        assert_eq!(pt.len(), 8);
        // The ENCRYPTED table's entry — which is the one every client reads, see below.
        assert_eq!(&pt[0..2], &0x0109u16.to_le_bytes());
        assert_eq!(&pt[2..4], &4u16.to_le_bytes());
        // The reason is BIG-endian: the client's >=6-byte "extended" branch.
        assert_eq!(&pt[4..8], &0x8003_0023u32.to_be_bytes());
    }

    /// The termination type above is only correct because of the version we advertise:
    /// moonlight-common-c sets `encryptedControlStream = APP_VERSION_AT_LEAST(7, 1, 431)`, and that
    /// alone decides whether the client reads `packetTypesGen7Enc` (termination `0x0109`) or
    /// `packetTypesGen7` (`0x0100`). Drop below that version and the message silently stops being
    /// understood — the stream would end as an error again, with nothing failing here to say why.
    #[test]
    fn advertised_version_keeps_the_client_on_the_encrypted_packet_table() {
        let q: Vec<i32> = super::super::APP_VERSION
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let at_least = q[0] > 7 || (q[0] == 7 && (q[1] > 1 || (q[1] == 1 && q[2] >= 431)));
        assert!(
            at_least,
            "APP_VERSION {} is below 7.1.431, so clients read the PLAIN packet table and \
             termination must become 0x0100",
            super::super::APP_VERSION
        );
    }

    /// The HDR-mode plaintext must match what moonlight-common-c parses: `[u16 type=0x010e]
    /// [u16 length=27][u8 enable][SS_HDR_METADATA]`, 31 bytes, all little-endian, primaries R,G,B.
    #[test]
    fn hdr_mode_plaintext_wire_layout() {
        let pt = super::hdr_mode_plaintext(true, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31); // 4 header + 1 enable + 26 metadata
        assert_eq!(&pt[0..2], &0x010eu16.to_le_bytes()); // type
        assert_eq!(&pt[2..4], &27u16.to_le_bytes()); // length = enable + metadata
        assert_eq!(pt[4], 1); // enabled
                              // Metadata starts at byte 5, R primary first (HdrMeta stores G,B,R; wire is R,G,B).
        assert_eq!(&pt[5..7], &35400u16.to_le_bytes()); // red.x
        assert_eq!(&pt[7..9], &14600u16.to_le_bytes()); // red.y
        assert_eq!(&pt[9..11], &8500u16.to_le_bytes()); // green.x
        assert_eq!(&pt[13..15], &6550u16.to_le_bytes()); // blue.x
        assert_eq!(&pt[17..19], &15635u16.to_le_bytes()); // whitePoint.x
        assert_eq!(&pt[21..23], &1000u16.to_le_bytes()); // maxDisplayLuminance (nits)
        assert_eq!(&pt[23..25], &1u16.to_le_bytes()); // minDisplayLuminance (1/10000 nit)
        assert_eq!(&pt[25..27], &1000u16.to_le_bytes()); // maxContentLightLevel (MaxCLL)
        assert_eq!(&pt[27..29], &400u16.to_le_bytes()); // maxFrameAverageLightLevel (MaxFALL)
        assert_eq!(&pt[29..31], &1000u16.to_le_bytes()); // maxFullFrameLuminance mirrors the peak
    }

    #[test]
    fn hdr_mode_plaintext_disabled_still_well_formed() {
        let pt = super::hdr_mode_plaintext(false, &pf_frame::hdr::generic_hdr10());
        assert_eq!(pt.len(), 31);
        assert_eq!(&pt[2..4], &27u16.to_le_bytes());
        assert_eq!(pt[4], 0); // disabled
    }

    /// The WP13 acceptance at filter level: under the Controller-only mask the pad passes and
    /// keyboard/pointer are counted-and-dropped — the exact test the session's injection arms
    /// run (`permitted` is what `on_receive` calls). Also pins the quiet-accounting reset.
    #[test]
    fn controller_only_mask_passes_the_pad_and_drops_keyboard_and_pointer() {
        use punktfunk_core::input::InputKind;
        use punktfunk_core::quic::{classify, GrantClass, GRANT_PRESET_CONTROLLER_ONLY};
        let mut drops = super::GrantDrops::new();
        let mask = GRANT_PRESET_CONTROLLER_ONLY;
        // Pad events inject (and pad creation with them — deny-at-setup is upstream of this).
        assert!(super::permitted(
            mask,
            classify(InputKind::GamepadButton),
            &mut drops
        ));
        // Keyboard (keys + committed text) and every pointer shape are inert.
        assert!(!super::permitted(
            mask,
            classify(InputKind::KeyDown),
            &mut drops
        ));
        assert!(!super::permitted(
            mask,
            classify(InputKind::TextInput),
            &mut drops
        ));
        assert!(!super::permitted(
            mask,
            classify(InputKind::MouseMove),
            &mut drops
        ));
        // The pen/touch plane is Pointer-class by its plane tag.
        assert!(!super::permitted(mask, GrantClass::Pointer, &mut drops));
        assert_eq!(
            drops.counts[super::GrantDrops::idx(GrantClass::Keyboard)],
            2
        );
        assert_eq!(drops.counts[super::GrantDrops::idx(GrantClass::Pointer)], 2);
        assert_eq!(drops.counts[super::GrantDrops::idx(GrantClass::Gamepad)], 0);
        // Session end logs totals once and resets for the next session.
        drops.end_of_session();
        assert_eq!(drops.counts, [0u64; 6]);
    }

    /// The session's live access state (WP13): a fingerprint with NO grants record is
    /// ungoverned (full control — the back-compat rule for existing Moonlight pairings), a
    /// record that exists governs, console edits fold in via the watch within one poll, an
    /// "expire now" edit is a past deadline through the same channel, and deleting the record
    /// returns the session to ungoverned rather than reading as a revocation (GameStream
    /// unpair ends sessions through the mgmt endpoint, not through this watch).
    #[test]
    fn session_access_resolves_folds_edits_and_expires() {
        use crate::native_pairing::{Access, NativePairing};
        use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
        use std::sync::Arc;
        let x = 0u8;
        let p = std::env::temp_dir().join(format!(
            "pf-gs-session-access-{}-{}.json",
            std::process::id(),
            &x as *const _ as usize
        ));
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(NativePairing::load_with(Some(p.clone()), None, false).unwrap());
        let now = super::super::wall_unix_now();

        // No registry wired (an AppState that never went through `serve`): ungoverned forever.
        let a = super::SessionAccess::resolve(None, "ab12".into());
        assert_eq!(a.mask, GRANT_ALL);
        assert!(!a.expired(now + 1_000_000));

        // Registry wired, no record: ungoverned — a stock Moonlight pairing keeps full control.
        let mut a = super::SessionAccess::resolve(Some(&np), "ab12".into());
        assert_eq!(a.mask, GRANT_ALL);
        assert_eq!(a.deadline, None);

        // A record created for this fingerprint (the console path) governs the live session
        // within one watch poll.
        np.add_with_access(
            "Moonlight Deck",
            "AB12", // registry keys case-insensitively, like the store
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 60),
            }),
        )
        .unwrap();
        a.poll();
        assert_eq!(a.mask, GRANT_GAMEPAD);
        assert!(!a.expired(now + 59));
        assert!(a.expired(now + 60), "the deadline second itself is expired");

        // "Expire now" is just a deadline in the past arriving through the same watch.
        np.set_access(
            "ab12",
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now - 1),
            },
        )
        .unwrap();
        a.poll();
        assert!(a.expired(now));

        // Deleting the record: back to ungoverned, session survives.
        assert!(np.remove("ab12").unwrap());
        a.poll();
        assert_eq!(a.mask, GRANT_ALL);
        assert!(!a.expired(now));
        let _ = std::fs::remove_file(&p);
    }
}
