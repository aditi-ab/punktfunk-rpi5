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

/// A create failure whose CAUSE the backend was able to identify, attached to the `anyhow` error
/// it returns (`err.context(PadCreateFault::…)`) so [`PadSlots::ensure`] can print the matching
/// remedy instead of the backend's default one.
///
/// Why this exists. The create-failure line's remedy is a per-backend constant (`PadSlots`'s
/// `hint`, from [`PadSlots::new`]), and on Windows that constant says "install/repair: punktfunk-host.exe
/// driver install --gamepad", because a pad create that fails there has nearly always failed for
/// want of the UMDF driver package. Nearly. On 2026-08-09 a `.173` devtest hit a create that
/// failed for the opposite reason — the drivers were fine and a LIVE SIBLING PROCESS already owned
/// the pad index's OS-level name — and the line told the operator to repair a driver that was
/// working. Worse, the run carried on: the retry could not succeed while the other process held
/// the index, and everything measured afterwards was that other process's pad (a frozen XInput
/// packet count read as a real measurement). A wrong remedy is worse than no remedy, so a backend
/// that can name the cause now says so and the line follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadCreateFault {
    /// The OS-level name this pad index needs — on Windows the `Global\pf…-boot-<index>` bootstrap
    /// mailbox — is already held by another LIVE process.
    ///
    /// Retrying stays right and is deliberately left alone: the name frees itself the moment the
    /// owner releases it (a session ending, a service restart), and that is exactly how the field
    /// case recovered. What retrying can never do is *hurry* it, and no driver install affects it
    /// at all — which is the whole content of [`Self::hint`].
    IndexOwnedElsewhere,
}

impl PadCreateFault {
    /// Short tag for the structured `fault` log field — greppable; the prose lives in
    /// [`Self::hint`].
    pub fn as_str(self) -> &'static str {
        match self {
            PadCreateFault::IndexOwnedElsewhere => "index-owned-elsewhere",
        }
    }

    /// The remedy this fault gets INSTEAD of the backend's default hint.
    pub fn hint(self) -> &'static str {
        match self {
            PadCreateFault::IndexOwnedElsewhere => {
                " — this pad index is already owned by another LIVE process (on a Windows host \
                 that is the LocalSystem PunktfunkHost service, whose session still holds the \
                 pad). The drivers are not the problem and reinstalling them will not help: the \
                 retry succeeds on its own once that process releases the index (end its session, \
                 or Restart-Service PunktfunkHost), or run against a pad index it does not hold."
            }
        }
    }
}

impl std::fmt::Display for PadCreateFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PadCreateFault::IndexOwnedElsewhere => f.write_str(
                "the OS name this pad index needs is already owned by another live process",
            ),
        }
    }
}

/// The fault a backend attached to a create error, if any.
///
/// An `anyhow` context downcast, which is what makes this usable from a backend: the fault is
/// found however many further `.context()` layers were wrapped around it on the way up, so a
/// backend can attach it at the exact call that failed and still describe the failure in its own
/// words afterwards. Split out of [`PadSlots::ensure`] so the choice is testable without standing
/// up a tracing subscriber — and so the downcast-through-context behaviour this depends on is
/// pinned by a test rather than assumed (the attaching code is `cfg(windows)` and cannot be
/// compiled, let alone run, on a developer machine).
fn create_fault(err: &anyhow::Error) -> Option<PadCreateFault> {
    err.downcast_ref::<PadCreateFault>().copied()
}

