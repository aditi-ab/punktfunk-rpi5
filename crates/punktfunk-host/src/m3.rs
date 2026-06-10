//! M3 — the `punktfunk/1` native host: QUIC control plane + the hardened M1 data plane over UDP.
//! This is punktfunk's own protocol, past the GameStream compatibility layer:
//!
//! * the Welcome negotiates **GF(2¹⁶) Leopard FEC** (inexpressible in GameStream) + AES-GCM;
//! * the client's Hello requests a display mode and the host creates a **native virtual
//!   output** at exactly that size/refresh (same vdisplay backends as the GameStream path);
//! * **input arrives as QUIC datagrams** — encrypted, congestion-managed, no ENet
//!   retransmission spikes — and feeds the session's input injector;
//! * video frames carry a wall-clock `pts_ns`, so a same-host client measures the full
//!   capture→encode→FEC→UDP→reassemble latency per frame.
//!
//! `punktfunk-host m3-host [--port 9777] [--source synthetic|virtual] [--seconds 30]
//!  [--frames 300]` serves sessions back to back (one at a time — the virtual output and
//!  encoder are single-tenant); `punktfunk-client-rs --connect host:9777` is the counterpart.
//!  The data plane runs on native threads (no async on the frame path).
//!
//! Alongside video + input, a session carries **audio** (desktop Opus, 5 ms frames, host →
//! client QUIC datagrams tagged [`punktfunk_core::quic::AUDIO_MAGIC`]) and **gamepads** (client
//! GamepadButton/GamepadAxis datagrams accumulated into per-pad state for the virtual xpad;
//! force feedback flows back as [`punktfunk_core::quic::RUMBLE_MAGIC`] datagrams).
//!
//! Trust: the host serves with its persistent identity (`~/.config/punktfunk/cert.pem`, shared
//! with GameStream pairing) and logs the SHA-256 fingerprint clients pin.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::{FecConfig, FecScheme, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF};
use punktfunk_core::quic::{
    endpoint, io, Hello, PairChallenge, PairProof, PairRequest, PairResult, Reconfigure,
    Reconfigured, Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum M3Source {
    /// Deterministic test frames (protocol verification; the client byte-checks them).
    Synthetic,
    /// Real capture: virtual display at the client's requested mode → NVENC.
    Virtual,
}

pub struct M3Options {
    pub port: u16,
    pub source: M3Source,
    /// Virtual-source stream duration.
    pub seconds: u32,
    /// Synthetic-source frame count.
    pub frames: u32,
    /// Exit after this many sessions (0 = serve forever).
    pub max_sessions: u32,
    /// Only serve clients whose certificate fingerprint is in the paired set (pairing
    /// ceremonies themselves are always allowed — that's how a client gets in).
    pub require_pairing: bool,
    /// Fixed pairing PIN (tests); `None` = a fresh random 4-digit PIN per ceremony.
    pub pairing_pin: Option<String>,
    /// Paired-clients store path override (tests); `None` = the default config path.
    pub paired_store: Option<std::path::PathBuf>,
}

/// The host's paired punktfunk/1 clients: `~/.config/punktfunk/punktfunk1-paired.json`.
/// (Separate from GameStream pairing, which has its own store and ceremony.)
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PairedClients {
    clients: Vec<PairedClient>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PairedClient {
    name: String,
    /// Hex SHA-256 of the client's certificate.
    fingerprint: String,
}

/// The store plus where it persists (the path is injectable for tests).
struct PairedState {
    path: std::path::PathBuf,
    clients: PairedClients,
}

type PairedStore = Arc<std::sync::Mutex<PairedState>>;

fn paired_path() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(std::path::PathBuf::from(home).join(".config/punktfunk/punktfunk1-paired.json"))
}

fn load_paired(path: &std::path::Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_paired(state: &PairedState) -> Result<()> {
    if let Some(dir) = state.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&state.path, serde_json::to_vec_pretty(&state.clients)?)?;
    Ok(())
}

impl PairedClients {
    fn contains(&self, fp: &[u8; 32]) -> bool {
        let hex = fingerprint_hex(fp);
        self.clients.iter().any(|c| c.fingerprint == hex)
    }
}

/// Deterministic test frame: `u32 LE index` then `data[i] = idx + i` (wrapping).
pub fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    d[0..4].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn run(opts: M3Options) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    rt.block_on(serve(opts))
}

fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// The persistent listener: accept clients back to back on one endpoint. Sessions are
/// served one at a time (the virtual output + NVENC are single-tenant); a client that
/// connects mid-session waits in the accept queue. A failed session logs and the loop
/// keeps serving — only endpoint-level failures are fatal.
async fn serve(opts: M3Options) -> Result<()> {
    let identity = crate::gamestream::cert::ServerIdentity::load_or_create()
        .context("load host identity (~/.config/punktfunk)")?;
    let fingerprint = endpoint::fingerprint_of_pem(&identity.cert_pem)
        .map_err(|e| anyhow!("cert fingerprint: {e}"))?;
    let ep = endpoint::server_with_identity(
        ([0, 0, 0, 0], opts.port).into(),
        &identity.cert_pem,
        &identity.key_pem,
    )
    .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    tracing::info!(
        port = opts.port,
        source = ?opts.source,
        fingerprint = %fingerprint_hex(&fingerprint),
        "punktfunk/1 host listening (QUIC) — clients pin this fingerprint"
    );

    // One audio capturer for the whole host lifetime, handed from session to session
    // (avoids a PipeWire stream setup per session — see AudioCapSlot).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    let paired_at = match &opts.paired_store {
        Some(p) => p.clone(),
        None => paired_path()?,
    };
    let paired: PairedStore = Arc::new(std::sync::Mutex::new(PairedState {
        clients: load_paired(&paired_at),
        path: paired_at,
    }));
    if opts.require_pairing {
        let n = paired.lock().unwrap().clients.clients.len();
        tracing::info!(paired = n, "pairing required for sessions");
    }

    let mut served = 0u32;
    loop {
        let incoming = ep
            .accept()
            .await
            .ok_or_else(|| anyhow!("endpoint closed"))?;
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "QUIC accept failed");
                continue;
            }
        };
        let peer = conn.remote_address();
        tracing::info!(%peer, "punktfunk/1 client connected");
        if let Err(e) = serve_session(conn, &opts, &audio_cap, &fingerprint, &paired).await {
            tracing::warn!(%peer, error = %format!("{e:#}"), "session ended with error");
        } else {
            tracing::info!(%peer, "session complete");
        }
        served += 1;
        if opts.max_sessions != 0 && served >= opts.max_sessions {
            break;
        }
        tracing::info!("ready for the next client");
    }
    ep.wait_idle().await;
    Ok(())
}

/// The accept loop is sequential, so the control phase must be bounded — a client that
/// connects and never finishes the handshake would otherwise wedge the host for everyone.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Persistent audio-capturer slot, reused across sessions (same pattern as the GameStream
/// path): keeps one warm PipeWire capture stream instead of a connect/negotiate cycle —
/// and a daemon-side node churn — per session. (Drop now tears a capturer down cleanly.)
type AudioCapSlot = Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>;

