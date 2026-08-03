//! The timeline presenter — Android's port of the Apple client's stage-4 deadline discipline
//! (`clients/apple/.../Stage2Pipeline.swift`, the `.deadline` pacing):
//!
//!  * a **newest-wins slot** (or a small smoothing FIFO, by user intent) between decode and
//!    release, so a burst coalesces in the app — as an explicit, counted drop — instead of
//!    queueing behind the display;
//!  * a **glass budget of one**: at most one undisplayed release in flight to SurfaceFlinger,
//!    reopened on the clock-predicted latch (with a 100 ms stale force-open as the liveness
//!    backstop, mirroring Apple's `PresentGate.staleAfter`), and bounded underneath by what
//!    `OnFrameRendered` actually confirmed reached glass ([`UNDISPLAYED_CAP`]) — because the
//!    prediction is only as good as the panel grid behind it, and 0.23.0 shipped a grid that
//!    could be wrong in one direction forever;
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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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
/// here is a ms off every frame's display stage. A device that misses at the live margin shows it
/// as a measured latch beyond one panel period (see the adaptation in
/// [`Presenter::flush_log`]) — that, not a drop counter, is the signal to widen.
const LATCH_MARGIN_NS: i64 = 2_500_000;

/// `debug.punktfunk.latch_margin_us` (0..=8000 µs): PIN the submit margin for a sweep —
/// setprop + stream restart, no rebuild. Unset/invalid = `None` = the adaptive default.
fn latch_margin_ns() -> Option<i64> {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe {
        libc::__system_property_get(
            c"debug.punktfunk.latch_margin_us".as_ptr(),
            buf.as_mut_ptr().cast(),
        )
    };
    if n > 0 {
        if let Ok(us) = std::str::from_utf8(&buf[..n as usize])
            .unwrap_or("")
            .trim()
            .parse::<i64>()
        {
            if (0..=8_000).contains(&us) {
                return Some(us * 1_000);
            }
        }
    }
    None
}

/// The budget's liveness backstop: a release whose predicted latch never seems to arrive
/// (clock glitch, mode switch) force-reopens the budget this long after the release, counted in
/// `forced` — reads 0 on healthy systems (Apple's `PresentGate.staleAfter`, same value).
const STALE_REOPEN_NS: i64 = 100_000_000;

/// Releases still unconfirmed by `OnFrameRendered` at which the presenter stops handing
/// SurfaceFlinger more work.
///
/// The reopen above is a PREDICTION off the learned panel grid. A grid finer than the panel
/// (0.23.0 could pin one permanently — see [`punktfunk_core::phase::PanelGrid`]) reopens the
/// budget before the display has consumed anything, and the presenter then releases faster than
/// the panel scans: the BufferQueue fills, MediaCodec runs out of output buffers, the decoder
/// stalls, and the no-output backstop starts begging for keyframes. The render callback is the
/// ground truth about what actually reached glass, so it bounds the prediction.
///
/// Six, not one: the platform is explicitly allowed to deliver these callbacks BATCHED, and this
/// module's own `RENDERED_CAP` note records them trailing a release by a vsync or two — so a
/// healthy device sits at 1-3 outstanding and a tight cap would throttle it for nothing (a held
/// frame in the newest-wins slot is a DROPPED frame the moment a fresher one decodes). This is
/// not a pacing knob; it is the "something is structurally wrong" rail, and a presenter genuinely
/// out-running its display climbs past any fixed cap within a second. If a device's BufferQueue
/// is shallower than this the rail simply never engages and the no-output backstop handles it,
/// exactly as before — best-effort, never worse than not having it.
const UNDISPLAYED_CAP: i32 = 6;

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
    /// Frames released to SurfaceFlinger that `OnFrameRendered` has not yet confirmed reached
    /// glass. The presenter's structural rail (see [`UNDISPLAYED_CAP`]) and the pf-present line's
    /// queue-depth readout. Lock-free because the release side runs on the decode loop and the
    /// confirm side on the codec's callback thread, once per frame each.
    undisplayed: AtomicI32,
    /// This device delivers render callbacks at all (API ≥ 33 and the platform accepted the
    /// registration). Until one arrives, `undisplayed` is meaningless and the rail stays down.
    confirms: AtomicBool,
}

