//! Live decode stats for the on-stream HUD (mirrors the Apple client's stats overlay): FPS,
//! receive throughput, and capture→client-receipt latency (p50/p95). The decode thread is the sole
//! writer (`note` per access unit); the JNI accessor `nativeVideoStats` drains a snapshot ~1 Hz and
//! resets the window. Pure `std` so it compiles on the host build too (the decode thread is
//! android-only, but `VideoThread` holds the shared handle unconditionally).

use std::sync::Mutex;
use std::time::Instant;

/// Rolling per-window accumulator. Rates are computed over the actual elapsed wall-time at drain
/// (robust to poll jitter), so a poll that lands at 0.9 s or 1.1 s still reports the right FPS.
pub struct VideoStats {
    inner: Mutex<Inner>,
}

struct Inner {
    window_start: Instant,
    frames: u64,
    bytes: u64,
    /// capture→client-receipt latency samples for this window, in microseconds.
    lat_us: Vec<u64>,
    /// Whether the host answered the clock-skew handshake (latency is cross-machine valid).
    skew_corrected: bool,
}

/// A drained, computed view of one window. `lat_valid` is false when no in-range latency sample
/// landed (then p50/p95 are 0 and the HUD hides the latency line, exactly like the Apple client).
pub struct Snapshot {
    pub fps: f64,
    pub mbps: f64,
    pub lat_p50_ms: f64,
    pub lat_p95_ms: f64,
    pub lat_valid: bool,
    pub skew_corrected: bool,
}

impl VideoStats {
    // `new`/`note` are driven only by the android-only decode thread; `drain` (the JNI accessor) is
    // ungated, so on the host build these two are unreferenced — that's expected, not dead code.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn new() -> VideoStats {
        VideoStats {
            inner: Mutex::new(Inner {
                window_start: Instant::now(),
                frames: 0,
                bytes: 0,
                lat_us: Vec::with_capacity(256),
                skew_corrected: false,
            }),
        }
    }

    /// Record one decoded access unit: its wire size and (if in range) its capture→client latency.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note(&self, bytes: usize, lat_us: Option<u64>, skew_corrected: bool) {
        let mut g = self.inner.lock().unwrap();
        g.frames += 1;
        g.bytes += bytes as u64;
        g.skew_corrected = skew_corrected;
        if let Some(l) = lat_us {
            g.lat_us.push(l);
        }
    }

    /// Compute the window's rates + latency percentiles, then reset for the next window.
    pub fn drain(&self) -> Snapshot {
        let mut g = self.inner.lock().unwrap();
        let elapsed = g.window_start.elapsed().as_secs_f64().max(1e-3);
        let fps = g.frames as f64 / elapsed;
        let mbps = g.bytes as f64 * 8.0 / 1_000_000.0 / elapsed;
        let (p50, p95, valid) = if g.lat_us.is_empty() {
            (0.0, 0.0, false)
        } else {
            g.lat_us.sort_unstable();
            let n = g.lat_us.len();
            let at = |p: f64| g.lat_us[((n as f64 * p) as usize).min(n - 1)] as f64 / 1000.0;
            (at(0.50), at(0.95), true)
        };
        let skew = g.skew_corrected;
        g.window_start = Instant::now();
        g.frames = 0;
        g.bytes = 0;
        g.lat_us.clear();
        Snapshot {
            fps,
            mbps,
            lat_p50_ms: p50,
            lat_p95_ms: p95,
            lat_valid: valid,
            skew_corrected: skew,
        }
    }
}
