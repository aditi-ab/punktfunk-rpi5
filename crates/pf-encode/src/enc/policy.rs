// Loss-recovery env-knob PARSING shared by the native backends (Linux NVENC/libav, Windows
// AMF/QSV) — extracted from three hand-copies that had already diverged twice: QSV's
// `ltr_disabled` dropped the trim + `yes`/`on` spellings (a `set VAR=1 ` with a trailing space
// silently left LTR enabled on Intel while the identical value worked on AMD), and QSV's
// `intra_refresh_period` ignored the env var entirely. Both were fixed in place — and stayed
// three copies, so the next drift was a matter of time. Parse each knob ONCE.
//
// Parsing only: DEFAULTS stay with their backend where they differ (QSV marks LTR ~1/4 s, AMF
// ~1/2 s — deliberate tuning, not drift), and API-bound clamps stay at the call site (QSV's
// `mfxU16` 8..=240). Sibling of `rfi.rs`, which did the same for the slot-recovery policy.

/// Truthy env opt-in: `1` / `true` / `yes` / `on`, trimmed (see the QSV trailing-space incident
/// above — every backend must accept the same spellings).
pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// `PUNKTFUNK_INTRA_REFRESH` — opt into the intra-refresh loss-recovery wave: a moving intra
/// band with recovery-point signalling refreshes the whole picture every
/// [`intra_refresh_period`] frames, so FEC-unrecoverable loss heals without the 20-40× full-IDR
/// spike (which under loss causes more loss — the cascade). Linux ANDs its runtime
/// `IR_UNSUPPORTED` latch on top; on Windows this is also the LTR↔IR selector (mutually
/// exclusive — the wave sweeps the picture, LTR pins references).
pub(crate) fn intra_refresh_requested() -> bool {
    env_flag("PUNKTFUNK_INTRA_REFRESH")
}

/// `PUNKTFUNK_IR_PERIOD_FRAMES` — the intra-refresh wave length in frames (>= 2 to be a wave);
/// default half a second of frames (heals fast, spreads the intra cost to ~2-3 % per frame).
/// Backends narrow to their API's field type at the call site.
pub(crate) fn intra_refresh_period(fps: u32) -> u32 {
    std::env::var("PUNKTFUNK_IR_PERIOD_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|v| *v >= 2)
        .unwrap_or_else(|| fps.max(16) / 2)
}

/// `PUNKTFUNK_LTR_INTERVAL_FRAMES` — explicit LTR mark-cadence override (>= 1 frame); `None`
/// leaves the backend's tuned default in charge.
#[cfg(target_os = "windows")]
pub(crate) fn ltr_interval_env() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_INTERVAL_FRAMES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 1)
}

/// Validation hook (`PUNKTFUNK_LTR_FORCE_AT=N`, spike-only): at `frame_idx == N` the encoder
/// self-triggers its real `invalidate_ref_frames` path, so a headless spike run exercises LTR
/// recovery end-to-end (mark → force → recovery-anchor tag) without a live client. `None`
/// normally; N must be positive — frame 0 is the opening IDR. (QSV's hand-copy skipped that
/// filter, so `=0` behaved differently per vendor.)
#[cfg(target_os = "windows")]
pub(crate) fn ltr_test_force_at() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_FORCE_AT")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
}
