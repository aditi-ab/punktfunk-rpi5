//! The slot-family RFI (reference-frame invalidation) recovery **policy**, shared by the three
//! backends that answer a loss with a re-reference to a known-good older frame instead of an IDR:
//! native AMF (user-LTR bitfield), native QSV (`mfxExtRefListCtrl` LTR) and Vulkan Video (the
//! app-owned DPB slot table). One decision, three mechanisms — the policy lived as three
//! hand-copies and had already diverged once (the fecbec2d taint sweep reached AMF/QSV a commit
//! before the Vulkan backend was carved out, and Vulkan shipped without it until a later fix).
//! The NVENC twins' *range* policy is the other half of WP7.2, in
//! [`super::nvenc_core::plan_range_recovery`].
//!
//! Policy only. Every mechanism — how a force is applied, how distrust is *persisted* (AMF clears
//! its mirror slot to `None`, QSV sets a separate `ltr_tainted` flag because its mirror must keep
//! naming the slot for the RejectedRefList, Vulkan blanks `slot_wire` to `-1` while `slot_poc`
//! keeps the picture resident for the RPS) — stays in its backend. Callers feed their
//! **currently-trusted** references and apply the returned taints through their own marker; that
//! caller-side filter is exactly what makes the three persistence schemes equivalent under one
//! pure function.
//!
//! The decline arm is also the backend's: AMF/QSV clear an un-consumed `pending_force` (the sweep
//! may have emptied the slot it points at), while Vulkan deliberately leaves `pending_loss` armed
//! (a stale arm is re-resolved at frame-build, where a failed re-pick forces the IDR that heals
//! the stream). Do not harmonize them here.

/// One loss event's recovery decision over a slot table: which trusted references become
/// untrustworthy, and which one anchors the recovery.
pub(super) struct SlotPlan {
    /// Bitmask of slots whose reference was encoded **at or after** the loss start — inside the
    /// client's corrupt window, so the client either never received it or decoded it against a
    /// broken chain. Serving one as "known-good" on a LATER loss ships corruption tagged as the
    /// recovery anchor; the backend must record the distrust in its own persistent marker, because
    /// these wires would otherwise look like valid pre-loss anchors to the next loss event.
    pub(super) tainted: u32,
    /// The recovery anchor: the newest trusted reference **strictly older** than the loss —
    /// `(slot, wire)` — i.e. the most recent picture the client still holds intact, so
    /// re-referencing it costs the smallest residual. `None`: every candidate is inside or after
    /// the corrupt window — the caller declines and its (coalesced) keyframe path recovers.
    pub(super) anchor: Option<(usize, i64)>,
}

