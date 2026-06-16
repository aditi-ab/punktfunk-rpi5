//! `punktfunk-client` — the native Windows punktfunk/1 client.
//!
//! Pure Rust: `NativeClient` linked as a crate (no C ABI, like the GTK Linux client) · FFmpeg
//! decode · WASAPI audio · SDL3 gamepads · a **WinUI 3** shell (windows-reactor) with the video
//! on a `SwapChainPanel` bound to a D3D11 composition swapchain. The trust surface mirrors the
//! other native clients: persistent identity, trust-on-first-use, SPAKE2 PIN pairing — all in-app
//! (host list, settings, pairing). `--headless` keeps a CLI connect path for tests/measurement.
//!
//! Usage:
//!   punktfunk-client                          (open the WinUI 3 window: host list, settings, pairing)
//!   punktfunk-client --discover               (list punktfunk hosts on the LAN)
//!   punktfunk-client --headless --connect host[:port] [--pin HEX] [--pair PIN] [--mode WxHxHz]
//!                    [--bitrate MBPS] [--mic]  (no window; count frames + print stats)

// Link as a GUI (windows) subsystem binary so the default windowed launch (MSIX / double-click)
// does NOT pop a console window. The CLI paths (--headless/--discover) reattach to the launching
// terminal's console at startup (see main), so their output is still visible when run from a shell.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod discovery;
#[cfg(windows)]
mod gamepad;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod present;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod trust;
#[cfg(windows)]
mod video;

#[cfg(windows)]
fn main() {
    // With #![windows_subsystem = "windows"] the process starts with no console, so the GUI/MSIX
    // launch is window-free. AttachConsole only binds to an ALREADY-EXISTING parent console (it
    // never creates one), so when launched from a terminal — `--headless`/`--discover` — stdout and
    // the tracing writer below land in that terminal; from Explorer/MSIX it's a harmless no-op.
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);

    if flag("--discover") {
        discover_and_print();
        return;
    }

    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            std::process::exit(1);
        }
    };

    if flag("--headless") {
        run_headless_cli(&args, identity);
        return;
    }

    // Windowed (default): the WinUI 3 app owns host selection, settings, and pairing.
    let gamepad = gamepad::GamepadService::start();
    if let Err(e) = app::run(identity, gamepad) {
        tracing::error!(error = %e, "WinUI app failed");
        std::process::exit(1);
    }
}

/// `--headless --connect host[:port] …`: connect from the CLI, count frames, print stats — the
/// Windows analogue of `punktfunk-client-rs`.
#[cfg(windows)]
fn run_headless_cli(args: &[String], identity: (String, String)) {
    use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
    use std::time::{Duration, Instant};

    let arg = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    let Some(target) = arg("--connect") else {
        eprintln!("--headless requires --connect host[:port]");
        std::process::exit(2);
    };
    let (host, port) = match target.rsplit_once(':') {
        Some((a, p)) => (a.to_string(), p.parse().unwrap_or(9777)),
        None => (target.clone(), 9777u16),
    };
    let mode = arg("--mode")
        .and_then(|m| {
            let mut it = m.split(['x', 'X']);
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
    let bitrate_kbps = arg("--bitrate")
        .and_then(|b| b.parse::<u32>().ok())
        .map(|m| m * 1000)
        .unwrap_or(0);

    let known = trust::KnownHosts::load();
    let mut pin = arg("--pin")
        .and_then(|h| trust::parse_hex32(&h))
        .or_else(|| {
            known
                .find_by_addr(&host, port)
                .and_then(|k| trust::parse_hex32(&k.fp_hex))
        });
    if let Some(code) = arg("--pair") {
        let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows-client".into());
        match punktfunk_core::client::NativeClient::pair(
            &host,
            port,
            (&identity.0, &identity.1),
            code.trim(),
            &name,
            Duration::from_secs(90),
        ) {
            Ok(fp) => {
                let mut k = trust::KnownHosts::load();
                k.upsert(trust::KnownHost {
                    name: host.clone(),
                    addr: host.clone(),
                    port,
                    fp_hex: trust::hex(&fp),
                    paired: true,
                });
                let _ = k.save();
                tracing::info!(fp = %trust::hex(&fp), "paired");
                pin = Some(fp);
            }
            Err(e) => {
                eprintln!("Pairing failed: {e:?}");
                std::process::exit(1);
            }
        }
    }

    tracing::info!(%host, port, ?mode, tofu = pin.is_none(), "connecting (headless)");
    let handle = session::start(session::SessionParams {
        host,
        port,
        mode,
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps,
        mic_enabled: flag("--mic"),
        pin,
        identity,
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut frames_seen = 0u64;
    loop {
        while let Ok(ev) = handle.events.try_recv() {
            match ev {
                session::SessionEvent::Connected {
                    mode, fingerprint, ..
                } => tracing::info!(?mode, fp = %trust::hex(&fingerprint), "connected"),
                session::SessionEvent::Stats(s) => tracing::info!(
                    fps = format!("{:.0}", s.fps),
                    mbps = format!("{:.1}", s.mbps),
                    decode_ms = format!("{:.2}", s.decode_ms),
                    lat_ms = format!("{:.2}", s.latency_ms),
                    frames_seen,
                    "stats"
                ),
                session::SessionEvent::Failed { msg, .. } => {
                    tracing::error!(%msg, "connect failed");
                    return;
                }
                session::SessionEvent::Ended(err) => {
                    tracing::info!(reason = err.as_deref().unwrap_or("done"), "session ended");
                    return;
                }
            }
        }
        while handle.frames.try_recv().is_ok() {
            frames_seen += 1;
        }
        if Instant::now() > deadline {
            tracing::info!(frames_seen, "harness deadline — stopping");
            handle.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `--discover`: browse the LAN for punktfunk hosts (mDNS) and print them, then exit.
#[cfg(windows)]
fn discover_and_print() {
    use std::time::{Duration, Instant};
    println!("Browsing the LAN for punktfunk hosts (~5 s)…");
    let rx = discovery::browse();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = std::collections::HashSet::new();
    while Instant::now() < deadline {
        while let Ok(h) = rx.try_recv() {
            if seen.insert(h.key.clone()) {
                println!(
                    "  {}  {}:{}  pair={}  fp={}",
                    h.name,
                    h.addr,
                    h.port,
                    if h.pair.is_empty() {
                        "optional"
                    } else {
                        &h.pair
                    },
                    if h.fp_hex.is_empty() { "-" } else { &h.fp_hex },
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if seen.is_empty() {
        println!("  (none found — is a host running with --native / m3-host?)");
    }
}

/// WinUI 3 / Direct3D11 / WASAPI / SDL3 are Windows turf; this stub keeps `cargo build
/// --workspace` green on Linux/macOS (the other native clients live in
/// crates/punktfunk-client-linux and clients/apple).
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "punktfunk-client-windows is Windows-only — the Linux client lives in \
         crates/punktfunk-client-linux, the macOS client in clients/apple"
    );
    std::process::exit(2);
}
