//! Per-AU H.265 conversion: one [`AuPlan`] into the `StdVideoDecodeH265*` structs,
//! slice offsets and DPB slot bindings a `vkCmdDecodeVideoKHR` call is built from —
//! [`crate::pic`] one codec over (M3's CPU half; the session/recording half is a
//! later WP).
//!
//! The codec difference that shapes this module: Vulkan H.265 decode takes NO
//! per-slice reference lists. The hardware re-derives 8.3.4's lists itself from
//! the slice bits, keyed by the picture-level RPS index arrays
//! (`RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr`) — so the AU-level binding set
//! here is the union of the plan's three CURRENT reference picture sets
//! ([`pf_bitstream::h265::RpsPlan`]), not the union of slice lists as in H.264.
//! The plan's per-slice lists still exist and are used as a cross-check: every
//! list entry must be a member of the binding set, or the conversion fails closed.
//!
//! Those arrays carry **DPB slot indices**, not positions in the decode op's
//! reference list — the distinction that
//! [`DecodePlanVkH265::std_pic`] documents at length, because the two readings
//! coincide on a freshly anchored stream and diverge a handful of AUs later, which
//! makes getting it wrong silent corruption rather than an error.
//!
//! Concealment note: a lost reference is ABSENT from the plan's RPS sets (flagged
//! upstream via `PlanWarning::MissingReference`), so the Std index arrays compact
//! past it — later positions shift by one relative to the damaged stream's
//! intent. That is deliberate: there is no slot to point at, `0xFF` padding keeps
//! the arrays well-formed, and the session layer has already been told to request
//! recovery. The alternative (fabricating an entry) is exactly what this crate
//! never does.

use ash::vk::native as hh;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::RefPic;
use tracing::trace;

use crate::slots::SlotError;
use crate::slots::SlotMap;

/// `STD_VIDEO_DECODE_H265_REF_PIC_SET_LIST_SIZE`: each Std RPS index array holds
/// eight entries — the hard ceiling on how many CURRENT references one set may
/// carry through Vulkan (the spec itself allows up to 16 per side; beyond eight
/// is unexpressible and rejected, see [`PlanToVkH265Error::RpsSetOverflow`]).
pub const H265_RPS_LIST_SIZE: usize = 8;

/// The Std sentinel for an unused RPS index-array entry.
const UNUSED_RPS_ENTRY: u8 = 0xFF;

/// One active reference of the AU: its DPB slot, its Std reference info, and the
/// planner id it resolves (kept so the backend can map the slot to its image).
#[derive(Debug, Clone)]
pub struct VkRefH265 {
    pub slot: u8,
    pub std: hh::StdVideoDecodeH265ReferenceInfo,
    pub id: PicId,
}

/// Everything CPU-derivable of one AU's decode submission. The GPU half adds the
/// live objects: bitstream buffer, DPB images, session and command recording.
#[derive(Debug, Clone)]
pub struct DecodePlanVkH265 {
    /// The picture info. Its `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr`
    /// arrays hold **DPB SLOT INDICES** (`0xFF` = unused) — the same numbers
    /// `VkVideoReferenceSlotInfoKHR::slotIndex` carries, NOT positions in
    /// [`Self::refs`] / `pDecodeInfo->pReferenceSlots`.
    ///
    /// This is the one place the two readings can be confused, and confusing
    /// them is silent corruption rather than an error: they COINCIDE for as long
    /// as the referenced pictures happen to sit in slots `0..refs.len()` in
    /// `refs` order, which on a fresh IDR they do, so a stream decodes correctly
    /// for its first few AUs and then quietly references the wrong pictures.
    /// (On the vendored 25fps H.265 vector that is exactly AUs 0-2 correct and
    /// AU 3 onwards wrong — its first B picture with two `StCurrAfter` entries
    /// binds slots 2 and 1 in that order, so `refs` positions 1,2 and slots 2,1
    /// disagree.)
    ///
    /// The authority is libavcodec's Vulkan H.265 hwaccel, which every driver is
    /// validated against: it fills these arrays with the index of the picture in
    /// its own DPB array and hands that SAME index to `slotIndex`, while packing
    /// `pReferenceSlots` densely over only the USED entries — so the two arrays
    /// are provably different numberings there, and the RPS one follows the slot.
    ///
    /// The backend must still lay `pReferenceSlots` out in [`Self::refs`] order
    /// and fail closed on a reference with no bound image — not because the
    /// indices depend on the order any more, but because a slot these arrays
    /// name that the decode op never binds is unresolvable for the hardware.
    pub std_pic: hh::StdVideoDecodeH265PictureInfo,
    /// Byte offset of each slice segment NALU in the AU as planned, START CODE
    /// INCLUDED — exactly one entry per slice segment of the AU, in plan order
    /// (so the recording layer's own rebased array, built by walking the same
    /// slices, matches this length by construction).
    /// AU-relative, NOT submission-final: the recording layer must
    /// pack the SLICE NALUs alone into the bitstream buffer and rebase these
    /// offsets while doing so — non-VCL NALUs inside the decode range hang VCN
    /// firmware (the H.264 decoder's slices-only packing exists for exactly
    /// that `vcn_unified_0` ring timeout; FFmpeg feeds slices-only for the same
    /// reason). Vulkan's `pSliceSegmentOffsets` then receives the REBASED
    /// offsets, each pointing at a start code within the packed buffer.
    pub slice_offsets: Vec<u32>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    /// Reference info for the setup slot: the picture's own POC, short-term.
    /// HEVC has no same-AU self-marking (H.264's IDR long_term_reference_flag /
    /// MMCO 6): C.3.4 marks every stored picture "used for short-term reference",
    /// and a picture turns long-term only when a LATER picture's RPS lists it in
    /// `RefPicSetLtCurr` — at which point that AU's [`Self::refs`] entry carries
    /// the long-term flag.
    pub setup_ref: hh::StdVideoDecodeH265ReferenceInfo,
    /// The planner id of the decoded picture (`AuPlan.dpb.stored`) — the backend
    /// keys its image bookkeeping by it.
    pub setup_id: PicId,
    /// Whether the decoded picture may be referenced by later pictures (false
    /// for sub-layer non-reference NALU types, RASL_N/TRAIL_N and friends). When
    /// `false` the setup slot exists for the decode itself plus any remaining
    /// DPB residency, and must never be bound as a reference for later AUs.
    pub setup_is_reference: bool,
    /// The unique referenced pictures of this AU — the union of the plan's three
    /// current RPS sets in set order (StCurrBefore, StCurrAfter, LtCurr), first
    /// appearance first. Every slot [`Self::std_pic`]'s index arrays name appears
    /// here exactly once, which is what makes those arrays resolvable against the
    /// decode op's reference list.
    pub refs: Vec<VkRefH265>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades
/// it to [`pf_bitstream::h265::PlanWarning`]s upstream; these are caller/session
/// bugs or envelope limits. (`PlanError::RaslSkipped` also never reaches this
/// layer: it is an error OF planning, handled as an Ok-skip by the client
/// wiring, and no plan exists to convert.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkH265Error {
    /// The plan holds no slices; there is nothing to submit.
    NoSlices,
    /// The plan's `DpbUpdate.stored` is `None`. `plan_au` always stores; only
    /// `flush()` produces such updates, and those go to [`SlotMap::apply`]
    /// directly.
    NoStoredId,
    /// An RPS entry's id holds no slot: an earlier plan of this stream never
    /// went through this [`SlotMap`].
    UnresolvedReference(PicId),
    /// A slice reference-list entry names a picture outside the plan's current
    /// RPS sets. 8.3.4 builds every list FROM those sets, so this is a planner
    /// contract violation — the picture would be missing from
    /// `pReferenceSlots` and the hardware could not resolve it.
    ReferenceOutsideRps(PicId),
    Slot(SlotError),
    /// A slice offset exceeds `u32` (Vulkan submits offsets as `u32`).
    OffsetOverflow(usize),
    /// A current RPS set holds more entries than the Std index arrays' eight
    /// ([`H265_RPS_LIST_SIZE`]) — expressible in H.265, not in Vulkan; outside
    /// the program envelope (punktfunk hosts keep well under it).
    RpsSetOverflow {
        set: &'static str,
        len: usize,
    },
    /// The first slice's inline `st_ref_pic_set()` predicts from an SPS
    /// candidate that does not exist — `NumDeltaPocsOfRefRpsIdx` cannot be
    /// derived, and the hardware would misparse the slice header.
    InvalidRefRpsIdx {
        curr_rps_idx: u8,
        delta_idx_minus1: u8,
    },
    /// The inline `st_ref_pic_set()`'s bit count exceeds `u16` (the Std field
    /// `NumBitsForSTRefPicSetInSlice`) — a header that large is corrupt.
    StRpsBitsOverflow(u32),
    /// The predicted-from candidate's `NumDeltaPocs` exceeds `u8` (the Std
    /// field `NumDeltaPocsOfRefRpsIdx`). Impossible off a real parse (≤ 32);
    /// a directly-constructed plan gets an error, never a clamped count the
    /// hardware would misparse the slice header with.
    NumDeltaPocsOverflow(u32),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB. The session
    /// must rebuild the video session and its [`SlotMap`]; converting against
    /// the stale map would hand out slot indices the session's image pool does
    /// not have (the H.264 module's exact contract).
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
}

