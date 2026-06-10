//! The embeddable `lumen/1` client connector (M4 groundwork), behind the `quic` feature.
//!
//! [`NativeClient::connect`] runs the full client side of the protocol — QUIC handshake
//! ([`crate::quic`]), UDP data plane ([`crate::session::Session`] on a native thread), input
//! datagrams — and hands the embedder a dead-simple surface: *pull reassembled access units,
//! push input events*. This is what the platform clients (SwiftUI/VideoToolbox, Android, …)
//! link via the C ABI (`lumen_connect` & co. in [`crate::abi`]); `lumen-client-rs` is the
//! Rust-native consumer of the same flow.
//!
//! Threading: one worker thread owns a tokio runtime (QUIC control plane only — design
//! invariant) plus a blocking data-plane pump; frames cross to the embedder over a bounded
//! channel. All methods are safe to call from any single embedder thread.

use crate::config::{Mode, Role};
use crate::error::{LumenError, Result};
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

pub struct NativeClient {
    frames: Receiver<Frame>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The host-confirmed session mode (from the Welcome).
    pub mode: Mode,
}

impl NativeClient {
    /// Connect to a `lumen/1` host and start the session at (up to) `mode`. Blocks until the
    /// handshake completes or `timeout` elapses.
    pub fn connect(host: &str, port: u16, mode: Mode, timeout: Duration) -> Result<NativeClient> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(FRAME_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Mode>>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let host = host.to_string();
        let shutdown_w = shutdown.clone();
        let worker = std::thread::Builder::new()
            .name("lumen-client".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(LumenError::Io(e)));
                        return;
                    }
                };
                rt.block_on(worker_main(
                    host, port, mode, frame_tx, input_rx, ready_tx, shutdown_w,
                ));
            })
            .map_err(LumenError::Io)?;

        let negotiated = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                return Err(LumenError::Timeout);
            }
        };
        Ok(NativeClient {
            frames: frame_rx,
            input_tx,
            shutdown,
            worker: Some(worker),
            mode: negotiated,
        })
    }

    /// Pull the next reassembled, FEC-recovered access unit; [`LumenError::NoFrame`] on
    /// timeout, [`LumenError::Closed`]-class errors once the session ended.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        match self.frames.recv_timeout(timeout) {
            Ok(f) => Ok(f),
            Err(RecvTimeoutError::Timeout) => Err(LumenError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(LumenError::Closed),
        }
    }

    /// Queue one input event for delivery as a QUIC datagram.
    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        self.input_tx.send(*ev).map_err(|_| LumenError::Closed)
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

/// The worker: QUIC handshake, then the input task + the blocking data-plane pump.
async fn worker_main(
    host: String,
    port: u16,
    mode: Mode,
    frame_tx: SyncSender<Frame>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    ready_tx: std::sync::mpsc::Sender<Result<Mode>>,
    shutdown: Arc<AtomicBool>,
) {
    let setup = async {
        let remote: std::net::SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|_| LumenError::InvalidArg("host:port"))?;
        let ep = endpoint::client_insecure()
            .map_err(|e| LumenError::Io(std::io::Error::other(e.to_string())))?;
        let conn = ep
            .connect(remote, "lumen")
            .map_err(|_| LumenError::InvalidArg("connect"))?
            .await
            .map_err(|e| LumenError::Io(std::io::Error::other(e.to_string())))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| LumenError::Io(std::io::Error::other(e.to_string())))?;

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
        Ok::<_, LumenError>((conn, session, welcome.mode))
    };

    let (conn, mut session, negotiated) = match setup.await {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _ = ready_tx.send(Ok(negotiated));

    // Input task: embedder events → QUIC datagrams.
    let input_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            let _ = input_conn.send_datagram(ev.encode().to_vec().into());
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
                Err(LumenError::NoFrame) => {
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(_) => break,
            }
        }
    })
    .await;

    conn.close(0u32.into(), b"client closed");
}
