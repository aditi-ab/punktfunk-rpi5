//! Live decode stats for the on-stream HUD, following the unified stats spec
//! (`design/stats-unification.md`): FPS, receive throughput, and the full stage split — headline
//! `end-to-end` = capture→displayed (p50/p95, measured directly from the `OnFrameRendered` render
//! timestamps — `note_displayed`) tiled by `host+network` = capture→received, `decode` =
//! received→decoded, and `display` = decoded→displayed (stage p50s). When the platform delivers no
//! render callbacks (allowed under load; `disp_valid` false), the HUD falls back to the v1
//! capture→decoded headline without the `display` term. When the host emits per-AU 0xCF host
//! timings, the `host+network` term further splits into `host` + `network` (Phase 2,
//! `note_host_split`); an old host emits none and the combined term stands. The spec's line-4 counters are per-window too:
//! `lost` / `FEC` are windowed here from the connector's cumulative counters (the caller passes the
//! current totals into `set_enabled`/`drain`), `skipped` counts the client's own newest-wins drops
//! (`note_skipped`). The decode thread is the sole writer
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
    /// Whether the timeline presenter is active this session (async loop, not sysprop-disabled) —
    /// stats index 29, so the HUD can label the display split it is (or isn't) getting.
    presenter_active: AtomicBool,
    /// The resolved decoder identity for the HUD: the codec's actual `AMediaCodec` name (e.g.
    /// `c2.qti.avc.decoder`) and whether it advertised `FEATURE_LowLatency`. Set once when the
    /// decode thread creates the codec (`set_decoder`), read one-shot by `nativeVideoDecoderLabel`.
    /// Separate from `inner` (never touched per-frame) so naming it costs nothing on the hot path.
    decoder: Mutex<Option<DecoderInfo>>,
    inner: Mutex<Inner>,
}

/// The chosen decoder's identity, surfaced on the stats HUD so before/after latency comparisons
/// name the codec that produced them.
struct DecoderInfo {
    name: String,
    low_latency: bool,
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
    /// `display` stage = decoded→displayed samples, in microseconds (client-local, single clock),
    /// from the `OnFrameRendered` render timestamps. Empty when the platform delivers no render
    /// callbacks — the HUD then drops the term and the headline endpoint moves back to `decoded`.
    display_us: Vec<u64>,
    /// `end-to-end` = capture→displayed samples, µs (skew-corrected) — the spec's headline,
    /// measured directly (not summed from stages). Empty under the same fallback as `display_us`.
    e2e_disp_us: Vec<u64>,
    /// The `display` stage's presenter split, decoded→release (pace wait: store + glass budget),
    /// µs. Empty on the legacy release-immediately path.
    pace_us: Vec<u64>,
    /// The other half of the split, release→displayed (SurfaceFlinger's latch + scanout), µs —
    /// from the `OnFrameRendered` render timestamps. `pace + latch ≈ display` per frame.
    latch_us: Vec<u64>,
    /// Frames confirmed on glass this window (`OnFrameRendered` callbacks) — the `presents`-vs-
    /// `fps` health pair: presents ≪ fps means the presenter is dropping/serializing; an fps
    /// deficit is upstream.
    presents: u64,
    /// Client-side newest-wins/pacing drops this window (decoded frames released without
    /// rendering, or parked AUs dropped on overflow) — the spec's `skipped` counter.
    skipped: u64,
    /// Baselines for windowing the session-cumulative connector counters: the unrecoverable-drop
    /// and FEC-recovered totals as of the last drain (or the enable that opened the window), so
    /// each snapshot reports only THIS window's `lost` / `FEC` (spec line 4).
    last_dropped_total: u64,
    last_fec_total: u64,
    /// Whether the host answered the clock-skew handshake (latency is cross-machine valid).
    skew_corrected: bool,
}

