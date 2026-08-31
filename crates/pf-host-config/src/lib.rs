//! Process-lifetime host configuration parsed once from environment variables.
//!
//! `HostConfig` owns stable operator and backend-selection knobs so capture, topology, and
//! encoding share one resolved value. Session-mutated compositor variables, path lookups,
//! credentials, and single-use tuning remain live reads at their call sites.
//!
//! `PUNKTFUNK_ZEROCOPY` is a tri-state override: unset defers to the platform/vendor default;
//! `0|false|off|no` disables it; any other present value enables it. This explicit-off grammar
//! is distinct from `pf-zerocopy`'s truthy parser.
#![forbid(unsafe_code)]

/// Which keyboard LAYOUT the box is configured for. Not a `PUNKTFUNK_*` knob — it is read from
/// what `localectl` recorded — but it is host configuration every input path needs (the injector
/// compiles its keymap from it; the gamescope backend hands it to the session it launches), and it
/// lives here so both can reach it without either crate depending on the other.
pub mod layout;

use std::sync::OnceLock;

/// Whether a `PUNKTFUNK_*` env var reads as ON, or `None` when it is unset — the host's
/// **explicit-off** grammar: `0` / `false` / `off` / `no` (trimmed, case-insensitive) are off and ANY
/// other value is on, so a presence-style `=1` keeps working. Every "default ON" knob below shares
/// it.
///
/// Exported because callers in other crates need the SAME grammar. A hand-rolled
/// `var(k).as_deref() != Ok("0")` accepts `"0 "` (trailing space, trivially produced by a systemd
/// drop-in or a shell heredoc) and `"false"` as ON — the bug class of ed525c4c, and the reason
/// `PUNKTFUNK_PIPEWIRE_NV12` in pf-capture now routes through here.
///
/// Note this is deliberately NOT the grammar `pf-zerocopy` uses for its own flags (truthy:
/// `1|true|yes|on`, everything else off) — see the module docs: independent features that share a
/// name prefix.
pub fn env_on(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|s| {
        !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Where desktop audio should be audible — which decides the render endpoint the loopback captures.
///
/// Supersedes the two env-only knobs that used to encode this (`PUNKTFUNK_HOST_AUDIO`,
/// `PUNKTFUNK_KEEP_DEFAULT`), which stay honoured as back-compat spellings so nobody's `host.env`
/// breaks. Named modes exist because "which endpoint do we capture" is a routing decision an
/// operator has to be able to make deliberately — the 2026-08-03 field report is what happens when
/// the only way to express it is an undocumented environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioOutputMode {
    /// Default. Prefer a render endpoint that is silent on the host, so streamed audio does not
    /// also play out of the host's speakers. Since 2026-08 a silent sink has to be able to carry
    /// the mix without narrowing it — otherwise real hardware wins anyway.
    #[default]
    ClientOnly,
    /// Prefer real hardware: audio plays on the host as well as the client. The old
    /// `PUNKTFUNK_HOST_AUDIO=1`.
    HostAndClient,
    /// Touch nothing — capture whatever the operator's own default playback device is, and never
    /// write the default-device policy. The old `PUNKTFUNK_KEEP_DEFAULT=1`.
    FollowDefault,
}

impl AudioOutputMode {
    /// `PUNKTFUNK_AUDIO_OUTPUT_MODE` wins; otherwise fall back to the legacy flags, `follow_default`
    /// first (it is the more restrictive promise — "do not touch my devices" must not be overridden
    /// by a stale `PUNKTFUNK_HOST_AUDIO` in the same `host.env`).
    fn from_env() -> AudioOutputMode {
        if let Ok(raw) = std::env::var("PUNKTFUNK_AUDIO_OUTPUT_MODE")
            && !raw.trim().is_empty()
        {
            if let Some(m) = AudioOutputMode::parse(&raw) {
                return m;
            }
            // Never silently fall through to a different routing than the operator asked for.
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

    /// The loopback plan should prefer real hardware over a silent sink.
    pub fn prefers_host_hardware(self) -> bool {
        matches!(self, AudioOutputMode::HostAndClient)
    }

    /// Leave the operator's default playback/recording devices completely alone.
    pub fn keeps_default(self) -> bool {
        matches!(self, AudioOutputMode::FollowDefault)
    }
}

/// Resolved host configuration. Holds the genuinely-constant operator/dispatch knobs (see module docs for
/// what is deliberately excluded). Fields read on only one platform are kept alive cross-platform by the
/// derived `Debug` impl, so the parser can stay a single platform-neutral function.
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
    /// `PUNKTFUNK_HOST_NAME` — the name this host shows up under in Moonlight (the serverinfo
    /// `<hostname>` element) and in Punktfunk's own clients (the mDNS service *instance* name both
    /// adverts carry). Unset/blank = the machine's own hostname, which is what it always was. Free
    /// text ("Living Room PC"); the DNS-level `<label>.local.` target keeps using a sanitized
    /// machine-safe label, so a spacey display name can't produce an invalid mDNS record.
    pub host_name: Option<String>,
    /// `PUNKTFUNK_MGMT_BIND` — the management API's listen address (`IP:PORT`), equivalent to the
    /// `--mgmt-bind` CLI flag, which still wins when both are given. Unset = `0.0.0.0:47990`.
    ///
    /// This exists so moving the port SURVIVES: `--mgmt-bind` lives in a unit file / service
    /// registration that a package upgrade rewrites, whereas `host.env` is operator-owned and is
    /// the documented place every other knob lives. The motivating case is coexistence with a
    /// Sunshine fork — 47990 is *their* web UI port as well as our management API, and it is the
    /// only port the two share once the GameStream planes are off, so moving it is the whole fix.
    ///
    /// Kept as the raw string rather than a parsed `SocketAddr`: this crate is the
    /// parse-once-from-env layer, and `main.rs` owns turning a bad value into the same
    /// `bad --mgmt-bind (want IP:PORT)` error the flag produces, from one place.
    pub mgmt_bind: Option<String>,
    /// `PUNKTFUNK_NATIVE_PORT` — the native punktfunk/1 (QUIC) control port, equivalent to the
    /// `--native-port` CLI flag, which still wins. Unset = 9777.
    ///
    /// Same survives-an-upgrade argument as [`Self::mgmt_bind`]: `--native-port` lives in an
    /// ExecStart a package rewrites. Unlike the mgmt port, the CLIENT side of moving this already
    /// worked — `KnownHost.port` is persisted per host and `--connect HOST:PORT` names it — so this
    /// key is the last piece of making the native port genuinely movable.
    ///
    /// Raw string, parsed in `main.rs`, for the same reason as `mgmt_bind`: a typo'd port must be a
    /// startup ERROR, not a silent fall back to 9777 while the operator believes they moved it.
    pub native_port: Option<String>,
    /// `PUNKTFUNK_GAMESTREAM` — enable the GameStream/Moonlight-compat planes (nvhttp pairing,
    /// RTSP, ENet control, `_nvstream` mDNS) from `host.env`, equivalent to the `--gamestream`
    /// CLI flag (either source turns it on). **Default OFF** — the secure native-only host: the
    /// compat planes carry plain-HTTP pairing + the legacy GCM-nonce path (security-review
    /// #5/#9), so stock-Moonlight support is opt-in on every route, and the packaged units ship
    /// without the flag so this knob is how a package user opts in.
    pub gamestream: bool,
    /// `PUNKTFUNK_ENCODER` — explicit encoder-backend override (lowercased; empty = auto-detect by GPU vendor).
    pub encoder_pref: String,
    /// `PUNKTFUNK_RENDER_ADAPTER` — discrete render-GPU pin by description substring (`Some` even when empty:
    /// the empty string still counts as "set" for the presence checks, and the value reader filters it).
    pub render_adapter: Option<String>,
    /// `PUNKTFUNK_IDD_DEPTH` — IDD-push pipeline depth override (default 2; the call site clamps to its `OUT_RING`).
    pub idd_depth: usize,
    /// `PUNKTFUNK_ZEROCOPY` — Windows D3D11 zero-copy encode input override. `None` (unset) defers to
    /// the per-vendor default (AMF on, QSV off — see module docs and `encode/ffmpeg_win.rs`).
    pub zerocopy: Option<bool>,
    /// `PUNKTFUNK_10BIT` — host policy gate for 10-bit encode (HEVC Main10 / AV1 10-bit).
    /// **Default ON** (since 10-bit went probe-gated end-to-end, 2026-07-16): the host merely
    /// *allows* 10-bit — a session only becomes 10-bit when the client advertised `VIDEO_CAP_10BIT`
    /// (behind its HDR setting + display-capability gate), the codec supports it (HEVC/AV1), and
    /// the GPU/backend passed the encode probe (`can_encode_10bit`) — otherwise 8-bit SDR.
    /// `PUNKTFUNK_10BIT=0`/`false`/`off`/`no` disables. Independent of `four_four_four` (depth vs chroma).
    pub ten_bit: bool,
    /// `PUNKTFUNK_444` — host policy gate for full-chroma HEVC 4:4:4 (Range Extensions).
    /// **Default ON** (since the pipeline went zero-copy + honest end-to-end, 2026-07-10): the
    /// host merely *allows* 4:4:4 — a session only becomes 4:4:4 when the client explicitly
    /// advertised it (a client-side setting, default OFF), the codec is HEVC, the capture can
    /// deliver full chroma, and the GPU/driver passed the encode probe — otherwise 4:2:0.
    /// `PUNKTFUNK_444=0`/`false`/`off`/`no` disables. Independent of `ten_bit` (chroma vs depth).
    pub four_four_four: bool,
    /// `PUNKTFUNK_CHACHA20` — host policy gate for the negotiated ChaCha20-Poly1305 session
    /// cipher (design/chacha20-session-cipher.md). **Default ON** (pure rollout safety — perf-only,
    /// both AEADs are full-strength): the host merely *allows* it — a session only seals with
    /// ChaCha when the client advertised `VIDEO_CAP_CHACHA20` (set by soft-AES armv7 clients,
    /// e.g. webOS TVs, whose GCM decrypt caps at ~100 Mbps); everyone else stays AES-128-GCM.
    /// `PUNKTFUNK_CHACHA20=0`/`false`/`off`/`no` disables.
    pub chacha20: bool,
    /// `PUNKTFUNK_AUDIO_OUTPUT_MODE` — where desktop audio should be audible, and therefore which
    /// render endpoint the loopback captures (`client_only` / `host_and_client` / `follow_default`).
    ///
    /// A first-class setting because the 2026-08-03 field report needed one: the default
    /// client-only routing sent that box's whole desktop mix through Steam's voice-carrier virtual
    /// endpoint for 25 sessions, and the only way to change it was an undocumented environment
    /// variable. See [`AudioOutputMode`].
    pub audio_output_mode: AudioOutputMode,
    /// `PUNKTFUNK_AUDIO_QUALITY` — desktop-audio encode tier (`low` / `standard` / `high`; default
    /// `high`). Kept as the raw string here because the tier table lives in `punktfunk-core`, and
    /// this crate is deliberately dependency-free (see the crate doc). The audio thread resolves it
    /// via `punktfunk_core::audio::AudioTier::parse` and warns on an unknown spelling rather than
    /// silently downgrading someone's audio.
    pub audio_quality: Option<String>,
    /// `PUNKTFUNK_AUDIO_REDUNDANCY` — force the redundant `0xD2` audio plane on or off. `None`
    /// (the default) = automatic: sent only to a client that asked for it, and only while the link
    /// is actually losing packets.
    pub audio_redundancy: Option<bool>,
    /// `PUNKTFUNK_AUDIO_HIRES` — host policy gate for the lossless `0xD3` audio plane
    /// (44.1/48/88.2/96/176.4 kHz, 16/24-bit PCM, stereo through 7.1;
    /// `design/hi-res-audio.md` §10). The rate set lives in
    /// [`punktfunk_core::audio::pcm::rate_is_supported`] and the channel count is decided by
    /// whether a frame fits a datagram, not by a list — so neither is restated here.
    ///
    /// **Default ON** (2026-08-17), explicit-off grammar — the same shape as `four_four_four`,
    /// `chacha20` and `ten_bit` above, and for the same reason: the host merely *allows* the plane,
    /// and the switch that decides any actual session is the CLIENT's, which is still default OFF.
    /// `PUNKTFUNK_AUDIO_HIRES=0`/`false`/`off`/`no` disables.
    ///
    /// It used to be default OFF, on the argument that the plane costs **1.4–8.5 Mbps in stereo,
    /// up to 33.9 in 7.1**, rides QUIC datagrams OUTSIDE the ABR loop (§4.6) and is therefore
    /// bandwidth nobody consented to. Every clause of that is still true — what was wrong was
    /// asking the OPERATOR to pre-consent to it, because the operator is not who spends it and, on
    /// a host with no settings UI, is a person who has to be told an environment variable exists
    /// before a user's own explicit menu choice can work at all. The field report that moved this:
    /// a user picked "Lossless 96 kHz / 24-bit" in the macOS client, got Opus, and nothing in any
    /// UI said why — the reason was one `INFO` line in the host's journal.
    ///
    /// What actually protects the link is the rest of the §8.4 gate, and it is mechanical rather
    /// than consent-based: the client must have asked, the capture path must HONESTLY deliver the
    /// rate (not merely open at it), the cost must fit `HIRES_MAX_VIDEO_SHARE_PCT` (25 %) of the
    /// session's video bitrate, and a frame must fit a datagram. A 5 Mbps session still cannot buy
    /// 96/24 no matter what this says. So the operator gate was never the thing keeping a modest
    /// link safe — it was only keeping the feature unreachable.
    ///
    /// ⚠ **The desktop CLIENTS read a variable of this same name with a RICHER grammar** — see
    /// `pf_client_core::session`, which takes `1`/`on`, a bare rate such as `96000`, or an explicit
    /// `<rate>/<bits>`. A box that is both host and client therefore configures both halves from
    /// one environment line, and they still compose after the flip: [`env_on`] reads everything
    /// that is not `0`/`false`/`off`/`no` as *on*, so a client-shaped `96000/24` says *allow* here
    /// too — and the one spelling that has to mean the same thing at both ends is now `0`, which
    /// does: it forces Opus on the host and forces Opus at the client. The interesting direction
    /// reversed with the default. It used to be "did anyone remember to turn this on"; it is now
    /// "did anyone turn it off".
    pub audio_hires: bool,
    /// `PUNKTFUNK_PERF` — per-stage timing instrumentation.
    pub perf: bool,
    /// `PUNKTFUNK_VIDEO_SOURCE` — GameStream video source select. `virtual` (the default — a
    /// per-client virtual output at the client's own mode) / `portal` (capture an existing
    /// monitor); anything else, including the literal `synthetic`, gets the test pattern.
    pub video_source: Option<String>,
    /// `PUNKTFUNK_CAPTURE_MONITOR` — pin capture at a NAMED physical monitor (`DP-1`, `HDMI-A-2`),
    /// instead of creating a virtual display or taking whichever head the portal hands back. The
    /// point of the knob is an unattended host: a background `systemd --user` service has nobody to
    /// answer a chooser dialog, so the monitor has to be config, not a prompt. A name that matches
    /// no head is a hard error at session open (never a silent fall-back to a different screen —
    /// showing the wrong monitor is worse than showing none). Linux-only today; see
    /// `design/per-monitor-portal-capture.md`.
    pub capture_monitor: Option<String>,
    /// `PUNKTFUNK_PORTAL_CURSOR_MODE` — `auto` (default) · `hidden` · `embedded` · `metadata`.
    /// Pin the ScreenCast cursor mode the Linux portal backends PREFER, instead of the one the
    /// session negotiates (`metadata` when the client draws the pointer itself, `embedded`
    /// otherwise). The pin is a preference, not a command: it still runs through
    /// `portal_cursor::pick`, so it can never ask a backend for a mode the backend does not
    /// advertise — that closes the session rather than degrading, which is the failure this knob
    /// sits next to. Exists for the backend that advertises a mode it implements badly, where
    /// negotiation has nothing to go on; `embedded` is the safe answer there.
    pub portal_cursor_mode: Option<String>,
    /// `PUNKTFUNK_COMPOSITOR` — explicit compositor override (operator/CI/test). NOT the runtime-detected
    /// session — this one is a constant operator knob; `apply_session_env` never writes it.
    pub compositor: Option<String>,
    /// `PUNKTFUNK_GAMEPAD` — client/operator virtual-pad backend preference (fed to `pick_gamepad`).
    pub gamepad: Option<String>,
    /// `PUNKTFUNK_VDISPLAY` — Windows virtual-display backend. The pf-vdisplay IddCx driver is now the only
    /// backend (the legacy SudoVDA backend was removed), so this is currently informational — kept for the
    /// shipped `host.env` and as a forward seam if a second backend is ever added.
    pub vdisplay: Option<String>,
    /// `PUNKTFUNK_STALL_PROBES` — run the Windows IDD-push capture's micro-probe engine (per-GPU
    /// fence probes, DWM tick/flush watchdogs, scanline + CPU sentinels — `idd_push/probes.rs`),
    /// the corroborating evidence legs on every stall report. Default ON while the
    /// interval-stutter field program runs; explicit-off grammar for perf-sensitive boxes — the
    /// engine costs standing threads (a blocking `DwmFlush` waiter, ~10 Hz fence copies per GPU,
    /// a 5 ms-cadence CPU sentinel). Off, stall lines still carry the driver telemetry + the ETW
    /// present/queue discriminator (cheap, session-filtered); only the probe legs read absent.
    pub stall_probes: bool,
    /// `PUNKTFUNK_GAMESCOPE_STEAM` — force the bare headless gamescope spawn into its Steam
    /// integration mode (`--steam`) for EVERY launch. A Steam title auto-enables `--steam` on its
    /// own regardless of this knob; it exists to force it on for non-Steam launches too. Managed
    /// gamescope-session-plus/SteamOS sessions own their own flags and do not consult this.
    pub gamescope_steam: bool,
    /// `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` — add `--force-grab-cursor` to the bare headless gamescope
    /// spawn for an actual game launch, forcing relative-mouse capture so FPS mouselook works over the
    /// injected pointer. Default OFF: it forces relative mode, which breaks absolute-pointer titles
    /// and menus, so it's opt-in per host until validated on-glass.
    pub gamescope_grab_cursor: bool,
    /// `PUNKTFUNK_GAMESCOPE_SPLASH` — run the host's built-in splash client inside every bare
    /// headless gamescope spawn. gamescope only composites (and only then pushes a PipeWire capture
    /// buffer) when a client paints, and a dedicated Steam launch paints NOTHING
    /// for the whole Steam bootstrap — so without the splash a fresh spawn's capture starves: format
    /// negotiated, zero buffers, first-frame timeout, and every retry kills the booting Steam and
    /// starts over (the "fresh gamescope output never delivers frames" field failure). Default ON;
    /// explicit-off grammar (`=0` disables, the on-glass A/B + emergency escape hatch).
    pub gamescope_splash: bool,
    /// `PUNKTFUNK_GAMESCOPE_HDR` — allow HDR (10-bit BT.2020 PQ) sessions on the gamescope
    /// backend. Needs the punktfunk gamescope build (`packaging/gamescope`), which teaches
    /// gamescope's PipeWire node the 10-bit PQ capture formats; the host probes for it and stays
    /// SDR when it isn't installed, so this knob only decides whether HDR is *attempted*.
    ///
    /// Default ON (explicit-off grammar, matching `PUNKTFUNK_10BIT`) since the post-0.22.3 flip:
    /// the capability chain behind it (the `+pfhdr` banner probe, managed spawn, the client's
    /// 10-bit cap, the per-source downgrade latch) keeps a stock-gamescope box on today's 8-bit
    /// path, so the knob's remaining job is the emergency escape hatch — an operator who hits a
    /// bad interaction sets `=0` and the gamescope backend is exactly the old SDR path again,
    /// spawn flags included.
    pub gamescope_hdr: bool,
    /// `PUNKTFUNK_GAMESCOPE_SDR_NITS` — the luminance SDR content is mapped to inside the PQ
    /// container of an HDR gamescope session (gamescope's `--hdr-sdr-content-nits`).
    /// An HDR stream carries the desktop, the Steam overlay and any SDR game through the same PQ
    /// encode, so this is the knob that decides how bright "white" looks on the client's panel.
    /// `None` = 203 nits, BT.2408 reference white, which is what our clients decode against —
    /// NOT gamescope's own default of 400, which sits nearly a stop above it. See `pf-vdisplay`'s
    /// `SDR_REFERENCE_WHITE_NITS` for why the host pins this rather than letting it float.
    pub gamescope_sdr_nits: Option<u32>,
    /// `PUNKTFUNK_GAMESCOPE_BIND` — may the host bind the patched gamescope over
    /// `/usr/bin/gamescope` inside the session unit's mount namespace? That redirect is the ONLY
    /// lever left on a distro whose `gamescope-session-plus` hardcodes that absolute path and
    /// reads `GAMESCOPE_BIN` nowhere (Nobara) — see `pf-vdisplay`'s `gamescope.rs`.
    ///
    /// **Three-valued**, because the mechanism is not free and the default has to be the careful
    /// one. A mount namespace in a systemd **user** unit necessarily comes with a **user**
    /// namespace, which maps only this uid — so every root-owned path the session inspects reads
    /// as `nobody`, and that is what made gamescope's Xwayland refuse `/tmp/.X11-unix` and killed
    /// Game Mode outright in 0.26.0-canary.
    ///
    /// * `None` (unset — the default): AUTO. The host reads the box's session script and arms the
    ///   redirect only where nothing else can reach gamescope. Every other distro gets no mount
    ///   namespace at all.
    /// * `Some(false)` (`=0`): never. The session runs the distro's stock gamescope — no HDR, no
    ///   in-node cursor, games see gamescope's 60 Hz headless default — degraded, but it starts.
    /// * `Some(true)` (`=1`): force. Arm it even where the script looks like it honours
    ///   `GAMESCOPE_BIN` — for the case that lever is defeated somewhere the host cannot see (a
    ///   `sessions.d` fragment presetting `GAMESCOPECMD`). It does NOT override the runtime
    ///   backstop: a session that fails with the redirect armed still disarms it.
    pub gamescope_bind: Option<bool>,
    /// `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` — extra refresh rates (Hz, comma-separated) a gamescope
    /// session offers its clients on top of the one it runs at, e.g. `60,90,120`.
    ///
    /// A headless gamescope has no EDID, so it cannot work out what else its display could run at:
    /// on a stock build it advertises exactly ONE rate and Steam's in-session display settings show
    /// a single entry. Our `+pfhdr3` build takes this list (`--custom-refresh-rates`) and publishes
    /// it, which is what puts real choices in that menu. The session's own rate is always included
    /// whatever is set here, so this can only ever ADD options.
    ///
    /// Empty (the default) = advertise only the negotiated rate. Ignored on a stock gamescope,
    /// which has no flag to take it.
    pub gamescope_refresh_rates: Vec<u32>,
    /// `PUNKTFUNK_RECOVER_SESSION_CMD` — operator hook fired (debounced) when a client connects while NO
    /// graphical session is live for this uid: the state a compositor crash leaves behind (gnome-shell
    /// SIGSEGV → GDM greeter, whose auto-login is once-per-boot, so the box would otherwise need a walk-up
    /// or reboot). Typically `sudo -n systemctl restart gdm` with a matching NOPASSWD sudoers rule, or
    /// `systemctl restart display-manager` under a polkit rule — with auto-login enabled the restart brings
    /// the desktop back and the client's retry lands in it. Unset/empty = disabled (the default).
    pub recover_session_cmd: Option<String>,
    /// `PUNKTFUNK_ON_CONNECT_CMD` — zero-config mirror of a `client.connected` hook
    /// (`crate::hooks`): fired detached with the event JSON on stdin + `PF_EVENT_*` env when a
    /// client connects, on either plane. The full hook surface (filters, webhooks, debounce)
    /// lives in `hooks.json`. Unset/empty = disabled (the default).
    pub on_connect_cmd: Option<String>,
    /// `PUNKTFUNK_ON_DISCONNECT_CMD` — the `client.disconnected` sibling of
    /// [`Self::on_connect_cmd`].
    pub on_disconnect_cmd: Option<String>,
    /// `PUNKTFUNK_MAX_FPS` — frame limiter for the GAME. `None` (unset, `0`, or unparseable) =
    /// no limit, the default and what every existing host does.
    ///
    /// This caps how fast the compositor lets the game render; it does **not** touch the session.
    /// The client still negotiates and receives its full rate — a 120 Hz session over a game
    /// limited to 60 sends 120 frames a second, 60 of them repeats of an unchanged picture, which
    /// costs an almost-empty P-frame. That split is the whole point: the game stops rendering
    /// frames nobody asked for, and the GPU time it gives up goes to capture and encode instead
    /// (and, on a laptop or handheld, to heat and battery).
    ///
    /// Capping the STREAM instead would be a different and mostly unwanted feature — it hands the
    /// client fewer frames than it asked for and saves the game's GPU nothing.
    ///
    /// Enforced by the compositor, so its reach is whatever that compositor offers. **gamescope**
    /// takes it as `--nested-refresh`, the rate it clamps the game to; note that is the nested
    /// output's rate, so everything gamescope composites moves at it, not the game alone — under
    /// gamescope there is only the one output. Values are clamped into 1..=240.
    pub max_fps: Option<u32>,
    /// `PUNKTFUNK_VDISPLAY_HZ_MULT` — run the VIRTUAL DISPLAY at this multiple of the session's
    /// frame rate while the stream stays paced at the session rate. Default 1 (off); 2 is the
    /// interesting one, hence the name this shipped under.
    ///
    /// A compositor only paints on its own vblank, so at 1× a frame can be finished just after
    /// the capture sampled and then waits nearly a whole interval to be picked up — up to
    /// ~16 ms of pure age at 60 Hz, and it is the jittery part of the latency, not the steady
    /// part. Driving the display at 2× halves that worst case without sending a single extra
    /// frame: the pacing clamp below keeps the wire at exactly the rate the client negotiated.
    ///
    /// It is not free — the compositor and the GPU do the extra composites — so it stays opt-in.
    /// Clamped to 1..=4; a backend that cannot honor the multiplied rate simply reports what it
    /// achieved and the pacing follows that, exactly as it does for any other refusal.
    pub vdisplay_hz_mult: u32,
}

impl HostConfig {
    fn from_env() -> Self {
        // Presence flag: set ⇒ true. Matches the original `var_os(k).is_some()` reads (and the few
        // `var(k).is_ok()` flag reads, which coincide for every real-world value).
        let flag = |k: &str| std::env::var_os(k).is_some();
        // String value: `var(k).ok()` — `Some` (possibly empty) when set with valid UTF-8, else `None`.
        let val = |k: &str| std::env::var(k).ok();
        Self {
            // (`PUNKTFUNK_IDD_PUSH` was removed: IDD-push is the sole Windows capture path, so the knob
            // only split dispatch — capture ignored it while the vdisplay manager obeyed it, and `=0`
            // produced dead-swap-chain reuse on reconnect. A stale setting in an old host.env is ignored.)
            host_name: val("PUNKTFUNK_HOST_NAME")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Blank-is-unset, like `host_name` above: an operator who comments a value out by
            // emptying it (`PUNKTFUNK_MGMT_BIND=`) means "default", not "parse the empty string".
            mgmt_bind: val("PUNKTFUNK_MGMT_BIND")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            native_port: val("PUNKTFUNK_NATIVE_PORT")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Default OFF, explicit-on grammar: the Moonlight-compat planes are opt-in
            // everywhere (see the field doc); `--gamestream` on the CLI also turns them on.
            gamestream: env_on("PUNKTFUNK_GAMESTREAM").unwrap_or(false),
            encoder_pref: std::env::var("PUNKTFUNK_ENCODER")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            render_adapter: val("PUNKTFUNK_RENDER_ADAPTER"),
            idd_depth: val("PUNKTFUNK_IDD_DEPTH")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2),
            zerocopy: env_on("PUNKTFUNK_ZEROCOPY"),
            // Default ON, explicit-off grammar (mirrors `four_four_four`: the client's HDR setting
            // is the real per-session switch; the encode probe keeps incapable GPUs honest at 8-bit).
            ten_bit: env_on("PUNKTFUNK_10BIT").unwrap_or(true),
            // Default ON, explicit-off grammar (the client's own 4:4:4 setting — default OFF —
            // is the real switch; see the field doc).
            four_four_four: env_on("PUNKTFUNK_444").unwrap_or(true),
            // Default ON, explicit-off grammar (the client's VIDEO_CAP_CHACHA20 bit is the real
            // per-session switch; see the field doc).
            chacha20: env_on("PUNKTFUNK_CHACHA20").unwrap_or(true),
            audio_output_mode: AudioOutputMode::from_env(),
            audio_quality: val("PUNKTFUNK_AUDIO_QUALITY").map(|s| s.trim().to_lowercase()),
            audio_redundancy: env_on("PUNKTFUNK_AUDIO_REDUNDANCY"),
            // Default ON, explicit-off grammar (the client's CLIENT_CAP_AUDIO_HIRES bit — and the
            // audio-format row its user picked — is the real per-session switch; the §8.4 gate's
            // capture, bandwidth and datagram conditions are what keep a modest link safe, not
            // this. See the field doc for why it stopped being opt-in).
            audio_hires: env_on("PUNKTFUNK_AUDIO_HIRES").unwrap_or(true),
            perf: flag("PUNKTFUNK_PERF"),
            // Default ON while the interval-stutter field program runs (see the field doc).
            stall_probes: env_on("PUNKTFUNK_STALL_PROBES").unwrap_or(true),
            // Defaults to `virtual` — the flagship per-client virtual output. It used to be unset,
            // which fell through to the synthetic test pattern: fine for a dev box that always has
            // a host.env, wrong for a packaged install, whose unit no longer requires that file at
            // all. `synthetic` is still reachable by naming it (any unrecognised value lands there).
            video_source: val("PUNKTFUNK_VIDEO_SOURCE").or_else(|| Some("virtual".to_string())),
            // Trimmed + emptied-to-None: `PUNKTFUNK_CAPTURE_MONITOR=` in a host.env means "not
            // set", not "match the monitor named empty string".
            capture_monitor: val("PUNKTFUNK_CAPTURE_MONITOR")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // Same emptied-to-None rule: a bare `PUNKTFUNK_PORTAL_CURSOR_MODE=` left in a host.env
            // means "not set", not an unrecognised value to warn about. The spellings are parsed
            // (and warned about) at the use site, `pf-vdisplay`'s `portal_cursor::want`.
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
            // Default ON, explicit-off grammar: the splash is what makes a fresh bare spawn deliver
            // its first frames at all; `=0` is the A/B + escape hatch.
            gamescope_splash: env_on("PUNKTFUNK_GAMESCOPE_SPLASH").unwrap_or(true),
            // Default OFF for one canary release (design §4 rollout), then flip the `unwrap_or`.
            gamescope_hdr: env_on("PUNKTFUNK_GAMESCOPE_HDR").unwrap_or(true),
            gamescope_sdr_nits: val("PUNKTFUNK_GAMESCOPE_SDR_NITS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|n| (1..=10_000).contains(n)),
            // Deliberately NOT `unwrap_or`: unset is its own answer here (auto — the host decides
            // per box), `=0` is the retreat to a stock-gamescope session, and `=1` is the force
            // for a box whose `GAMESCOPE_BIN` is defeated somewhere the host cannot read.
            gamescope_bind: env_on("PUNKTFUNK_GAMESCOPE_BIND"),
            // Unparseable entries are DROPPED rather than failing the host: this only ever widens a
            // menu, and the session's own rate is added back unconditionally, so the worst a typo
            // can cost is the extra option the operator wanted — never the session.
            gamescope_refresh_rates: parse_refresh_rates(
                val("PUNKTFUNK_GAMESCOPE_REFRESH_RATES").as_deref(),
            ),
            recover_session_cmd: val("PUNKTFUNK_RECOVER_SESSION_CMD")
                .filter(|s| !s.trim().is_empty()),
            on_connect_cmd: val("PUNKTFUNK_ON_CONNECT_CMD").filter(|s| !s.trim().is_empty()),
            on_disconnect_cmd: val("PUNKTFUNK_ON_DISCONNECT_CMD").filter(|s| !s.trim().is_empty()),
            // 0 means "no limit" rather than "stream nothing" — it is the natural way to spell
            // "off" in a config file, and a 0 fps session is not a thing anyone wants.
            max_fps: val("PUNKTFUNK_MAX_FPS")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&f| f > 0)
                .map(|f| f.clamp(1, 240)),
            vdisplay_hz_mult: val("PUNKTFUNK_VDISPLAY_HZ_MULT")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1)
                .clamp(1, 4),
        }
    }
}

