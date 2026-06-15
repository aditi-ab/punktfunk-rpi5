//! `punktfunk-client` — the native Windows punktfunk/1 client.
//!
//! Pure Rust: `NativeClient` linked as a crate (no C ABI, like the GTK Linux client) ·
//! FFmpeg decode · WASAPI audio · SDL3 gamepads · a winit + Direct3D11 present surface. The
//! trust surface mirrors the other native clients: persistent identity, TOFU prompt with the
//! host fingerprint, SPAKE2 PIN pairing.
//!
//! Until the UI shell lands, the binary runs **headless** (`--connect host[:port]`): connect,
//! decode, play audio, and print per-second stats — the Windows analogue of
//! `punktfunk-client-rs`, for validating the protocol/decode path against a live host.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod discovery;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod trust;
#[cfg(windows)]
mod video;

#[cfg(windows)]
fn main() {
    use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
    use std::time::{Duration, Instant};

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

    let Some(target) = arg("--connect") else {
        eprintln!(
            "punktfunk-client (headless): --connect host[:port] [--pin HEX] [--mode WxHxHz] \
             [--bitrate MBPS] [--mic]\n\
             The windowed UI is not wired yet; this runs the protocol/decode path headless."
        );
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
    let pin = arg("--pin").and_then(|h| trust::parse_hex32(&h));
    let bitrate_kbps = arg("--bitrate")
        .and_then(|b| b.parse::<u32>().ok())
        .map(|m| m * 1000)
        .unwrap_or(0);
    let mic_enabled = args.iter().any(|a| a == "--mic");

    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            std::process::exit(1);
        }
    };

    tracing::info!(%host, port, ?mode, tofu = pin.is_none(), "connecting (headless)");
    let handle = session::start(session::SessionParams {
        host,
        port,
        mode,
        compositor: CompositorPref::Auto,
        gamepad: GamepadPref::Auto,
        bitrate_kbps,
        mic_enabled,
        pin,
        identity,
    });

    // Headless consumer: drain events + frames, print stats, run until the host ends or
    // ~60 s elapse (the harness bound). Frames are counted and dropped (no present yet).
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut frames_seen = 0u64;
    loop {
        while let Ok(ev) = handle.events.try_recv() {
            match ev {
                session::SessionEvent::Connected {
                    mode, fingerprint, ..
                } => tracing::info!(
                    ?mode,
                    fp = %trust::hex(&fingerprint),
                    "connected"
                ),
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
