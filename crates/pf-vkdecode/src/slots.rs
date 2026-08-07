//! The hardware DPB slot ledger: [`pf_bitstream::h264::PicId`]s mapped to the slot
//! indices a Vulkan Video session binds DPB images by.
//!
//! Division of labour: pf-bitstream's DPB runs the 8.2.5/C.4.5.3 processes and
//! DECIDES which pictures live and die — this map only translates its verdicts into
//! stable slot indices. It therefore never evicts on its own: running out of slots is
//! an error ([`SlotError::Full`]), because it can only mean removals were missed, and
//! a silent eviction would hide that bug behind corrupted output.

use pf_bitstream::h264::DpbUpdate;
use pf_bitstream::h264::PicId;
use tracing::trace;

/// The H.264 slot ceiling: 16 reference frames plus the picture being decoded.
const MAX_SLOTS: usize = 17;

/// What went wrong with a slot operation. Both variants are caller bugs, not stream
/// conditions — pf-bitstream degrades stream damage to warnings long before here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// No free slot. The map is sized to `max_dpb_frames + 1`, which the planner's
    /// DPB never exceeds; overflow means this map missed `removed` entries.
    Full { capacity: usize },
    /// The id already holds a slot; ids are per-picture and never re-assigned.
    AlreadyAssigned { id: PicId, slot: u8 },
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Full { capacity } => {
                write!(
                    f,
                    "all {capacity} DPB slots are held — removals were missed"
                )
            }
            SlotError::AlreadyAssigned { id, slot } => {
                write!(f, "picture {id} already holds slot {slot}")
            }
        }
    }
}

impl std::error::Error for SlotError {}

/// The slot ledger. One per decode session; feed it every [`DpbUpdate`] in decode
/// order (via [`Self::apply`] or `plan_to_vk`, which applies internally).
///
/// Invariants (unit-tested):
/// - a [`PicId`] keeps its slot from [`Self::assign`] until [`Self::release`];
/// - a slot is reused only after its holder is released;
/// - assigning past capacity errors instead of evicting.
///
/// Slots are pure planner bookkeeping: consumers never hold a SLOT (the decoder's
/// picture pool decouples IMAGES from slots — a re-activated slot binds a fresh
/// free image, so a delivered picture's image is never a decode target while the
/// consumer reads it).
#[derive(Debug, Clone)]
pub struct SlotMap {
    /// `slots[i]` holds the id bound to slot `i`, `None` while the slot is free.
    slots: Vec<Option<PicId>>,
}

impl SlotMap {
    /// Sized from [`pf_bitstream::h264::PicturePlan::max_dpb_frames`] plus one for
    /// the picture being decoded (its setup slot coexists with a full reference
    /// window).
    ///
    /// pf-bitstream's envelope gate rejects any SPS asking for a DPB deeper than the
    /// spec's 16 frames before a plan exists, so a larger request here is a caller
    /// bug — debug-asserted, never silently clamped (a clamp would turn the bug into
    /// silent evictions later).
    pub fn new(max_dpb_frames: usize) -> Self {
        debug_assert!(
            max_dpb_frames < MAX_SLOTS,
            "a {max_dpb_frames}-frame DPB exceeds the H.264 ceiling pf-bitstream's \
             envelope gate enforces"
        );
        Self {
            slots: vec![None; max_dpb_frames + 1],
        }
    }

