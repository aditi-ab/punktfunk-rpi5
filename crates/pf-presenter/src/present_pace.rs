//! The presentation intent engine (design/desktop-presentation-rebuild.md WP2): the
//! store, clock, and gate the run loop composes into the two intents.
//!
//! * [`FrameStore`] — newest-wins slot (latency) or smoothing FIFO with preroll
//!   (smoothness), ported from the Apple `FrameStore` / Android `presenter.rs` so all
//!   three clients agree on what the intents mean.
//! * [`LatchClock`] — the panel latch grid, learned from `VK_KHR_present_wait` on-glass
//!   stamps (measured, never queried — the Android refresh-rate lie and VRR both punish
//!   trusting a reported rate). Without present-wait it degrades to a grid rooted at the
//!   last submit on the mode's refresh period.
//! * [`PresentGate`] — the FIFO glass budget: one undisplayed present in flight, so the
//!   swapchain's own queue can never become a standing queue (+1 refresh per slot,
//!   forever — the law every bounded-FIFO pacing rediscovered on Apple). MAILBOX cannot
//!   queue and never needs it.
//! * [`SourcePacer`] — the shared [`punktfunk_core::phase::CadenceClock`] bound to this
//!   client: under the smoothness intent frames are played out on the SOURCE's cadence
//!   instead of on their arrival instant, so a raggedly-delivering host stops landing its
//!   jitter on the glass 1:1.
//!
//! Everything here is pure state + arithmetic on `CLOCK_REALTIME` ns (the
//! `pf_client_core::session::now_ns` domain the on-glass stamps live in) —
//! `DecodedFrame::decoded_ns` included, which is what lets the cadence clock run with no
//! domain conversion anywhere in this path. The run loop owns all clocks and Vulkan calls,
//! which is what keeps this testable.

use punktfunk_core::phase::{CadenceClock, CadenceHealth, CadenceTuning};
use std::collections::VecDeque;

/// Stale-present force-open: an undisplayed present older than this is presumed lost
/// (occluded window, wedged compositor) and the gate opens anyway, counted as `forced`
/// — reads 0 on healthy systems. The Apple/Android presenters use the same 100 ms.
const STALE_REOPEN_NS: u64 = 100_000_000;

/// The adaptive slot-pick margin's ceiling and step (Android's measured values: start
/// at 0 — a fixed lead was pure display tax on the reference device — and widen only
/// when measured misses demand it).
pub(crate) const MARGIN_STEP_NS: u64 = 500_000;
pub(crate) const MARGIN_MAX_NS: u64 = 2_500_000;

/// The decoded-frame store between the wake channel and the present call.
///
/// `capacity == 0` = newest-wins (latency intent): `submit` replaces, `take` clears.
/// `capacity 1..=3` = smoothing FIFO: preroll-to-capacity, drop-oldest on overflow,
/// an underflow after preroll re-arms the preroll (the previous frame persists on
/// glass — a repeat by omission) while headroom rebuilds.
pub(crate) struct FrameStore<T> {
    capacity: usize,
    frames: VecDeque<T>,
    prerolled: bool,
    /// Newest-wins displacements (normal operation under latency, not a fault signal).
    replaced: u32,
    /// FIFO drop-oldest evictions — the Apple debug line's `qDrop`.
    overflow_drops: u32,
    /// FIFO dry-after-preroll events — `qDry`.
    underflows: u32,
}

impl<T> FrameStore<T> {
    pub(crate) fn new(capacity: usize) -> FrameStore<T> {
        FrameStore {
            capacity,
            frames: VecDeque::with_capacity(capacity.max(1) + 1),
            prerolled: false,
            replaced: 0,
            overflow_drops: 0,
            underflows: 0,
        }
    }

    pub(crate) fn is_smoothing(&self) -> bool {
        self.capacity > 0
    }

    pub(crate) fn submit(&mut self, f: T) {
        if self.capacity == 0 {
            if self.frames.pop_front().is_some() {
                self.replaced += 1;
            }
            self.frames.push_back(f);
        } else {
            self.frames.push_back(f);
            // Drop the OLDEST past capacity: bounded added latency, the newest keeps
            // flowing. Also trims a transient capacity+1 a put_back left behind.
            while self.frames.len() > self.capacity {
                self.frames.pop_front();
                self.overflow_drops += 1;
            }
        }
    }

    /// The frame this pass will present, if any.
    ///
    /// `due` answers "has this frame's due time arrived?" for the front of a smoothing
    /// FIFO. Newest-wins never asks it — under the latency intent a frame is due the
    /// instant it exists — which is what keeps the cadence clock out of that path
    /// entirely.
    pub(crate) fn take(&mut self, due: impl FnOnce(&T) -> bool) -> Option<T> {
        if self.capacity == 0 {
            return self.frames.pop_front();
        }
        if !self.prerolled {
            // Preroll gate: without it a steady stream drains every frame on arrival
            // and jitter headroom never builds (the Apple store's lesson).
            if self.frames.len() < self.capacity {
                return None;
            }
            self.prerolled = true;
        }
        let Some(f) = self.frames.front() else {
            self.underflows += 1;
            self.prerolled = false;
            return None;
        };
        // Not yet due is the smoothing intent WORKING: the store has a frame and is
        // holding it for its slot. No counter moves and the preroll stands — reading this
        // as a dry buffer would re-arm the preroll on every well-paced frame.
        if !due(f) {
            return None;
        }
        self.frames.pop_front()
    }

