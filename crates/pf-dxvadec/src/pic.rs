//! Per-AU H.264 conversion: one [`AuPlan`] into the `DXVA_PicParams_H264`,
//! `DXVA_Qmatrix_H264` and slice-control records an
//! `ID3D11VideoContext::SubmitDecoderBuffers` call is built from —
//! [`pf_vkdecode::pic`]'s job, one hardware API over.
//!
//! # Surfaces ARE slots
//!
//! Vulkan's picture pool is deliberately decoupled from its DPB slots (a
//! re-activated slot binds a fresh image). DXVA has no such indirection: a
//! `DXVA_PicEntry_H264` carries the **uncompressed surface index**, so the DPB
//! slot index and the decode texture array's `ArraySlice` are the same number by
//! construction. That is why this module can drive the very same
//! [`SlotMap`](pf_vkdecode::SlotMap) the Vulkan rung uses without adapting it: the
//! ledger's contract — a picture keeps its slot for exactly as long as
//! pf-bitstream's DPB holds the picture — is precisely DXVA's surface lifetime
//! too.
//!
//! # `RefFrameList` is the MARKED DPB, not the AU's reference set
//!
//! The DXVA specification asks for every picture currently marked "used for
//! reference" to appear in `RefFrameList`, and `UsedForReferenceFlags` is a
//! statement about the DPB rather than about this access unit. libavcodec's DXVA
//! path fills it by walking its whole DPB — `short_ref` then `long_ref` — never
//! the derived lists.
//!
//! An earlier revision of this module put the union of the AU's own derived
//! reference lists there, on the strength of the native Vulkan rung binding the
//! same set bit-exact. That argument does not transfer, because the two APIs
//! define the field differently: Vulkan's `pReferenceSlots` is spec-defined as the
//! slots THIS decode operation uses, so a subset is right there. Where the
//! difference bites is not list derivation — the omitted pictures are the tail of
//! the sorted initial list, so 8.2.4.2/8.2.4.3 reproduce the same final list
//! either way, which is exactly why a smoke test cannot see it — but the
//! long-term/RFI class: a long-term reference held across pictures that name none
//! of them would vanish from `RefFrameList` for those AUs and reappear later, and
//! a driver keeping internal per-reference state is entitled to read the absence
//! as "no longer a reference", discard it, and decode the recovery against
//! whatever the surface then holds.
//!
//! So the array is built from [`pf_bitstream::h264::AuPlan::dpb_refs`], the marked
//! DPB pf-bitstream captures at begin-picture time, with the AU's own references
//! FIRST and the rest of the marked DPB appended. The order is this module's
//! choice, not the spec's — DXVA imposes none (a driver resolves an entry by its
//! `FrameNumList`/`FieldOrderCntList` pair) and libavcodec emits a different one,
//! so a byte-diff against libavcodec must compare this array as a SET. What the
//! ordering buys: a DPB deeper than the sixteen-entry array can only ever lose a
//! picture no slice of this AU names.
//!
//! The snapshot is also the AUTHORITY for each entry's marking, POC pair and
//! `FrameNumList` key, in preference to the slice lists' copy of them. That
//! matters under concealment: pf-bitstream substitutes a lost reference in place
//! and relabels the substitute short-term with its own `frame_num`, so a picture
//! the DPB holds long-term can appear short-term in a list. Feeding a driver an
//! `AssociatedFlag` of 0 with a `LongTermFrameIdx` in the `FrameNum` slot (or the
//! reverse) is how `LongTermPicNum` matching resolves to the wrong surface.
//!
//! # Progressive envelope
//!
//! pf-bitstream rejects interlaced streams before a plan exists, so
//! `field_pic_flag`, `MbaffFrameFlag`, `CurrPic.AssociatedFlag` and the
//! bottom-field halves of `UsedForReferenceFlags` are written for a frame and
//! nothing else. The top/bottom PicOrderCnt PAIRS are still real pairs — a
//! progressive frame's bottom count differs from its top whenever the PPS carries
//! `bottom_field_pic_order_in_frame_present_flag` — and ride through from
//! pf-bitstream verbatim.

use std::ops::Range;

use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;
use pf_vkdecode::SlotError;
use pf_vkdecode::SlotMap;
use tracing::trace;

use crate::dxva::H264BitFields;
use crate::dxva::PicEntry;
use crate::dxva::PicParamsH264;
use crate::dxva::QmatrixH264;
use crate::dxva::SliceH264Short;

/// `RefFrameList`'s length — the H.264 DXVA reference-frame ceiling, and the
/// spec's own maximum reference count.
const REF_FRAME_LIST_LEN: usize = 16;

/// One entry of `RefFrameList`: a picture the DPB holds marked for reference,
/// resolved to its surface index. Kept alongside the packed picture parameters for
/// the backend's logging and for tests that assert the mapping rather than the
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaRef {
    /// The DPB slot, which is the decode texture array's `ArraySlice`.
    pub slot: u8,
    pub id: PicId,
    pub is_long_term: bool,
    /// The stored picture's 8.2.1 field order counts — `FieldOrderCntList[i]`.
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// `FrameNumList[i]`: `frame_num` for a short-term reference,
    /// `LongTermFrameIdx` for a long-term one.
    pub frame_num_or_lt_idx: u16,
}

impl DxvaRef {
    fn new(slot: u8, rp: &RefPic) -> DxvaRef {
        DxvaRef {
            slot,
            id: rp.id,
            is_long_term: rp.is_long_term,
            top_field_order_cnt: rp.top_field_order_cnt,
            bottom_field_order_cnt: rp.bottom_field_order_cnt,
            frame_num_or_lt_idx: rp.frame_num_or_lt_idx,
        }
    }
}