/// Pairing needs a human in the loop (reading the PIN off the host, typing it into the
/// client), so its budget is far larger than the machine-speed session handshake.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The host side of the PIN ceremony (see `punktfunk_core::quic::pair_proof`): generate a
/// PIN, display it (log), challenge with a fresh salt, verify the client's single proof
/// attempt, and persist the client's certificate fingerprint on success.
async fn pair_ceremony(
    conn: &quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    req: PairRequest,
    host_fp: &[u8; 32],
    paired: &PairedStore,
    opts: &M3Options,
) -> Result<()> {
    let client_fp = endpoint::peer_fingerprint(conn)
        .ok_or_else(|| anyhow!("pairing requires the client to present a certificate"))?;

    let pin = opts.pairing_pin.clone().unwrap_or_else(|| {
        use rand::Rng;
        format!("{:04}", rand::thread_rng().gen_range(0..10_000u32))
    });
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    tracing::info!(
        name = %req.name,
        client = %fingerprint_hex(&client_fp),
        "PAIRING REQUEST — enter this PIN on the client: {pin}"
    );

    io::write_msg(&mut send, &PairChallenge { salt }.encode()).await?;
    let proof = tokio::time::timeout(PAIRING_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("pairing timed out waiting for the PIN proof"))??;
    let proof = PairProof::decode(&proof).map_err(|e| anyhow!("PairProof decode: {e:?}"))?;

    let expected = punktfunk_core::quic::pair_proof(&pin, &salt, &client_fp, host_fp);
    // Constant-time compare — don't leak a prefix-match timing oracle on the proof.
    let ok = proof
        .hmac
        .iter()
        .zip(expected.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0;

    if ok {
        let mut store = paired.lock().unwrap();
        let hex = fingerprint_hex(&client_fp);
        store.clients.clients.retain(|c| c.fingerprint != hex); // re-pair updates the name
        store.clients.clients.push(PairedClient {
            name: req.name.clone(),
            fingerprint: hex,
        });
        if let Err(e) = save_paired(&store) {
            tracing::error!(error = %format!("{e:#}"), "could not persist paired clients");
        }
        tracing::info!(name = %req.name, "pairing complete — client trusted");
    } else {
        tracing::warn!(name = %req.name, "pairing FAILED (wrong PIN) — fingerprint not stored");
    }
    io::write_msg(&mut send, &PairResult { ok }.encode()).await?;
    let _ = send.finish();
    // Let the result reach the client before the connection drops.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    conn.close(0u32.into(), b"pairing done");
    anyhow::ensure!(ok, "pairing rejected (wrong PIN)");
    Ok(())
}

/// One client session: handshake → input/audio planes → data plane until done/disconnect.
/// Everything torn down on return (RAII: virtual output, encoder, threads via channel close).
/// A connection whose first message is a PairRequest runs the pairing ceremony instead.
async fn serve_session(
    conn: quinn::Connection,
    opts: &M3Options,
    audio_cap: &AudioCapSlot,
    host_fp: &[u8; 32],
    paired: &PairedStore,
) -> Result<()> {
    let peer = conn.remote_address();

    // First message decides what this connection is: a pairing ceremony or a session.
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("control stream timeout"))?
        .context("accept control stream")?;
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("first message timeout"))??;
    if let Ok(req) = PairRequest::decode(&first) {
        return pair_ceremony(&conn, send, recv, req, host_fp, paired, opts).await;
    }

    let source = opts.source;
    let frames = opts.frames;
    let handshake = async {
        let hello = Hello::decode(&first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
        anyhow::ensure!(
            hello.abi_version == punktfunk_core::ABI_VERSION,
            "ABI mismatch: client {} host {}",
            hello.abi_version,
            punktfunk_core::ABI_VERSION
        );
        if opts.require_pairing {
            let known = endpoint::peer_fingerprint(&conn)
                .map(|fp| paired.lock().unwrap().clients.contains(&fp))
                .unwrap_or(false);
            anyhow::ensure!(
                known,
                "unpaired client rejected (this host requires pairing — run the PIN ceremony first)"
            );
        }
        crate::encode::validate_dimensions(
            crate::encode::Codec::H265,
            hello.mode.width,
            hello.mode.height,
        )
        .context("client-requested mode")?;

        // Reserve a UDP port for the data plane (bind, read it back, rebind in UdpTransport).
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let udp_port = probe.local_addr()?.port();
        drop(probe);

        let mut key = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut key);
        let welcome = Welcome {
            abi_version: punktfunk_core::ABI_VERSION,
            udp_port,
            mode: hello.mode,
            // The post-GameStream point of punktfunk/1: Leopard GF(2¹⁶) FEC + real encryption.
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key,
            salt: *b"pkf1",
            frames: match source {
                M3Source::Synthetic => frames,
                M3Source::Virtual => 0, // unbounded — client streams until we close
            },
        };
        io::write_msg(&mut send, &welcome.encode()).await?;

        let start = Start::decode(&io::read_msg(&mut recv).await?)
            .map_err(|e| anyhow!("Start decode: {e:?}"))?;
        Ok::<_, anyhow::Error>((hello, welcome, udp_port, start))
    };
    let (hello, welcome, udp_port, start) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (mut ctrl_send, mut ctrl_recv) = (send, recv);
    let client_udp = std::net::SocketAddr::new(peer.ip(), start.client_udp_port);
    tracing::info!(%client_udp, udp_port, mode = ?hello.mode, "handshake complete — streaming");

    // Control task: the handshake stream stays open for mid-stream renegotiation. A
    // validated Reconfigure is acked, then handed to the data-plane thread, which rebuilds
    // capture/encoder/virtual output at the new mode (the data plane itself is untouched).
    let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<punktfunk_core::Mode>();
    tokio::spawn(async move {
        let mut active = hello.mode;
        while let Ok(msg) = io::read_msg(&mut ctrl_recv).await {
            let Ok(req) = Reconfigure::decode(&msg) else {
                tracing::warn!("unknown control message — ignoring");
                continue;
            };
            let ok = crate::encode::validate_dimensions(
                crate::encode::Codec::H265,
                req.mode.width,
                req.mode.height,
            )
            .is_ok();
            if ok {
                active = req.mode;
                tracing::info!(mode = ?req.mode, "mode switch accepted");
            } else {
                tracing::warn!(mode = ?req.mode, "mode switch rejected (invalid dimensions)");
            }
            let ack = Reconfigured {
                accepted: ok,
                mode: active,
            };
            if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                break;
            }
            if ok && reconfig_tx.send(req.mode).is_err() {
                break; // data plane gone
            }
        }
    });

    // Input plane: QUIC datagrams → channel → a native injector thread (the injector owns
    // non-Send compositor state, so it lives on its own thread). The thread also owns the
    // session's virtual gamepads and sends force feedback back over `conn`. It exits when
    // the channel closes (datagram task ends on disconnect) — fresh state per session.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputEvent>();
    let input_handle = {
        let conn = conn.clone();
        std::thread::Builder::new()
            .name("punktfunk-m3-input".into())
            .spawn(move || input_thread(input_rx, conn))
            .context("spawn input thread")?
    };
    let input_conn = conn.clone();
    tokio::spawn(async move {
        let mut count = 0u64;
        while let Ok(d) = input_conn.read_datagram().await {
            if let Some(ev) = InputEvent::decode(&d) {
                count += 1;
                if input_tx.send(ev).is_err() {
                    break;
                }
            }
        }
        tracing::info!(count, "input datagram stream ended");
    });

    // Stop signal: stream duration elapsed or the client went away.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            stop.store(true, Ordering::SeqCst);
        });
    }

    // Audio plane (virtual source only — synthetic runs are protocol tests): desktop Opus
    // → host→client QUIC datagrams, on its own native thread. Best-effort on every failure
    // (no PipeWire audio, spawn error): the session continues without audio — and a spawn
    // error must NOT early-return here, the threads above are already running.
    let audio_handle = if opts.source == M3Source::Virtual {
        let conn = conn.clone();
        let stop = stop.clone();
        let cap = audio_cap.clone();
        std::thread::Builder::new()
            .name("punktfunk-m3-audio".into())
            .spawn(move || audio_thread(conn, stop, cap))
            .map_err(|e| tracing::error!(error = %e, "audio thread spawn failed — session continues without audio"))
            .ok()
    } else {
        None
    };

    // Data plane on a native thread (no async on the hot path — design invariant).
    let cfg = welcome.session_config(Role::Host);
    let source = opts.source;
    let (seconds, frames) = (opts.seconds, opts.frames);
    let mode = hello.mode;
    let stop_stream = stop.clone();
    let result: Result<()> = async {
        tokio::task::spawn_blocking(move || -> Result<()> {
            let transport =
                UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &client_udp.to_string())
                    .context("bind data plane")?;
            let mut session = Session::new(cfg, Box::new(transport))
                .map_err(|e| anyhow!("host session: {e:?}"))?;
            match source {
                M3Source::Synthetic => synthetic_stream(&mut session, frames, &stop_stream),
                M3Source::Virtual => {
                    virtual_stream(&mut session, mode, seconds, &stop_stream, &reconfig_rx)
                }
            }
        })
        .await
        .context("stream thread")??;
        // Give the client a moment to drain before the close.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Ok(())
    }
    .await;

    // Teardown on EVERY path (a failed data plane must not leave the connection open with
    // audio still streaming): stop the audio thread, close, then join both side-plane
    // threads so the next session starts fresh (closing the connection ends the datagram
    // task, which drops the input channel, which exits the input thread + its gamepads).
    stop.store(true, Ordering::SeqCst);
    conn.close(
        if result.is_ok() { 0u32 } else { 1u32 }.into(),
        if result.is_ok() { b"done" } else { b"error" },
    );
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(h) = audio_handle {
            let _ = h.join();
        }
        let _ = input_handle.join();
    })
    .await;
    result
}

