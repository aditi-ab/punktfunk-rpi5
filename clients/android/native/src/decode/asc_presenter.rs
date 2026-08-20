//! The ASurfaceControl present backend (default): MediaCodec → `AImageReader` → `ASurfaceControl`
//! transactions, scheduled against the panel's real present clock.
//!
//! Where the SurfaceView presenter ([`super::presenter`]) predicts SurfaceFlinger's latch off a
//! choreographer grid that Android down-rates for a game uid, this backend gets the truth: every
//! applied transaction reports its real latch time and the previous buffer's release fence on
//! completion ([`super::surface_control::PresentComplete`]). Those two facts are the whole point —
//! the panel period is learned from real latch spacings (no down-rate lie), the glass budget is
//! bounded by real completions (no mispredicted reopen backpressuring the codec), and the latch
//! metric is always available (not the best-effort `OnFrameRendered` the SurfaceView path leans on).
//!
//! Both present intents ride the one actuator — a desired present time on the transaction:
//!   * **latency** (default `present_priority`): newest-wins. Each pump drains the reader to the
//!     newest image (`acquireLatestImageAsync` drops the rest back to the pool) and presents it at
//!     the next real vsync. Minimal depth.
//!   * **smooth**: a small FIFO drained on each frame's [`CadenceClock`] due time — the source's own
//!     cadence, recovered from the wire pts, finally with a truthful present clock beneath it.
//!
//! Memory safety does NOT rest on the release fences: an `AImage` (and the `AHardwareBuffer` it
//! wraps) stays alive through SurfaceFlinger's own reference taken by `setBuffer`, so deleting our
//! handle early at worst reuses a buffer a touch soon (a visible tear), never a use-after-free. The
//! fences are the correctness of *timing*, not of memory — which is what lets this ship behind an
//! auto-fallback with the residual risk being visual, not a crash.

use ndk::hardware_buffer::HardwareBuffer;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use punktfunk_core::phase::{CadenceClock, CadenceTuning, PanelGrid};
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use super::async_loop::DecodeEvent;
use super::latency::now_realtime_ns;
use super::presenter::PresentPriority;
use super::surface_control::{Layer, PresentComplete};
use super::vsync::now_monotonic_ns;

/// Reader pool depth. Must cover the codec's own in-flight outputs + the presenter's held candidate
/// / FIFO + the buffers still latched on SurfaceFlinger awaiting their release fence. Eight is
/// generous for a one-in-flight-ish presenter and small enough that no device balks.
const READER_MAX_IMAGES: i32 = 8;

/// SurfaceFlinger latch lead: a present targeted closer than this to a vsync is treated as missed
/// and the next grid point is used. Starts at 0 (the P2e on-glass finding — SF latched with no lead
/// on the NP3) and only ever grows if a device proves it needs more; kept simple here (fixed 0)
/// because the real-latch feedback makes the aggressive gamble self-correcting: a miss just presents
/// one vsync later, the same cost the predicted path always paid.
const LATCH_MARGIN_NS: i64 = 0;

/// Fallback panel period while none has been learned yet — one 120 Hz frame.
const FALLBACK_PERIOD_NS: i64 = 8_333_333;

/// One image acquired from the reader, held until it is presented (or dropped as a newest-wins
/// eviction). Carries the decode stamps paired by pts for the latency metrics.
struct Acquired {
    image: Image,
    buffer: HardwareBuffer,
    fence: Option<OwnedFd>,
    pts_us: u64,
    /// `CLOCK_REALTIME` decode-output stamp (for the skew-corrected end-to-end).
    decoded_real: i128,
    /// The source's due time on the cadence grid (`CLOCK_MONOTONIC`), `None` under latency.
    due_ns: Option<i64>,
}

/// One image applied to SurfaceFlinger, awaiting its completion (metrics) and its successor's
/// completion (the release fence that frees it back to the pool).
struct Presented {
    seq: u64,
    image: Image,
    pts_us: u64,
    decoded_real: i128,
    /// `CLOCK_REALTIME` / `CLOCK_MONOTONIC` instants the transaction was applied — the latch metric
    /// pairs the completion's monotonic latch against `release_mono`, and rebases it onto realtime
    /// via `release_real` for the skew-corrected end-to-end.
    release_real: i128,
    release_mono: i64,
}

