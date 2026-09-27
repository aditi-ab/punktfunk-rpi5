//! Decoder machinery the H.264, H.265 and AV1 decoders share: the op ring,
//! pending/ready bookkeeping, slot ledgers, recovery latch, and coding scope.

use std::collections::BTreeMap;

use ash::vk;
use pf_bitstream::h264::ColourDescription;
use pf_bitstream::h264::DisplayCrop;
use pf_bitstream::h264::DpbUpdate;
use pf_bitstream::h264::PicId;
use tracing::debug;
use tracing::trace;

use super::DecodedVkFrame;
use super::VkDecodeError;
use super::DECODE_TIMEOUT_NS;
use crate::caps::DecodeProfile;
use crate::device::DecodeDevice;
use crate::images::PicturePool;
use crate::slots::SlotMap;

/// Query and command pools. Query slots cycle per submission (checked against
/// [`DecodedVkFrame::submission`]); command buffers cycle within the bitstream
/// ring's in-flight bound. This type owns and destroys the Vulkan objects.
///
/// `query_pool` is `None` without `queryResultStatusSupport`: recording a
/// RESULT_STATUS query is invalid there (RADV hangs the VCN ring). Verdicts
/// then fall back to timeline completion.
pub(crate) struct OpRing {
    device: ash::Device,
    pub(crate) query_pool: Option<vk::QueryPool>,
    pub(crate) query_count: u32,
    cmd_pool: vk::CommandPool,
    pub(crate) cmds: Vec<vk::CommandBuffer>,
}

impl OpRing {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        decode_profile: DecodeProfile,
        query_count: u32,
        cmd_count: u32,
    ) -> Result<Self, vk::Result> {
        let query_pool = if dev.result_status_queries() {
            let mut chain = decode_profile.chain();
            // SAFETY: fn contract. `chain` outlives the call, and the helper's
            // SIGNATURE — not a comment — is what keeps it immobile across it.
            Some(unsafe { Self::create_status_query_pool(dev, chain.wire(), query_count)? })
        } else {
            debug!(
                "decode family lacks queryResultStatusSupport — no per-op status \
                 queries on this driver (verdicts fall back to timeline completion)"
            );
            None
        };

        let destroy_query = |pool: Option<vk::QueryPool>| {
            if let Some(pool) = pool {
                // SAFETY: destroying the just-created query pool (unwind path).
                unsafe { dev.ash().destroy_query_pool(pool, None) };
            }
        };
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(dev.decode_qf())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: live device; unwind destroys the query pool on failure.
        let cmd_pool = match unsafe { dev.ash().create_command_pool(&pool_ci, None) } {
            Ok(p) => p,
            Err(e) => {
                destroy_query(query_pool);
                return Err(e);
            }
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(cmd_count);
        // SAFETY: live device + the pool created above; unwind destroys both pools
        // (destroying the command pool frees any allocated buffers).
        let cmds = match unsafe { dev.ash().allocate_command_buffers(&alloc) } {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: destroying the command pool created above.
                unsafe { dev.ash().destroy_command_pool(cmd_pool, None) };
                destroy_query(query_pool);
                return Err(e);
            }
        };
        Ok(Self {
            device: dev.ash().clone(),
            query_pool,
            query_count,
            cmd_pool,
            cmds,
        })
    }

    /// RESULT_STATUS query pool against `profile`.
    ///
    /// Split out so the profile borrow outlives `vkCreateQueryPool`.
    /// `push_next` would clobber the profile's own `p_next`; the chain is a raw
    /// `*const`, which ends the borrow the moment it is taken. A `&` parameter
    /// holds it for the whole call.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`DeviceHandles`] contract).
    unsafe fn create_status_query_pool(
        dev: &DecodeDevice,
        profile: &vk::VideoProfileInfoKHR<'_>,
        query_count: u32,
    ) -> Result<vk::QueryPool, vk::Result> {
        let mut query_ci = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::RESULT_STATUS_ONLY_KHR)
            .query_count(query_count);
        // Manual chain: `push_next` would clobber the profile's own `p_next`.
        query_ci.p_next = std::ptr::from_ref(profile).cast();
        // SAFETY: fn contract; `query_ci` roots the wired chain for the call, and
        // `profile` is borrowed for the whole of this body so the chain cannot move
        // out from under that pointer. The video profile chained in satisfies the
        // "same profile as the session" rule for queries used inside a coding scope.
        unsafe { dev.ash().create_query_pool(&query_ci, None) }
    }
}

