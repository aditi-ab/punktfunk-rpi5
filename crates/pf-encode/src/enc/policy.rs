//! Shared loss-recovery env parsing for the native NVENC/libav, AMF, and QSV
//! backends. Defaults stay with each backend (QSV LTR ~1/4 s, AMF ~1/2 s —
//! tuning, not drift). API clamps stay at the call site (QSV `mfxU16` 8..=240).
//! Sibling of `rfi.rs`, which owns the slot-recovery policy.

/// Trimmed truthy env opt-in. A trailing space must not disagree per backend.
pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// `PUNKTFUNK_INTRA_REFRESH` — opt into the intra-refresh wave: a moving
/// intra band heals FEC-unrecoverable loss without a 20-40× IDR spike.
/// Linux ANDs its `IR_UNSUPPORTED` latch on top. On Windows this also
/// selects LTR vs IR (the wave sweeps the picture; LTR pins references).
pub(crate) fn intra_refresh_requested() -> bool {
    env_flag("PUNKTFUNK_INTRA_REFRESH")
}

/// `PUNKTFUNK_IR_PERIOD_FRAMES` — wave length in frames (`>= 2` or it is not
/// a wave). Default is half a second of frames (~2-3 % intra cost per frame).
/// Backends clamp to their API field at the call site.
pub(crate) fn intra_refresh_period(fps: u32) -> u32 {
    std::env::var("PUNKTFUNK_IR_PERIOD_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|v| *v >= 2)
        .unwrap_or_else(|| fps.max(16) / 2)
}

/// `PUNKTFUNK_LTR_INTERVAL_FRAMES` — LTR mark cadence (`>= 1`). `None` leaves
/// the backend's tuned default; it does not disable LTR.
#[cfg(target_os = "windows")]
pub(crate) fn ltr_interval_env() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_INTERVAL_FRAMES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 1)
}

/// `PUNKTFUNK_LTR_FORCE_AT=N` — spike-only: at `frame_idx == N` the encoder
/// self-triggers `invalidate_ref_frames` so a headless run exercises LTR
/// recovery. `None` normally. N must be `> 0`; frame 0 is the opening IDR.
#[cfg(target_os = "windows")]
pub(crate) fn ltr_test_force_at() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_FORCE_AT")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
}
