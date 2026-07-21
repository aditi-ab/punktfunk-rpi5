//! `punktfunk-host` — the Linux streaming host (plan §2, §6, §7).
//!
//! Creates a client-sized virtual display, captures it via PipeWire, encodes with
//! VAAPI/NVENC, and hands encoded access units to `punktfunk_core` for FEC + packetization +
//! pacing + send. Input flows back via libei/uinput. The platform backends are
//! `#[cfg(target_os = "linux")]`; the crate compiles everywhere so the workspace builds
//! on non-Linux dev machines — it just can't run the pipeline there.
//!
//! Subcommands: `serve` runs the native punktfunk/1 host + management REST API by default, and —
//! with `--gamestream` — the GameStream/Moonlight-compat planes too (opt-in, trusted-LAN only);
//! `punktfunk1-host` runs the native punktfunk/1 host standalone; `spike` is a capture→encode→file
//! pipeline dev tool that also round-trips the encoded AUs through a `punktfunk_core` loopback.

// Scaffold: trait methods and config paths are defined ahead of their backends.
#![allow(dead_code)]
// Unsafe-proof program: every `unsafe {}` / `unsafe impl` in the crate must carry a `// SAFETY:`
// proof of why it is sound. This crate-root deny is the permanent, catch-all gate (it also covers
// any future module); individual files keep their own `#![deny(...)]` as belt-and-suspenders.
#![deny(clippy::undocumented_unsafe_blocks)]

mod audio;
mod bringup;
mod capture;
mod detect;
mod devtest;
mod discovery;
mod wol;
// Goal-1 stage 6: top-level platform-only modules live under `src/linux/` and `src/windows/`; `#[path]`
// keeps the `crate::*` module names flat (every existing path is unchanged).
#[cfg(target_os = "windows")]
#[path = "windows/crash.rs"]
mod crash;
#[cfg(target_os = "linux")]
#[path = "linux/drm_sync.rs"]
mod drm_sync;
// The video encode backends live in the `pf-encode` leaf crate (plan §W6); this shim keeps every
// existing `crate::encode::*` path valid (the host is the sole consumer, via the negotiator + the
// GameStream/native/mgmt planes). Feature flags (nvenc/amf-qsv/vulkan-encode/pyrowave) forward to
// pf-encode from this crate's `[features]`.
mod encode {
    pub(crate) use pf_encode::*;
}
mod events;
mod gamestream;
#[cfg(target_os = "linux")]
#[path = "linux/gpuclocks.rs"]
mod gpuclocks;
mod hooks;
// The input-injection backends live in the `pf-inject` subsystem crate (plan §W6); this shim keeps
// every existing `crate::inject::*` path valid (the native/gamestream input planes + devtest consume
// the trait, factory, and per-device backends through it).
mod inject {
    pub(crate) use pf_inject::*;
}
#[cfg(target_os = "windows")]
#[path = "windows/install.rs"]
mod install;
#[cfg(target_os = "windows")]
#[path = "windows/interactive.rs"]
mod interactive;
mod library;
mod log_capture;
mod mgmt;
mod mgmt_token;
mod native;
mod native_pairing;
mod pipeline;
mod plugins;
mod send_pacing;
#[cfg(target_os = "windows")]
#[path = "windows/service.rs"]
mod service;
mod session_plan;
mod session_status;
mod spike;
mod stats_recorder;
// The plugin store: signed catalogs, tiered trust, and install/uninstall jobs that run through the
// same runner CLI the `plugins` subcommand uses (design/plugin-store.md).
mod store;
mod stream_marker;
// `monitor_devnode::startup_recover()` (below) re-enables PnP monitor devnodes disabled by a prior
// run; it lives in the `pf-win-display` leaf crate (plan §W6).
#[cfg(target_os = "windows")]
use pf_win_display::monitor_devnode;
// Virtual-display orchestration lives in the `pf-vdisplay` subsystem crate (plan §W6); this shim
// keeps every existing `crate::vdisplay::*` path valid (serve/mgmt/native/capture consume the trait,
// registry, and manager through it). The DDC panel control + the KWin zkde protocol moved with it.
mod vdisplay {
    pub(crate) use pf_vdisplay::*;
}
// The zero-copy GPU plumbing lives in the `pf-zerocopy` leaf crate (plan §W6); this shim keeps
// every existing `crate::zerocopy::*` path valid for the host's remaining callers (session_plan).
#[cfg(target_os = "linux")]
mod zerocopy {
    pub(crate) use pf_zerocopy::*;
}

