//! M3 — the `lumen/1` native host: QUIC control plane + the hardened M1 data plane over UDP.
//! This is lumen's own protocol, past the GameStream compatibility layer:
//!
//! * the Welcome negotiates **GF(2¹⁶) Leopard FEC** (inexpressible in GameStream) + AES-GCM;
//! * the client's Hello requests a display mode and the host creates a **native virtual
//!   output** at exactly that size/refresh (same vdisplay backends as the GameStream path);
//! * **input arrives as QUIC datagrams** — encrypted, congestion-managed, no ENet
//!   retransmission spikes — and feeds the session's input injector;
//! * video frames carry a wall-clock `pts_ns`, so a same-host client measures the full
//!   capture→encode→FEC→UDP→reassemble latency per frame.
//!
//! `lumen-host m3-host [--port 9777] [--source synthetic|virtual] [--seconds 30]
//!  [--frames 300]` serves one session; `lumen-client-rs --connect host:9777` is the
//! counterpart. The data plane runs on native threads (no async on the frame path).

use anyhow::{anyhow, Context, Result};
use lumen_core::config::{FecConfig, FecScheme, Role};
use lumen_core::input::InputEvent;
use lumen_core::packet::{FLAG_PIC, FLAG_SOF};
use lumen_core::quic::{endpoint, io, Hello, Start, Welcome};
use lumen_core::transport::UdpTransport;
use lumen_core::Session;
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
    rt.block_on(serve_one(opts))
}

async fn serve_one(opts: M3Options) -> Result<()> {
    let ep = endpoint::server(([0, 0, 0, 0], opts.port).into())
        .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    tracing::info!(port = opts.port, source = ?opts.source, "lumen/1 host listening (QUIC)");

    let incoming = ep
        .accept()
        .await
        .ok_or_else(|| anyhow!("endpoint closed"))?;
    let conn = incoming.await.context("QUIC accept")?;
    let peer = conn.remote_address();
    tracing::info!(%peer, "lumen/1 client connected");
    let (mut send, mut recv) = conn.accept_bi().await.context("accept control stream")?;

    let hello = Hello::decode(&io::read_msg(&mut recv).await?)
        .map_err(|e| anyhow!("Hello decode: {e:?}"))?;
    anyhow::ensure!(
        hello.abi_version == lumen_core::ABI_VERSION,
        "ABI mismatch: client {} host {}",
        hello.abi_version,
        lumen_core::ABI_VERSION
    );
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
        abi_version: lumen_core::ABI_VERSION,
        udp_port,
        mode: hello.mode,
        // The post-GameStream point of lumen/1: Leopard GF(2¹⁶) FEC + real encryption.
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 20,
            max_data_per_block: 4096,
        },
        shard_payload: 1200,
        encrypt: true,
        key,
        salt: *b"lmn1",
        frames: match opts.source {
            M3Source::Synthetic => opts.frames,
            M3Source::Virtual => 0, // unbounded — client streams until we close
        },
    };
    io::write_msg(&mut send, &welcome.encode()).await?;

    let start = Start::decode(&io::read_msg(&mut recv).await?)
        .map_err(|e| anyhow!("Start decode: {e:?}"))?;
    let client_udp = std::net::SocketAddr::new(peer.ip(), start.client_udp_port);
    tracing::info!(%client_udp, udp_port, mode = ?hello.mode, "handshake complete — streaming");

    // Input plane: QUIC datagrams → channel → a native injector thread (the injector owns
    // non-Send compositor state, so it lives on its own thread).
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("lumen-m3-input".into())
        .spawn(move || input_thread(input_rx))
        .context("spawn input thread")?;
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

    // Data plane on a native thread (no async on the hot path — design invariant).
    let cfg = welcome.session_config(Role::Host);
    let source = opts.source;
    let (seconds, frames) = (opts.seconds, opts.frames);
    let mode = hello.mode;
    let stop_stream = stop.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &client_udp.to_string())
                .context("bind data plane")?;
        let mut session =
            Session::new(cfg, Box::new(transport)).map_err(|e| anyhow!("host session: {e:?}"))?;
        match source {
            M3Source::Synthetic => synthetic_stream(&mut session, frames, &stop_stream),
            M3Source::Virtual => virtual_stream(&mut session, mode, seconds, &stop_stream),
        }
    })
    .await
    .context("stream thread")??;

    // Give the client a moment to drain, then close cleanly.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    conn.close(0u32.into(), b"done");
    ep.wait_idle().await;
    Ok(())
}