impl Drop for OpRing {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device; the owning decoder
        // drains GPU work before dropping state. Destroying the command pool frees
        // its buffers; both destroys ignore NULL.
        unsafe {
            self.device.destroy_command_pool(self.cmd_pool, None);
            if let Some(pool) = self.query_pool {
                self.device.destroy_query_pool(pool, None);
            }
        }
    }
}

/// Decoded picture waiting for its output verdict, plus the fields its
/// [`DecodedVkFrame`] needs.
pub(crate) struct PendingPic {
    pub(crate) image: usize,
    pub(crate) submission: u64,
    pub(crate) query_slot: u32,
    pub(crate) timeline_value: u64,
    pub(crate) crop: DisplayCrop,
    pub(crate) colour: ColourDescription,
    pub(crate) poc: i32,
    pub(crate) is_idr: bool,
    /// Folded at plan time (the codec's counting unit is known only there).
    /// Display order is not decode order; see [`DecodedVkFrame::recovery`].
    pub(crate) recovery: crate::recovery::RecoveryMark,
    /// See [`DecodedVkFrame::decode_order`].
    pub(crate) decode_order: u64,
    /// From the plan at decode time; display order is not decode order. See
    /// [`DecodedVkFrame::references_clean`].
    pub(crate) references_clean: bool,
}

/// Retired generation's picture pool. Lives until release tokens return, then
/// the pool dies.
pub(crate) struct RetiredPool {
    pub(crate) generation: u64,
    pub(crate) pool: PicturePool,
}

/// Set when an AU fails after planning has already advanced. The next `decode`
/// consumes it and flushes to the next random-access point before planning
/// anything new.
///
/// Fail closed (H.265 shown): `RefPicSetStCurr*`/`LtCurr` are indices into this op's
/// reference array, so dropping a missing binding re-points every later index
/// at the wrong picture. By the time that error returns, `plan_to_vk_h265` has
/// already mutated [`SlotMap`] and coincide sync has cleared the setup image,
/// so planner and slots both claim picture N is resident with no image holding
/// it — every later AU then fails [`build_scope`] with `UnboundReferenceSlot`.
///
/// Recovery is a flush to the next IRAP/IDR/key (planner flush plus
/// [`reset_slot_bindings`]), not reference substitution. Own type so the
/// latch/consume cycle is testable without a device ([`crate::session::ResetArm`]).
#[derive(Debug, Default)]
pub(crate) struct RecoveryLatch(bool);

impl RecoveryLatch {
    /// Record that recovery is owed. Idempotent: two failures in a row still owe
    /// exactly one flush.
    pub(crate) fn latch(&mut self) {
        self.0 = true;
    }

    /// Whether recovery is owed, clearing the latch — once per failure run, not
    /// on every later decode.
    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    /// Whether recovery is owed, without consuming it (state snapshots).
    pub(crate) fn is_latched(&self) -> bool {
        self.0
    }
}

/// Build the delivered frame for one settled pending picture (pending → held).
///
/// [`DecodedVkFrame::format`] comes off the pool, which stamped it from the
/// `caps.output_format` its images were created with.
pub(crate) fn build_frame(
    pool: &mut PicturePool,
    coincide: bool,
    image_extent: vk::Extent2D,
    entry: &PendingPic,
    generation: u64,
) -> DecodedVkFrame {
    let format = pool.format;
    let picture = &mut pool.pictures[entry.image];
    picture.pending = false;
    picture.held += 1;
    DecodedVkFrame {
        image: picture.image,
        format,
        view: picture.view,
        plane_views: picture.plane_views,
        layer: picture.layer,
        layout: if coincide {
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR
        } else {
            vk::ImageLayout::VIDEO_DECODE_DST_KHR
        },
        coded_width: image_extent.width,
        coded_height: image_extent.height,
        crop: entry.crop,
        colour: entry.colour,
        semaphore: picture.semaphore,
        value: entry.timeline_value,
        poc: entry.poc,
        is_idr: entry.is_idr,
        recovery: entry.recovery,
        decode_order: entry.decode_order,
        references_clean: entry.references_clean,
        query_slot: entry.query_slot,
        submission: entry.submission,
        picture: entry.image as u32,
        generation,
    }
}

