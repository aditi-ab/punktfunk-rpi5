//! Process-lifetime host configuration, parsed once from the environment.
//!
//! [`HostConfig`] is the one resolved value capture, topology, and encoding
//! share. Session-mutated compositor variables, path lookups, credentials,
//! and single-use tuning stay live reads at their call sites.
//!
//! Pin knobs with `PUNKTFUNK_*` in operator-owned `host.env`. [`config`]
//! parses on first access. [`env_on`] is the explicit-off grammar; it is not
//! `pf-zerocopy`'s truthy parser.
#![forbid(unsafe_code)]

/// Keyboard LAYOUT from `localectl`, not a `PUNKTFUNK_*` knob. Shared so the
/// injector and the gamescope backend do not depend on each other.
pub mod layout;

use std::sync::OnceLock;

/// Explicit-off for a `PUNKTFUNK_*` var: trimmed, case-insensitive
/// `0`/`false`/`off`/`no` are off; any other present value is on; unset is
/// `None`. Callers must use this — `var(k) != Ok("0")` treats `"0 "` and
/// `"false"` as ON.
///
/// Not `pf-zerocopy`'s grammar (`1|true|yes|on` on, everything else off).
pub fn env_on(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|s| {
        !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Which render endpoint the loopback captures. Legacy `PUNKTFUNK_HOST_AUDIO`
/// and `PUNKTFUNK_KEEP_DEFAULT` still select a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioOutputMode {
    /// Silent render endpoint; streamed audio does not also play on the host.
    #[default]
    ClientOnly,
    /// Host hardware plays as well as the client. `PUNKTFUNK_HOST_AUDIO=1`.
    HostAndClient,
    /// Operator's default playback device; never write default-device policy.
    /// `PUNKTFUNK_KEEP_DEFAULT=1`.
    FollowDefault,
}

impl AudioOutputMode {
    /// `PUNKTFUNK_AUDIO_OUTPUT_MODE` wins; else `KEEP_DEFAULT` before `HOST_AUDIO`
    /// so a stale host-audio flag cannot override "do not touch my devices".
    fn from_env() -> AudioOutputMode {
        if let Ok(raw) = std::env::var("PUNKTFUNK_AUDIO_OUTPUT_MODE")
            && !raw.trim().is_empty()
        {
            if let Some(m) = AudioOutputMode::parse(&raw) {
                return m;
            }
            // Unknown spelling: warn and keep going; do not invent a variant.
            eprintln!(
                "punktfunk: PUNKTFUNK_AUDIO_OUTPUT_MODE={raw:?} is not one of \
                 client_only/host_and_client/follow_default — using client_only"
            );
        }
        if std::env::var_os("PUNKTFUNK_KEEP_DEFAULT").is_some() {
            return AudioOutputMode::FollowDefault;
        }
        if std::env::var_os("PUNKTFUNK_HOST_AUDIO").is_some() {
            return AudioOutputMode::HostAndClient;
        }
        AudioOutputMode::ClientOnly
    }

    pub fn parse(s: &str) -> Option<AudioOutputMode> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "client_only" | "client" => Some(AudioOutputMode::ClientOnly),
            "host_and_client" | "both" | "host" => Some(AudioOutputMode::HostAndClient),
            "follow_default" | "follow" => Some(AudioOutputMode::FollowDefault),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioOutputMode::ClientOnly => "client_only",
            AudioOutputMode::HostAndClient => "host_and_client",
            AudioOutputMode::FollowDefault => "follow_default",
        }
    }

    pub fn prefers_host_hardware(self) -> bool {
        matches!(self, AudioOutputMode::HostAndClient)
    }

    pub fn keeps_default(self) -> bool {
        matches!(self, AudioOutputMode::FollowDefault)
    }
}

