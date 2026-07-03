//! Live decode stats for the on-stream HUD, following the unified stats spec
//! (`design/stats-unification.md`): FPS, receive throughput, and the Android v1 stage split —
//! headline `end-to-end` = capture→decoded (p50/p95) tiled by `host+network` = capture→received
//! and `decode` = received→decoded (stage p50s). When the host emits per-AU 0xCF host timings, the
//! `host+network` term further splits into `host` + `network` (Phase 2, `note_host_split`); an old
//! host emits none and the combined term stands. The decode thread is the sole writer
//! (`note_received` per access unit at receipt, `note_decoded` per decoder output buffer); the JNI
//! accessor `nativeVideoStats` drains a snapshot ~1 Hz and resets the window. Sampling is gated on
//! the HUD actually being visible (`set_enabled`, driven by `nativeSetVideoStatsEnabled`) so the
//! hidden steady state costs one relaxed atomic load per frame.
//! Pure `std` so it compiles on the host build too (the decode thread is android-only, but
//! `SessionHandle` holds the shared handle unconditionally).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Rolling per-window accumulator. Rates are computed over the actual elapsed wall-time at drain
/// (robust to poll jitter), so a poll that lands at 0.9 s or 1.1 s still reports the right FPS.
pub struct VideoStats {
    /// HUD gate: the samplers run on the per-frame decode path, so while the overlay is hidden
    /// they (and the caller's latency computation — see `enabled`) early-out on this flag alone.
    /// Off until Kotlin shows the HUD.
    enabled: AtomicBool,
    inner: Mutex<Inner>,
}

struct Inner {
    window_start: Instant,
    frames: u64,
    bytes: u64,
    /// `end-to-end` = capture→decoded latency samples for this window, in microseconds
    /// (skew-corrected clock base).
    e2e_us: Vec<u64>,
    /// `host+network` stage = capture→received samples, in microseconds (skew-corrected).
    hostnet_us: Vec<u64>,
    /// Phase-2 split of `host+network` (design/stats-unification.md Phase 2), fed only when the
    /// host emits per-AU 0xCF timings: `host` = the host's own capture→sent duration, µs.
    host_us: Vec<u64>,
    /// The matching `network` term, µs: capture→received minus the host's capture→sent
    /// (wire + reassembly). Always pushed in lockstep with `host_us`.
    net_us: Vec<u64>,
    /// `decode` stage = received→decoded samples, in microseconds (client-local, single clock).
    decode_us: Vec<u64>,
    /// Whether the host answered the clock-skew handshake (latency is cross-machine valid).
    skew_corrected: bool,
}

/// A drained, computed view of one window. `lat_valid` is false when no in-range end-to-end sample
/// landed (then the latency figures are 0 and the HUD hides the latency lines, exactly like the
/// Apple client).
pub struct Snapshot {
    pub fps: f64,
    pub mbps: f64,
    /// Headline `end-to-end` (capture→decoded) percentiles, ms.
    pub e2e_p50_ms: f64,
    pub e2e_p95_ms: f64,
    /// Stage p50s (ms): `host+network` (capture→received) and `decode` (received→decoded).
    pub hostnet_p50_ms: f64,
    pub decode_p50_ms: f64,
    /// Phase-2 `host` / `network` split p50s (ms) — 0.0 when no 0xCF timing matched this window
    /// (old host / no samples yet), in which case the HUD keeps the combined `host+network` term.
    pub host_p50_ms: f64,
    pub net_p50_ms: f64,
    pub lat_valid: bool,
    pub skew_corrected: bool,
}

/// Percentile over a sorted-in-place µs sample vec, in ms. 0.0 when empty.
fn pctl_ms(sorted_us: &[u64], p: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let n = sorted_us.len();
    sorted_us[((n as f64 * p) as usize).min(n - 1)] as f64 / 1000.0
}