use anyhow::{bail, Context, Result};
use encode::Codec;
use spike::{Options, Source};
use std::path::PathBuf;

fn main() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // `service run` is launched by the SCM with no console — log to a file instead of stderr.
    #[cfg(target_os = "windows")]
    let service_run = {
        let a: Vec<String> = std::env::args().skip(1).take(2).collect();
        a.first().map(String::as_str) == Some("service")
            && a.get(1).map(String::as_str) == Some("run")
    };
    #[cfg(not(target_os = "windows"))]
    let service_run = false;

    if service_run {
        #[cfg(target_os = "windows")]
        service::init_file_logging(filter);
    } else {
        // Logs go to stderr so stdout stays machine-readable (`punktfunk-host openapi > spec.json`).
        // A second layer tees DEBUG-and-up into the in-memory ring served by GET /api/v1/logs —
        // deliberately not gated by RUST_LOG, so console-side debugging never needs a restart.
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        log_capture::install_global(
            tracing_subscriber::registry()
                .with(
                    log_capture::RingLayer
                        .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(filter),
                ),
        );
    }

    // Tee every panic through `tracing` BEFORE the default hook: a panicking thread otherwise
    // prints only to stderr — absent from the web console's Logs tab (the ring) and gone entirely
    // when stderr is detached — so a field report reads "host died, zero errors in the logs".
    // The default hook still runs afterwards for the usual stderr message/abort behavior.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Manual payload downcast (`payload_as_str` needs Rust 1.91; workspace MSRV is 1.82).
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        tracing::error!(
            thread = std::thread::current().name().unwrap_or("<unnamed>"),
            location = %info
                .location()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<unknown>".into()),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "PANIC: {payload}"
        );
        default_panic(info);
    }));
    // Native crashes (an access violation inside a GPU runtime/driver DLL) are logged by a
    // last-resort SEH filter for the same reason — they otherwise kill the host with no trace.
    #[cfg(target_os = "windows")]
    crash::install();

    if let Err(e) = real_main() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