struct PresentMeterInner {
    latch_us: Vec<u64>,
    displays: u64,
    /// The `decode` stage's feed split, received→queued µs (P3 science: hand-off + input-slot
    /// wait). Empty when no receipt stamp matched (HUD off and ABR not measuring decode).
    feed_us: Vec<u64>,
    /// The codec-pure half, queued→decoded µs, measured from the AU's LAST piece — always on,
    /// so a HUD-off logcat A/B still reads the decoder's own time.
    codec_us: Vec<u64>,
    /// Capture→decoded end-to-end µs (skew-corrected, clamped) — always on for the same reason:
    /// the wireless A/B's headline without having to reach the on-screen HUD.
    e2e_us: Vec<u64>,
}

impl PresentMeter {
    pub(super) fn new() -> PresentMeter {
        PresentMeter {
            inner: Mutex::new(PresentMeterInner {
                latch_us: Vec::with_capacity(256),
                displays: 0,
                feed_us: Vec::with_capacity(256),
                codec_us: Vec::with_capacity(256),
                e2e_us: Vec::with_capacity(256),
            }),
            undisplayed: AtomicI32::new(0),
            confirms: AtomicBool::new(false),
        }
    }

    /// One displayed frame's release→displayed latch, µs. Callback thread; poison-proof.
    ///
    /// Also the glass budget's CONFIRM: this frame left the BufferQueue, so one outstanding
    /// release is settled. Clamped at zero — the legacy `arrival` path renders without going
    /// through [`Presenter::pump`], so confirms can outnumber counted releases.
    pub(super) fn note_latch(&self, latch_us: Option<u64>) {
        self.confirms.store(true, Ordering::Relaxed);
        let _ = self
            .undisplayed
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some((v - 1).max(0))
            });
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

    /// One frame handed to SurfaceFlinger, awaiting its confirm. Decode thread.
    fn note_released(&self) {
        self.undisplayed.fetch_add(1, Ordering::Relaxed);
    }

    /// Releases still unconfirmed, and whether confirms happen on this device at all.
    fn outstanding(&self) -> (i32, bool) {
        (
            self.undisplayed.load(Ordering::Relaxed),
            self.confirms.load(Ordering::Relaxed),
        )
    }

    /// Write off the outstanding releases: the platform stopped confirming (it is allowed to
    /// drop callbacks under load) or SurfaceFlinger discarded the buffers without presenting
    /// them. Never stall the stream on a ledger we cannot audit.
    fn forgive_outstanding(&self) {
        self.undisplayed.store(0, Ordering::Relaxed);
    }

    /// One decoded frame's always-on measurements: the `decode`-stage split (feed =
    /// received→queued when a receipt stamp matched; codec = queued→decoded when the queued
    /// stamp did) and the capture→decoded end-to-end, µs. Decode thread; poison-proof.
    pub(super) fn note_decode(
        &self,
        feed_us: Option<u64>,
        codec_us: Option<u64>,
        e2e_us: Option<u64>,
    ) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(f) = feed_us {
            if g.feed_us.len() < 4096 {
                g.feed_us.push(f);
            }
        }
        if let Some(c) = codec_us {
            if g.codec_us.len() < 4096 {
                g.codec_us.push(c);
            }
        }
        if let Some(e) = e2e_us {
            if g.e2e_us.len() < 4096 {
                g.e2e_us.push(e);
            }
        }
    }

    #[allow(clippy::type_complexity)] // one caller unpacks it in place; a struct would be noise
    fn drain(&self) -> (Vec<u64>, u64, Vec<u64>, Vec<u64>, Vec<u64>) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let displays = g.displays;
        g.displays = 0;
        (
            std::mem::take(&mut g.latch_us),
            displays,
            std::mem::take(&mut g.feed_us),
            std::mem::take(&mut g.codec_us),
            std::mem::take(&mut g.e2e_us),
        )
    }
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
    /// Pump passes that held a frame back because too many earlier releases were still
    /// unconfirmed ([`UNDISPLAYED_CAP`]) — reads 0 on a healthy device, and a climbing value is
    /// the signature of a presenter out-running its display.
    queue_waits: u64,
    /// When the unconfirmed-release rail first engaged, so it can be forgiven if the confirms
    /// simply stopped coming. `None` while the rail is down.
    backed_up_since: Option<i64>,
    pace_us: Vec<u64>,
    last_flush: Instant,
    /// The live submit margin. Starts at 0 (P2e on-glass: SurfaceFlinger latched every
    /// release with NO lead on the NP3 — the fixed 2.5 ms was pure display latency) and
    /// widens by measurement: `paced` misses in a 1 Hz window push it +500 µs toward
    /// [`LATCH_MARGIN_NS`], the pre-sweep known-safe ceiling. A sysprop override PINS it.
    margin_ns: i64,
    /// `debug.punktfunk.latch_margin_us` was set — the margin is pinned, never adapted.
    margin_pinned: bool,
}