impl VideoStats {
    pub fn new() -> VideoStats {
        VideoStats {
            enabled: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                window_start: Instant::now(),
                frames: 0,
                bytes: 0,
                e2e_us: Vec::with_capacity(256),
                hostnet_us: Vec::with_capacity(256),
                host_us: Vec::with_capacity(256),
                net_us: Vec::with_capacity(256),
                decode_us: Vec::with_capacity(256),
                skew_corrected: false,
            }),
        }
    }

    /// Whether the HUD wants samples. The decode thread checks this BEFORE building a latency
    /// sample, so the per-frame wall-clock reads are skipped too while hidden.
    // Read only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Toggle sampling. Enabling resets the window, so the first HUD poll after a show never mixes
    /// in counters (or a window start) from before the overlay was visible.
    pub fn set_enabled(&self, on: bool) {
        let was = self.enabled.swap(on, Ordering::Relaxed);
        if on && !was {
            let mut g = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.window_start = Instant::now();
            g.frames = 0;
            g.bytes = 0;
            g.e2e_us.clear();
            g.hostnet_us.clear();
            g.host_us.clear();
            g.net_us.clear();
            g.decode_us.clear();
        }
    }

    /// Record one received access unit: its wire size and (if in range) its capture→received
    /// `host+network` stage sample. Receipt is the fps/goodput counting point per the spec.
    // Driven only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_received(&self, bytes: usize, hostnet_us: Option<u64>, skew_corrected: bool) {
        if !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock (the caller already skipped the clock read)
        }
        // Poison-proof: this runs per-frame on the decode thread, which has no catch_unwind —
        // a panic elsewhere must not turn every later lock into a second panic (the counters
        // stay consistent regardless).
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.frames += 1;
        g.bytes += bytes as u64;
        g.skew_corrected = skew_corrected;
        if let Some(l) = hostnet_us {
            g.hostnet_us.push(l);
        }
    }

    /// Record one matched host/network split sample (Phase 2): the host's reported capture→sent
    /// duration and our capture→received minus it, both µs — one pair per AU whose 0xCF host
    /// timing arrived and matched by pts. An old host emits none, leaving the vecs empty and the
    /// snapshot p50s at 0 (HUD keeps the combined `host+network` term).
    // Driven only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_host_split(&self, host_us: u64, net_us: u64) {
        if !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock
        }
        // Poison-proof for the same reason as `note_received`.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.host_us.push(host_us);
        g.net_us.push(net_us);
    }

    /// Record one decoded output frame: its capture→decoded `end-to-end` sample and its
    /// received→decoded `decode` stage sample (either may be absent — e.g. the receipt stamp for
    /// this pts predates the HUD being shown).
    // Driven only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_decoded(&self, e2e_us: Option<u64>, decode_us: Option<u64>) {
        if !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock (the caller already skipped the clock read)
        }
        // Poison-proof for the same reason as `note_received`.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(l) = e2e_us {
            g.e2e_us.push(l);
        }
        if let Some(l) = decode_us {
            g.decode_us.push(l);
        }
    }

    /// Compute the window's rates + latency percentiles, then reset for the next window.
    pub fn drain(&self) -> Snapshot {
        // Poison-proof for the same reason as `note_received` — a poisoned window still drains
        // fine.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed = g.window_start.elapsed().as_secs_f64().max(1e-3);
        let fps = g.frames as f64 / elapsed;
        let mbps = g.bytes as f64 * 8.0 / 1_000_000.0 / elapsed;
        g.e2e_us.sort_unstable();
        g.hostnet_us.sort_unstable();
        g.host_us.sort_unstable();
        g.net_us.sort_unstable();
        g.decode_us.sort_unstable();
        let snap = Snapshot {
            fps,
            mbps,
            e2e_p50_ms: pctl_ms(&g.e2e_us, 0.50),
            e2e_p95_ms: pctl_ms(&g.e2e_us, 0.95),
            hostnet_p50_ms: pctl_ms(&g.hostnet_us, 0.50),
            decode_p50_ms: pctl_ms(&g.decode_us, 0.50),
            host_p50_ms: pctl_ms(&g.host_us, 0.50),
            net_p50_ms: pctl_ms(&g.net_us, 0.50),
            lat_valid: !g.e2e_us.is_empty(),
            skew_corrected: g.skew_corrected,
        };
        g.window_start = Instant::now();
        g.frames = 0;
        g.bytes = 0;
        g.e2e_us.clear();
        g.hostnet_us.clear();
        g.host_us.clear();
        g.net_us.clear();
        g.decode_us.clear();
        snap
    }
}
