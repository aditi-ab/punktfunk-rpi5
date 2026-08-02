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
//!
//! Everything here is pure state + arithmetic on `CLOCK_REALTIME` ns (the
//! `pf_client_core::session::now_ns` domain the on-glass stamps live in); the run loop
//! owns all clocks and Vulkan calls, which is what keeps this testable.

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

    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
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

    pub(crate) fn take(&mut self) -> Option<T> {
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
        match self.frames.pop_front() {
            Some(f) => Some(f),
            None => {
                self.underflows += 1;
                self.prerolled = false;
                None
            }
        }
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
    grid: punktfunk_core::phase::PanelGrid,
    fallback_period_ns: u64,
}

impl LatchClock {
    pub(crate) fn new(refresh_hz: u32) -> LatchClock {
        LatchClock {
            anchor_ns: 0,
            grid: punktfunk_core::phase::PanelGrid::seeded(refresh_hz as i32),
            fallback_period_ns: 1_000_000_000 / u64::from(refresh_hz.max(1)),
        }
    }

    /// Fold a batch of on-glass stamps (ascending submission order). A single stamp
    /// re-anchors without touching the learned period — that is also the no-present-wait
    /// degradation, where each submit stamp anchors an approximate grid on the mode's
    /// refresh period.
    pub(crate) fn note_batch(&mut self, stamps: &[u64]) {
        if let Some(&last) = stamps.last() {
            self.anchor_ns = last;
        }
        let min_delta = stamps
            .windows(2)
            .map(|w| w[1].saturating_sub(w[0]))
            .filter(|&d| d > 1_000_000) // < 1 ms apart = a queued pair, not a grid step
            .min();
        if let Some(d) = min_delta {
            self.grid.observe(d as i64);
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
    verdict: Cadence,
}

/// Enough deltas to distinguish jitter from a real off-grid cadence.
const CADENCE_MIN_SAMPLES: usize = 24;
/// Median off-grid distance under this fraction of a period reads as grid-locked. Present
/// stamps carry real measurement jitter (the wait returns, then we read the clock), so
/// this is deliberately loose — the two regimes differ by far more than this in practice.
const CADENCE_FIXED_MILLI: u32 = 150;

impl CadenceProbe {
    pub(crate) fn new() -> CadenceProbe {
        CadenceProbe {
            off_grid_milli: Vec::with_capacity(64),
            verdict: Cadence::Unknown,
        }
    }

    /// Fold one window's on-glass stamps against the learned panel period.
    pub(crate) fn note(&mut self, stamps: &[u64], period_ns: u64) {
        if period_ns == 0 {
            return;
        }
        for w in stamps.windows(2) {
            let delta = w[1].saturating_sub(w[0]);
            if delta == 0 {
                continue;
            }
            let rem = delta % period_ns;
            // Distance to the NEAREST multiple, so a delta just under k×period reads as
            // close to the grid rather than a whole period away from k-1.
            let off = rem.min(period_ns - rem);
            self.off_grid_milli
                .push((off.saturating_mul(1000) / period_ns) as u32);
        }
        if self.off_grid_milli.len() >= CADENCE_MIN_SAMPLES {
            self.off_grid_milli.sort_unstable();
            let median = self.off_grid_milli[self.off_grid_milli.len() / 2];
            self.verdict = if median <= CADENCE_FIXED_MILLI {
                Cadence::Fixed
            } else {
                Cadence::Variable
            };
            self.off_grid_milli.clear();
        }
    }

    pub(crate) fn verdict(&self) -> Cadence {
        self.verdict
    }

    /// A mode switch / display change invalidates the evidence.
    pub(crate) fn reset(&mut self) {
        self.off_grid_milli.clear();
        self.verdict = Cadence::Unknown;
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
        assert_eq!(s.take(), None);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        assert_eq!(s.take(), Some(3), "only the newest survives");
        assert_eq!(s.take(), None);
        // A taken-but-unpresented frame returns — unless a fresher one arrived.
        s.submit(4);
        let f = s.take().unwrap();
        s.put_back(f);
        assert_eq!(s.take(), Some(4));
        let f = s.take();
        assert_eq!(f, None);
        s.submit(5);
        let f = s.take().unwrap();
        s.submit(6);
        s.put_back(f); // 6 arrived while 5 was out — 6 wins
        assert_eq!(s.take(), Some(6));
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
        assert_eq!(s.take(), None, "prerolling: below capacity, nothing vends");
        s.submit(2);
        assert_eq!(s.take(), Some(1), "preroll reached — FIFO order");
        assert_eq!(
            s.take(),
            Some(2),
            "once prerolled the buffer drains normally"
        );
        // Dry after preroll = one underflow, preroll re-arms.
        assert_eq!(s.take(), None);
        s.submit(3);
        assert_eq!(s.take(), None, "re-armed preroll holds again");
        s.submit(4);
        assert_eq!(s.take(), Some(3));
        // Overflow drops the OLDEST: [4] → [4,5] → 6 evicts 4 → 7 evicts 5.
        s.submit(5);
        s.submit(6);
        s.submit(7);
        assert_eq!(s.take(), Some(6));
        assert_eq!(s.take(), Some(7));
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
        let f = s.take().unwrap();
        s.put_back(f);
        assert_eq!(s.take(), Some(1), "the put-back frame is still first");
    }

    /// force_latency collapses a smoothing store to a newest-wins slot mid-stream.
    #[test]
    fn force_latency_collapses_to_one_slot() {
        let mut s: FrameStore<u32> = FrameStore::new(3);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        s.force_latency();
        assert!(!s.is_smoothing());
        assert_eq!(s.take(), Some(3), "only the newest survives the collapse");
        s.submit(4);
        s.submit(5);
        assert_eq!(s.take(), Some(5));
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

    /// The mode's refresh is a CLAIM, not a measurement — a refused mode switch or a
    /// compositor running its own rate leaves the seed too fast. The old downward-only
    /// cap pinned that wrong grid for the session (the Android 0.23.0 defect); the
    /// shared learner climbs back out once the evidence is consistent.
    #[test]
    fn latch_clock_recovers_from_a_seed_faster_than_the_real_panel() {
        const REAL: u64 = 16_666_666; // the panel is really 60 Hz…
        let mut c = LatchClock::new(120); // …but the mode claimed 120
        assert_eq!(c.period_ns(), 8_333_333, "seeded from the claim");

        // Consistent 60 Hz evidence, one window at a time.
        for i in 0..8 {
            let t = 1_000_000_000 + i * 2 * REAL;
            c.note_batch(&[t, t + REAL]);
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

        // Fixed panel, stream at panel rate: every delta is exactly one period.
        let mut probe = CadenceProbe::new();
        assert_eq!(probe.verdict(), Cadence::Unknown, "no evidence yet");
        let stamps: Vec<u64> = (0..40).map(|i| 1_000_000_000 + i * P).collect();
        probe.note(&stamps, P);
        assert_eq!(probe.verdict(), Cadence::Fixed);

        // Fixed panel, stream at HALF panel rate: deltas are 2×P — still grid-locked.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..40).map(|i| 1_000_000_000 + i * 2 * P).collect();
        probe.note(&stamps, P);
        assert_eq!(
            probe.verdict(),
            Cadence::Fixed,
            "a slower stream on a fixed panel picks a larger k, it does not leave the grid"
        );

        // Fixed panel with realistic measurement jitter (±0.5 ms on an 8.3 ms period)
        // must not read as variable.
        let mut probe = CadenceProbe::new();
        let jitter = [0i64, 300_000, -250_000, 120_000, -400_000, 80_000];
        let stamps: Vec<u64> = (0..40)
            .map(|i| (1_000_000_000 + i as i64 * P as i64 + jitter[i % jitter.len()]) as u64)
            .collect();
        probe.note(&stamps, P);
        assert_eq!(probe.verdict(), Cadence::Fixed, "jitter is not VRR");

        // VRR live: a 100 fps stream on a 120 Hz-max panel. 10 ms is not a multiple of
        // 8.33 ms, so every present sits off the grid.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..40).map(|i| 1_000_000_000 + i * 10_000_000).collect();
        probe.note(&stamps, P);
        assert_eq!(probe.verdict(), Cadence::Variable);

        // A display change throws the evidence away rather than carrying a stale verdict.
        probe.reset();
        assert_eq!(probe.verdict(), Cadence::Unknown);

        // Below the sample floor nothing is claimed.
        let mut probe = CadenceProbe::new();
        probe.note(&[1_000_000_000, 1_010_000_000, 1_020_000_000], P);
        assert_eq!(probe.verdict(), Cadence::Unknown);

        // A period we never learned can't discriminate anything.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..40).map(|i| 1_000_000_000 + i * 10_000_000).collect();
        probe.note(&stamps, 0);
        assert_eq!(probe.verdict(), Cadence::Unknown);
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