impl std::fmt::Display for PlanToVkH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkH265Error::NoSlices => write!(f, "the plan holds no slices"),
            PlanToVkH265Error::NoStoredId => {
                write!(
                    f,
                    "the plan stores no picture (flush updates go to SlotMap::apply)"
                )
            }
            PlanToVkH265Error::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToVkH265Error::ReferenceOutsideRps(id) => {
                write!(
                    f,
                    "slice list references picture {id} outside the current RPS sets"
                )
            }
            PlanToVkH265Error::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToVkH265Error::OffsetOverflow(offset) => {
                write!(f, "slice offset {offset} exceeds u32")
            }
            PlanToVkH265Error::RpsSetOverflow { set, len } => {
                write!(
                    f,
                    "{set} holds {len} entries; Vulkan expresses at most {H265_RPS_LIST_SIZE}"
                )
            }
            PlanToVkH265Error::InvalidRefRpsIdx {
                curr_rps_idx,
                delta_idx_minus1,
            } => {
                write!(
                    f,
                    "inline st_ref_pic_set predicts from a nonexistent candidate \
                     (CurrRpsIdx {curr_rps_idx}, delta_idx_minus1 {delta_idx_minus1})"
                )
            }
            PlanToVkH265Error::StRpsBitsOverflow(bits) => {
                write!(f, "st_ref_pic_set bit count {bits} exceeds u16")
            }
            PlanToVkH265Error::NumDeltaPocsOverflow(count) => {
                write!(f, "candidate NumDeltaPocs {count} exceeds u8")
            }
            PlanToVkH265Error::CapacityMismatch { required, capacity } => {
                write!(
                    f,
                    "the plan needs {required} slots but the map holds {capacity} — \
                     an SPS renegotiation resized the DPB; rebuild session and map"
                )
            }
        }
    }
}

impl std::error::Error for PlanToVkH265Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToVkH265Error::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToVkH265Error {
    fn from(err: SlotError) -> Self {
        PlanToVkH265Error::Slot(err)
    }
}

/// One [`RefPic`] as Std reference info.
fn ref_info(rp: &RefPic) -> hh::StdVideoDecodeH265ReferenceInfo {
    // SAFETY: StdVideoDecodeH265ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word and one integer; all-zero is a valid value for every field.
    let mut std: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_used_for_long_term_reference(u32::from(rp.is_long_term));
    // unused_for_reference stays 0: membership in a CURRENT set is the
    // definition of being used for reference by this picture.
    std.PicOrderCntVal = rp.pic_order_cnt;
    std
}

