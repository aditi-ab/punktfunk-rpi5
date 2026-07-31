//! The timeline presenter — Android's port of the Apple client's stage-4 deadline discipline
//! (`clients/apple/.../Stage2Pipeline.swift`, the `.deadline` pacing):
//!
//!  * a **newest-wins slot** (or a small smoothing FIFO, by user intent) between decode and
//!    release, so a burst coalesces in the app — as an explicit, counted drop — instead of
//!    queueing behind the display;
//!  * a **glass budget of exactly one**: at most one undisplayed release in flight to
//!    SurfaceFlinger, reopened on the clock-predicted latch (with a 100 ms stale force-open as
//!    the liveness backstop, mirroring Apple's `PresentGate.staleAfter`). The BufferQueue can
//!    hold at most the frame being scanned out plus one — a standing queue is unconstructible;
//!  * a **timed release**: `AMediaCodec_releaseOutputBufferAtTime` targeting the platform's own
//!    frame timeline (API 33+, via [`super::vsync`]), so the latch phase is deterministic instead
//!    of inheriting network + decode jitter. On the 31/32 fallback the release is ASAP —
//!    identical to the legacy path — and only the budget prediction uses the measured period.
//!
//! The legacy behaviour (release the newest ready buffer immediately, unbudgeted) remains
//! selectable at runtime: `adb shell setprop debug.punktfunk.presenter arrival` — the on-device
//! A/B needs no rebuild. The user-facing escape hatch stays the "Low-latency mode" master toggle
//! (off = the synchronous pre-overhaul loop, no presenter at all).

use ndk::media::media_codec::MediaCodec;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use super::display::DisplayTracker;
use super::latency::now_realtime_ns;
use super::vsync::VsyncShared;

/// Submit-margin ahead of a timeline's EXPECTED PRESENT — SurfaceFlinger's own latch lead: the
/// released buffer must be in the BufferQueue by SF's wakeup for that vsync (a few ms before
/// present). A present closer than this is treated as missed and the next one is targeted. This
/// is deliberately NOT the timeline's `deadline` (which budgets for GPU rendering a video
/// buffer doesn't do — see `VsyncShared::next_target`); a too-tight gamble here presents one
/// vsync later, the exact cost the deadline gate paid on every frame.
///
/// 2.5 ms: SF's latch runs ~1-2 ms before present on modern devices (its `sfOffset`), and the
/// release itself is a binder call well under a ms. 4 ms measured latch p50 8-10; each ms cut
/// here is a ms off every frame's display stage. If a device misses at this margin the `paced`
/// counter shows it (a miss presents one vsync later, coalescing the next frame) — that is the
/// signal to widen, not stutter.
const LATCH_MARGIN_NS: i64 = 2_500_000;

/// The budget's liveness backstop: a release whose predicted latch never seems to arrive
/// (clock glitch, mode switch) force-reopens the budget this long after the release, counted in
/// `forced` — reads 0 on healthy systems (Apple's `PresentGate.staleAfter`, same value).
const STALE_REOPEN_NS: i64 = 100_000_000;

/// Fallback latch-prediction period while the vsync clock is unmeasured/absent: one 120 Hz frame.
const FALLBACK_PERIOD_NS: i64 = 8_333_333;

/// The user's presentation intent — the Apple client's `PresentPriority`, same resolution rules:
/// anything but an explicit "smooth" is latency; a smooth buffer outside 1..=3 becomes 2.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PresentPriority {
    /// Newest-wins, release the instant the budget opens. The default.
    Latency,
    /// A small FIFO (1..=3 frames) drained one per vsync: jitter absorbed at one refresh of
    /// added display latency per slot, which the metrics show rather than hide.
    Smooth { buffer: usize },
}

impl PresentPriority {
    /// From the JNI ints (`presentPriority` 0 = latency / 1 = smooth; `smoothBuffer` 0 = auto).
    pub(crate) fn resolve(priority: i32, buffer: i32) -> PresentPriority {
        if priority != 1 {
            return PresentPriority::Latency;
        }
        let b = if (1..=3).contains(&buffer) {
            buffer as usize
        } else {
            2
        };
        PresentPriority::Smooth { buffer: b }
    }
}

/// One decoded output buffer held for presentation.
struct HeldFrame {
    index: usize,
    pts_us: u64,
    /// The output callback's `CLOCK_REALTIME` stamp — the pace metric's start (decoded→release).
    decoded_ns: i128,
}

