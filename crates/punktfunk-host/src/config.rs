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
//! - **Runtime-mutated session vars.** On Linux, [`crate::vdisplay::apply_session_env`] rewrites the process
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
    /// `PUNKTFUNK_10BIT` — host policy gate for HEVC Main10 (only honored when the client also advertised 10-bit).
    pub ten_bit: bool,
    /// `PUNKTFUNK_444` — host policy gate for full-chroma HEVC 4:4:4 (Range Extensions). Honored only
    /// when the client also advertised 4:4:4, the codec is HEVC, and the GPU/driver supports a 4:4:4
    /// encode (probed) — otherwise the session stays 4:2:0. Independent of `ten_bit` (chroma vs depth).
    pub four_four_four: bool,
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
            zerocopy: val("PUNKTFUNK_ZEROCOPY").map(|s| {
                !matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            }),
            ten_bit: flag("PUNKTFUNK_10BIT"),
            four_four_four: flag("PUNKTFUNK_444"),
            perf: flag("PUNKTFUNK_PERF"),
            video_source: val("PUNKTFUNK_VIDEO_SOURCE"),
            compositor: val("PUNKTFUNK_COMPOSITOR"),
            gamepad: val("PUNKTFUNK_GAMEPAD"),
            vdisplay: val("PUNKTFUNK_VDISPLAY"),
        }
    }
}

/// The process-wide host configuration, parsed once on first access.
pub fn config() -> &'static HostConfig {
    static CFG: OnceLock<HostConfig> = OnceLock::new();
    CFG.get_or_init(HostConfig::from_env)
}