/// Operator and dispatch knobs resolved once. Session-mutated values stay at
/// their call sites. Unused-on-this-platform fields stay so `Debug` and the
/// parser remain one platform-neutral function.
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
    /// `PUNKTFUNK_HOST_NAME` — Moonlight `<hostname>` and the mDNS instance name.
    /// Unset/blank = the machine hostname. Display-only; the DNS `<label>.local.`
    /// is a sanitized label so a spacey name cannot produce an invalid record.
    pub host_name: Option<String>,
    /// `PUNKTFUNK_MGMT_BIND` — management listen (`IP:PORT`). `--mgmt-bind` wins.
    /// Unset = `0.0.0.0:47990` (Sunshine's web UI; the only port the two share).
    /// Lives in `host.env` because a package upgrade rewrites the unit file.
    /// Raw string: `main.rs` turns a bad value into the same error as the flag.
    pub mgmt_bind: Option<String>,
    /// `PUNKTFUNK_NATIVE_PORT` — native QUIC control port. `--native-port` wins.
    /// Unset = 9777. Raw string so a typo is a startup error, not a silent 9777.
    pub native_port: Option<String>,
    /// `PUNKTFUNK_GAMESTREAM` — GameStream/Moonlight-compat planes. `--gamestream`
    /// also turns them on. **Default OFF**: they carry plain-HTTP pairing.
    pub gamestream: bool,
    /// `PUNKTFUNK_ENCODER` — encoder-backend override (lowercased). Empty = auto-detect by GPU vendor.
    pub encoder_pref: String,
    /// `PUNKTFUNK_RENDER_ADAPTER` — discrete render-GPU pin by description substring.
    /// `Some` even when empty: empty still counts as set for presence checks.
    pub render_adapter: Option<String>,
    /// `PUNKTFUNK_IDD_DEPTH` — IDD-push pipeline depth. Default 2; the call site clamps to its `OUT_RING`.
    pub idd_depth: usize,
    /// `PUNKTFUNK_ZEROCOPY` — Windows D3D11 zero-copy encode input. `None` defers to
    /// the per-vendor default (AMF on, QSV off — `encode/ffmpeg_win.rs`).
    pub zerocopy: Option<bool>,
    /// `PUNKTFUNK_10BIT` — host policy gate for HEVC Main10 / AV1. **Default ON**,
    /// explicit-off. The host only *allows* 10-bit; the session still needs
    /// `VIDEO_CAP_10BIT` and `can_encode_10bit`. Independent of `four_four_four`.
    pub ten_bit: bool,
    /// `PUNKTFUNK_444` — host policy gate for HEVC 4:4:4. **Default ON**,
    /// explicit-off. The host only *allows* 4:4:4; the session still needs the
    /// client to advertise it, HEVC, full-chroma capture, and the encode probe.
    /// Independent of `ten_bit`.
    pub four_four_four: bool,
    /// `PUNKTFUNK_CHACHA20` — host policy gate for ChaCha20-Poly1305
    /// (`design/chacha20-session-cipher.md`). **Default ON**, explicit-off.
    /// The host only *allows* it; a session uses ChaCha only when the client
    /// advertised `VIDEO_CAP_CHACHA20`. Everyone else stays AES-128-GCM.
    pub chacha20: bool,
    /// `PUNKTFUNK_AUDIO_OUTPUT_MODE` — see [`AudioOutputMode`].
    pub audio_output_mode: AudioOutputMode,
    /// `PUNKTFUNK_AUDIO_QUALITY` — encode tier (`low`/`standard`/`high`; default
    /// `high`). Raw string: the table lives in `punktfunk-core`. The audio thread
    /// warns on an unknown spelling rather than silently downgrading.
    pub audio_quality: Option<String>,
    /// `PUNKTFUNK_AUDIO_REDUNDANCY` — force the redundant `0xD2` audio plane.
    /// `None` = automatic: only to a client that asked, and only while losing packets.
    pub audio_redundancy: Option<bool>,
    /// `PUNKTFUNK_AUDIO_HIRES` — host policy gate for lossless `0xD3`
    /// (`design/hi-res-audio.md`). **Default ON**, explicit-off. The host only
    /// *allows* the plane; the client's format pick is the session switch.
    /// [`env_on`] treats a client-shaped `96000/24` as allow. `0` forces Opus.
    pub audio_hires: bool,
    /// `PUNKTFUNK_PERF` — per-stage timing instrumentation.
    pub perf: bool,
    /// `PUNKTFUNK_VIDEO_SOURCE` — `virtual` (default: per-client virtual output) /
    /// `portal` (an existing monitor); anything else, including `synthetic`, is
    /// the test pattern.
    pub video_source: Option<String>,
    /// `PUNKTFUNK_CAPTURE_MONITOR` — pin capture at a named physical monitor
    /// (`DP-1`). Config, not a prompt: a `--user` service has nobody to answer a
    /// chooser. A name that matches no head is a hard error. Linux-only;
    /// `design/per-monitor-portal-capture.md`.
    pub capture_monitor: Option<String>,
    /// `PUNKTFUNK_PORTAL_CURSOR_MODE` — `auto` (default) · `hidden` · `embedded` ·
    /// `metadata`. Preference, not a command: `portal_cursor::pick` closes the
    /// session if the backend does not advertise it. `embedded` is the safe pin.
    pub portal_cursor_mode: Option<String>,
    /// `PUNKTFUNK_COMPOSITOR` — explicit compositor override (operator/CI/test).
    /// Not the runtime-detected session; `apply_session_env` never writes this.
    pub compositor: Option<String>,
    /// `PUNKTFUNK_GAMEPAD` — virtual-pad backend preference, fed to `pick_gamepad`.
    pub gamepad: Option<String>,
    /// `PUNKTFUNK_VDISPLAY` — Windows virtual-display backend. IddCx is the only
    /// backend; kept for shipped `host.env`.
    pub vdisplay: Option<String>,
    /// `PUNKTFUNK_STALL_PROBES` — run the Windows IDD-push capture's micro-probe engine (per-GPU
    /// fence probes, DWM tick/flush watchdogs, scanline + CPU sentinels — `idd_push/probes.rs`),
    /// the corroborating evidence legs on every stall report. Default OFF (immunity plan WP3:
    /// standing fence/scanline/DWM traffic alters the hottest path while diagnosing it — an
    /// observer effect the disturbance reports must not carry by default); `=1` opts a box under
    /// diagnosis in. Off, stall lines still carry the driver telemetry + the ETW present/queue
    /// discriminator (cheap, session-filtered); only the probe legs read absent.
    pub stall_probes: bool,
    /// `PUNKTFUNK_GAMESCOPE_STEAM` — force `--steam` on every bare headless gamescope
    /// launch. Steam titles already pass it; this is for non-Steam. Managed
    /// gamescope-session-plus/SteamOS sessions ignore it.
    pub gamescope_steam: bool,
    /// `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` — `--force-grab-cursor` on a real game
    /// launch. Default OFF: relative mode breaks absolute-pointer titles and menus.
    pub gamescope_grab_cursor: bool,
    /// `PUNKTFUNK_GAMESCOPE_SPLASH` — splash on every bare headless gamescope spawn.
    /// gamescope only composites (and pushes PipeWire) when a client paints.
    /// **Default ON**; `=0` is the escape hatch.
    pub gamescope_splash: bool,
    /// `PUNKTFUNK_GAMESCOPE_ISOLATE` — per-session EIS/audio/mic planes
    /// (`design/gamescope-multiuser.md`). **Default ON**; `=0` restores shared
    /// host-lifetime planes. Shared-desktop and managed/attach routes are untouched.
    pub gamescope_isolate: bool,
    /// `PUNKTFUNK_GAMESCOPE_HDR` — allow HDR on gamescope. The host probes the
    /// punktfunk build (`packaging/gamescope`) and stays SDR if missing; this only
    /// decides whether HDR is *attempted*. **Default ON**, matching `PUNKTFUNK_10BIT`.
    pub gamescope_hdr: bool,
    /// `PUNKTFUNK_GAMESCOPE_SDR_NITS` — SDR luminance inside the PQ container
    /// (`--hdr-sdr-content-nits`). `None` = 203 nits (BT.2408), what our clients
    /// decode against — not gamescope's 400. See `SDR_REFERENCE_WHITE_NITS`.
    pub gamescope_sdr_nits: Option<u32>,
    /// `PUNKTFUNK_GAMESCOPE_BIND` — bind patched gamescope over `/usr/bin/gamescope`
    /// in the session unit's mount namespace. `gamescope-session-plus` hardcodes
    /// that path (`pf-vdisplay`'s `gamescope.rs`).
    ///
    /// Three-valued. A user-unit mount namespace maps only this uid, so
    /// root-owned `/tmp/.X11-unix` reads as `nobody` and Xwayland refuses to start.
    /// `None` = AUTO (arm only when the script cannot reach gamescope another way).
    /// `Some(false)` = never. `Some(true)` = force; a failed redirect still disarms.
    pub gamescope_bind: Option<bool>,
    /// `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` — extra Hz (comma-separated) a gamescope
    /// session offers on top of the rate it runs at. Can only ADD; the session rate
    /// is always included. Empty = negotiated rate only. Ignored on stock gamescope.
    pub gamescope_refresh_rates: Vec<u32>,
    /// `PUNKTFUNK_RECOVER_SESSION_CMD` — operator hook (debounced) when a client
    /// connects with no graphical session for this uid. Unset/empty = disabled.
    pub recover_session_cmd: Option<String>,
    /// `PUNKTFUNK_ON_CONNECT_CMD` — `client.connected` hook: detached, event JSON
    /// on stdin + `PF_EVENT_*`. Filters live in `hooks.json`. Unset/empty = disabled.
    pub on_connect_cmd: Option<String>,
    /// `PUNKTFUNK_ON_DISCONNECT_CMD` — `client.disconnected` sibling of
    /// [`Self::on_connect_cmd`].
    pub on_disconnect_cmd: Option<String>,
    /// `PUNKTFUNK_MAX_FPS` — game-side frame limiter. `None` (unset, `0`, unparseable)
    /// = no limit. Caps compositor render rate, not the session: a 120 Hz session
    /// over a 60 fps cap still sends 120 frames (60 repeats). gamescope:
    /// `--nested-refresh`, clamped to 1..=240.
    pub max_fps: Option<u32>,
    /// `PUNKTFUNK_VDISPLAY_HZ_MULT` — virtual-display refresh as a multiple of the
    /// session rate; the stream stays at the session rate. Default 1; 2 halves
    /// worst-case age (~16 ms at 60 Hz) without extra wire frames. Clamped 1..=4.
    pub vdisplay_hz_mult: u32,
    /// `PUNKTFUNK_GAMESCOPE_VRR=0` — opt out of adaptive sync. Default on: capable
    /// gamescope gets `--adaptive-sync` + `--framerate-limit` at the game rate so
    /// it paints on the game's commit. Inert on stock gamescope (`adaptive_sync_args`).
    pub gamescope_vrr: bool,
}