/// A drained, computed view of one window. `lat_valid` is false when no in-range end-to-end sample
/// landed (then the latency figures are 0 and the HUD hides the latency lines, exactly like the
/// Apple client).
pub struct Snapshot {
    pub fps: f64,
    pub mbps: f64,
    /// Headline `end-to-end` (capture→decoded) percentiles, ms — the fallback headline when no
    /// render callback landed this window (`disp_valid` false).
    pub e2e_p50_ms: f64,
    pub e2e_p95_ms: f64,
    /// The full headline: `end-to-end` = capture→displayed percentiles, ms, measured directly from
    /// the render timestamps. Meaningful only when `disp_valid`.
    pub e2e_disp_p50_ms: f64,
    pub e2e_disp_p95_ms: f64,
    /// Stage p50s (ms): `host+network` (capture→received), `decode` (received→decoded), and
    /// `display` (decoded→displayed; 0.0 when `disp_valid` is false — the HUD drops the term).
    pub hostnet_p50_ms: f64,
    pub decode_p50_ms: f64,
    pub display_p50_ms: f64,
    /// Whether any capture→displayed sample landed this window — gates the HUD's headline endpoint
    /// (`capture→displayed` vs the capture→decoded fallback) and the equation's `display` term.
    pub disp_valid: bool,
    /// The `display` stage's presenter split p50s (ms): `pace` = decoded→release (store + glass
    /// budget), `latch` = release→displayed (SurfaceFlinger). 0.0 when no sample landed (legacy
    /// path / no render callbacks).
    pub pace_p50_ms: f64,
    pub latch_p50_ms: f64,
    /// Frames confirmed on glass this window (`OnFrameRendered` callbacks).
    pub presents: u64,
    /// Phase-2 `host` / `network` split p50s (ms) — 0.0 when no 0xCF timing matched this window
    /// (old host / no samples yet), in which case the HUD keeps the combined `host+network` term.
    pub host_p50_ms: f64,
    pub net_p50_ms: f64,
    pub lat_valid: bool,
    pub skew_corrected: bool,
    /// Access units received this window (the count behind `fps`) — lets the HUD compute the
    /// spec's loss percentage `lost / (received + lost)` exactly.
    pub frames: u64,
    /// Unrecoverable network frame drops this window (spec `lost`, windowed from the
    /// session-cumulative connector counter).
    pub lost: u64,
    /// Client-side newest-wins/pacing drops this window (spec `skipped`).
    pub skipped: u64,
    /// FEC shards recovered this window (spec `FEC`, windowed from the cumulative counter).
    pub fec: u64,
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
            presenter_active: AtomicBool::new(false),
            decoder: Mutex::new(None),
            inner: Mutex::new(Inner {
                window_start: Instant::now(),
                frames: 0,
                bytes: 0,
                e2e_us: Vec::with_capacity(256),
                hostnet_us: Vec::with_capacity(256),
                host_us: Vec::with_capacity(256),
                net_us: Vec::with_capacity(256),
                decode_us: Vec::with_capacity(256),
                display_us: Vec::with_capacity(256),
                e2e_disp_us: Vec::with_capacity(256),
                pace_us: Vec::with_capacity(256),
                latch_us: Vec::with_capacity(256),
                presents: 0,
                skipped: 0,
                last_dropped_total: 0,
                last_fec_total: 0,
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

    /// Record whether the timeline presenter runs this session (decode thread, once at start).
    // Set only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_presenter_active(&self, on: bool) {
        self.presenter_active.store(on, Ordering::Relaxed);
    }

    /// Whether the timeline presenter is active (stats index 29).
    pub fn presenter_active(&self) -> bool {
        self.presenter_active.load(Ordering::Relaxed)
    }

    /// Toggle sampling. Enabling resets the window, so the first HUD poll after a show never mixes
    /// in counters (or a window start) from before the overlay was visible. `dropped_total` /
    /// `fec_total` are the connector's session-cumulative counters at this instant — they seed the
    /// windowing baselines so the first snapshot's `lost` / `FEC` cover only time the HUD was up.
    pub fn set_enabled(&self, on: bool, dropped_total: u64, fec_total: u64) {
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
            g.display_us.clear();
            g.e2e_disp_us.clear();
            g.pace_us.clear();
            g.latch_us.clear();
            g.presents = 0;
            g.skipped = 0;
            g.last_dropped_total = dropped_total;
            g.last_fec_total = fec_total;
        }
    }