/// Split one [`DpbUpdate`]: `outputs` (bump order) become deliverable;
/// `removed` ids that never reached output are returned so their images are
/// freed. H.265 shares H.264's [`DpbUpdate`].
pub(crate) fn settle_dpb<F>(pending: &mut BTreeMap<PicId, F>, dpb: &DpbUpdate) -> (Vec<F>, Vec<F>) {
    settle_dpb_ids(pending, &dpb.outputs, &dpb.removed)
}

/// [`settle_dpb`] over the two id lists directly.
///
/// AV1 declares its own [`pf_bitstream::av1::DpbUpdate`] — structurally the
/// same, a distinct type. Settling at the id lists lets all three codecs share
/// one implementation.
pub(crate) fn settle_dpb_ids<F>(
    pending: &mut BTreeMap<PicId, F>,
    outputs: &[PicId],
    removed: &[PicId],
) -> (Vec<F>, Vec<F>) {
    let mut ready = Vec::new();
    for id in outputs {
        match pending.remove(id) {
            Some(entry) => ready.push(entry),
            // Ids planned before this decoder existed, or dropped across a
            // rebuild: display-order gaps, not errors.
            None => trace!(id, "output id without a pending picture"),
        }
    }
    let dropped = removed.iter().filter_map(|id| pending.remove(id)).collect();
    (ready, dropped)
}

/// Bounded timeline wait (no-op for the never-signalled value 0).
///
/// # Safety
///
/// `device` is live and `semaphore` is a live timeline semaphore on it.
pub(crate) unsafe fn wait_timeline(
    device: &ash::Device,
    semaphore: vk::Semaphore,
    value: u64,
    what: &'static str,
) -> Result<(), VkDecodeError> {
    if value == 0 {
        return Ok(());
    }
    let semaphores = [semaphore];
    let values = [value];
    let info = vk::SemaphoreWaitInfo::default()
        .semaphores(&semaphores)
        .values(&values);
    // SAFETY: fn contract; the info arrays are locals outliving the call.
    match unsafe { device.wait_semaphores(&info, DECODE_TIMEOUT_NS) } {
        Ok(()) => Ok(()),
        Err(vk::Result::TIMEOUT) => Err(VkDecodeError::Timeout(what)),
        Err(e) => Err(VkDecodeError::from(e)),
    }
}

/// Empty the three per-slot ledgers a recovery resets: DPB residency,
/// slot→picture bindings, and cached per-slot reference info. Returns the pool
/// picture indices the cleared bindings were pinning, for the caller to unbind.
/// Pure over the ledgers so recovery is testable without a device.
///
/// All three empty together: leftover reference info would let [`build_scope`]
/// bind a slot the planner no longer knows. Generic over the cached Std type.
pub(crate) fn reset_slot_bindings<S>(
    slots: &mut SlotMap,
    slot_image: &mut [Option<usize>],
    slot_refs: &mut [Option<S>],
) -> Vec<usize> {
    // `release` is the only way a slot is freed ([`SlotMap`]); collect because
    // `held` borrows the map the releases mutate.
    for (_slot, id) in slots.held().collect::<Vec<_>>() {
        slots.release(id);
    }
    let unbound = slot_image.iter_mut().filter_map(Option::take).collect();
    for cached in slot_refs.iter_mut() {
        *cached = None;
    }
    unbound
}