/// The ASurfaceControl present backend.
pub(super) struct AscBackend {
    reader: ImageReader,
    /// Cached reader window handed to `MediaCodec::configure` as the decoder's output surface.
    reader_window: NativeWindow,
    layer: Layer,
    /// `None` under latency; the source-cadence loop under smooth.
    cadence: Option<CadenceClock>,
    /// FIFO capacity: 0 = newest-wins (latency); 1..=3 = the smoothing store depth.
    fifo_capacity: usize,
    /// The negotiated source frame interval — the cadence cushion ceiling.
    frame_interval_ns: i64,
    /// Transactions applied but not yet completed — the real glass budget.
    inflight: u32,
    /// The pipeline depth the budget allows (2 = double-buffer; a shade more under smooth).
    inflight_cap: u32,

    // -- held images --
    /// Latency: the newest acquired image not yet presented. Smooth leaves this `None`.
    candidate: Option<Acquired>,
    /// Smooth: images held for their due time, oldest first.
    fifo: VecDeque<Acquired>,
    /// Images on SurfaceFlinger, oldest first, awaiting release.
    presented: VecDeque<Presented>,

    // -- present clock --
    /// The panel period learned from real latch spacings — a READOUT for the pf.present line only.
    /// It must NOT drive the present target: the target produces the latch, so learning the period
    /// from the latch and then targeting it locks the panel to whatever it first latched.
    panel: PanelGrid,
    /// The honest panel period from the mode table (`panel_hz`) — what the smooth grid snaps to.
    /// Fixed for the session; the mode table is authoritative for the panel's fastest refresh.
    panel_seed_ns: i64,
    last_latch_ns: i64,
    /// `ADataSpace` for the transaction (BT709 for SDR — never untagged; see `color_dataspace`).
    dataspace: i32,
    /// Layer frame-rate vote (source Hz), applied once.
    frame_rate: f32,
    src_w: i32,
    src_h: i32,

    // -- bookkeeping --
    next_seq: u64,
    /// Decode stamps parked at `on_output`, keyed by the pts the codec echoes onto the buffer:
    /// `(pts_us, decoded_real_ns, decoded_mono_ns)`.
    stamps: VecDeque<(u64, i128, i64)>,

    // -- 1 Hz pf.present window --
    released: u64,
    skipped: u64,
    displays: u64,
    forced: u64,
    latch_us: Vec<u64>,
    pace_us: Vec<u64>,
    e2e_us: Vec<u64>,
    last_flush: Instant,
}