impl Presenter {
    pub(super) fn new(priority: PresentPriority) -> Presenter {
        let pinned = latch_margin_ns();
        let (margin_ns, margin_pinned) = match pinned {
            Some(ns) => (ns, true),
            None => (0, false),
        };
        log::info!(
            "presenter: latch margin {}us{}",
            margin_ns / 1_000,
            if margin_pinned {
                " (debug.punktfunk.latch_margin_us pin)"
            } else {
                " (adaptive — widens on latch misses)"
            }
        );
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
            queue_waits: 0,
            backed_up_since: None,
            pace_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
            margin_ns,
            margin_pinned,
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
        meter: &PresentMeter,
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
        // The measured rail beneath that prediction (see `UNDISPLAYED_CAP`). Evaluated on every
        // pass — frame waiting or not — so its forgiveness timer measures real elapsed time
        // rather than how often a frame happened to be ready.
        let backlogged = self.unconfirmed_backlog(meter, now_mono_ns);
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
        if self.inflight.is_some() || backlogged {
            // Budget closed — park it back; a fresher submit replaces it (newest-wins), the next
            // vsync tick / loop pass retries the pairing.
            if backlogged {
                self.queue_waits += 1;
            }
            self.no_budget += 1;
            match self.fifo_capacity {
                0 => self.frames.push_back(frame),
                _ => self.frames.push_front(frame),
            }
            return false;
        }
        // Release: timeline-timed when the clock has one, ASAP otherwise.
        let target = clock.and_then(|c| c.next_target(now_mono_ns, self.margin_ns));
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
            .map(|t| t.expected_present_ns - self.margin_ns)
            .unwrap_or(now_mono_ns + period.unwrap_or(FALLBACK_PERIOD_NS));
        self.inflight = Some(InFlight {
            reopen_at_ns,
            released_at_ns: now_mono_ns,
        });
        self.released += 1;
        meter.note_released();
        let release_real_ns = now_realtime_ns();
        let pace_us = ((release_real_ns - frame.decoded_ns).max(0) / 1000) as u64;
        if self.pace_us.len() < 4096 {
            self.pace_us.push(pace_us);
        }
        stats.note_release(pace_us);
        tracker.note_rendered(frame.pts_us, frame.decoded_ns, release_real_ns);
        true
    }