/// Coincide: unbind slots the ledger no longer holds, and the setup slot's
/// previous image, before it binds fresh. Pictures stay pending/held on those
/// flags ([`crate::images`]). A referenced slot must still bind after this.
pub(crate) fn sync_slot_bindings(
    slots: &SlotMap,
    slot_image: &mut [Option<usize>],
    setup_slot: u8,
) -> Vec<usize> {
    let mut held = vec![false; slot_image.len()];
    for (slot, _id) in slots.held() {
        held[usize::from(slot)] = true;
    }
    let setup = usize::from(setup_slot);
    let mut unbound = Vec::new();
    for (slot, binding) in slot_image.iter_mut().enumerate() {
        if binding.is_some() && (!held[slot] || slot == setup) {
            unbound.extend(binding.take());
        }
    }
    unbound
}

/// One bound-slot list entry: DPB slot index (`-1` for the setup activation),
/// picture resource view, and that slot's codec reference info.
/// No derived equality: the Std bindgen struct has none; tests compare fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScopeEntry<S> {
    pub(crate) slot_index: i32,
    pub(crate) view: vk::ImageView,
    pub(crate) std: S,
}

/// One of this AU's references as [`build_scope`] sees it: a DPB slot and the
/// codec reference info to bind with it. All three codecs share the builder so
/// "refuse to guess" has one implementation; AV1 checks its reference names
/// against the result afterwards.
pub(crate) trait ScopeRef {
    type Std: Copy;
    fn slot(&self) -> u8;
    fn std(&self) -> Self::Std;
}

/// [`build_scope`]'s answer: bound-slot list, and how many leading entries are
/// this AU's own references (the prefix the decode op takes as its reference
/// array — ordering note in `build_scope`).
pub(crate) type Scope<R> = (Vec<ScopeEntry<<R as ScopeRef>::Std>>, usize);

/// Build the coding scope's bound-slot list and how many leading entries are
/// this AU's references.
///
/// Layout: (1) every `refs` entry in order — the decode op takes this prefix;
/// (2) every other still-held slot, so its association survives the scope;
/// (3) the setup slot as the activation entry, slot index `-1`.
///
/// A reference whose slot binds no image is a hard error, never a skip.
/// H.265 `RefPicSetStCurr*`/`LtCurr` name DPB slots, and every named slot is
/// one of `refs` ([`crate::pic_h265`]); dropping an entry leaves hardware a
/// named slot this op never bound.
///
/// `reference_count` is captured the instant the `refs` loop ends, before the
/// held-slot pass appends. The decode op takes `scope[..reference_count]` as
/// its reference list; a count after the second pass could hand it a
/// still-held slot this AU does not reference.
pub(crate) fn build_scope<R: ScopeRef>(
    refs: &[R],
    held_slots: impl Iterator<Item = u8>,
    setup_slot: u8,
    setup_view: vk::ImageView,
    setup_ref: R::Std,
    slot_refs: &[Option<R::Std>],
    view_of: impl Fn(u8) -> Option<vk::ImageView>,
) -> Result<Scope<R>, VkDecodeError> {
    let mut scope: Vec<ScopeEntry<R::Std>> = Vec::with_capacity(refs.len() + slot_refs.len() + 1);
    for r in refs {
        match view_of(r.slot()) {
            Some(view) => scope.push(ScopeEntry {
                slot_index: i32::from(r.slot()),
                view,
                std: r.std(),
            }),
            None => return Err(VkDecodeError::UnboundReferenceSlot { slot: r.slot() }),
        }
    }
    let reference_count = scope.len();
    for slot in held_slots {
        if slot == setup_slot || refs.iter().any(|r| r.slot() == slot) {
            continue;
        }
        match (
            slot_refs.get(usize::from(slot)).copied().flatten(),
            view_of(slot),
        ) {
            (Some(std), Some(view)) => scope.push(ScopeEntry {
                slot_index: i32::from(slot),
                view,
                std,
            }),
            // Every held slot was a setup slot once; leave unbound rather than fake.
            _ => trace!(
                slot,
                "held slot without reference info/binding — left unbound"
            ),
        }
    }
    scope.push(ScopeEntry {
        slot_index: -1,
        view: setup_view,
        std: setup_ref,
    });
    Ok((scope, reference_count))
}