/// The one-in-flight glass budget.
struct InFlight {
    /// Monotonic instant the budget reopens: the release target's expected present (clock), or
    /// `release + period` on the fallback path.
    reopen_at_ns: i64,
    released_at_ns: i64,
}

/// Latch samples + display confirms recorded by the `OnFrameRendered` callback thread, drained by
/// the presenter's 1 Hz `pf-present` line. Always on (independent of the HUD) — this is what makes
/// a HUD-off wireless A/B readable from logcat.
pub(super) struct PresentMeter {
    inner: Mutex<PresentMeterInner>,
}

struct PresentMeterInner {
    latch_us: Vec<u64>,
    displays: u64,
}

impl PresentMeter {
    pub(super) fn new() -> PresentMeter {
        PresentMeter {
            inner: Mutex::new(PresentMeterInner {
                latch_us: Vec::with_capacity(256),
                displays: 0,
            }),
        }
    }

    /// One displayed frame's release→displayed latch, µs. Callback thread; poison-proof.
    pub(super) fn note_latch(&self, latch_us: Option<u64>) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.displays += 1;
        if let Some(l) = latch_us {
            if g.latch_us.len() < 4096 {
                g.latch_us.push(l);
            }
        }
    }

    fn drain(&self) -> (Vec<u64>, u64) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let displays = g.displays;
        g.displays = 0;
        (std::mem::take(&mut g.latch_us), displays)
    }
}

/// Circular (vector-mean) statistics of latch samples against the panel period: the mean latch
/// mod the period (ns) and the coherence (‰). The mean is what a phase controller can actually
/// steer under jitter — a median of a period-spanning distribution is immovable (the v1 lesson);
/// the coherence says whether ANY phase exists to steer (0 = uniformly smeared, 1000 = locked).
/// `None` under 8 samples — too little evidence to report a phase at all.
fn circular_latch(samples_us: &[u64], period_ns: i64) -> Option<(u64, u16)> {
    if samples_us.len() < 8 || period_ns <= 0 {
        return None;
    }
    let period_us = period_ns as f64 / 1000.0;
    let (mut x, mut y) = (0.0f64, 0.0f64);
    for &s in samples_us {
        let theta = (s as f64 % period_us) / period_us * std::f64::consts::TAU;
        x += theta.cos();
        y += theta.sin();
    }
    let n = samples_us.len() as f64;
    let r = (x * x + y * y).sqrt() / n;
    let mean_theta = y.atan2(x).rem_euclid(std::f64::consts::TAU);
    let mean_ns = (mean_theta / std::f64::consts::TAU * period_ns as f64) as u64;
    Some((mean_ns, (r * 1000.0) as u16))
}

/// p50/max of an unsorted µs sample vec, in ms. (0, 0) when empty.
fn p50_max_ms(mut v: Vec<u64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    let p50 = v[v.len() / 2] as f64 / 1000.0;
    let max = *v.last().unwrap() as f64 / 1000.0;
    (p50, max)
}

pub(super) struct Presenter {
    /// 0 = newest-wins; 1..=3 = smoothing FIFO capacity.
    fifo_capacity: usize,
    frames: VecDeque<HeldFrame>,
    /// FIFO preroll: `take` withholds until the buffer filled to capacity once, re-armed on a dry
    /// run — the Apple `FrameStore` semantics (headroom never builds without it).
    prerolled: bool,
    inflight: Option<InFlight>,
    /// A vsync arrived since the last release — the FIFO's one-per-refresh drain pace.
    vsync_tick: bool,
    // -- 1 Hz pf-present window, always on --
    released: u64,
    paced_drops: u64,
    no_budget: u64,
    forced: u64,
    dry: u64,
    pace_us: Vec<u64>,
    last_flush: Instant,
}

impl Presenter {
    pub(super) fn new(priority: PresentPriority) -> Presenter {
        Presenter {
            fifo_capacity: match priority {
                PresentPriority::Latency => 0,
                PresentPriority::Smooth { buffer } => buffer,
            },
            frames: VecDeque::new(),
            prerolled: false,
            inflight: None,
            vsync_tick: false,
            released: 0,
            paced_drops: 0,
            no_budget: 0,
            forced: 0,
            dry: 0,
            pace_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
        }
    }

    /// A vsync pulse from the clock thread's event — the retry tick for a parked frame and the
    /// FIFO's drain pace.
    pub(super) fn on_vsync(&mut self) {
        self.vsync_tick = true;
    }

