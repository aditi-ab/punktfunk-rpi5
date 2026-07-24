//! The pre-decode FIFO video hand-off (`FrameChannel`) + jump-to-live tuning consts + `DecodeLatAcc`.

use crate::session::Frame;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Depth at/above which the pre-decode hand-off queue counts as "not draining" for the clock-free
/// standing-queue detector. A consumer that keeps up (or drains newest-per-vsync, like the Apple
/// client) holds this near 0; a transient Wi-Fi clump or a small jitter buffer spikes it briefly then
/// drains. Sits above a reasonable jitter buffer (~100 ms @ 60 fps) so only a genuine backlog trips it.
pub(crate) const QUEUE_HIGH: usize = 6;

/// Depth at/below which the hand-off queue is considered drained — resets the standing-queue counter.
/// A true standing queue never falls back to this; a clump does within a few frames.
pub(crate) const QUEUE_LOW: usize = 2;

/// How long the hand-off queue must sit ≥ [`QUEUE_HIGH`] (never dropping to [`QUEUE_LOW`], and
/// still ≥ high at the trip) before the pump declares a standing backlog and jumps to live.
/// WALL-CLOCK (latency plan T1.4) — the old 30-FRAME count made the accepted backlog scale with
/// fps (500 ms @60 but 125 ms @240) and stretched further under the frame-driven host's slower
/// static-scene repeat cadence. 250 ms sits comfortably above a Wi-Fi clump's drain time (a
/// clump empties in a few frames, ≤ ~100 ms) at any rate.
pub(crate) const STANDING_TIME: Duration = Duration::from_millis(250);

/// Memory backstop on the pre-decode hand-off queue. The standing-queue detector jumps to live long
/// before this (typically ≤ QUEUE_HIGH + a [`STANDING_TIME`] run deep), and a jump already requested a
/// keyframe, so on the rare path that outruns it (a wedged consumer during the flush cooldown) dropping
/// the OLDEST queued AU is safe — the pending IDR re-anchors decode regardless. Purely bounds memory.
const FRAME_QUEUE_HARD_CAP: usize = 90;

/// Backlog latency bound: when completed frames keep arriving further than this behind the host's
/// capture clock (skew-corrected), the pump jumps to live (discards the receive backlog + the queued
/// AUs and requests a keyframe) instead of playing that far behind forever. Deliberately generous — an
/// interactive stream is unusable well before 400 ms, but the bound must sit safely above the skew
/// handshake's own error (≈ RTT/2) plus normal delivery jitter so a healthy stream can never trip it.
/// This is the CLOCK-BASED detector; the clock-free [`QUEUE_HIGH`]/[`STANDING_TIME`] detector covers
/// same-clock and no-handshake sessions (where `clock_offset_ns == 0` disarms this one).
pub(crate) const FLUSH_LATENCY: Duration = Duration::from_millis(400);

/// How long frames must run CONTINUOUSLY over the [`FLUSH_LATENCY`] bound before the clock-based
/// jump fires. WALL-CLOCK (latency plan T1.4, was a 30-frame count — same fps-scaling problem as
/// the standing-queue detector). A genuine standing queue puts EVERY frame over the bound; a
/// one-off burst (an IDR, a Wi-Fi scan blip) clears within a frame or two and resets the run.
pub(crate) const FLUSH_AFTER: Duration = Duration::from_millis(250);

/// Minimum spacing between jump-to-live events, so a bottleneck that instantly rebuilds the queue (a
/// link/consumer that can't sustain the bitrate at all) degrades into a periodic skip + a logged
/// warning instead of a continuous flush/keyframe storm.
pub(crate) const FLUSH_COOLDOWN: Duration = Duration::from_secs(2);

/// A clock-triggered jump-to-live that discarded fewer datagrams than this (and no queued AUs)
/// found NO local backlog: the frames read as late, but nothing here was actually behind. Two
/// causes, and flushing helps neither: a **wall-clock step** (NTP mid-session on either end)
/// shifted the skew-corrected latency by a constant — every future frame reads over-bound and the
/// detector would fire forever, one flush + recovery IDR per cooldown, dragging the bitrate
/// controller to its floor; or the delay is standing in an **upstream queue** (router bufferbloat),
/// which a local flush can't drain — the OWD signal already feeds the bitrate controller, the
/// actual remedy. Even at the 5 Mbps bitrate floor a genuine 400 ms backlog is ~170 datagrams, so
/// 64 cleanly separates "empty" from "real". See `NOOP_CLOCK_FLUSHES_TO_DISARM`.
pub(crate) const NOOP_FLUSH_DATAGRAMS: u64 = 64;

