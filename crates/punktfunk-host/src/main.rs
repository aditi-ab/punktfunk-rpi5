//! Streaming host: virtual display, capture, encode, then FEC + packetize + pace
//! + send via `punktfunk_core`. Input returns through the inject backends.
//!
//! `serve` is the secure default (native punktfunk/1 + management API).
//! `--gamestream` is opt-in, trusted-LAN only. `punktfunk1-host` is the native
//! plane alone. `spike` writes encoded AUs to a file and loopbacks them through
//! `punktfunk_core`.
//!
//! Platform backends are `#[cfg]`; the crate still compiles on every workspace
//! OS. Pin: `design/`. Evidence: this crate's tests and `docs/adr/`.

// Dead methods/paths exist before their backends land.
#![allow(dead_code)]
// Keep `unsafe fn` only where a caller can violate a contract (raw pointer / borrowed HANDLE).
// Workspace lints already require `// SAFETY:` on every `unsafe` block.

mod audio;
mod bringup;
mod capture;
mod detect;
mod devtest;
/// Structured health verdicts — design/web-console-diagnostics.md.
#[forbid(unsafe_code)]
mod diagnostics;
// Network-facing; same `forbid` as `mod mgmt`.
#[forbid(unsafe_code)]
mod discovery;
#[forbid(unsafe_code)]
mod wol;
// `#[path]` keeps `crate::*` names flat while files live under `src/linux/` / `src/windows/`.
#[cfg(target_os = "windows")]
#[path = "windows/crash.rs"]
mod crash;
#[cfg(target_os = "linux")]
#[path = "linux/drm_sync.rs"]
mod drm_sync;
// Shim: encode backends live in `pf-encode`; keep `crate::encode::*` for this crate's callers.
mod encode {
    pub(crate) use pf_encode::*;
}
mod events;
// Session⇄game lifetime — design/session-game-lifetime.md.
mod gamelease;
// WM_CLOSE on the interactive desktop, then TerminateProcess.
#[cfg(target_os = "windows")]
#[path = "windows/game_term.rs"]
mod game_term;
mod gamestream;
#[cfg(target_os = "linux")]
#[path = "linux/gpuclocks.rs"]
mod gpuclocks;
mod hooks;
// Network-facing; same `forbid` as `mod mgmt`. Tests mutate process env (`set_var` is unsafe in 2024).
#[cfg_attr(not(test), forbid(unsafe_code))]
mod identity;
// Shim: inject backends live in `pf-inject`; keep `crate::inject::*` for this crate's callers.
mod inject {
    pub(crate) use pf_inject::*;
}
#[cfg(target_os = "windows")]
#[path = "windows/install.rs"]
mod install;
#[cfg(target_os = "windows")]
#[path = "windows/interactive.rs"]
mod interactive;
mod client_logs;
// Re-`Hello::launch` must not start a second copy — design/session-game-lifetime.md.
mod launchreg;
mod library;
mod log_capture;
// Network-facing secure-default surface. `not(test)` because tests mutate process env
// (`set_var` is unsafe in 2024) and `native` has in-process C-ABI roundtrips.
#[cfg_attr(not(test), forbid(unsafe_code))]
mod mgmt;
#[forbid(unsafe_code)]
mod mgmt_token;
// Loopback client of the surfaces above; same `forbid` (holds the operator token and cert pin).
#[forbid(unsafe_code)]
mod ctl;
#[cfg_attr(not(test), forbid(unsafe_code))]
mod native;
#[forbid(unsafe_code)]
mod native_pairing;
mod osinfo;
mod pipeline;
mod plugins;
mod power;
// Process-table half of session⇄game binding — design/session-game-lifetime.md. Empty on macOS.
mod procscan;
// Plugin-reported liveness; `procscan` only sees the process table.
mod runstate;
mod send_pacing;
#[cfg(target_os = "windows")]
#[path = "windows/service.rs"]
mod service;
mod session_plan;
// Operator policy for session⇄game binding (`session-settings.json`).
mod session_settings;
mod session_status;
mod sleep_inhibit;
mod spike;
mod stats_recorder;
// Per-user tray start/stop/status — the only recovery path after a crash or upgrade.
#[cfg(target_os = "windows")]
#[path = "windows/tray.rs"]
mod tray;
// Signed catalogs and install jobs via the `plugins` runner — design/plugin-store.md.
mod store;
mod stream_marker;
mod update;
#[cfg(target_os = "windows")]
use pf_win_display::monitor_devnode;
#[cfg(target_os = "windows")]
use pf_win_display::win_display::isolate_journal;
// Shim: virtual-display lives in `pf-vdisplay`; keep `crate::vdisplay::*` for this crate's callers.
mod vdisplay {
    pub(crate) use pf_vdisplay::*;
}
// Shim: GPU import lives in `pf-zerocopy`; keep `crate::zerocopy::*` for `session_plan`.
#[cfg(target_os = "linux")]
mod zerocopy {
    pub(crate) use pf_zerocopy::*;
}