    /// Accept one decoded, gate-approved output buffer. Newest-wins evicts everything older
    /// (released unrendered — the explicit, counted drop); the FIFO evicts its oldest past
    /// capacity. Returns how many frames were dropped by the policy (the HUD's `skipped`).
    pub(super) fn submit(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_ns: i128,
    ) -> u64 {
        let mut dropped = 0u64;
        if self.fifo_capacity == 0 {
            while let Some(stale) = self.frames.pop_front() {
                release_unrendered(codec, stale.index);
                dropped += 1;
            }
        }
        self.frames.push_back(HeldFrame {
            index,
            pts_us,
            decoded_ns,
        });
        if self.fifo_capacity > 0 && self.frames.len() > self.fifo_capacity {
            if let Some(stale) = self.frames.pop_front() {
                release_unrendered(codec, stale.index);
                dropped += 1;
            }
        }
        self.paced_drops += dropped;
        dropped
    }

    /// The present decision point — run on every loop pass (frame arrivals, vsync ticks, and the
    /// 5 ms housekeeping wake all land here). Releases AT MOST one frame (the budget). Returns
    /// `true` when a frame was released to glass this call.
    #[allow(clippy::too_many_arguments)] // one call site; the seams are the point
    pub(super) fn pump(
        &mut self,
        codec: &MediaCodec,
        clock: Option<&VsyncShared>,
        tracker: &DisplayTracker,
        stats: &crate::stats::VideoStats,
        now_mono_ns: i64,
    ) -> bool {
        // Budget bookkeeping first: reopen on the predicted latch, force-open on the backstop.
        if let Some(f) = &self.inflight {
            if now_mono_ns >= f.reopen_at_ns {
                self.inflight = None;
            } else if now_mono_ns - f.released_at_ns > STALE_REOPEN_NS {
                self.forced += 1;
                self.inflight = None;
            }
        }
        // Pick the frame this pump may release.
        let frame = if self.fifo_capacity == 0 {
            self.frames.pop_back() // submit() kept it a single slot; back == the newest
        } else {
            // FIFO: drain exactly one frame per vsync tick, after preroll; a drain tick that
            // finds the buffer dry re-arms preroll (the Apple `FrameStore` underflow semantics —
            // the previous frame persists on glass, a repeat by omission, while headroom
            // rebuilds). Everything is gated on the tick so an idle stream neither counts
            // underflows nor churns the preroll flag 200×/s.
            if !self.vsync_tick {
                return false;
            }
            if !self.prerolled {
                if self.frames.len() < self.fifo_capacity {
                    return false;
                }
                self.prerolled = true;
            }
            if self.frames.is_empty() {
                self.prerolled = false;
                self.dry += 1;
                self.vsync_tick = false; // this tick's drain ran (and found nothing)
                return false;
            }
            self.frames.pop_front()
        };
        let Some(frame) = frame else { return false };
        if self.inflight.is_some() {
            // Budget closed — park it back; a fresher submit replaces it (newest-wins), the next
            // vsync tick / loop pass retries the pairing.
            self.no_budget += 1;
            match self.fifo_capacity {
                0 => self.frames.push_back(frame),
                _ => self.frames.push_front(frame),
            }
            return false;
        }
        // Release: timeline-timed when the clock has one, ASAP otherwise.
        let target = clock.and_then(|c| c.next_target(now_mono_ns, LATCH_MARGIN_NS));
        let released = match target {
            Some(t) => codec
                .release_output_buffer_at_time_by_index(frame.index, t.expected_present_ns)
                .map_err(|e| log::warn!("presenter: release_at_time({}): {e}", frame.index)),
            None => codec
                .release_output_buffer_by_index(frame.index, true)
                .map_err(|e| log::warn!("presenter: release({}): {e}", frame.index)),
        };
        self.vsync_tick = false;
        if released.is_err() {
            return false; // the buffer is gone either way; nothing to book-keep
        }
        let period = clock.map(|c| c.period_ns()).filter(|&p| p > 0);
        // Reopen at SurfaceFlinger's LATCH for the targeted vsync (expected present minus the
        // latch lead) — the instant SF consumes the queued buffer and the slot frees, so the
        // next release can target the NEXT refresh. Not the platform `deadline` (with the
        // aggressive present gate it can already be in the past — an instant reopen would let
        // two releases pile onto the same vsync) and not the present time itself (a period too
        // late — it would cap the sustainable rate at roughly half the panel).
        let reopen_at_ns = target
            .map(|t| t.expected_present_ns - LATCH_MARGIN_NS)
            .unwrap_or(now_mono_ns + period.unwrap_or(FALLBACK_PERIOD_NS));
        self.inflight = Some(InFlight {
            reopen_at_ns,
            released_at_ns: now_mono_ns,
        });
        self.released += 1;
        let release_real_ns = now_realtime_ns();
        let pace_us = ((release_real_ns - frame.decoded_ns).max(0) / 1000) as u64;
        if self.pace_us.len() < 4096 {
            self.pace_us.push(pace_us);
        }
        stats.note_release(pace_us);
        tracker.note_rendered(frame.pts_us, frame.decoded_ns, release_real_ns);
        true
    }