    /// The frame `take` would consider next, without consuming it — the run loop sizes its
    /// event-wait from that frame's due time.
    pub(crate) fn front(&self) -> Option<&T> {
        self.frames.front()
    }

    /// A frame taken but not presented (gate closed, present failed before consuming
    /// it). Newest-wins reinserts only into an empty slot — a fresher decode wins;
    /// FIFO puts it back at the front (it is the oldest).
    pub(crate) fn put_back(&mut self, f: T) {
        if self.capacity == 0 {
            if self.frames.is_empty() {
                self.frames.push_back(f);
            }
        } else {
            self.frames.push_front(f);
        }
    }

    /// Collapse to newest-wins for the rest of the stream (PyroWave: its plane-ring
    /// retirement accounting assumes the depth-2 newest-wins hand-off, and its all-intra
    /// frames make buffering pointless anyway).
    ///
    /// Gated with its only caller: the power-user build (`--no-default-features`, which
    /// the Windows ARM64 leg ships) has no PyroWave decode path, and an ungated helper
    /// is dead code there.
    #[cfg(feature = "pyrowave")]
    pub(crate) fn force_latency(&mut self) {
        if self.capacity == 0 {
            return;
        }
        self.capacity = 0;
        self.prerolled = false;
        while self.frames.len() > 1 {
            self.frames.pop_front();
        }
    }

    /// Drain the window's counters: `(replaced, overflow_drops, underflows)`.
    pub(crate) fn take_counters(&mut self) -> (u32, u32, u32) {
        let c = (self.replaced, self.overflow_drops, self.underflows);
        self.replaced = 0;
        self.overflow_drops = 0;
        self.underflows = 0;
        c
    }
}

/// The panel latch grid: a recent on-glass instant + the latch period, extrapolated
/// forward for slot targeting.
///
/// The period learner is the SHARED [`punktfunk_core::phase::PanelGrid`], not a local
/// rule. An earlier version of this clock capped the learned period at the display
/// mode's refresh, on the reasoning that a stream running below panel rate spaces its
/// presents at k×period and the cap stops a 30 fps stream claiming a 30 Hz panel. That
/// cap is the same defect the Android presenter shipped in 0.23.0: the seed is only what
/// the *mode* claims, and when the real panel is slower (a refused mode switch, a
/// compositor running its own rate) a downward-only learner pins a grid that never
/// arrives, for the whole session, with no way back. `PanelGrid` moves both ways —
/// narrowing at once, widening only after eight consecutive agreeing observations and
/// then to the narrowest of them.
///
/// What is fed to it is still the window's MIN spacing: within one window that resists
/// the k×period inflation the old cap was aimed at, while the streak requirement means a
/// genuinely slower panel is still discovered. Same grid the host-facing `LatchGrid`
/// publish reads, so the phase-lock report and the local scheduler cannot disagree.
pub(crate) struct LatchClock {
    anchor_ns: u64,
    /// The previous stamp, kept ACROSS calls. The run loop drains present-wait samples
    /// every pass, so a "batch" is very often a single stamp — computing spacings only
    /// within a batch (`windows(2)`) observed nothing at all on glass, and the learner
    /// silently ran on its seed forever.
    last_ns: u64,
    /// Narrowest spacing seen since the last handoff to the grid, and how many have
    /// accumulated. The grid is fed the MIN of a run rather than every spacing: our
    /// observations are the spacing of OUR presents, which is k×period whenever the
    /// stream runs below panel rate, and the min over a run is the best available
    /// estimate of the true grid step.
    pending_min_ns: u64,
    pending_count: u32,
    grid: punktfunk_core::phase::PanelGrid,
    fallback_period_ns: u64,
}

/// Spacings per handoff to [`punktfunk_core::phase::PanelGrid`]. Small enough that a real
/// mode change is picked up in well under a second at any sane frame rate.
const GRID_OBSERVE_EVERY: u32 = 16;

impl LatchClock {
    pub(crate) fn new(refresh_hz: u32) -> LatchClock {
        LatchClock {
            anchor_ns: 0,
            last_ns: 0,
            pending_min_ns: 0,
            pending_count: 0,
            grid: punktfunk_core::phase::PanelGrid::seeded(refresh_hz as i32),
            fallback_period_ns: 1_000_000_000 / u64::from(refresh_hz.max(1)),
        }
    }

    /// Fold on-glass stamps (ascending). Spacings are measured against the previous
    /// stamp whatever the batching, so the loop's one-sample-per-pass drain still feeds
    /// the learner.
    pub(crate) fn note_batch(&mut self, stamps: &[u64]) {
        for &s in stamps {
            if self.last_ns != 0 && s > self.last_ns {
                let d = s - self.last_ns;
                // < 1 ms apart = a queued pair, not a grid step.
                if d > 1_000_000 {
                    self.pending_min_ns = if self.pending_min_ns == 0 {
                        d
                    } else {
                        self.pending_min_ns.min(d)
                    };
                    self.pending_count += 1;
                    if self.pending_count >= GRID_OBSERVE_EVERY {
                        self.grid.observe(self.pending_min_ns as i64);
                        self.pending_min_ns = 0;
                        self.pending_count = 0;
                    }
                }
            }
            self.last_ns = s;
        }
        if let Some(&last) = stamps.last() {
            self.anchor_ns = last;
        }
    }