/// Plan the recovery for a loss starting at wire frame `loss_first`, over the backend's
/// currently-trusted references (`(slot, wire)`; previously-distrusted entries must already be
/// filtered out by the caller — see the module doc). Sweep and pick are one call so the anchor is
/// chosen from the same snapshot the taints are computed from, by construction: the anchor
/// delegates to [`pick_anchor`], and `wire >= loss_first` (taint) and `wire < loss_first`
/// (anchor) are disjoint, so a slot tainted by this call can never be this call's anchor.
pub(super) fn plan_slot_recovery(refs: &[(usize, i64)], loss_first: i64) -> SlotPlan {
    // The callers' validity gate (`first < 0 → decline`) is what makes their sentinel filters
    // (-1 / `None`) exact views of "trusted"; this assert keeps the contract visible from inside
    // the extracted code. Plain assert: the lint legs run --release, and a compiled-out check
    // here would silently drop taints instead of failing loudly.
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

/// The pick half alone: newest trusted reference strictly older than the loss. Ties break to the
/// first entry in `refs` (every caller feeds ascending slot order, so the lowest slot wins —
/// the strict `>` all three backends used). Standalone because Vulkan re-picks at frame-build
/// time: its arm carries the loss start, not the slot, so the slot is resolved against the table
/// as it stands when the recovery frame is actually encoded.
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

    /// Adapt a raw slot table (the Vulkan `slot_wire` shape: `-1` = empty) into the trusted view
    /// the policy takes — the same filter the backend adapters apply.
    fn view(wires: &[i64]) -> Vec<(usize, i64)> {
        wires
            .iter()
            .enumerate()
            .filter_map(|(s, &w)| (w >= 0).then_some((s, w)))
            .collect()
    }

    /// Apply a plan's taints the way the Vulkan adapter does (blank the wire) — the persistence
    /// half the pure fn hands back to the caller.
    fn apply(wires: &mut [i64], tainted: u32) {
        for (s, w) in wires.iter_mut().enumerate() {
            if tainted & (1 << s) != 0 {
                *w = -1;
            }
        }
    }

    /// The RFI anchor picker: newest resident wire strictly older than the loss; empty/newer
    /// slots never qualify. (Migrated 1:1 from `vulkan_video.rs`'s `pick_recovery_slot` tests —
    /// same vectors, now `(slot, wire)`-valued.)
    #[test]
    fn picks_newest_pre_loss() {
        // slots hold wires 5..12 (ring position arbitrary); loss starts at 9 → anchor = wire 8.
        let wires = [8i64, 9, 10, 11, 12, 5, 6, 7];
        assert_eq!(pick_anchor(&view(&wires), 9), Some((0, 8)));
        // loss older than everything resident → no anchor (caller keyframes).
        assert_eq!(pick_anchor(&view(&wires), 5), None);
        // empty slots (-1) are skipped by the view filter and never anchor.
        assert_eq!(pick_anchor(&view(&[-1, 3, -1, 4]), 5), Some((3, 4)));
        assert_eq!(pick_anchor(&view(&[-1; 8]), 5), None);
        // wire == loss_first is INSIDE the corrupt window: strictly-older only.
        assert_eq!(pick_anchor(&view(&[9, 8]), 9), Some((1, 8)));
        // tie on the wire → first entry (= lowest slot) wins, the strict `>` all three backends
        // used.
        assert_eq!(pick_anchor(&[(2, 7), (5, 7)], 9), Some((2, 7)));
        // empty view.
        assert_eq!(pick_anchor(&[], 9), None);
    }

    /// The taint sweep (fecbec2d's fix): a slot encoded inside an EARLIER, still unrepaired loss
    /// window must not become the "known-good" anchor of a LATER loss. Without persisted
    /// distrust the picker accepts it — it is resident and its wire is below the second loss
    /// start — and the frame ships tagged `recovery_anchor`, lifting the client's freeze onto a
    /// reference it never decoded. (Migrated 1:1 from `vulkan_video.rs`; the hand-replicated
    /// sweep is now `plan_slot_recovery` itself.)
    #[test]
    fn taint_sweep_excludes_slots_from_an_earlier_loss() {
        // Slots hold wires 0..7. Loss 1 starts at wire 4, so wires 4..7 are undecodable at the
        // client. A second loss report arrives at wire 6 while they are all still resident.
        let tainted_wires = [4i64, 5, 6, 7];

        // WITHOUT the sweep this is the bug: the newest wire below 6 is wire 5 — squarely inside
        // loss 1's unrepaired window — and it would be served as the "known-good" anchor.
        let unswept = [0i64, 1, 2, 3, 4, 5, 6, 7];
        let (_, picked_wire) = pick_anchor(&view(&unswept), 6).expect("unswept picks something");
        assert!(
            tainted_wires.contains(&picked_wire),
            "precondition: without the sweep the anchor comes from the earlier loss window"
        );

        // WITH the plan, loss 1 taints 4..7 (and anchors on wire 3 — never a tainted wire, by
        // the disjoint predicates), so loss 2 can only reach genuinely clean wires.
        let mut wires = unswept;
        let plan = plan_slot_recovery(&view(&wires), 4);
        assert_eq!(plan.tainted, 0b1111_0000);
        assert_eq!(plan.anchor, Some((3, 3)));
        apply(&mut wires, plan.tainted);
        assert_eq!(wires, [0, 1, 2, 3, -1, -1, -1, -1]);
        let (slot, wire) = pick_anchor(&view(&wires), 6).expect("clean wires remain");
        assert_eq!((slot, wire), (3, 3), "newest clean survivor is wire 3");

        // Encoding resumes after recovery; wires 8..11 refill the swept slots and are clean. A
        // later loss at wire 10 legitimately anchors on wire 9 — the sweep must not over-reject.
        wires[4] = 8;
        wires[5] = 9;
        wires[6] = 10;
        wires[7] = 11;
        let plan = plan_slot_recovery(&view(&wires), 10);
        assert_eq!(plan.anchor, Some((5, 9)), "wire 9 is post-recovery, clean");
        apply(&mut wires, plan.tainted);

        // A loss covering every live wire leaves nothing clean → decline, caller serves an IDR.
        let mut all = [5i64, 6, 7, 8, 9, 10, 11, 12];
        let plan = plan_slot_recovery(&view(&all), 5);
        assert_eq!(plan.tainted, 0b1111_1111);
        assert_eq!(plan.anchor, None);
        apply(&mut all, plan.tainted);
        assert_eq!(pick_anchor(&view(&all), 5), None);
    }
}