/// `NumDeltaPocsOfRefRpsIdx` (the Std picture-info field): when the first
/// slice's inline `st_ref_pic_set()` uses inter-RPS prediction, the hardware
/// re-parses those slice bits and needs `NumDeltaPocs[RefRpsIdx]` of the SOURCE
/// candidate to size the `used_by_curr_pic_flag`/`use_delta_flag` loop (7.4.8);
/// otherwise 0.
fn num_delta_pocs_of_ref_rps_idx(plan: &AuPlan) -> Result<u8, PlanToVkH265Error> {
    let hdr = &plan
        .slices
        .first()
        .expect("caller validated the plan holds slices")
        .header;
    // Inline means CurrRpsIdx == num_short_term_ref_pic_sets (8.3.2 NOTE 2);
    // an SPS-indexed RPS re-parses nothing in the slice header.
    let inline = !hdr.short_term_ref_pic_set_sps_flag
        && hdr.curr_rps_idx == plan.sps.num_short_term_ref_pic_sets;
    if !inline || !hdr.short_term_ref_pic_set.inter_ref_pic_set_prediction_flag {
        return Ok(0);
    }
    // RefRpsIdx = stRpsIdx - (delta_idx_minus1 + 1), stRpsIdx = CurrRpsIdx here
    // (equation 7-59). u16 arithmetic so a hostile delta cannot wrap.
    let delta = hdr.short_term_ref_pic_set.delta_idx_minus1;
    let source = u16::from(hdr.curr_rps_idx)
        .checked_sub(u16::from(delta) + 1)
        .and_then(|idx| plan.sps.short_term_ref_pic_set.get(usize::from(idx)))
        .ok_or(PlanToVkH265Error::InvalidRefRpsIdx {
            curr_rps_idx: hdr.curr_rps_idx,
            delta_idx_minus1: delta,
        })?;
    // NumDeltaPocs = num_negative + num_positive <= 32 off any real parse,
    // comfortably u8 — but a directly-constructed plan could exceed it, and a
    // silently clamped count would misparse the slice header on hardware:
    // typed error, like everything else in this file.
    u8::try_from(source.num_delta_pocs)
        .map_err(|_| PlanToVkH265Error::NumDeltaPocsOverflow(source.num_delta_pocs))
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// Unlike the H.264 [`crate::plan_to_vk`], no `sps_id` parameter: an H.265
/// [`AuPlan`] carries its activated SPS and PPS, so every id resolves from the
/// plan itself.
///
/// Atomicity contract (identical to the H.264 module): every fallible step runs
/// before any mutation of `slots`, so an error leaves the map exactly as it was.
/// In order:
/// 1. capacity is validated against the plan's `max_dpb_frames` (read-only);
/// 2. the RPS binding set resolves against the PRE-removal state (read-only) —
///    a current-set member always survives its own AU (8.3.2's marking keeps it
///    referenced), but resolving before removals keeps the transaction shape
///    byte-for-byte the H.264 one and costs nothing;
/// 3. slice lists are cross-checked and offsets validated (read-only);
/// 4. `removed` is applied — removals were real regardless of this AU's fate —
///    and the setup slot is assigned last. A stored-and-evicted picture (its id
///    in this same plan's `removed`) still gets its slot for the decode itself
///    and is released right after, exactly the H.264 defensive path.
pub fn plan_to_vk_h265(
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanVkH265, PlanToVkH265Error> {
    let first_slice = plan.slices.first().ok_or(PlanToVkH265Error::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToVkH265Error::NoStoredId)?;

    // The map must match THIS plan's DPB depth; a mismatch means an SPS
    // renegotiation resized the DPB and the session must be rebuilt.
    let required = plan.picture.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVkH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // The AU-level binding set: the union of the three current RPS sets, first
    // appearance first (module docs — Vulkan H.265 keys everything by these,
    // not by slice lists). Each picture appears once even if a corrupt stream's
    // concealment resolved two entries to the same stored picture.
    let mut refs: Vec<VkRefH265> = Vec::new();
    let mut index_arrays = [[UNUSED_RPS_ENTRY; H265_RPS_LIST_SIZE]; 3];
    let sets: [(&'static str, &[RefPic]); 3] = [
        ("RefPicSetStCurrBefore", &plan.rps.st_curr_before),
        ("RefPicSetStCurrAfter", &plan.rps.st_curr_after),
        ("RefPicSetLtCurr", &plan.rps.lt_curr),
    ];
    for (array, (name, set)) in index_arrays.iter_mut().zip(sets) {
        if set.len() > H265_RPS_LIST_SIZE {
            return Err(PlanToVkH265Error::RpsSetOverflow {
                set: name,
                len: set.len(),
            });
        }
        for (position, rp) in set.iter().enumerate() {
            let entry = match refs.iter().position(|existing| existing.id == rp.id) {
                Some(index) => {
                    // A concealment-resolved duplicate across sets (doc above):
                    // the stored picture binds ONCE, but if ANY occurrence
                    // marks it long-term the binding must say so — hardware
                    // treats LT references differently (no MV scaling, POC-LSB
                    // matching), and an `RefPicSetLtCurr` index into a
                    // short-term-marked slot is an internally inconsistent DPB.
                    if rp.is_long_term {
                        refs[index].std.flags.set_used_for_long_term_reference(1);
                    }
                    refs[index].slot
                }
                None => {
                    let slot = slots
                        .slot_of(rp.id)
                        .ok_or(PlanToVkH265Error::UnresolvedReference(rp.id))?;
                    refs.push(VkRefH265 {
                        slot,
                        std: ref_info(rp),
                        id: rp.id,
                    });
                    slot
                }
            };
            // A DPB SLOT index, not a position in `refs` — see the
            // `DecodePlanVkH265::std_pic` docs. Slots are bounded by the session's
            // `maxDpbSlots` (<= 17 under this crate's envelope), so no real slot
            // can collide with the 0xFF sentinel.
            debug_assert_ne!(entry, UNUSED_RPS_ENTRY, "a real DPB slot is never 0xFF");
            array[position] = entry;
        }
    }

    // Cross-check: 8.3.4 builds every slice list from the current sets, so any
    // entry outside the binding set is a planner-contract violation the
    // hardware could not resolve (error-type docs).
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if !refs.iter().any(|existing| existing.id == rp.id) {
                return Err(PlanToVkH265Error::ReferenceOutsideRps(rp.id));
            }
        }
    }

    let pic = &plan.picture;

    // SAFETY: StdVideoDecodeH265PictureInfo is a plain-C bindgen struct of a
    // bitfield word, integers and byte arrays; all-zero is a valid value for
    // every field.
    let mut std_pic: hh::StdVideoDecodeH265PictureInfo = unsafe { std::mem::zeroed() };
    std_pic.flags.set_IrapPicFlag(u32::from(pic.is_irap));
    std_pic.flags.set_IdrPicFlag(u32::from(pic.is_idr));
    std_pic.flags.set_IsReference(u32::from(pic.is_reference));
    std_pic.flags.set_short_term_ref_pic_set_sps_flag(u32::from(
        first_slice.header.short_term_ref_pic_set_sps_flag,
    ));
    std_pic.sps_video_parameter_set_id = plan.sps.video_parameter_set_id;
    std_pic.pps_seq_parameter_set_id = plan.pps.seq_parameter_set_id;
    std_pic.pps_pic_parameter_set_id = plan.pps.pic_parameter_set_id;
    std_pic.NumDeltaPocsOfRefRpsIdx = num_delta_pocs_of_ref_rps_idx(plan)?;
    std_pic.PicOrderCntVal = pic.pic_order_cnt;
    // 0 when the RPS came from the SPS by index (PicturePlan field docs) —
    // exactly Vulkan's convention for this field.
    std_pic.NumBitsForSTRefPicSetInSlice = u16::try_from(pic.short_term_ref_pic_set_size_bits)
        .map_err(|_| PlanToVkH265Error::StRpsBitsOverflow(pic.short_term_ref_pic_set_size_bits))?;
    [
        std_pic.RefPicSetStCurrBefore,
        std_pic.RefPicSetStCurrAfter,
        std_pic.RefPicSetLtCurr,
    ] = index_arrays;

    // The setup slot's reference info: the picture's own identity, short-term
    // (see the DecodePlanVkH265 field docs for why there is no long-term leg
    // here, unlike H.264).
    // SAFETY: as above — all-zero is a valid StdVideoDecodeH265ReferenceInfo.
    let mut setup_ref: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
    setup_ref.PicOrderCntVal = pic.pic_order_cnt;

    let mut slice_offsets = Vec::with_capacity(plan.slices.len());
    for slice in &plan.slices {
        // SlicePlan.data starts at the slice NALU's start code — exactly the
        // offset Vulkan wants (struct docs).
        slice_offsets.push(
            u32::try_from(slice.data.start)
                .map_err(|_| PlanToVkH265Error::OffsetOverflow(slice.data.start))?,
        );
    }

    // Mutations LAST, after every fallible step above (fn docs). Removals first
    // — they were real regardless of this AU's fate — then the setup
    // assignment, releasing immediately when this very plan already evicted the
    // stored picture (the H.264 defensive path; the slot must still exist for
    // the decode itself).
    let setup_evicted = plan.dpb.removed.contains(&setup_id);
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        if !slots.release(id) {
            // Tolerated but never silent: reachable only when the caller
            // skipped feeding an AU's plan through this map.
            trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
        }
    }
    let setup_slot = slots.assign(setup_id)?;
    if setup_evicted {
        slots.release(setup_id);
    }

    Ok(DecodePlanVkH265 {
        std_pic,
        slice_offsets,
        setup_slot,
        setup_ref,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::rc::Rc;

    use cros_codecs::codec::h265::parser::Nalu;
    use cros_codecs::codec::h265::parser::Pps;
    use cros_codecs::codec::h265::parser::ShortTermRefPicSet;
    use cros_codecs::codec::h265::parser::Sps;
    use pf_bitstream::h265::ColourDescription;
    use pf_bitstream::h265::DisplayCrop;
    use pf_bitstream::h265::DpbUpdate;
    use pf_bitstream::h265::H265Planner;
    use pf_bitstream::h265::Level;
    use pf_bitstream::h265::NaluType;
    use pf_bitstream::h265::PicturePlan;
    use pf_bitstream::h265::RpsPlan;
    use pf_bitstream::h265::SliceHeader;
    use pf_bitstream::h265::SlicePlan;

    use super::*;

    // The same vendored vectors pf-bitstream's h265 tests plan (its goldens:
    // 250 AUs / 250 slices for the 25fps clip), included from the same path.
    const TEST_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );
    const TEST_64X64_I_P_B_P: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/64x64-I-P-B-P.h265"
    );

    /// Test-only AU splitter, mirroring pf-bitstream's h265 helper (which is
    /// `#[cfg(test)]`-private there): a new AU starts at a non-VCL NALU
    /// following slices, or at a slice segment with
    /// `first_slice_segment_in_pic_flag == 1` (the first bit of the byte after
    /// the 2-byte NAL header) when the current AU already has slices.
    fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut cursor = Cursor::new(stream);
        let mut au_start = 0usize;
        let mut au_has_slice = false;

        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let header_start = cursor.position() as usize;
            let start = header_start - nalu.offset;
            let is_slice = (nalu.header.type_ as u32) < 32;
            let first_slice_flag =
                is_slice && stream.get(header_start + 2).is_some_and(|b| b & 0x80 != 0);

            if au_has_slice && (!is_slice || first_slice_flag) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// **Our own host's low-delay HEVC**, vendored beside the goldens the GPU legs
    /// decode it against (`tests/data/lowdelay-640x480-h265.nv12.sha256` carries the
    /// `punktfunk-host spike` command and the ffmpeg cross-check). 120 pictures of
    /// 640x480 IPPP, a five-picture DPB against four marked references and
    /// `sps_max_num_reorder_pics = 0`, so 115 of its 120 access units retire a picture.
    const LOWDELAY_640X480_H265: &[u8] = include_bytes!("../tests/data/lowdelay-640x480.h265");

    /// This rung is immune to the release-ordering defect for a STRONGER reason than
    /// the DXVA one, and this pins the difference instead of asserting it in prose.
    ///
    /// Both HEVC conversions release `removed` inline and let [`SlotMap::assign`] hand
    /// the freed slot to the decode target. DXVA survives that because `H265Planner`
    /// snapshots `dpb_refs` AFTER `decode_rps`, so the retired picture is not in the
    /// marked DPB `RefPicList` is built from — a property of the PLANNER, one call
    /// away from being untrue, which is why `pf_dxvadec::pic_h265` drives the
    /// counterfactual through its conversion.
    ///
    /// This conversion never reads `dpb_refs` at all. `pReferenceSlots` is spec-defined
    /// as the slots the decode operation USES, so it binds `plan.rps` — the three
    /// current sets, which `decode_rps` derives and which therefore cannot name a
    /// picture that same RPS just dropped. The test proves it the only way that means
    /// anything: it hands the conversion a `dpb_refs` deliberately widened to the
    /// PRE-RPS marked set (the mutation that makes the DXVA rung alias on 115 of these
    /// 120 access units) and asserts nothing changes here.
    ///
    /// If this ever fails, someone has made this conversion bind the marked DPB — a
    /// legitimate thing to want, since a *Foll* long-term anchor invisible to the
    /// hardware is the RFI failure shape — and it now needs the `release_after_decode`
    /// deferral the H.264 and AV1 conversions carry.
    #[test]
    fn a_pre_rps_marked_dpb_changes_nothing_here_because_the_current_sets_are_what_bind() {
        let aus = split_into_aus(LOWDELAY_640X480_H265);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut prev: Option<AuPlan> = None;
        let mut converted = 0usize;
        let mut widened = 0usize;

        for au in aus {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));

            // The marked DPB as this AU's `decode_rps` found it: the previous AU's
            // snapshot plus the picture it stored. Exactly what `dpb_snapshot()` would
            // return from above `decode_rps` instead of below it.
            let mut as_if = plan.clone();
            as_if.dpb_refs = match &prev {
                None => Vec::new(),
                Some(prev) => {
                    let mut marked = prev.dpb_refs.clone();
                    if let Some(id) = prev.dpb.stored {
                        assert!(
                            prev.picture.is_reference,
                            "this stream carries no sub-layer non-reference pictures"
                        );
                        marked.push(RefPic {
                            id,
                            pic_order_cnt: prev.picture.pic_order_cnt,
                            is_long_term: false,
                        });
                    }
                    marked
                }
            };
            if as_if.dpb_refs.len() > plan.dpb_refs.len() {
                widened += 1;
            }

            let vk = plan_to_vk_h265(&as_if, map).expect("conversion");
            converted += 1;
            for r in &vk.refs {
                assert_ne!(
                    r.slot, vk.setup_slot,
                    "a reference aliases the setup slot even though this conversion \
                     binds only the current RPS sets — it has started reading \
                     `dpb_refs`, and now needs a deferred release"
                );
            }
            prev = Some(plan);
        }

        assert_eq!(converted, 120);
        assert_eq!(
            widened, 115,
            "the mutation must actually widen the marked set on the access units that \
             retire a picture, else this test asserts nothing about anything"
        );
    }

    #[test]
    fn the_full_25fps_vector_converts_with_stable_slots_and_start_code_offsets() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        // PicId -> the slot it was assigned; entries leave only on `removed`.
        let mut held: BTreeMap<PicId, u8> = BTreeMap::new();
        let mut converted = 0usize;
        // AUs whose `refs` are NOT in slot order — the AUs on which "DPB slot" and
        // "position in refs" are distinguishable (see the loop body).
        let mut slot_order_differs = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk_h265(&plan, slots).expect("the clean vector converts");
            converted += 1;

            // Slot stability: every reference resolves to the slot its picture
            // was assigned when IT was decoded, and no ref shares the setup slot.
            for r in &vk.refs {
                assert_eq!(
                    held.get(&r.id),
                    Some(&r.slot),
                    "a referenced picture's slot changed while it was referenced"
                );
                assert_ne!(r.slot, vk.setup_slot, "a reference aliases the setup slot");
            }

            // The Std index arrays resolve exactly the plan's RPS sets, in
            // order, 0xFF beyond — BY DPB SLOT (struct docs), and every slot they
            // name is one the decode op will bind, i.e. one of `refs`'.
            for (array, set) in [
                (&vk.std_pic.RefPicSetStCurrBefore, &plan.rps.st_curr_before),
                (&vk.std_pic.RefPicSetStCurrAfter, &plan.rps.st_curr_after),
                (&vk.std_pic.RefPicSetLtCurr, &plan.rps.lt_curr),
            ] {
                for (position, entry) in array.iter().enumerate() {
                    match set.get(position) {
                        Some(rp) => {
                            let r =
                                vk.refs
                                    .iter()
                                    .find(|r| r.slot == *entry)
                                    .unwrap_or_else(|| {
                                        panic!(
                                            "AU {converted}: RPS entry names DPB slot {entry}, \
                                         which this decode op does not bind ({:?}) — the \
                                         hardware cannot resolve it",
                                            vk.refs.iter().map(|r| r.slot).collect::<Vec<_>>()
                                        )
                                    });
                            assert_eq!(r.id, rp.id, "AU {converted}: the slot's picture");
                            assert_eq!(r.std.PicOrderCntVal, rp.pic_order_cnt);
                        }
                        None => assert_eq!(*entry, UNUSED_RPS_ENTRY),
                    }
                }
            }
            // …and the vector genuinely DISCRIMINATES the two readings, so this
            // assertion can never quietly become vacuous: on a freshly anchored
            // stream "DPB slot" and "position in refs" agree, and a test that only
            // ever saw agreeing AUs would pass either way. Count the AUs where
            // they disagree; the total is asserted below.
            if vk
                .refs
                .iter()
                .enumerate()
                .any(|(position, r)| usize::from(r.slot) != position)
            {
                slot_order_differs += 1;
            }

            // Slice offsets: one per slice, each at a start-code boundary of
            // the AU, exactly where the plan said the slice begins.
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

            assert_eq!(vk.std_pic.PicOrderCntVal, plan.picture.pic_order_cnt);
            assert_eq!(
                u32::from(plan.picture.is_idr),
                vk.std_pic.flags.IdrPicFlag()
            );
            assert_eq!(
                u32::from(plan.picture.is_irap),
                vk.std_pic.flags.IrapPicFlag()
            );
            assert_eq!(
                u32::from(plan.picture.is_reference),
                vk.std_pic.flags.IsReference()
            );
            assert_eq!(vk.setup_ref.PicOrderCntVal, plan.picture.pic_order_cnt);
            assert_eq!(
                vk.std_pic.NumBitsForSTRefPicSetInSlice,
                plan.picture.short_term_ref_pic_set_size_bits as u16
            );

            // Mirror the map's bookkeeping: record the new picture, drop the
            // removed.
            let stored = plan.dpb.stored.unwrap();
            assert_eq!(vk.setup_id, stored);
            assert_eq!(vk.setup_is_reference, plan.picture.is_reference);
            held.insert(stored, vk.setup_slot);
            for id in &plan.dpb.removed {
                held.remove(id);
            }

            // held() must mirror the plan-driven bookkeeping exactly, every AU.
            let ledger: BTreeMap<PicId, u8> = slots.held().map(|(slot, id)| (id, slot)).collect();
            assert_eq!(ledger, held);
        }

        assert_eq!(converted, 250, "the vector's own golden");
        // The vector's hierarchical-B RPS sets put the references out of slot order
        // from AU 3 on (its first B picture with two `StCurrAfter` entries binds
        // slots 2 then 1), so the index-array assertions above are decisive rather
        // than accidental. 247 of 250: AUs 0-2 agree, which is precisely why the
        // wrong reading survived to hardware and produced three correct frames
        // followed by 247 corrupt ones.
        assert_eq!(
            slot_order_differs, 247,
            "the vector must distinguish DPB slots from positions in refs"
        );

        // Teardown: the flush update releases every remaining slot — the
        // codec-neutral DpbUpdate drives the SAME SlotMap H.264 uses.
        let mut slots = slots.unwrap();
        slots.apply(&planner.flush());
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn the_b_frame_vector_populates_both_current_index_arrays_around_the_picture() {
        let aus = split_into_aus(TEST_64X64_I_P_B_P);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut b_pictures_seen = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk_h265(&plan, slots).expect("the clean vector converts");

            if plan.rps.st_curr_after.is_empty() {
                continue;
            }
            b_pictures_seen += 1;
            // 8.3.2: StCurrBefore entries sit below the picture's POC,
            // StCurrAfter above — resolved through the index arrays' DPB SLOTS.
            let bound = |slot: u8| {
                vk.refs
                    .iter()
                    .find(|r| r.slot == slot)
                    .unwrap_or_else(|| panic!("RPS entry names unbound DPB slot {slot}"))
            };
            let before = vk.std_pic.RefPicSetStCurrBefore[0];
            let after = vk.std_pic.RefPicSetStCurrAfter[0];
            assert!(bound(before).std.PicOrderCntVal < plan.picture.pic_order_cnt);
            assert!(bound(after).std.PicOrderCntVal > plan.picture.pic_order_cnt);
            assert_ne!(before, after, "distinct slots on the two sides");
        }

        assert!(b_pictures_seen > 0, "the vector must contain B pictures");
    }

    // ------- hand-built plan fixtures (the vendored crate has no H.265
    // synthesizer; every AuPlan field is public, so edge cases construct the
    // planner's output shape directly — the contract under test is the plan,
    // not the bitstream) -------

    fn mini_sps() -> Rc<Sps> {
        Rc::new(Sps {
            video_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            pic_width_in_luma_samples: 64,
            pic_height_in_luma_samples: 64,
            ..Default::default()
        })
    }

    fn mini_pps(sps: &Rc<Sps>) -> Rc<Pps> {
        // The vendored Pps derives no Default; only the fields this module
        // reads (the two ids and the SPS chain) carry meaning here.
        Rc::new(Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            dependent_slice_segments_enabled_flag: false,
            output_flag_present_flag: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_enabled_flag: false,
            cabac_init_present_flag: false,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            init_qp_minus26: 0,
            constrained_intra_pred_flag: false,
            transform_skip_enabled_flag: false,
            cu_qp_delta_enabled_flag: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present_flag: false,
            weighted_pred_flag: false,
            weighted_bipred_flag: false,
            transquant_bypass_enabled_flag: false,
            tiles_enabled_flag: false,
            entropy_coding_sync_enabled_flag: false,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            uniform_spacing_flag: true,
            column_width_minus1: [0; 20],
            row_height_minus1: [0; 22],
            loop_filter_across_tiles_enabled_flag: true,
            loop_filter_across_slices_enabled_flag: false,
            deblocking_filter_control_present_flag: false,
            deblocking_filter_override_enabled_flag: false,
            deblocking_filter_disabled_flag: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            scaling_list_data_present_flag: false,
            scaling_list: Default::default(),
            lists_modification_present_flag: false,
            log2_parallel_merge_level_minus2: 0,
            slice_segment_header_extension_present_flag: false,
            extension_present_flag: false,
            range_extension_flag: false,
            range_extension: Default::default(),
            scc_extension_flag: false,
            scc_extension: Default::default(),
            qp_bd_offset_y: 0,
            sps: Rc::clone(sps),
        })
    }

    fn mini_picture(poc: i32, max_dpb_frames: usize) -> PicturePlan {
        PicturePlan {
            nalu_type: if poc == 0 {
                NaluType::IdrWRadl
            } else {
                NaluType::TrailR
            },
            is_idr: poc == 0,
            is_irap: poc == 0,
            no_rasl_output_flag: poc == 0,
            is_reference: true,
            pic_order_cnt: poc,
            coded_width: 64,
            coded_height: 64,
            display_crop: DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            colour: ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            general_profile_idc: 1,
            level_idc: Level::L4,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            chroma_format_idc: 1,
            max_dpb_frames,
            short_term_ref_pic_set_size_bits: 0,
            recovery_point: None,
            // These fixtures model a healthy stream; the clean bit is the planner's
            // observation and nothing in this conversion layer reads it.
            references_clean: true,
        }
    }

    fn mini_slice(refs0: &[RefPic], refs1: &[RefPic]) -> SlicePlan {
        SlicePlan {
            data: 0..32,
            header: SliceHeader::default(),
            ref_list0: refs0.to_vec(),
            ref_list1: refs1.to_vec(),
        }
    }

    /// A plan storing `stored` with the given RPS sets and one slice whose
    /// list0 is the concatenation the 8-8 temporal order would produce.
    fn mini_plan(
        stored: PicId,
        poc: i32,
        rps: RpsPlan,
        removed: Vec<PicId>,
        max_dpb_frames: usize,
    ) -> AuPlan {
        let sps = mini_sps();
        let pps = mini_pps(&sps);
        let mut list0: Vec<RefPic> = Vec::new();
        list0.extend(rps.st_curr_before.iter().copied());
        list0.extend(rps.st_curr_after.iter().copied());
        list0.extend(rps.lt_curr.iter().copied());
        // The marked DPB is this AU's current sets and nothing more here: the Vulkan
        // rung binds `pReferenceSlots` from the CURRENT sets (the slots this decode
        // operation uses), so the snapshot is not an input to anything under test.
        let dpb_refs = list0.clone();
        AuPlan {
            picture: mini_picture(poc, max_dpb_frames),
            rps,
            slices: vec![mini_slice(&list0, &[])],
            dpb: DpbUpdate {
                stored: Some(stored),
                outputs: vec![stored],
                removed,
            },
            dpb_refs,
            warnings: Vec::new(),
            sps,
            pps,
        }
    }

    fn st_ref(id: PicId, poc: i32) -> RefPic {
        RefPic {
            id,
            pic_order_cnt: poc,
            is_long_term: false,
        }
    }

    fn lt_ref(id: PicId, poc: i32) -> RefPic {
        RefPic {
            id,
            pic_order_cnt: poc,
            is_long_term: true,
        }
    }

    #[test]
    fn a_long_term_rps_entry_carries_the_flag_and_its_index_lands_in_lt_curr() {
        let mut slots = SlotMap::new(4);
        // Two pictures already decoded through this map: the anchor (id 10,
        // poc 0, pinned long-term) and the previous picture (id 11, poc 1).
        slots.assign(10).unwrap();
        slots.assign(11).unwrap();

        let plan = mini_plan(
            12,
            2,
            RpsPlan {
                st_curr_before: vec![st_ref(11, 1)],
                st_curr_after: Vec::new(),
                lt_curr: vec![lt_ref(10, 0)],
            },
            Vec::new(),
            4,
        );
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();

        assert_eq!(vk.refs.len(), 2);
        // The entries are DPB SLOTS: id 11 took slot 1 and id 10 slot 0, while the
        // RPS set order binds id 11 FIRST — so a positional reading would name slot
        // 0 for the short-term set and get the long-term anchor instead. That is the
        // whole M3 field failure in one fixture.
        assert_eq!(vk.std_pic.RefPicSetStCurrBefore[0], 1, "id 11's DPB slot");
        assert_eq!(vk.std_pic.RefPicSetLtCurr[0], 0, "id 10's DPB slot");
        let bound = |slot: u8| vk.refs.iter().find(|r| r.slot == slot).expect("bound");
        let st = bound(vk.std_pic.RefPicSetStCurrBefore[0]);
        assert_eq!(st.id, 11);
        assert_eq!(st.std.flags.used_for_long_term_reference(), 0);
        let lt = bound(vk.std_pic.RefPicSetLtCurr[0]);
        assert_eq!(lt.id, 10);
        assert_eq!(lt.std.flags.used_for_long_term_reference(), 1);
        assert_eq!(lt.std.PicOrderCntVal, 0);
        assert_eq!(vk.std_pic.RefPicSetStCurrAfter[0], UNUSED_RPS_ENTRY);
        // The setup picture itself activates short-term (no same-AU
        // self-marking in HEVC — struct docs).
        assert_eq!(vk.setup_ref.flags.used_for_long_term_reference(), 0);
        assert_eq!(vk.setup_ref.PicOrderCntVal, 2);
    }

    #[test]
    fn a_failed_conversion_leaves_the_slot_map_untouched_and_the_session_recovers() {
        // A right-sized map that never saw the reference's AU, holding one
        // unrelated slot: the reference must fail loudly, not resolve to a
        // fabricated slot.
        let mut slots = SlotMap::new(4);
        slots.assign(999).unwrap();

        let plan = mini_plan(
            5,
            1,
            RpsPlan {
                st_curr_before: vec![st_ref(4, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            vec![3], // a removal that must NOT be applied on the failed path
            4,
        );
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::UnresolvedReference(4)
        );

        // Atomicity: the failed conversion mutated nothing.
        assert_eq!(slots.active(), 1);
        assert_eq!(slots.held().collect::<Vec<_>>(), vec![(0, 999)]);

        // And the session recovers: the next valid plan (an IDR restart whose
        // `removed` names ids this map never assigned — tolerated by design)
        // still converts on the same map.
        let idr = mini_plan(6, 0, RpsPlan::default(), vec![4, 5], 4);
        let vk = plan_to_vk_h265(&idr, &mut slots).unwrap();
        assert_eq!(vk.setup_slot, 1, "the lowest free slot after the held one");
        assert_eq!(slots.active(), 2);
    }

    #[test]
    fn an_sps_switch_that_resizes_the_dpb_is_a_capacity_mismatch_not_a_guess() {
        // The map was built for a 6-deep DPB; a renegotiated stream plans with
        // 16. Refuse, so the session rebuilds session + map instead of handing
        // out slots the image pool does not have.
        let mut slots = SlotMap::new(6);
        plan_to_vk_h265(
            &mini_plan(0, 0, RpsPlan::default(), Vec::new(), 6),
            &mut slots,
        )
        .unwrap();

        let renegotiated = mini_plan(1, 0, RpsPlan::default(), Vec::new(), 16);
        assert_eq!(
            plan_to_vk_h265(&renegotiated, &mut slots).unwrap_err(),
            PlanToVkH265Error::CapacityMismatch {
                required: 17,
                capacity: 7
            }
        );
        // And the mismatch mutated nothing.
        assert_eq!(slots.active(), 1);
    }

    #[test]
    fn empty_and_flush_shaped_plans_are_rejected_with_typed_errors() {
        let mut slots = SlotMap::new(4);

        let mut no_slices = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        no_slices.slices.clear();
        assert_eq!(
            plan_to_vk_h265(&no_slices, &mut slots).unwrap_err(),
            PlanToVkH265Error::NoSlices
        );

        let mut no_stored = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        no_stored.dpb.stored = None;
        assert_eq!(
            plan_to_vk_h265(&no_stored, &mut slots).unwrap_err(),
            PlanToVkH265Error::NoStoredId
        );

        let mut huge_offset = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        huge_offset.slices[0].data = (u32::MAX as usize + 1)..(u32::MAX as usize + 40);
        assert_eq!(
            plan_to_vk_h265(&huge_offset, &mut slots).unwrap_err(),
            PlanToVkH265Error::OffsetOverflow(u32::MAX as usize + 1)
        );
        assert_eq!(slots.active(), 0, "every rejection left the map untouched");
    }

    #[test]
    fn an_rps_set_deeper_than_the_std_index_arrays_is_rejected_not_truncated() {
        let mut slots = SlotMap::new(16);
        for id in 0..9u64 {
            slots.assign(id).unwrap();
        }
        let deep: Vec<RefPic> = (0..9).map(|i| st_ref(i, i as i32)).collect();
        let plan = mini_plan(
            20,
            9,
            RpsPlan {
                st_curr_before: deep,
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            Vec::new(),
            16,
        );
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::RpsSetOverflow {
                set: "RefPicSetStCurrBefore",
                len: 9
            }
        );
        assert_eq!(slots.active(), 9, "the rejection mutated nothing");
    }

    #[test]
    fn a_slice_list_entry_outside_the_rps_sets_fails_closed() {
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        let mut plan = mini_plan(
            3,
            2,
            RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            Vec::new(),
            4,
        );
        // Id 2 holds a slot but is in NO current set: it would be missing from
        // pReferenceSlots, so the hardware could not resolve the list entry.
        plan.slices[0].ref_list0.push(st_ref(2, 1));
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::ReferenceOutsideRps(2)
        );
    }

    #[test]
    fn a_stored_and_evicted_picture_still_gets_a_slot_for_the_decode_itself() {
        // The defensive same-plan eviction path (H.264 parity): the stored id
        // appears in its own plan's `removed` — the slot exists during the
        // decode and is released right after, so the next picture can reuse it.
        let mut slots = SlotMap::new(1); // capacity 2
        let mut plan = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 1);
        plan.dpb.removed = vec![0];
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.setup_slot, 0);
        assert_eq!(slots.active(), 0, "released after assignment");
        assert_eq!(slots.slot_of(0), None);
    }

    #[test]
    fn num_delta_pocs_of_ref_rps_idx_derives_from_the_predicted_inline_rps() {
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();

        // The activated SPS carries two candidates; the inline slice RPS
        // predicts from the second (delta_idx_minus1 = 0 ⇒ RefRpsIdx = 1).
        let mut sps = (*mini_sps()).clone();
        sps.num_short_term_ref_pic_sets = 2;
        sps.short_term_ref_pic_set = vec![
            ShortTermRefPicSet {
                num_delta_pocs: 3,
                ..Default::default()
            },
            ShortTermRefPicSet {
                num_delta_pocs: 5,
                ..Default::default()
            },
        ];
        let sps = Rc::new(sps);
        let pps = mini_pps(&sps);

        let header = SliceHeader {
            short_term_ref_pic_set_sps_flag: false,
            curr_rps_idx: 2, // == num_short_term_ref_pic_sets: inline
            short_term_ref_pic_set: ShortTermRefPicSet {
                inter_ref_pic_set_prediction_flag: true,
                delta_idx_minus1: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut picture = mini_picture(1, 4);
        picture.short_term_ref_pic_set_size_bits = 23;
        let plan = AuPlan {
            picture,
            rps: RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            slices: vec![SlicePlan {
                data: 0..32,
                header,
                ref_list0: vec![st_ref(1, 0)],
                ref_list1: Vec::new(),
            }],
            dpb: DpbUpdate {
                stored: Some(2),
                outputs: vec![2],
                removed: Vec::new(),
            },
            dpb_refs: vec![st_ref(1, 0)],
            warnings: Vec::new(),
            sps: Rc::clone(&sps),
            pps: Rc::clone(&pps),
        };
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(
            vk.std_pic.NumDeltaPocsOfRefRpsIdx, 5,
            "the SOURCE set's count"
        );
        assert_eq!(vk.std_pic.flags.short_term_ref_pic_set_sps_flag(), 0);
        assert_eq!(vk.std_pic.NumBitsForSTRefPicSetInSlice, 23);

        // A prediction pointing past the candidate table cannot be derived.
        let mut broken = plan.clone();
        {
            let header = &mut broken.slices[0].header;
            header.short_term_ref_pic_set.delta_idx_minus1 = 2; // RefRpsIdx = -1
        }
        broken.dpb.stored = Some(3);
        assert_eq!(
            plan_to_vk_h265(&broken, &mut slots).unwrap_err(),
            PlanToVkH265Error::InvalidRefRpsIdx {
                curr_rps_idx: 2,
                delta_idx_minus1: 2
            }
        );
    }

    #[test]
    fn parameter_set_ids_flow_from_the_plans_activated_sets() {
        let mut slots = SlotMap::new(4);
        let mut sps = (*mini_sps()).clone();
        sps.video_parameter_set_id = 3;
        sps.seq_parameter_set_id = 7;
        let sps = Rc::new(sps);
        let mut pps = (*mini_pps(&sps)).clone();
        pps.pic_parameter_set_id = 9;
        pps.seq_parameter_set_id = 7;
        let pps = Rc::new(pps);

        let mut plan = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        plan.sps = sps;
        plan.pps = pps;
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.std_pic.sps_video_parameter_set_id, 3);
        assert_eq!(vk.std_pic.pps_seq_parameter_set_id, 7);
        assert_eq!(vk.std_pic.pps_pic_parameter_set_id, 9);
    }

    #[test]
    fn a_picture_referenced_by_two_sets_binds_one_slot_listed_once() {
        // Concealment can resolve an lsb-masked long-term entry and a
        // short-term entry to the SAME stored picture; Vulkan wants each slot
        // bound once, with both index arrays pointing at that one entry.
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();
        let plan = mini_plan(
            2,
            1,
            RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: vec![lt_ref(1, 0)],
            },
            Vec::new(),
            4,
        );
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.refs.len(), 1, "one binding for one picture");
        assert_eq!(
            vk.std_pic.RefPicSetStCurrBefore[0],
            vk.std_pic.RefPicSetLtCurr[0]
        );
        // The short-term set bound the picture FIRST, but the LtCurr occurrence
        // must still mark the shared binding long-term: hardware treats LT
        // references differently (no MV scaling, POC-LSB matching), and an
        // LtCurr index into a short-term-marked slot is an internally
        // inconsistent DPB the driver may reject or mispredict from.
        assert_eq!(vk.refs[0].std.flags.used_for_long_term_reference(), 1);
    }
}