    /// Release every held buffer unrendered — the teardown path, BEFORE `codec.stop()`.
    pub(super) fn release_all(&mut self, codec: &MediaCodec) {
        while let Some(f) = self.frames.pop_front() {
            release_unrendered(codec, f.index);
        }
        self.inflight = None;
    }

    /// The 1 Hz `pf-present` logcat mirror (target `pf.present`) — the Apple client's Console
    /// `pf-present` line, so a HUD-off on-device A/B is readable wirelessly:
    /// `released` (to glass) / `displays` (OnFrameRendered confirms) / `paced` (policy drops) /
    /// `noBudget` (waits on the closed budget) / `forced` (stale force-opens — 0 when healthy) /
    /// `qDry` (FIFO underflows) / `pace` (decoded→release) / `latch` (release→displayed) /
    /// `vsync` (the measured panel period).
    ///
    /// Returns this window's CIRCULAR latch statistics `(vector-mean latch ns mod panel period,
    /// coherence ‰)` when a window actually flushed — the phase-lock reporter's v2 error signal
    /// (design/phase-locked-capture.md §6; the v1 median was immovable under jitter).
    pub(super) fn flush_log(
        &mut self,
        meter: &PresentMeter,
        clock: Option<&VsyncShared>,
    ) -> Option<(u64, u16)> {
        if self.last_flush.elapsed() < std::time::Duration::from_secs(1) {
            return None;
        }
        self.last_flush = Instant::now();
        let (latch, displays) = meter.drain();
        if self.released == 0 && displays == 0 {
            return None; // idle stream — nothing worth a line
        }
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let circ =
            clock.and_then(|c| circular_latch(&latch, c.panel_period_ns().max(c.period_ns())));
        let (latch_p50, latch_max) = p50_max_ms(latch);
        let period_ms = clock.map(|c| c.period_ns() as f64 / 1e6).unwrap_or(0.0);
        let panel_ms = clock
            .map(|c| c.panel_period_ns() as f64 / 1e6)
            .unwrap_or(0.0);
        log::info!(
            target: "pf.present",
            "released={} displays={} paced={} noBudget={} forced={} qDry={} \
             paceMs p50={:.2} max={:.2} latchMs p50={:.2} max={:.2} circ={:.2}ms coh={} \
             vsyncMs={:.2} panelMs={:.2}",
            self.released,
            displays,
            self.paced_drops,
            self.no_budget,
            self.forced,
            self.dry,
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            circ.map(|(m, _)| m as f64 / 1e6).unwrap_or(0.0),
            circ.map(|(_, c)| c).unwrap_or(0),
            period_ms,
            panel_ms,
        );
        self.released = 0;
        self.paced_drops = 0;
        self.no_budget = 0;
        self.forced = 0;
        self.dry = 0;
        circ
    }
}

fn release_unrendered(codec: &MediaCodec, index: usize) {
    if let Err(e) = codec.release_output_buffer_by_index(index, false) {
        log::warn!("presenter: release_output_buffer({index}, false): {e}");
    }
}

/// `debug.punktfunk.presenter` sysprop: `arrival` = the legacy release-immediately path,
/// anything else / unset = the timeline presenter. The rebuild-free on-device A/B lever.
pub(super) fn presenter_disabled_by_sysprop() -> bool {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe {
        libc::__system_property_get(
            c"debug.punktfunk.presenter".as_ptr(),
            buf.as_mut_ptr().cast(),
        )
    };
    n > 0 && &buf[..n as usize] == b"arrival"
}