    pub(crate) fn period_ns(&self) -> u64 {
        let learned = self.grid.period_ns();
        if learned > 0 {
            learned as u64
        } else {
            self.fallback_period_ns
        }
    }

    pub(crate) fn anchor_ns(&self) -> u64 {
        self.anchor_ns
    }

    /// The first predicted latch strictly after `after_ns` (`anchor + k·period`). With
    /// no anchor yet: one period out — callers get a usable, if unanchored, deadline.
    pub(crate) fn next_slot_after(&self, after_ns: u64) -> u64 {
        let p = self.period_ns();
        if self.anchor_ns == 0 || after_ns < self.anchor_ns {
            return after_ns.saturating_add(p);
        }
        let k = (after_ns - self.anchor_ns) / p + 1;
        self.anchor_ns + k * p
    }
}

/// Whether the panel is refreshing on a fixed grid or following our cadence.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Cadence {
    /// Not enough evidence yet — say nothing rather than guess.
    #[default]
    Unknown,
    /// On-glass instants land on multiples of the panel period: a fixed-refresh panel.
    Fixed,
    /// On-glass instants track our present spacing instead: variable refresh is live.
    Variable,
}

impl Cadence {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Cadence::Unknown => "",
            Cadence::Fixed => "no",
            Cadence::Variable => "yes",
        }
    }
}

/// Is variable refresh actually live? **Measured, never queried** — no portable query
/// exists (SDL exposes none, Wayland does not report adaptive-sync state, and Windows
/// surfaces nothing through Vulkan), and the platforms that *do* answer have been caught
/// lying before (Android reports a game-uid's down-rated refresh as the panel's).
///
/// The discriminator is quantization. On a fixed-refresh panel every on-glass instant
/// lands on the vblank grid, so the spacing between consecutive presents is always
/// ~k×period for whole k — even when the stream runs slower than the panel, where it just
/// picks a larger k. Under real VRR the panel refreshes *when we present*, so the spacing
/// follows our own cadence and sits wherever it likes relative to the grid.
///
/// So: fold each delta to its distance from the nearest multiple of the period. Tight
/// against the grid ⇒ Fixed; consistently off it ⇒ Variable. A stream running exactly at
/// panel rate is indistinguishable either way (both give delta ≈ period), which is
/// harmless — at that rate VRR has nothing to do.
pub(crate) struct CadenceProbe {
    /// Off-grid distances as a fraction of the period, in thousandths.
    off_grid_milli: Vec<u32>,
    /// Previous stamp, kept across calls for the same reason [`LatchClock`] does: the
    /// live drain hands over one sample at a time.
    last_ns: u64,
    /// The last round's raw reading and how many rounds have agreed — a verdict is only
    /// published once [`CADENCE_STABLE_ROUNDS`] agree.
    candidate: Cadence,
    agree_rounds: u8,
    verdict: Cadence,
}

/// Enough deltas to distinguish jitter from a real off-grid cadence.
const CADENCE_MIN_SAMPLES: usize = 24;
/// Consecutive agreeing rounds before a verdict is published.
///
/// ⭐ On glass (GNOME/Wayland, .21, 2026-08-02) the raw per-round verdict FLAPPED between
/// runs with VRR provably disabled. The cause is structural, not a tuning miss: under a
/// compositor our on-glass stamp is the compositor's release, so anything that perturbs
/// delivery — an occluded or unfocused surface being throttled, a distressed pipeline
/// missing vblanks — smears the spacings exactly the way real VRR does. This probe can
/// therefore only ever say "presents are not landing on the grid", so it demands
/// agreement across rounds and refuses evidence from a distressed window (see
/// [`CadenceProbe::note`]'s `healthy` flag) before claiming anything.
const CADENCE_STABLE_ROUNDS: u8 = 2;
/// Median off-grid distance under this fraction of a period reads as grid-locked. Present
/// stamps carry real measurement jitter (the wait returns, then we read the clock), so
/// this is deliberately loose — the two regimes differ by far more than this in practice.
const CADENCE_FIXED_MILLI: u32 = 150;

impl CadenceProbe {
    pub(crate) fn new() -> CadenceProbe {
        CadenceProbe {
            off_grid_milli: Vec::with_capacity(64),
            last_ns: 0,
            candidate: Cadence::Unknown,
            agree_rounds: 0,
            verdict: Cadence::Unknown,
        }
    }