/// A lightweight management/CLI subcommand — package/service/driver ops, spec/library dumps — as
/// opposed to a streaming/capture command. These never touch DXGI or run the host, so they skip the
/// startup banner and (on Windows) the GPU-preference hook, whose DPI-awareness probe otherwise
/// prints an alarming `SetProcessDpiAwarenessContext … "access denied"` WARN on a plain
/// `plugins add`. `service run` is the SCM-launched host itself, so it is explicitly NOT lightweight
/// (it must keep the hook — the hybrid-GPU ACCESS_LOST fix depends on it).
fn is_management_cli(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("plugins")
        | Some("driver")
        | Some("web")
        | Some("openapi")
        | Some("library")
        | Some("detect-conflicts")
        | Some("-h")
        | Some("--help")
        | Some("help")
        | None => true,
        Some("service") => args.get(1).map(String::as_str) != Some("run"),
        _ => false,
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--version` prints the build-stamped version (build.rs) to stdout and exits — no logging.
    if matches!(
        args.first().map(String::as_str),
        Some("--version") | Some("-V") | Some("version")
    ) {
        println!("punktfunk-host {}", env!("PUNKTFUNK_VERSION"));
        return Ok(());
    }

    // Lightweight CLI commands (e.g. `plugins add`) get none of the host-startup noise below.
    let management_cli = is_management_cli(&args);

    if !management_cli {
        tracing::info!(
            "punktfunk-host {} (punktfunk_core ABI v{})",
            env!("PUNKTFUNK_VERSION"),
            punktfunk_core::ABI_VERSION
        );
    }

    // Wire pf-vdisplay's display-lifecycle events into the SSE event bus (the subsystem crate emits a
    // neutral DisplayEvent; the orchestrator owns the bus type — plan §W6). Set once, ignore re-set.
    let _ = pf_vdisplay::DISPLAY_EVENT_SINK.set(Box::new(|ev| match ev {
        pf_vdisplay::DisplayEvent::Created {
            backend,
            width,
            height,
            refresh_hz,
        } => events::emit(events::EventKind::DisplayCreated {
            backend,
            mode: events::mode_str(width, height, refresh_hz),
        }),
        pf_vdisplay::DisplayEvent::Released { count } => {
            events::emit(events::EventKind::DisplayReleased { count })
        }
    }));

    // Install the win32u GPU-preference hook (same technique as Apollo, reimplemented — no GPL source
    // copied) BEFORE anything touches DXGI (the virtual-display
    // render-adapter selection creates a DXGI factory during virtual-display setup, well before
    // capture). On a hybrid-GPU box this stops DXGI from reparenting the virtual output off the
    // capture GPU — the ACCESS_LOST churn fix. Idempotent (Once); harmless on non-hybrid boxes.
    // Skipped for lightweight CLI commands (`plugins`, `openapi`, …): they never touch DXGI, and the
    // hook's DPI-awareness probe prints a misleading "access denied" WARN that looks like a failure.
    #[cfg(target_os = "windows")]
    if !management_cli {
        crate::capture::dxgi::install_gpu_pref_hook();
    }

    // NVIDIA clock hygiene (Linux, host subcommands only): install the P2-cap driver profile. The
    // vendor clock *pin* (PUNKTFUNK_PIN_CLOCKS) is no longer held for the host lifetime — it is
    // armed per live client via `gpuclocks::session_pin()` on both streaming planes, so idle clocks
    // are left alone while nobody is connected. No-op off NVIDIA / on the tool subcommands.
    #[cfg(target_os = "linux")]
    if matches!(
        args.first().map(String::as_str),
        Some("serve") | Some("punktfunk1-host")
    ) {
        gpuclocks::on_host_start();
    }

    match args.first().map(String::as_str) {
        // The host: the native punktfunk/1 plane + management API by default (secure), and — with
        // --gamestream — the GameStream/Moonlight-compat planes too (opt-in; #5/#9 trusted-LAN caveat).
        Some("serve") => {
            let (mgmt_opts, native, gamestream) = parse_serve(&args[1..])?;
            // Claim the pf-vdisplay single-instance guard EAGERLY, before any client connects: the
            // claim is first-comer-wins, and a lazily-claiming service could lose its own machine's
            // driver to a stray second host started while the service sat idle.
            #[cfg(target_os = "windows")]
            vdisplay::manager::claim_instance_eagerly();
            // Crash recovery for the experimental `pnp_disable_monitors` axis: re-enable any
            // monitor devnodes a previous host disabled for an Exclusive session and never
            // restored (crash/kill/power loss) — before any new session touches the topology.
            #[cfg(target_os = "windows")]
            monitor_devnode::startup_recover();
            gamestream::serve(mgmt_opts, native, gamestream)
        }
        // Report other Moonlight-compatible hosts (Sunshine/Apollo/…) installed or running on this
        // machine — side-by-side use is unsupported. Exit 1 if any are found (so the installers and
        // support scripts can gate on it), 0 if clean. The host also runs this at `serve` startup.
        Some("detect-conflicts") => {
            let found = detect::scan();
            if found.is_empty() {
                println!("No conflicting game-streaming host detected.");
                Ok(())
            } else {
                print!("{}", detect::render_report(&found));
                std::process::exit(1);
            }
        }
        // Install and run host plugins: `plugins add playnite`, `plugins enable`, … Package ops are
        // forwarded to the bun runner; enable/disable/status drive the systemd unit (Linux) or the
        // PunktfunkScripting scheduled task (Windows). See plugins.rs.
        Some("plugins") => plugins::main(&args[1..]),
        // Print the management API's OpenAPI document (for client codegen).
        Some("openapi") => {
            print!("{}", mgmt::openapi_json());
            Ok(())
        }
        // Dump the resolved game library (installed stores + custom entries) as JSON — the same
        // payload `GET /api/v1/library` serves. A diagnostic for "does the host see my games?".
        Some("library") => {
            println!("{}", serde_json::to_string_pretty(&library::all_games())?);
            Ok(())
        }
        // Standalone input-injection smoke test (no client needed): open the session's input
        // backend and inject a scripted mouse/keyboard pattern. Watch a focused app / `wev`.
        Some("input-test") => devtest::input_test(),
        // Zero-copy FFI/GPU probe: init the EGL importer + CUDA context (no capture needed).
        #[cfg(target_os = "linux")]
        Some("zerocopy-probe") => zerocopy::probe(),
        // Hidden: the isolated GPU-import worker the capture path spawns from a pinned fd to its
        // own executable image (design/zerocopy-worker-isolation.md) — never run by hand; --fd
        // names the inherited socketpair end.
        #[cfg(target_os = "linux")]
        Some("zerocopy-worker") => zerocopy::worker::run_from_args(&args[1..]),
        // NV12 colour self-test (no display/capture needed): convert a known RGBA pattern to NV12
        // on the GPU and compare against a BT.709 limited-range reference. Validates the Tier 2A
        // `PUNKTFUNK_NV12` convert is colour-correct. Prints PASS/FAIL + max Y/U/V error.
        #[cfg(target_os = "linux")]
        Some("nv12-selftest") => zerocopy::nv12_selftest(),
        // HDR P010 colour self-test (Windows; no display/capture needed): upload a known scRGB FP16
        // pattern, run the `HdrP010Converter` shader → P010 on the GPU, read the Y/UV planes back, and
        // compare against an f64 BT.2020-PQ limited-range reference. Validates the
        // `PUNKTFUNK_HDR_SHADER_P010` colour math without green-screening a live HDR stream. Prints
        // PASS/FAIL + max Y/Cb/Cr error.
        #[cfg(target_os = "windows")]
        Some("hdr-p010-selftest") => {
            // Optional args: a `WxH` size (default 64x64 — pass the real capture size: heights
            // like 1080 are NOT 16-aligned and exercise a different driver path) and a GPU
            // vendor (`intel`|`nvidia`|`amd` — dual-GPU boxes otherwise test the default
            // adapter, which may not be the one that encodes).
            let mut size = (64u32, 64u32);
            let mut vendor = None;
            for a in args.iter().skip(2) {
                match a.as_str() {
                    "intel" => vendor = Some(0x8086),
                    "nvidia" => vendor = Some(0x10de),
                    "amd" => vendor = Some(0x1002),
                    s => {
                        let parsed = s
                            .split_once('x')
                            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                        match parsed {
                            Some(wh) => size = wh,
                            None => anyhow::bail!(
                                "hdr-p010-selftest: unrecognized arg {s:?} (want WxH or intel|nvidia|amd)"
                            ),
                        }
                    }
                }
            }
            crate::capture::dxgi::hdr_p010_selftest_at(size.0, size.1, vendor)
        }
        // Linux HDR readiness probe (GNOME 50+ portal path): prints whether a monitor is currently
        // in BT.2100 (HDR) colour mode, whether the NVENC/VAAPI backend probes Main10 for
        // HEVC/AV1, and the GameStream HDR capability the two combine into — the "why isn't my
        // stream HDR?" diagnostic (no display/session needed for the encoder half).
        #[cfg(target_os = "linux")]
        Some("hdr-probe") => {
            let monitor_hdr = pf_capture::gnome_hdr_monitor_active();
            let hevc10 = encode::can_encode_10bit(encode::Codec::H265);
            let av110 = encode::can_encode_10bit(encode::Codec::Av1);
            println!("monitor in BT.2100 (HDR) colour mode: {monitor_hdr}");
            println!("encoder Main10 (HEVC): {hevc10}");
            println!("encoder 10-bit (AV1):  {av110}");
            println!(
                "GameStream HDR capable (PUNKTFUNK_10BIT + video_source=portal + encoder): {}",
                gamestream::host_hdr_capable()
            );
            Ok(())
        }
        // Compositor readiness probe: exit 0 iff the (detected or PUNKTFUNK_COMPOSITOR-forced)
        // compositor is up and able to create a virtual output *now*. A session-bringup
        // script polls this to gate on real readiness instead of a blind `sleep`.
        Some("probe-compositor") => {
            let compositor = vdisplay::detect()?;
            vdisplay::probe(compositor).with_context(|| format!("{compositor:?} not ready"))?;
            println!("{compositor:?} ready");
            Ok(())
        }
        // Create a virtual DualSense via UHID and exercise it (validation, no streaming session).
        #[cfg(target_os = "linux")]
        Some("dualsense-test") => devtest::dualsense_test(&args),
        // Create a virtual Switch Pro Controller via UHID and exercise it (validation, no session).
        #[cfg(target_os = "linux")]
        Some("switchpro-test") => devtest::switchpro_test(&args),
        // Windows N4 SPIKE: hold a software-devnode HID Steam Deck and watch Steam Input promote it.
        #[cfg(target_os = "windows")]
        Some("deck-windows-spike") => devtest::deck_windows_spike(&args),
        // Windows: hold the pf-mouse virtual HID pointer and sweep the real cursor via HID reports
        // (validates the resident-mouse cursor-presence fix on-glass). `--seconds N`.
        #[cfg(target_os = "windows")]
        Some("vmouse-spike") => devtest::vmouse_spike(&args),
        // Windows: create a virtual DualSense (or --ds4/--edge/--deck/--xbox) via the UMDF driver and
        // hold it, driving the real *WindowsManager end to end. `--index N`, `--seconds N`.
        #[cfg(target_os = "windows")]
        Some("dualsense-windows-test") => devtest::dualsense_windows_test(&args),
        // Capture→encode→file pipeline spike (dev tool).
        Some("spike") => spike::run(parse_spike(&args[1..])?),
        // Native punktfunk/1 host (QUIC control plane + UDP data plane).
        Some("punktfunk1-host") => {
            let get = |flag: &str| {
                args.iter()
                    .skip_while(|a| *a != flag)
                    .nth(1)
                    .map(String::as_str)
            };
            let source = match get("--source") {
                Some("virtual") => native::Punktfunk1Source::Virtual,
                _ => native::Punktfunk1Source::Synthetic,
            };
            // Fixed pairing PIN for test harnesses/CI (deterministic ceremony instead of scraping
            // the logged random PIN). An empty value would arm SPAKE2 with an empty password —
            // refuse it loudly, mirroring the --mgmt-token guard.
            let pairing_pin = match get("--pairing-pin") {
                Some(p) if p.trim().is_empty() => bail!("--pairing-pin must not be empty"),
                p => p.map(str::to_string),
            };
            native::run(native::Punktfunk1Options {
                port: get("--port").and_then(|s| s.parse().ok()).unwrap_or(9777),
                source,
                seconds: get("--seconds").and_then(|s| s.parse().ok()).unwrap_or(30),
                frames: get("--frames").and_then(|s| s.parse().ok()).unwrap_or(300),
                max_sessions: get("--max-sessions")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                max_concurrent: get("--max-concurrent")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(native::DEFAULT_MAX_CONCURRENT),
                // Secure by default: REQUIRE PIN pairing (reject unpaired clients) unless
                // --allow-tofu opts into trust-on-first-use — the host then accepts unpaired
                // clients and advertises pair=optional. Pairing is always armed so a PIN is
                // available (logged at startup); `--require-pairing`/`--allow-pairing` are now
                // the default and accepted as no-ops for back-compat.
                require_pairing: !args.iter().any(|a| a == "--allow-tofu"),
                allow_pairing: true,
                pairing_pin,
                paired_store: None,
                // Fixed data-plane port: bind it and stream direct (no hole-punch), removing the
                // ~2.5 s punch-timeout on a firewalled host. Default (absent) = a random port +
                // hole-punch. Also honors PUNKTFUNK_DATA_PORT.
                data_port: get("--data-port")
                    .map(str::to_string)
                    .or_else(|| std::env::var("PUNKTFUNK_DATA_PORT").ok())
                    .and_then(|s| s.parse().ok()),
                // Disconnect-detection latency (QUIC control-connection idle timeout): --idle-timeout-ms
                // overrides PUNKTFUNK_IDLE_TIMEOUT_MS; absent = the core default (8s).
                idle_timeout: get("--idle-timeout-ms")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|&ms| ms > 0)
                    .map(std::time::Duration::from_millis)
                    .or_else(native::idle_timeout_from_env),
                mdns: !args.iter().any(|a| a == "--no-mdns") && discovery::mdns_enabled(),
            })
        }
        // Windows service control: install/uninstall/start/stop/status + the SCM `run` entry point.
        // Replaces the ad-hoc launch chain — `service install` registers an auto-start SYSTEM service
        // that launches the host into the active interactive session.
        #[cfg(target_os = "windows")]
        Some("service") => service::main(&args[1..]),
        // Install-time work the Windows installer delegates to the exe instead of locale-parsed
        // PowerShell *files* (the ANSI-codepage parse-break root fix; see windows/install.rs).
        #[cfg(target_os = "windows")]
        Some("driver") => install::driver_main(&args[1..]),
        #[cfg(target_os = "windows")]
        Some("web") => install::web_main(&args[1..]),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        // Unknown subcommand → usage. (No implicit default; a bare `punktfunk-host` with no
        // args hits the None arm above and prints help.)
        Some(other) => bail!("unknown command '{other}' (try --help)"),
    }
}

/// `serve` options. The **native punktfunk/1 plane + management API are the secure default and always
/// run**; `--gamestream` additionally enables the GameStream/Moonlight-compat planes (opt-in — they
/// carry the inherent on-path #5/#9 weaknesses, so only on a trusted LAN). Returns the mgmt options,
/// the native host config, and whether GameStream is enabled. Native pairing is **required by default**
/// (an open host any LAN device can stream from is insecure); `--open` turns it off.
fn parse_serve(args: &[String]) -> Result<(mgmt::Options, native::NativeServe, bool)> {
    let mut opts = mgmt::Options::default();
    let mut native_port: u16 = 9777; // the native plane always runs now

    // Fixed data-plane UDP port: `Some(p)` binds p and streams direct (no hole-punch, no ~2.5 s
    // punch-timeout on a firewalled host); `None` (default) = a random port + hole-punch. Env
    // default, `--data-port` overrides.
    let mut data_port: Option<u16> = std::env::var("PUNKTFUNK_DATA_PORT")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut open = false;
    let mut gamestream = false;
    let mut no_mdns = false;
    // Did the operator pin the mgmt bind themselves? If not, we LAN-expose the read surface below so
    // paired clients can browse the game library out of the box (the bearer admin surface stays
    // loopback-gated in `mgmt::require_auth` regardless of the bind).
    let mut mgmt_bind_explicit = false;
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
                    .map_err(|_| anyhow::anyhow!("bad --mgmt-bind (want IP:PORT)"))?;
                mgmt_bind_explicit = true;
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
            // The native plane is now the DEFAULT (always runs in `serve`); `--native` is kept as an
            // accepted no-op for back-compat / explicitness.
            "--native" => {}
            "--native-port" => {
                native_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?
            }
            "--data-port" => {
                data_port = Some(
                    next()?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --data-port (want a port number)"))?,
                )
            }
            // Opt into the GameStream/Moonlight-compat planes (off by default — they carry the
            // inherent on-path #5/#9 weaknesses; only for a trusted LAN).
            "--gamestream" | "--moonlight" => gamestream = true,
            // Disable mandatory native pairing — any device can connect (trusted single-user
            // setups only). The default REQUIRES pairing.
            "--open" => open = true,
            // Skip the mDNS adverts (native + GameStream) — multicast-dead environments
            // (bridged Docker, CI netns); clients connect via a manually-added host.
            "--no-mdns" => no_mdns = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    // The mgmt API is HTTPS + token-authenticated ALWAYS (even on loopback). Resolve the token:
    // the --mgmt-token flag (above) wins, else PUNKTFUNK_MGMT_TOKEN env, else the persisted
    // ~/.config/punktfunk/mgmt-token, else a freshly generated + persisted one — so a bare `serve`
    // Just Works with auth on, no operator step, and the bundled web console reads the same file.
    if opts.token.is_none() {
        opts.token = Some(crate::mgmt_token::load_or_generate()?);
    }
    // The scripting runner's scoped credential: minted + persisted (plugin-token) alongside the
    // admin token so a plugin's zero-config `connect()` picks it up — it authorizes the plugin
    // surface but not hook registration or pairing administration (mgmt::auth::plugin_may_access).
    opts.plugin_token = Some(crate::mgmt_token::load_or_generate_plugin()?);
    // Default the mgmt listener to ALL interfaces (not just loopback) so a paired native client can
    // fetch the game library over mTLS with no operator step — the whole point of "browse works by
    // default". This only LAN-exposes the read-only cert allowlist; the bearer-token admin surface
    // is confined to loopback peers in `mgmt::require_auth`, so binding wide adds no admin exposure.
    // An operator who pinned `--mgmt-bind` (e.g. `127.0.0.1:47990` to restore loopback-only) keeps it.
    if !mgmt_bind_explicit {
        opts.bind = std::net::SocketAddr::from(([0, 0, 0, 0], mgmt::DEFAULT_PORT));
    }
    let native = native::NativeServe {
        port: native_port,
        require_pairing: !open,
        // Advertise the mgmt port over mDNS so clients learn where to browse the library (rather than
        // assuming the default). `opts.bind.port()` is the real port even if the operator moved it.
        mgmt_port: opts.bind.port(),
        data_port,
        mdns: !no_mdns && discovery::mdns_enabled(),
    };
    Ok((opts, native, gamestream))
}

fn parse_spike(args: &[String]) -> Result<Options> {
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
                    "synthetic-nv12" => Source::SyntheticNv12,
                    "portal" => Source::Portal,
                    "kwin-virtual" => Source::KwinVirtual,
                    other => {
                        bail!(
                            "unknown --source '{other}' \
                             (synthetic|synthetic-nv12|portal|kwin-virtual)"
                        )
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
            // Raw concatenated PyroWave packets — a lab dump, not an FFmpeg-playable stream.
            Codec::PyroWave => "pyrowave",
        };
        PathBuf::from(format!("/tmp/punktfunk-spike.{ext}"))
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
    punktfunk-host serve [OPTIONS]            native punktfunk/1 host + management REST API
                                              (secure default; add --gamestream for Moonlight compat)
    punktfunk-host plugins <CMD>              install/run host plugins (add, remove, list, enable,
                                              disable, status) — `plugins --help` for details
    punktfunk-host openapi                    print the management API's OpenAPI document (codegen)
    punktfunk-host punktfunk1-host [OPTIONS]  native punktfunk/1 host (QUIC control + UDP data plane)
    punktfunk-host probe-compositor           exit 0 iff the compositor is up + ready (bringup gate)
    punktfunk-host spike [OPTIONS]            capture→encode→file pipeline spike (dev tool)

SERVE OPTIONS:
    --mgmt-bind <IP:PORT>        management API address (default: 0.0.0.0:47990 — paired clients
                                 reach the read-only surface, incl. the game library, over mTLS;
                                 the bearer admin API stays loopback-only. Pin 127.0.0.1:47990 to
                                 bind loopback only)
    --mgmt-token <TOKEN>         bearer token for the management API (or PUNKTFUNK_MGMT_TOKEN); the
                                 admin endpoints it guards are honored only from a loopback peer
                                 (the co-located web console), never over the LAN
    --gamestream  (--moonlight)  ALSO run the GameStream/Moonlight-compat planes (nvhttp pairing,
                                 RTSP, ENet control, _nvstream mDNS). OFF by default — they carry
                                 inherent on-path weaknesses (plain-HTTP pairing + legacy GCM nonce
                                 reuse, security-review #5/#9); enable only on a TRUSTED LAN
    --native                     no-op (the native punktfunk/1 plane always runs in `serve` now)
    --native-port <PORT>         native QUIC port (default 9777)
    --data-port <PORT>           pin the per-session video data plane to this fixed UDP port and
                                 stream direct (no hole-punch) — open exactly this port in a host
                                 firewall to avoid the ~2.5 s punch-timeout. Default (unset) or
                                 PUNKTFUNK_DATA_PORT: a random port + hole-punch (crosses NAT)
    --open                       disable mandatory native pairing (default: pairing REQUIRED —
                                 an open host any LAN device can stream from is insecure)
    --no-mdns                    skip the mDNS adverts (native + GameStream) — for multicast-dead
                                 environments (bridged Docker, CI); clients connect via a manually
                                 added host. Also PUNKTFUNK_MDNS=0

PUNKTFUNK1-HOST OPTIONS:
    --port <N>                   QUIC listen port (default: 9777)
    --source <synthetic|virtual> test frames, or virtual display + NVENC (default: synthetic)
    --seconds <N>                per-session stream duration, virtual source (default: 30)
    --frames <N>                 per-session frame count, synthetic source (default: 300)
    --max-sessions <N>           exit after N sessions; 0 = serve forever (default: 0)
    --max-concurrent <N>         stream at most N sessions at once (NVENC bound); overflow waits
                                 in the accept queue; 0 = unlimited (default: 4)
    --data-port <PORT>           pin the video data plane to this fixed UDP port and stream direct
                                 (no hole-punch; open exactly this port to skip the ~2.5 s punch-
                                 timeout). Default or PUNKTFUNK_DATA_PORT: random port + hole-punch.
                                 A fixed port fits one session; concurrent ones fall back to random
    --allow-tofu                 also accept UNPAIRED clients (trust-on-first-use) and advertise
                                 pair=optional. Default: pairing REQUIRED — the host rejects
                                 unpaired clients and logs a 4-digit pairing PIN at startup;
                                 TOFU without pairing is insecure on a LAN
    --pairing-pin <PIN>          fixed pairing PIN instead of the random per-ceremony one — for
                                 test harnesses/CI (deterministic `probe --pair`); do not use on
                                 a real LAN (a guessable PIN defeats the ceremony's rate limit)
    --no-mdns                    skip the _punktfunk._udp mDNS advert (multicast-dead environments;
                                 clients use --connect HOST:PORT). Also PUNKTFUNK_MDNS=0

SPIKE OPTIONS:
    --source <synthetic|portal|kwin-virtual>
                                 frame source (default: portal). 'kwin-virtual' creates a
                                 KWin virtual output at --width x --height and captures it
    --seconds <N>                capture duration in seconds (default: 5)
    --fps <N>                    target frame rate (default: 60)
    --codec <h264|h265|av1>      NVENC codec (default: h265)
    --bitrate <MBPS>             target bitrate in Mbps (default: 20)
    --width <W> --height <H>     synthetic source size (default: 1920x1080)
    --out <PATH>                 raw Annex-B output (default: /tmp/punktfunk-spike.<ext>)
    --no-loopback                skip the punktfunk_core round-trip verification
    -h, --help                   this help

NOTES:
    'portal' needs headless Sway + xdg-desktop-portal-wlr running in this session
    (see design/linux-setup.md). 'synthetic' needs no capture session and always runs.
    Encoded AUs are written to a playable file AND (unless --no-loopback) fed through a
    punktfunk_core host→client loopback that reassembles and byte-verifies each one.
    Both 'serve' and 'punktfunk1-host' advertise the native service over mDNS
    (_punktfunk._udp) for client auto-discovery — 'punktfunk-probe --discover' lists them."
    );
    #[cfg(target_os = "windows")]
    eprintln!(
        "\nWINDOWS SERVICE (end-user deployment — replaces a manual launch):\n\
        \x20   punktfunk-host service install    register an auto-start SYSTEM service + firewall rules\n\
        \x20   punktfunk-host service uninstall  remove the service + firewall rules\n\
        \x20   punktfunk-host service start|stop|restart|status\n\
        \x20   config: %ProgramData%\\punktfunk\\host.env\n\
        \nWINDOWS DIAGNOSTICS:\n\
        \x20   punktfunk-host hdr-p010-selftest  GPU colour check for the PUNKTFUNK_HDR_SHADER_P010 path\n\
        \x20                                     (scRGB FP16 -> P010 BT.2020 PQ shader vs an f64 reference)"
    );
}
