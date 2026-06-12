//! `punktfunk-client-rs` — the reference client for `punktfunk/1` (M3): QUIC control plane, UDP data
//! plane, input over QUIC datagrams. Two modes, decided by the host's Welcome:
//!
//! * **verification** (`frames > 0`, synthetic host): byte-checks deterministic test frames;
//! * **stream** (`frames == 0`, virtual host): receives real NVENC AUs, writes a playable
//!   `.h265`, and reports per-frame **capture→…→reassembled latency** percentiles (the host
//!   stamps each frame with its capture wall clock; same-host runs share that clock).
//!
//! `--input-test` exercises the input plane: scripted mouse/keyboard datagrams during the
//! stream (watch them land in the host session, e.g. xev inside gamescope). `--mic-test`
//! exercises the mic uplink: a synthetic 440 Hz tone streamed as Opus (0xCB) → the host's
//! virtual microphone source (record it host-side to hear the tone). `--touch-test` drags a
//! synthetic finger in a circle → host libei `ei_touchscreen` injection. `--rich-input-test`
//! drives a virtual DualSense touchpad + motion over the 0xCC plane (host on
//! `PUNKTFUNK_GAMEPAD=dualsense`) and logs the 0xCD HID-output feedback (lightbar / adaptive
//! triggers) that comes back.
//!
//! `--pin <64-hex>` pins the host's certificate fingerprint (the host logs it at startup);
//! without it the client trusts on first use and prints the observed fingerprint to pin.
//! `--pair <PIN>` runs the SPAKE2 pairing ceremony: read the PIN the host prints when it
//! arms pairing (`--allow-pairing`/`--require-pairing`), pass it here; on success the
//! client prints the verified host fingerprint to `--pin` from then on.
//! Host→client datagrams (Opus audio, rumble) are counted and reported with the stream
//! stats — decode/playback is the platform clients' job.
//!
//! `--compositor NAME` requests a host compositor backend (`auto`|`kwin`|`wlroots`|`mutter`|
//! `gamescope`); the host honors it if available, else auto-detects and reports the resolved
//! choice in its Welcome (logged as `session offer … compositor=…`).
//!
//! `--gamepad NAME` requests a host virtual-pad backend (`auto`|`xbox360`|`dualsense`); the
//! host honors it where available (DualSense needs Linux UHID), else falls back to X-Box 360,
//! and reports the resolved choice in its Welcome (logged as `session offer … gamepad=…`).
//!
//! `--discover [SECS]` browses the LAN for native (`_punktfunk._udp`) hosts the host advertises
//! over mDNS, prints each (name, addr:port, pairing requirement, cert fingerprint to pin), and
//! exits without connecting.
//!
//! Usage: `punktfunk-client-rs [--connect HOST:PORT] [--mode WxHxFPS] [--out FILE] [--input-test]
//!         [--pin HEX] [--compositor NAME] [--gamepad NAME] | --discover [SECS]`
//! (M4 adds VAAPI decode + wgpu present on this skeleton.)

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::config::Role;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::FLAG_PROBE;
use punktfunk_core::quic::{
    endpoint, io, ClockEcho, ClockProbe, Hello, ProbeRequest, ProbeResult, Reconfigure,
    Reconfigured, Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::{CompositorPref, Mode, PunktfunkError, Session};
use std::io::Write;

struct Args {
    connect: String,
    mode: Mode,
    out: Option<String>,
    input_test: bool,
    /// `--mic-test` — stream a synthetic 440 Hz tone as the mic uplink (proves the mic path).
    mic_test: bool,
    /// `--touch-test` — drag a synthetic finger in a circle (proves the touch path).
    touch_test: bool,
    /// `--rich-input-test` — drive the DualSense touchpad + motion over 0xCC (host needs
    /// `PUNKTFUNK_GAMEPAD=dualsense`); also logs the 0xCD HID-output feedback that comes back.
    rich_input_test: bool,
    pin: Option<[u8; 32]>,
    /// `--remode WxHxFPS:SECS` — request this mode SECS seconds into the stream.
    remode: Option<(Mode, u32)>,
    /// `--pair PIN` — run the pairing ceremony instead of a session.
    pair: Option<String>,
    /// `--name LABEL` — how the host labels this client when pairing.
    name: String,
    /// `--compositor NAME` — request a host compositor backend (auto|kwin|wlroots|mutter|gamescope).
    compositor: CompositorPref,
    /// `--gamepad NAME` — request a host virtual-pad backend (auto|xbox360|dualsense).
    gamepad: GamepadPref,
    /// `--bitrate KBPS` — request this encoder bitrate (kilobits/s); 0 = host default.
    bitrate_kbps: u32,
    /// `--speed-test KBPS:MS` — after the stream starts, ask the host for a `MS`-millisecond
    /// bandwidth probe burst at `KBPS`, then report measured throughput + loss.
    speed_test: Option<(u32, u32)>,
    /// `--discover [SECS]` — browse the LAN for native (`_punktfunk._udp`) hosts for `SECS`
    /// seconds (default 4), print what's found, and exit. No connection is made.
    discover: Option<u64>,
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
    // A present-but-unrecognized --compositor must abort rather than silently auto-detect.
    let compositor = match get("--compositor") {
        None => CompositorPref::Auto,
        Some(s) => match CompositorPref::from_name(s) {
            Some(c) => c,
            None => {
                eprintln!("--compositor must be one of: auto, kwin, wlroots, mutter, gamescope");
                std::process::exit(2);
            }
        },
    };
    // Same fail-closed discipline for --gamepad.
    let gamepad = match get("--gamepad") {
        None => GamepadPref::Auto,
        Some(s) => match GamepadPref::from_name(s) {
            Some(g) => g,
            None => {
                eprintln!("--gamepad must be one of: auto, xbox360, dualsense");
                std::process::exit(2);
            }
        },
    };
    Args {
        connect: get("--connect").unwrap_or("127.0.0.1:9777").to_string(),
        mode,
        out: get("--out").map(String::from),
        input_test: argv.iter().any(|a| a == "--input-test"),
        mic_test: argv.iter().any(|a| a == "--mic-test"),
        touch_test: argv.iter().any(|a| a == "--touch-test"),
        rich_input_test: argv.iter().any(|a| a == "--rich-input-test"),
        pin,
        remode,
        pair: get("--pair").map(String::from),
        name: get("--name").unwrap_or("punktfunk-client-rs").to_string(),
        compositor,
        gamepad,
        bitrate_kbps: get("--bitrate").and_then(|s| s.parse().ok()).unwrap_or(0),
        speed_test: get("--speed-test").and_then(|s| {
            let (kbps, ms) = s.split_once(':')?;
            Some((kbps.parse().ok()?, ms.parse().ok()?))
        }),
        // `--discover` may be a bare flag or carry a seconds value (`--discover 8`); only treat
        // the following token as a count when it parses as a number (else it's the next flag).
        discover: argv
            .iter()
            .any(|a| a == "--discover")
            .then(|| get("--discover").and_then(|s| s.parse().ok()).unwrap_or(4)),
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
    // Discovery mode: browse the LAN for native hosts, print them, and exit (no connection).
    if let Some(secs) = args.discover {
        return discover(secs);
    }
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

/// Browse the LAN for native (`_punktfunk._udp`) hosts for `secs` seconds and print them, then
/// exit — the discovery side of the host's mDNS advert (host crate `discovery.rs`). TXT keys:
/// `fp` (host cert fingerprint to pin), `pair` (required|optional), `id` (stable host id).
fn discover(secs: u64) -> Result<()> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let receiver = daemon
        .browse("_punktfunk._udp.local.")
        .context("browse _punktfunk._udp")?;
    tracing::info!(secs, "browsing for native punktfunk/1 hosts (_punktfunk._udp)…");
    // One row per host, keyed by the stable uniqueid (falls back to the fullname) so the same
    // host re-advertising or answering on several interfaces collapses to a single entry.
    let mut hosts: BTreeMap<String, String> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Timeout == time left to the deadline: an event returns immediately, otherwise the recv
        // returns Err exactly at the deadline (or if the daemon channel closes) and we stop.
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let props = info.get_properties();
                let val = |k: &str| props.get_property_val_str(k).unwrap_or("").to_string();
                let addr = info
                    .get_addresses()
                    .iter()
                    .next()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "?".into());
                let fp = val("fp");
                let fp_short = fp.get(..16).unwrap_or(fp.as_str());
                let name = info.get_fullname().split('.').next().unwrap_or("?").to_string();
                let id = val("id");
                let key = if id.is_empty() {
                    info.get_fullname().to_string()
                } else {
                    id
                };
                let row = format!(
                    "  {name:<24} {addr}:{:<6} pair={:<9} fp={fp_short}…",
                    info.get_port(),
                    val("pair"),
                );
                hosts.insert(key, row);
            }
            Ok(_) => {} // SearchStarted / ServiceFound / removals — ignore
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    if hosts.is_empty() {
        println!("no native punktfunk/1 hosts found on the LAN ({secs}s)");
    } else {
        println!("native punktfunk/1 hosts ({}):", hosts.len());
        for row in hosts.values() {
            println!("{row}");
        }
        println!(
            "\nconnect with: punktfunk-client-rs --connect <addr:port> [--pin <fp> | --pair <PIN>]"
        );
    }
    Ok(())
}

