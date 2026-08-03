//! Shared virtual-pad slot table + creation lifecycle, used by every backend manager (Linux
//! uinput/uhid, Windows XUSB/UMDF). See [`PadSlots`].

use crate::pad_gate::PadGate;
use anyhow::Result;
use punktfunk_core::input::MAX_PADS;
use std::time::{Duration, Instant};

// The unplug sweep walks a u16 `active_mask` (the wire type); every slot must have a bit.
const _: () = assert!(MAX_PADS <= 16);

/// How long a pad's `active_mask` bit must stay clear before the sweep actually drops it. A pad
/// teardown is a whole PnP device removal (system-wide device-change broadcasts, and on re-create
/// a full hidclass re-enumeration), so a client-side mask glitch of a few state frames must not
/// flap devnodes. A REAL unplug just tears down one grace later; games already saw the pad go
/// quiet.
const SWEEP_GRACE: Duration = Duration::from_millis(300);

/// The slot table + lifecycle every virtual-pad manager repeats: `Vec<Option<P>>` keyed by wire pad
/// index, the `active_mask` unplug sweep, and the [`PadGate`]-guarded create. Extracted verbatim
/// from seven copy-pasted managers (G12) so a lifecycle fix lands once, not seven times.
///
/// Division of labor: `PadSlots` owns the pads' *existence* (create / sweep / lookup) and logs the
/// shared lifecycle lines (unplug, create-failure); the backend keeps everything per-controller —
/// its state model, feedback pump, and the success log inside `open` (which knows the transport
/// detail worth printing). Per-index sibling state (`state` / `last_rumble` / dedup / clocks) stays
/// in the manager, which resets it on the indices [`sweep`](Self::sweep) returns and on a `true`
/// from [`ensure`](Self::ensure).
pub struct PadSlots<P> {
    pads: Vec<Option<P>>,
    /// When each ALLOCATED slot's `active_mask` bit was first seen clear (`None` = active, or
    /// empty) — the [`SWEEP_GRACE`] debounce clock.
    inactive_since: Vec<Option<Instant>>,
    /// Create-retry gate: a transient backend failure backs off and retries instead of permanently
    /// disabling every pad for the session.
    gate: PadGate,
    /// Backend tag in the shared lifecycle log lines, e.g. `"DualSense/Windows"` — keeps every
    /// existing per-backend line byte-identical (ops greps survive the extraction).
    label: &'static str,
    /// Device name in the create-failure line ("virtual `<device>` creation failed …").
    device: &'static str,
    /// Suffix for the create-failure line — empty on Linux, the driver-install hint on Windows.
    hint: &'static str,
}

impl<P> PadSlots<P> {
    /// An empty table of [`MAX_PADS`] slots whose lifecycle log lines carry `label` / `device` /
    /// `hint` (see the field docs).
    pub fn new(label: &'static str, device: &'static str, hint: &'static str) -> PadSlots<P> {
        PadSlots {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            inactive_since: (0..MAX_PADS).map(|_| None).collect(),
            gate: PadGate::new(),
            label,
            device,
            hint,
        }
    }

    /// The backend tag this table logs with (for the manager's own arrival line).
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Fold one state frame's `active_mask` into the grace clocks, then drop whatever has run out
    /// (see [`Self::reap`]). Returns the dropped indices as a bitmask so the caller resets its
    /// per-index sibling state; an index another manager owns is `None` here, so it is never
    /// touched. The grace is the devnode-churn debounce: a mask that glitches clear for a few
    /// frames and returns re-arms nothing.
    ///
    /// A frame can only ARM the grace, never complete it — no time has passed at the instant the
    /// clock starts. Since the producer emits exactly ONE frame per detach, [`Self::reap`] on the
    /// manager's periodic pump is what actually finishes the unplug; a backend that only ever
    /// called `sweep` would keep the detached pad alive for the rest of the session.
    pub fn sweep(&mut self, active_mask: u16) -> u16 {
        self.sweep_at(active_mask, Instant::now())
    }

    /// Drop every allocated pad whose grace has run out, logging each — the half of the unplug
    /// that needs no state frame. Returns the dropped indices as a bitmask, same as [`Self::sweep`].
    ///
    /// This can only ever *complete* an unplug some frame already started: it never arms a clock,
    /// so however often it runs it cannot drop a pad whose `active_mask` bit never went clear.
    /// That is what makes it safe to call from a hot pump loop.
    pub fn reap(&mut self) -> u16 {
        self.reap_at(Instant::now())
    }

