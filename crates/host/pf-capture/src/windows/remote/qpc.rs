//! The clocks a remote source runs on in production: `Instant`, wall time, and the
//! system-wide QPC the far side stamps its slots and heartbeat in.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::au_assembly::Clocks;

pub(crate) const CLOCKS: Clocks = Clocks {
    now: Instant::now,
    now_ns,
    qpc_age_us,
    qpc_ticks_us,
};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// QPC ticks per second, read once. `0` when the call failed; every span then reads 0.
pub(crate) fn freq() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: plain FFI; `f` is a valid local out-param. Frequency is fixed at boot.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(0) as u64
    })
}

/// A span of QPC ticks in µs (QPC is system-wide, so two far-side stamps subtract).
fn qpc_ticks_us(ticks: u64) -> u64 {
    match freq() {
        0 => 0,
        freq => ticks.saturating_mul(1_000_000) / freq,
    }
}

/// Age of a far-side QPC stamp in µs. 0 if the stamp is ahead.
fn qpc_age_us(stamp: u64) -> u64 {
    let mut now = 0i64;
    // SAFETY: plain FFI; `now` is a valid local out-param.
    if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
        return 0;
    }
    qpc_ticks_us((now as u64).saturating_sub(stamp))
}
