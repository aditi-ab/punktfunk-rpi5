//! `punktfunk-host` — the Linux streaming host (plan §2, §6, §7).
//!
//! Creates a client-sized virtual display, captures it via PipeWire, encodes with
//! VAAPI/NVENC, and hands encoded access units to `punktfunk_core` for FEC + packetization +
//! pacing + send. Input flows back via libei/uinput. The platform backends are
//! `#[cfg(target_os = "linux")]`; the crate compiles everywhere so the workspace builds
//! on non-Linux dev machines — it just can't run the pipeline there.
//!
//! Status: M0. The `m0` subcommand runs the capture→encode→file pipeline spike and feeds
//! the encoded AUs through a `punktfunk_core` loopback. M2 wires the full P1 host that a stock
//! Moonlight client connects to.

// Scaffold: trait methods and config paths are defined ahead of their backends.
#![allow(dead_code)]

mod audio;
mod capture;
mod discovery;
mod dmabuf_fence;
mod drm_sync;
mod encode;
mod gamestream;
mod inject;
mod m0;
mod m3;
mod mgmt;
mod native_pairing;
mod pipeline;
mod pwinit;
mod vdisplay;
#[cfg(target_os = "linux")]
mod zerocopy;

use anyhow::{bail, Context, Result};
use encode::Codec;
use m0::{Options, Source};
use std::path::PathBuf;