    /// Backdate every armed grace clock by [`SWEEP_GRACE`], so the NEXT sweep drops the pads
    /// whose bits are still clear — consumer tests (the managers') drive the debounce without
    /// wall-clock sleeps. Test-only: production code has no business expiring the grace.
    #[cfg(test)]
    pub(crate) fn expire_grace(&mut self) {
        for since in self.inactive_since.iter_mut().flatten() {
            *since -= SWEEP_GRACE;
        }
    }

    /// [`Self::sweep`] with an injectable clock (unit tests drive the grace window): arm or disarm
    /// each slot's clock from the mask, then reap whatever has already run out.
    fn sweep_at(&mut self, active_mask: u16, now: Instant) -> u16 {
        for i in 0..MAX_PADS {
            if active_mask & (1 << i) != 0 {
                self.inactive_since[i] = None; // active (again): a glitch never reaches the drop
            } else if self.pads[i].is_some() && self.inactive_since[i].is_none() {
                self.inactive_since[i] = Some(now); // newly inactive — start the grace
            }
        }
        self.reap_at(now)
    }

    /// [`Self::reap`] with an injectable clock. Deliberately arms nothing — it only ever reads
    /// `inactive_since` and clears it, so a pad whose bit never went clear has no clock to run out
    /// and cannot be dropped here.
    fn reap_at(&mut self, now: Instant) -> u16 {
        let mut swept = 0u16;
        for i in 0..MAX_PADS {
            let Some(since) = self.inactive_since[i] else {
                continue; // active, or never went clear — nothing to complete
            };
            if self.pads[i].is_none() {
                self.inactive_since[i] = None; // the slot went away by some other route
                continue;
            }
            if now.duration_since(since) >= SWEEP_GRACE {
                tracing::info!(index = i, "controller unplugged ({})", self.label);
                self.pads[i] = None;
                self.inactive_since[i] = None;
                swept |= 1 << i;
            }
        }
        swept
    }