/// Per-pad accumulated state: punktfunk/1 gamepad events are incremental (one button or axis
/// per datagram, see `punktfunk_core::input::gamepad`), the virtual xpad applies full frames.
#[derive(Clone, Copy, Default)]
struct PadState {
    buttons: u32,
    left_trigger: u8,
    right_trigger: u8,
    ls_x: i16,
    ls_y: i16,
    rs_x: i16,
    rs_y: i16,
}

impl PadState {
    /// Fold one wire event into the state. `false` = unknown axis id (event dropped).
    fn apply(&mut self, ev: &InputEvent) -> bool {
        if ev.kind == InputKind::GamepadButton {
            if ev.x != 0 {
                self.buttons |= ev.code;
            } else {
                self.buttons &= !ev.code;
            }
            return true;
        }
        use punktfunk_core::input::gamepad::*;
        let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let trigger = ev.x.clamp(0, 255) as u8;
        match ev.code {
            AXIS_LS_X => self.ls_x = stick,
            AXIS_LS_Y => self.ls_y = stick,
            AXIS_RS_X => self.rs_x = stick,
            AXIS_RS_Y => self.rs_y = stick,
            AXIS_LT => self.left_trigger = trigger,
            AXIS_RT => self.right_trigger = trigger,
            _ => return false,
        }
        true
    }