    /// Fold on-glass stamps against the learned panel period. Spacings are measured
    /// against the previous stamp whatever the batching.
    ///
    /// `healthy` is the caller's statement that this window's presents were flowing
    /// normally (no stale force-opens). A distressed pipeline smears spacings for reasons
    /// that have nothing to do with the panel, so its evidence is dropped — the timeline
    /// continuity is still advanced, it simply does not count as a sample.
    pub(crate) fn note(&mut self, stamps: &[u64], period_ns: u64, healthy: bool) {
        if period_ns == 0 || !healthy {
            self.last_ns = stamps.last().copied().unwrap_or(self.last_ns);
            return;
        }
        for &s in stamps {
            let prev = std::mem::replace(&mut self.last_ns, s);
            if prev == 0 || s <= prev {
                continue;
            }
            let delta = s - prev;
            let rem = delta % period_ns;
            // Distance to the NEAREST multiple, so a delta just under k×period reads as
            // close to the grid rather than a whole period away from k-1.
            let off = rem.min(period_ns - rem);
            self.off_grid_milli
                .push((off.saturating_mul(1000) / period_ns) as u32);
            // A round closes on the SAMPLE count, inside the loop — not once per call.
            // Evaluating per call would make the verdict depend on how the caller happens
            // to batch its stamps (one big batch = one round, forever short of the
            // agreement requirement), and the live drain and the tests batch differently.
            self.close_round_if_ready();
        }
    }

    /// Publish a verdict once a round's worth of spacings agree with the previous round.
    fn close_round_if_ready(&mut self) {
        if self.off_grid_milli.len() >= CADENCE_MIN_SAMPLES {
            self.off_grid_milli.sort_unstable();
            let median = self.off_grid_milli[self.off_grid_milli.len() / 2];
            let round = if median <= CADENCE_FIXED_MILLI {
                Cadence::Fixed
            } else {
                Cadence::Variable
            };
            if round == self.candidate {
                self.agree_rounds = self.agree_rounds.saturating_add(1);
            } else {
                self.candidate = round;
                self.agree_rounds = 1;
            }
            if self.agree_rounds >= CADENCE_STABLE_ROUNDS {
                self.verdict = round;
            }
            self.off_grid_milli.clear();
        }
    }

    pub(crate) fn verdict(&self) -> Cadence {
        self.verdict
    }

    /// A mode switch / display change invalidates the evidence.
    pub(crate) fn reset(&mut self) {
        self.off_grid_milli.clear();
        self.last_ns = 0;
        self.candidate = Cadence::Unknown;
        self.agree_rounds = 0;
        self.verdict = Cadence::Unknown;
    }
}

/// Plays frames out on the SOURCE's cadence: the shared
/// [`CadenceClock`](punktfunk_core::phase::CadenceClock) plus the two policy calls that
/// belong to this client rather than to the loop — which intent it applies to, and which
/// cushion the panel's measured refresh behaviour asks for.
///
/// The defect it exists for is a host that delivers raggedly. The 2026-08-15 Skynet trace
/// has KWin's screencast arriving 0.11-8.22 ms off its own grid — up to a full 120 Hz
/// period — for 24 minutes, with the bitrate pinned and zero packet loss; presented on
/// arrival, every one of those milliseconds lands on the glass.
///
/// The invariant to hold onto when touching this: the loop smooths the OFFSET, never the
/// timestamps. Genuine variation in the source's own cadence passes straight through to the
/// due time, so anything that made the due times more evenly spaced than the source would be
/// a bug and not an improvement (design/presenter-cadence-rework-implementation-plan.md
/// §2.2).
pub(crate) struct SourcePacer {
    clock: CadenceClock,
    /// Running the free-running tuning — i.e. the last verdict [`follow`](Self::follow)
    /// saw was [`Cadence::Variable`].
    free_running: bool,
}

impl SourcePacer {
    pub(crate) fn new() -> SourcePacer {
        SourcePacer {
            clock: CadenceClock::new(CadenceTuning::snapping()),
            free_running: false,
        }
    }

    /// Fold a frame arriving at the store and answer when it is due, in the same clock
    /// domain `ready_ns` came in.
    ///
    /// `None` under the latency intent, which is arrival-driven by definition: it costs
    /// what it always did, and the loop never carries an estimate built from samples it
    /// then ignored.
    ///
    /// Called at SUBMIT rather than at take, so the estimate sees the arrival process the
    /// transport actually produced — the frames the store goes on to drop are part of it,
    /// and folding what survived the store would hide exactly the jitter being measured.
    pub(crate) fn due_ns(
        &mut self,
        smoothing: bool,
        src_pts_ns: u64,
        ready_ns: u64,
        frame_interval_ns: i64,
    ) -> Option<i64> {
        smoothing.then(|| {
            self.clock
                .due_ns(src_pts_ns, ready_ns as i64, frame_interval_ns)
        })
    }

    /// Follow the measured refresh verdict. Snapping a due time onto the latch grid carries
    /// roughly half a refresh of implicit slack, presenting at it directly carries none, so
    /// the two want different cushions.
    ///
    /// Re-tuning costs a re-anchor (the tuning is fixed at construction), which is why this
    /// is keyed to the probe's PUBLISHED verdict — agreed across rounds — and not to a
    /// per-window reading that was measured flapping on glass.
    pub(crate) fn follow(&mut self, verdict: Cadence) {
        let free = verdict == Cadence::Variable;
        if free != self.free_running {
            self.free_running = free;
            self.clock = CadenceClock::new(if free {
                CadenceTuning::free_running()
            } else {
                CadenceTuning::snapping()
            });
        }
    }

