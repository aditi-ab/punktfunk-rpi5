//! The embeddable `punktfunk/1` client connector (M4 groundwork), behind the `quic` feature.
//!
//! [`NativeClient::connect`] runs the full client side of the protocol — QUIC handshake
//! ([`crate::quic`]), UDP data plane ([`crate::session::Session`] on a native thread), input
//! datagrams — and hands the embedder a dead-simple surface: *pull reassembled access units,
//! push input events*. This is what the platform clients (SwiftUI/VideoToolbox, Android, …)
//! link via the C ABI (`punktfunk_connect` & co. in [`crate::abi`]); `punktfunk-client-rs` is the
//! Rust-native consumer of the same flow.
//!
//! Threading: one worker thread owns a tokio runtime (QUIC control plane only — design
//! invariant) plus a blocking data-plane pump; frames cross to the embedder over a bounded
//! channel. All methods are safe to call from any single embedder thread.

use crate::config::{CompositorPref, GamepadPref, Mode, Role};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    endpoint, io, Hello, HidOutput, ProbeRequest, ProbeResult, Reconfigure, Reconfigured,
    RequestKeyframe, RichInput, Start, Welcome,
};
use crate::session::{Frame, Session};
use crate::transport::UdpTransport;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A control-stream request the embedder makes on the open handshake stream: a mode switch or a
/// speed test. One outbound channel carries both so the worker's `select!` has a single writer
/// (two `&mut ctrl_send` borrows across select branches don't compile).
enum CtrlRequest {
    Mode(Mode),
    Probe(ProbeRequest),
    Keyframe,
}

/// What the worker reports to [`NativeClient::connect`] once the handshake lands: the negotiated
/// mode, the host-resolved compositor backend, the host-resolved gamepad backend, the host's
/// certificate fingerprint, the resolved encoder bitrate (kbps), and the host↔client clock offset
/// (ns, host minus client; 0 = no skew correction / an old host that didn't answer the handshake).
type Negotiated = (Mode, CompositorPref, GamepadPref, [u8; 32], u32, i64);

/// Accumulated state of an in-flight / finished speed test. The data-plane pump folds each
/// received [`FLAG_PROBE`] access unit in; the control task records the host's [`ProbeResult`]
/// when it lands. Read (and finalized into numbers) by [`NativeClient::probe_result`].
#[derive(Default)]
struct ProbeState {
    /// A probe is in progress (set by `request_probe`, cleared by nothing — the latest one wins).
    active: bool,
    /// Probe access-unit payload bytes the client received, and their count.
    recv_bytes: u64,
    recv_packets: u32,
    /// First/last probe AU arrival — the measured receive window.
    start: Option<Instant>,
    last: Option<Instant>,
    /// The host's report ([`ProbeResult`]); present once the burst finished.
    host_bytes: u64,
    host_packets: u32,
    /// The host's `ProbeResult` arrived → the measurement is final.
    done: bool,
}

/// A finished/partial speed-test measurement, returned by [`NativeClient::probe_result`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeOutcome {
    /// The host's end-of-burst report has arrived — the numbers below are final.
    pub done: bool,
    /// Probe payload bytes / packets the client received.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Probe payload bytes / packets the host reported sending.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// The client-measured receive window (first→last probe AU), in milliseconds.
    pub elapsed_ms: u32,
    /// Measured goodput = `recv_bytes * 8 / elapsed_ms` (kilobits/second). This is the figure to
    /// drive a [`Hello::bitrate_kbps`] choice from.
    pub throughput_kbps: u32,
    /// Delivery loss = `(host_bytes - recv_bytes) / host_bytes`, as a percentage (0 if unknown).
    pub loss_pct: f32,
}

/// Frames buffered between the data-plane pump and the embedder. Small: the embedder
/// (decoder) should drain at frame rate; when it falls behind, the newest frame is dropped
/// (display freshness over completeness — FEC/keyframes recover).
const FRAME_QUEUE: usize = 16;

/// Audio packets buffered for the embedder: 64 × 5 ms = 320 ms of slack. A lagging
/// embedder drops the newest packet (the audio renderer conceals the gap).
const AUDIO_QUEUE: usize = 64;

/// Rumble updates buffered for the embedder. Overflow drops the NEWEST update (same
/// `try_send` discipline as the other planes) — the host re-sends rumble state
/// periodically, so a dropped transition (including a stop) heals within ~500 ms.
const RUMBLE_QUEUE: usize = 16;

