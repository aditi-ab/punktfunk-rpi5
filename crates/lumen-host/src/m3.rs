//! M3 seed — the `lumen/1` native host: QUIC control plane (lumen-core `quic`) + the hardened
//! M1 data plane over real UDP. This is the first end-to-end run of lumen's own protocol,
//! past the GameStream compatibility layer: the Welcome negotiates **GF(2¹⁶) Leopard FEC**
//! (positively not expressible in GameStream) and AES-GCM with per-direction salts.
//!
//! `lumen-host m3-host [--port 9777] [--frames 300]` serves one session: handshake on QUIC,
//! then a native thread streams deterministic, verifiable test frames through
//! `lumen_core::Session` → `UdpTransport`. `lumen-client-rs --connect host:9777` is the
//! counterpart (reassembles, FEC-recovers, verifies content).

use anyhow::{anyhow, Context, Result};
use lumen_core::config::{FecConfig, FecScheme, Mode, Role};
use lumen_core::packet::{FLAG_PIC, FLAG_SOF};
use lumen_core::quic::{endpoint, io, Hello, Start, Welcome};
use lumen_core::transport::UdpTransport;
use lumen_core::Session;
use rand::RngCore;

/// Deterministic test frame: `u32 LE index` then `data[i] = idx + i` (wrapping).
pub fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    d[0..4].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}

pub fn run(port: u16, frames: u32) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    rt.block_on(serve_one(port, frames))
}

async fn serve_one(port: u16, frames: u32) -> Result<()> {
    let ep = endpoint::server(([0, 0, 0, 0], port).into())
        .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    tracing::info!(port, "lumen/1 host listening (QUIC)");

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

    // Reserve a UDP port for the data plane (bind, read it back, rebind in UdpTransport).
    let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
    let udp_port = probe.local_addr()?.port();
    drop(probe);

    let mut key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key);
    let welcome = Welcome {
        abi_version: lumen_core::ABI_VERSION,
        udp_port,
        mode: Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        },
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
        frames,
    };
    io::write_msg(&mut send, &welcome.encode()).await?;

    let start = Start::decode(&io::read_msg(&mut recv).await?)
        .map_err(|e| anyhow!("Start decode: {e:?}"))?;
    let client_udp = std::net::SocketAddr::new(peer.ip(), start.client_udp_port);
    tracing::info!(%client_udp, udp_port, "handshake complete — streaming");

    // Data plane on a native thread (no async on the hot path — design invariant).
    let cfg = welcome.session_config(Role::Host);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &client_udp.to_string())
                .context("bind data plane")?;
        let mut session =
            Session::new(cfg, Box::new(transport)).map_err(|e| anyhow!("host session: {e:?}"))?;
        let interval = std::time::Duration::from_millis(1000 / 60);
        for idx in 0..frames {
            let data = test_frame(idx, 64 * 1024);
            session
                .submit_frame(&data, idx as u64 * 16_666_667, (FLAG_PIC | FLAG_SOF) as u32)
                .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
            std::thread::sleep(interval);
        }
        tracing::info!(frames, "all frames sent");
        Ok(())
    })
    .await
    .context("stream thread")??;

    // Give the client a moment to drain, then close cleanly.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    conn.close(0u32.into(), b"done");
    ep.wait_idle().await;
    Ok(())
}