impl HostConfig {
    fn from_env() -> Self {
        // Presence, not value.
        let flag = |k: &str| std::env::var_os(k).is_some();
        // `Some` (possibly empty) when set with valid UTF-8.
        let val = |k: &str| std::env::var(k).ok();
        Self {
            host_name: val("PUNKTFUNK_HOST_NAME")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Blank-is-unset: `PUNKTFUNK_MGMT_BIND=` means default.
            mgmt_bind: val("PUNKTFUNK_MGMT_BIND")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            native_port: val("PUNKTFUNK_NATIVE_PORT")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            gamestream: env_on("PUNKTFUNK_GAMESTREAM").unwrap_or(false),
            encoder_pref: std::env::var("PUNKTFUNK_ENCODER")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            render_adapter: val("PUNKTFUNK_RENDER_ADAPTER"),
            idd_depth: val("PUNKTFUNK_IDD_DEPTH")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2),
            zerocopy: env_on("PUNKTFUNK_ZEROCOPY"),
            ten_bit: env_on("PUNKTFUNK_10BIT").unwrap_or(true),
            four_four_four: env_on("PUNKTFUNK_444").unwrap_or(true),
            chacha20: env_on("PUNKTFUNK_CHACHA20").unwrap_or(true),
            audio_output_mode: AudioOutputMode::from_env(),
            audio_quality: val("PUNKTFUNK_AUDIO_QUALITY").map(|s| s.trim().to_lowercase()),
            audio_redundancy: env_on("PUNKTFUNK_AUDIO_REDUNDANCY"),
            audio_hires: env_on("PUNKTFUNK_AUDIO_HIRES").unwrap_or(true),
            perf: flag("PUNKTFUNK_PERF"),
            // Default OFF (immunity plan WP3 — no standing observer effect); opt-in per box.
            stall_probes: env_on("PUNKTFUNK_STALL_PROBES").unwrap_or(false),
            // Defaults to `virtual` — the flagship per-client virtual output. It used to be unset,
            // which fell through to the synthetic test pattern: fine for a dev box that always has
            // a host.env, wrong for a packaged install, whose unit no longer requires that file at
            // all. `synthetic` is still reachable by naming it (any unrecognised value lands there).
            video_source: val("PUNKTFUNK_VIDEO_SOURCE").or_else(|| Some("virtual".to_string())),
            // Emptied-to-None: `PUNKTFUNK_CAPTURE_MONITOR=` is unset, not a blank name.
            capture_monitor: val("PUNKTFUNK_CAPTURE_MONITOR")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Emptied-to-None. Spellings are parsed at `portal_cursor::want`.
            portal_cursor_mode: val("PUNKTFUNK_PORTAL_CURSOR_MODE")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            compositor: val("PUNKTFUNK_COMPOSITOR"),
            gamepad: val("PUNKTFUNK_GAMEPAD"),
            vdisplay: val("PUNKTFUNK_VDISPLAY"),
            gamescope_steam: val("PUNKTFUNK_GAMESCOPE_STEAM").is_some_and(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            gamescope_grab_cursor: val("PUNKTFUNK_GAMESCOPE_GRAB_CURSOR").is_some_and(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }),
            gamescope_splash: env_on("PUNKTFUNK_GAMESCOPE_SPLASH").unwrap_or(true),
            gamescope_isolate: env_on("PUNKTFUNK_GAMESCOPE_ISOLATE").unwrap_or(true),
            gamescope_hdr: env_on("PUNKTFUNK_GAMESCOPE_HDR").unwrap_or(true),
            gamescope_sdr_nits: val("PUNKTFUNK_GAMESCOPE_SDR_NITS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|n| (1..=10_000).contains(n)),
            // Unset is AUTO; `=0` is stock gamescope; `=1` is force.
            gamescope_bind: env_on("PUNKTFUNK_GAMESCOPE_BIND"),
            // Junk entries are dropped; this only widens a menu.
            gamescope_refresh_rates: parse_refresh_rates(
                val("PUNKTFUNK_GAMESCOPE_REFRESH_RATES").as_deref(),
            ),
            recover_session_cmd: val("PUNKTFUNK_RECOVER_SESSION_CMD")
                .filter(|s| !s.trim().is_empty()),
            on_connect_cmd: val("PUNKTFUNK_ON_CONNECT_CMD").filter(|s| !s.trim().is_empty()),
            on_disconnect_cmd: val("PUNKTFUNK_ON_DISCONNECT_CMD").filter(|s| !s.trim().is_empty()),
            // 0 means no limit, not "stream nothing".
            max_fps: val("PUNKTFUNK_MAX_FPS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&f| f > 0)
                .map(|f| f.clamp(1, 240)),
            vdisplay_hz_mult: val("PUNKTFUNK_VDISPLAY_HZ_MULT")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1)
                .clamp(1, 4),
            gamescope_vrr: val("PUNKTFUNK_GAMESCOPE_VRR").as_deref().map(str::trim) != Some("0"),
        }
    }
}

