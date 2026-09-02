//! Slot-family RFI recovery **policy** for the three backends that answer a loss
//! with a re-reference to a known-good older frame instead of an IDR: native AMF
//! (user-LTR bitfield), native QSV (`mfxExtRefListCtrl` LTR), and Vulkan Video
//! (app-owned DPB slot table).
//!
//! Policy only. Callers feed **currently-trusted** `(slot, wire)` pairs and apply
//! the returned taints through their own persistent marker. How a force is applied
//! and how distrust is stored (AMF clears the mirror slot, QSV sets `ltr_tainted`,
//! Vulkan blanks `slot_wire` to `-1`) stays in the backend; the caller-side filter
//! is what makes those schemes equivalent under one pure function.
//!
//! Decline is also the backend's: AMF/QSV drop an un-consumed `pending_force`;
//! Vulkan leaves `pending_loss` armed so frame-build can re-pick or force an IDR.
//! Do not harmonize them here. NVENC's range policy is
//! [`super::nvenc_core::plan_range_recovery`].

pub(super) struct SlotPlan {
    /// Slots with `wire >= loss_first`. Persist the distrust in the backend marker:
    /// without it, the next loss treats these as pre-loss anchors.
    pub(super) tainted: u32,
    /// Newest trusted `(slot, wire)` strictly older than the loss. `None` → the
    /// caller declines and recovers via its keyframe path.
    pub(super) anchor: Option<(usize, i64)>,
}

/// Taint and pick from one snapshot of currently-trusted `(slot, wire)` pairs
/// (caller already dropped previously-distrusted entries). `wire >= loss_first`
/// taints; `wire < loss_first` is the only eligible anchor, so this call cannot
/// pick a slot it just tainted.
pub(super) fn plan_slot_recovery(refs: &[(usize, i64)], loss_first: i64) -> SlotPlan {
    // Callers gate `first < 0` before they get here; `-1`/`None` sentinels are
    // "untrusted". Plain `assert`: `--release` lint runs, and a compiled-out
    // check would drop taints instead of failing.
    assert!(
        loss_first >= 0,
        "loss_first must be validity-gated by the caller"
    );
    let mut tainted = 0u32;
    for &(slot, wire) in refs {
        if wire >= loss_first {
            assert!(slot < 32, "slot table exceeds the u32 taint mask");
            tainted |= 1 << slot;
        }
    }
    SlotPlan {
        tainted,
        anchor: pick_anchor(refs, loss_first),
    }
}