use anyhow::{bail, Context, Result};
use encode::Codec;
use spike::{Options, Source};
use std::path::PathBuf;

fn main() {
    // Before any `ureq` agent (cover-art, webhooks, catalog, updates).
    punktfunk_core::tls::install_default_provider();
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // SCM `service run` has no console; log to a file, not stderr.
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
        // stderr so stdout stays machine-readable (`openapi > spec.json`). The ring tees DEBUG+
        // ungated by RUST_LOG so the console Logs tab works without a restart.
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

    // Push panics into the ring before the default hook (stderr is gone when detached).
    // Do not emit via `tracing`: the registry uses TLS, so a destructor-path panic re-enters
    // the hook (`MustAbort::PanicInHook`) and the cause is erased. `LogRing` is OnceLock+Mutex;
    // `thread::current().name()` and `Backtrace::force_capture()` are TLS-teardown safe.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `payload_as_str` needs Rust 1.91; MSRV is 1.82.
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let location = info
            .location()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<unknown>".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        log_capture::ring().push_remote(
            "ERROR",
            "punktfunk_host::panic",
            &format!("PANIC: {payload} (thread={thread}, at {location})\n{backtrace}"),
            ts_ms,
        );
        default_panic(info);
    }));
    // SEH last-resort: a GPU-runtime AV otherwise kills the process with no ring entry.
    #[cfg(target_os = "windows")]
    crash::install();

    if let Err(e) = real_main() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

/// Aim absolute input at the pinned capture monitor (env, else stored policy).
///
/// Called at startup and whenever the console writes the pin so a picker change
/// re-aims without a host restart. Lives here, not in the mirror backend:
/// `pf-vdisplay` must not depend on `pf-inject`, and the injector is host-lifetime
/// (shared by every session) — design/per-monitor-portal-capture.md.
#[cfg(target_os = "linux")]
pub(crate) fn refresh_capture_monitor_anchor(context: &str) {
    let Some(want) = pf_vdisplay::capture_monitor() else {
        // Drop a stale anchor so a later virtual-display session does not inherit it.
        // Log a clear only when an anchor was set; unpinned hosts must stay quiet at startup.
        if pf_inject::absolute_anchor().is_some() {
            tracing::info!(
                context,
                "capture monitor: cleared — sessions create a virtual display again and absolute \
                 input is no longer anchored"
            );
        }
        pf_inject::set_absolute_anchor(None);
        return;
    };
    match pf_vdisplay::detect().and_then(pf_vdisplay::monitors::list) {
        Ok(ms) => match pf_vdisplay::monitors::resolve(&ms, &want) {
            Ok(m) => {
                // Match libei by origin: two heads can share a size; a mirror is not client-sized.
                pf_inject::set_absolute_anchor(Some(pf_inject::AbsoluteAnchor {
                    origin: Some((m.x, m.y)),
                    mapping_id: None,
                }));
                tracing::info!(
                    context,
                    connector = %m.connector,
                    description = %m.description,
                    mode = %m.mode_label(),
                    at = %format!("+{}+{}", m.x, m.y),
                    "capture monitor: sessions will mirror this monitor (no virtual display) and \
                     absolute input is anchored to it"
                );
            }
            // Do not guess an anchor; `create` fails with the same unresolved pin.
            Err(e) => {
                pf_inject::set_absolute_anchor(None);
                tracing::warn!(
                    context,
                    error = %e,
                    "capture monitor: the pinned monitor is not on this host — sessions will fail \
                     to start until it is corrected or cleared"
                );
            }
        },
        Err(e) => {
            pf_inject::set_absolute_anchor(None);
            tracing::warn!(
                context,
                error = %format!("{e:#}"),
                monitor = %want,
                "capture monitor: a monitor is pinned but the monitors could not be enumerated"
            );
        }
    }
}

