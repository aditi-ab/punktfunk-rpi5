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

use crate::config::{Mode, Role};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::quic::{endpoint, io, Hello, Reconfigure, Reconfigured, Start, Welcome};
use crate::session::{Frame, Session};
use crate::transport::UdpTransport;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

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

/// One Opus packet from the host's audio datagram stream (48 kHz stereo, 5 ms frames).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub pts_ns: u64,
    /// The raw Opus payload — feed it to an Opus decoder as one frame.
    pub data: Vec<u8>,
}

pub struct NativeClient {
    frames: Receiver<Frame>,
    audio: Receiver<AudioPacket>,
    rumble: Receiver<(u16, u16, u16)>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    reconfig_tx: tokio::sync::mpsc::UnboundedSender<Mode>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The currently active session mode (the Welcome's, then updated by every accepted
    /// [`NativeClient::request_mode`]).
    mode: Arc<std::sync::Mutex<Mode>>,
    /// SHA-256 fingerprint of the certificate the host actually presented. A TOFU caller
    /// (`pin = None`) persists this and passes it as the pin from then on.
    pub host_fingerprint: [u8; 32],
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
    pub fn connect(
        host: &str,
        port: u16,
        mode: Mode,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(FRAME_QUEUE);
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<(u16, u16, u16)>(RUMBLE_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (reconfig_tx, reconfig_rx) = tokio::sync::mpsc::unbounded_channel::<Mode>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(Mode, [u8; 32])>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));

        let host = host.to_string();
        let shutdown_w = shutdown.clone();
        let mode_slot_w = mode_slot.clone();
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
                    pin,
                    identity,
                    frame_tx,
                    audio_tx,
                    rumble_tx,
                    input_rx,
                    reconfig_rx,
                    ready_tx,
                    shutdown: shutdown_w,
                    mode_slot: mode_slot_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        let (negotiated, fingerprint) = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                return Err(PunktfunkError::Timeout);
            }
        };
        *mode_slot.lock().unwrap() = negotiated;
        Ok(NativeClient {
            frames: frame_rx,
            audio: audio_rx,
            rumble: rumble_rx,
            input_tx,
            reconfig_tx,
            shutdown,
            worker: Some(worker),
            mode: mode_slot,
            host_fingerprint: fingerprint,
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
        self.reconfig_tx
            .send(mode)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Pull the next reassembled, FEC-recovered access unit; [`PunktfunkError::NoFrame`] on
    /// timeout, [`PunktfunkError::Closed`]-class errors once the session ended.
    ///
    /// Plane concurrency: each pull method drains its own queue, so video, audio and
    /// rumble may each be pulled from their own thread — but at most one thread per plane
    /// (`&self` here supports the cross-plane sharing; a plane's queue is still
    /// single-consumer by contract).
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        match self.frames.recv_timeout(timeout) {
            Ok(f) => Ok(f),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next Opus audio packet; [`PunktfunkError::NoFrame`] on timeout,
    /// [`PunktfunkError::Closed`] once the session ended. Drain on a dedicated audio thread —
    /// packets arrive every 5 ms.
    pub fn next_audio(&self, timeout: Duration) -> Result<AudioPacket> {
        match self.audio.recv_timeout(timeout) {
            Ok(p) => Ok(p),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next rumble update `(pad, low, high)`; same semantics as
    /// [`NativeClient::next_audio`]. Amplitudes are 0..0xFFFF, `(0, 0)` = stop.
    pub fn next_rumble(&self, timeout: Duration) -> Result<(u16, u16, u16)> {
        match self.rumble.recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Queue one input event for delivery as a QUIC datagram.
    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        self.input_tx.send(*ev).map_err(|_| PunktfunkError::Closed)
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
    pin: Option<[u8; 32]>,
    identity: Option<(String, String)>,
    frame_tx: SyncSender<Frame>,
    audio_tx: SyncSender<AudioPacket>,
    rumble_tx: SyncSender<(u16, u16, u16)>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    reconfig_rx: tokio::sync::mpsc::UnboundedReceiver<Mode>,
    ready_tx: std::sync::mpsc::Sender<Result<(Mode, [u8; 32])>>,
    shutdown: Arc<AtomicBool>,
    mode_slot: Arc<std::sync::Mutex<Mode>>,
}

/// The worker: QUIC handshake, then the input/datagram/control tasks + the blocking
/// data-plane pump.
async fn worker_main(args: WorkerArgs) {
    let WorkerArgs {
        host,
        port,
        mode,
        pin,
        identity,
        frame_tx,
        audio_tx,
        rumble_tx,
        mut input_rx,
        mut reconfig_rx,
        ready_tx,
        shutdown,
        mode_slot,
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
            }
            .encode(),
        )
        .await?;
        let welcome = Welcome::decode(&io::read_msg(&mut recv).await?)?;

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

        let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())?;
        let session = Session::new(welcome.session_config(Role::Client), Box::new(transport))?;
        Ok::<_, PunktfunkError>((conn, session, send, recv, welcome.mode, fingerprint))
    };

    let (conn, mut session, mut ctrl_send, mut ctrl_recv, negotiated, fingerprint) =
        match setup.await {
            Ok(t) => t,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
    let _ = ready_tx.send(Ok((negotiated, fingerprint)));

    // Input task: embedder events → QUIC datagrams.
    let input_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            let _ = input_conn.send_datagram(ev.encode().to_vec().into());
        }
    });

    // Control task: the handshake stream stays open for mid-stream renegotiation. One
    // request at a time — write Reconfigure, await Reconfigured, publish the active mode.
    {
        let mode_slot = mode_slot.clone();
        tokio::spawn(async move {
            while let Some(want) = reconfig_rx.recv().await {
                if io::write_msg(&mut ctrl_send, &Reconfigure { mode: want }.encode())
                    .await
                    .is_err()
                {
                    break;
                }
                let ack = match io::read_msg(&mut ctrl_recv).await {
                    Ok(b) => match Reconfigured::decode(&b) {
                        Ok(a) => a,
                        Err(_) => break, // protocol error — stop renegotiating
                    },
                    Err(_) => break, // stream closed
                };
                if ack.accepted {
                    *mode_slot.lock().unwrap() = ack.mode;
                    tracing::info!(mode = ?ack.mode, "host accepted mode switch");
                } else {
                    tracing::warn!(requested = ?want, active = ?ack.mode, "host rejected mode switch");
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
    let pump_shutdown = shutdown.clone();
    let _ = tokio::task::spawn_blocking(move || {
        while !pump_shutdown.load(Ordering::SeqCst) {
            match session.poll_frame() {
                Ok(frame) => {
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