    /// Present at the due time itself instead of snapping it to the latch grid. True only
    /// where variable refresh is MEASURED live, which is the one case where the panel
    /// refreshes when we present and there is no grid to aim at.
    pub(crate) fn free_running(&self) -> bool {
        self.free_running
    }

    /// Re-anchor on the next frame — every discontinuity this loop already knows about (a
    /// display change, an accepted mid-session mode switch). The measured jitter survives
    /// it by design: it describes the link, not the stream.
    pub(crate) fn reset(&mut self) {
        self.clock.reset();
    }

    pub(crate) fn health(&self) -> CadenceHealth {
        self.clock.health()
    }
}

/// The FIFO glass budget: at most one undisplayed present in flight, measured by the
/// present-wait waiter's outstanding count. Never consulted under MAILBOX/IMMEDIATE
/// (they cannot queue) or without present-wait (nothing to count with — behavior is
/// then exactly the shipped arrival pacing).
#[derive(Default)]
pub(crate) struct PresentGate {
    /// Submit stamp of the newest tracked present; 0 = none yet.
    last_present_ns: u64,
    gated: u32,
    forced: u32,
}

impl PresentGate {
    /// May a new present go out? Open when nothing undisplayed is in flight; a stale
    /// in-flight present (occlusion, wedged compositor) force-opens after 100 ms so the
    /// stream survives, counted as `forced`.
    pub(crate) fn open(&mut self, outstanding: usize, now_ns: u64) -> bool {
        if outstanding == 0 {
            return true;
        }
        if self.last_present_ns != 0
            && now_ns.saturating_sub(self.last_present_ns) > STALE_REOPEN_NS
        {
            self.forced += 1;
            return true;
        }
        self.gated += 1;
        false
    }

    pub(crate) fn note_present(&mut self, now_ns: u64) {
        self.last_present_ns = now_ns;
    }

    /// Drain the window's counters: `(gated, forced)`.
    pub(crate) fn take_counters(&mut self) -> (u32, u32) {
        let c = (self.gated, self.forced);
        self.gated = 0;
        self.forced = 0;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Newest-wins: submit replaces, take clears, put_back only fills an empty slot.
    #[test]
    fn newest_wins_replaces_and_putback_never_clobbers() {
        let mut s: FrameStore<u32> = FrameStore::new(0);
        assert!(!s.is_smoothing());
        assert_eq!(s.take(|_| true), None);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        assert_eq!(s.take(|_| true), Some(3), "only the newest survives");
        assert_eq!(s.take(|_| true), None);
        // A taken-but-unpresented frame returns — unless a fresher one arrived.
        s.submit(4);
        let f = s.take(|_| true).unwrap();
        s.put_back(f);
        assert_eq!(s.take(|_| true), Some(4));
        let f = s.take(|_| true);
        assert_eq!(f, None);
        s.submit(5);
        let f = s.take(|_| true).unwrap();
        s.submit(6);
        s.put_back(f); // 6 arrived while 5 was out — 6 wins
        assert_eq!(s.take(|_| true), Some(6));
        assert_eq!(
            s.take_counters(),
            (2, 0, 0),
            "two displacements, no fifo counters"
        );
    }

    /// FIFO: preroll to capacity, drop-oldest overflow, underflow re-arms the preroll.
    #[test]
    fn fifo_prerolls_overflows_oldest_and_rearms_on_dry() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        assert!(s.is_smoothing());
        s.submit(1);
        assert_eq!(
            s.take(|_| true),
            None,
            "prerolling: below capacity, nothing vends"
        );
        s.submit(2);
        assert_eq!(s.take(|_| true), Some(1), "preroll reached — FIFO order");
        assert_eq!(
            s.take(|_| true),
            Some(2),
            "once prerolled the buffer drains normally"
        );
        // Dry after preroll = one underflow, preroll re-arms.
        assert_eq!(s.take(|_| true), None);
        s.submit(3);
        assert_eq!(s.take(|_| true), None, "re-armed preroll holds again");
        s.submit(4);
        assert_eq!(s.take(|_| true), Some(3));
        // Overflow drops the OLDEST: [4] → [4,5] → 6 evicts 4 → 7 evicts 5.
        s.submit(5);
        s.submit(6);
        s.submit(7);
        assert_eq!(s.take(|_| true), Some(6));
        assert_eq!(s.take(|_| true), Some(7));
        let (replaced, drops, dry) = s.take_counters();
        assert_eq!(replaced, 0);
        assert_eq!(drops, 2, "6 evicted 4, 7 evicted 5");
        assert_eq!(dry, 1);
    }