// Package/service/driver CLI: skip the banner and the Windows GPU-pref hook (its DPI
// probe WARNs `access denied` on `plugins add`). `service run` is the SCM host, not CLI.
fn is_management_cli(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("plugins")
        | Some("driver")
        | Some("web")
        | Some("tray")
        // Loopback API client; `watch` is long-lived — do not take GPU clocks or the DXGI hook.
        | Some("ctl")
        | Some("openapi")
        | Some("library")
        | Some("detect-conflicts")
        // Prints the same list `refresh_capture_monitor_anchor` would log; skip host startup.
        | Some("list-monitors")
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

    if matches!(
        args.first().map(String::as_str),
        Some("--version") | Some("-V") | Some("version")
    ) {
        println!("punktfunk-host {}", env!("PUNKTFUNK_VERSION"));
        return Ok(());
    }

    let management_cli = is_management_cli(&args);

    if !management_cli {
        tracing::info!(
            "punktfunk-host {} (punktfunk_core ABI v{})",
            env!("PUNKTFUNK_VERSION"),
            punktfunk_core::ABI_VERSION
        );
    }

    #[cfg(target_os = "linux")]
    if !management_cli {
        refresh_capture_monitor_anchor("startup");
    }

    // Once: pf-vdisplay emits a crate-neutral `DisplayEvent`; this crate owns the SSE bus type.
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

    // Before DXGI: virtual-display setup creates a factory. Hybrid-GPU boxes otherwise reparent
    // the virtual output off the capture GPU (ACCESS_LOST). Idempotent Once.
    #[cfg(target_os = "windows")]
    if !management_cli {
        crate::capture::dxgi::install_gpu_pref_hook();
    }

    // P2-cap driver profile only. Clock pin is per live client (`gpuclocks::session_pin`), not
    // host-lifetime, so idle clocks stay down. No-op off NVIDIA.
    #[cfg(target_os = "linux")]
    if matches!(
        args.first().map(String::as_str),
        Some("serve") | Some("punktfunk1-host")
    ) {
        gpuclocks::on_host_start();
    }

    match args.first().map(String::as_str) {
        Some("serve") => {
            let (mgmt_opts, native, gamestream) = parse_serve(&args[1..])?;
            // First-comer-wins: claim before any client, or an idle service loses the driver to a stray host.
            #[cfg(target_os = "windows")]
            vdisplay::manager::claim_instance_eagerly();
            // Re-enable PnP monitors a prior Exclusive session disabled and never restored.
            // Must run before any new session touches the topology.
            #[cfg(target_os = "windows")]
            monitor_devnode::startup_recover();
            // Unpin AMD connector emulation a prior host locked. The pin outlives the process
            // (and can outlive a reboot).
            #[cfg(target_os = "windows")]
            pf_win_display::adl_emul::startup_recover();
            // Re-light Exclusive CCD-isolate panels. After the devnode leg so re-enabled
            // monitors exist for the EXTEND preset (the snapshot was process memory).
            #[cfg(target_os = "windows")]
            isolate_journal::startup_recover();
            gamestream::serve(mgmt_opts, native, gamestream)
        }
        Some("detect-conflicts") => {
            let found = detect::scan();
            if found.is_empty() {
                println!("No conflicting game-streaming host detected.");
                return Ok(());
            }
            print!("{}", detect::render_report(&found));
            // Exit 1 only for a host that runs or will auto-start. Dormant leftovers print, then 0
            // (installers gate on this; see `detect` docs and `punktfunk-host.iss`).
            if detect::any_active(&found) {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("ctl") => ctl::main(&args[1..]),
        Some("plugins") => plugins::main(&args[1..]),
        Some("openapi") => {
            print!("{}", mgmt::openapi_json());
            Ok(())
        }
        // Same JSON as `GET /api/v1/library`.
        Some("library") => {
            println!("{}", serde_json::to_string_pretty(&library::all_games())?);
            Ok(())
        }
        Some("input-test") => devtest::input_test(),
        #[cfg(target_os = "linux")]
        Some("pen-test") => devtest::pen_test(),
        #[cfg(target_os = "linux")]
        Some("zerocopy-probe") => zerocopy::probe(),
        // Hidden: capture spawns this from a pinned fd of its own image — design/zerocopy-worker-isolation.md.
        #[cfg(target_os = "linux")]
        Some("zerocopy-worker") => zerocopy::worker::run_from_args(&args[1..]),
        // Hidden: backgrounds beside a nested gamescope app so a fresh headless instance composites
        // from the first second. Needs that session's DISPLAY.
        #[cfg(target_os = "linux")]
        Some("gamescope-splash") => vdisplay::gamescope_splash_client(),
        #[cfg(target_os = "linux")]
        Some("nv12-selftest") => zerocopy::nv12_selftest(),
        #[cfg(target_os = "windows")]
        Some("hdr-p010-selftest") => {
            // `WxH` (default 64×64) and vendor. 1080 is not 16-aligned — a different driver path.
            // Dual-GPU boxes otherwise test the default adapter, not the encoder.
            let mut size = (64u32, 64u32);
            let mut vendor = None;
            // `args` starts at the subcommand (`skip(1)`), so optionals begin at index 1.
            for a in args.iter().skip(1) {
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
        #[cfg(target_os = "linux")]
        Some("hdr-probe") => {
            let monitor_hdr = pf_capture::gnome_hdr_monitor_active();
            let hevc10 = encode::can_encode_10bit(encode::Codec::H265);
            let av110 = encode::can_encode_10bit(encode::Codec::Av1);
            let gs_binary_hdr = pf_vdisplay::gamescope_hdr_available();
            let gs_knob = pf_host_config::config().gamescope_hdr;
            let compositor = vdisplay::detect().ok();
            println!("monitor in BT.2100 (HDR) colour mode: {monitor_hdr}");
            println!("gamescope offers 10-bit PQ capture:   {gs_binary_hdr}");
            println!("PUNKTFUNK_GAMESCOPE_HDR:              {gs_knob}");
            // In-node cursor lets the session take the zero-CSC encode source; otherwise a
            // full-frame blend. Invisible until you compare two streams, so print it here.
            println!(
                "gamescope paints the cursor in-node:  {}",
                pf_vdisplay::gamescope_composites_cursor()
            );
            println!("encoder Main10 (HEVC): {hevc10}");
            println!("encoder 10-bit (AV1):  {av110}");
            println!(
                "native-plane HDR on the resolved compositor ({}): {}",
                compositor.map_or("none".to_string(), |c| format!("{c:?}")),
                crate::capture::capturer_supports_hdr_for(compositor)
            );
            println!(
                "GameStream HDR capable (PUNKTFUNK_10BIT + a capable source + encoder): {}",
                gamestream::host_hdr_capable()
            );
            Ok(())
        }
        // Exit 0 iff a virtual output can be created now — bringup scripts poll this instead of `sleep`.
        Some("probe-compositor") => {
            let compositor = vdisplay::detect()?;
            vdisplay::probe(compositor).with_context(|| format!("{compositor:?} not ready"))?;
            println!("{compositor:?} ready");
            Ok(())
        }
        // Connector names `PUNKTFUNK_CAPTURE_MONITOR` takes — available before the mgmt API is up.
        #[cfg(target_os = "linux")]
        Some("list-monitors") => {
            let compositor = vdisplay::detect()?;
            let monitors = vdisplay::monitors::list(compositor)
                .with_context(|| format!("enumerate monitors on {compositor:?}"))?;
            if monitors.is_empty() {
                println!("{compositor:?}: no monitors");
                return Ok(());
            }
            let pinned = vdisplay::capture_monitor();
            println!("{compositor:?}:");
            for m in &monitors {
                let mut tags = Vec::new();
                if m.primary {
                    tags.push("primary");
                }
                if !m.enabled {
                    tags.push("disabled");
                }
                if m.managed {
                    tags.push("punktfunk virtual display");
                }
                if pinned
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(&m.connector))
                {
                    tags.push("PINNED");
                }
                println!(
                    "  {:<12} {:>13} at +{},+{}  scale {}  {}{}",
                    m.connector,
                    m.mode_label(),
                    m.x,
                    m.y,
                    m.scale,
                    m.description,
                    if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", tags.join(", "))
                    }
                );
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Some("mirror-test") => devtest::mirror_test(&args),
        #[cfg(target_os = "linux")]
        Some("anchor-test") => devtest::anchor_test(&args),
        #[cfg(target_os = "linux")]
        Some("dualsense-test") => devtest::dualsense_test(&args),
        #[cfg(target_os = "linux")]
        Some("pad-sink-test") => devtest::pad_sink_test(&args),
        #[cfg(target_os = "linux")]
        Some("pad-usbip-test") => devtest::pad_usbip_test(&args),
        #[cfg(target_os = "linux")]
        Some("switchpro-test") => devtest::switchpro_test(&args),
        #[cfg(target_os = "windows")]
        Some("deck-windows-spike") => devtest::deck_windows_spike(&args),
        #[cfg(target_os = "windows")]
        Some("vmouse-spike") => devtest::vmouse_spike(&args),
        #[cfg(target_os = "windows")]
        Some("channel-proof-probe") => devtest::channel_proof_probe(&args),
        #[cfg(target_os = "windows")]
        Some("dualsense-windows-test") => devtest::dualsense_windows_test(&args),
        #[cfg(target_os = "windows")]
        Some("pad-endpoint") => devtest::pad_endpoint(&args),
        #[cfg(target_os = "windows")]
        Some("audio-probe") => devtest::audio_probe(&args),
        Some("spike") => spike::run(parse_spike(&args[1..])?),
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
            // Empty would arm SPAKE2 with an empty password (same trap as `--mgmt-token`).
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
                // Pairing required unless `--allow-tofu`. `--require-pairing`/`--allow-pairing` are no-ops.
                require_pairing: !args.iter().any(|a| a == "--allow-tofu"),
                allow_pairing: true,
                pairing_pin,
                paired_store: None,
                // Fixed port: direct send, no ~2.5 s punch-timeout on a firewalled host. Absent = random + punch.
                data_port: get("--data-port")
                    .map(str::to_string)
                    .or_else(|| std::env::var("PUNKTFUNK_DATA_PORT").ok())
                    .and_then(|s| s.parse().ok()),
                // QUIC idle timeout; flag overrides env; absent = core default (8 s).
                idle_timeout: get("--idle-timeout-ms")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|&ms| ms > 0)
                    .map(std::time::Duration::from_millis)
                    .or_else(native::idle_timeout_from_env),
                mdns: !args.iter().any(|a| a == "--no-mdns") && discovery::mdns_enabled(),
            })
        }
        #[cfg(target_os = "windows")]
        Some("service") => service::main(&args[1..]),
        // Installer work in-process: locale-parsed PowerShell files break on ANSI codepages.
        #[cfg(target_os = "windows")]
        Some("driver") => install::driver_main(&args[1..]),
        #[cfg(target_os = "windows")]
        Some("web") => install::web_main(&args[1..]),
        // HKLM Run fires only at sign-in; this is how an upgrade/crash gets the icon back.
        #[cfg(target_os = "windows")]
        Some("tray") => tray::main(&args[1..]),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        // No implicit `serve`; bare invocation is `None` above and prints help.
        Some(other) => bail!("unknown command '{other}' (try --help)"),
    }
}

