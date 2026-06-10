//! `punktfunk-client-rs` — the reference client for `punktfunk/1` (M3): QUIC control plane, UDP data
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
//! `--pin <64-hex>` pins the host's certificate fingerprint (the host logs it at startup);
//! without it the client trusts on first use and prints the observed fingerprint to pin.
//! Host→client datagrams (Opus audio, rumble) are counted and reported with the stream
//! stats — decode/playback is the platform clients' job.
//!
//! Usage: `punktfunk-client-rs [--connect HOST:PORT] [--mode WxHxFPS] [--out FILE] [--input-test]
//!         [--pin HEX]` (M4 adds VAAPI decode + wgpu present on this same skeleton.)

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::Role;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{endpoint, io, Hello, Reconfigure, Reconfigured, Start, Welcome};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::{Mode, PunktfunkError, Session};
use std::io::Write;

struct Args {
    connect: String,
    mode: Mode,
    out: Option<String>,
    input_test: bool,
    pin: Option<[u8; 32]>,
    /// `--remode WxHxFPS:SECS` — request this mode SECS seconds into the stream.
    remode: Option<(Mode, u32)>,
    /// `--pair PIN` — run the pairing ceremony instead of a session.
    pair: Option<String>,
    /// `--name LABEL` — how the host labels this client when pairing.
    name: String,
}