    /// Record the resolved decoder identity for the HUD — the codec's real `AMediaCodec` name and
    /// whether it reported `FEATURE_LowLatency`. Called once from the decode thread right after the
    /// codec is created (before `configure`), overwriting any prior value on a surface recreate.
    // Set only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_decoder(&self, name: &str, low_latency: bool) {
        let mut g = self
            .decoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Some(DecoderInfo {
            name: name.to_owned(),
            low_latency,
        });
    }

    /// The decoder label for the HUD, e.g. `c2.qti.avc.decoder · low-latency`, or `""` before the
    /// decode thread has resolved one. Cheap (a lock + a string build); safe on the UI thread.
    pub fn decoder_label(&self) -> String {
        let g = self
            .decoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*g {
            Some(d) if d.low_latency => format!("{} · low-latency", d.name),
            Some(d) => d.name.clone(),
            None => String::new(),
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

    /// Record client-side frame skips (spec `skipped`): decoded output buffers released without
    /// rendering under the newest-wins policy, or parked AUs dropped on queue overflow.
    // Driven only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_skipped(&self, n: u64) {
        if n == 0 || !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock
        }
        // Poison-proof for the same reason as `note_received`.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.skipped += n;
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

    /// Record one displayed frame (the `OnFrameRendered` render timestamp, re-based to the
    /// realtime clock): its capture→displayed `end-to-end` sample, its decoded→displayed
    /// `display` stage sample, and the presenter split's `latch` half (release→displayed) — any
    /// may be absent (the e2e clamp rejected an out-of-range value, or the release record for
    /// this pts was already evicted). Fired from the codec's render-callback thread, not the
    /// decode thread — the lock makes that safe.
    // Driven only by the android-only decode path; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_displayed(
        &self,
        e2e_us: Option<u64>,
        display_us: Option<u64>,
        latch_us: Option<u64>,
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock (the callback already skipped the clock reads)
        }
        // Poison-proof for the same reason as `note_received`.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.presents += 1;
        if let Some(l) = e2e_us {
            g.e2e_disp_us.push(l);
        }
        if let Some(l) = display_us {
            g.display_us.push(l);
        }
        if let Some(l) = latch_us {
            g.latch_us.push(l);
        }
    }

    /// Record one released frame's pace-wait (decoded→release: the presenter's store + glass
    /// budget), µs — the `display` stage's other half. Decode-thread only.
    // Driven only by the android-only decode thread; unreferenced on the host build — expected.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_release(&self, pace_us: u64) {
        if !self.enabled.load(Ordering::Relaxed) {
            return; // HUD hidden — skip the lock
        }
        // Poison-proof for the same reason as `note_received`.
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.pace_us.push(pace_us);
    }

    /// Compute the window's rates + latency percentiles, then reset for the next window.
    /// `dropped_total` / `fec_total` are the connector's session-cumulative counters now; the
    /// snapshot's `lost` / `fec` are their deltas since the last drain (or the enabling show).
    pub fn drain(&self, dropped_total: u64, fec_total: u64) -> Snapshot {
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
        g.display_us.sort_unstable();
        g.e2e_disp_us.sort_unstable();
        g.pace_us.sort_unstable();
        g.latch_us.sort_unstable();
        let snap = Snapshot {
            fps,
            mbps,
            e2e_p50_ms: pctl_ms(&g.e2e_us, 0.50),
            e2e_p95_ms: pctl_ms(&g.e2e_us, 0.95),
            e2e_disp_p50_ms: pctl_ms(&g.e2e_disp_us, 0.50),
            e2e_disp_p95_ms: pctl_ms(&g.e2e_disp_us, 0.95),
            hostnet_p50_ms: pctl_ms(&g.hostnet_us, 0.50),
            decode_p50_ms: pctl_ms(&g.decode_us, 0.50),
            display_p50_ms: pctl_ms(&g.display_us, 0.50),
            disp_valid: !g.e2e_disp_us.is_empty(),
            pace_p50_ms: pctl_ms(&g.pace_us, 0.50),
            latch_p50_ms: pctl_ms(&g.latch_us, 0.50),
            presents: g.presents,
            host_p50_ms: pctl_ms(&g.host_us, 0.50),
            net_p50_ms: pctl_ms(&g.net_us, 0.50),
            lat_valid: !g.e2e_us.is_empty(),
            skew_corrected: g.skew_corrected,
            frames: g.frames,
            lost: dropped_total.saturating_sub(g.last_dropped_total),
            skipped: g.skipped,
            fec: fec_total.saturating_sub(g.last_fec_total),
        };
        g.window_start = Instant::now();
        g.frames = 0;
        g.bytes = 0;
        g.e2e_us.clear();
        g.hostnet_us.clear();
        g.host_us.clear();
        g.net_us.clear();
        g.decode_us.clear();
        g.display_us.clear();
        g.e2e_disp_us.clear();
        g.pace_us.clear();
        g.latch_us.clear();
        g.presents = 0;
        g.skipped = 0;
        g.last_dropped_total = dropped_total;
        g.last_fec_total = fec_total;
        snap
    }
}