/// Consecutive no-op clock-triggered flushes (see [`NOOP_FLUSH_DATAGRAMS`]) before the clock-based
/// detector is disarmed. The clock-free standing-queue detector stays armed — it measures the
/// local queue directly and can't be fooled by a clock step. No longer for the rest of the
/// session: an applied mid-stream clock re-sync re-arms the detector (the disarm stays as the
/// final backstop between re-syncs).
pub(crate) const NOOP_CLOCK_FLUSHES_TO_DISARM: u32 = 2;

/// Cadence of the control task's periodic mid-stream clock re-sync (see [`ClockResync`]): often
/// enough to bound slow drift and pick up an NTP step within a minute, rare enough to be free
/// (8 tiny control messages per batch). The pump additionally fires one immediately after the
/// FIRST no-op clock flush — the moment a step is actually suspected.
pub(crate) const CLOCK_RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Standing-latency bleed (the 2026-07 two-pair investigation): how far above the session's own
/// one-way-delay floor a report window's MINIMUM must sit to count as a standing elevation. The
/// jump-to-live detectors above deliberately ignore anything below ~6 frames / 400 ms, so a
/// small standing state — a sub-frame kernel/reassembly backlog, or a stale clock offset after a
/// wall-clock step — is carried forever and reads as permanent extra "network" latency. 10 ms
/// sits above skew-handshake error + normal LAN jitter, and below a single 60 fps frame period,
/// so the observed one-frame plateau (~17 ms) trips it while a healthy stream cannot.
pub(crate) const STANDING_LAT_THRESH_NS: i128 = 10_000_000;

/// Consecutive elevated report windows (~750 ms each) before the bleed escalates — ~4.5 s of a
/// continuously standing, loss-free elevation. Windows with any loss reset the run: loss means
/// genuine congestion, which the FEC/ABR machinery owns, not this detector.
pub(crate) const STANDING_LAT_WINDOWS: u32 = 6;

/// Per-session cap on flush+keyframe bleeds. A standing state that survives a clock re-sync AND
/// this many local flushes is not local and not clock — the path latency itself changed; the
/// detector disarms with a warning instead of paying a recovery keyframe every few seconds.
pub(crate) const STANDING_LAT_MAX_BLEEDS: u32 = 3;

/// What the standing-latency detector asks the pump to do this window (see [`StandingLatency`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StandingLatAction {
    None,
    /// First escalation: ask for a mid-stream clock re-sync — free, and a stale offset from a
    /// stepped/slewed wall clock produces exactly this signature (an applied re-sync re-bases
    /// the floor via the pump's `clock_gen` watch, clearing the elevation if that was the cause).
    Resync {
        above_ms: i64,
    },
    /// The elevation survived a re-sync attempt: flush the local receive backlog + request a
    /// keyframe (the jump-to-live action), draining a real sub-threshold standing queue. The
    /// pump reports execution back via [`StandingLatency::bled`]; an unexecuted action simply
    /// re-arms next window.
    Bleed {
        above_ms: i64,
    },
    /// Bleed cap reached and the elevation is back: give up and say so.
    Disarm {
        above_ms: i64,
    },
}

/// Detector for a small, constant, loss-free one-way-delay elevation — the standing state the
/// jump-to-live thresholds deliberately tolerate. Tracks the session's OWD floor (minimum of
/// report-window minimums since start / last re-base) and escalates when windows sit
/// persistently above it: re-sync first, then a bounded number of flush+keyframe bleeds, then
/// disarm. Pure state machine (no clocks, no I/O) so the escalation ladder is unit-testable.
pub(crate) struct StandingLatency {
    /// Lowest window-minimum OWD seen since session start / last [`rebase`](Self::rebase).
    floor_ns: Option<i128>,
    /// Minimum per-frame OWD this report window; `None` = no frames yet.
    window_min_ns: Option<i128>,
    /// Consecutive elevated windows.
    run: u32,
    /// The current elevation already got its re-sync request — next escalation is a bleed.
    resync_tried: bool,
    bleeds: u32,
    disarmed: bool,
}

impl StandingLatency {
    pub(crate) fn new() -> Self {
        StandingLatency {
            floor_ns: None,
            window_min_ns: None,
            run: 0,
            resync_tried: false,
            bleeds: 0,
            disarmed: false,
        }
    }

    /// Feed one frame's skew-corrected OWD (capture→reassembly-complete, ns). Caller gates on a
    /// live clock offset and plausibility (0 < owd < 10 s), like the ABR OWD signal.
    pub(crate) fn note_frame(&mut self, owd_ns: i128) {
        self.window_min_ns = Some(match self.window_min_ns {
            Some(m) => m.min(owd_ns),
            None => owd_ns,
        });
    }

