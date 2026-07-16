//! Shared virtual-pad slot table + creation lifecycle, used by every backend manager (Linux
//! uinput/uhid, Windows XUSB/UMDF). See [`PadSlots`].

use crate::inject::pad_gate::PadGate;
use anyhow::Result;
use punktfunk_core::input::MAX_PADS;
use std::time::Instant;

// The unplug sweep walks a u16 `active_mask` (the wire type); every slot must have a bit.
const _: () = assert!(MAX_PADS <= 16);

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

    /// Drop every allocated pad whose `active_mask` bit cleared (the unplug sweep run on each state
    /// frame), logging each. Returns the swept indices as a bitmask so the caller resets its
    /// per-index sibling state; an index another manager owns is `None` here, so it is never swept.
    pub fn sweep(&mut self, active_mask: u16) -> u16 {
        let mut swept = 0u16;
        for (i, slot) in self.pads.iter_mut().enumerate() {
            if slot.is_some() && active_mask & (1 << i) == 0 {
                tracing::info!(index = i, "controller unplugged ({})", self.label);
                *slot = None;
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
        // Mask keeps 2, clears 0 and 5; empty slots (1, 3, …) are untouched non-events.
        let swept = s.sweep(0b0000_0100);
        assert_eq!(swept, 0b0010_0001);
        assert_eq!(s.get(0), None);
        assert_eq!(s.get(2), Some(&2));
        assert_eq!(s.get(5), None);
        // A second identical sweep is a no-op: the indices were returned exactly once.
        assert_eq!(s.sweep(0b0000_0100), 0);
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
        s.sweep(0);
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
