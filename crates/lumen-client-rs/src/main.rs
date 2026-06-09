//! `lumen-client-rs` — the reference client. M3 seed mode: speak `lumen/1` (QUIC control
//! plane) to a lumen host, bring up the client `lumen_core::Session` over UDP, reassemble +
//! FEC-recover the host's deterministic test frames, and verify them byte-exactly. (M4 adds
//! VAAPI decode + wgpu present on this same skeleton.)
//!
//! Usage: `lumen-client-rs [--connect HOST:PORT]` (default `127.0.0.1:9777`).

use anyhow::{anyhow, Context, Result};
use lumen_core::config::Role;
use lumen_core::quic::{endpoint, io, Hello, Start, Welcome};
use lumen_core::transport::UdpTransport;
use lumen_core::{LumenError, Session};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let addr = std::env::args()
        .skip_while(|a| a != "--connect")
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9777".into());
    if let Err(e) = run(&addr) {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run(addr: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(session(addr))
}

async fn session(addr: &str) -> Result<()> {
    let remote: std::net::SocketAddr = addr.parse().context("--connect host:port")?;
    let ep = endpoint::client_insecure().map_err(|e| anyhow!("QUIC client endpoint: {e}"))?;
    let conn = ep
        .connect(remote, "lumen")
        .context("connect")?
        .await
        .context("QUIC handshake")?;
    tracing::info!(%remote, "lumen/1 connected");
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;

    io::write_msg(
        &mut send,
        &Hello {
            abi_version: lumen_core::ABI_VERSION,
        }
        .encode(),
    )
    .await?;
    let welcome = Welcome::decode(&io::read_msg(&mut recv).await?)
        .map_err(|e| anyhow!("Welcome decode: {e:?}"))?;
    tracing::info!(
        mode = ?welcome.mode,
        fec = ?welcome.fec,
        encrypt = welcome.encrypt,
        frames = welcome.frames,
        "session offer"
    );

    // Reserve our data-plane port, then tell the host to start.
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
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;

    // Data plane on a blocking thread (native threads only on the frame path).
    let verified = tokio::task::spawn_blocking(move || -> Result<u32> {
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())
                .context("bind data plane")?;
        let mut session =
            Session::new(cfg, Box::new(transport)).map_err(|e| anyhow!("client session: {e:?}"))?;
        let mut ok = 0u32;
        let mut mismatched = 0u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut last_rx = std::time::Instant::now();
        while ok + mismatched < expected && std::time::Instant::now() < deadline {
            match session.poll_frame() {
                Ok(frame) => {
                    last_rx = std::time::Instant::now();
                    let idx = u32::from_le_bytes(frame.data[0..4].try_into().unwrap());
                    if frame.data == test_frame(idx, frame.data.len()) {
                        ok += 1;
                    } else {
                        mismatched += 1;
                        tracing::warn!(idx, "frame content mismatch");
                    }
                }
                Err(LumenError::NoFrame) => {
                    if last_rx.elapsed() > std::time::Duration::from_secs(5) {
                        break; // stream went quiet
                    }
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
                Err(e) => return Err(anyhow!("poll_frame: {e:?}")),
            }
        }
        tracing::info!(ok, mismatched, expected, "verification complete");
        anyhow::ensure!(mismatched == 0, "{mismatched} corrupted frames");
        anyhow::ensure!(ok == expected, "received {ok}/{expected} frames");
        Ok(ok)
    })
    .await??;

    tracing::info!(
        verified,
        "lumen/1 session PASSED — GF(2^16) FEC + AES-GCM over real UDP, QUIC-negotiated"
    );
    conn.close(0u32.into(), b"done");
    Ok(())
}

/// The host's deterministic test frame (mirror of `lumen-host::m3::test_frame`).
fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    if len >= 4 {
        d[0..4].copy_from_slice(&idx.to_le_bytes());
    }
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}