    /// Total slot count (fixed at construction).
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Slots currently held.
    pub fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// The held slots as `(slot, id)` pairs, in slot order — WP-B walks this to
    /// build `VkVideoReferenceSlotInfoKHR` bindings and to map slots back to their
    /// images.
    pub fn held(&self) -> impl Iterator<Item = (u8, PicId)> + '_ {
        self.slots
            .iter()
            .enumerate()
            // The envelope-gated capacity (<= 17) keeps every index within u8.
            .filter_map(|(index, slot)| slot.map(|id| (index as u8, id)))
    }

    /// Bind `id` to the lowest free slot.
    pub fn assign(&mut self, id: PicId) -> Result<u8, SlotError> {
        if let Some(slot) = self.slot_of(id) {
            return Err(SlotError::AlreadyAssigned { id, slot });
        }
        let free = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(SlotError::Full {
                capacity: self.slots.len(),
            })?;
        self.slots[free] = Some(id);
        // The envelope-gated capacity (<= 17) keeps every index within u8.
        Ok(free as u8)
    }

    /// The slot `id` holds, if any.
    pub fn slot_of(&self, id: PicId) -> Option<u8> {
        self.slots
            .iter()
            .position(|slot| *slot == Some(id))
            // The envelope-gated capacity (<= 17) keeps every index within u8.
            .map(|index| index as u8)
    }

    /// Free `id`'s slot. Returns whether the id held one.
    ///
    /// Slot lifetime is DPB RESIDENCY: a picture holds its slot for exactly as long
    /// as the planner's DPB holds the picture — as a reference OR as a decoded
    /// picture awaiting output — and that residency ends only when a
    /// [`DpbUpdate::removed`] entry reports it. This method is that report's
    /// primitive: `plan_to_vk` and [`Self::apply`] call it with the planner's
    /// `removed` ids and nothing else may release a slot.
    ///
    /// Releasing is CPU-side bookkeeping (the slot becomes assignable to a later
    /// picture); keeping the released slot's IMAGE out of reuse until in-flight
    /// decodes complete is the backend's synchronization, not this ledger's.
    pub fn release(&mut self, id: PicId) -> bool {
        match self.slots.iter().position(|slot| *slot == Some(id)) {
            Some(index) => {
                self.slots[index] = None;
                true
            }
            None => false,
        }
    }

    /// Apply one [`DpbUpdate`]: release every `removed` id.
    ///
    /// `outputs` is deliberately ignored: output-readiness is display sequencing,
    /// not the end of DPB residency — a display-ready picture can still be a
    /// reference (its slot stays), and only its later `removed` entry frees the
    /// slot.
    pub fn apply(&mut self, update: &DpbUpdate) {
        for &id in &update.removed {
            if !self.release(id) {
                // Tolerated but never silent: reachable only when the caller skipped
                // feeding an AU's plan through this map.
                trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pic_id_keeps_its_slot_until_released_and_the_slot_is_then_reusable() {
        let mut slots = SlotMap::new(3); // capacity 4
        let s0 = slots.assign(10).unwrap();
        let s1 = slots.assign(11).unwrap();
        assert_ne!(s0, s1);

        // Stable across unrelated churn.
        assert_eq!(slots.slot_of(10), Some(s0));
        slots.release(11);
        assert_eq!(slots.slot_of(10), Some(s0));
        assert_eq!(slots.slot_of(11), None);

        // The freed slot is reusable; the held one is not.
        let s2 = slots.assign(12).unwrap();
        assert_eq!(s2, s1, "the lowest free slot is the released one");
        assert_eq!(slots.slot_of(10), Some(s0));
        assert_eq!(slots.active(), 2);
    }

    #[test]
    fn assigning_past_capacity_is_an_error_never_a_silent_eviction() {
        let mut slots = SlotMap::new(1); // capacity 2
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        assert_eq!(slots.assign(3), Err(SlotError::Full { capacity: 2 }));
        // The failed assign evicted nothing.
        assert_eq!(slots.slot_of(1), Some(0));
        assert_eq!(slots.slot_of(2), Some(1));
    }

    #[test]
    fn re_assigning_a_held_id_is_an_error_not_a_move() {
        let mut slots = SlotMap::new(2);
        let s = slots.assign(7).unwrap();
        assert_eq!(
            slots.assign(7),
            Err(SlotError::AlreadyAssigned { id: 7, slot: s })
        );
        assert_eq!(slots.active(), 1);
    }

    #[test]
    fn capacity_is_dpb_frames_plus_one_for_the_setup_slot() {
        assert_eq!(SlotMap::new(16).capacity(), 17);
        assert_eq!(SlotMap::new(4).capacity(), 5);
    }

    #[test]
    #[should_panic(expected = "envelope")]
    fn a_dpb_past_the_h264_ceiling_is_a_debug_panic_not_a_clamp() {
        // pf-bitstream's envelope gate makes this unreachable from a real stream;
        // reaching it means a caller bypassed the planner.
        let _ = SlotMap::new(17);
    }

    #[test]
    fn held_lists_slot_id_pairs_in_slot_order() {
        let mut slots = SlotMap::new(3);
        slots.assign(10).unwrap();
        slots.assign(11).unwrap();
        slots.assign(12).unwrap();
        slots.release(11);
        assert_eq!(slots.held().collect::<Vec<_>>(), vec![(0, 10), (2, 12)]);
    }

    #[test]
    fn apply_releases_removed_ids_and_ignores_outputs() {
        let mut slots = SlotMap::new(3);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        slots.apply(&DpbUpdate {
            stored: None,
            outputs: vec![1], // display-ready, still a reference: must keep its slot
            removed: vec![2],
        });
        assert_eq!(slots.slot_of(1), Some(0));
        assert_eq!(slots.slot_of(2), None);
    }

    #[test]
    fn a_hundred_synthetic_dpb_updates_churn_without_aliasing_a_slot() {
        // A sliding window of 4 references over 100 pictures: each id's slot must
        // stay fixed while it lives, and no two live ids may ever share a slot.
        let mut slots = SlotMap::new(4);
        let mut recorded: Vec<(PicId, u8)> = Vec::new();
        for id in 0u64..100 {
            let slot = slots.assign(id).unwrap();
            assert!(
                recorded.iter().all(|&(_, held)| held != slot),
                "assign handed out a slot a live picture still holds"
            );
            recorded.push((id, slot));

            let removed = if id >= 4 { vec![id - 4] } else { Vec::new() };
            slots.apply(&DpbUpdate {
                stored: Some(id),
                outputs: vec![id],
                removed: removed.clone(),
            });
            for gone in removed {
                recorded.retain(|&(held_id, _)| held_id != gone);
            }
            // Every live picture still holds exactly the slot it was assigned.
            for &(live, slot) in &recorded {
                assert_eq!(slots.slot_of(live), Some(slot));
            }
            assert!(slots.active() <= 5);
        }
    }
}