/// `"60, 90,120"` → `[60, 90, 120]`, sorted and deduped. Junk entries and out-of-range rates are
/// skipped rather than rejected wholesale — see the call site for why. Pure + unit-tested.
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
    /// The rate to hand the compositor as the GAME's refresh: the session's rate, capped by
    /// [`Self::max_fps`]. Only the compositor's game-facing rate goes through here — the session's
    /// own mode, the encoder and the wire never do (see the field docs for why).
    ///
    /// `0` in means `0` out. A zero rate is rejected upstream, and quietly turning it into a real
    /// one here would hide that.
    pub fn game_fps(&self, session_hz: u32) -> u32 {
        match self.max_fps {
            Some(cap) if session_hz > cap => cap,
            _ => session_hz,
        }
    }
}

/// The process-wide host configuration, parsed once on first access.
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
        // Unset: every session rate passes through untouched — the default, and every existing
        // host. The game keeps rendering at the session's rate, exactly as it always did.
        for hz in [24, 30, 60, 120, 144, 240] {
            assert_eq!(cfg(None).game_fps(hz), hz);
        }
        // Set: capped above, exact at, untouched below. A session BELOW the limit keeps its own
        // rate — the knob is a ceiling on the game, not a target to render up to.
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
        // Spaces, unsorted input and duplicates all normalise.
        assert_eq!(
            parse_refresh_rates(Some(" 120, 60 ,90, 60")),
            vec![60, 90, 120]
        );
        // Unset and empty are the default: advertise only the session's own rate.
        assert!(parse_refresh_rates(None).is_empty());
        assert!(parse_refresh_rates(Some("")).is_empty());
        assert!(parse_refresh_rates(Some("   ")).is_empty());
        // A typo costs its own entry, never the whole list — the knob only widens a menu.
        assert_eq!(parse_refresh_rates(Some("60,abc,120")), vec![60, 120]);
        // Out of range in both directions (0 is not a refresh rate; 1920 is a width).
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
        // Unknown spellings are rejected so the caller can say so, not silently re-routed.
        for s in ["", "silent", "off", "true"] {
            assert_eq!(AudioOutputMode::parse(s), None, "{s:?}");
        }
        // Round-trip through the canonical spelling.
        for m in [
            AudioOutputMode::ClientOnly,
            AudioOutputMode::HostAndClient,
            AudioOutputMode::FollowDefault,
        ] {
            assert_eq!(AudioOutputMode::parse(m.as_str()), Some(m));
        }
    }

    /// The two predicates are what the wiring plan and the capture loop actually branch on, and
    /// they must stay mutually exclusive: "prefer host hardware" and "touch nothing" are different
    /// promises, and conflating them would either silence the host or stomp the operator's devices.
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