    /// put_back under FIFO goes to the FRONT (it is the oldest), and the transient
    /// capacity+1 is trimmed by the next submit.
    #[test]
    fn fifo_putback_restores_order() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        s.submit(1);
        s.submit(2);
        let f = s.take(|_| true).unwrap();
        s.put_back(f);
        assert_eq!(
            s.take(|_| true),
            Some(1),
            "the put-back frame is still first"
        );
    }

    /// A frame held for its due time is the smoothing intent working, not the store
    /// running dry: nothing is counted and the preroll it built stays armed. Counting it
    /// as an underflow would re-arm the preroll on every well-paced frame and stall the
    /// stream for a buffer's worth of frames each time.
    #[test]
    fn a_frame_held_for_its_due_time_is_not_an_underflow() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        s.submit(10);
        s.submit(20);
        assert_eq!(s.take(|_| false), None, "prerolled, but nothing is due yet");
        assert_eq!(s.take(|&v| v >= 10), Some(10));
        assert_eq!(s.take(|&v| v >= 30), None, "20 is not due yet either");
        assert_eq!(s.take(|_| true), Some(20));
        // NOW it is genuinely dry, which is an underflow and does re-arm the preroll.
        assert_eq!(s.take(|_| true), None);
        s.submit(30);
        assert_eq!(s.take(|_| true), None, "re-armed preroll holds again");
        assert_eq!(
            s.take_counters(),
            (0, 0, 1),
            "one dry, nothing from the holds"
        );
    }

    /// The desktop half of the intent split: a newest-wins store vends on arrival and
    /// never so much as ASKS for a due time, so no cadence clock can end up gating the
    /// latency intent even if a caller handed it one.
    #[test]
    fn the_latency_store_vends_without_ever_asking_a_due_time() {
        let mut s: FrameStore<u32> = FrameStore::new(0);
        s.submit(7);
        let mut asked = false;
        let got = s.take(|_| {
            asked = true;
            false
        });
        assert_eq!(
            got,
            Some(7),
            "arrival-driven: the frame goes out regardless"
        );
        assert!(!asked, "…and the due time was never consulted");
    }

    /// force_latency collapses a smoothing store to a newest-wins slot mid-stream.
    #[cfg(feature = "pyrowave")]
    #[test]
    fn force_latency_collapses_to_one_slot() {
        let mut s: FrameStore<u32> = FrameStore::new(3);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        s.force_latency();
        assert!(!s.is_smoothing());
        assert_eq!(
            s.take(|_| true),
            Some(3),
            "only the newest survives the collapse"
        );
        s.submit(4);
        s.submit(5);
        assert_eq!(s.take(|_| true), Some(5));
    }

    /// The clock learns the min positive spacing (capped at the mode refresh), anchors
    /// on the newest stamp, and extrapolates the next slot; sub-ms pairs (a queued
    /// double-present) never become the period.
    #[test]
    fn latch_clock_learns_and_extrapolates() {
        const P: u64 = 16_666_666; // 60 Hz
        let mut c = LatchClock::new(60);
        assert_eq!(c.period_ns(), P, "fallback = the mode refresh");
        // No anchor: a usable deadline one period out.
        assert_eq!(c.next_slot_after(1_000), 1_000 + P);

        c.note_batch(&[1_000_000_000, 1_000_000_000 + P, 1_000_000_000 + 2 * P]);
        assert_eq!(c.period_ns(), P);
        assert_eq!(c.anchor_ns(), 1_000_000_000 + 2 * P);
        let next = c.next_slot_after(c.anchor_ns());
        assert_eq!(next, 1_000_000_000 + 3 * P);
        // Mid-slot query lands on the same boundary; a later one steps whole periods.
        assert_eq!(c.next_slot_after(next - 1), next);
        assert_eq!(c.next_slot_after(next), next + P);

        // A queued pair (< 1 ms apart) must not poison the period.
        c.note_batch(&[2_000_000_000, 2_000_000_500]);
        assert_eq!(c.period_ns(), P);
        assert_eq!(c.anchor_ns(), 2_000_000_500, "the anchor still advances");

        // A stream presenting every OTHER refresh spaces its glass stamps at 2×P. One
        // such window must NOT move the grid — the shared learner needs a streak before
        // it will widen, which is what keeps a briefly-slow stream from claiming a slow
        // panel while still allowing a genuinely slower display to be discovered.
        c.note_batch(&[3_000_000_000, 3_000_000_000 + 2 * P]);
        assert_eq!(c.period_ns(), P, "one wide window is not a slower panel");

        // A single stamp re-anchors without touching the period.
        c.note_batch(&[5_000_000_000]);
        assert_eq!(c.anchor_ns(), 5_000_000_000);
        assert_eq!(c.period_ns(), P);

        // A faster panel learns its own finer grid.
        let mut fast = LatchClock::new(120);
        fast.note_batch(&[1_000_000_000, 1_008_333_333]);
        assert_eq!(fast.period_ns(), 8_333_333);
    }

    /// ⭐ The live loop drains present-wait samples EVERY pass, so stamps arrive one at a
    /// time. Measuring spacings only within a batch meant the learner observed nothing on
    /// glass and silently ran on its seed (found on .21, 2026-08-02: `period_us` read back
    /// exactly the 60 Hz fallback while the panel really was 60 Hz — correct by luck, and
    /// wrong the moment the mode lies).
    #[test]
    fn latch_clock_learns_from_one_sample_at_a_time() {
        const REAL: u64 = 16_666_666;
        let mut c = LatchClock::new(120); // seeded too fast, as a refused mode switch would
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 8 + 8) {
            t += REAL;
            c.note_batch(&[t]); // ONE stamp per call — the live shape
        }
        assert_eq!(
            c.period_ns(),
            REAL,
            "single-stamp batches must still feed the grid learner"
        );
        assert_eq!(c.anchor_ns(), t);
    }

    /// The mode's refresh is a CLAIM, not a measurement — a refused mode switch or a
    /// compositor running its own rate leaves the seed too fast. The old downward-only
    /// cap pinned that wrong grid for the session (the Android 0.23.0 defect); the
    /// shared learner climbs back out once the evidence is consistent.
    #[test]
    fn latch_clock_recovers_from_a_seed_faster_than_the_real_panel() {
        const REAL: u64 = 16_666_666; // the panel is really 60 Hz…
        let mut c = LatchClock::new(120); // …but the mode claimed 120
        assert_eq!(c.period_ns(), 8_333_333, "seeded from the claim");

        // Consistent 60 Hz evidence. The grid is fed the MIN of every
        // GRID_OBSERVE_EVERY spacings, and PanelGrid widens only after 8 agreeing
        // observations, so a real widen needs 8 × GRID_OBSERVE_EVERY spacings — the
        // deliberate cost of not letting one slow patch redefine the panel.
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 8 + GRID_OBSERVE_EVERY) {
            t += REAL;
            c.note_batch(&[t]);
        }
        assert_eq!(
            c.period_ns(),
            REAL,
            "a sustained slower grid is adopted instead of aimed past forever"
        );
    }

    /// The VRR discriminator: presents landing on the vblank grid read Fixed, presents
    /// landing wherever our own cadence puts them read Variable — including the case that
    /// matters most, a stream SLOWER than the panel, where a fixed panel still quantizes
    /// to a larger whole multiple.
    #[test]
    fn cadence_probe_separates_grid_locked_from_variable() {
        const P: u64 = 8_333_333; // 120 Hz
                                  // Enough spacings for CADENCE_STABLE_ROUNDS full rounds: a verdict is published
                                  // only once consecutive rounds agree (on glass a single round FLAPPED).
        const ROUNDS: u64 = (CADENCE_MIN_SAMPLES as u64) * (CADENCE_STABLE_ROUNDS as u64) + 4;

        // Fixed panel, stream at panel rate: every delta is exactly one period.
        let mut probe = CadenceProbe::new();
        assert_eq!(probe.verdict(), Cadence::Unknown, "no evidence yet");
        let stamps: Vec<u64> = (0..ROUNDS).map(|i| 1_000_000_000 + i * P).collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed);

        // Fixed panel, stream at HALF panel rate: deltas are 2×P — still grid-locked.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS).map(|i| 1_000_000_000 + i * 2 * P).collect();
        probe.note(&stamps, P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Fixed,
            "a slower stream on a fixed panel picks a larger k, it does not leave the grid"
        );

        // Fixed panel with realistic measurement jitter (±0.5 ms on an 8.3 ms period)
        // must not read as variable.
        let mut probe = CadenceProbe::new();
        let jitter = [0i64, 300_000, -250_000, 120_000, -400_000, 80_000];
        let stamps: Vec<u64> = (0..ROUNDS as usize)
            .map(|i| (1_000_000_000 + i as i64 * P as i64 + jitter[i % jitter.len()]) as u64)
            .collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed, "jitter is not VRR");

        // VRR live: a 100 fps stream on a 120 Hz-max panel. 10 ms is not a multiple of
        // 8.33 ms, so every present sits off the grid.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS)
            .map(|i| 1_000_000_000 + i * 10_000_000)
            .collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Variable);

        // A display change throws the evidence away rather than carrying a stale verdict.
        probe.reset();
        assert_eq!(probe.verdict(), Cadence::Unknown);

        // Below the sample floor nothing is claimed.
        let mut probe = CadenceProbe::new();
        probe.note(&[1_000_000_000, 1_010_000_000, 1_020_000_000], P, true);
        assert_eq!(probe.verdict(), Cadence::Unknown);

        // ⭐ THE SHAPE THE LIVE LOOP ACTUALLY PRODUCES: the run loop drains present-wait
        // samples every pass, so stamps arrive ONE AT A TIME. Measuring spacings only
        // within a batch observed nothing at all on glass — `vrr` stayed Unknown and the
        // latch clock ran on its seed forever. Found on .21, 2026-08-02.
        let mut probe = CadenceProbe::new();
        for i in 0..ROUNDS {
            probe.note(&[1_000_000_000 + i * 10_000_000], P, true); // 100 fps, off a 120 Hz grid
        }
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "one-sample batches must still yield spacings"
        );

        // A period we never learned can't discriminate anything.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS)
            .map(|i| 1_000_000_000 + i * 10_000_000)
            .collect();
        probe.note(&stamps, 0, true);
        assert_eq!(probe.verdict(), Cadence::Unknown);
    }

    /// ⭐ Batching must not change the verdict. The same spacings delivered as one big
    /// batch, or one stamp at a time, must reach the same conclusion — the live loop
    /// drains one at a time while tests hand over vectors, and an evaluation keyed to
    /// call boundaries silently made the two disagree.
    #[test]
    fn cadence_verdict_is_independent_of_batching() {
        const P: u64 = 8_333_333;
        let n = (CADENCE_MIN_SAMPLES as u64) * (CADENCE_STABLE_ROUNDS as u64) + 4;

        let stamps: Vec<u64> = (0..n).map(|i| 1_000_000_000 + i * P).collect();
        let mut bulk = CadenceProbe::new();
        bulk.note(&stamps, P, true);

        let mut drip = CadenceProbe::new();
        for s in &stamps {
            drip.note(&[*s], P, true);
        }

        assert_eq!(bulk.verdict(), Cadence::Fixed);
        assert_eq!(drip.verdict(), bulk.verdict(), "batching must not matter");
    }

    /// 120 Hz, and a source stamping a clock domain far from the present clock's — the
    /// pacer must never need to be told about the difference.
    const SRC_P: i64 = 8_333_333;
    const SRC_PTS0: u64 = 1_786_000_000_000_000_000;
    const SRC_READY0: u64 = 1_000_000_000;

    /// Fold `n` frames of a clean 120 Hz source through the pacer.
    fn fold(p: &mut SourcePacer, smoothing: bool, n: u64) {
        for k in 0..n {
            p.due_ns(
                smoothing,
                SRC_PTS0 + k * SRC_P as u64,
                SRC_READY0 + k * SRC_P as u64,
                SRC_P,
            );
        }
    }

    /// The clock half of the intent split: under latency the pacer folds NOTHING. The
    /// estimate exists only where it is used, so the latency path costs exactly what it
    /// cost before this work and a stream that collapses to latency mid-flight (PyroWave)
    /// leaves no half-built loop behind it.
    #[test]
    fn the_latency_intent_folds_no_frames_into_the_cadence_clock() {
        let mut p = SourcePacer::new();
        for k in 0..64u64 {
            assert_eq!(
                p.due_ns(
                    false,
                    SRC_PTS0 + k * SRC_P as u64,
                    SRC_READY0 + k * SRC_P as u64,
                    SRC_P
                ),
                None,
                "latency has no due time to answer with"
            );
        }
        let h = p.health();
        assert_eq!(h.frames, 0, "not one sample reached the loop");
        assert_eq!((h.offset_ns, h.skew_ns, h.jitter_ns), (0, 0, 0));
        // …and the very same frames under smoothness do reach it.
        assert!(p.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P).is_some());
        assert_eq!(p.health().frames, 1);
    }

    /// One domain in, same domain out (the clock's own invariant, asserted here because
    /// this binding is the one that feeds `decoded_ns` and reads back a `now_ns` deadline
    /// with no conversion between them): the due time lands on the PRESENT clock's
    /// timeline, however far the source's stamps are from it.
    #[test]
    fn a_due_time_comes_back_on_the_present_clocks_timeline() {
        let mut p = SourcePacer::new();
        fold(&mut p, true, 400);
        let k = 400u64;
        let due = p
            .due_ns(
                true,
                SRC_PTS0 + k * SRC_P as u64,
                SRC_READY0 + k * SRC_P as u64,
                SRC_P,
            )
            .unwrap();
        let ready = (SRC_READY0 + k * SRC_P as u64) as i64;
        assert!(
            (due - ready).abs() <= p.health().cushion_ns,
            "due {due} is not within a cushion of the present-clock ready {ready}"
        );
    }

    /// The VRR half: where variable refresh is MEASURED live there is no grid to snap to,
    /// so the due time is presented directly and the cushion has to cover the distribution
    /// on its own. Re-tuning is a fresh loop, which is why it follows the probe's published
    /// verdict — agreed across rounds — and not a per-window reading.
    #[test]
    fn a_measured_vrr_verdict_switches_the_cushion_policy() {
        let mut p = SourcePacer::new();
        assert!(!p.free_running(), "snapping until the panel says otherwise");
        fold(&mut p, true, 200);
        p.follow(Cadence::Fixed);
        assert!(!p.free_running());
        assert_eq!(
            p.health().frames,
            200,
            "a verdict that changes nothing must not re-anchor"
        );
        p.follow(Cadence::Variable);
        assert!(p.free_running());
        assert_eq!(p.health().frames, 0, "re-tuning is a fresh loop");
        p.follow(Cadence::Unknown);
        assert!(
            !p.free_running(),
            "Unknown is the absence of a measurement, not a measurement of VRR"
        );

        // The two tunings differ where it matters: with no jitter measured yet, the
        // free-running cushion already holds a frame back by more than the snapping one,
        // which is riding on the half-refresh the snap-up gives it for free.
        let mut snap = SourcePacer::new();
        snap.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        let mut free = SourcePacer::new();
        free.follow(Cadence::Variable);
        free.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        assert!(
            free.health().cushion_ns > snap.health().cushion_ns,
            "free-running {} must cushion past snapping {}",
            free.health().cushion_ns,
            snap.health().cushion_ns
        );
    }

    /// Gate: open at zero outstanding, closed at one, force-open past the stale bound.
    #[test]
    fn gate_budgets_one_undisplayed_present() {
        let mut g = PresentGate::default();
        let t0 = 1_000_000_000u64;
        assert!(g.open(0, t0));
        g.note_present(t0);
        assert!(!g.open(1, t0 + 8_000_000), "one in flight — hold");
        assert!(
            g.open(1, t0 + STALE_REOPEN_NS + 1),
            "stale in-flight present force-opens"
        );
        let (gated, forced) = g.take_counters();
        assert_eq!((gated, forced), (1, 1));
        assert_eq!(g.take_counters(), (0, 0), "counters drain");
    }
}