/// Run the wall-clock skew handshake: `ROUNDS` `ClockProbe`/`ClockEcho` round-trips on the control
/// stream, returning the host−client clock offset (ns) from the minimum-RTT sample, or `None` if the
/// host never answers (an old host — the caller then assumes a shared clock). Each read is bounded so
/// a silent old host can't wedge session start.
async fn clock_sync(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> Option<i64> {
    const ROUNDS: usize = 8;
    let read_timeout = std::time::Duration::from_secs(2);
    let mut samples: Vec<(u64, u64, u64, u64)> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t1 = now_ns();
        let probe = ClockProbe { t1_ns: t1 }.encode();
        if io::write_msg(send, &probe).await.is_err() {
            break;
        }
        let read = tokio::time::timeout(read_timeout, io::read_msg(recv)).await;
        let echo = match read {
            Ok(Ok(b)) => match ClockEcho::decode(&b) {
                Ok(e) => e,
                Err(_) => break, // not a ClockEcho -> give up on skew
            },
            _ => break, // timeout or stream error -> old host / no skew support
        };
        samples.push((echo.t1_ns, echo.t2_ns, echo.t3_ns, now_ns()));
    }
    let (offset, rtt) = punktfunk_core::quic::clock_offset_ns(&samples)?;
    tracing::info!(
        offset_ns = offset,
        rtt_us = rtt / 1000,
        rounds = samples.len(),
        "clock skew estimated (host-client); latency now cross-machine valid"
    );
    Some(offset)
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
            compositor: args.compositor,
            gamepad: args.gamepad,
            bitrate_kbps: args.bitrate_kbps,
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
        compositor = welcome.compositor.as_str(),
        gamepad = welcome.gamepad.as_str(),
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

    // Wall-clock skew handshake on the still-private control stream (before --remode/--speed-test
    // take it): align our clock to the host's so the per-frame capture→reassembled latency is valid
    // across machines. `None` ⇒ an old host that doesn't answer — fall back to a shared clock (0).
    let clock_offset_ns = clock_sync(&mut send, &mut recv).await;

    // Speed-test accumulators: the data-plane loop folds each FLAG_PROBE filler AU in here; the
    // --speed-test reporter below reads them once the host's ProbeResult lands. first/last hold
    // now_ns timestamps of the receive window (0 = unset).
    let probe_recv_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let probe_recv_packets = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let probe_first_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let probe_last_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Mid-stream renegotiation test: after a delay, ask the host to switch modes on the
    // still-open control stream. The stream then carries new-mode AUs (IDR + in-band
    // parameter sets) — ffprobe the --out file to see both resolutions. Mutually exclusive with
    // --speed-test (both own the control stream).
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
    } else if let Some((target_kbps, duration_ms)) = args.speed_test {
        // Bandwidth probe: after the stream warms up, ask the host to burst FLAG_PROBE filler;
        // measure what arrives vs. what it reports sending.
        let mut ss = send;
        let mut sr = recv;
        let (pb, pp, pf, pl) = (
            probe_recv_bytes.clone(),
            probe_recv_packets.clone(),
            probe_first_ns.clone(),
            probe_last_ns.clone(),
        );
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await; // let the stream warm up
            tracing::info!(target_kbps, duration_ms, "requesting speed-test probe");
            if io::write_msg(
                &mut ss,
                &ProbeRequest {
                    target_kbps,
                    duration_ms,
                }
                .encode(),
            )
            .await
            .is_err()
            {
                tracing::error!("ProbeRequest write failed");
                return;
            }
            let res = match io::read_msg(&mut sr).await.map(|b| ProbeResult::decode(&b)) {
                Ok(Ok(r)) => r,
                other => {
                    tracing::error!(?other, "bad ProbeResult");
                    return;
                }
            };
            // The reliable result can beat the last UDP shards — let them reassemble.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let recv_bytes = pb.load(Relaxed);
            let recv_packets = pp.load(Relaxed);
            let (first, last) = (pf.load(Relaxed), pl.load(Relaxed));
            let window_ms = if first > 0 && last > first {
                (last - first) / 1_000_000
            } else {
                0
            };
            let throughput_kbps = recv_bytes.saturating_mul(8).checked_div(window_ms).unwrap_or(0);
            let loss_pct = if res.bytes_sent > 0 {
                res.bytes_sent.saturating_sub(recv_bytes) as f64 / res.bytes_sent as f64 * 100.0
            } else {
                0.0
            };
            tracing::info!(
                target_kbps,
                host_sent_bytes = res.bytes_sent,
                host_sent_packets = res.packets_sent,
                recv_bytes,
                recv_packets,
                window_ms,
                throughput_kbps,
                loss_pct = format!("{loss_pct:.1}%"),
                "SPEED TEST complete",
            );
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

    // Mic plane: stream a synthetic 440 Hz tone as the mic uplink (0xCB), Opus-encoded 5 ms
    // stereo frames — proves client→host mic passthrough end to end without a real microphone
    // (the host decodes it into its virtual PipeWire source; record that source to hear the tone).
    #[cfg(not(target_os = "linux"))]
    if args.mic_test {
        tracing::warn!("--mic-test requires Linux (libopus) — skipped");
    }
    #[cfg(target_os = "linux")]
    if args.mic_test {
        let conn2 = conn.clone();
        tokio::spawn(async move {
            let mut enc =
                match opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(error = %e, "mic-test: opus encoder init failed");
                        return;
                    }
                };
            let _ = enc.set_bitrate(opus::Bitrate::Bits(64_000));
            tracing::info!("mic-test: streaming a 440 Hz tone as the mic uplink");
            let mut phase = 0.0f32;
            let step = 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            let mut pcm = [0f32; 240 * 2]; // 5 ms stereo
            let mut out = [0u8; 4000];
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(5));
            for seq in 0u32.. {
                interval.tick().await;
                for f in 0..240 {
                    let s = (phase.sin()) * 0.25;
                    phase += step;
                    if phase > std::f32::consts::PI * 2.0 {
                        phase -= std::f32::consts::PI * 2.0;
                    }
                    pcm[f * 2] = s;
                    pcm[f * 2 + 1] = s;
                }
                if let Ok(n) = enc.encode_float(&pcm, &mut out) {
                    let d = punktfunk_core::quic::encode_mic_datagram(seq, now_ns(), &out[..n]);
                    if conn2.send_datagram(d.into()).is_err() {
                        break;
                    }
                }
            }
            tracing::info!("mic-test: done");
        });
    }

    // Touch plane: drag a synthetic finger (touch id 0) in a circle on the client surface, so
    // the host injects it via libei ei_touchscreen — proves the touch path end to end. `flags`
    // packs the surface w/h; x/y are pixels (the host maps them into the device region).
    if args.touch_test {
        let conn2 = conn.clone();
        let (w, h) = (args.mode.width, args.mode.height);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let flags = (w << 16) | (h & 0xffff);
            let (cx, cy, r) = (w as f32 / 2.0, h as f32 / 2.0, h as f32 / 4.0);
            let touch = |kind, x: f32, y: f32| InputEvent {
                kind,
                _pad: [0; 3],
                code: 0, // touch id 0
                x: x as i32,
                y: y as i32,
                flags,
            };
            tracing::info!("touch-test: dragging a finger in a circle for ~6s");
            for loop_i in 0..3u32 {
                let _ = conn2.send_datagram(
                    touch(InputKind::TouchDown, cx + r, cy)
                        .encode()
                        .to_vec()
                        .into(),
                );
                for i in 0..60u32 {
                    let a = std::f32::consts::TAU * i as f32 / 60.0;
                    let mv = touch(InputKind::TouchMove, cx + r * a.cos(), cy + r * a.sin());
                    let _ = conn2.send_datagram(mv.encode().to_vec().into());
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                let _ = conn2.send_datagram(
                    touch(InputKind::TouchUp, cx + r, cy)
                        .encode()
                        .to_vec()
                        .into(),
                );
                let _ = loop_i;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            tracing::info!("touch-test: done");
        });
    }

    // Rich-input plane: instantiate pad 0 on the host (a gamepad event creates the virtual
    // DualSense), then drive its touchpad (drag a finger across) + motion (gyro wobble) over the
    // 0xCC plane. Proves the rich client→host path; the 0xCD feedback is logged by the receive
    // loop below. Requires the host on the DualSense backend (`PUNKTFUNK_GAMEPAD=dualsense`).
    if args.rich_input_test {
        let conn2 = conn.clone();
        tokio::spawn(async move {
            use punktfunk_core::input::gamepad::AXIS_LS_X;
            use punktfunk_core::quic::RichInput;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // A neutral gamepad axis event makes the host create the virtual DualSense pad 0.
            let arrive = InputEvent {
                kind: InputKind::GamepadAxis,
                _pad: [0; 3],
                code: AXIS_LS_X,
                x: 0,
                y: 0,
                flags: 0,
            };
            let _ = conn2.send_datagram(arrive.encode().to_vec().into());
            tracing::info!(
                "rich-input-test: dragging the DualSense touchpad + wobbling motion for ~6s"
            );
            let touch = |active, x, y| RichInput::Touchpad {
                pad: 0,
                finger: 0,
                active,
                x,
                y,
            };
            for _ in 0..3u32 {
                let _ = conn2.send_datagram(touch(true, 0, 32768).encode().into());
                for i in 0..60u32 {
                    let x = ((i * 65535) / 60) as u16;
                    let _ = conn2.send_datagram(touch(true, x, 32768).encode().into());
                    let g = (((i as i32 % 20) - 10) * 500) as i16; // gyro wobble
                    let _ = conn2.send_datagram(
                        RichInput::Motion {
                            pad: 0,
                            gyro: [g, 0, 0],
                            accel: [0, 0, 16384],
                        }
                        .encode()
                        .into(),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                let _ = conn2.send_datagram(touch(false, 65535, 32768).encode().into());
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            tracing::info!("rich-input-test: done");
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
    let hidout_pkts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let (a, ab, r, h) = (
            audio_pkts.clone(),
            audio_bytes.clone(),
            rumble_pkts.clone(),
            hidout_pkts.clone(),
        );
        let conn2 = conn.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            while let Ok(d) = conn2.read_datagram().await {
                if let Some((_, _, opus)) = punktfunk_core::quic::decode_audio_datagram(&d) {
                    a.fetch_add(1, Relaxed);
                    ab.fetch_add(opus.len() as u64, Relaxed);
                } else if punktfunk_core::quic::decode_rumble_datagram(&d).is_some() {
                    r.fetch_add(1, Relaxed);
                } else if let Some(hid) = punktfunk_core::quic::HidOutput::decode(&d) {
                    // The DualSense feedback plane (lightbar / player LEDs / adaptive triggers).
                    // Log the first few so a playtest can see triggers/LEDs arrive without spam.
                    if h.fetch_add(1, Relaxed) < 12 {
                        tracing::info!(?hid, "DualSense HID output (0xCD)");
                    }
                }
            }
        });
    }

    let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;
    let out_path = args.out.clone();
    let (pb, pp, pf, pl) = (
        probe_recv_bytes.clone(),
        probe_recv_packets.clone(),
        probe_first_ns.clone(),
        probe_last_ns.clone(),
    );

    // Express our receive time in the host clock before differencing against the host-stamped
    // capture pts. 0 ⇒ same-host or an old host that didn't answer the skew handshake (the latency
    // is then only valid same-host, as before).
    let clock_offset = clock_offset_ns.unwrap_or(0);
    let skew_corrected = clock_offset_ns.is_some();

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
                    // Speed-test filler isn't video: fold it into the probe accumulators and skip
                    // verification / the --out sink.
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        use std::sync::atomic::Ordering::Relaxed;
                        let n = now_ns();
                        let _ = pf.compare_exchange(0, n, Relaxed, Relaxed);
                        pl.store(n, Relaxed);
                        pb.fetch_add(frame.data.len() as u64, Relaxed);
                        pp.fetch_add(1, Relaxed);
                        continue;
                    }
                    bytes += frame.data.len() as u64;
                    // capture→reassembled: our receive instant in the host clock (now + offset)
                    // minus the host's capture pts. offset is 0 same-host / old host.
                    let lat = (now_ns() as i128 + clock_offset as i128 - frame.pts_ns as i128)
                        .max(0) as u64;
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
            skew_corrected,
            "punktfunk/1 stream complete (capture→reassembled latency; skew_corrected=true ⇒ \
             cross-machine valid, false ⇒ same-host clock)"
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
        let (a, ab, r, h) = (
            audio_pkts.load(Relaxed),
            audio_bytes.load(Relaxed),
            rumble_pkts.load(Relaxed),
            hidout_pkts.load(Relaxed),
        );
        if a > 0 || r > 0 || h > 0 {
            tracing::info!(
                audio_pkts = a,
                audio_kb = ab / 1000,
                rumble_pkts = r,
                hidout_pkts = h,
                "host→client datagrams (Opus 48 kHz stereo, 5 ms frames; rumble; DualSense HID)"
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
