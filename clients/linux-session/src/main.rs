//! `punktfunk-session` — the Vulkan session binary (punktfunk-planning
//! `linux-client-rearchitecture.md`, Phase 1: the software-path presenter MVP, which IS
//! the power-user CLI build).
//!
//! One stream session per invocation: `--connect host[:port]` (+ `--fp HEX`,
//! `--launch id`, `--fullscreen`), exits when the session ends. Reads the same identity
//! / known-hosts / settings stores as the GTK client (`punktfunk-client`), so pairing
//! there (or via its headless `--pair`) makes this binary connect silently.
//!
//! Stdout is the machine interface (the shell↔session contract): `{"ready":true}` after
//! the first presented frame, `stats:` lines per 1 s window, one `{"error": …}` /
//! `{"ended": …}` JSON line on the way out. Logs go to stderr. Exit codes: 0 clean end,
//! 2 connect failed, 3 trust rejected / pairing required, 4 presenter init failed.

#[cfg(all(target_os = "linux", feature = "ui"))]
mod browse;

#[cfg(target_os = "linux")]
mod session_main {
    use pf_client_core::gamepad::GamepadService;
    use pf_client_core::session::SessionParams;
    use pf_client_core::trust;
    use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    pub const EXIT_CONNECT_FAILED: u8 = 2;
    pub const EXIT_TRUST_REJECTED: u8 = 3;
    pub const EXIT_PRESENTER_FAILED: u8 = 4;

    /// The value following `flag` in argv, if present (`--flag value`).
    pub(crate) fn arg_value(flag: &str) -> Option<String> {
        std::env::args()
            .skip_while(|a| a != flag)
            .nth(1)
            .filter(|v| !v.starts_with("--"))
    }

    pub(crate) fn arg_flag(flag: &str) -> bool {
        std::env::args().any(|a| a == flag)
    }

    /// Run fullscreen: `--fullscreen`, or the Deck/gamescope env as a fallback so a
    /// manual launch under Gaming Mode does the right thing too.
    pub(crate) fn fullscreen_mode() -> bool {
        arg_flag("--fullscreen")
            || std::env::var_os("SteamDeck").is_some()
            || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
    }

    /// `host[:port]`, port defaulting to the native 9777.
    pub(crate) fn parse_host_port(target: &str) -> (String, u16) {
        match target.rsplit_once(':') {
            Some((a, p)) => match p.parse() {
                Ok(port) => (a.to_string(), port),
                Err(_) => {
                    eprintln!("unparsable port in '{target}', using default 9777");
                    (a.to_string(), 9777)
                }
            },
            None => (target.to_string(), 9777),
        }
    }

