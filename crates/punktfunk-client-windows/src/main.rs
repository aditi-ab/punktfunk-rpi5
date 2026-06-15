//! `punktfunk-client` — the native Windows punktfunk/1 client.
//!
//! Pure Rust: `NativeClient` linked as a crate (no C ABI, like the GTK Linux client) ·
//! FFmpeg decode · WASAPI audio · SDL3 gamepads · a winit window + Direct3D11 flip-model
//! swapchain present surface. The trust surface mirrors the other native clients: persistent
//! identity, trust-on-first-use, SPAKE2 PIN pairing.
//!
//! Usage:
//!   punktfunk-client --connect host[:port] [--pin HEX] [--pair PIN] [--mode WxHxHz]
//!                    [--bitrate MBPS] [--mic]
//!   punktfunk-client --headless --connect …   (no window; count frames + print stats)
//!
//! Trust: an explicit `--pin HEX` (or a host already pinned in the known-hosts store) connects
//! silently; `--pair PIN` runs the SPAKE2 ceremony first; otherwise the connect is
//! trust-on-first-use (the observed fingerprint is pinned on success).

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod discovery;
#[cfg(windows)]
mod gamepad;
#[cfg(windows)]
mod keymap;
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
    use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    if flag("--discover") {
        discover_and_print();
        return;
    }

    let Some(target) = arg("--connect") else {
        eprintln!(
            "punktfunk-client: --connect host[:port] [--pin HEX] [--pair PIN] [--mode WxHxHz] \
             [--bitrate MBPS] [--mic] [--headless]\n\
             punktfunk-client --discover                (list punktfunk hosts on the LAN)"
        );
        std::process::exit(2);
    };

    // Saved settings supply defaults when a CLI flag is absent (the GUI host-list/settings
    // chrome is a follow-up; until then these are the persisted preferences). A CLI flag both
    // overrides and is written back, so the next bare run reuses it.
    let mut settings = trust::Settings::load();
    let (host, port) = match target.rsplit_once(':') {
        Some((a, p)) => (a.to_string(), p.parse().unwrap_or(9777)),
        None => (target.clone(), 9777u16),
    };
    // CLI overrides fold into the persisted settings, then we derive the effective values.
    if let Some(m) = arg("--mode").and_then(|m| {
        let mut it = m.split(['x', 'X']);
        Some((
            it.next()?.parse::<u32>().ok()?,
            it.next()?.parse::<u32>().ok()?,
            it.next()?.parse::<u32>().ok()?,
        ))
    }) {
        (settings.width, settings.height, settings.refresh_hz) = m;
    }
    if let Some(b) = arg("--bitrate").and_then(|b| b.parse::<u32>().ok()) {
        settings.bitrate_kbps = b * 1000;
    }
    if flag("--mic") {
        settings.mic_enabled = true;
    }
    settings.save();
    let mode = if settings.width != 0 && settings.refresh_hz != 0 {
        Mode {
            width: settings.width,
            height: settings.height,
            refresh_hz: settings.refresh_hz,
        }
    } else {
        Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }
    };
    let bitrate_kbps = settings.bitrate_kbps;
    let mic_enabled = settings.mic_enabled;

    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            std::process::exit(1);
        }
    };

    // Resolve trust: explicit pin > already-pinned host > pairing ceremony > TOFU.
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
            std::time::Duration::from_secs(90),
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
                eprintln!("Pairing failed: {e:?} (wrong PIN, or pairing not armed on the host?)");
                std::process::exit(1);
            }
        }
    }

    let headless = flag("--headless");
    // The app-lifetime gamepad service runs only for the windowed client; it also resolves the
    // "Automatic" pad type to whatever physical controller is attached (other-client parity).
    let gamepad_service = (!headless).then(gamepad::GamepadService::start);
    let gamepad_pref = match GamepadPref::from_name(&settings.gamepad) {
        Some(GamepadPref::Auto) | None => gamepad_service
            .as_ref()
            .map_or(GamepadPref::Auto, |s| s.auto_pref()),
        Some(explicit) => explicit,
    };

    tracing::info!(%host, port, ?mode, tofu = pin.is_none(), "connecting");
    let handle = session::start(session::SessionParams {
        host: host.clone(),
        port,
        mode,
        compositor: CompositorPref::Auto,
        gamepad: gamepad_pref,
        bitrate_kbps,
        mic_enabled,
        pin,
        identity,
    });

    if headless {
        run_headless(handle);
        return;
    }

    let info = app::ConnectInfo {
        name: host.clone(),
        addr: host,
        port,
        tofu: pin.is_none(),
    };
    let gamepad_service = gamepad_service.expect("started for the windowed path");
    if let Err(e) = app::WinApp::new(handle, info, gamepad_service).run() {
        tracing::error!(error = %e, "windowed app failed");
        std::process::exit(1);
    }
}

/// Headless runner (`--headless`): drain events + frames, print stats, exit when the host
/// ends or the harness deadline elapses — the Windows analogue of `punktfunk-client-rs`.
#[cfg(windows)]
fn run_headless(handle: session::SessionHandle) {
    use std::time::{Duration, Instant};
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
                session::SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => {
                    tracing::error!(%msg, trust_rejected, "connect failed");
                    if trust_rejected {
                        tracing::error!(
                            "host fingerprint changed or pairing required — re-pair with --pair PIN"
                        );
                    }
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

/// `--discover`: browse the LAN for punktfunk hosts (mDNS) and print them, then exit — the
/// CLI analogue of the GTK client's discovered-hosts list.
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

/// Win32/Direct3D11/WASAPI/SDL3 are Windows turf; this stub keeps `cargo build --workspace`
/// green on Linux/macOS (the other native clients live in crates/punktfunk-client-linux and
/// clients/apple).
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "punktfunk-client-windows is Windows-only — the Linux client lives in \
         crates/punktfunk-client-linux, the macOS client in clients/apple"
    );
    std::process::exit(2);
}