/// Native plane + management API always run. `--gamestream` is trusted-LAN only.
/// Pairing is required unless `--open`. Returns `(mgmt, native, gamestream)`.
fn parse_serve(args: &[String]) -> Result<(mgmt::Options, native::NativeServe, bool)> {
    let mut opts = mgmt::Options::default();
    let mut native_port: u16 = 9777;

    // Env default; `--data-port` overrides. `Some` = direct bind; `None` = random + hole-punch (~2.5 s).
    let mut data_port: Option<u16> = std::env::var("PUNKTFUNK_DATA_PORT")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut open = false;
    let mut gamestream = false;
    let mut no_mdns = false;
    // If unset, bind wide below so paired clients can browse. Admin stays loopback in `require_auth`.
    let mut mgmt_bind_explicit = false;
    // Explicit `--native-port` outranks `PUNKTFUNK_NATIVE_PORT` after the loop.
    let mut native_port_explicit = false;
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
                // Empty satisfies "token required" while authenticating nobody (`"$UNSET_VAR"`).
                if token.trim().is_empty() {
                    bail!("--mgmt-token must not be empty");
                }
                opts.token = Some(token);
            }
            // No-op: the native plane always runs.
            "--native" => {}
            "--native-port" => {
                native_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?;
                native_port_explicit = true;
            }
            "--data-port" => {
                data_port = Some(
                    next()?
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --data-port (want a port number)"))?,
                )
            }
            "--gamestream" | "--moonlight" => gamestream = true,
            "--open" => open = true,
            // Bridged Docker / CI netns: multicast never arrives.
            "--no-mdns" => no_mdns = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    // Flag, else env, else persisted `mgmt-token`, else generate. HTTPS+token even on loopback.
    if opts.token.is_none() {
        opts.token = Some(crate::mgmt_token::load_or_generate()?);
    }
    // Mint only if the runner is installed — otherwise a second admin-adjacent credential sits
    // on disk for a subsystem that is not running. Scope: `plugin_may_access`, not pairing/hooks.
    if crate::plugins::runtime_status().installed {
        opts.plugin_token = Some(crate::mgmt_token::load_or_generate_plugin()?);
    }
    // Default all-interfaces so paired clients browse over mTLS. Admin stays loopback in
    // `require_auth`. Packaged units ship a fixed ExecStart — `host.env` is the upgrade-safe pin;
    // CLI wins as the more explicit of the two.
    if !mgmt_bind_explicit {
        opts.bind = match pf_host_config::config().mgmt_bind.as_deref() {
            Some(s) => s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_MGMT_BIND '{s}' (want IP:PORT)"))?,
            None => std::net::SocketAddr::from(([0, 0, 0, 0], mgmt::DEFAULT_PORT)),
        };
    }
    // A bad value is fatal — serving 9777 while host.env says otherwise reads as
    // "I moved the port and the client still cannot reach me".
    if !native_port_explicit {
        if let Some(s) = pf_host_config::config().native_port.as_deref() {
            native_port = s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_NATIVE_PORT '{s}' (want a port)"))?;
        }
    }
    // Same function as the token persist so the console unit sees both. A race falls back to
    // 47990; `Restart=always` retries.
    mgmt::publish_endpoint(opts.bind);
    let native = native::NativeServe {
        port: native_port,
        require_pairing: !open,
        // Real bound port, not the default, so mDNS clients follow a moved mgmt port.
        mgmt_port: opts.bind.port(),
        data_port,
        mdns: !no_mdns && discovery::mdns_enabled(),
    };
    // CLI or `PUNKTFUNK_GAMESTREAM`. Packaged units ship native-only ExecStart; env is the pin.
    let gamestream = gamestream || pf_host_config::config().gamestream;
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
    let mut wire_chunk: Option<usize> = None;

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
                    // Needs `pyrowave` and `PUNKTFUNK_ENCODER=pyrowave` (raw-dmabuf passthrough).
                    "pyrowave" => Codec::PyroWave,
                    other => bail!("unknown --codec '{other}' (h264|h265|av1|pyrowave)"),
                }
            }
            "--bitrate" => {
                bitrate_mbps = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --bitrate (Mbps)"))?
            }
            "--out" => out = Some(PathBuf::from(next()?)),
            "--no-loopback" => loopback = false,
            "--wire-chunk" => {
                let v: usize = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --wire-chunk (bytes)"))?;
                wire_chunk = (v > 0).then_some(v);
            }
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
            // Concatenated packets; not an FFmpeg-playable stream.
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
        wire_chunk,
    })
}