    /// Whether SurfaceFlinger is sitting on too many unconfirmed releases to be handed another.
    ///
    /// The predicted reopen is only as good as the panel grid behind it; this is the measured
    /// rail underneath it (see [`UNDISPLAYED_CAP`]). It self-clears two ways — the confirms catch
    /// up, or [`STALE_REOPEN_NS`] passes with the backlog stuck, which means the ledger itself is
    /// unreliable (callbacks dropped under load, or SF discarded the buffers) and is written off
    /// rather than allowed to wedge the stream.
    fn unconfirmed_backlog(&mut self, meter: &PresentMeter, now_ns: i64) -> bool {
        let (outstanding, confirms_live) = meter.outstanding();
        if !confirms_live || outstanding < UNDISPLAYED_CAP {
            self.backed_up_since = None;
            return false;
        }
        match self.backed_up_since {
            Some(t) if now_ns - t > STALE_REOPEN_NS => {
                meter.forgive_outstanding();
                self.backed_up_since = None;
                self.forced += 1;
                false
            }
            _ => {
                self.backed_up_since.get_or_insert(now_ns);
                true
            }
        }
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
    /// `qDry` (FIFO underflows) / `qWait` (pumps held back by unconfirmed releases — 0 when
    /// healthy) / `unconfirmed` (releases OnFrameRendered hasn't settled) /
    /// `pace` (decoded→release) / `latch` (release→displayed) /
    /// `feed`+`codec` (the decode stage split: received→queued hand-off/slot wait + the
    /// codec-pure queued→decoded time) / `e2e` (capture→decoded, skew-corrected — the wireless
    /// A/B headline) / `vsync` (the measured panel period).
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
        let (latch, displays, feed, codec, e2e) = meter.drain();
        if self.released == 0 && displays == 0 {
            return None; // idle stream — nothing worth a line
        }
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (feed_p50, feed_max) = p50_max_ms(feed);
        let (codec_p50, codec_max) = p50_max_ms(codec);
        let (e2e_p50, e2e_max) = p50_max_ms(e2e);
        let circ = clock.and_then(|c| {
            punktfunk_core::phase::circular_latch(&latch, c.panel_period_ns().max(c.period_ns()))
        });
        let latch_samples = latch.len();
        let (latch_p50, latch_max) = p50_max_ms(latch);
        let period_ms = clock.map(|c| c.period_ns() as f64 / 1e6).unwrap_or(0.0);
        let panel_ns = clock.map(|c| c.panel_period_ns()).unwrap_or(0);
        let (outstanding, _) = meter.outstanding();
        log::info!(
            target: "pf.present",
            "released={} displays={} paced={} noBudget={} forced={} qDry={} \
             qWait={} unconfirmed={} \
             paceMs p50={:.2} max={:.2} latchMs p50={:.2} max={:.2} \
             feedMs p50={:.2} max={:.2} codecMs p50={:.2} max={:.2} \
             e2eMs p50={:.2} max={:.2} circ={:.2}ms coh={} \
             vsyncMs={:.2} panelMs={:.2}",
            self.released,
            displays,
            self.paced_drops,
            self.no_budget,
            self.forced,
            self.dry,
            self.queue_waits,
            outstanding,
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            feed_p50,
            feed_max,
            codec_p50,
            codec_max,
            e2e_p50,
            e2e_max,
            circ.map(|(m, _)| m as f64 / 1e6).unwrap_or(0.0),
            circ.map(|(_, c)| c).unwrap_or(0),
            period_ms,
            panel_ns as f64 / 1e6,
        );
        self.released = 0;
        // Margin adaptation, off the MEASURED latch. A release targets the first grid point past
        // `now + margin`, so a frame that makes its vsync is on glass within one panel period of
        // that margin; beyond it, SurfaceFlinger wanted more lead and the frame waited out an
        // extra refresh. Widen toward the pre-sweep ceiling. One-way by design: a margin that
        // once proved necessary is never re-gambled mid-stream (the next stream restarts at 0).
        //
        // ⚠ NOT `paced_drops`, which 0.23.0 used: those are the newest-wins store's own policy
        // evictions — a second frame decoding while one is held — which happen whenever the
        // stream out-runs the panel and say nothing at all about SF's latch lead. Driving the
        // margin from them widened it to the ceiling on healthy devices, re-imposing the 2.5 ms
        // of pure display latency the P2e sweep had just measured away.
        let latch_p50_ns = (latch_p50 * 1e6) as i64;
        if !self.margin_pinned
            && self.margin_ns < LATCH_MARGIN_NS
            && panel_ns > 0
            && latch_samples >= 8
            && latch_p50_ns > panel_ns + self.margin_ns
        {
            self.margin_ns = (self.margin_ns + 500_000).min(LATCH_MARGIN_NS);
            log::warn!(
                "presenter: latch p50 {:.2}ms over the {:.2}ms panel period — margin widened to {}us",
                latch_p50,
                panel_ns as f64 / 1e6,
                self.margin_ns / 1_000
            );
        }
        if self.queue_waits > 0 {
            log::warn!(
                "presenter: {} pump(s) held back — {} release(s) still unconfirmed by \
                 OnFrameRendered (the display is not keeping up with the release rate)",
                self.queue_waits,
                outstanding
            );
        }
        self.paced_drops = 0;
        self.no_budget = 0;
        self.forced = 0;
        self.dry = 0;
        self.queue_waits = 0;
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