/// Everything CPU-derivable of one AU's DXVA submission. The Windows layer adds
/// the live objects: the decoder, the mapped buffers and the output view.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodePlanDxva {
    pub pic_params: PicParamsH264,
    pub qmatrix: QmatrixH264,
    /// Byte ranges of the AU's slice NALUs, start code included, in plan order —
    /// exactly what [`crate::pack::pack`] takes.
    pub slice_ranges: Vec<Range<usize>>,
    /// The surface the decoded picture is written into
    /// (`CreateVideoDecoderOutputView` over this array slice, and
    /// `DecoderBeginFrame`'s target).
    pub setup_slot: u8,
    /// The planner id of the decoded picture (`AuPlan.dpb.stored`).
    pub setup_id: PicId,
    /// Whether later AUs may reference the decoded picture. When `false` the
    /// surface exists for the decode itself plus any remaining DPB residency,
    /// and may already have been released by this very AU's `removed`.
    pub setup_is_reference: bool,
    /// The marked DPB, resolved to surfaces — the AU's own references first, then
    /// every other marked picture (module docs). Laid out in exactly this order in
    /// `pic_params.RefFrameList`.
    pub refs: Vec<DxvaRef>,
    /// `MbWidth * MbHeight` of the coded picture: what libavcodec writes into the
    /// `NumMBsInBuffer` of the BITSTREAM and SLICE_CONTROL buffer descriptors on
    /// the H.264 path (`commit_bitstream_and_slice_buffer`, both slice formats).
    ///
    /// The picture parameters already carry the same two numbers, so this is a
    /// convenience rather than new information — but the Windows layer sees only
    /// the packed bytes, and a descriptor field libavcodec fills is not a field to
    /// leave at zero on the strength of its being redundant. A driver that
    /// validates the descriptor rejects at the first `SubmitDecoderBuffers`, which
    /// is precisely the failure this backend's "reproduce libavcodec exactly"
    /// method exists to avoid.
    pub mb_count: u32,
}

/// Conversion failures. Stream DAMAGE never lands here — pf-bitstream degrades it
/// to [`pf_bitstream::h264::PlanWarning`]s upstream; these are caller bugs, or
/// features outside what this backend submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaError {
    /// The plan holds no slices; there is nothing to submit.
    NoSlices,
    /// The plan's `DpbUpdate.stored` is `None`. `plan_au` always stores; only
    /// `flush()` produces such updates, and those go to
    /// [`SlotMap::apply`](pf_vkdecode::SlotMap::apply) directly.
    NoStoredId,
    /// A reference list entry's id holds no slot: an earlier plan of this stream
    /// never went through this [`SlotMap`].
    UnresolvedReference(PicId),
    Slot(SlotError),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB. The session must
    /// rebuild the decoder, its surface pool and its [`SlotMap`]; converting
    /// against the stale map would hand out surface indices the pool does not
    /// have.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// The AU references more distinct pictures than `RefFrameList` holds.
    /// Unreachable from a spec-conformant stream (16 is the H.264 ceiling too),
    /// and an error rather than a truncation because a dropped reference decodes
    /// to a wrong picture instead of a missing one.
    TooManyReferences(usize),
    /// Flexible macroblock ordering. `SliceGroupMap` would have to carry a
    /// derived map this backend does not build; punktfunk hosts never emit FMO,
    /// and libavcodec's own DXVA path does not implement it either. Refusing
    /// hands the session to the FFmpeg rung with a clean stream.
    SliceGroups {
        count: u32,
    },
    /// `separate_colour_plane_flag` (4:4:4 with independently coded planes).
    /// pf-bitstream's envelope gate refuses it upstream; checked again here
    /// because the picture-parameters layout for it is a different shape
    /// entirely.
    SeparateColourPlanes,
    /// A picture dimension in macroblocks exceeds the `USHORT` the picture
    /// parameters carry — 65536 macroblocks a side, i.e. a megapixel-scale
    /// impossibility off a real SPS.
    DimensionOverflow {
        width_mbs: u32,
        height_mbs: u32,
    },
}

impl std::fmt::Display for PlanToDxvaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaError::NoSlices => write!(f, "the plan holds no slices"),
            PlanToDxvaError::NoStoredId => write!(
                f,
                "the plan stores no picture (flush updates go to SlotMap::apply)"
            ),
            PlanToDxvaError::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToDxvaError::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToDxvaError::CapacityMismatch { required, capacity } => write!(
                f,
                "the plan needs {required} slots but the map holds {capacity} — \
                 an SPS renegotiation resized the DPB; rebuild decoder and map"
            ),
            PlanToDxvaError::TooManyReferences(count) => {
                write!(f, "{count} references exceed DXVA's RefFrameList of 16")
            }
            PlanToDxvaError::SliceGroups { count } => write!(
                f,
                "the PPS codes {count} slice groups (FMO), which this backend does not submit"
            ),
            PlanToDxvaError::SeparateColourPlanes => {
                write!(f, "separate_colour_plane_flag is outside this backend")
            }
            PlanToDxvaError::DimensionOverflow {
                width_mbs,
                height_mbs,
            } => write!(
                f,
                "a {width_mbs}x{height_mbs}-macroblock picture exceeds the DXVA picture parameters"
            ),
        }
    }
}

