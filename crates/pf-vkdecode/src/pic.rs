//! Per-AU conversion: one [`AuPlan`] into the `StdVideoDecodeH264*` structs, slice
//! offsets and DPB slot bindings a `vkCmdDecodeVideoKHR` call is built from (WP-B).
//!
//! Progressive envelope: pf-bitstream's planner rejects interlaced streams before a
//! plan exists, so every field/bottom FLAG here is written 0. The top/bottom
//! PicOrderCnt pairs are still real pairs — a progressive frame's bottom count
//! differs from its top whenever the PPS carries
//! `bottom_field_pic_order_in_frame_present_flag` — and ride through from
//! pf-bitstream verbatim.

use ash::vk::native as hh;
use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;

use crate::slots::SlotError;
use crate::slots::SlotMap;

/// One active reference of the AU: its DPB slot, its Std reference info, and the
/// planner id it resolves (kept so the backend can map the slot to its image).
#[derive(Debug, Clone)]
pub struct VkRef {
    pub slot: u8,
    pub std: hh::StdVideoDecodeH264ReferenceInfo,
    pub id: PicId,
}

/// Everything CPU-derivable of one AU's decode submission. WP-B adds the live
/// objects: bitstream buffer, DPB images, session and command recording.
#[derive(Debug, Clone)]
pub struct DecodePlanVk {
    pub std_pic: hh::StdVideoDecodeH264PictureInfo,
    /// Byte offset of each slice NALU in the AU as planned, START CODE INCLUDED.
    /// AU-relative, NOT submission-final: the recording layer packs the SLICE
    /// NALUs alone into the bitstream buffer and rebases these offsets while
    /// doing so (non-VCL NALUs inside the decode range hang VCN firmware — see
    /// the slices-only packing in `decoder.rs`); Vulkan's `pSliceOffsets`
    /// receives the rebased offsets, each pointing at a start code within the
    /// packed buffer.
    pub slice_offsets: Vec<u32>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    /// Reference info for the setup slot: the picture's own FrameNum/POC, with the
    /// long-term flag already set when this very AU marks itself long-term (IDR
    /// `long_term_reference_flag` or MMCO 6).
    pub setup_ref: hh::StdVideoDecodeH264ReferenceInfo,
    /// The planner id of the decoded picture (`AuPlan.dpb.stored`) — the backend
    /// keys its image bookkeeping by it.
    pub setup_id: PicId,
    /// Whether the decoded picture is a reference. When `false` the setup slot
    /// exists for the decode itself (plus any remaining DPB residency the planner
    /// grants the picture) and must never be bound as a reference for later AUs —
    /// it may even have been released already, via this very AU's `removed`, when
    /// the picture bypassed the DPB.
    pub setup_is_reference: bool,
    /// The unique referenced pictures across all slices, in first-appearance order.
    pub refs: Vec<VkRef>,
    /// Slots this access unit's own end-of-picture bookkeeping retires while the
    /// decode op still BINDS them. Release them once that op is recorded — never
    /// inside the conversion, and never dropped.
    ///
    /// The H.264 twin of [`crate::pic_av1::DecodePlanVkAv1::release_after_decode`],
    /// and it exists for exactly the same reason: [`SlotMap::assign`] takes the lowest
    /// free slot, so a release here hands the setup assignment two lines later the
    /// slot a reference of this very AU still occupies. `pf_bitstream`'s `H264Planner`
    /// snapshots `dpb_refs` in `begin_picture`, BEFORE `finish_picture` runs 8.2.5's
    /// marking and C.4.5.3's bump, so a picture the sliding window unmarks and the
    /// bump evicts is in both `dpb_refs` and `dpb.removed` for one AU — which needs
    /// the eviction to be of an already-OUTPUT picture, i.e. low-delay H.264.
    ///
    /// **Measured 2026-08-07 on this rung as well as the DXVA one:** 297 of 300 AUs of
    /// every stream a punktfunk host emits, at 720p, 1080p and 2160p alike. See
    /// [`pf_dxvadec::pic::DecodePlanDxva::release_after_decode`]'s docs for the full
    /// measurement and for why `test-25fps.h264` measures zero.
    ///
    /// Both of this rung's DPB modes break on it, differently and neither loudly:
    /// in DISTINCT mode `slot_view` hands the aliased reference the same DPB array
    /// layer the setup writes, a read-write alias of one subresource; in COINCIDE mode
    /// the binding sync clears `slot_image[setup]` (it is the setup slot now) and the
    /// reference resolves to no bound image at all, dropping out of `pReferenceSlots`
    /// with a `trace!` and nothing else.
    ///
    /// The caller must apply these on its FAILURE paths too. They are slot-ledger
    /// bookkeeping the planner already committed, not something this AU's fate can
    /// undo — dropping them leaks a slot per AU and reaches `SlotError::Full`.
    pub release_after_decode: Vec<PicId>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades it to
/// [`pf_bitstream::h264::PlanWarning`]s upstream; these are caller/session bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkError {
    /// The plan holds no slices; there is nothing to submit.
    NoSlices,
    /// The plan's `DpbUpdate.stored` is `None`. `plan_au` always stores; only
    /// `flush()` produces such updates, and those go to [`SlotMap::apply`] directly.
    NoStoredId,
    /// A reference list entry's id holds no slot: an earlier plan of this stream
    /// never went through this [`SlotMap`].
    UnresolvedReference(PicId),
    Slot(SlotError),
    /// A slice offset exceeds `u32` (Vulkan submits offsets as `u32`).
    OffsetOverflow(usize),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB. The session (WP-C)
    /// must rebuild the video session and its [`SlotMap`]; converting against the
    /// stale map would hand out slot indices the session's image pool does not have.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
}