#[cfg(test)]
mod tests {
    use ash::vk::native as hh;
    use ash::vk::Handle as _;

    use super::*;
    use crate::pic_h265::VkRefH265;

    /// Reference-info carrying the two fields the assertions read.
    fn std_ref(poc: i32, long_term: bool) -> hh::StdVideoDecodeH265ReferenceInfo {
        // SAFETY: StdVideoDecodeH265ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word and one integer; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
        std.PicOrderCntVal = poc;
        std.flags
            .set_used_for_long_term_reference(u32::from(long_term));
        std
    }

    fn vk_ref(slot: u8, poc: i32, long_term: bool) -> VkRefH265 {
        VkRefH265 {
            slot,
            std: std_ref(poc, long_term),
            id: u64::from(slot) + 100,
        }
    }

    /// Distinguishable fake view per slot. Never dereferenced; the scope only
    /// carries handles.
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    #[test]
    fn the_scopes_leading_entries_are_the_refs_in_plan_order() {
        // Plan refs are in RPS set order (StCurrBefore, StCurrAfter, LtCurr),
        // not slot order. Std index arrays point at positions in that order, so
        // the scope must not sort or dedup them.
        let refs = vec![
            vk_ref(5, 40, false),
            vk_ref(1, 60, false),
            vk_ref(3, 8, true),
        ];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3, 5, 7].into_iter(),
            2,
            fake_view(2),
            std_ref(50, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 3, "exactly this AU's references lead");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![5, 1, 3],
            "plan order, not slot order — the RPS index arrays depend on it"
        );
        for (entry, r) in scope.iter().zip(&refs) {
            assert_eq!(entry.view, fake_view(r.slot));
            assert_eq!(entry.std.PicOrderCntVal, r.std.PicOrderCntVal);
            assert_eq!(
                entry.std.flags.used_for_long_term_reference(),
                r.std.flags.used_for_long_term_reference(),
                "the long-term marking rides with the binding"
            );
        }

        assert_eq!(scope[3].slot_index, 7);
        let last = scope.last().unwrap();
        assert_eq!(
            last.slot_index, -1,
            "the setup slot binds its resource without a current association"
        );
        assert_eq!(last.view, fake_view(2));
        assert_eq!(last.std.PicOrderCntVal, 50);
        assert_eq!(
            scope.len(),
            5,
            "3 refs + 1 other held slot + the activation"
        );
    }

    #[test]
    fn a_reference_slot_without_a_bound_image_fails_the_whole_op() {
        // Compacting past it would shift every later RefPicSetStCurr* index
        // onto the wrong picture. Fail closed.
        let refs = vec![vk_ref(4, 10, false), vk_ref(6, 20, false)];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let err = build_scope(
            &refs,
            [4u8, 6].into_iter(),
            0,
            fake_view(0),
            std_ref(30, false),
            &slot_refs,
            |slot| (slot != 6).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 6 }),
            "{err}"
        );
    }

    #[test]
    fn held_slots_are_bound_once_and_the_setup_slot_never_twice() {
        // Slot 3 is both a reference and still held; slot 2 is setup and also
        // held. Neither may appear twice: a duplicate slot index in one coding
        // scope is invalid, and a second entry for a reference would also
        // break the index arrays.
        let refs = vec![vk_ref(3, 12, false)];
        let slot_refs = vec![Some(std_ref(99, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 2, 3].into_iter(),
            2,
            fake_view(2),
            std_ref(24, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 1);
        let indices: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
        assert_eq!(indices, vec![3, 1, -1]);
        assert_eq!(
            indices.iter().filter(|&&i| i == 3).count(),
            1,
            "a referenced slot is bound exactly once"
        );
        assert!(
            !indices.contains(&2),
            "the setup slot is bound only as the -1 activation entry"
        );
    }

    #[test]
    fn a_held_slot_with_no_cached_reference_info_is_left_unbound_not_faked() {
        // Only reachable if a slot was never a setup slot on this session.
        // Drop it rather than bind zeroed reference info (POC 0, short-term).
        let refs: Vec<VkRefH265> = Vec::new();
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> = vec![None; 4];
        slot_refs[1] = Some(std_ref(7, false));
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3].into_iter(),
            0,
            fake_view(0),
            std_ref(9, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![1, -1],
            "slot 3 had no cached info and is simply not bound"
        );
    }

    #[test]
    fn a_post_mutation_failure_wedges_every_later_au_until_the_ledgers_are_reset() {
        // AU planned, slot assigned, setup image unbound, then decode failed.
        // Planner and SlotMap still claim the picture is resident.
        let mut slots = SlotMap::new(3);
        slots.assign(100).unwrap(); // slot 0, bound
        slots.assign(200).unwrap(); // slot 1, bound
        slots.assign(300).unwrap(); // slot 2, this AU's setup — binding cleared
        let mut slot_image: Vec<Option<usize>> = vec![Some(7), Some(8), None, None];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> =
            vec![Some(std_ref(10, false)); 4];

        // Later AUs that reference slot 2 fail closed (RPS index arrays).
        let err = build_scope(
            &[vk_ref(2, 30, false)],
            [0u8, 1, 2].into_iter(),
            0,
            fake_view(0),
            std_ref(40, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 2 }),
            "{err}"
        );

        let unbound = reset_slot_bindings(&mut slots, &mut slot_image, &mut slot_refs);
        assert_eq!(
            unbound,
            vec![7, 8],
            "the pool pictures the stale bindings pinned go back on the free list"
        );
        assert_eq!(slots.active(), 0, "no picture is DPB-resident any more");
        assert_eq!(
            slots.capacity(),
            4,
            "capacity survives — no session rebuild"
        );
        assert!(slot_image.iter().all(Option::is_none));
        assert!(
            slot_refs.iter().all(Option::is_none),
            "cached reference info goes too, or build_scope could bind a slot the \
             planner no longer knows about"
        );

        let setup_slot = slots.assign(400).unwrap();
        assert_eq!(setup_slot, 0, "the freed slots are assignable again");
        slot_image[usize::from(setup_slot)] = Some(9);
        // Empty slice needs its element type named: `build_scope` is generic
        // over the codecs' reference types.
        let no_refs: [VkRefH265; 0] = [];
        let (scope, reference_count) = build_scope(
            &no_refs,
            slots.held().map(|(slot, _id)| slot),
            setup_slot,
            fake_view(setup_slot),
            std_ref(0, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![-1],
            "only the setup activation entry — the stream is decoding again"
        );
    }

    #[test]
    fn the_recovery_latch_is_owed_once_and_consumed_by_exactly_one_decode() {
        // Two failures in a row still owe one flush; the decode that performs
        // it clears the debt. Otherwise every later decode would re-flush and
        // the stream could never build a DPB again.
        let mut latch = RecoveryLatch::default();
        assert!(!latch.is_latched(), "a fresh decoder owes nothing");
        assert!(!latch.take());

        latch.latch();
        latch.latch();
        assert!(
            latch.is_latched(),
            "visible in debug_snapshot before it runs"
        );
        assert!(latch.take(), "the next decode recovers");
        assert!(!latch.is_latched());
        assert!(!latch.take(), "and the one after that just decodes");
    }

    #[test]
    fn settle_dpb_readies_outputs_in_order_and_returns_never_output_removals() {
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(1, 100);
        pending.insert(2, 200);
        pending.insert(3, 300);

        // 1 outputs and is removed (normal bump). 2 is removed without output
        // (`no_output_of_prior_pics`): free its image, do not leak in the map.
        let update = DpbUpdate {
            stored: Some(3),
            outputs: vec![1],
            removed: vec![1, 2],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![100]);
        assert_eq!(dropped, vec![200]);
        assert_eq!(
            pending.keys().copied().collect::<Vec<_>>(),
            vec![3],
            "the still-buffered picture stays pending"
        );

        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(5, 500);
        pending.insert(4, 400);
        let update = DpbUpdate {
            stored: None,
            outputs: vec![5, 99, 4],
            removed: vec![],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![500, 400], "bump order, not id order");
        assert!(dropped.is_empty());
    }
}
