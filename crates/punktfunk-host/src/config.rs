//! `HostConfig` — the host's runtime knobs parsed ONCE from the environment, instead of the ~68 scattered
//! `env::var` reads recomputed at every call site (some up to 8×, which lets capture + encode silently
//! disagree on the resolved backend — plan §2.4). The service / launcher loads `host.env` into the process
//! environment before the host starts, and the environment is constant for the process lifetime, so a
//! lazily-parsed global is equivalent to "parsed once at startup".
//!
//! **Goal-1 stage 1** (`docs/windows-host-goal1-plan.md`): this is the foundation. Subsequent stages grow
//! this struct + migrate the remaining read sites onto it, then `SessionPlan` (stage 2) consumes it as the
//! single owner of the capture/topology/encoder decision. New fields are added here AS call sites migrate —
//! a field that nothing reads yet would just be dead, so they land together with their migration.

use std::sync::OnceLock;

/// Resolved host configuration. Grows as `env::var` call sites migrate onto it (Goal-1).
#[derive(Debug, Clone, Default)]
pub struct HostConfig {
    /// `PUNKTFUNK_IDD_PUSH` — use the IDD direct-push capturer (in-process Session-0 capture; no WGC helper).
    pub idd_push: bool,
    /// `PUNKTFUNK_ENCODER` — explicit encoder-backend override (lowercased; empty = auto-detect by GPU vendor).
    pub encoder_pref: String,
    /// `PUNKTFUNK_NO_HELPER` — never spawn the user-session WGC helper.
    pub no_helper: bool,
    /// `PUNKTFUNK_FORCE_HELPER` — force the WGC helper even when not running as SYSTEM.
    pub force_helper: bool,
}

impl HostConfig {
    fn from_env() -> Self {
        let flag = |k: &str| std::env::var_os(k).is_some();
        Self {
            idd_push: flag("PUNKTFUNK_IDD_PUSH"),
            encoder_pref: std::env::var("PUNKTFUNK_ENCODER")
                .unwrap_or_default()
                .to_ascii_lowercase(),
            no_helper: flag("PUNKTFUNK_NO_HELPER"),
            force_helper: flag("PUNKTFUNK_FORCE_HELPER"),
        }
    }
}

/// The process-wide host configuration, parsed once on first access.
pub fn config() -> &'static HostConfig {
    static CFG: OnceLock<HostConfig> = OnceLock::new();
    CFG.get_or_init(HostConfig::from_env)
}