impl AscBackend {
    /// Create the reader + compositor layer, or `None` on API < 29 / init failure (the caller then
    /// runs the SurfaceView presenter). `window` is the SurfaceView's `ANativeWindow`; `src_w/h` the
    /// negotiated decode size; `surface_size` the LIVE view size the layer composites into;
    /// `panel_hz` the mode-table panel rate (seeds the learner);
    /// `dataspace` the `ADataSpace` from the negotiated colour; `source_hz` the negotiated stream rate.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create(
        window: &NativeWindow,
        src_w: i32,
        src_h: i32,
        surface_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
        panel_hz: i32,
        dataspace: i32,
        source_hz: u32,
        priority: PresentPriority,
    ) -> Option<AscBackend> {
        let layer = Layer::create(window, surface_size)?;
        let usage = ndk::hardware_buffer::HardwareBufferUsage::GPU_SAMPLED_IMAGE
            | ndk::hardware_buffer::HardwareBufferUsage::COMPOSER_OVERLAY;
        let reader = match ImageReader::new_with_usage(
            src_w.max(1),
            src_h.max(1),
            ImageFormat::PRIVATE,
            usage,
            READER_MAX_IMAGES,
        ) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("asc: ImageReader init failed ({e:?}) — falling back to SurfaceView");
                return None;
            }
        };
        let reader_window = match reader.window() {
            Ok(w) => w,
            Err(e) => {
                log::warn!("asc: ImageReader has no window ({e:?}) — falling back to SurfaceView");
                return None;
            }
        };
        let frame_interval_ns = match source_hz {
            0 => FALLBACK_PERIOD_NS,
            hz => 1_000_000_000 / i64::from(hz),
        };
        let (fifo_capacity, cadence, inflight_cap) = match priority {
            PresentPriority::Latency => (0usize, None, 2u32),
            PresentPriority::Smooth { buffer } => (
                buffer,
                Some(CadenceClock::new(CadenceTuning::snapping())),
                (buffer as u32 + 1).clamp(2, 4),
            ),
        };
        log::info!(
            "asc: backend up — {} ({}x{} @ {} Hz src, panel seed {} Hz, dataspace {:#x})",
            match priority {
                PresentPriority::Latency => "latency (newest-wins)".to_string(),
                PresentPriority::Smooth { buffer } => format!("smooth (buffer {buffer})"),
            },
            src_w,
            src_h,
            source_hz,
            panel_hz,
            dataspace,
        );
        Some(AscBackend {
            reader,
            reader_window,
            layer,
            cadence,
            fifo_capacity,
            frame_interval_ns,
            inflight: 0,
            inflight_cap,
            candidate: None,
            fifo: VecDeque::new(),
            presented: VecDeque::new(),
            panel: PanelGrid::seeded(panel_hz),
            panel_seed_ns: if panel_hz > 0 {
                1_000_000_000 / panel_hz as i64
            } else {
                FALLBACK_PERIOD_NS
            },
            last_latch_ns: 0,
            dataspace,
            frame_rate: if source_hz > 0 { source_hz as f32 } else { 0.0 },
            src_w: src_w.max(1),
            src_h: src_h.max(1),
            next_seq: 0,
            stamps: VecDeque::new(),
            released: 0,
            skipped: 0,
            displays: 0,
            forced: 0,
            latch_us: Vec::with_capacity(256),
            pace_us: Vec::with_capacity(256),
            e2e_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
        })
    }

    /// The decoder output surface (the reader's window) for `MediaCodec::configure`.
    pub(super) fn reader_window(&self) -> &NativeWindow {
        &self.reader_window
    }

    /// Re-anchor the cadence loop on the next frame — the discontinuity hook the decode loop calls
    /// when the re-anchor gate arms (a loss froze the picture and the decoder recovered behind it,
    /// so the source→presentable delay the loop measured no longer holds). No-op under latency.
    pub(super) fn reset_cadence(&mut self) {
        if let Some(c) = self.cadence.as_mut() {
            c.reset();
        }
    }

    /// Route one decoded output buffer: render it into the reader when `present` (the re-anchor
    /// gate approved it), else drop it off-glass. Parks the decode stamps for the pts the codec
    /// echoes onto the buffer so `pump` can pair the latency metrics after acquire.
    pub(super) fn on_output(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_real: i128,
        decoded_mono: i64,
        present: bool,
    ) {
        if present {
            self.stamps.push_back((pts_us, decoded_real, decoded_mono));
            if self.stamps.len() > 128 {
                self.stamps.pop_front();
            }
        }
        if let Err(e) = codec.release_output_buffer_by_index(index, present) {
            log::warn!("asc: release_output_buffer_by_index({index}, {present}): {e}");
        }
    }

    /// Pop the decode stamps for `pts_us`, evicting older entries (decode order == input order).
    fn take_stamp(&mut self, pts_us: u64) -> Option<(i128, i64)> {
        while let Some(&(p, real, mono)) = self.stamps.front() {
            if p > pts_us {
                break;
            }
            self.stamps.pop_front();
            if p == pts_us {
                return Some((real, mono));
            }
        }
        None
    }

    /// The desired present time for the frame being released, `CLOCK_MONOTONIC` (`0` = ASAP, only
    /// used to bootstrap the phase before the first latch is known).
    ///
    /// Both modes snap `not_before` up to an explicit panel-grid point: without one, applying two
    /// transactions close together lets SurfaceFlinger coalesce the pair onto a single vsync and
    /// idle the next — the on-glass 60-on-a-120-panel result of a plain ASAP present. Giving each
    /// frame its own grid-spaced present time makes SF present them on consecutive vsyncs.
    ///
    /// PERIOD is the mode-table seed (the honest panel maximum) — NEVER the latch-learned period,
    /// or a slow latch would ratchet the target down and hold the panel at the lower rate. PHASE is
    /// the last real latch. Latency passes `not_before = now + margin`; smooth additionally floors
    /// it at the source due time.
    fn next_present_target(&self, now_mono: i64, not_before: i64) -> i64 {
        let period = self.panel_seed_ns;
        if self.last_latch_ns <= 0 || period <= 0 {
            return 0; // bootstrap: no phase yet — present ASAP to establish the first latch
        }
        let floor = not_before.max(now_mono);
        let ahead = floor - self.last_latch_ns;
        let k = ahead.div_euclid(period) + 1;
        self.last_latch_ns + k.max(1) * period
    }

    /// Drain the reader into the held set (newest-wins candidate, or the smoothing FIFO), then
    /// present the due frame if the budget is open. Returns `true` when a frame was applied.
    pub(super) fn pump(
        &mut self,
        now_mono: i64,
        stats: &crate::stats::VideoStats,
        ev_tx: &mpsc::Sender<DecodeEvent>,
    ) -> bool {
        self.drain_reader();
        if self.inflight >= self.inflight_cap {
            return false;
        }
        // Pick the frame to present.
        let frame = if self.fifo_capacity == 0 {
            self.candidate.take()
        } else {
            let reach = now_mono + LATCH_MARGIN_NS + self.panel_seed_ns;
            match self.fifo.front() {
                Some(f) if f.due_ns.is_none_or(|due| due <= reach) => self.fifo.pop_front(),
                _ => return false,
            }
        };
        let Some(mut frame) = frame else {
            return false;
        };
        let not_before = frame.due_ns.map_or(now_mono + LATCH_MARGIN_NS, |d| {
            d.max(now_mono + LATCH_MARGIN_NS)
        });
        let target = self.next_present_target(now_mono, not_before);
        let seq = self.next_seq;
        let applied = self.layer.present(
            &frame.buffer,
            self.src_w,
            self.src_h,
            frame.fence.take(),
            target,
            self.dataspace,
            // The layer's fixed-source rate — applied once, at layer config (see `Layer::present`).
            self.frame_rate,
            seq,
            ev_tx,
        );
        if !applied {
            return false; // transaction failed; the image drops here, back to the pool
        }
        let release_real = now_realtime_ns();
        let pace_us = ((release_real - frame.decoded_real).max(0) / 1000) as u64;
        self.pace_us.push(pace_us);
        stats.note_release(pace_us);
        self.presented.push_back(Presented {
            seq,
            image: frame.image,
            pts_us: frame.pts_us,
            decoded_real: frame.decoded_real,
            release_real,
            release_mono: now_mono,
        });
        self.inflight += 1;
        self.next_seq += 1;
        self.released += 1;
        true
    }

    /// Acquire newly rendered images out of the reader: latency keeps only the newest (older are
    /// dropped back to the pool by `acquireLatest`); smooth keeps order up to capacity.
    fn drain_reader(&mut self) {
        if self.fifo_capacity == 0 {
            // Newest-wins: one acquire-latest collapses the whole burst to the freshest buffer.
            if let Some(acq) = self.acquire(true) {
                if self.candidate.replace(acq).is_some() {
                    self.skipped += 1; // an un-presented candidate was superseded
                }
            }
        } else {
            // Smooth: pull every ready image in order into the FIFO, evicting the oldest past cap.
            while let Some(acq) = self.acquire(false) {
                self.fifo.push_back(acq);
                while self.fifo.len() > self.fifo_capacity {
                    self.fifo.pop_front();
                    self.skipped += 1;
                }
            }
        }
    }

    /// Acquire one image (`latest` drops older, else FIFO) and pair its decode stamps + cadence due.
    /// `None` when the reader is empty or a transient acquire error occurs.
    fn acquire(&mut self, latest: bool) -> Option<Acquired> {
        // SAFETY: we never touch the image's pixels — the acquire fence is handed straight to
        // SurfaceFlinger via `setBuffer`, which is exactly the "await before access" the async
        // acquire requires.
        let res = unsafe {
            if latest {
                self.reader.acquire_latest_image_async()
            } else {
                self.reader.acquire_next_image_async()
            }
        };
        let (image, fence) = match res {
            Ok(AcquireResult::Image(pair)) => pair,
            Ok(_) => return None, // no buffer available / max acquired
            Err(e) => {
                log::warn!("asc: acquire image failed: {e:?}");
                return None;
            }
        };
        let buffer = match image.hardware_buffer() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("asc: image has no hardware buffer: {e:?}");
                return None; // `image` drops here → back to the pool
            }
        };
        // The buffer timestamp is the pts the codec echoed (ns); pair the parked decode stamps.
        let pts_ns = image.timestamp().unwrap_or(0).max(0);
        let pts_us = (pts_ns / 1000) as u64;
        let (decoded_real, decoded_mono) = self
            .take_stamp(pts_us)
            .unwrap_or((now_realtime_ns(), now_monotonic_ns()));
        let due_ns = self.cadence.as_mut().map(|c| {
            c.due_ns(
                pts_us.saturating_mul(1000),
                decoded_mono,
                self.frame_interval_ns,
            )
        });
        Some(Acquired {
            image,
            buffer,
            fence,
            pts_us,
            decoded_real,
            due_ns,
        })
    }

    /// A completed transaction: reopen the budget, learn the panel period from the real latch,
    /// record the latch + end-to-end, and free the buffer this frame replaced with its release
    /// fence. Runs on the decode thread (the callback only forwarded the data).
    pub(super) fn on_present_complete(
        &mut self,
        pc: PresentComplete,
        clock_offset: i64,
        stats: &crate::stats::VideoStats,
        video_e2e: &AtomicU64,
    ) {
        self.inflight = self.inflight.saturating_sub(1);
        // Metrics for the frame that just latched (its own `seq`).
        if pc.latch_ns > 0 {
            if let Some(p) = self.presented.iter().find(|p| p.seq == pc.seq) {
                let latch_ns = (pc.latch_ns - p.release_mono).clamp(0, 10_000_000_000);
                let displayed_real = p.release_real + latch_ns as i128;
                let e2e_ns = displayed_real + clock_offset as i128 - p.pts_us as i128 * 1000;
                let latch_use = (latch_ns / 1000) as u64;
                let display_use = ((displayed_real - p.decoded_real).max(0) / 1000) as u64;
                self.latch_us.push(latch_use);
                self.displays += 1;
                if e2e_ns > 0 && e2e_ns < 10_000_000_000 {
                    let e2e_use = (e2e_ns / 1000) as u64;
                    self.e2e_us.push(e2e_use);
                    // Publish glass-to-glass RAW for the audio plane to align against.
                    video_e2e.store(e2e_ns as u64, Ordering::Relaxed);
                    stats.note_displayed(Some(e2e_use), Some(display_use), Some(latch_use));
                } else {
                    stats.note_displayed(None, Some(display_use), Some(latch_use));
                }
            }
            // Learn the true panel period from consecutive real latches.
            if self.last_latch_ns > 0 {
                self.panel.observe(pc.latch_ns - self.last_latch_ns);
            }
            self.last_latch_ns = pc.latch_ns;
        }
        // Retire every buffer this transaction replaced (seq < completed): the immediate
        // predecessor gets the real release fence, any older straggler a plain delete (memory-safe
        // — SurfaceFlinger holds its own reference until it is actually done).
        let mut retired: Vec<Presented> = Vec::new();
        while self.presented.front().is_some_and(|p| p.seq < pc.seq) {
            retired.push(self.presented.pop_front().unwrap());
        }
        match (retired.pop(), pc.prev_release_fence) {
            (Some(last), Some(fence)) => last.image.delete_async(fence),
            (Some(last), None) => drop(last.image),
            (None, Some(fence)) => drop(fence),
            (None, None) => {}
        }
        // (`retired` now holds only older stragglers, dropped here — plain AImage_delete.)
        drop(retired);
    }

    /// Publish the reader-drop count to the HUD and emit the 1 Hz `pf.present` mirror line. Called
    /// once per loop pass; the `skipped` counter feeds the HUD each pass, the log line at 1 Hz.
    pub(super) fn flush(&mut self, stats: &crate::stats::VideoStats) {
        if self.skipped > 0 {
            stats.note_skipped(std::mem::take(&mut self.skipped));
        }
        if self.last_flush.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_flush = Instant::now();
        if self.released == 0 && self.displays == 0 {
            return; // idle
        }
        let (latch_p50, latch_max) = p50_max_ms(std::mem::take(&mut self.latch_us));
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (e2e_p50, e2e_max) = p50_max_ms(std::mem::take(&mut self.e2e_us));
        // Under the smoothness intent, tail the source-cadence loop's health: `late‰` of all frames
        // folded (a due time already past when the frame became presentable — the direct signal the
        // cushion is too small, WP8's acceptance criterion), `jitter` (the loop residual's mean
        // absolute deviation), `cushion`, and `reanchors`. Absent under latency (no loop). Counters
        // are cumulative since the last re-anchor, so `late` reads as a rate over enough frames.
        let cadence = self
            .cadence
            .as_ref()
            .map(CadenceClock::health)
            .map(|h| {
                format!(
                    " late={}‰ jitterMs={:.2} cushionMs={:.2} reanchors={}",
                    h.late.saturating_mul(1000) / h.frames.max(1),
                    h.jitter_ns as f64 / 1e6,
                    h.cushion_ns as f64 / 1e6,
                    h.reanchors,
                )
            })
            .unwrap_or_default();
        log::info!(
            target: "pf.present",
            "asc released={} displays={} inflight={} qDepth={} paceMs p50={:.2} max={:.2} \
             latchMs p50={:.2} max={:.2} e2eMs p50={:.2} max={:.2} panelMs={:.2} forced={}{}",
            self.released,
            self.displays,
            self.inflight,
            self.fifo.len(),
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            e2e_p50,
            e2e_max,
            self.panel.period_ns() as f64 / 1e6,
            self.forced,
            cadence,
        );
        self.released = 0;
        self.displays = 0;
    }

    /// Teardown: drop every held image (candidate, FIFO, and still-presented) back to the pool
    /// before the reader + codec go away. Plain deletes — SurfaceFlinger releases its own refs as
    /// it finishes, so this is memory-safe without waiting on the fences.
    pub(super) fn release_all(&mut self) {
        self.candidate = None;
        self.fifo.clear();
        self.presented.clear();
    }
}

