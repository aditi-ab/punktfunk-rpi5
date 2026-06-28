//! `HostConfig` — the host's runtime knobs parsed ONCE from the environment, instead of the ~68 scattered
//! `env::var` reads recomputed at every call site (some up to 8×, which lets capture + encode silently
//! disagree on the resolved backend — plan §2.4). The service / launcher loads `host.env` into the process
//! environment before the host starts, and **for the knobs captured here the environment is constant for the
//! process lifetime**, so a lazily-parsed global is equivalent to "parsed once at startup".
//!
//! **Goal-1 stages 1–2** (`design/windows-host-rewrite.md` §2.2): stage 1 stood this up; stage 2 migrated the
//! genuinely-constant operator/dispatch knobs onto it (the dispatch-disagreement bug class: `idd_push`,
//! `capture_backend`, `encoder_pref`, `render_adapter`, `no_wgc`, the vdisplay backend select — plus the
//! plan-named `secure_dda`/`idd_depth`/`zerocopy`/`ten_bit`/`four_four_four` and the multi-site `perf`/`compositor`/
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
//! `PUNKTFUNK_ZEROCOPY` note: this field uses **presence** semantics (`var_os(..).is_some()`) to match the
//! Windows `encode/ffmpeg_win.rs` reader. The Linux `zerocopy` module keeps its own *truthy* parser
//! (`1|true|yes|on`) — the two are independent features that share a name; do NOT conflate them.

use std::sync::OnceLock;

/// Resolved host configuration. Holds the genuinely-constant operator/dispatch knobs (see module docs for
/// what is deliberately excluded). Fields read on only one platform are kept alive cross-platform by the
/// derived `Debug` impl, so the parser can stay a single platform-neutral function.
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
    /// `PUNKTFUNK_IDD_PUSH` — capture from the pf-vdisplay driver's shared ring (in-process Session-0
    /// capture; no WGC helper). **Value-aware** (`0`/`false`/`no`/`off`/empty ⇒ off, else on); unset ⇒ off.
    /// The installer's default `host.env` sets it on, so a fresh install runs the validated IDD-push path
    /// (it falls back to DDA if the driver can't attach — see [`crate::capture`]). NOT a bare presence flag
    /// (so an operator can turn it OFF in `host.env` with `=0`, which a `var_os` presence check can't).
    pub idd_push: bool,
    /// `PUNKTFUNK_ENCODER` — explicit encoder-backend override (lowercased; empty = auto-detect by GPU vendor).
    pub encoder_pref: String,
    /// `PUNKTFUNK_NO_HELPER` — never spawn the user-session WGC helper.
    pub no_helper: bool,
    /// `PUNKTFUNK_FORCE_HELPER` — force the WGC helper even when not running as SYSTEM.
    pub force_helper: bool,
    /// `PUNKTFUNK_NO_WGC` — force the pure single-process DDA path (skip WGC and the two-process relay).
    pub no_wgc: bool,
    /// `PUNKTFUNK_CAPTURE` — explicit Windows capture-backend override (lowercased; `dda`/`dxgi` vs the WGC default).
    pub capture_backend: String,
    /// `PUNKTFUNK_RENDER_ADAPTER` — discrete render-GPU pin by description substring (`Some` even when empty:
    /// the empty string still counts as "set" for the presence checks, and the value reader filters it).
    pub render_adapter: Option<String>,
    /// `PUNKTFUNK_SECURE_DDA` — enable the experimental DDA-on-secure-desktop (Winlogon/UAC) mux leg.
    pub secure_dda: bool,
    /// `PUNKTFUNK_IDD_DEPTH` — IDD-push pipeline depth override (default 2; the call site clamps to its `OUT_RING`).
    pub idd_depth: usize,
    /// `PUNKTFUNK_ZEROCOPY` — opt into the Windows D3D11 zero-copy encode path (presence semantics; see module docs).
    pub zerocopy: bool,
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
            // Value-aware (not a bare presence flag): the shipped default `host.env` turns it ON, and an
            // operator turns it OFF with `PUNKTFUNK_IDD_PUSH=0` (a `var_os` presence check would read `=0`
            // as "on"). Unset ⇒ off (the dev / non-pf-driver default).
            idd_push: match std::env::var("PUNKTFUNK_IDD_PUSH") {
                Ok(v) => !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "" | "0" | "false" | "no" | "off"
                ),
                Err(_) => false,
            },
            encoder_pref: std::env::var("PUNKTFUNK_ENCODER")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            no_helper: flag("PUNKTFUNK_NO_HELPER"),
            force_helper: flag("PUNKTFUNK_FORCE_HELPER"),
            no_wgc: flag("PUNKTFUNK_NO_WGC"),
            capture_backend: std::env::var("PUNKTFUNK_CAPTURE")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            render_adapter: val("PUNKTFUNK_RENDER_ADAPTER"),
            secure_dda: flag("PUNKTFUNK_SECURE_DDA"),
            idd_depth: val("PUNKTFUNK_IDD_DEPTH")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2),
            zerocopy: flag("PUNKTFUNK_ZEROCOPY"),
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