impl std::error::Error for PlanToDxvaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToDxvaError::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToDxvaError {
    fn from(err: SlotError) -> Self {
        PlanToDxvaError::Slot(err)
    }
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `status_id` becomes `StatusReportFeedbackNumber` — a per-picture tag the
/// driver echoes in its status reports. libavcodec makes it a monotonic counter
/// starting at 1; the caller does the same, because 0 is the value a driver reads
/// out of a buffer nobody wrote.
///
/// Atomicity contract, identical to [`pf_vkdecode::plan_to_vk`]'s: every fallible
/// step runs before any mutation of `slots`, so an error leaves the map exactly
/// as it was. In order:
/// 1. envelope and capacity are validated (read-only);
/// 2. references resolve against the PRE-removal state (read-only) — this AU's
///    own end-of-picture marking can evict a picture its slices legitimately
///    reference;
/// 3. `removed` is applied, then the setup slot is assigned last.
pub fn plan_to_dxva(
    plan: &AuPlan,
    slots: &mut SlotMap,
    status_id: u32,
) -> Result<DecodePlanDxva, PlanToDxvaError> {
    let first_slice = plan.slices.first().ok_or(PlanToDxvaError::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaError::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if pps.num_slice_groups_minus1 != 0 {
        return Err(PlanToDxvaError::SliceGroups {
            count: pps.num_slice_groups_minus1 + 1,
        });
    }
    if sps.separate_colour_plane_flag {
        return Err(PlanToDxvaError::SeparateColourPlanes);
    }

    // The map must match THIS plan's DPB depth; a mismatch means an SPS
    // renegotiation resized the DPB and the decoder must be rebuilt.
    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToDxvaError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // Picture dimensions in macroblocks. Height is expressed in FRAME
    // macroblocks (the DXVA spec is explicit that it is not a field count), so
    // the map-units count doubles for a non-frame-only SPS — unreachable inside
    // pf-bitstream's progressive envelope, written out anyway so the expression
    // says what the spec says rather than what our envelope happens to allow.
    let width_mbs = u32::from(sps.pic_width_in_mbs_minus1) + 1;
    let height_mbs = (u32::from(sps.pic_height_in_map_units_minus1) + 1)
        * (2 - u32::from(sps.frame_mbs_only_flag));
    let (Ok(width_minus1), Ok(height_minus1)) = (
        u16::try_from(width_mbs.saturating_sub(1)),
        u16::try_from(height_mbs.saturating_sub(1)),
    ) else {
        return Err(PlanToDxvaError::DimensionOverflow {
            width_mbs,
            height_mbs,
        });
    };

    // `RefFrameList`: the marked DPB (module docs), the AU's own references first
    // so a DPB deeper than the array can only lose a picture no slice names.
    // Per-slice list ORDER (the `ref_idx` mapping) is not expressed here at all —
    // it lives in the slice headers the hardware parses.
    let mut refs: Vec<DxvaRef> = Vec::new();
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if refs.iter().any(|existing| existing.id == rp.id) {
                continue;
            }
            // The snapshot is the authority for the marking and the pair-key: a
            // list entry may be a concealment substitute relabelled short-term
            // (module docs). A list naming a picture the DPB does not hold marked
            // is a planner-contract violation rather than stream damage; it cannot
            // happen off 8.2.4 (every initial list is built FROM the marked DPB),
            // and the entry's own copy is the honest fallback if it ever does.
            let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
            if marked.is_none() {
                trace!(
                    id = rp.id,
                    "a slice list names a picture the marked DPB does not hold"
                );
            }
            let slot = slots
                .slot_of(rp.id)
                .ok_or(PlanToDxvaError::UnresolvedReference(rp.id))?;
            refs.push(DxvaRef::new(slot, marked.unwrap_or(rp)));
        }
    }
    // The AU's own set is what the array MUST hold; 16 is the H.264 ceiling too,
    // so exceeding it means the plan is malformed. An error rather than a
    // truncation, because a dropped reference decodes to a wrong picture instead
    // of a missing one.
    if refs.len() > REF_FRAME_LIST_LEN {
        return Err(PlanToDxvaError::TooManyReferences(refs.len()));
    }
    // Then the rest of the marked DPB, in the planner's DPB order. Overflow past
    // the array is dropped rather than refused: these are pictures this AU does
    // not reference, so the decode is unaffected and refusing would cost the
    // whole picture. A slot the map never saw is skipped for the same reason —
    // the AU's own references have already been resolved strictly above.
    for rp in &plan.dpb_refs {
        if refs.len() == REF_FRAME_LIST_LEN {
            trace!(
                marked = plan.dpb_refs.len(),
                "the marked DPB exceeds RefFrameList; the tail is not expressible"
            );
            break;
        }
        if refs.iter().any(|existing| existing.id == rp.id) {
            continue;
        }
        match slots.slot_of(rp.id) {
            Some(slot) => refs.push(DxvaRef::new(slot, rp)),
            None => trace!(id = rp.id, "a marked DPB picture holds no slot in this map"),
        }
    }

