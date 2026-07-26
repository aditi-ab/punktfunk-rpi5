//! `HostConfig` — the host's runtime knobs parsed ONCE from the environment, instead of the ~68 scattered
//! `env::var` reads recomputed at every call site (some up to 8×, which lets capture + encode silently
//! disagree on the resolved backend — plan §2.4). The service / launcher loads `host.env` into the process
//! environment before the host starts, and **for the knobs captured here the environment is constant for the
//! process lifetime**, so a lazily-parsed global is equivalent to "parsed once at startup".
//!
//! **Goal-1 stages 1–2** (`design/windows-host-rewrite.md` §2.2): stage 1 stood this up; stage 2 migrated the
//! genuinely-constant operator/dispatch knobs onto it (the dispatch-disagreement bug class:
//! `encoder_pref`, `render_adapter`, the vdisplay backend select — plus the plan-named
//! `idd_depth`/`zerocopy`/`ten_bit`/`four_four_four` and the multi-site `perf`/`compositor`/
//! `video_source`/`gamepad`). `SessionPlan` (stage 3) consumes it as the single owner of the
//! capture/topology/encoder decision.
//!
//! **What is deliberately NOT here (and must stay a live `env::var` read):**
//! - **Runtime-mutated session vars.** On Linux, `crate::vdisplay::apply_session_env` rewrites the process
//!   env on *every connect* so one host follows a Bazzite box across Gaming↔Desktop: `WAYLAND_DISPLAY`,
//!   `XDG_CURRENT_DESKTOP`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, and the *derived* `PUNKTFUNK_*`
//!   vars `INPUT_BACKEND`, `GAMESCOPE_SESSION`/`GAMESCOPE_NODE`, `KWIN_VIRTUAL_PRIMARY`,
//!   `MUTTER_VIRTUAL_PRIMARY`, `FORCE_SHM` (+ `GAMESCOPE_APP` on the launch path). Parsing these once would
//!   freeze them at startup and silently break session-following — they are NOT constant.
//! - **Single-use local tuning** read exactly where it is used (no resolve-once benefit, and a parse with a
//!   call-site-local default/clamp): e.g. `FEC_PCT` (two *different* semantics — GameStream default-20 vs
//!   punktfunk/1 `Option`/clamp-90), `VIDEO_DROP`, `VBV_FRAMES`, `SPLIT_ENCODE`, `PACE_BURST_KB`, the
//!   `capture/dxgi.rs` timing knobs, the `*_LIVE` test gates.
//! - **Path / genuinely-dynamic reads**: the config-dir resolution, `PATH` executable search, the
//!   env-forward-to-child loop, `PUNKTFUNK_MGMT_TOKEN`, `PUNKTFUNK_HOST_CMD`, `PUNKTFUNK_RENDER_NODE`.
//!
//! `PUNKTFUNK_ZEROCOPY` note: this field is a **tri-state override** (`None` = unset). Unset defers to
//! the per-vendor default in `encode/ffmpeg_win.rs::zerocopy_enabled` (AMF on — on-glass validated
//! 2026-07-06; QSV off until validated on Intel glass); an explicit value forces it (`0|false|off|no`
//! = off, anything else = on, so the old presence-style `=1` keeps working). The Linux `zerocopy`
//! module keeps its own *truthy* parser (`1|true|yes|on`) — the two are independent features that
//! share a name; do NOT conflate them.

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

/// Resolved host configuration. Holds the genuinely-constant operator/dispatch knobs (see module docs for
/// what is deliberately excluded). Fields read on only one platform are kept alive cross-platform by the
/// derived `Debug` impl, so the parser can stay a single platform-neutral function.
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
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
    /// `PUNKTFUNK_PERF` — per-stage timing instrumentation.
    pub perf: bool,
    /// `PUNKTFUNK_VIDEO_SOURCE` — GameStream video source select (`virtual` / `portal` / unset → synthetic).
    pub video_source: Option<String>,
    /// `PUNKTFUNK_COMPOSITOR` — explicit compositor override (operator/CI/test). NOT the runtime-detected
    /// session — this one is a constant operator knob; `apply_session_env` never writes it.
    pub compositor: Option<String>,
    /// `PUNKTFUNK_GAMEPAD` — client/operator virtual-pad backend preference (fed to `pick_gamepad`).
    pub gamepad: Option<String>,
    /// `PUNKTFUNK_VDISPLAY` — Windows virtual-display backend. The pf-vdisplay IddCx driver is now the only
    /// backend (the legacy SudoVDA backend was removed), so this is currently informational — kept for the
    /// shipped `host.env` and as a forward seam if a second backend is ever added.
    pub vdisplay: Option<String>,
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
            perf: flag("PUNKTFUNK_PERF"),
            video_source: val("PUNKTFUNK_VIDEO_SOURCE"),
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
            recover_session_cmd: val("PUNKTFUNK_RECOVER_SESSION_CMD")
                .filter(|s| !s.trim().is_empty()),
            on_connect_cmd: val("PUNKTFUNK_ON_CONNECT_CMD").filter(|s| !s.trim().is_empty()),
            on_disconnect_cmd: val("PUNKTFUNK_ON_DISCONNECT_CMD").filter(|s| !s.trim().is_empty()),
        }
    }
}

/// The process-wide host configuration, parsed once on first access.
pub fn config() -> &'static HostConfig {
    static CFG: OnceLock<HostConfig> = OnceLock::new();
    CFG.get_or_init(HostConfig::from_env)
}