/// HID-output (DualSense lightbar / player LEDs / adaptive triggers) buffered for the embedder.
/// Same overflow discipline as rumble; the host re-sends on the next feedback change.
const HIDOUT_QUEUE: usize = 32;

/// One Opus packet from the host's audio datagram stream (48 kHz stereo, 5 ms frames).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub pts_ns: u64,
    /// The raw Opus payload — feed it to an Opus decoder as one frame.
    pub data: Vec<u8>,
}

pub struct NativeClient {
    // Each plane's receiver sits behind its own mutex so `NativeClient` is `Sync` and Rust
    // embedders can share one `Arc<NativeClient>` across their plane threads (the same
    // one-thread-per-plane contract the C ABI documents — the lock is uncontended there,
    // and two threads racing one plane now serialize instead of being undefined).
    frames: Mutex<Receiver<Frame>>,
    audio: Mutex<Receiver<AudioPacket>>,
    rumble: Mutex<Receiver<(u16, u16, u16)>>,
    /// Inbound DualSense feedback (lightbar / player LEDs / adaptive triggers) — 0xCD datagrams.
    hidout: Mutex<Receiver<HidOutput>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// Outbound mic frames `(seq, pts_ns, opus)` → encoded as 0xCB datagrams by the worker.
    mic_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64, Vec<u8>)>,
    /// Outbound rich input (DualSense touchpad / motion) → 0xCC datagrams by the worker.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<RichInput>,
    /// Outbound control-stream requests (mode switch, speed test) → the worker's control task.
    ctrl_tx: tokio::sync::mpsc::UnboundedSender<CtrlRequest>,
    /// Speed-test accumulator, shared with the data-plane pump + control task.
    probe: Arc<Mutex<ProbeState>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The currently active session mode (the Welcome's, then updated by every accepted
    /// [`NativeClient::request_mode`]).
    mode: Arc<std::sync::Mutex<Mode>>,
    /// SHA-256 fingerprint of the certificate the host actually presented. A TOFU caller
    /// (`pin = None`) persists this and passes it as the pin from then on.
    pub host_fingerprint: [u8; 32],
    /// The compositor backend the host actually resolved for this session ([`Welcome::compositor`]).
    /// `Auto` = an older host that didn't say. Clients use it for compositor-specific behavior (e.g.
    /// drawing a client-side cursor by default on gamescope, whose capture carries no cursor).
    pub resolved_compositor: CompositorPref,
    /// The virtual gamepad backend the host actually resolved ([`Welcome::gamepad`]).
    /// `Auto` = an older host that didn't say (assume X-Box 360, no DualSense feedback).
    pub resolved_gamepad: GamepadPref,
    /// The encoder bitrate the host actually configured ([`Welcome::bitrate_kbps`], kbps): our
    /// requested rate clamped to the host's range, or its default if we requested `0`. `0` = an
    /// older host that didn't report it.
    pub resolved_bitrate_kbps: u32,
    /// Host clock minus client clock (ns), from the connect-time skew handshake. Add it to a local
    /// receive/present timestamp to express it in the host's capture clock (the AU `pts_ns`), making
    /// glass-to-glass latency valid across machines. `0` = no correction (an old host that didn't
    /// answer, or genuinely synced clocks).
    pub clock_offset_ns: i64,
}