/// The injector thread: open the session's input backend on first event, then inject.
fn input_thread(rx: std::sync::mpsc::Receiver<InputEvent>) {
    let mut injector: Option<Box<dyn crate::inject::InputInjector>> = None;
    while let Ok(ev) = rx.recv() {
        if injector.is_none() {
            let backend = crate::inject::default_backend();
            match crate::inject::open(backend) {
                Ok(i) => {
                    tracing::info!(?backend, "lumen/1 input injector opened");
                    injector = Some(i);
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "input injection unavailable");
                    return;
                }
            }
        }
        if let Err(e) = injector.as_mut().unwrap().inject(&ev) {
            tracing::warn!(error = %format!("{e:#}"), "inject failed");
        }
    }
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

/// Real capture→encode→lumen/1: a native virtual output at the client's mode, NVENC AUs
/// stamped with the capture wall clock (the client derives per-frame pipeline latency).
fn virtual_stream(
    session: &mut Session,
    mode: lumen_core::Mode,
    seconds: u32,
    stop: &AtomicBool,
) -> Result<()> {
    let compositor = crate::vdisplay::detect().context("detect compositor")?;
    tracing::info!(?compositor, ?mode, "lumen/1 virtual display");
    let mut vd = crate::vdisplay::open(compositor)?;
    let vout = vd.create(mode).context("create virtual output")?;
    let mut capturer =
        crate::capture::capture_virtual_output(vout).context("capture virtual output")?;
    capturer.set_active(true);

    let mut frame = capturer.next_frame().context("first frame")?;
    let mut enc = crate::encode::open_video(
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut next = std::time::Instant::now();
    let mut sent: u64 = 0;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
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
    tracing::info!(sent, "lumen/1 virtual stream complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end through the C ABI — the exact contract platform clients (Swift) link:
    /// in-process lumen/1 host, `lumen_connect` → `lumen_connection_next_au` pulls verified
    /// frames → `lumen_connection_send_input` enqueues → `lumen_connection_close`.
    #[test]
    fn c_abi_connection_roundtrip() {
        use lumen_core::abi::{
            lumen_connect, lumen_connection_close, lumen_connection_mode, lumen_connection_next_au,
            lumen_connection_send_input,
        };
        use lumen_core::error::LumenStatus;

        let host = std::thread::spawn(|| {
            run(M3Options {
                port: 19777,
                source: M3Source::Synthetic,
                seconds: 0,
                frames: 25,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        let addr = std::ffi::CString::new("127.0.0.1").unwrap();
        let conn = unsafe { lumen_connect(addr.as_ptr(), 19777, 1280, 720, 60, 10_000) };
        assert!(!conn.is_null(), "lumen_connect failed");

        let (mut w, mut h, mut hz) = (0u32, 0u32, 0u32);
        assert_eq!(
            unsafe { lumen_connection_mode(conn, &mut w, &mut h, &mut hz) },
            LumenStatus::Ok
        );
        assert_eq!((w, h, hz), (1280, 720, 60));

        let mut got = 0u32;
        let mut frame = unsafe { std::mem::zeroed() };
        while got < 25 {
            match unsafe { lumen_connection_next_au(conn, &mut frame, 2000) } {
                LumenStatus::Ok => {
                    let data = unsafe { std::slice::from_raw_parts(frame.data, frame.len) };
                    let idx = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    assert_eq!(
                        data,
                        &test_frame(idx, data.len())[..],
                        "frame {idx} content"
                    );
                    got += 1;
                }
                LumenStatus::NoFrame => continue,
                other => panic!("next_au: {other:?}"),
            }
        }

        let ev = lumen_core::input::InputEvent {
            kind: lumen_core::input::InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        assert_eq!(
            unsafe { lumen_connection_send_input(conn, &ev) },
            LumenStatus::Ok
        );

        unsafe { lumen_connection_close(conn) };
        host.join().unwrap().unwrap();
    }
}