fn main() {
    // Logs go to stderr so stdout stays machine-readable (`punktfunk-host openapi > spec.json`).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = real_main() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    tracing::info!(
        "punktfunk-host (punktfunk_core ABI v{})",
        punktfunk_core::ABI_VERSION
    );

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // GameStream host control plane (P1.1: mDNS + serverinfo) + management API, and (with
        // --native) the native punktfunk/1 host in the same process — the unified host.
        Some("serve") => {
            let (mgmt_opts, native) = parse_serve(&args[1..])?;
            gamestream::serve(mgmt_opts, native)
        }
        // Print the management API's OpenAPI document (for client codegen).
        Some("openapi") => {
            print!("{}", mgmt::openapi_json());
            Ok(())
        }
        // Standalone input-injection smoke test (no client needed): open the session's input
        // backend and inject a scripted mouse/keyboard pattern. Watch a focused app / `wev`.
        Some("input-test") => input_test(),
        // Zero-copy FFI/GPU probe: init the EGL importer + CUDA context (no capture needed).
        #[cfg(target_os = "linux")]
        Some("zerocopy-probe") => zerocopy::probe(),
        // Compositor readiness probe: exit 0 iff the (detected or PUNKTFUNK_COMPOSITOR-forced)
        // compositor is up and able to create a virtual output *now*. A session-bringup
        // script polls this to gate on real readiness instead of a blind `sleep`.
        Some("probe-compositor") => {
            let compositor = vdisplay::detect()?;
            vdisplay::probe(compositor).with_context(|| format!("{compositor:?} not ready"))?;
            println!("{compositor:?} ready");
            Ok(())
        }
        // Create a virtual DualSense via UHID and exercise it (validation, no streaming session):
        // toggles the Cross button, sweeps the left stick, and prints any HID output the kernel
        // sends back. Verify with `evtest` / `ls /dev/input/by-id/*Punktfunk*` / `wpctl status`.
        #[cfg(target_os = "linux")]
        Some("dualsense-test") => {
            use inject::dualsense::{DsState, DualSensePad};
            let secs: u64 = args
                .iter()
                .skip_while(|a| *a != "--seconds")
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            use std::time::{Duration, Instant};
            let mut pad =
                DualSensePad::open(0).context("create virtual DualSense via /dev/uhid")?;
            // Answer the kernel's init GET_REPORTs promptly so hid-playstation creates the input
            // devices before we start streaming state.
            let init = Instant::now() + Duration::from_millis(800);
            while Instant::now() < init {
                pad.service(0);
                std::thread::sleep(Duration::from_millis(10));
            }
            println!(
                "virtual DualSense created — check `evtest`, `ls /dev/input/by-id/*Punktfunk*`, \
                 `ls /sys/class/leds/`. Cycling Cross + sweeping LS for {secs}s."
            );
            let deadline = Instant::now() + Duration::from_secs(secs);
            let (mut i, mut last_write) = (0i32, Instant::now());
            while Instant::now() < deadline {
                let fb = pad.service(0);
                if let Some((low, high)) = fb.rumble {
                    println!("  rumble from kernel/game: low={low} high={high}");
                }
                for o in fb.hidout {
                    println!("  hid output from kernel/game: {o:?}");
                }
                if last_write.elapsed() >= Duration::from_millis(300) {
                    last_write = Instant::now();
                    i += 1;
                    let buttons = if i % 2 == 0 {
                        punktfunk_core::input::gamepad::BTN_A
                    } else {
                        0
                    };
                    let lx = (((i % 64) - 32) * 1024) as i16; // sweep left stick X
                    let st = DsState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
                    pad.write_state(&st).context("write DualSense report")?;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            println!("dualsense-test: done");
            Ok(())
        }
        // M0 pipeline spike.
        Some("m0") => m0::run(parse_m0(&args[1..])?),
        // M3: native punktfunk/1 host (QUIC control plane + UDP data plane).
        Some("m3-host") => {
            let get = |flag: &str| {
                args.iter()
                    .skip_while(|a| *a != flag)
                    .nth(1)
                    .map(String::as_str)
            };
            let source = match get("--source") {
                Some("virtual") => m3::M3Source::Virtual,
                _ => m3::M3Source::Synthetic,
            };
            m3::run(m3::M3Options {
                port: get("--port").and_then(|s| s.parse().ok()).unwrap_or(9777),
                source,
                seconds: get("--seconds").and_then(|s| s.parse().ok()).unwrap_or(30),
                frames: get("--frames").and_then(|s| s.parse().ok()).unwrap_or(300),
                max_sessions: get("--max-sessions")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                max_concurrent: get("--max-concurrent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(m3::DEFAULT_MAX_CONCURRENT),
                require_pairing: args.iter().any(|a| a == "--require-pairing"),
                allow_pairing: args.iter().any(|a| a == "--allow-pairing"),
                pairing_pin: None,
                paired_store: None,
            })
        }
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        // Bare flags (no subcommand) default to the m0 spike for back-compat.
        Some(_) => m0::run(parse_m0(&args)?),
    }
}

/// Inject a scripted mouse + keyboard pattern through the session's input backend (libei on
/// KWin/GNOME, wlr on Sway). Lets us validate input injection without a Moonlight client.
#[cfg(target_os = "linux")]
fn input_test() -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;

    let backend = inject::default_backend();
    tracing::info!(?backend, "input-test: opening injector");
    let mut inj = inject::open(backend)?;
    // An async backend (libei) needs a moment to establish its portal/EIS session + device
    // resume; events injected before then are dropped.
    std::thread::sleep(Duration::from_secs(4));

    let ev = |kind, code, x, y| InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags: 0,
    };
    tracing::info!(
        "input-test: injecting a mouse square + 'A'/click taps for ~8s (watch wev / focused app)"
    );
    for i in 0..160u32 {
        let (dx, dy) = match (i / 10) % 4 {
            0 => (12, 0),
            1 => (0, 12),
            2 => (-12, 0),
            _ => (0, -12),
        };
        if let Err(e) = inj.inject(&ev(InputKind::MouseMove, 0, dx, dy)) {
            tracing::warn!(error = %format!("{e:#}"), "input-test: inject failed");
        }
        if i % 20 == 0 {
            let _ = inj.inject(&ev(InputKind::KeyDown, 0x41, 0, 0)); // 'A'
            let _ = inj.inject(&ev(InputKind::KeyUp, 0x41, 0, 0));
            let _ = inj.inject(&ev(InputKind::MouseButtonDown, 1, 0, 0)); // left click
            let _ = inj.inject(&ev(InputKind::MouseButtonUp, 1, 0, 0));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!("input-test: done");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn input_test() -> Result<()> {
    bail!("input-test requires Linux")
}

/// `serve` options: the management API (GameStream ports are protocol-fixed) + whether to also run
/// the native punktfunk/1 host in-process (`--native`, the unified host). Returns the mgmt options
/// and the native host config (`None` = GameStream only). Native pairing is **required by default**
/// (an open host any LAN device can stream from is insecure); `--open` turns it off.
fn parse_serve(args: &[String]) -> Result<(mgmt::Options, Option<m3::NativeServe>)> {
    let mut opts = mgmt::Options::default();
    let mut native_port: Option<u16> = None;
    let mut open = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--mgmt-bind" => {
                opts.bind = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --mgmt-bind (want IP:PORT)"))?
            }
            "--mgmt-token" => {
                let token = next()?;
                // An empty token would satisfy the non-loopback "token required" guard
                // while authenticating nobody (or, worse, everybody) — refuse it loudly
                // rather than letting `--mgmt-token "$UNSET_VAR"` ship a dead credential.
                if token.trim().is_empty() {
                    bail!("--mgmt-token must not be empty");
                }
                opts.token = Some(token);
            }
            // Also run the native punktfunk/1 (QUIC) host in this process — the unified host.
            // Pairing is then armed on demand from the management API / web console.
            "--native" => native_port = Some(native_port.unwrap_or(9777)),
            "--native-port" => {
                native_port = Some(
                    next()?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?,
                )
            }
            // Disable mandatory native pairing — any device can connect (trusted single-user
            // setups only). The default REQUIRES pairing.
            "--open" => open = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    // Flag wins over the environment so a unit file can set a default and a shell override it.
    if opts.token.is_none() {
        opts.token = std::env::var("PUNKTFUNK_MGMT_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
    }
    let native = native_port.map(|port| m3::NativeServe {
        port,
        require_pairing: !open,
    });
    Ok((opts, native))
}

fn parse_m0(args: &[String]) -> Result<Options> {
    let mut source = Source::Portal;
    let mut width = 1920u32;
    let mut height = 1080u32;
    let mut fps = 60u32;
    let mut seconds = 5u32;
    let mut codec = Codec::H265;
    let mut bitrate_mbps = 20u64;
    let mut out: Option<PathBuf> = None;
    let mut loopback = true;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--source" => {
                source = match next()?.as_str() {
                    "synthetic" => Source::Synthetic,
                    "portal" => Source::Portal,
                    "kwin-virtual" => Source::KwinVirtual,
                    other => {
                        bail!("unknown --source '{other}' (synthetic|portal|kwin-virtual)")
                    }
                }
            }
            "--width" => {
                width = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --width"))?
            }
            "--height" => {
                height = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --height"))?
            }
            "--fps" => fps = next()?.parse().map_err(|_| anyhow::anyhow!("bad --fps"))?,
            "--seconds" => {
                seconds = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --seconds"))?
            }
            "--codec" => {
                codec = match next()?.as_str() {
                    "h264" => Codec::H264,
                    "h265" | "hevc" => Codec::H265,
                    "av1" => Codec::Av1,
                    other => bail!("unknown --codec '{other}' (h264|h265|av1)"),
                }
            }
            "--bitrate" => {
                bitrate_mbps = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --bitrate (Mbps)"))?
            }
            "--out" => out = Some(PathBuf::from(next()?)),
            "--no-loopback" => loopback = false,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }

    if fps == 0 || width == 0 || height == 0 || seconds == 0 {
        bail!("--fps/--width/--height/--seconds must be > 0");
    }

    let out = out.unwrap_or_else(|| {
        let ext = match codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "obu",
        };
        PathBuf::from(format!("/tmp/punktfunk-m0.{ext}"))
    });

    Ok(Options {
        source,
        width,
        height,
        fps,
        seconds,
        codec,
        bitrate_bps: bitrate_mbps.saturating_mul(1_000_000),
        out,
        loopback,
    })
}