impl NativeClient {
    /// Connect to a `punktfunk/1` host and start the session at (up to) `mode`. Blocks until the
    /// handshake completes or `timeout` elapses.
    ///
    /// `pin`: expected SHA-256 of the host's certificate. `Some` and the host presents
    /// anything else → the handshake is rejected ([`PunktfunkError::Crypto`]). `None` = trust on
    /// first use; check [`NativeClient::host_fingerprint`] after connecting.
    ///
    /// `identity`: this client's persistent self-signed identity (PEM cert + PKCS#8 key,
    /// see [`endpoint::generate_identity`]), presented via TLS client auth so a host can
    /// recognize a paired client. `None` = anonymous (rejected by hosts requiring pairing).
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        host: &str,
        port: u16,
        mode: Mode,
        compositor: CompositorPref,
        gamepad: GamepadPref,
        bitrate_kbps: u32,
        launch: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(FRAME_QUEUE);
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<(u16, u16, u16)>(RUMBLE_QUEUE);
        let (hidout_tx, hidout_rx) = std::sync::mpsc::sync_channel::<HidOutput>(HIDOUT_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, u64, Vec<u8>)>();
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<RichInput>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel::<CtrlRequest>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));
        let probe = Arc::new(Mutex::new(ProbeState::default()));

        let host = host.to_string();
        let shutdown_w = shutdown.clone();
        let mode_slot_w = mode_slot.clone();
        let probe_w = probe.clone();
        let worker = std::thread::Builder::new()
            .name("punktfunk-client".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(PunktfunkError::Io(e)));
                        return;
                    }
                };
                rt.block_on(worker_main(WorkerArgs {
                    host,
                    port,
                    mode,
                    compositor,
                    gamepad,
                    bitrate_kbps,
                    launch,
                    pin,
                    identity,
                    frame_tx,
                    audio_tx,
                    rumble_tx,
                    hidout_tx,
                    input_rx,
                    mic_rx,
                    rich_input_rx,
                    ctrl_rx,
                    ready_tx,
                    shutdown: shutdown_w,
                    mode_slot: mode_slot_w,
                    probe: probe_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        let (
            negotiated,
            resolved_compositor,
            resolved_gamepad,
            fingerprint,
            resolved_bitrate_kbps,
            clock_offset_ns,
        ) = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                return Err(PunktfunkError::Timeout);
            }
        };
        *mode_slot.lock().unwrap() = negotiated;
        Ok(NativeClient {
            frames: Mutex::new(frame_rx),
            audio: Mutex::new(audio_rx),
            rumble: Mutex::new(rumble_rx),
            hidout: Mutex::new(hidout_rx),
            input_tx,
            mic_tx,
            rich_input_tx,
            ctrl_tx,
            probe,
            shutdown,
            worker: Some(worker),
            mode: mode_slot,
            host_fingerprint: fingerprint,
            resolved_compositor,
            resolved_gamepad,
            resolved_bitrate_kbps,
            clock_offset_ns,
        })
    }

    /// Run the PIN pairing ceremony against a host: connect (trust-on-first-use — the PIN
    /// proof is what authenticates the certificates), prove knowledge of the PIN the host
    /// is displaying, and return the host's now-verified fingerprint for pinning. The host
    /// persists this client's fingerprint in its paired set.
    ///
    /// `identity` is this client's persistent PEM identity (cert, key) — the same one
    /// later passed to [`NativeClient::connect`]; `pin` is what the user read off the host
    /// (its log / UI); `name` is the label the host stores.
    pub fn pair(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<[u8; 32]> {
        use crate::quic::{pake, PairChallenge, PairProof, PairRequest, PairResult};

        let client_fp = endpoint::fingerprint_of_pem(identity.0)
            .map_err(|_| PunktfunkError::InvalidArg("client cert pem"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(PunktfunkError::Io)?;
        let pin = pin.to_string();
        let name = name.to_string();
        let remote: std::net::SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;

        rt.block_on(async move {
            // The quinn endpoint must be created inside the runtime (it spawns its driver).
            let (ep, observed) = endpoint::client_pinned_with_identity(None, Some(identity));
            let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

            // The SPAKE2 exchange over an already-open bi-stream; never closes the conn (the
            // caller does, then flushes), so any early exit still lets the host see the close.
            let exchange = |conn: quinn::Connection, host_fp: [u8; 32]| async move {
                let (mut send, mut recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                // SPAKE2 as A, binding our fingerprint + the host cert we observed (TOFU).
                let (pake, spake_a) = pake::start(true, &pin, &client_fp, &host_fp);
                io::write_msg(&mut send, &PairRequest { name, spake_a }.encode()).await?;
                let challenge = PairChallenge::decode(&io::read_msg(&mut recv).await?)?;
                let confirms = pake.finish(&challenge.spake_b)?;
                // The host's confirmation proves it reached the same key (right PIN, same
                // certs) — only then do we pin it and send our own confirmation.
                if !pake::verify(&confirms.host, &challenge.confirm) {
                    return Err(PunktfunkError::Crypto); // wrong PIN or MITM
                }
                io::write_msg(
                    &mut send,
                    &PairProof {
                        confirm: confirms.client,
                    }
                    .encode(),
                )
                .await?;
                let result = PairResult::decode(&io::read_msg(&mut recv).await?)?;
                if result.ok {
                    Ok(host_fp)
                } else {
                    Err(PunktfunkError::Crypto) // host rejected post-confirm
                }
            };

            let ceremony = async {
                let conn = ep
                    .connect(remote, "punktfunk")
                    .map_err(|_| PunktfunkError::InvalidArg("connect"))?
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                let host_fp = observed.lock().unwrap().ok_or(PunktfunkError::Crypto)?;
                let outcome = exchange(conn.clone(), host_fp).await;
                // Always tell the host we're done so it never blocks at its read — code 0 on
                // success, 1 on a refused/aborted ceremony.
                let code: u32 = if outcome.is_ok() { 0 } else { 1 };
                conn.close(code.into(), b"pair done");
                outcome
            };
            let outcome = tokio::time::timeout(timeout, ceremony)
                .await
                .map_err(|_| PunktfunkError::Timeout)?;
            // Flush the CONNECTION_CLOSE before the runtime is dropped — otherwise the host
            // may never see it and would block at its read for the full pairing timeout.
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
            outcome
        })
    }

    /// The currently active session mode — the Welcome's, until an accepted
    /// [`NativeClient::request_mode`] switches it.
    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    /// Ask the host to switch the live session to `mode` (no reconnect). Non-blocking:
    /// the request is queued; on acceptance the stream continues at the new mode (next
    /// frames open with an IDR carrying new parameter sets) and [`NativeClient::mode`]
    /// reflects it. A rejected request leaves the session unchanged.
    pub fn request_mode(&self, mode: Mode) -> Result<()> {
        self.ctrl_tx
            .send(CtrlRequest::Mode(mode))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Ask the host's encoder to emit a fresh IDR keyframe now (client recovery on a stalled
    /// decode). Non-blocking, fire-and-forget — the recovered keyframe is the only ack. The
    /// caller should throttle (the decode stays wedged across several frames until the IDR
    /// lands, so requesting on every frame would flood the control stream).
    pub fn request_keyframe(&self) -> Result<()> {
        self.ctrl_tx
            .send(CtrlRequest::Keyframe)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Start a bandwidth speed test: ask the host to burst filler over the data plane at
    /// `target_kbps` of goodput for `duration_ms`, *briefly pausing video*. Non-blocking — the
    /// measurement accumulates in the background; poll [`NativeClient::probe_result`] until its
    /// `done` flag is set. Starting a probe resets any prior measurement. The host clamps both
    /// fields (≤ 3 Gbps, ≤ 5 s).
    pub fn request_probe(&self, target_kbps: u32, duration_ms: u32) -> Result<()> {
        // Reset the accumulator so a fresh run doesn't blend into the previous one.
        *self.probe.lock().unwrap() = ProbeState {
            active: true,
            ..Default::default()
        };
        self.ctrl_tx
            .send(CtrlRequest::Probe(ProbeRequest {
                target_kbps,
                duration_ms,
            }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Read the current speed-test measurement (partial until `done`, final once the host's
    /// end-of-burst report lands). Derives goodput + loss from the accumulated probe bytes.
    pub fn probe_result(&self) -> ProbeOutcome {
        let p = self.probe.lock().unwrap();
        let elapsed_ms = match (p.start, p.last) {
            (Some(s), Some(l)) => l.duration_since(s).as_millis() as u32,
            _ => 0,
        };
        // bytes × 8 / ms = kilobits/second.
        let throughput_kbps = if elapsed_ms > 0 {
            (p.recv_bytes.saturating_mul(8) / elapsed_ms as u64) as u32
        } else {
            0
        };
        let loss_pct = if p.host_bytes > 0 {
            p.host_bytes.saturating_sub(p.recv_bytes) as f64 / p.host_bytes as f64 * 100.0
        } else {
            0.0
        } as f32;
        ProbeOutcome {
            done: p.done,
            recv_bytes: p.recv_bytes,
            recv_packets: p.recv_packets,
            host_bytes: p.host_bytes,
            host_packets: p.host_packets,
            elapsed_ms,
            throughput_kbps,
            loss_pct,
        }
    }

    /// Pull the next reassembled, FEC-recovered access unit; [`PunktfunkError::NoFrame`] on
    /// timeout, [`PunktfunkError::Closed`]-class errors once the session ended.
    ///
    /// Plane concurrency: each pull method drains its own queue, so video, audio and
    /// rumble may each be pulled from their own thread — but at most one thread per plane
    /// (`&self` here supports the cross-plane sharing; a plane's queue is still
    /// single-consumer by contract).
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        match self.frames.lock().unwrap().recv_timeout(timeout) {
            Ok(f) => Ok(f),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next Opus audio packet; [`PunktfunkError::NoFrame`] on timeout,
    /// [`PunktfunkError::Closed`] once the session ended. Drain on a dedicated audio thread —
    /// packets arrive every 5 ms.
    pub fn next_audio(&self, timeout: Duration) -> Result<AudioPacket> {
        match self.audio.lock().unwrap().recv_timeout(timeout) {
            Ok(p) => Ok(p),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next rumble update `(pad, low, high)`; same semantics as
    /// [`NativeClient::next_audio`]. Amplitudes are 0..0xFFFF, `(0, 0)` = stop.
    pub fn next_rumble(&self, timeout: Duration) -> Result<(u16, u16, u16)> {
        match self.rumble.lock().unwrap().recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next DualSense HID-output feedback event (lightbar / player LEDs / adaptive
    /// trigger) the host's virtual pad received from a game; same timeout/closed semantics as
    /// [`NativeClient::next_rumble`]. Replay it on a real DualSense (e.g. via the platform's
    /// `GCDualSenseAdaptiveTrigger` API). Only the DualSense host backend emits these.
    pub fn next_hidout(&self, timeout: Duration) -> Result<HidOutput> {
        match self.hidout.lock().unwrap().recv_timeout(timeout) {
            Ok(h) => Ok(h),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Queue one input event for delivery as a QUIC datagram.
    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        self.input_tx.send(*ev).map_err(|_| PunktfunkError::Closed)
    }

    /// Queue one Opus mic frame for delivery as a 0xCB uplink datagram (the inverse of
    /// [`next_audio`](Self::next_audio)). `seq`/`pts_ns` are the caller's own counters (the host
    /// uses them only for diagnostics). The host decodes it into a virtual microphone source.
    /// Best-effort — like every datagram, it's dropped under loss; no retransmit.
    pub fn send_mic(&self, seq: u32, pts_ns: u64, opus: Vec<u8>) -> Result<()> {
        self.mic_tx
            .send((seq, pts_ns, opus))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Queue one rich input event (DualSense touchpad contact or motion sample) for delivery as a
    /// 0xCC datagram. The host applies it to its virtual DualSense pad. Best-effort, dropped under
    /// loss like every datagram. No-op unless the host runs the DualSense gamepad backend.
    pub fn send_rich_input(&self, rich: RichInput) -> Result<()> {
        self.rich_input_tx
            .send(rich)
            .map_err(|_| PunktfunkError::Closed)
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

struct WorkerArgs {
    host: String,
    port: u16,
    mode: Mode,
    compositor: CompositorPref,
    gamepad: GamepadPref,
    bitrate_kbps: u32,
    launch: Option<String>,
    pin: Option<[u8; 32]>,
    identity: Option<(String, String)>,
    frame_tx: SyncSender<Frame>,
    audio_tx: SyncSender<AudioPacket>,
    rumble_tx: SyncSender<(u16, u16, u16)>,
    hidout_tx: SyncSender<HidOutput>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    mic_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, u64, Vec<u8>)>,
    rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<RichInput>,
    ctrl_rx: tokio::sync::mpsc::UnboundedReceiver<CtrlRequest>,
    ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    shutdown: Arc<AtomicBool>,
    mode_slot: Arc<std::sync::Mutex<Mode>>,
    probe: Arc<Mutex<ProbeState>>,
}

/// The worker: QUIC handshake, then the input/datagram/control tasks + the blocking
/// data-plane pump.
async fn worker_main(args: WorkerArgs) {
    let WorkerArgs {
        host,
        port,
        mode,
        compositor,
        gamepad,
        bitrate_kbps,
        launch,
        pin,
        identity,
        frame_tx,
        audio_tx,
        rumble_tx,
        hidout_tx,
        mut input_rx,
        mut mic_rx,
        mut rich_input_rx,
        mut ctrl_rx,
        ready_tx,
        shutdown,
        mode_slot,
        probe,
    } = args;
    let setup = async {
        let remote: std::net::SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;
        let (ep, observed) = endpoint::client_pinned_with_identity(
            pin,
            identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        );
        let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        let conn = ep
            .connect(remote, "punktfunk")
            .map_err(|_| PunktfunkError::InvalidArg("connect"))?
            .await
            .map_err(|e| {
                // A pin mismatch surfaces as a TLS failure; report it as a crypto error so
                // the embedder can distinguish "wrong host identity" from plain IO trouble.
                let fp_mismatch = pin.is_some()
                    && observed.lock().unwrap().map(|fp| Some(fp) != pin) == Some(true);
                if fp_mismatch {
                    PunktfunkError::Crypto
                } else {
                    PunktfunkError::Io(std::io::Error::other(e.to_string()))
                }
            })?;
        let fingerprint = observed.lock().unwrap().unwrap_or([0u8; 32]);
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

        io::write_msg(
            &mut send,
            &Hello {
                abi_version: crate::ABI_VERSION,
                mode,
                compositor,
                gamepad,
                bitrate_kbps,
                // No device name yet: the connect ABI has no name parameter (pairing does). The
                // host falls back to a fingerprint-derived label in its pending-approval list.
                name: None,
                // Library id to launch this session, if the embedder asked for one.
                launch: launch.clone(),
            }
            .encode(),
        )
        .await?;
        let welcome = Welcome::decode(&io::read_msg(&mut recv).await?)?;
        if welcome.compositor != CompositorPref::Auto {
            tracing::info!(
                compositor = welcome.compositor.as_str(),
                "host resolved compositor"
            );
        }
        if welcome.gamepad != GamepadPref::Auto {
            tracing::info!(
                gamepad = welcome.gamepad.as_str(),
                "host resolved gamepad backend"
            );
        }

        // Reserve our data-plane port, then start the host.
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let udp_port = probe.local_addr()?.port();
        drop(probe);
        io::write_msg(
            &mut send,
            &Start {
                client_udp_port: udp_port,
            }
            .encode(),
        )
        .await?;

        // Wall-clock skew handshake on the control stream (before the session's control task takes
        // it): align our clock to the host's so the embedder can express receive/present instants in
        // the host's capture clock (the AU `pts_ns`). 0 ⇒ an old host that didn't answer (shared-clock
        // assumption, as before). This is the substrate for glass-to-glass present-time measurement.
        let clock_offset_ns = match crate::quic::clock_sync(&mut send, &mut recv).await {
            Some(skew) => {
                tracing::info!(
                    offset_ns = skew.offset_ns,
                    rtt_us = skew.rtt_ns / 1000,
                    rounds = skew.rounds,
                    "clock skew estimated (host-client)"
                );
                skew.offset_ns
            }
            None => 0,
        };

        let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())?;
        // Hole-punch the host's data port so video traverses a NAT / stateful inter-VLAN firewall
        // (control + side planes ride the client-initiated QUIC; the raw video UDP needs the client
        // to open the path first). Stops with the session via the shared shutdown flag.
        if let Ok(sock) = transport.try_clone_socket() {
            crate::transport::spawn_data_punch(sock, shutdown.clone());
        }
        let session = Session::new(welcome.session_config(Role::Client), Box::new(transport))?;
        Ok::<_, PunktfunkError>((
            conn,
            session,
            send,
            recv,
            welcome.mode,
            welcome.compositor,
            welcome.gamepad,
            fingerprint,
            welcome.bitrate_kbps,
            clock_offset_ns,
        ))
    };

    let (
        conn,
        mut session,
        mut ctrl_send,
        mut ctrl_recv,
        negotiated,
        resolved_compositor,
        resolved_gamepad,
        fingerprint,
        resolved_bitrate_kbps,
        clock_offset_ns,
    ) = match setup.await {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _ = ready_tx.send(Ok((
        negotiated,
        resolved_compositor,
        resolved_gamepad,
        fingerprint,
        resolved_bitrate_kbps,
        clock_offset_ns,
    )));

    // Input task: embedder events → QUIC datagrams.
    let input_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            let _ = input_conn.send_datagram(ev.encode().to_vec().into());
        }
    });

    // Mic task: embedder Opus mic frames → 0xCB uplink datagrams (best-effort, dropped on loss).
    let mic_conn = conn.clone();
    tokio::spawn(async move {
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d.into());
        }
    });

    // Rich-input task: embedder DualSense touchpad / motion → 0xCC uplink datagrams.
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(rich) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(rich.encode().into());
        }
    });

    // Control task: the handshake stream stays open for mid-stream renegotiation + speed tests.
    // Outbound requests (mode switch, probe) and inbound replies (Reconfigured, ProbeResult) are
    // multiplexed with `select!`; a single outbound channel (`ctrl_rx`) keeps one writer so the
    // two `&mut ctrl_send` borrows don't collide across branches.
    {
        let mode_slot = mode_slot.clone();
        let probe = probe.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    req = ctrl_rx.recv() => {
                        let Some(req) = req else { break }; // client dropped
                        let bytes = match req {
                            CtrlRequest::Mode(m) => Reconfigure { mode: m }.encode(),
                            CtrlRequest::Probe(p) => p.encode(),
                            CtrlRequest::Keyframe => RequestKeyframe.encode(),
                        };
                        if io::write_msg(&mut ctrl_send, &bytes).await.is_err() {
                            break;
                        }
                    }
                    msg = io::read_msg(&mut ctrl_recv) => {
                        let Ok(msg) = msg else { break }; // stream closed
                        if let Ok(ack) = Reconfigured::decode(&msg) {
                            if ack.accepted {
                                *mode_slot.lock().unwrap() = ack.mode;
                                tracing::info!(mode = ?ack.mode, "host accepted mode switch");
                            } else {
                                tracing::warn!(active = ?ack.mode, "host rejected mode switch");
                            }
                        } else if let Ok(result) = ProbeResult::decode(&msg) {
                            let mut p = probe.lock().unwrap();
                            p.host_bytes = result.bytes_sent;
                            p.host_packets = result.packets_sent;
                            p.done = true;
                            tracing::info!(
                                bytes_sent = result.bytes_sent,
                                packets_sent = result.packets_sent,
                                duration_ms = result.duration_ms,
                                "speed-test probe result"
                            );
                        } else {
                            tracing::warn!("unknown control message — ignoring");
                        }
                    }
                }
            }
        });
    }

    // Datagram demux: host → client audio/rumble (try_send: a lagging embedder drops the
    // newest packet rather than backing up the QUIC receive path).
    let dgram_conn = conn.clone();
    tokio::spawn(async move {
        while let Ok(d) = dgram_conn.read_datagram().await {
            match d.first() {
                Some(&crate::quic::AUDIO_MAGIC) => {
                    if let Some((seq, pts_ns, opus)) = crate::quic::decode_audio_datagram(&d) {
                        let _ = audio_tx.try_send(AudioPacket {
                            seq,
                            pts_ns,
                            data: opus.to_vec(),
                        });
                    }
                }
                Some(&crate::quic::RUMBLE_MAGIC) => {
                    if let Some(r) = crate::quic::decode_rumble_datagram(&d) {
                        let _ = rumble_tx.try_send(r);
                    }
                }
                Some(&crate::quic::HIDOUT_MAGIC) => {
                    if let Some(h) = HidOutput::decode(&d) {
                        let _ = hidout_tx.try_send(h);
                    }
                }
                _ => {} // unknown tag — a newer host; ignore
            }
        }
    });

    // Watch for connection close → stop the pump.
    {
        let shutdown = shutdown.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    // Data-plane pump on a blocking thread: poll the session, hand frames to the embedder.
    // try_send drops the newest frame when the embedder lags (freshness over completeness).
    // Speed-test filler ([`FLAG_PROBE`]) is folded into the probe accumulator instead of the
    // decoder queue — it isn't video.
    let pump_shutdown = shutdown.clone();
    let pump_probe = probe.clone();
    let _ = tokio::task::spawn_blocking(move || {
        while !pump_shutdown.load(Ordering::SeqCst) {
            match session.poll_frame() {
                Ok(frame) => {
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        let mut p = pump_probe.lock().unwrap();
                        if p.active {
                            let now = Instant::now();
                            p.start.get_or_insert(now);
                            p.last = Some(now);
                            p.recv_bytes += frame.data.len() as u64;
                            p.recv_packets += 1;
                        }
                        continue; // not video — never enqueue for the decoder
                    }
                    let _ = frame_tx.try_send(frame);
                }
                Err(PunktfunkError::NoFrame) => {
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(_) => break,
            }
        }
    })
    .await;

    conn.close(0u32.into(), b"client closed");
}