impl std::fmt::Display for PlanToVkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkError::NoSlices => write!(f, "the plan holds no slices"),
            PlanToVkError::NoStoredId => {
                write!(
                    f,
                    "the plan stores no picture (flush updates go to SlotMap::apply)"
                )
            }
            PlanToVkError::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToVkError::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToVkError::OffsetOverflow(offset) => {
                write!(f, "slice offset {offset} exceeds u32")
            }
            PlanToVkError::CapacityMismatch { required, capacity } => {
                write!(
                    f,
                    "the plan needs {required} slots but the map holds {capacity} — \
                     an SPS renegotiation resized the DPB; rebuild session and map"
                )
            }
        }
    }
}

impl std::error::Error for PlanToVkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToVkError::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToVkError {
    fn from(err: SlotError) -> Self {
        PlanToVkError::Slot(err)
    }
}

/// One [`RefPic`] as Std reference info.
fn ref_info(rp: &RefPic) -> hh::StdVideoDecodeH264ReferenceInfo {
    // SAFETY: StdVideoDecodeH264ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word and integers; all-zero is a valid value for every field.
    let mut std: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_used_for_long_term_reference(u32::from(rp.is_long_term));
    // top/bottom_field_flag stay 0 and is_non_existing stays 0: progressive envelope,
    // and pf-bitstream never emits a gap placeholder as an id (it substitutes and
    // warns instead).
    //
    // FrameNum carries exactly the pair-key the Std struct wants: frame_num for
    // short-term references, LongTermFrameIdx for long-term ones.
    std.FrameNum = rp.frame_num_or_lt_idx;
    // The stored picture's real 8.2.1 pair: top != bottom whenever the PPS carried
    // bottom_field_pic_order_in_frame_present_flag and the slice a nonzero
    // delta_pic_order_cnt_bottom — even for progressive frames.
    std.PicOrderCnt = [rp.top_field_order_cnt, rp.bottom_field_order_cnt];
    std
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `sps_id` is the id of the active SPS: an [`AuPlan`] names the active PPS (each
/// slice header carries `pic_parameter_set_id`) but not the SPS that PPS references —
/// the caller resolves it through its parameter-set table (in WP-B,
/// [`crate::OwnedStdPps`]'s `seq_parameter_set_id` keyed by the first slice's PPS id).
///
/// Atomicity contract: every fallible step runs before any mutation of `slots`, so
/// an error leaves the map exactly as it was. In order:
/// 1. capacity is validated against the plan's `max_dpb_frames` (read-only);
/// 2. references resolve against the PRE-removal state (read-only) — this AU's own
///    end-of-picture marking (8.2.5) can evict a picture its slices legitimately
///    reference, e.g. the sliding window dropping the oldest short-term reference,
///    so `removed` must not be applied before the lists are mapped;
/// 3. slice offsets are validated (read-only);
/// 4. the setup slot is assigned last (its failures are caller bugs; nothing is ever
///    half-applied). `removed` is NOT applied here — it leaves as
///    [`DecodePlanVk::release_after_decode`] for the caller to apply once the decode
///    op is recorded, because applying it now would give the assignment back a slot
///    this AU's own references occupy (that field's docs carry the measurement).
///    Released slots become assignable to later pictures; keeping the underlying
///    images alive until in-flight decodes complete is WP-B's synchronization, not
///    this map's.
pub fn plan_to_vk(
    plan: &AuPlan,
    slots: &mut SlotMap,
    sps_id: u8,
) -> Result<DecodePlanVk, PlanToVkError> {
    let first_slice = plan.slices.first().ok_or(PlanToVkError::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToVkError::NoStoredId)?;

    // The map must match THIS plan's DPB depth; a mismatch means an SPS
    // renegotiation resized the DPB and the session must be rebuilt (WP-C).
    let required = plan.picture.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVkError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // Unique referenced pictures across every slice's two lists, first appearance
    // first. Per-slice list ORDER (ref_idx mapping) lives in the SlicePlans; this Vec
    // is the AU-level slot binding set Vulkan wants (each slot listed once).
    let mut refs: Vec<VkRef> = Vec::new();
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if refs.iter().any(|existing| existing.id == rp.id) {
                continue;
            }
            let slot = slots
                .slot_of(rp.id)
                .ok_or(PlanToVkError::UnresolvedReference(rp.id))?;
            refs.push(VkRef {
                slot,
                std: ref_info(rp),
                id: rp.id,
            });
        }
    }

    let pic = &plan.picture;

    // SAFETY: StdVideoDecodeH264PictureInfo is a plain-C bindgen struct of a bitfield
    // word and integers; all-zero is a valid value for every field.
    let mut std_pic: hh::StdVideoDecodeH264PictureInfo = unsafe { std::mem::zeroed() };
    let is_intra = plan
        .slices
        .iter()
        .all(|slice| slice.header.slice_type.is_i() || slice.header.slice_type.is_si());
    std_pic.flags.set_is_intra(u32::from(is_intra));
    std_pic.flags.set_is_reference(u32::from(pic.is_reference));
    std_pic.flags.set_IdrPicFlag(u32::from(pic.is_idr));
    // field_pic_flag / bottom_field_flag / complementary_field_pair stay 0 by the
    // progressive envelope (module docs).
    std_pic.seq_parameter_set_id = sps_id;
    std_pic.pic_parameter_set_id = first_slice.header.pic_parameter_set_id;
    std_pic.frame_num = pic.frame_num;
    std_pic.idr_pic_id = if pic.is_idr {
        first_slice.header.idr_pic_id
    } else {
        0
    };
    std_pic.PicOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];

    // The setup slot's reference info: this picture's own identity. When the AU marks
    // ITSELF long-term — an IDR's long_term_reference_flag, or an MMCO 6 assigning an
    // index (8.2.5) — the slot activates as a long-term reference keyed by
    // LongTermFrameIdx, mirroring how ref_info keys long-term entries.
    //
    // ACTIVATION-vs-REFERENCE asymmetry under MMCO 5 (spec-legal, deliberately NOT
    // rejected): these are the picture's 8.2.1 values as decoded, but an MMCO 5 in
    // this same AU rebases the STORED frame_num/POC to zero after decoding
    // (8.2.5.4.5), so later AUs reference this slot by the rebased pair (RefPic
    // carries the stored values). punktfunk hosts never emit MMCO 5;
    // pf_bitstream::h264::PlanWarning::Mmco5Rebase flags any occurrence so field
    // logs tell us if that assumption ever breaks.
    // SAFETY: as above — all-zero is a valid StdVideoDecodeH264ReferenceInfo.
    let mut setup_ref: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
    setup_ref.PicOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];
    let marking = &first_slice.header.dec_ref_pic_marking;
    let self_lt_idx = if pic.is_idr {
        marking.long_term_reference_flag.then_some(0u32)
    } else if marking.adaptive_ref_pic_marking_mode_flag {
        marking
            .inner
            .iter()
            .find(|op| op.memory_management_control_operation == 6)
            .map(|op| op.long_term_frame_idx)
    } else {
        None
    };
    match self_lt_idx {
        Some(idx) => {
            setup_ref.flags.set_used_for_long_term_reference(1);
            // Same saturation as pf-bitstream's frame_num_or_lt_idx: the spec bounds
            // the ue(v)-coded index at 15, the parser does not.
            setup_ref.FrameNum = u16::try_from(idx).unwrap_or(u16::MAX);
        }
        None => setup_ref.FrameNum = pic.frame_num,
    }

    let mut slice_offsets = Vec::with_capacity(plan.slices.len());
    for slice in &plan.slices {
        // SlicePlan.data starts at the slice NALU's start code — exactly the offset
        // Vulkan wants (struct docs).
        slice_offsets.push(
            u32::try_from(slice.data.start)
                .map_err(|_| PlanToVkError::OffsetOverflow(slice.data.start))?,
        );
    }

    // Mutations LAST, after every fallible step above (fn docs).
    //
    // The removals are handed back rather than applied: releasing one here returns
    // its slot to the setup assignment below, and this AU's own references sit in
    // those slots (`DecodePlanVk::release_after_decode`).
    //
    // The AU's own picture can itself appear in `removed`: a non-reference picture
    // with no free frame buffer bypasses the DPB and is stored-and-evicted within
    // one plan. Its slot must still exist for the decode itself, so it is assigned
    // here and released right after — see `DecodePlanVk::setup_is_reference`. That
    // is the one removal that is NOT deferred: handing the caller the slot being
    // decoded into is the very aliasing the deferral exists to prevent.
    let setup_evicted = plan.dpb.removed.contains(&setup_id);
    let release_after_decode: Vec<PicId> = plan
        .dpb
        .removed
        .iter()
        .copied()
        .filter(|id| *id != setup_id)
        .collect();
    let setup_slot = slots.assign(setup_id)?;
    if setup_evicted {
        slots.release(setup_id);
    }

    Ok(DecodePlanVk {
        std_pic,
        slice_offsets,
        setup_slot,
        setup_ref,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
        release_after_decode,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::rc::Rc;

    use cros_codecs::codec::h264::nalu_writer::NaluWriter;
    use cros_codecs::codec::h264::parser::Nalu;
    use cros_codecs::codec::h264::parser::NaluType;
    use cros_codecs::codec::h264::parser::Pps;
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::Sps;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;
    use pf_bitstream::h264::H264Planner;
    use pf_bitstream::h264::Level;

    use super::*;

    // The same vendored vector pf-bitstream's tests plan (its goldens: 250 AUs, 500
    // slices), included from the same path rather than copied.
    const TEST_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// Test-only AU splitter, mirroring pf-bitstream's `split_into_aus` helper (which
    /// is `#[cfg(test)]`-private there): a new AU starts at a non-slice NALU following
    /// slices, or at a slice with `first_mb_in_slice == 0` (whose ue(v) encoding makes
    /// the first RBSP bit 1) when the current AU already has slices.
    fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut cursor = Cursor::new(stream);
        let mut au_start = 0usize;
        let mut au_has_slice = false;

        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let nalu_offset = cursor.position() as usize;
            let start = nalu_offset - nalu.offset;
            let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
            let first_mb_zero =
                is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

            if au_has_slice && (!is_slice || first_mb_zero) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// The decoder's picture-pool occupancy (`bound`/`pending`/`held`, exactly
    /// `decode_inner`'s bookkeeping minus the GPU) over the WHOLE vendored
    /// vector, with a consumer that HOLDS `hold` delivered frames before
    /// releasing the oldest — the real client's shape (~4-7 held across its
    /// channels, preroll and in-flight present). Returns the first starved AU
    /// index, if any.
    fn simulate_pool_occupancy(pool_size: usize, hold: usize) -> Option<usize> {
        use std::collections::VecDeque;

        #[derive(Clone, Default)]
        struct SimPicture {
            bound: bool,
            pending: bool,
            held: u32,
        }

        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut pictures = vec![SimPicture::default(); pool_size];
        let mut slot_image: Vec<Option<usize>> = Vec::new();
        // id -> pool image of the decoded picture awaiting its output verdict.
        let mut pending: BTreeMap<PicId, usize> = BTreeMap::new();
        // Delivered frames the consumer holds, oldest first.
        let mut consumer: VecDeque<usize> = VecDeque::new();

        for (index, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| {
                slot_image = vec![None; plan.picture.max_dpb_frames + 1];
                SlotMap::new(plan.picture.max_dpb_frames)
            });
            let vk = plan_to_vk(&plan, slots, 0).expect("the clean vector converts");

            // Binding sync: released slots unbind; the setup slot rebinds fresh. The
            // deferred releases have deliberately NOT run yet — that is the whole
            // point of `DecodePlanVk::release_after_decode`, and doing it in the wrong
            // order here would unbind the images this AU's own references read.
            let setup = usize::from(vk.setup_slot);
            let mut held_slots = vec![false; slot_image.len()];
            for (slot, _id) in slots.held() {
                held_slots[usize::from(slot)] = true;
            }
            for (slot, binding) in slot_image.iter_mut().enumerate() {
                if let Some(picture) = *binding {
                    if !held_slots[slot] || slot == setup {
                        pictures[picture].bound = false;
                        *binding = None;
                    }
                }
            }

            // The decode target: a free pool image.
            let Some(dst) = pictures
                .iter()
                .position(|p| !p.bound && !p.pending && p.held == 0)
            else {
                return Some(index);
            };
            pictures[dst].pending = true;
            pictures[dst].bound = true;
            slot_image[setup] = Some(dst);
            pending.insert(vk.setup_id, dst);

            // Post-submit: the slots the conversion held back, exactly where
            // `Decoder::decode` applies them.
            for id in &vk.release_after_decode {
                assert!(slots.release(*id), "a deferred id held no slot");
            }

            // Settle: outputs deliver to the consumer; removed-never-output free.
            for id in &plan.dpb.outputs {
                if let Some(picture) = pending.remove(id) {
                    pictures[picture].pending = false;
                    pictures[picture].held += 1;
                    consumer.push_back(picture);
                }
            }
            for id in &plan.dpb.removed {
                if let Some(picture) = pending.remove(id) {
                    pictures[picture].pending = false;
                }
            }
            // The hold-N consumer: releases only once it holds MORE than `hold`.
            while consumer.len() > hold {
                let released = consumer.pop_front().expect("nonempty");
                pictures[released].held -= 1;
            }
        }
        None
    }

    /// The .25 field-failure regression, pool-model edition: the vendored vector
    /// keeps up to `max_dpb_frames + 1 = 8` pictures resident AND the real
    /// client holds ~4 delivered frames — the pool must absorb BOTH at once.
    /// `required_slots + HOLD_HEADROOM` never starves; the counterfactual shows
    /// an under-headroomed pool starving on the same clean stream, which is the
    /// exact class the fixed-size ring shipped in the first WP-B round.
    #[test]
    fn the_full_vector_with_a_hold_four_consumer_never_starves_the_picture_pool() {
        // This vector: max_dpb_frames = 7 → required_slots = 8 (measured;
        // asserted inside via SlotMap sizing).
        let required_slots = 8;
        let headroom = crate::images::HOLD_HEADROOM as usize;
        assert_eq!(
            simulate_pool_occupancy(required_slots + headroom, 4),
            None,
            "the shipped sizing must survive the whole vector with 4 held frames"
        );
        // Counterfactual: holds beyond the headroom starve — the documented
        // NoFreeSlot condition, now meaning exactly what it says.
        assert!(
            simulate_pool_occupancy(required_slots + 2, 4).is_some(),
            "an under-headroomed pool must starve (else this regression proves nothing)"
        );
    }

    #[test]
    fn the_full_25fps_vector_converts_with_stable_slots_and_start_code_offsets() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        // PicId -> the slot it was assigned; entries leave only on `removed`.
        let mut held: BTreeMap<PicId, u8> = BTreeMap::new();
        let mut converted = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk(&plan, slots, 0).expect("the clean vector converts");
            converted += 1;

            // Slot stability: every reference resolves to the slot its picture was
            // assigned when IT was decoded, and no ref shares the setup slot.
            for r in &vk.refs {
                assert_eq!(
                    held.get(&r.id),
                    Some(&r.slot),
                    "a referenced picture's slot changed while it was referenced"
                );
                assert_ne!(r.slot, vk.setup_slot, "a reference aliases the setup slot");
            }

            // Slice offsets: one per slice, each at a start-code boundary of the AU,
            // and exactly where the plan said the slice begins.
            assert_eq!(vk.slice_offsets.len(), plan.slices.len());
            for (offset, slice) in vk.slice_offsets.iter().zip(&plan.slices) {
                let offset = *offset as usize;
                assert_eq!(offset, slice.data.start);
                let at = &au[offset..];
                assert!(
                    at.starts_with(&[0, 0, 1]) || at.starts_with(&[0, 0, 0, 1]),
                    "slice offset {offset} does not sit on a start code"
                );
            }

            assert_eq!(vk.std_pic.frame_num, plan.picture.frame_num);
            assert_eq!(
                u32::from(plan.picture.is_idr),
                vk.std_pic.flags.IdrPicFlag()
            );
            assert_eq!(
                vk.setup_ref.PicOrderCnt[0],
                plan.picture.top_field_order_cnt
            );

            // Mirror the map's bookkeeping: record the new picture, drop the removed.
            //
            // The removals are dropped only after the deferred releases run, because
            // that is when the MAP drops them — before that the conversion is still
            // holding them so this AU's submission can name their slots
            // (`DecodePlanVk::release_after_decode`).
            let stored = plan.dpb.stored.unwrap();
            assert_eq!(vk.setup_id, stored);
            assert_eq!(vk.setup_is_reference, plan.picture.is_reference);
            held.insert(stored, vk.setup_slot);
            assert_eq!(
                vk.release_after_decode,
                plan.dpb
                    .removed
                    .iter()
                    .copied()
                    .filter(|id| *id != stored)
                    .collect::<Vec<_>>(),
                "the deferral is the plan's whole `removed` list less the stored id"
            );
            for id in &vk.release_after_decode {
                assert!(slots.release(*id), "a deferred id held no slot");
            }
            for id in &plan.dpb.removed {
                held.remove(id);
            }

            // held() must mirror the plan-driven bookkeeping exactly, every AU.
            let ledger: BTreeMap<PicId, u8> = slots.held().map(|(slot, id)| (id, slot)).collect();
            assert_eq!(ledger, held);
        }

        assert_eq!(converted, 250, "the vector's own golden");

        // Teardown: the flush update releases every remaining slot.
        let mut slots = slots.unwrap();
        slots.apply(&planner.flush());
        assert_eq!(slots.active(), 0);
    }

    /// Byte-level authoring, mirroring pf-bitstream's MMCO/LTR test (its helpers are
    /// `#[cfg(test)]`-private): parameter sets via the vendored builders +
    /// synthesizer, slice headers hand-written with the vendored `NaluWriter`. The
    /// planner only reads headers, so no slice data follows the rbsp stop bit.
    fn authored_sps_pps() -> (Rc<Sps>, Rc<Pps>) {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .resolution(64, 64)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        (sps, pps)
    }

    /// `bottom_delta` writes `delta_pic_order_cnt_bottom` — only legal when the PPS
    /// the slice references sets `bottom_field_pic_order_in_frame_present_flag`
    /// (the parser reads the field iff the flag is set, so writer and PPS must
    /// agree).
    fn write_idr_slice(bottom_delta: Option<i32>) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(3, NaluType::SliceIdr as u8).unwrap();
            w.write_ue(0u32).unwrap(); // first_mb_in_slice
            w.write_ue(2u32).unwrap(); // slice_type: I
            w.write_ue(0u32).unwrap(); // pic_parameter_set_id
            w.write_f(4, 0u32).unwrap(); // frame_num, u(4): log2_max_frame_num_minus4 = 0
            w.write_ue(7u32).unwrap(); // idr_pic_id
            w.write_f(4, 0u32).unwrap(); // pic_order_cnt_lsb, u(4)
            if let Some(delta) = bottom_delta {
                w.write_se(delta).unwrap(); // delta_pic_order_cnt_bottom
            }
            w.write_f(1, 0u32).unwrap(); // no_output_of_prior_pics_flag
            w.write_f(1, 0u32).unwrap(); // long_term_reference_flag
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    /// One P slice NALU. `mmco_ops` = `None` for sliding-window marking, `Some(ops)`
    /// for adaptive marking with `(operation, single-argument)` pairs (ops 2/4/6 all
    /// take exactly one) — the writer appends the terminating op 0. `bottom_delta`
    /// as in [`write_idr_slice`].
    fn write_p_slice(
        frame_num: u32,
        poc_lsb: u32,
        bottom_delta: Option<i32>,
        num_ref_idx_l0_active: u32,
        mmco_ops: Option<&[(u32, u32)]>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(1, NaluType::Slice as u8).unwrap();
            w.write_ue(0u32).unwrap(); // first_mb_in_slice
            w.write_ue(0u32).unwrap(); // slice_type: P
            w.write_ue(0u32).unwrap(); // pic_parameter_set_id
            w.write_f(4, frame_num).unwrap(); // frame_num, u(4)
            w.write_f(4, poc_lsb).unwrap(); // pic_order_cnt_lsb, u(4)
            if let Some(delta) = bottom_delta {
                w.write_se(delta).unwrap(); // delta_pic_order_cnt_bottom
            }
            w.write_f(1, 1u32).unwrap(); // num_ref_idx_active_override_flag
            w.write_ue(num_ref_idx_l0_active - 1).unwrap();
            w.write_f(1, 0u32).unwrap(); // ref_pic_list_modification_flag_l0
            match mmco_ops {
                None => w.write_f(1, 0u32).map(|_| ()).unwrap(),
                Some(ops) => {
                    w.write_f(1, 1u32).unwrap(); // adaptive_ref_pic_marking_mode_flag
                    for (op, arg) in ops {
                        w.write_ue(*op).unwrap();
                        w.write_ue(*arg).unwrap();
                    }
                    w.write_ue(0u32).unwrap(); // memory_management_control_operation end
                }
            }
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    #[test]
    fn an_mmco_self_marking_sets_the_setup_lt_flag_and_later_refs_carry_it() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice(None));
        // AU1 marks itself long-term: MMCO 4 admits long-term index 0, MMCO 6
        // assigns it to the current picture.
        let au1 = write_p_slice(1, 2, None, 1, Some(&[(4, 1), (6, 0)]));
        let au2 = write_p_slice(2, 4, None, 2, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);

        let idr_id = p0.dpb.stored.unwrap();
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();
        assert_eq!(vk0.std_pic.flags.IdrPicFlag(), 1);
        assert_eq!(vk0.std_pic.flags.is_intra(), 1);
        assert_eq!(vk0.std_pic.idr_pic_id, 7, "from the authored slice header");
        assert_eq!(vk0.std_pic.PicOrderCnt, [0, 0]);
        assert_eq!(vk0.setup_ref.flags.used_for_long_term_reference(), 0);
        assert_eq!(vk0.setup_id, idr_id);
        assert!(vk0.setup_is_reference, "an IDR is a reference");
        assert!(vk0.refs.is_empty());

        // AU1: the setup slot activates LONG-TERM (its own MMCO 6), keyed by
        // LongTermFrameIdx 0, not by frame_num 1.
        let p1 = planner.plan_au(&au1).unwrap();
        let lt_id = p1.dpb.stored.unwrap();
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();
        assert_eq!(vk1.std_pic.flags.is_intra(), 0);
        assert_eq!(vk1.std_pic.PicOrderCnt, [2, 2]);
        assert_eq!(vk1.setup_ref.flags.used_for_long_term_reference(), 1);
        assert_eq!(vk1.setup_ref.FrameNum, 0, "LongTermFrameIdx, not frame_num");
        assert_eq!(vk1.refs.len(), 1);
        assert_eq!(vk1.refs[0].id, idr_id);
        assert_eq!(vk1.refs[0].std.flags.used_for_long_term_reference(), 0);

        // AU2 references both: the IDR short-term (keyed by frame_num) and AU1
        // long-term (keyed by LongTermFrameIdx), each on its stable slot.
        let p2 = planner.plan_au(&au2).unwrap();
        let vk2 = plan_to_vk(&p2, &mut slots, 0).unwrap();
        let by_id: BTreeMap<PicId, &VkRef> = vk2.refs.iter().map(|r| (r.id, r)).collect();
        let idr_ref = by_id[&idr_id];
        assert_eq!(idr_ref.std.flags.used_for_long_term_reference(), 0);
        assert_eq!(idr_ref.std.FrameNum, 0);
        assert_eq!(idr_ref.slot, vk0.setup_slot);
        let lt_ref = by_id[&lt_id];
        assert_eq!(lt_ref.std.flags.used_for_long_term_reference(), 1);
        assert_eq!(lt_ref.std.FrameNum, 0, "LongTermFrameIdx, not frame_num");
        assert_eq!(
            lt_ref.std.PicOrderCnt,
            [2, 2],
            "the stored top/bottom pair (equal here: no delta_pic_order_cnt_bottom)"
        );
        assert_eq!(lt_ref.slot, vk1.setup_slot);
        assert_ne!(vk2.setup_slot, idr_ref.slot);
        assert_ne!(vk2.setup_slot, lt_ref.slot);
    }

    #[test]
    fn a_failed_conversion_leaves_the_slot_map_untouched_and_the_session_recovers() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice(None));
        let au1 = write_p_slice(1, 2, None, 1, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&au1).unwrap();

        // A right-sized map that never saw AU0, holding one unrelated slot: the
        // reference must fail loudly, not resolve to a fabricated slot.
        let mut fresh = SlotMap::new(p1.picture.max_dpb_frames);
        fresh.assign(999).unwrap();
        assert_eq!(
            plan_to_vk(&p1, &mut fresh, 0).unwrap_err(),
            PlanToVkError::UnresolvedReference(idr_id)
        );

        // Atomicity: the failed conversion mutated nothing.
        assert_eq!(fresh.active(), 1);
        assert_eq!(fresh.held().collect::<Vec<_>>(), vec![(0, 999)]);

        // And the session recovers: the next valid AU (an IDR restart) still
        // converts on the same map. Its `removed` names ids this map never assigned
        // (planned before the map existed) — tolerated by design.
        let p2 = planner.plan_au(&write_idr_slice(None)).unwrap();
        let vk2 = plan_to_vk(&p2, &mut fresh, 0).unwrap();
        assert_eq!(vk2.setup_slot, 1, "the lowest free slot after the held one");
        assert_eq!(fresh.active(), 2);
    }

    #[test]
    fn an_sps_switch_that_resizes_the_dpb_is_a_capacity_mismatch_not_a_guess() {
        // Two SPSes whose only planning-relevant difference is DPB depth: Level 1 at
        // 320x240 gives MaxDpbMbs 396 / 300 = 1 frame; Level 4 gives 16.
        let authored = |level: Level, max_refs: u8| -> (Rc<Sps>, Rc<Pps>) {
            let sps = SpsBuilder::new()
                .seq_parameter_set_id(0)
                .profile_idc(Profile::Main)
                .level_idc(level)
                .frame_mbs_only_flag(true)
                .direct_8x8_inference_flag(true)
                .max_num_ref_frames(max_refs)
                .log2_max_frame_num_minus4(0)
                .pic_order_cnt_type(0)
                .log2_max_pic_order_cnt_lsb_minus4(0)
                .resolution(320, 240)
                .build();
            let pps = PpsBuilder::new(Rc::clone(&sps))
                .pic_parameter_set_id(0)
                .pic_init_qp(26)
                .build();
            (sps, pps)
        };
        let (sps_a, pps_a) = authored(Level::L1, 1);
        let (sps_b, pps_b) = authored(Level::L4, 4);

        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps_a, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps_a, &mut au0, true).unwrap();
        au0.extend(write_idr_slice(None));
        let mut au1 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps_b, &mut au1, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps_b, &mut au1, true).unwrap();
        au1.extend(write_idr_slice(None));

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert_eq!(p0.picture.max_dpb_frames, 1);
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        plan_to_vk(&p0, &mut slots, 0).unwrap();

        // The renegotiated stream needs a deeper DPB than this map was built for:
        // refuse, so the session (WP-C) rebuilds session + map instead of handing
        // out slots the image pool does not have.
        let p1 = planner.plan_au(&au1).unwrap();
        assert_eq!(p1.picture.max_dpb_frames, 16);
        assert_eq!(
            plan_to_vk(&p1, &mut slots, 0).unwrap_err(),
            PlanToVkError::CapacityMismatch {
                required: 17,
                capacity: 2
            }
        );
    }

    #[test]
    fn a_full_dpb_bump_reuses_the_slot_but_the_pool_model_binds_a_fresh_image() {
        // Depth-1 DPB (Level 1 at 320x240 ⇒ max_dpb_frames 1, capacity 2): every
        // stored P evicts the previous picture, and that picture's id lands in
        // BOTH `outputs` and `removed` of the SAME plan.
        //
        // ⚠ This test used to assert that `plan_to_vk` freed the evicted slot and
        // re-assigned it as THIS AU's setup, calling that "the planner's normal
        // behaviour". It was not: AU1 is a P picture that REFERENCES the picture it
        // was evicting, so the submission named one slot as both `pSetupReferenceSlot`
        // and a reference — a decode into the surface being predicted from. The same
        // defect the AV1 rung was fixed for on 2026-08-07, authored here in miniature
        // and asserted as correct. `DecodePlanVk::release_after_decode` is the fix, and
        // this depth-1 stream is its tightest possible case: capacity 2, so the setup
        // has exactly one slot to go to and it is the spare.
        //
        // What the test still proves, and what it was really written for, is the pool
        // decoupling: the delivered picture's IMAGE is never the new decode target
        // while the consumer holds it (the HIGH overwrite bug of the adversarial
        // round, and the .25 field failure's class).
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L1)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(1)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .resolution(320, 240)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice(None));

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();

        // Decoder-side pool bookkeeping (mirrors decode_inner): image 0 hosts
        // picture 0; the consumer receives and HOLDS its frame.
        let pool = 2 + 1; // required_slots + 1 of headroom is enough here
        let mut bound = vec![false; pool];
        let mut held = vec![0u32; pool];
        let mut slot_image: Vec<Option<usize>> = vec![None; slots.capacity()];
        let free = |bound: &[bool], held: &[u32]| (0..pool).find(|&i| !bound[i] && held[i] == 0);

        let img0 = free(&bound, &held).unwrap();
        bound[img0] = true;
        slot_image[usize::from(vk0.setup_slot)] = Some(img0);

        // AU1 bumps AU0's picture: outputs+removed carry id0, and plan_to_vk
        // hands the SAME slot back as the setup.
        let p1 = planner
            .plan_au(&write_p_slice(1, 2, None, 1, None))
            .unwrap();
        assert!(p1.dpb.outputs.contains(&vk0.setup_id) && p1.dpb.removed.contains(&vk0.setup_id));
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();

        // AU1 REFERENCES the very picture it evicts — so the eviction is deferred and
        // the setup goes to the spare slot instead. Without the deferral these two
        // assertions are what fails, and they are the whole defect in two lines.
        assert!(
            vk1.refs.iter().any(|r| r.id == vk0.setup_id),
            "AU1 must reference the picture it evicts, or this proves nothing"
        );
        assert_ne!(
            vk1.setup_slot, vk0.setup_slot,
            "the setup must not take the slot of a picture this AU still references"
        );
        for r in &vk1.refs {
            assert_ne!(r.slot, vk1.setup_slot, "a reference aliases the setup slot");
        }
        assert_eq!(
            vk1.release_after_decode,
            vec![vk0.setup_id],
            "the eviction is handed back, not applied"
        );

        // Binding sync: the setup slot is fresh, so nothing is unbound for it;
        // picture 0's image is delivered to the consumer (held), NOT freed. Its slot
        // is still HELD at this point — which is exactly what keeps its image bound
        // while the decode op reads it as a reference.
        assert!(
            slots
                .held()
                .any(|(slot, id)| slot == vk0.setup_slot && id == vk0.setup_id),
            "the referenced picture must still hold its slot through the submission"
        );
        bound[img0] = false;
        held[img0] += 1; // outputs → delivered, consumer holds it

        // The pool hands the re-activated slot a FRESH image — never image 0.
        let img1 = free(&bound, &held).expect("headroom guarantees a free image");
        assert_ne!(
            img1, img0,
            "the held (delivered, unreleased) image must never be re-bound as a \
             decode target — the pool decoupling IS the overwrite fix"
        );
        bound[img1] = true;
        slot_image[usize::from(vk1.setup_slot)] = Some(img1);

        // Post-submit: the deferred release lands, and NOW the evicted slot is free —
        // a picture later than this submission may have it, which is the only thing
        // the deferral ever postponed.
        for id in &vk1.release_after_decode {
            assert!(slots.release(*id));
        }
        assert!(
            !slots.held().any(|(slot, _)| slot == vk0.setup_slot),
            "the deferred release must actually free the slot"
        );

        // Once the consumer releases frame 0, image 0 returns to the free list.
        held[img0] -= 1;
        assert_eq!(free(&bound, &held), Some(img0));
    }

    #[test]
    fn a_nonzero_delta_bottom_reaches_setup_and_reference_poc_pairs_distinctly() {
        // A PPS with bottom_field_pic_order_in_frame_present_flag: progressive
        // frames then carry delta_pic_order_cnt_bottom and bottom != top. The
        // builder has no setter for the flag, so the Pps is constructed directly
        // (its fields are public) and synthesized from there.
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .resolution(64, 64)
            .build();
        let pps = Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            entropy_coding_mode_flag: false,
            bottom_field_pic_order_in_frame_present_flag: true,
            num_slice_groups_minus1: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            weighted_pred_flag: false,
            weighted_bipred_idc: 0,
            pic_init_qp_minus26: 0,
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            deblocking_filter_control_present_flag: false,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
            pic_scaling_matrix_present_flag: false,
            scaling_lists_4x4: [[0; 16]; 6],
            scaling_lists_8x8: [[0; 64]; 6],
            second_chroma_qp_index_offset: 0,
            sps: Rc::clone(&sps),
        };

        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice(Some(2))); // top 0, bottom 0 + 2
        let au1 = write_p_slice(1, 4, Some(1), 1, None); // top 4, bottom 4 + 1

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert_eq!(
            (
                p0.picture.top_field_order_cnt,
                p0.picture.bottom_field_order_cnt
            ),
            (0, 2)
        );
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();
        // BOTH orders, both structs: a top/bottom swap or a collapse to one value
        // must fail here.
        assert_eq!(vk0.std_pic.PicOrderCnt, [0, 2]);
        assert_eq!(vk0.setup_ref.PicOrderCnt, [0, 2]);

        let p1 = planner.plan_au(&au1).unwrap();
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();
        assert_eq!(vk1.std_pic.PicOrderCnt, [4, 5]);
        assert_eq!(vk1.setup_ref.PicOrderCnt, [4, 5]);
        // The reference carries the STORED pair of the IDR — through RefPic, not a
        // fabricated bottom.
        assert_eq!(vk1.refs.len(), 1);
        assert_eq!(vk1.refs[0].id, p0.dpb.stored.unwrap());
        assert_eq!(vk1.refs[0].std.PicOrderCnt, [0, 2]);
    }
}