/// What one [`PadSlots::sweep`] changed, as bitmasks over the wire pad indices.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Sweep {
    /// Slots whose pad was torn down because its grace ran out — the caller resets their
    /// per-index sibling state.
    pub dropped: u16,
    /// Slots whose `active_mask` bit returned *inside* the grace window. The debounce did its job
    /// and no devnode flapped — but the pad that comes back is not necessarily the pad that left:
    /// unplug one controller and plug another into the same wire index within [`SWEEP_GRACE`] and
    /// the new one drives the old one's live virtual pad, skipping the create path (and therefore
    /// the manager's reset) entirely. Anything the manager persists on the client's behalf —
    /// touch contacts, motion — is the previous controller's and has to go; a pad with no gyro
    /// would otherwise inherit the last one's rotation and never send a sample to correct it.
    pub reclaimed: u16,
}

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
    /// (see [`Self::reap`]). Returns what changed so the caller can fix up its per-index sibling
    /// state; an index another manager owns is `None` here, so it is never touched. The grace is
    /// the devnode-churn debounce: a mask that glitches clear for a few frames and returns re-arms
    /// nothing.
    ///
    /// A frame can only ARM the grace, never complete it — no time has passed at the instant the
    /// clock starts. Since the producer emits exactly ONE frame per detach, [`Self::reap`] on the
    /// manager's periodic pump is what actually finishes the unplug; a backend that only ever
    /// called `sweep` would keep the detached pad alive for the rest of the session.
    pub fn sweep(&mut self, active_mask: u16) -> Sweep {
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
    fn sweep_at(&mut self, active_mask: u16, now: Instant) -> Sweep {
        let mut reclaimed = 0u16;
        for i in 0..MAX_PADS {
            if active_mask & (1 << i) != 0 {
                // Active (again): a glitch never reaches the drop. If a clock WAS armed, this slot
                // just handed a live pad to whatever controller is present now, without passing
                // through `ensure` — see `Sweep::reclaimed`.
                if self.inactive_since[i].take().is_some() {
                    reclaimed |= 1 << i;
                }
            } else if self.pads[i].is_some() && self.inactive_since[i].is_none() {
                self.inactive_since[i] = Some(now); // newly inactive — start the grace
            }
        }
        Sweep {
            dropped: self.reap_at(now),
            reclaimed,
        }
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
                // Which remedy to print. The backend's default `hint` assumes the failure is the
                // one that dominates the field — on Windows an absent or stale driver package —
                // and sends the operator to reinstall. For a create that failed because a live
                // sibling owns this index that advice is not merely useless, it is a wrong lead
                // that costs a debugging session (2026-08-09, `.173`), so a named fault overrides
                // it. Anonymous failures keep the previous wording byte for byte.
                //
                // `index` is new and unconditional: the line used to name the backend and the
                // device but never the SLOT, so a multi-pad session's failure could not be told
                // from any other pad's.
                let fault = create_fault(&e);
                tracing::error!(
                    index = idx,
                    error = %format!("{e:#}"),
                    fault = fault.map_or("unclassified", PadCreateFault::as_str),
                    "virtual {} creation failed — retrying with backoff{}",
                    self.device,
                    fault.map_or(self.hint, PadCreateFault::hint)
                );
                self.gate.on_failure(Instant::now());
                false
            }
        }
    }

    /// How many pads this table currently holds.
    ///
    /// The question a bring-up harness has to ask before it believes anything it measures: a
    /// create that failed leaves the slot empty and [`Self::ensure`] only logs, so a devtest that
    /// pushes frames regardless is measuring whatever OTHER process's pad is answering on that
    /// index — which is exactly how a stale pad's frozen packet count was once read as a result
    /// (2026-08-09). Not `len` (and so not paired with `is_empty`): it counts LIVE pads, not the
    /// fixed [`MAX_PADS`] slots the table always has.
    pub fn live(&self) -> usize {
        self.pads.iter().flatten().count()
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
            Sweep::default(),
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
        assert_eq!(s.sweep(0b0), Sweep::default()); // bit clears — arms only
        for _ in 0..5 {
            assert_eq!(s.reap(), 0, "dropped a pad inside its grace");
        }
        // The bit returns — disarms, and reports the re-claim: the pad survived, but whoever is
        // driving it now may not be the controller that armed the clock.
        assert_eq!(
            s.sweep(0b1),
            Sweep {
                dropped: 0,
                reclaimed: 1,
            }
        );
        s.expire_grace();
        assert_eq!(s.reap(), 0, "a returned bit must leave nothing armed");
        assert!(s.get(0).is_some());
    }

    #[test]
    fn a_mask_that_never_went_clear_is_not_a_reclaim() {
        // `reclaimed` must mean "came back inside the grace", not "is present" — a steady-state
        // frame stream would otherwise clear the client's touch and motion on every single frame.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        for _ in 0..5 {
            assert_eq!(s.sweep(0b1), Sweep::default());
        }
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
        assert_eq!(s.sweep_at(0b0000_0100, t0), Sweep::default());
        assert_eq!(s.get(0), Some(&0), "still inside the grace");
        // …and the drop lands once the bits have stayed clear for the whole grace.
        let swept = s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE);
        assert_eq!(
            swept,
            Sweep {
                dropped: 0b0010_0001,
                reclaimed: 0,
            }
        );
        assert_eq!(s.get(0), None);
        assert_eq!(s.get(2), Some(&2));
        assert_eq!(s.get(5), None);
        // A further identical sweep is a no-op: the indices were returned exactly once.
        assert_eq!(
            s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE * 2),
            Sweep::default()
        );
    }

    #[test]
    fn a_mask_glitch_inside_the_grace_never_drops() {
        // The devnode-churn debounce: a bit that clears for a few state frames and returns must
        // not tear down (and later re-create) a whole PnP devnode.
        let mut s = slots();
        assert!(s.ensure(1, |_| Ok(7)));
        let t0 = Instant::now();
        // The bit clears — grace armed.
        assert_eq!(s.sweep_at(0, t0), Sweep::default());
        // The bit returns: disarmed, and reported as a re-claim (the pad lives, but its owner may
        // have changed — see `Sweep::reclaimed`).
        assert_eq!(
            s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE / 2),
            Sweep {
                dropped: 0,
                reclaimed: 0b0000_0010,
            }
        );
        // …and once, not on every subsequent frame.
        assert_eq!(
            s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE * 10),
            Sweep::default()
        );
        assert_eq!(s.get(1), Some(&7), "the glitch never reached the drop");
    }

    /// The mechanism the Windows backend's diagnosis rests on, and the one thing about it that
    /// could quietly stop working: [`create_fault`] must find the fault through however many
    /// `.context()` layers wrapped it. The real chain is built in `gamepad_raii::create_named`
    /// (`cfg(windows)`, so neither compiled nor run here) and has exactly this shape — the OS
    /// error at the bottom, the fault, then the human sentence on top — so reproduce it verbatim.
    #[test]
    fn a_named_fault_survives_the_context_layers_wrapped_around_it() {
        let err = anyhow::Error::msg("Zugriff verweigert (0x80070005)")
            .context(PadCreateFault::IndexOwnedElsewhere)
            .context("bootstrap mailbox Global\\pfds-boot-0 already exists");
        assert_eq!(
            create_fault(&err),
            Some(PadCreateFault::IndexOwnedElsewhere)
        );
        // …and the operator-facing rendering still carries every layer, newest first, so the
        // underlying OS error is never traded away for the diagnosis.
        let shown = format!("{err:#}");
        assert!(shown.contains("Global\\pfds-boot-0"), "{shown}");
        assert!(
            shown.contains("already owned by another live process"),
            "{shown}"
        );
        assert!(shown.contains("0x80070005"), "{shown}");
    }

    #[test]
    fn an_unclassified_failure_carries_no_fault() {
        // Every other backend failure — a missing driver, a wedged PnP, an EBUSY on /dev/uinput —
        // must keep the backend's own hint, so the absence of a fault has to read as absence.
        assert_eq!(
            create_fault(&anyhow::Error::msg("SwDeviceCreate failed")),
            None
        );
    }

    /// THE regression this classification exists for: the contended remedy must not send an
    /// operator to reinstall a driver that is working fine, and must name what actually has to
    /// happen. Asserted on the text because the text is the whole deliverable.
    #[test]
    fn the_contended_hint_never_tells_the_operator_to_reinstall_drivers() {
        let hint = PadCreateFault::IndexOwnedElsewhere.hint();
        assert!(
            !hint.contains("driver install"),
            "the contended hint must not repeat the driver-repair advice: {hint}"
        );
        assert!(hint.contains("already owned"), "{hint}");
        assert!(hint.contains("Restart-Service"), "{hint}");
    }

    /// A named fault must not turn the create into a permanent latch — that latch is the exact
    /// `broken: bool` behaviour [`PadGate`] was built to remove, and the field case healed by
    /// itself precisely because the retry was still running when the owning service restarted.
    #[test]
    fn a_contended_create_still_backs_off_and_retries_rather_than_latching() {
        let mut s = slots();
        let contended = || {
            Err(anyhow::Error::msg("Zugriff verweigert")
                .context(PadCreateFault::IndexOwnedElsewhere))
        };
        assert!(!s.ensure(0, |_| contended()));
        assert_eq!(s.live(), 0);
        // Backed off, not latched: once the window elapses the closure runs again. `ensure` reads
        // the wall clock, so clear the backoff directly rather than sleeping through it — the
        // window's own arithmetic is pinned by `pad_gate`'s tests.
        s.gate.on_success();
        let mut ran = false;
        assert!(s.ensure(0, |i| {
            ran = true;
            Ok(i as u32)
        }));
        assert!(ran, "the create was never re-attempted");
        assert_eq!(s.live(), 1);
    }

    #[test]
    fn live_counts_built_pads_not_slots() {
        let mut s = slots();
        assert_eq!(s.live(), 0, "an empty table has no pads, only slots");
        assert!(s.ensure(0, |_| Ok(0)));
        assert!(s.ensure(4, |_| Ok(4)));
        assert_eq!(s.live(), 2);
        let t0 = Instant::now();
        s.sweep_at(0, t0);
        s.sweep_at(0, t0 + SWEEP_GRACE);
        assert_eq!(s.live(), 0);
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