/// `"60, 90,120"` → `[60, 90, 120]`, sorted and deduped. Junk and out-of-range
/// rates are skipped rather than rejecting the list.
fn parse_refresh_rates(raw: Option<&str>) -> Vec<u32> {
    let mut out: Vec<u32> = raw
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .filter(|&hz| (1..=1000).contains(&hz))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

impl HostConfig {
    /// Compositor refresh for the GAME: the session rate, capped by [`Self::max_fps`].
    /// Session mode, encoder, and wire never go through here.
    ///
    /// `0` in means `0` out. A zero rate is rejected upstream.
    pub fn game_fps(&self, session_hz: u32) -> u32 {
        match self.max_fps {
            Some(cap) if session_hz > cap => cap,
            _ => session_hz,
        }
    }
}

/// Process-wide host configuration, parsed once on first access.
pub fn config() -> &'static HostConfig {
    static CFG: OnceLock<HostConfig> = OnceLock::new();
    CFG.get_or_init(HostConfig::from_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_fps: Option<u32>) -> HostConfig {
        HostConfig {
            max_fps,
            ..Default::default()
        }
    }

    #[test]
    fn game_fps_caps_only_above_the_limit() {
        for hz in [24, 30, 60, 120, 144, 240] {
            assert_eq!(cfg(None).game_fps(hz), hz);
        }
        // Ceiling, not a target: a session below the limit keeps its own rate.
        let c = cfg(Some(60));
        assert_eq!(c.game_fps(120), 60);
        assert_eq!(c.game_fps(60), 60);
        assert_eq!(c.game_fps(30), 30);
        // An invalid rate stays invalid rather than being laundered into a real one.
        assert_eq!(c.game_fps(0), 0);
    }

    #[test]
    fn refresh_rate_list_parses_and_tolerates_junk() {
        assert_eq!(parse_refresh_rates(Some("60,90,120")), vec![60, 90, 120]);
        assert_eq!(
            parse_refresh_rates(Some(" 120, 60 ,90, 60")),
            vec![60, 90, 120]
        );
        assert!(parse_refresh_rates(None).is_empty());
        assert!(parse_refresh_rates(Some("")).is_empty());
        assert!(parse_refresh_rates(Some("   ")).is_empty());
        // A typo costs its own entry, never the whole list.
        assert_eq!(parse_refresh_rates(Some("60,abc,120")), vec![60, 120]);
        // 0 is not a refresh rate; 1920 is a width.
        assert_eq!(parse_refresh_rates(Some("0,60,1920")), vec![60]);
    }

    #[test]
    fn audio_output_mode_parses_its_spellings() {
        for (s, want) in [
            ("client_only", AudioOutputMode::ClientOnly),
            ("client-only", AudioOutputMode::ClientOnly),
            ("  CLIENT  ", AudioOutputMode::ClientOnly),
            ("host_and_client", AudioOutputMode::HostAndClient),
            ("both", AudioOutputMode::HostAndClient),
            ("follow_default", AudioOutputMode::FollowDefault),
            ("follow", AudioOutputMode::FollowDefault),
        ] {
            assert_eq!(AudioOutputMode::parse(s), Some(want), "{s:?}");
        }
        // Unknown spellings are rejected, not silently re-routed.
        for s in ["", "silent", "off", "true"] {
            assert_eq!(AudioOutputMode::parse(s), None, "{s:?}");
        }
        for m in [
            AudioOutputMode::ClientOnly,
            AudioOutputMode::HostAndClient,
            AudioOutputMode::FollowDefault,
        ] {
            assert_eq!(AudioOutputMode::parse(m.as_str()), Some(m));
        }
    }

    /// `prefers_host_hardware` and `keeps_default` must stay mutually exclusive:
    /// conflating them would either silence the host or stomp the operator's devices.
    #[test]
    fn audio_output_mode_predicates_are_disjoint() {
        assert_eq!(AudioOutputMode::default(), AudioOutputMode::ClientOnly);
        for m in [
            AudioOutputMode::ClientOnly,
            AudioOutputMode::HostAndClient,
            AudioOutputMode::FollowDefault,
        ] {
            assert!(!(m.prefers_host_hardware() && m.keeps_default()), "{m:?}");
        }
        assert!(AudioOutputMode::HostAndClient.prefers_host_hardware());
        assert!(AudioOutputMode::FollowDefault.keeps_default());
        assert!(!AudioOutputMode::ClientOnly.prefers_host_hardware());
        assert!(!AudioOutputMode::ClientOnly.keeps_default());
    }
}