    let mut pp = PicParamsH264::zeroed();
    pp.wFrameWidthInMbsMinus1 = width_minus1;
    pp.wFrameHeightInMbsMinus1 = height_minus1;
    pp.num_ref_frames = sps.max_num_ref_frames;
    pp.bit_depth_luma_minus8 = pic.bit_depth_luma_minus8;
    pp.bit_depth_chroma_minus8 = pic.bit_depth_chroma_minus8;
    // Reserved, and not actually free: libavcodec writes 3 here for every
    // standard profile (0 only for the legacy Intel ClearVideo GUID and for the
    // old ATI zigzag workaround, neither of which this backend uses), and the
    // Microsoft reference decoder does the same. Left at 3 rather than 0 so we
    // are byte-identical to the path every Windows player exercises.
    pp.Reserved16Bits = 3;
    pp.StatusReportFeedbackNumber = status_id;
    pp.CurrFieldOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];
    pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    pp.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
    // "The fields after ContinuationFlag are present." Always, here: this
    // backend never submits the truncated form.
    pp.ContinuationFlag = 1;
    pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    // The PPS defaults, NOT a slice's override: the picture parameters describe
    // the parameter set, and each slice header carries its own
    // `num_ref_idx_active_override_flag` for the hardware to apply.
    pp.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    pp.frame_num = pic.frame_num;
    pp.log2_max_frame_num_minus4 = sps.log2_max_frame_num_minus4;
    pp.pic_order_cnt_type = sps.pic_order_cnt_type;
    // Each POC type reads exactly one of these; the other stays 0 rather than
    // carrying a value the SPS never coded for this type.
    if sps.pic_order_cnt_type == 0 {
        pp.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
    } else if sps.pic_order_cnt_type == 1 {
        pp.delta_pic_order_always_zero_flag = u8::from(sps.delta_pic_order_always_zero_flag);
    }
    pp.direct_8x8_inference_flag = u8::from(sps.direct_8x8_inference_flag);
    pp.entropy_coding_mode_flag = u8::from(pps.entropy_coding_mode_flag);
    pp.pic_order_present_flag = u8::from(pps.bottom_field_pic_order_in_frame_present_flag);
    // num_slice_groups_minus1 / slice_group_map_type / slice_group_change_rate_minus1
    // / SliceGroupMap all stay 0: FMO was refused above.
    pp.deblocking_filter_control_present_flag =
        u8::from(pps.deblocking_filter_control_present_flag);
    pp.redundant_pic_cnt_present_flag = u8::from(pps.redundant_pic_cnt_present_flag);

    let is_intra = plan
        .slices
        .iter()
        .all(|slice| slice.header.slice_type.is_i() || slice.header.slice_type.is_si());
    pp.wBitFields = H264BitFields {
        chroma_format_idc: pic.chroma_format_idc,
        ref_pic_flag: pic.nal_ref_idc != 0,
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_idc: pps.weighted_bipred_idc,
        frame_mbs_only_flag: sps.frame_mbs_only_flag,
        transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
        // The DXVA spec defines MinLumaBipredSize8x8Flag as level_idc >= 31,
        // which is the level at which 8x8 is the smallest bi-predicted luma
        // block; libavcodec writes the identical comparison.
        min_luma_bipred_size_8x8: pic.level_idc as u8 >= 31,
        intra_pic_flag: is_intra,
    }
    .pack();

    // The reference arrays: entry i describes RefFrameList[i]. Unused entries are
    // 0xFF with zeroed counts, which is the padding both the spec and every
    // shipping decoder use.
    pp.RefFrameList = [PicEntry::UNUSED; REF_FRAME_LIST_LEN];
    for (i, r) in refs.iter().enumerate() {
        pp.RefFrameList[i] = PicEntry::new(r.slot, r.is_long_term);
        // Both field counts of a progressive frame; a real pair whenever the PPS
        // carried bottom_field_pic_order_in_frame_present_flag. TOP first — the
        // one ordering in this file no vector of ours can catch, because a
        // progressive stream without that flag has the two counts equal.
        pp.FieldOrderCntList[i] = [r.top_field_order_cnt, r.bottom_field_order_cnt];
        // frame_num for short-term references, LongTermFrameIdx for long-term
        // ones — the pair-key DXVA identifies references by, exactly as
        // pf-bitstream's DPB snapshot hands it over.
        pp.FrameNumList[i] = r.frame_num_or_lt_idx;
        // Two bits per entry: top field at 2i, bottom at 2i+1. A progressive
        // frame reference is marked for both. `i < 16` bounds the shift.
        pp.UsedForReferenceFlags |= 0b11 << (2 * i);
    }
    // NonExistingFrameFlags stays 0: pf-bitstream substitutes for a lost
    // reference and warns; it never hands over a "non-existing frame" placeholder
    // for the hardware to invent a picture from.

    // The quantization matrices, in the coded (zig-zag) order both the parser
    // stores and DXVA wants — see `QmatrixH264`'s docs. The PPS's lists are
    // authoritative: the parser has already applied Table 7-2's fallback rules,
    // so a PPS that codes no matrix already carries the SPS's (or the flat
    // default).
    let mut qm = QmatrixH264::zeroed();
    qm.bScalingLists4x4 = pps.scaling_lists_4x4;
    // Only Intra-Y and Inter-Y travel: DXVA has two 8x8 slots because 8x8 chroma
    // lists exist only in 4:4:4, which is refused above. The vendored parser
    // stores the 4:2:0 pair at indices 0 and 1 (its loop runs `for i in
    // 0..num_8x8` with num_8x8 = 2) — NOT at 0 and 3 the way libavcodec's own
    // scaling_matrix8 is indexed.
    qm.bScalingLists8x8[0] = pps.scaling_lists_8x8[0];
    qm.bScalingLists8x8[1] = pps.scaling_lists_8x8[1];

    let slice_ranges: Vec<Range<usize>> = plan.slices.iter().map(|s| s.data.clone()).collect();

    // Mutations LAST, after every fallible step above (fn docs). Removals first —
    // they were real regardless of this AU's fate — then the setup assignment.
    //
    // The AU's own picture can itself appear in `removed`: a non-reference
    // picture with no free frame buffer bypasses the DPB and is stored-and-
    // evicted within one plan. Its surface must still exist for the decode
    // itself, so it is assigned here and released right after.
    let setup_evicted = plan.dpb.removed.contains(&setup_id);
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        if !slots.release(id) {
            // Tolerated but never silent: reachable only when the caller skipped
            // feeding an AU's plan through this map.
            trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
        }
    }
    let setup_slot = slots.assign(setup_id)?;
    if setup_evicted {
        slots.release(setup_id);
    }
    // Written after the assignment for the obvious reason that the surface index
    // is not known until then; AssociatedFlag is the bottom-field flag, 0 by the
    // progressive envelope.
    pp.CurrPic = PicEntry::new(setup_slot, false);

    // `first_slice` is read for nothing but this debug aid today — the DXVA
    // picture parameters name no PPS id (unlike Vulkan's Std picture info,
    // which does), because the parameter set travels IN the picture parameters
    // rather than being referenced by id.
    debug_assert_eq!(
        first_slice.header.pic_parameter_set_id, pps.pic_parameter_set_id,
        "the plan's activated PPS must be the first slice's"
    );

    Ok(DecodePlanDxva {
        pic_params: pp,
        qmatrix: qm,
        slice_ranges,
        setup_slot,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
        mb_count: width_mbs * height_mbs,
    })
}