impl AscBackend {
    /// Update the `ADataSpace` applied to every subsequent transaction (a refinement from the
    /// codec's output format — the analogue of the SurfaceView path's `apply_hdr_dataspace`; the
    /// negotiated colour set the initial value at create).
    pub(super) fn set_dataspace(&mut self, dataspace: i32) {
        if self.dataspace != dataspace {
            self.dataspace = dataspace;
            log::info!("asc: buffer dataspace now {dataspace:#x}");
        }
    }
}

/// Whether the ASurfaceControl backend is selected. Default ON; `debug.punktfunk.present_backend =
/// surfaceview` forces the legacy SurfaceView presenter (the field escape hatch, no rebuild). Any
/// other value — or an ASC init failure downstream — still lands on ASC-then-fallback.
pub(super) fn asc_backend_selected() -> bool {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe {
        libc::__system_property_get(
            c"debug.punktfunk.present_backend".as_ptr(),
            buf.as_mut_ptr().cast(),
        )
    };
    !(n > 0 && &buf[..n as usize] == b"surfaceview")
}

/// p50/max of an unsorted µs sample vec, in ms. (0, 0) when empty.
fn p50_max_ms(mut v: Vec<u64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    (
        v[v.len() / 2] as f64 / 1000.0,
        *v.last().unwrap() as f64 / 1000.0,
    )
}