    /// Create the pad at `idx` via `open` if the slot is empty and the create gate allows it.
    /// Returns `true` only on a fresh create (the caller resets its per-index sibling state);
    /// `open` logs its own success line (it knows the transport detail), failure is logged here.
    pub fn ensure(&mut self, idx: usize, open: impl FnOnce(u8) -> Result<P>) -> bool {
        if idx >= MAX_PADS || self.pads[idx].is_some() || !self.gate.allow(Instant::now()) {
            return false;
        }
        match open(idx as u8) {
            Ok(p) => {
                self.pads[idx] = Some(p);
                self.inactive_since[idx] = None; // a fresh pad starts its grace clock unarmed
                self.gate.on_success();
                true
            }
            Err(e) => {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "virtual {} creation failed — retrying with backoff{}",
                    self.device,
                    self.hint
                );
                self.gate.on_failure(Instant::now());
                false
            }
        }
    }

    /// The live pad at `idx`, if any (out-of-range → `None`).
    pub fn get(&self, idx: usize) -> Option<&P> {
        self.pads.get(idx).and_then(|s| s.as_ref())
    }

    /// The live pad at `idx`, mutably, if any (out-of-range → `None`).
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut P> {
        self.pads.get_mut(idx).and_then(|s| s.as_mut())
    }

    /// Iterate the live pads as `(index, &mut pad)` (the feedback-pump shape).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut P)> {
        self.pads
            .iter_mut()
            .enumerate()
            .filter_map(|(i, s)| s.as_mut().map(|p| (i, p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn slots() -> PadSlots<u32> {
        PadSlots::new("Test", "test pad", "")
    }

    #[test]
    fn a_single_frame_plus_a_reap_completes_the_unplug() {
        // The shape production actually produces: ONE cleared-mask frame, then time, then a reap
        // with no further frame. Before the arm/reap split the pad survived here forever.
        let mut s = slots();
        assert!(s.ensure(2, |i| Ok(i as u32)));
        assert_eq!(
            s.sweep(0b0),
            0,
            "a frame arms the grace but cannot itself drop"
        );
        assert!(s.get(2).is_some());
        s.expire_grace();
        assert_eq!(s.reap(), 1 << 2, "the reap did not complete the unplug");
        assert!(s.get(2).is_none());
        assert_eq!(s.reap(), 0, "nothing left to reap");
    }

    #[test]
    fn reap_never_drops_a_pad_no_frame_ever_deactivated() {
        // Reaping COMPLETES an unplug; it must never invent one. A pad whose bit never went clear
        // has no armed clock, so any number of reaps — even with the clock backdated — leaves it.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        for _ in 0..10 {
            assert_eq!(s.reap(), 0);
            s.expire_grace();
        }
        assert!(
            s.get(0).is_some(),
            "reap dropped a pad that never went inactive"
        );
    }

    #[test]
    fn a_glitch_that_returns_inside_the_grace_never_drops_the_pad() {
        // The anti-flap guarantee, now that reaps are frequent: a client mask that blips clear and
        // comes back must not churn a PnP devnode.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        assert_eq!(s.sweep(0b0), 0); // bit clears — arms only
        for _ in 0..5 {
            assert_eq!(s.reap(), 0, "dropped a pad inside its grace");
        }
        assert_eq!(s.sweep(0b1), 0); // the bit returns — disarms
        s.expire_grace();
        assert_eq!(s.reap(), 0, "a returned bit must leave nothing armed");
        assert!(s.get(0).is_some());
    }

    #[test]
    fn ensure_creates_once_and_reports_freshness() {
        let mut s = slots();
        // Fresh create → true; the pad is live.
        assert!(s.ensure(3, |i| Ok(i as u32 * 10)));
        assert_eq!(s.get(3), Some(&30));
        // Occupied slot → no re-open (the closure must not run), no reset signal.
        assert!(!s.ensure(3, |_| panic!("re-opened an occupied slot")));
        // Out of range → never opens.
        assert!(!s.ensure(MAX_PADS, |_| panic!("opened out of range")));
        assert_eq!(s.get(MAX_PADS), None);
    }

    #[test]
    fn sweep_drops_only_cleared_bits_and_returns_them_once() {
        let mut s = slots();
        assert!(s.ensure(0, |_| Ok(0)));
        assert!(s.ensure(2, |_| Ok(2)));
        assert!(s.ensure(5, |_| Ok(5)));
        // Mask keeps 2, clears 0 and 5; empty slots (1, 3, …) are untouched non-events. The
        // first sweep only ARMS the grace clock…
        let t0 = Instant::now();
        assert_eq!(s.sweep_at(0b0000_0100, t0), 0);
        assert_eq!(s.get(0), Some(&0), "still inside the grace");
        // …and the drop lands once the bits have stayed clear for the whole grace.
        let swept = s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE);
        assert_eq!(swept, 0b0010_0001);
        assert_eq!(s.get(0), None);
        assert_eq!(s.get(2), Some(&2));
        assert_eq!(s.get(5), None);
        // A further identical sweep is a no-op: the indices were returned exactly once.
        assert_eq!(s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE * 2), 0);
    }

    #[test]
    fn a_mask_glitch_inside_the_grace_never_drops() {
        // The devnode-churn debounce: a bit that clears for a few state frames and returns must
        // not tear down (and later re-create) a whole PnP devnode.
        let mut s = slots();
        assert!(s.ensure(1, |_| Ok(7)));
        let t0 = Instant::now();
        assert_eq!(s.sweep_at(0, t0), 0); // bit clears — grace armed
        assert_eq!(s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE / 2), 0); // bit returns — disarmed
        assert_eq!(s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE * 10), 0);
        assert_eq!(s.get(1), Some(&7), "the glitch never reached the drop");
    }

    #[test]
    fn create_failure_arms_the_gate_and_success_heals_it() {
        let mut s = slots();
        assert!(!s.ensure(1, |_| bail!("transient")));
        // Backoff in effect: the next attempt is blocked without even calling `open`.
        assert!(!s.ensure(1, |_| panic!("open during backoff")));
        // The gate is manager-wide (create failures are systemic), so other indices block too.
        assert!(!s.ensure(2, |_| panic!("open during backoff")));
        // …and a sweep-then-recreate of a *different* live pad is equally gated, but the table
        // itself is intact: nothing was allocated.
        assert_eq!(s.get(1), None);
    }

    #[test]
    fn recreate_after_sweep_resets_freshness() {
        let mut s = slots();
        assert!(s.ensure(4, |_| Ok(1)));
        let t0 = Instant::now();
        s.sweep_at(0, t0);
        s.sweep_at(0, t0 + SWEEP_GRACE);
        assert_eq!(s.get(4), None);
        // The slot is free again → a fresh create (true) with a new value.
        assert!(s.ensure(4, |_| Ok(2)));
        assert_eq!(s.get(4), Some(&2));
    }

    #[test]
    fn iter_mut_yields_live_pads_with_indices() {
        let mut s = slots();
        assert!(s.ensure(1, |_| Ok(10)));
        assert!(s.ensure(6, |_| Ok(60)));
        let seen: Vec<(usize, u32)> = s.iter_mut().map(|(i, p)| (i, *p)).collect();
        assert_eq!(seen, vec![(1, 10), (6, 60)]);
    }
}