    /// Close a report window. `loss_free` = the window carried zero loss (loss resets the run —
    /// congestion is the FEC/ABR machinery's problem, and queues under loss are not "standing").
    pub(crate) fn on_window(&mut self, loss_free: bool) -> StandingLatAction {
        let Some(wmin) = self.window_min_ns.take() else {
            return StandingLatAction::None; // no frames this window — no evidence either way
        };
        let floor = *self.floor_ns.get_or_insert(wmin);
        self.floor_ns = Some(floor.min(wmin));
        let above_ns = wmin - floor;
        if self.disarmed {
            return StandingLatAction::None;
        }
        if !loss_free || above_ns < STANDING_LAT_THRESH_NS {
            self.run = 0;
            if above_ns < STANDING_LAT_THRESH_NS {
                self.resync_tried = false; // elevation cleared — a future one re-syncs first again
            }
            return StandingLatAction::None;
        }
        self.run += 1;
        if self.run < STANDING_LAT_WINDOWS {
            return StandingLatAction::None;
        }
        self.run = 0; // each escalation gets a fresh observation run
        let above_ms = (above_ns / 1_000_000) as i64;
        if !self.resync_tried {
            self.resync_tried = true;
            StandingLatAction::Resync { above_ms }
        } else if self.bleeds < STANDING_LAT_MAX_BLEEDS {
            StandingLatAction::Bleed { above_ms }
        } else {
            self.disarmed = true;
            StandingLatAction::Disarm { above_ms }
        }
    }

    /// The pump executed a [`StandingLatAction::Bleed`] (flush + keyframe). The floor is KEPT: a
    /// successful bleed brings OWD back down to it (elevation clears naturally); an unsuccessful
    /// one leaves the elevation visible so the ladder continues toward the cap.
    pub(crate) fn bled(&mut self) {
        self.bleeds += 1;
        self.window_min_ns = None;
    }

    /// A mid-stream clock re-sync was APPLIED (the pump's `clock_gen` watch): every OWD reading
    /// shifted, so the floor and any elevation measured under the old offset are meaningless —
    /// re-learn from scratch. The bleed budget survives (it caps keyframes per session).
    pub(crate) fn rebase(&mut self) {
        self.floor_ns = None;
        self.window_min_ns = None;
        self.run = 0;
        self.resync_tried = false;
    }
}

/// Client decode-stage latency accumulator for the adaptive-bitrate controller's decode signal.
/// The embedder adds one sample per decoded frame ([`NativeClient::report_decode_us`], µs from the
/// AU leaving [`NativeClient::next_frame`] to its decoded output) and the data-plane pump drains a
/// window mean once per report window to feed [`crate::abr::BitrateController::on_window`]. This is
/// the only signal that sees the CLIENT'S decoder: on a fast LAN a mobile HW decoder saturates long
/// before the link, backlogging frames inside the decoder where loss/OWD never register. Sum+count
/// (not a running mean) so the pump takes an unweighted window mean and resets. Always accumulated —
/// the controller ignores it when Automatic is off, and the pump drains it every window regardless,
/// so it stays bounded (a full window at 240 fps is ~180 samples).
#[derive(Default)]
pub(crate) struct DecodeLatAcc {
    pub(crate) sum_us: u64,
    pub(crate) count: u32,
}

/// Host encode-stage latency accumulator — [`DecodeLatAcc`]'s mirror for the HOST side of the
/// pipeline. The datagram task adds one sample per 0xCF `HostStages::encode_us` (host encoder
/// submit → bitstream ready) and the pump drains a window mean into
/// [`crate::abr::BitrateController::on_window`]'s encode signal. Host encode time was measured,
/// shipped and drawn on the overlay, but never an ABR input — which is how a fat-LAN Automatic
/// session drove the encoder past its compute knee with nothing to stop it (§ABR overdrive).
/// Its own accumulator rather than the overlay's `host_timing` channel: that channel is a lossy
/// `try_send` the embedder may never drain, and the controller must not depend on it.
#[derive(Default)]
pub(crate) struct EncodeLatAcc {
    pub(crate) sum_us: u64,
    pub(crate) count: u32,
}