fn parse_mode(m: &str) -> Option<Mode> {
    let mut it = m.split('x');
    Some(Mode {
        width: it.next()?.parse().ok()?,
        height: it.next()?.parse().ok()?,
        refresh_hz: it.next()?.parse().ok()?,
    })
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// This client's persistent identity (`~/.config/punktfunk/client-{cert,key}.pem`),
/// generated on first use — presented on every connect so hosts can recognize it once
/// paired.
fn load_or_create_identity() -> Result<(String, String)> {
    let home = std::env::var("HOME").context("HOME unset")?;
    let dir = std::path::PathBuf::from(home).join(".config/punktfunk");
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&cp, &c)?;
    std::fs::write(&kp, &k)?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let get = |flag: &str| {
        argv.iter()
            .skip_while(|a| *a != flag)
            .nth(1)
            .map(String::as_str)
    };
    let mode = get("--mode").and_then(parse_mode).unwrap_or(Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    });
    let remode = get("--remode").and_then(|s| {
        let (m, secs) = s.split_once(':')?;
        Some((parse_mode(m)?, secs.parse().ok()?))
    });
    // A present-but-malformed --pin must abort, not silently downgrade to trust-on-first-use
    // (the user asked for verification; fail closed).
    let pin = match get("--pin") {
        None => None,
        Some(s) => {
            match parse_hex32(s) {
                Some(p) => Some(p),
                None => {
                    eprintln!("--pin must be exactly 64 hex chars (the host logs its fingerprint at startup)");
                    std::process::exit(2);
                }
            }
        }
    };
    Args {
        connect: get("--connect").unwrap_or("127.0.0.1:9777").to_string(),
        mode,
        out: get("--out").map(String::from),
        input_test: argv.iter().any(|a| a == "--input-test"),
        pin,
        remode,
        pair: get("--pair").map(String::from),
        name: get("--name").unwrap_or("punktfunk-client-rs").to_string(),
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
    // Pairing mode: run the PIN ceremony and print the fingerprint to pin, then exit.
    if let Some(pin) = &args.pair {
        let (host, port) = args
            .connect
            .rsplit_once(':')
            .context("--connect host:port")?;
        let identity = load_or_create_identity()?;
        let fp = punktfunk_core::client::NativeClient::pair(
            host,
            port.parse().context("port")?,
            (&identity.0, &identity.1),
            pin,
            &args.name,
            std::time::Duration::from_secs(90),
        )
        .map_err(|e| anyhow!("pairing failed: {e:?} (wrong PIN?)"))?;
        tracing::info!(
            fingerprint = %hex(&fp),
            "PAIRED — connect with --pin {} from now on",
            hex(&fp)
        );
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(session(args))
}

async fn session(args: Args) -> Result<()> {
    let remote: std::net::SocketAddr = args.connect.parse().context("--connect host:port")?;
    let identity = load_or_create_identity()?;
    let (ep, observed) = endpoint::client_pinned_with_identity(
        args.pin,
        Some((identity.0.as_str(), identity.1.as_str())),
    );
    let ep = ep.map_err(|e| anyhow!("QUIC client endpoint: {e}"))?;
    let conn = ep
        .connect(remote, "punktfunk")
        .context("connect")?
        .await
        .context("QUIC handshake (a pin mismatch fails here)")?;
    match (args.pin, *observed.lock().unwrap()) {
        (Some(_), _) => tracing::info!(%remote, "punktfunk/1 connected — host fingerprint pinned"),
        (None, Some(fp)) => tracing::info!(
            %remote,
            fingerprint = %hex(&fp),
            "punktfunk/1 connected (trust-on-first-use) — pass --pin to verify this host"
        ),
        (None, None) => tracing::info!(%remote, "punktfunk/1 connected"),
    }
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;

    io::write_msg(
        &mut send,
        &Hello {
            abi_version: punktfunk_core::ABI_VERSION,
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

    // Mid-stream renegotiation test: after a delay, ask the host to switch modes on the
    // still-open control stream. The stream then carries new-mode AUs (IDR + in-band
    // parameter sets) — ffprobe the --out file to see both resolutions.
    if let Some((new_mode, after_secs)) = args.remode {
        let mut rs = send;
        let mut rr = recv;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(after_secs as u64)).await;
            tracing::info!(?new_mode, "requesting mid-stream mode switch");
            if io::write_msg(&mut rs, &Reconfigure { mode: new_mode }.encode())
                .await
                .is_err()
            {
                tracing::error!("Reconfigure write failed");
                return;
            }
            match io::read_msg(&mut rr)
                .await
                .map(|b| Reconfigured::decode(&b))
            {
                Ok(Ok(ack)) if ack.accepted => {
                    tracing::info!(mode = ?ack.mode, "mode switch ACCEPTED")
                }
                Ok(Ok(ack)) => tracing::warn!(active = ?ack.mode, "mode switch REJECTED"),
                other => tracing::error!(?other, "bad Reconfigured"),
            }
        });
    }

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
                    // Gamepad plane: tap A + sweep the left stick on pad 0 (the host
                    // accumulates these into its virtual xpad; needs /dev/uinput access).
                    use punktfunk_core::input::gamepad::{AXIS_LS_X, BTN_A};
                    let pad_events = [
                        (InputKind::GamepadButton, BTN_A, 1),
                        (InputKind::GamepadButton, BTN_A, 0),
                        (
                            InputKind::GamepadAxis,
                            AXIS_LS_X,
                            ((i as i32) % 64 - 32) * 1024,
                        ),
                    ];
                    for (kind, code, x) in pad_events {
                        let ev = InputEvent {
                            kind,
                            _pad: [0; 3],
                            code,
                            x,
                            y: 0,
                            flags: 0, // pad index 0
                        };
                        let _ = conn2.send_datagram(ev.encode().to_vec().into());
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

    // Host→client datagrams: count Opus audio + rumble (playback is the platform clients'
    // job; here we verify the planes flow).
    let audio_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let audio_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rumble_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let (a, ab, r) = (audio_pkts.clone(), audio_bytes.clone(), rumble_pkts.clone());
        let conn2 = conn.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            while let Ok(d) = conn2.read_datagram().await {
                if let Some((_, _, opus)) = punktfunk_core::quic::decode_audio_datagram(&d) {
                    a.fetch_add(1, Relaxed);
                    ab.fetch_add(opus.len() as u64, Relaxed);
                } else if punktfunk_core::quic::decode_rumble_datagram(&d).is_some() {
                    r.fetch_add(1, Relaxed);
                }
            }
        });
    }

    let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;
    let out_path = args.out.clone();

    // Data plane on a blocking thread (native threads only on the frame path).
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
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
                Err(PunktfunkError::NoFrame) => {
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
            "punktfunk/1 stream complete (capture→reassembled latency, same-host clock)"
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
    .await?;

    // Report the side planes whether or not the video plane succeeded.
    {
        use std::sync::atomic::Ordering::Relaxed;
        let (a, ab, r) = (
            audio_pkts.load(Relaxed),
            audio_bytes.load(Relaxed),
            rumble_pkts.load(Relaxed),
        );
        if a > 0 || r > 0 {
            tracing::info!(
                audio_pkts = a,
                audio_kb = ab / 1000,
                rumble_pkts = r,
                "host→client datagrams (Opus 48 kHz stereo, 5 ms frames)"
            );
        }
    }

    conn.close(0u32.into(), b"done");
    result
}

/// The host's deterministic test frame (mirror of `punktfunk-host::m3::test_frame`).
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