    /// One session's pump parameters from the Settings store — shared by `--connect`
    /// and every `--browse` launch. Explicit settings, `0` fields resolved to the
    /// window's display (the GTK client reads the monitor under its window — same
    /// contract).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn session_params(
        settings: &trust::Settings,
        addr: String,
        port: u16,
        pin: [u8; 32],
        identity: (String, String),
        launch: Option<String>,
        gamepad: &GamepadService,
        native: Mode,
        force_software: Arc<AtomicBool>,
    ) -> SessionParams {
        let mode = Mode {
            width: if settings.width == 0 {
                native.width
            } else {
                settings.width
            },
            height: if settings.width == 0 {
                native.height
            } else {
                settings.height
            },
            refresh_hz: if settings.refresh_hz == 0 {
                native.refresh_hz.max(30)
            } else {
                settings.refresh_hz
            },
        };
        SessionParams {
            host: addr,
            port,
            mode,
            compositor: CompositorPref::from_name(&settings.compositor)
                .unwrap_or(CompositorPref::Auto),
            gamepad: match GamepadPref::from_name(&settings.gamepad) {
                Some(GamepadPref::Auto) | None => gamepad.auto_pref(),
                Some(explicit) => explicit,
            },
            bitrate_kbps: settings.bitrate_kbps,
            audio_channels: settings.audio_channels,
            preferred_codec: settings.preferred_codec(),
            mic_enabled: settings.mic_enabled,
            // The Settings preference (auto → VAAPI where it exists; the presenter
            // demotes to software on boxes whose Vulkan can't import the dmabufs).
            // PUNKTFUNK_DECODER still overrides inside the decoder for bisects.
            decoder: settings.decoder.clone(),
            launch,
            pin: Some(pin),
            identity,
            connect_timeout: Duration::from_secs(15),
            force_software,
        }
    }

    /// One JSON status line on stdout (the shell parses these; strings hand-escaped via
    /// the minimal rules a reason string can need).
    fn json_line(key: &str, msg: &str, trust_rejected: Option<bool>) {
        let escaped: String = msg
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                '\n' => vec!['\\', 'n'],
                c if (c as u32) < 0x20 => vec![' '],
                c => vec![c],
            })
            .collect();
        match trust_rejected {
            Some(t) => println!("{{\"{key}\":\"{escaped}\",\"trust_rejected\":{t}}}"),
            None => println!("{{\"{key}\":\"{escaped}\"}}"),
        }
    }

    pub fn run() -> u8 {
        // Logs to STDERR — stdout is the machine interface (ready/stats/error lines).
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        // Steam launches its shortcuts with SDL_GAMECONTROLLER_IGNORE_DEVICES naming
        // every pad Steam Input has virtualized; capturing the Deck's real built-in
        // controller needs it cleared (same rationale as the GTK client's `app::run`).
        for var in [
            "SDL_GAMECONTROLLER_IGNORE_DEVICES",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT",
        ] {
            if let Ok(v) = std::env::var(var) {
                tracing::info!(var, value = %v, "clearing Steam's SDL device filter");
                std::env::remove_var(var);
            }
        }

        if let Some(target) = arg_value("--browse") {
            #[cfg(feature = "ui")]
            return crate::browse::run(&target);
            #[cfg(not(feature = "ui"))]
            {
                let _ = target;
                eprintln!(
                    "--browse needs the console UI — this is the minimal build \
                     (rebuild without --no-default-features)"
                );
                return EXIT_PRESENTER_FAILED;
            }
        }
        let Some(target) = arg_value("--connect") else {
            eprintln!(
                "usage: punktfunk-session --connect host[:port] [--fp HEX] [--launch id] [--fullscreen]\n\
                 \x20      punktfunk-session --browse host[:port] [--mgmt PORT] [--fullscreen]\n\
                 \n\
                 Streams from a paired punktfunk host in a Vulkan window; --browse opens the\n\
                 console game library instead (paired hosts only). Pair first via the\n\
                 desktop client or `punktfunk-client --pair <PIN> --connect host[:port]` —\n\
                 this binary never connects to a host it has no pinned fingerprint for."
            );
            return EXIT_CONNECT_FAILED;
        };
        let (addr, port) = parse_host_port(&target);

        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                json_line("error", &format!("client identity: {e:#}"), None);
                return EXIT_CONNECT_FAILED;
            }
        };
        let settings = trust::Settings::load();

        // Trust follows the GTK client's `--connect` rules: a stored (or `--fp`) pin
        // connects silently; an unknown host is REFUSED — there is no dialog here, and a
        // silent TOFU would defeat the pinning model. Pair via the desktop client.
        let known = trust::KnownHosts::load();
        let known_host = known.hosts.iter().find(|h| h.addr == addr && h.port == port);
        let pin = arg_value("--fp")
            .as_deref()
            .and_then(trust::parse_hex32)
            .or_else(|| known_host.and_then(|h| trust::parse_hex32(&h.fp_hex)));
        let Some(pin) = pin else {
            json_line(
                "error",
                &format!(
                    "no pinned fingerprint for {addr}:{port} — pair first \
                     (punktfunk-client --pair <PIN> --connect {addr}:{port}) or pass --fp HEX"
                ),
                Some(true),
            );
            return EXIT_TRUST_REJECTED;
        };

        let host_label = known_host.map_or_else(|| addr.clone(), |h| h.name.clone());
        let launch = arg_value("--launch");
        let title = launch
            .clone()
            .map_or_else(|| host_label.clone(), |id| format!("{host_label} · {id}"));

        let fullscreen = arg_flag("--fullscreen")
            || std::env::var_os("SteamDeck").is_some()
            || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some();

        let opts = pf_presenter::SessionOpts {
            window_title: format!("Punktfunk · {title}"),
            fullscreen,
            print_stats: settings.show_stats || arg_flag("--stats"),
            json_status: true,
            on_connected: Some(Box::new(|fingerprint: [u8; 32]| {
                // This host's card carries the accent bar in the desktop client now.
                trust::touch_last_used(&trust::hex(&fingerprint));
            })),
            // The Skia console UI (stats OSD, capture HUD) — compiled out of the
            // power-user build (`--no-default-features` drops the `ui` feature).
            #[cfg(feature = "ui")]
            overlay: Some(Box::new(pf_console_ui::SkiaOverlay::new())),
            #[cfg(not(feature = "ui"))]
            overlay: None,
        };

        let outcome = pf_presenter::run_session(opts, move |gamepad, native, force_software| {
            session_params(
                &settings,
                addr,
                port,
                pin,
                identity,
                launch,
                gamepad,
                native,
                force_software,
            )
        });

        match outcome {
            Ok(pf_presenter::Outcome::Ended(None)) => 0,
            Ok(pf_presenter::Outcome::Ended(Some(reason))) => {
                // The host ending the session (game quit, host shutdown) is a normal end
                // for a one-shot stream binary — report the reason, exit clean.
                json_line("ended", &reason, None);
                0
            }
            Ok(pf_presenter::Outcome::ConnectFailed { msg, trust_rejected }) => {
                json_line("error", &msg, Some(trust_rejected));
                if trust_rejected {
                    EXIT_TRUST_REJECTED
                } else {
                    EXIT_CONNECT_FAILED
                }
            }
            Err(e) => {
                json_line("error", &format!("presenter: {e:#}"), None);
                EXIT_PRESENTER_FAILED
            }
        }
    }

}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(session_main::run())
}

/// Vulkan/SDL3/PipeWire are Linux turf; this stub keeps `cargo build --workspace` green
/// elsewhere (the Mac client lives in clients/apple).
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("punktfunk-session is Linux-only — the macOS client lives in clients/apple");
    std::process::exit(2);
}