fn print_usage() {
    eprintln!(
        "punktfunk-host — Linux streaming host

USAGE:
    punktfunk-host serve [OPTIONS]    GameStream host control plane (M2: mDNS + serverinfo …)
                                  + the management REST API
    punktfunk-host openapi            print the management API's OpenAPI document (codegen)
    punktfunk-host m3-host [OPTIONS]  native punktfunk/1 host (QUIC control plane + UDP data plane)
    punktfunk-host probe-compositor   exit 0 iff the compositor is up + ready (session-bringup gate)
    punktfunk-host m0 [OPTIONS]       M0 capture→encode→file pipeline spike

SERVE OPTIONS:
    --mgmt-bind <IP:PORT>        management API address (default: 127.0.0.1:47990)
    --mgmt-token <TOKEN>         bearer token for the management API (or PUNKTFUNK_MGMT_TOKEN);
                                 required when --mgmt-bind is not loopback
    --native                     also run the native punktfunk/1 (QUIC) host in this process —
                                 the unified host; pairing is armed from the management API/console
    --native-port <PORT>         native QUIC port (default 9777; implies --native)
    --open                       disable mandatory native pairing (default: pairing REQUIRED —
                                 an open host any LAN device can stream from is insecure)

M3-HOST OPTIONS:
    --port <N>                   QUIC listen port (default: 9777)
    --source <synthetic|virtual> test frames, or virtual display + NVENC (default: synthetic)
    --seconds <N>                per-session stream duration, virtual source (default: 30)
    --frames <N>                 per-session frame count, synthetic source (default: 300)
    --max-sessions <N>           exit after N sessions; 0 = serve forever (default: 0)
    --max-concurrent <N>         stream at most N sessions at once (NVENC bound); overflow waits
                                 in the accept queue; 0 = unlimited (default: 4)
    --allow-pairing              accept PIN pairing ceremonies (arm pairing mode)
    --require-pairing            only serve PIN-paired clients (implies --allow-pairing;
                                 the host logs a 4-digit PIN when a client starts pairing)

M0 OPTIONS:
    --source <synthetic|portal|kwin-virtual>
                                 frame source (default: portal). 'kwin-virtual' creates a
                                 KWin virtual output at --width x --height and captures it
    --seconds <N>                capture duration in seconds (default: 5)
    --fps <N>                    target frame rate (default: 60)
    --codec <h264|h265|av1>      NVENC codec (default: h265)
    --bitrate <MBPS>             target bitrate in Mbps (default: 20)
    --width <W> --height <H>     synthetic source size (default: 1920x1080)
    --out <PATH>                 raw Annex-B output (default: /tmp/punktfunk-m0.<ext>)
    --no-loopback                skip the punktfunk_core round-trip verification
    -h, --help                   this help

NOTES:
    'portal' needs headless Sway + xdg-desktop-portal-wlr running in this session
    (see docs/linux-setup.md). 'synthetic' needs no capture session and always runs.
    Encoded AUs are written to a playable file AND (unless --no-loopback) fed through a
    punktfunk_core host→client loopback that reassembles and byte-verifies each one.
    Both 'serve --native' and 'm3-host' advertise the native service over mDNS
    (_punktfunk._udp) for client auto-discovery — 'punktfunk-client-rs --discover' lists them."
    );
}