    fn frame(&self, index: usize, active_mask: u16) -> crate::gamestream::gamepad::GamepadFrame {
        crate::gamestream::gamepad::GamepadFrame {
            index: index as i16,
            active_mask,
            buttons: self.buttons,
            left_trigger: self.left_trigger,
            right_trigger: self.right_trigger,
            ls_x: self.ls_x,
            ls_y: self.ls_y,
            rs_x: self.rs_x,
            rs_y: self.rs_y,
        }
    }
}

/// Highest pad index addressable on the wire (`flags` field); the uinput manager caps
/// actual pad creation at its own MAX_PADS.
const MAX_WIRE_PADS: usize = 16;

/// The injector thread: open the session's input backend on first event, then inject.
/// Gamepad kinds route to the session's [`GamepadManager`](crate::inject::gamepad), with
/// force feedback pumped between events and sent back as rumble datagrams.
fn input_thread(rx: std::sync::mpsc::Receiver<InputEvent>, conn: quinn::Connection) {
    let mut injector: Option<Box<dyn crate::inject::InputInjector>> = None;
    let mut injector_broken = false;
    let mut pads = crate::inject::gamepad::GamepadManager::new();
    let mut pad_state = [PadState::default(); MAX_WIRE_PADS];
    let mut pad_mask = 0u16;
    // Rumble is idempotent state on a lossy channel (client-side overflow drops datagrams),
    // so re-send the current state of every rumbling-capable pad every 500 ms — a dropped
    // transition (including a stop) heals on the next refresh.
    let mut rumble_state = [(0u16, 0u16); MAX_WIRE_PADS];
    let mut rumble_seen = [false; MAX_WIRE_PADS];
    let mut last_refresh = std::time::Instant::now();
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(4)) {
            Ok(ev) => match ev.kind {
                InputKind::GamepadButton | InputKind::GamepadAxis => {
                    let idx = ev.flags as usize;
                    if idx >= MAX_WIRE_PADS || !pad_state[idx].apply(&ev) {
                        continue;
                    }
                    pad_mask |= 1 << idx;
                    let frame = pad_state[idx].frame(idx, pad_mask);
                    pads.handle(&crate::gamestream::gamepad::GamepadEvent::State(frame));
                }
                _ => {
                    if injector.is_none() && !injector_broken {
                        let backend = crate::inject::default_backend();
                        match crate::inject::open(backend) {
                            Ok(i) => {
                                tracing::info!(?backend, "punktfunk/1 input injector opened");
                                injector = Some(i);
                            }
                            Err(e) => {
                                // Keep running for gamepads — uinput pads work even when
                                // the pointer/keyboard backend doesn't.
                                tracing::error!(error = %format!("{e:#}"), "pointer/keyboard injection unavailable");
                                injector_broken = true;
                            }
                        }
                    }
                    if let Some(inj) = injector.as_mut() {
                        if let Err(e) = inj.inject(&ev) {
                            tracing::warn!(error = %format!("{e:#}"), "inject failed");
                        }
                    }
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Service force feedback every iteration (≤4 ms latency; games block on EVIOCSFF).
        pads.pump_rumble(|pad, low, high| {
            if let Some(s) = rumble_state.get_mut(pad as usize) {
                *s = (low, high);
                rumble_seen[pad as usize] = true;
            }
            let d = punktfunk_core::quic::encode_rumble_datagram(pad, low, high);
            let _ = conn.send_datagram(d.to_vec().into());
        });
        if last_refresh.elapsed() >= std::time::Duration::from_millis(500) {
            last_refresh = std::time::Instant::now();
            for (i, &(low, high)) in rumble_state.iter().enumerate() {
                if rumble_seen[i] {
                    let d = punktfunk_core::quic::encode_rumble_datagram(i as u16, low, high);
                    let _ = conn.send_datagram(d.to_vec().into());
                }
            }
        }
    }
}

/// The audio thread: desktop capture → Opus (48 kHz stereo, 5 ms, CBR — same tuning as the
/// GameStream path) → `AUDIO_MAGIC` datagrams. QUIC already encrypts; no extra layer.
/// The capturer comes from (and returns to) the persistent slot — see [`AudioCapSlot`].
#[cfg(target_os = "linux")]
fn audio_thread(conn: quinn::Connection, stop: Arc<AtomicBool>, audio_cap: AudioCapSlot) {
    use crate::audio::{CHANNELS, SAMPLE_RATE};
    const FRAME_MS: usize = 5;
    const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000; // 240

    let mut capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) => {
            c.drain(); // discard audio captured between sessions
            c
        }
        None => match crate::audio::open_audio_capture(CHANNELS as u32) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio unavailable — session continues without it");
                return;
            }
        },
    };
    let mut enc = match opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::LowDelay,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "opus encoder");
            *audio_cap.lock().unwrap() = Some(capturer);
            return;
        }
    };
    enc.set_bitrate(opus::Bitrate::Bits(128_000)).ok();
    enc.set_vbr(false).ok();

    let frame_len = SAMPLES_PER_FRAME * CHANNELS;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut opus_buf = vec![0u8; 1500];
    let mut seq: u32 = 0;
    let mut capture_dead = false;
    tracing::info!("punktfunk/1 audio streaming (Opus 48 kHz stereo, 5 ms datagrams)");
    'session: while !stop.load(Ordering::SeqCst) {
        let chunk = match capturer.next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture ended");
                capture_dead = true;
                break;
            }
        };
        acc.extend_from_slice(&chunk);
        while acc.len() >= frame_len {
            let frame: Vec<f32> = acc.drain(..frame_len).collect();
            let pts_ns = now_ns();
            match enc.encode_float(&frame, &mut opus_buf) {
                Ok(n) => {
                    let d =
                        punktfunk_core::quic::encode_audio_datagram(seq, pts_ns, &opus_buf[..n]);
                    if conn.send_datagram(d.into()).is_err() {
                        break 'session; // connection gone
                    }
                    seq = seq.wrapping_add(1);
                }
                Err(e) => tracing::warn!(error = %e, "opus encode"),
            }
        }
    }
    // Return the live capturer for the next session; a dead one is dropped so the next
    // session reopens fresh.
    if !capture_dead {
        *audio_cap.lock().unwrap() = Some(capturer);
    }
}

