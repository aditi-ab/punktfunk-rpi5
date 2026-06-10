//! `lumen-client-rs` — the reference client for `lumen/1` (M3): QUIC control plane, UDP data
//! plane, input over QUIC datagrams. Two modes, decided by the host's Welcome:
//!
//! * **verification** (`frames > 0`, synthetic host): byte-checks deterministic test frames;
//! * **stream** (`frames == 0`, virtual host): receives real NVENC AUs, writes a playable
//!   `.h265`, and reports per-frame **capture→…→reassembled latency** percentiles (the host
//!   stamps each frame with its capture wall clock; same-host runs share that clock).
//!
//! `--input-test` exercises the input plane: scripted mouse/keyboard datagrams during the
//! stream (watch them land in the host session, e.g. xev inside gamescope).
//!
//! Usage: `lumen-client-rs [--connect HOST:PORT] [--mode WxHxFPS] [--out FILE] [--input-test]`
//! (M4 adds VAAPI decode + wgpu present on this same skeleton.)

use anyhow::{anyhow, Context, Result};
use lumen_core::config::Role;
use lumen_core::input::{InputEvent, InputKind};
use lumen_core::quic::{endpoint, io, Hello, Start, Welcome};
use lumen_core::transport::UdpTransport;
use lumen_core::{LumenError, Mode, Session};
use std::io::Write;

struct Args {
    connect: String,
    mode: Mode,
    out: Option<String>,
    input_test: bool,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        argv.iter()
            .skip_while(|a| *a != flag)
            .nth(1)
            .map(String::as_str)
    };
    let mode = get("--mode")
        .and_then(|m| {
            let mut it = m.split('x');
            Some(Mode {
                width: it.next()?.parse().ok()?,
                height: it.next()?.parse().ok()?,
                refresh_hz: it.next()?.parse().ok()?,
            })
        })
        .unwrap_or(Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        });
    Args {
        connect: get("--connect").unwrap_or("127.0.0.1:9777").to_string(),
        mode,
        out: get("--out").map(String::from),
        input_test: argv.iter().any(|a| a == "--input-test"),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = parse_args();
    if let Err(e) = run(args) {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(session(args))
}

async fn session(args: Args) -> Result<()> {
    let remote: std::net::SocketAddr = args.connect.parse().context("--connect host:port")?;
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
            mode: args.mode,
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

    // Input plane: scripted events as QUIC datagrams (mouse square + 'A' taps), proving the
    // low-latency input path without a real input device.
    if args.input_test {
        let conn2 = conn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            tracing::info!("input-test: sending scripted datagrams for ~6s");
            for i in 0..160u32 {
                let (dx, dy) = match (i / 10) % 4 {
                    0 => (12, 0),
                    1 => (0, 12),
                    2 => (-12, 0),
                    _ => (0, -12),
                };
                let mv = InputEvent {
                    kind: InputKind::MouseMove,
                    _pad: [0; 3],
                    code: 0,
                    x: dx,
                    y: dy,
                    flags: 0,
                };
                let _ = conn2.send_datagram(mv.encode().to_vec().into());
                if i % 20 == 0 {
                    for kind in [InputKind::KeyDown, InputKind::KeyUp] {
                        let key = InputEvent {
                            kind,
                            _pad: [0; 3],
                            code: 0x41, // VK 'A'
                            x: 0,
                            y: 0,
                            flags: 0,
                        };
                        let _ = conn2.send_datagram(key.encode().to_vec().into());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
            tracing::info!("input-test: done");
        });
    }

    // Closed-flag for the blocking receive loop.
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = closed.clone();
        let conn2 = conn.clone();
        tokio::spawn(async move {
            conn2.closed().await;
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;
    let out_path = args.out.clone();

    // Data plane on a blocking thread (native threads only on the frame path).
    tokio::task::spawn_blocking(move || -> Result<()> {
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())
                .context("bind data plane")?;
        let mut session =
            Session::new(cfg, Box::new(transport)).map_err(|e| anyhow!("client session: {e:?}"))?;
        let mut sink = match &out_path {
            Some(p) => Some(std::io::BufWriter::new(
                std::fs::File::create(p).with_context(|| format!("create {p}"))?,
            )),
            None => None,
        };

        let mut ok = 0u32;
        let mut mismatched = 0u32;
        let mut bytes = 0u64;
        let mut latencies_us: Vec<u64> = Vec::new();
        let mut last_rx = std::time::Instant::now();
        let started = std::time::Instant::now();
        loop {
            if expected > 0 && ok + mismatched >= expected {
                break;
            }
            if closed.load(std::sync::atomic::Ordering::SeqCst)
                && last_rx.elapsed() > std::time::Duration::from_millis(300)
            {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(120)
                || last_rx.elapsed() > std::time::Duration::from_secs(8)
            {
                break;
            }
            match session.poll_frame() {
                Ok(frame) => {
                    last_rx = std::time::Instant::now();
                    bytes += frame.data.len() as u64;
                    // The host stamps pts with its capture wall clock; same-host runs share it.
                    let lat = now_ns().saturating_sub(frame.pts_ns);
                    if lat > 0 && lat < 10_000_000_000 {
                        latencies_us.push(lat / 1000);
                    }
                    if expected > 0 {
                        // Verification mode: deterministic content.
                        let idx = u32::from_le_bytes(frame.data[0..4].try_into().unwrap());
                        if frame.data == test_frame(idx, frame.data.len()) {
                            ok += 1;
                        } else {
                            mismatched += 1;
                        }
                    } else {
                        ok += 1;
                        if let Some(s) = sink.as_mut() {
                            s.write_all(&frame.data).context("write AU")?;
                        }
                    }
                }
                Err(LumenError::NoFrame) => {
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
                Err(e) => return Err(anyhow!("poll_frame: {e:?}")),
            }
        }
        if let Some(mut s) = sink {
            s.flush().ok();
        }

        latencies_us.sort_unstable();
        let pct = |p: f64| -> u64 {
            if latencies_us.is_empty() {
                return 0;
            }
            let i = ((latencies_us.len() as f64 * p) as usize).min(latencies_us.len() - 1);
            latencies_us[i]
        };
        tracing::info!(
            frames = ok,
            mismatched,
            mb = bytes / 1_000_000,
            lat_p50_us = pct(0.50),
            lat_p95_us = pct(0.95),
            lat_p99_us = pct(0.99),
            lat_max_us = latencies_us.last().copied().unwrap_or(0),
            "lumen/1 stream complete (capture→reassembled latency, same-host clock)"
        );
        if expected > 0 {
            anyhow::ensure!(mismatched == 0, "{mismatched} corrupted frames");
            anyhow::ensure!(ok == expected, "received {ok}/{expected} frames");
            tracing::info!("verification PASSED");
        } else {
            anyhow::ensure!(ok > 0, "no frames received");
        }
        Ok(())
    })
    .await??;

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