/// The pre-decode video hand-off from the data-plane pump to the embedder. Unlike the side planes
/// (self-contained samples that drop the newest on overflow), video AUs are reference-chained under the
/// host's infinite GOP: dropping ANY frame mid-stream corrupts every dependent frame until the next
/// IDR. So this queue is strictly FIFO and never drops a frame from the middle. When the embedder falls
/// PERSISTENTLY behind — the queue stops draining — the pump JUMPS TO LIVE instead ([`clear`] + a
/// keyframe request), so decode resumes cleanly at an IDR rather than ratcheting latency forever (the
/// old bounded channel silently dropped the NEWEST AU on overflow — backwards for a live stream, and a
/// reference-chain break the loss counters never saw). A transient burst fills it briefly and drains on
/// its own, so a clump never costs a keyframe.
///
/// [`clear`]: FrameChannel::clear
pub(crate) struct FrameChannel {
    inner: Mutex<FrameQueue>,
    ready: Condvar,
}

struct FrameQueue {
    q: VecDeque<Frame>,
    /// Set when the pump exits so a blocked [`FrameChannel::pop`] reports the stream ended
    /// ([`PunktfunkError::Closed`]) rather than a spurious timeout (the old mpsc did this on sender drop).
    closed: bool,
}

/// Outcome of [`FrameChannel::pop`] — mirrors the old `recv_timeout` results so `next_frame`'s
/// Timeout/Closed mapping is unchanged.
pub(crate) enum FramePop {
    Frame(Frame),
    Timeout,
    Closed,
}