fn print_usage() {
    eprintln!(
        "punktfunk-host — Linux streaming host

USAGE:
    punktfunk-host serve [OPTIONS]            native punktfunk/1 host + management REST API
                                              (secure default; add --gamestream for Moonlight compat)
    punktfunk-host ctl <VERB>                 operator control over the local management API —
                                              pairing, devices, sessions, `watch` (line-JSON for a
                                              shell widget); `ctl --help` for the verb list
    punktfunk-host plugins <CMD>              install/run host plugins (add, remove, list, enable,
                                              disable, status) — `plugins --help` for details
    punktfunk-host tray <CMD>                 status-tray lifecycle (start, stop, status) — Windows;
                                              `start` is how you get the icon back without a re-logon
    punktfunk-host openapi                    print the management API's OpenAPI document (codegen)
    punktfunk-host punktfunk1-host [OPTIONS]  native punktfunk/1 host (QUIC control + UDP data plane)
    punktfunk-host probe-compositor           exit 0 iff the compositor is up + ready (bringup gate)
    punktfunk-host list-monitors              list the host's physical monitors (Linux) — the
                                              connector names PUNKTFUNK_CAPTURE_MONITOR takes
    punktfunk-host spike [OPTIONS]            capture→encode→file pipeline spike (dev tool)

SERVE OPTIONS:
    --mgmt-bind <IP:PORT>        management API address (or PUNKTFUNK_MGMT_BIND in host.env, which
                                 this flag overrides). Default: 0.0.0.0:47990 — paired clients
                                 reach the read-only surface, incl. the game library, over mTLS;
                                 the bearer admin API stays loopback-only. Pin 127.0.0.1:47990 to
                                 bind loopback only. Move the PORT (e.g. 0.0.0.0:47991) to share a
                                 machine with Sunshine/Apollo/Vibeshine, whose web UI owns 47990 —
                                 clients follow via mDNS and the console via mgmt-endpoint
    --mgmt-token <TOKEN>         bearer token for the management API (or PUNKTFUNK_MGMT_TOKEN); the
                                 admin endpoints it guards are honored only from a loopback peer
                                 (the co-located web console), never over the LAN
    --gamestream  (--moonlight)  ALSO run the GameStream/Moonlight-compat planes (nvhttp pairing,
                                 RTSP, ENet control, _nvstream mDNS). OFF by default — they carry
                                 inherent on-path weaknesses (plain-HTTP pairing + legacy GCM nonce
                                 reuse, security-review #5/#9); enable only on a TRUSTED LAN.
                                 Also PUNKTFUNK_GAMESTREAM=1 in host.env (how a packaged install
                                 opts in — the shipped units run native-only)
    --native                     no-op (the native punktfunk/1 plane always runs in `serve` now)
    --native-port <PORT>         native QUIC port (or PUNKTFUNK_NATIVE_PORT in host.env, which
                                 this flag overrides). Default 9777. Clients follow via mDNS, and
                                 a manually-added host keeps whatever port it was added with
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
    --codec <h264|h265|av1|pyrowave>
                                 encode codec (default: h265). 'pyrowave' also wants
                                 PUNKTFUNK_ENCODER=pyrowave so capture takes the passthrough
    --bitrate <MBPS>             target bitrate in Mbps (default: 20)
    --width <W> --height <H>     synthetic source size (default: 1920x1080)
    --out <PATH>                 raw Annex-B output (default: /tmp/punktfunk-spike.<ext>)
    --no-loopback                skip the punktfunk_core round-trip verification
    --wire-chunk <BYTES>         PyroWave datagram-aligned packetization at this shard payload
                                 (a real session passes its negotiated shard_payload, e.g. 1408).
                                 With PUNKTFUNK_PYROWAVE_STREAMED_AU=1 also armed, the AU is
                                 drained through poll_chunk and sealed as a STREAMED wire frame
                                 (VIDEO_CAP_STREAMED_AU), then byte-verified by the loopback
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
