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
use crate::quic::{endpoint, io, Hello, Start, Welcome};
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
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The host-confirmed session mode (from the Welcome).
    pub mode: Mode,
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
    pub fn connect(
        host: &str,
        port: u16,
        mode: Mode,
        pin: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(FRAME_QUEUE);
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<(u16, u16, u16)>(RUMBLE_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(Mode, [u8; 32])>>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let host = host.to_string();
        let shutdown_w = shutdown.clone();
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
                    frame_tx,
                    audio_tx,
                    rumble_tx,
                    input_rx,
                    ready_tx,
                    shutdown: shutdown_w,
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
        Ok(NativeClient {
            frames: frame_rx,
            audio: audio_rx,
            rumble: rumble_rx,
            input_tx,
            shutdown,
            worker: Some(worker),
            mode: negotiated,
            host_fingerprint: fingerprint,
        })
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
    frame_tx: SyncSender<Frame>,
    audio_tx: SyncSender<AudioPacket>,
    rumble_tx: SyncSender<(u16, u16, u16)>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    ready_tx: std::sync::mpsc::Sender<Result<(Mode, [u8; 32])>>,
    shutdown: Arc<AtomicBool>,
}

/// The worker: QUIC handshake, then the input/datagram tasks + the blocking data-plane pump.
async fn worker_main(args: WorkerArgs) {
    let WorkerArgs {
        host,
        port,
        mode,
        pin,
        frame_tx,
        audio_tx,
        rumble_tx,
        mut input_rx,
        ready_tx,
        shutdown,
    } = args;
    let setup = async {
        let remote: std::net::SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;
        let (ep, observed) = endpoint::client_pinned(pin);
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
        Ok::<_, PunktfunkError>((conn, session, welcome.mode, fingerprint))
    };

    let (conn, mut session, negotiated, fingerprint) = match setup.await {
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