impl FrameChannel {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(FrameQueue {
                q: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Pump side: append a completed AU and wake a blocked consumer. Enforces the memory backstop
    /// ([`FRAME_QUEUE_HARD_CAP`]) by dropping the oldest (see its doc — a jump-to-live keyframe is
    /// already in flight by the time this can bite).
    pub(crate) fn push(&self, frame: Frame) {
        let mut st = self.inner.lock().unwrap();
        st.q.push_back(frame);
        while st.q.len() > FRAME_QUEUE_HARD_CAP {
            st.q.pop_front();
        }
        drop(st);
        self.ready.notify_one();
    }

    /// Pump side: current queued depth — the clock-free standing-queue signal.
    pub(crate) fn depth(&self) -> usize {
        self.inner.lock().unwrap().q.len()
    }

    /// Pump side: discard the whole backlog (the jump-to-live path); returns how many were dropped.
    pub(crate) fn clear(&self) -> usize {
        let mut st = self.inner.lock().unwrap();
        let n = st.q.len();
        st.q.clear();
        n
    }

    /// Pump side: mark the stream ended and wake every blocked consumer.
    pub(crate) fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    /// Consumer side: pop the oldest AU, waiting up to `timeout` for one to arrive.
    pub(crate) fn pop(&self, timeout: Duration) -> FramePop {
        let mut st = self.inner.lock().unwrap();
        if st.q.is_empty() && !st.closed {
            st = self.ready.wait_timeout(st, timeout).unwrap().0;
        }
        if let Some(f) = st.q.pop_front() {
            FramePop::Frame(f)
        } else if st.closed {
            FramePop::Closed
        } else {
            FramePop::Timeout
        }
    }
}

#[cfg(test)]
mod frame_channel_tests {
    use super::{FrameChannel, FramePop, FRAME_QUEUE_HARD_CAP};
    use crate::session::Frame;
    use std::time::Duration;

    fn frame(i: u32) -> Frame {
        Frame {
            data: vec![i as u8],
            frame_index: i,
            pts_ns: i as u64,
            flags: 0,
            complete: true,
            received_ns: 0,
        }
    }

    fn popped(ch: &FrameChannel) -> Option<u32> {
        match ch.pop(Duration::from_millis(0)) {
            FramePop::Frame(f) => Some(f.frame_index),
            _ => None,
        }
    }

    #[test]
    fn fifo_order_and_depth() {
        let ch = FrameChannel::new();
        assert_eq!(ch.depth(), 0);
        ch.push(frame(1));
        ch.push(frame(2));
        assert_eq!(ch.depth(), 2);
        assert_eq!(popped(&ch), Some(1)); // oldest first (never newest-wins pre-decode)
        assert_eq!(popped(&ch), Some(2));
        assert_eq!(ch.depth(), 0);
    }

    #[test]
    fn empty_pop_times_out_not_closed() {
        let ch = FrameChannel::new();
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn clear_drops_backlog_and_reports_count() {
        let ch = FrameChannel::new();
        for i in 0..5 {
            ch.push(frame(i));
        }
        assert_eq!(ch.clear(), 5); // the jump-to-live discard returns what it dropped
        assert_eq!(ch.depth(), 0);
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn close_after_drain_reports_closed() {
        let ch = FrameChannel::new();
        ch.push(frame(7));
        ch.close();
        // Queued frames still drain BEFORE the Closed signal.
        assert_eq!(popped(&ch), Some(7));
        assert!(matches!(ch.pop(Duration::from_millis(1)), FramePop::Closed));
    }

    #[test]
    fn hard_cap_drops_oldest() {
        let ch = FrameChannel::new();
        let total = FRAME_QUEUE_HARD_CAP as u32 + 10;
        for i in 0..total {
            ch.push(frame(i));
        }
        // Capped at the backstop; the OLDEST were dropped, so the newest survive in order.
        assert_eq!(ch.depth(), FRAME_QUEUE_HARD_CAP);
        assert_eq!(popped(&ch), Some(total - FRAME_QUEUE_HARD_CAP as u32));
    }
}

#[cfg(test)]
mod standing_latency_tests {
    use super::{
        StandingLatAction, StandingLatency, STANDING_LAT_MAX_BLEEDS, STANDING_LAT_THRESH_NS,
        STANDING_LAT_WINDOWS,
    };

    const FLOOR: i128 = 2_000_000; // a healthy 2 ms LAN OWD
    const ELEVATED: i128 = FLOOR + STANDING_LAT_THRESH_NS + 7_000_000; // ~one 60fps frame above

    /// Run `n` windows at `owd`, asserting every window but the last returns None; returns the
    /// last window's action.
    fn run_windows(d: &mut StandingLatency, owd: i128, n: u32) -> StandingLatAction {
        for i in 0..n {
            d.note_frame(owd);
            let a = d.on_window(true);
            if i + 1 < n {
                assert_eq!(a, StandingLatAction::None, "window {i} escalated early");
            } else {
                return a;
            }
        }
        unreachable!("n > 0 by construction");
    }

    /// Learn a clean floor: one window at the healthy OWD.
    fn learned(d: &mut StandingLatency) {
        d.note_frame(FLOOR);
        assert_eq!(d.on_window(true), StandingLatAction::None);
    }

    #[test]
    fn healthy_stream_never_escalates() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        // Jitter riding above the floor but under the threshold: never a run.
        for _ in 0..(STANDING_LAT_WINDOWS * 4) {
            d.note_frame(FLOOR + STANDING_LAT_THRESH_NS - 1);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
    }

    #[test]
    fn escalation_ladder_resync_then_bleeds_then_disarm() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        // First full elevated run asks for the free fix: a clock re-sync.
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        // Re-sync didn't help (no rebase came) — each further run is a bleed, up to the cap...
        for _ in 0..STANDING_LAT_MAX_BLEEDS {
            assert!(matches!(
                run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
                StandingLatAction::Bleed { .. }
            ));
            d.bled();
        }
        // ...then the detector gives up loudly, once, and stays quiet.
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Disarm { .. }
        ));
        d.note_frame(ELEVATED);
        assert_eq!(d.on_window(true), StandingLatAction::None);
    }

    #[test]
    fn loss_windows_reset_the_run() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        for _ in 0..(STANDING_LAT_WINDOWS - 1) {
            d.note_frame(ELEVATED);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
        // A lossy window means congestion, not a standing state: run resets...
        d.note_frame(ELEVATED);
        assert_eq!(d.on_window(false), StandingLatAction::None);
        // ...so the ladder needs the full run again before acting.
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
    }

    #[test]
    fn recovery_resets_the_ladder_to_resync_first() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        // The elevation clears on its own (e.g. the successful bleed case, or transient): the
        // next episode starts back at the free escalation, not at a bleed.
        d.note_frame(FLOOR);
        assert_eq!(d.on_window(true), StandingLatAction::None);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
    }

    #[test]
    fn applied_resync_rebases_and_clears_a_stale_offset_elevation() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        // The re-sync APPLIES (pump sees clock_gen move) → rebase. The corrected offset brings
        // OWD readings back to truth; the floor re-learns and nothing ever escalates to a bleed.
        d.rebase();
        for _ in 0..(STANDING_LAT_WINDOWS * 2) {
            d.note_frame(FLOOR);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
    }

    #[test]
    fn empty_windows_are_no_evidence() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        for _ in 0..(STANDING_LAT_WINDOWS - 1) {
            d.note_frame(ELEVATED);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
        // A frameless window (paused stream) neither advances nor resets the run...
        assert_eq!(d.on_window(true), StandingLatAction::None);
        // ...so one more elevated window completes it.
        d.note_frame(ELEVATED);
        assert!(matches!(
            d.on_window(true),
            StandingLatAction::Resync { .. }
        ));
    }
}