/// The slice-control records for a packed AU.
///
/// Split from [`plan_to_dxva`] because the byte locations only exist after the
/// bitstream buffer is mapped and packed — the conversion is per-plan, this is
/// per-submission.
pub fn slice_control(records: &[crate::pack::SliceRecord]) -> Vec<SliceH264Short> {
    records
        .iter()
        .map(|r| SliceH264Short {
            BSNALunitDataLocation: r.location,
            SliceBytesInBuffer: r.bytes,
            // 0 — a whole slice in one buffer; this backend never chops.
            wBadSliceChopping: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
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

    /// The same vendored vector pf-bitstream and pf-vkdecode plan (goldens: 250
    /// AUs, 500 slices), included from the same path rather than copied.
    const TEST_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// Test-only AU splitter, the same shape pf-vkdecode's `pic` tests use
    /// (which in turn mirrors pf-bitstream's `#[cfg(test)]`-private helper): a
    /// new AU starts at a non-slice NALU following a slice, or at a slice whose
    /// `first_mb_in_slice` is 0 following a slice.
    fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
        use cros_codecs::codec::h264::parser::NaluType;
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

    /// Plan the vendored stream and convert every AU, returning the plans paired
    /// with their conversions.
    fn convert_stream() -> Vec<(AuPlan, DecodePlanDxva)> {
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut out = Vec::new();
        for (i, au) in split_into_aus(TEST_25FPS).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            if map.capacity() != plan.picture.max_dpb_frames + 1 {
                *map = SlotMap::new(plan.picture.max_dpb_frames);
            }
            let dxva = plan_to_dxva(&plan, map, i as u32 + 1).expect("conversion");
            out.push((plan, dxva));
        }
        out
    }

    /// A 64x64 Main-profile SPS/PPS pair, for the one shape no vendored vector
    /// carries: long-term reference marking. Authored with the vendored builders
    /// and synthesizer, exactly as pf-bitstream's own MMCO tests do — slice headers
    /// written by hand below because upstream has no slice-header synthesizer.
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

    /// One IDR slice NALU. The planner reads headers only, so no slice data
    /// follows the rbsp stop bit.
    fn write_idr_slice() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(3, NaluType::SliceIdr as u8).unwrap();
            w.write_ue(0u32).unwrap(); // first_mb_in_slice
            w.write_ue(2u32).unwrap(); // slice_type: I
            w.write_ue(0u32).unwrap(); // pic_parameter_set_id
            w.write_f(4, 0u32).unwrap(); // frame_num, u(4)
            w.write_ue(0u32).unwrap(); // idr_pic_id
            w.write_f(4, 0u32).unwrap(); // pic_order_cnt_lsb, u(4)
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

    /// One P slice NALU. `mmco_ops` = `None` for sliding-window marking, `Some`
    /// for adaptive marking with `(operation, single-argument)` pairs — the writer
    /// appends the terminating op 0.
    fn write_p_slice(
        frame_num: u32,
        poc_lsb: u32,
        ref_idc: u8,
        num_ref_idx_l0_active: u32,
        mmco_ops: Option<&[(u32, u32)]>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(ref_idc, NaluType::Slice as u8).unwrap();
            w.write_ue(0u32).unwrap(); // first_mb_in_slice
            w.write_ue(0u32).unwrap(); // slice_type: P
            w.write_ue(0u32).unwrap(); // pic_parameter_set_id
            w.write_f(4, frame_num).unwrap(); // frame_num, u(4)
            w.write_f(4, poc_lsb).unwrap(); // pic_order_cnt_lsb, u(4)
            w.write_f(1, 1u32).unwrap(); // num_ref_idx_active_override_flag
            w.write_ue(num_ref_idx_l0_active - 1).unwrap();
            w.write_f(1, 0u32).unwrap(); // ref_pic_list_modification_flag_l0
            if ref_idc != 0 {
                match mmco_ops {
                    None => w.write_f(1, 0u32).map(|_| ()).unwrap(),
                    Some(ops) => {
                        w.write_f(1, 1u32).unwrap(); // adaptive_ref_pic_marking_mode_flag
                        for (op, arg) in ops {
                            w.write_ue(*op).unwrap();
                            w.write_ue(*arg).unwrap();
                        }
                        w.write_ue(0u32).unwrap(); // end of the MMCO list
                    }
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

    /// The unique pictures the AU's own slice lists name, in first-appearance
    /// order — what `RefFrameList` used to hold, and what it must now merely start
    /// with.
    fn au_reference_ids(plan: &AuPlan) -> Vec<PicId> {
        let mut ids = Vec::new();
        for slice in &plan.slices {
            for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
                if !ids.contains(&rp.id) {
                    ids.push(rp.id);
                }
            }
        }
        ids
    }

    #[test]
    fn the_reference_list_is_the_marked_dpb_led_by_the_pictures_this_au_names() {
        let converted = convert_stream();
        let mut wider_than_the_au = 0usize;
        for (plan, dxva) in &converted {
            let au = au_reference_ids(plan);
            // The AU's own references lead, in their own order: truncation at the
            // array's sixteen can then only ever drop a picture no slice names.
            assert_eq!(
                dxva.refs
                    .iter()
                    .take(au.len())
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                au
            );
            // And the whole marked DPB is there — every picture the planner reports
            // marked, none missing, none invented.
            let mut listed: Vec<PicId> = dxva.refs.iter().map(|r| r.id).collect();
            let mut marked: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            listed.sort_unstable();
            marked.sort_unstable();
            assert_eq!(listed, marked);
            if dxva.refs.len() > au.len() {
                wider_than_the_au += 1;
            }
        }
        // This vector is not a degenerate case for the change: the DPB holds a
        // reference the AU's own lists do not reach on nearly half its pictures.
        assert!(
            wider_than_the_au >= 100,
            "only {wider_than_the_au} of {} AUs exercised the difference",
            converted.len()
        );
    }

    #[test]
    fn a_long_term_reference_no_slice_names_still_reaches_the_reference_list() {
        // The RFI shape, end to end: an anchor pinned long-term that the current
        // picture's (truncated) reference list never names. Built here rather than
        // taken from a vector because no vendored vector carries an MMCO 6.
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        // AU1 pins itself long-term (MMCO 4 admits index 0, MMCO 6 assigns it).
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        // AU2 activates ONE reference, which 8.2.4.2.1 makes the short-term IDR.
        let au2 = write_p_slice(2, 4, 1, 1, None);

        let mut planner = H264Planner::new();
        let plans: Vec<AuPlan> = [au0.as_slice(), au1.as_slice(), au2.as_slice()]
            .into_iter()
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        let converted: Vec<DecodePlanDxva> = plans
            .iter()
            .enumerate()
            .map(|(i, plan)| plan_to_dxva(plan, &mut slots, i as u32 + 1).expect("convert"))
            .collect();

        let idr_id = plans[0].dpb.stored.unwrap();
        let lt_id = plans[1].dpb.stored.unwrap();
        assert_eq!(au_reference_ids(&plans[2]), vec![idr_id]);

        // Both pictures are in RefFrameList: the one AU2 names, and the long-term
        // anchor it does not. A driver told the anchor is gone may discard it.
        let dxva = &converted[2];
        assert_eq!(
            dxva.refs
                .iter()
                .map(|r| (r.id, r.is_long_term, r.frame_num_or_lt_idx))
                .collect::<Vec<_>>(),
            vec![(idr_id, false, 0), (lt_id, true, 0)]
        );
        // …with the marking and pair-key the DPB holds, not the slice list's.
        assert!(
            dxva.pic_params.RefFrameList[1].associated(),
            "AssociatedFlag"
        );
        assert_eq!(dxva.pic_params.FrameNumList[1], 0, "LongTermFrameIdx");
        assert_eq!(dxva.pic_params.UsedForReferenceFlags & 0b1111, 0b1111);
        assert_eq!(dxva.pic_params.RefFrameList[2], PicEntry::UNUSED);
    }

    #[test]
    fn an_unequal_field_order_count_pair_rides_through_top_first() {
        // Every vector this crate has is progressive with
        // bottom_field_pic_order_in_frame_present_flag clear, so TopFieldOrderCnt
        // == BottomFieldOrderCnt everywhere and a swapped pair is invisible across
        // all 250 AUs. The pair is real whenever a PPS does carry that flag, so
        // this drives one through the conversion with the two counts distinct.
        let mut planner = H264Planner::new();
        let aus = split_into_aus(TEST_25FPS);
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let mut second = planner.plan_au(aus[1]).expect("plan 1");
        let ref_id = first.dpb.stored.unwrap();

        for rp in second
            .dpb_refs
            .iter_mut()
            .filter(|rp| rp.id == ref_id)
            .chain(
                second.slices[0]
                    .ref_list0
                    .iter_mut()
                    .filter(|rp| rp.id == ref_id),
            )
        {
            rp.top_field_order_cnt = 4;
            rp.bottom_field_order_cnt = 6;
        }

        let mut slots = SlotMap::new(first.picture.max_dpb_frames);
        plan_to_dxva(&first, &mut slots, 1).expect("convert 0");
        let dxva = plan_to_dxva(&second, &mut slots, 2).expect("convert 1");
        assert_eq!(dxva.refs[0].id, ref_id);
        assert_eq!(
            dxva.pic_params.FieldOrderCntList[0],
            [4, 6],
            "TopFieldOrderCnt occupies index 0"
        );
    }

    #[test]
    fn the_macroblock_count_is_the_coded_picture_in_macroblocks() {
        // libavcodec writes this into `NumMBsInBuffer` on the H.264 bitstream and
        // slice-control descriptors (`commit_bitstream_and_slice_buffer`:
        // `h->mb_width * h->mb_height`). The vendored vector is 320x240 — 20x15
        // macroblocks.
        for (plan, dxva) in convert_stream() {
            let width = u32::from(plan.sps.pic_width_in_mbs_minus1) + 1;
            let height = (u32::from(plan.sps.pic_height_in_map_units_minus1) + 1)
                * (2 - u32::from(plan.sps.frame_mbs_only_flag));
            assert_eq!(dxva.mb_count, width * height);
            assert_eq!(dxva.mb_count, 20 * 15);
            // The same two numbers the picture parameters carry, minus one each.
            assert_eq!(
                u32::from(dxva.pic_params.wFrameWidthInMbsMinus1 + 1)
                    * u32::from(dxva.pic_params.wFrameHeightInMbsMinus1 + 1),
                dxva.mb_count
            );
        }
    }

    #[test]
    fn the_whole_vendored_stream_converts_without_a_single_refusal() {
        let converted = convert_stream();
        // pf-bitstream's own golden for this vector is 250 planned AUs; the
        // conversion must not lose any of them.
        assert_eq!(converted.len(), 250);
    }

    #[test]
    fn the_setup_surface_is_the_current_picture_entry_and_is_never_also_a_reference_entry() {
        for (_, dxva) in convert_stream() {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            assert!(
                !dxva.pic_params.CurrPic.associated(),
                "progressive: CurrPic's AssociatedFlag is the bottom-field flag"
            );
            // The picture being decoded must not appear in its own reference
            // list — that would be a surface read and written in one operation.
            for r in &dxva.refs {
                assert_ne!(r.slot, dxva.setup_slot, "a reference aliases the target");
            }
        }
    }

    #[test]
    fn reference_entries_carry_their_pictures_frame_num_poc_pair_and_used_flags() {
        for (plan, dxva) in convert_stream() {
            for (i, r) in dxva.refs.iter().enumerate() {
                // The planner's DPB snapshot is the authority for every one of
                // these — not the slice lists' copy.
                let rp = plan
                    .dpb_refs
                    .iter()
                    .find(|d| d.id == r.id)
                    .expect("every entry is a marked DPB picture");
                assert_eq!(dxva.pic_params.RefFrameList[i].index(), r.slot);
                assert_eq!(dxva.pic_params.RefFrameList[i].associated(), r.is_long_term);
                assert_eq!(r.is_long_term, rp.is_long_term);
                assert_eq!(dxva.pic_params.FrameNumList[i], rp.frame_num_or_lt_idx);
                assert_eq!(
                    dxva.pic_params.FieldOrderCntList[i],
                    [rp.top_field_order_cnt, rp.bottom_field_order_cnt]
                );
                // A progressive reference is used for BOTH fields.
                assert_eq!(
                    dxva.pic_params.UsedForReferenceFlags >> (2 * i) & 0b11,
                    0b11
                );
            }
            // Everything past the marked DPB is the 0xFF sentinel with a cleared
            // use flag — never a stale surface index.
            for i in dxva.refs.len()..REF_FRAME_LIST_LEN {
                assert_eq!(dxva.pic_params.RefFrameList[i], PicEntry::UNUSED);
                assert_eq!(dxva.pic_params.FrameNumList[i], 0);
                assert_eq!(dxva.pic_params.FieldOrderCntList[i], [0, 0]);
                assert_eq!(dxva.pic_params.UsedForReferenceFlags >> (2 * i) & 0b11, 0);
            }
            // pf-bitstream never emits a gap placeholder as an id.
            assert_eq!(dxva.pic_params.NonExistingFrameFlags, 0);
        }
    }

    #[test]
    fn the_idr_is_intra_and_the_inter_pictures_are_not() {
        let converted = convert_stream();
        let (_, first) = &converted[0];
        assert_ne!(first.pic_params.wBitFields & (1 << 15), 0, "IDR is intra");
        assert!(first.refs.is_empty(), "an IDR references nothing");
        // The vector is IPPP…, so the second picture is inter and references the
        // first.
        let (_, second) = &converted[1];
        assert_eq!(second.pic_params.wBitFields & (1 << 15), 0);
        assert_eq!(second.refs.len(), 1);
    }

    #[test]
    fn the_picture_parameters_carry_the_active_sps_and_pps_verbatim() {
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        let pp = &dxva.pic_params;
        assert_eq!(
            pp.wFrameWidthInMbsMinus1, plan.sps.pic_width_in_mbs_minus1,
            "320-wide stream: 20 macroblocks"
        );
        assert_eq!(
            pp.wFrameHeightInMbsMinus1,
            plan.sps.pic_height_in_map_units_minus1
        );
        assert_eq!(pp.num_ref_frames, plan.sps.max_num_ref_frames);
        assert_eq!(pp.frame_num, plan.picture.frame_num);
        assert_eq!(
            pp.log2_max_frame_num_minus4,
            plan.sps.log2_max_frame_num_minus4
        );
        assert_eq!(pp.pic_order_cnt_type, plan.sps.pic_order_cnt_type);
        assert_eq!(pp.pic_init_qp_minus26, plan.pps.pic_init_qp_minus26);
        assert_eq!(pp.pic_init_qs_minus26, plan.pps.pic_init_qs_minus26);
        assert_eq!(pp.chroma_qp_index_offset, plan.pps.chroma_qp_index_offset);
        assert_eq!(
            pp.second_chroma_qp_index_offset,
            plan.pps.second_chroma_qp_index_offset
        );
        assert_eq!(
            pp.entropy_coding_mode_flag,
            u8::from(plan.pps.entropy_coding_mode_flag)
        );
        assert_eq!(
            pp.num_ref_idx_l0_active_minus1,
            plan.pps.num_ref_idx_l0_default_active_minus1
        );
        // The invariants a driver reads before anything else.
        assert_eq!(pp.ContinuationFlag, 1);
        assert_eq!(pp.Reserved16Bits, 3);
        assert_eq!(pp.StatusReportFeedbackNumber, 1);
        // FMO is refused, so its whole descriptor block stays zero.
        assert_eq!(pp.num_slice_groups_minus1, 0);
        assert_eq!(pp.slice_group_map_type, 0);
        assert_eq!(pp.slice_group_change_rate_minus1, 0);
        assert!(pp.SliceGroupMap.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_quantization_matrices_are_the_parsers_lists_in_coded_order() {
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        assert_eq!(dxva.qmatrix.bScalingLists4x4, plan.pps.scaling_lists_4x4);
        assert_eq!(
            dxva.qmatrix.bScalingLists8x8[0],
            plan.pps.scaling_lists_8x8[0]
        );
        assert_eq!(
            dxva.qmatrix.bScalingLists8x8[1],
            plan.pps.scaling_lists_8x8[1]
        );
    }

    #[test]
    fn slice_ranges_ride_through_in_plan_order() {
        for (plan, dxva) in convert_stream() {
            assert_eq!(dxva.slice_ranges.len(), plan.slices.len());
            for (range, slice) in dxva.slice_ranges.iter().zip(&plan.slices) {
                assert_eq!(*range, slice.data);
            }
        }
    }

    #[test]
    fn poc_type_specific_fields_are_written_only_for_the_type_that_codes_them() {
        // The vendored stream is POC type 0, so the type-1 field must be clear
        // even though the SPS has a (default) value for it.
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        assert_eq!(plan.sps.pic_order_cnt_type, 0);
        assert_eq!(
            dxva.pic_params.log2_max_pic_order_cnt_lsb_minus4,
            plan.sps.log2_max_pic_order_cnt_lsb_minus4
        );
        assert_eq!(dxva.pic_params.delta_pic_order_always_zero_flag, 0);
    }

    #[test]
    fn a_capacity_mismatch_is_refused_and_leaves_the_map_untouched() {
        let mut planner = H264Planner::new();
        let au = split_into_aus(TEST_25FPS).into_iter().next().unwrap();
        let plan = planner.plan_au(au).expect("plan");
        // A map sized for a different DPB depth: an SPS renegotiation.
        let mut slots = SlotMap::new(plan.picture.max_dpb_frames + 1);
        let before = slots.active();
        assert_eq!(
            plan_to_dxva(&plan, &mut slots, 1),
            Err(PlanToDxvaError::CapacityMismatch {
                required: plan.picture.max_dpb_frames + 1,
                capacity: plan.picture.max_dpb_frames + 2,
            })
        );
        assert_eq!(slots.active(), before, "a refusal must not mutate the map");
    }

    #[test]
    fn a_reference_the_map_never_saw_is_refused_and_leaves_the_map_untouched() {
        // Plan two AUs but feed only the second through the map: its reference
        // resolves to nothing.
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let second = planner.plan_au(aus[1]).expect("plan 1");
        let mut slots = SlotMap::new(second.picture.max_dpb_frames);
        assert!(!second.slices[0].ref_list0.is_empty());
        let missing = second.slices[0].ref_list0[0].id;
        assert_eq!(first.dpb.stored, Some(missing));
        assert_eq!(
            plan_to_dxva(&second, &mut slots, 1),
            Err(PlanToDxvaError::UnresolvedReference(missing))
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn slice_control_records_carry_the_packers_locations_verbatim() {
        let records = [
            crate::pack::SliceRecord {
                location: 0,
                bytes: 40,
            },
            crate::pack::SliceRecord {
                location: 40,
                bytes: 216,
            },
        ];
        let control = slice_control(&records);
        assert_eq!(control.len(), 2);
        assert_eq!(control[0].BSNALunitDataLocation, 0);
        assert_eq!(control[0].SliceBytesInBuffer, 40);
        assert_eq!(control[1].BSNALunitDataLocation, 40);
        assert_eq!(control[1].SliceBytesInBuffer, 216);
        assert!(control.iter().all(|c| c.wBadSliceChopping == 0));
    }

    #[test]
    fn a_slot_is_reused_only_after_its_picture_leaves_the_dpb() {
        // The whole-stream churn check: no two live pictures may share a surface
        // index, which for DXVA is the difference between a decode and a
        // corrupted reference.
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut live: Vec<(PicId, u8)> = Vec::new();
        for (i, au) in split_into_aus(TEST_25FPS).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let removed = plan.dpb.removed.clone();
            let dxva = plan_to_dxva(&plan, map, i as u32 + 1).expect("conversion");
            live.retain(|&(id, _)| !removed.contains(&id));
            assert!(
                live.iter().all(|&(_, slot)| slot != dxva.setup_slot),
                "AU {i} decodes into a surface a live picture still holds"
            );
            live.push((dxva.setup_id, dxva.setup_slot));
        }
    }
}