/// Newest trusted `wire` strictly older than the loss. Ties keep the first
/// `refs` entry (callers feed ascending slot order; the backends used `>`).
/// Vulkan re-picks at frame-build against the table as it stands then.
pub(super) fn pick_anchor(refs: &[(usize, i64)], loss_first: i64) -> Option<(usize, i64)> {
    let mut best: Option<(usize, i64)> = None;
    for &(slot, wire) in refs {
        if wire < loss_first && best.is_none_or(|(_, b)| wire > b) {
            best = Some((slot, wire));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::{pick_anchor, plan_slot_recovery};

    fn view(wires: &[i64]) -> Vec<(usize, i64)> {
        wires
            .iter()
            .enumerate()
            .filter_map(|(s, &w)| (w >= 0).then_some((s, w)))
            .collect()
    }

    fn apply(wires: &mut [i64], tainted: u32) {
        for (s, w) in wires.iter_mut().enumerate() {
            if tainted & (1 << s) != 0 {
                *w = -1;
            }
        }
    }

    #[test]
    fn picks_newest_pre_loss() {
        let wires = [8i64, 9, 10, 11, 12, 5, 6, 7];
        assert_eq!(pick_anchor(&view(&wires), 9), Some((0, 8)));
        assert_eq!(pick_anchor(&view(&wires), 5), None);
        assert_eq!(pick_anchor(&view(&[-1, 3, -1, 4]), 5), Some((3, 4)));
        assert_eq!(pick_anchor(&view(&[-1; 8]), 5), None);
        // `wire == loss_first` is inside the corrupt window: strictly older only.
        assert_eq!(pick_anchor(&view(&[9, 8]), 9), Some((1, 8)));
        // Tie keeps the first `refs` entry — the backends used `>`, not `>=`.
        assert_eq!(pick_anchor(&[(2, 7), (5, 7)], 9), Some((2, 7)));
        assert_eq!(pick_anchor(&[], 9), None);
    }

    /// A slot from an earlier unrepaired loss must not become a later loss's
    /// "known-good" anchor: without persisted distrust it is still resident and
    /// below the second start, so the picker would serve it as `recovery_anchor`.
    #[test]
    fn taint_sweep_excludes_slots_from_an_earlier_loss() {
        // Loss at 4 taints 4..7; a second report at 6 still sees them resident.
        let tainted_wires = [4i64, 5, 6, 7];

        let unswept = [0i64, 1, 2, 3, 4, 5, 6, 7];
        let (_, picked_wire) = pick_anchor(&view(&unswept), 6).expect("unswept picks something");
        assert!(
            tainted_wires.contains(&picked_wire),
            "precondition: without the sweep the anchor comes from the earlier loss window"
        );

        let mut wires = unswept;
        let plan = plan_slot_recovery(&view(&wires), 4);
        assert_eq!(plan.tainted, 0b1111_0000);
        assert_eq!(plan.anchor, Some((3, 3)));
        apply(&mut wires, plan.tainted);
        assert_eq!(wires, [0, 1, 2, 3, -1, -1, -1, -1]);
        let (slot, wire) = pick_anchor(&view(&wires), 6).expect("clean wires remain");
        assert_eq!((slot, wire), (3, 3), "newest clean survivor is wire 3");

        // Post-recovery refill: a later loss at 10 may anchor on 9; do not over-taint.
        wires[4] = 8;
        wires[5] = 9;
        wires[6] = 10;
        wires[7] = 11;
        let plan = plan_slot_recovery(&view(&wires), 10);
        assert_eq!(plan.anchor, Some((5, 9)), "wire 9 is post-recovery, clean");
        apply(&mut wires, plan.tainted);

        let mut all = [5i64, 6, 7, 8, 9, 10, 11, 12];
        let plan = plan_slot_recovery(&view(&all), 5);
        assert_eq!(plan.tainted, 0b1111_1111);
        assert_eq!(plan.anchor, None);
        apply(&mut all, plan.tainted);
        assert_eq!(pick_anchor(&view(&all), 5), None);
    }

    /// Wholesale withdrawal (`Encoder::distrust_references`) has no loss range,
    /// so every resident ref is dropped. The next pick must decline rather than
    /// serve an anchor over unrepaired damage.
    #[test]
    fn distrusting_every_reference_makes_the_next_anchor_pick_decline() {
        let mut wires = [4i64, 5, 6, 7, -1, -1, -1, -1];
        assert_eq!(
            pick_anchor(&view(&wires), 9),
            Some((3, 7)),
            "precondition: this table would happily anchor"
        );

        apply(&mut wires, u32::MAX);
        assert_eq!(
            pick_anchor(&view(&wires), 9),
            None,
            "every reference withdrawn → no anchor, caller falls through to its keyframe path"
        );
        // Persisted: any later loss, not only this one, still finds nothing.
        assert_eq!(pick_anchor(&view(&wires), 100), None);
    }

    /// Withdrawal is per-slot, not the session: a slot re-marked with a fresh
    /// frame (after the IDR flush that emptied the table) is a legal anchor again.
    #[test]
    fn a_re_marked_slot_restores_anchor_trust_after_a_full_withdrawal() {
        let mut wires = [4i64, 5, 6, 7, -1, -1, -1, -1];
        apply(&mut wires, u32::MAX);
        assert_eq!(pick_anchor(&view(&wires), 20), None);

        wires[0] = 14;
        wires[1] = 15;
        assert_eq!(
            pick_anchor(&view(&wires), 20),
            Some((1, 15)),
            "a re-marked slot is trusted again — the suppression is a few frames, not the session"
        );
    }
}