/// Stub — punktfunk/1 audio needs Linux (PipeWire capture + libopus); non-Linux dev builds
/// run sessions without it, same as when the capturer fails to open.
#[cfg(not(target_os = "linux"))]
fn audio_thread(_conn: quinn::Connection, _stop: Arc<AtomicBool>, _audio_cap: AudioCapSlot) {
    tracing::warn!(
        "punktfunk/1 audio requires Linux (PipeWire + libopus) — session continues without it"
    );
}

fn synthetic_stream(session: &mut Session, frames: u32, stop: &AtomicBool) -> Result<()> {
    let interval = std::time::Duration::from_millis(1000 / 60);
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let data = test_frame(idx, 64 * 1024);
        session
            .submit_frame(&data, now_ns(), (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// Real capture→encode→punktfunk/1: a native virtual output at the client's mode, NVENC AUs
/// stamped with the capture wall clock (the client derives per-frame pipeline latency).
///
/// `reconfig` delivers accepted mid-stream mode switches: the capture/encode pipeline is
/// rebuilt at the new mode (capturer drop tears down the PipeWire stream and, via its
/// keepalive, the virtual output) while the data-plane `session` continues untouched —
/// the rebuilt encoder opens with an IDR + in-band parameter sets.
fn virtual_stream(
    session: &mut Session,
    mode: punktfunk_core::Mode,
    seconds: u32,
    stop: &AtomicBool,
    reconfig: &std::sync::mpsc::Receiver<punktfunk_core::Mode>,
) -> Result<()> {
    let compositor = crate::vdisplay::detect().context("detect compositor")?;
    tracing::info!(?compositor, ?mode, "punktfunk/1 virtual display");
    let mut vd = crate::vdisplay::open(compositor)?;
    let (mut capturer, mut enc, mut frame, mut interval) = build_pipeline(&mut vd, mode)?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut next = std::time::Instant::now();
    let mut sent: u64 = 0;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        if let Ok(new_mode) = reconfig.try_recv() {
            tracing::info!(?new_mode, "rebuilding pipeline for mode switch");
            // Tear down in order — capture stream (and with it the virtual output) before
            // the new output appears, encoder with it. The data plane keeps running.
            drop(enc);
            drop(capturer);
            (capturer, enc, frame, interval) = build_pipeline(&mut vd, new_mode)?;
            next = std::time::Instant::now();
        }
        if let Some(f) = capturer.try_latest().context("capture")? {
            frame = f;
        }
        let capture_ns = now_ns();
        enc.submit(&frame).context("encoder submit")?;
        while let Some(au) = enc.poll().context("encoder poll")? {
            let flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            session
                .submit_frame(&au.data, capture_ns, flags)
                .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
            sent += 1;
        }
        next += interval;
        match next.checked_duration_since(std::time::Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = std::time::Instant::now(),
        }
    }
    tracing::info!(sent, "punktfunk/1 virtual stream complete");
    Ok(())
}

/// One mode's capture/encode pipeline: (capturer, encoder, first frame, frame interval).
/// Dropping the capturer tears down the PipeWire stream and the virtual output with it.
type Pipeline = (
    Box<dyn crate::capture::Capturer>,
    Box<dyn crate::encode::Encoder>,
    crate::capture::CapturedFrame,
    std::time::Duration,
);

fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
) -> Result<Pipeline> {
    let vout = vd.create(mode).context("create virtual output")?;
    let mut capturer =
        crate::capture::capture_virtual_output(vout).context("capture virtual output")?;
    capturer.set_active(true);
    let frame = capturer.next_frame().context("first frame")?;
    let enc = crate::encode::open_video(
        crate::encode::Codec::H265,
        frame.format,
        frame.width,
        frame.height,
        mode.refresh_hz,
        20_000_000,
        frame.is_cuda(),
    )
    .context("open NVENC")?;
    let interval = std::time::Duration::from_secs_f64(1.0 / mode.refresh_hz.max(1) as f64);
    Ok((capturer, enc, frame, interval))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gp(kind: InputKind, code: u32, x: i32, pad: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: pad,
        }
    }

    /// Incremental wire events accumulate into the full pad frame the virtual xpad applies.
    #[test]
    fn gamepad_accumulator() {
        use punktfunk_core::input::gamepad::*;
        let mut s = PadState::default();
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_LB, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LS_X, -32768, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_RT, 255, 0)));
        let f = s.frame(2, 0b0100);
        assert_eq!(f.buttons, BTN_A | BTN_LB);
        assert_eq!((f.ls_x, f.right_trigger), (-32768, 255));
        assert_eq!((f.index, f.active_mask), (2, 0b0100));

        // Release folds out; axis values clamp; unknown axis ids are rejected.
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 0, 0)));
        assert_eq!(s.frame(0, 1).buttons, BTN_LB);
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LT, 9_999, 0)));
        assert_eq!(s.left_trigger, 255);
        assert!(!s.apply(&gp(InputKind::GamepadAxis, 42, 1, 0)));

        // The punktfunk/1 button bits are the GameStream bits — one wire contract end to end.
        assert_eq!(BTN_A, crate::gamestream::gamepad::BTN_A);
        assert_eq!(BTN_GUIDE, crate::gamestream::gamepad::BTN_GUIDE);
        assert_eq!(BTN_DPAD_UP, crate::gamestream::gamepad::BTN_DPAD_UP);
    }

    /// Pull and byte-verify `count` synthetic frames through the C ABI connection.
    unsafe fn pull_verified(conn: *mut punktfunk_core::abi::PunktfunkConnection, count: u32) {
        use punktfunk_core::error::PunktfunkStatus;
        let mut got = 0u32;
        let mut frame = unsafe { std::mem::zeroed() };
        while got < count {
            match unsafe {
                punktfunk_core::abi::punktfunk_connection_next_au(conn, &mut frame, 2000)
            } {
                PunktfunkStatus::Ok => {
                    let data = unsafe { std::slice::from_raw_parts(frame.data, frame.len) };
                    let idx = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    assert_eq!(
                        data,
                        &test_frame(idx, data.len())[..],
                        "frame {idx} content"
                    );
                    got += 1;
                }
                PunktfunkStatus::NoFrame => continue,
                other => panic!("next_au: {other:?}"),
            }
        }
    }

    /// End-to-end through the C ABI — the exact contract platform clients (Swift) link:
    /// in-process punktfunk/1 host, `punktfunk_connect` (TOFU → pinned reconnect) →
    /// `punktfunk_connection_next_au` pulls verified frames → `punktfunk_connection_send_input`
    /// enqueues → `punktfunk_connection_close`. Three sequential sessions against ONE host
    /// process prove the persistent listener, and a wrong pin is rejected.
    #[test]
    fn c_abi_connection_roundtrip() {
        use punktfunk_core::abi::{
            punktfunk_connect, punktfunk_connection_close, punktfunk_connection_mode,
            punktfunk_connection_send_input,
        };
        use punktfunk_core::error::PunktfunkStatus;

        let host = std::thread::spawn(|| {
            run(M3Options {
                port: 19777,
                source: M3Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 3,
                require_pairing: false,
                pairing_pin: None,
                paired_store: None,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Session 1: TOFU (no pin) — observe the host fingerprint.
        let addr = std::ffi::CString::new("127.0.0.1").unwrap();
        let mut observed = [0u8; 32];
        let conn = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                observed.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn.is_null(), "punktfunk_connect failed");
        assert_ne!(observed, [0u8; 32], "fingerprint not reported");

        let (mut w, mut h, mut hz) = (0u32, 0u32, 0u32);
        assert_eq!(
            unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) },
            PunktfunkStatus::Ok
        );
        assert_eq!((w, h, hz), (1280, 720, 60));

        // Mid-stream renegotiation: request a new mode, the host acks on the control
        // stream, and punktfunk_connection_mode reflects the switch.
        assert_eq!(
            unsafe {
                punktfunk_core::abi::punktfunk_connection_request_mode(conn, 1920, 1080, 144)
            },
            PunktfunkStatus::Ok
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert_eq!(
                unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) },
                PunktfunkStatus::Ok
            );
            if (w, h, hz) == (1920, 1080, 144) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mode switch not acked (still {w}x{h}@{hz})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        unsafe { pull_verified(conn, 25) };

        let ev = punktfunk_core::input::InputEvent {
            kind: punktfunk_core::input::InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        assert_eq!(
            unsafe { punktfunk_connection_send_input(conn, &ev) },
            PunktfunkStatus::Ok
        );
        unsafe { punktfunk_connection_close(conn) };

        // Session 2 (same host process — the listener survived): pin the fingerprint.
        let conn2 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                observed.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn2.is_null(), "pinned reconnect failed");
        unsafe { pull_verified(conn2, 25) };
        unsafe { punktfunk_connection_close(conn2) };

        // Session 3: a wrong pin must be rejected by the handshake.
        let bad = [0xAAu8; 32];
        let conn3 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                bad.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(conn3.is_null(), "wrong pin must fail the handshake");

        // The host saw the rejected handshake attempt as session 3? No — a TLS-failed
        // handshake never yields a connection, so accept() is still waiting. Connect once
        // more (TOFU) to complete the host's third session and let it exit.
        let conn4 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn4.is_null());
        unsafe { pull_verified(conn4, 25) };
        unsafe { punktfunk_connection_close(conn4) };

        host.join().unwrap().unwrap();
    }

    fn test_paired_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
    }

    /// The PIN pairing ceremony + the --require-pairing gate, end to end in-process:
    /// wrong PIN rejected; right PIN pairs and returns the host fingerprint; a paired
    /// identity gets a session on a pairing-required host; an anonymous client does not.
    #[test]
    fn pairing_ceremony_and_gate() {
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let host = std::thread::spawn(|| {
            run(M3Options {
                port: 19778,
                source: M3Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 4,
                require_pairing: true,
                pairing_pin: Some("4321".into()),
                paired_store: Some(test_paired_path()),
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let timeout = std::time::Duration::from_secs(10);
        let (cert, key) = endpoint::generate_identity().unwrap();
        let identity = (cert.as_str(), key.as_str());
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // 1: wrong PIN → Crypto, nothing stored.
        let err = NativeClient::pair("127.0.0.1", 19778, identity, "0000", "imposter", timeout)
            .unwrap_err();
        assert!(
            matches!(err, punktfunk_core::PunktfunkError::Crypto),
            "{err:?}"
        );

        // 2: anonymous session on a pairing-required host → rejected (connect fails).
        assert!(
            NativeClient::connect("127.0.0.1", 19778, mode, None, None, timeout).is_err(),
            "anonymous session must be rejected"
        );

        // 3: correct PIN → paired, host fingerprint returned.
        let host_fp =
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
                .expect("pairing with the right PIN");
        assert!(test_paired_path().exists());
        let _ = std::fs::remove_file(test_paired_path()); // already loaded; tidy /tmp

        // 4: the paired identity gets a session — pinned to the ceremony's fingerprint.
        let client = NativeClient::connect(
            "127.0.0.1",
            19778,
            mode,
            Some(host_fp),
            Some((cert.clone(), key.clone())),
            timeout,
        )
        .expect("paired session");
        assert_eq!(client.host_fingerprint, host_fp);
        drop(client);

        host.join().unwrap().unwrap();
    }
}
