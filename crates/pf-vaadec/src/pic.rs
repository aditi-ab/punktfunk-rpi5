//! One [`AuPlan`] into the libva buffers a `vaRenderPicture` call carries: the
//! picture parameters, the inverse-quantization matrices and one slice-parameter
//! record per slice.
//!
//! The counterpart of `pf-dxvadec`'s `pic` module, and it follows the same
//! transaction discipline for the same reason — a half-applied DPB update is the
//! shape of a corrupt reference:
//!
//! 1. envelope and capacity are validated (read-only);
//! 2. references resolve against the PRE-removal state (read-only) — this access
//!    unit's own end-of-picture marking can evict a picture its slices legitimately
//!    reference;
//! 3. `removed` is applied, then the setup slot is assigned last.
//!
//! # Three things VAAPI wants that the other two backends do not
//!
//! **A bit offset.** `slice_data_bit_offset` is the position where `slice_data()`
//! begins, counted from and including the NAL header byte with emulation-prevention
//! bytes removed. DXVA takes a byte offset and Vulkan takes nothing. It costs no new
//! parsing: the vendored parser records exactly this as
//! `SliceHeader::header_bit_size` — `(nalu.size - epb) * 8 - bits_left` — because
//! cros-codecs' own production backend is VAAPI.
//!
//! **The slice data without its start code.** That bit offset is relative to the NAL
//! header byte, so the buffer must begin there. `SlicePlan::data` is
//! start-code-INCLUSIVE and the prefix is three OR four bytes (the real host emits
//! four on every access unit), so the prefix is measured per slice rather than
//! assumed — the same normalisation the Vulkan ring layer performs, and the same
//! defect class that made HEVC unplayable when it was skipped.
//!
//! **The per-slice reference lists.** DXVA's short-format slice control expresses no
//! lists at all — the hardware re-parses the slice header — but VAAPI wants
//! `RefPicList0`/`RefPicList1` in 8.2.4.2 order, which is precisely what
//! `SlicePlan::ref_list0`/`ref_list1` already carry.
//!
//! # Two reference sets, and they are not the same set
//!
//! `VAPictureParameterBufferH264::reference_frames` is documented as "in DPB": a
//! statement about the decoded picture buffer, exactly like DXVA's `RefFrameList`
//! and exactly UNLIKE Vulkan's `pReferenceSlots` (the slots THIS operation uses).
//! It is therefore filled from the planner's per-AU `dpb_refs` snapshot — the marked
//! DPB — while the per-slice lists come from the slice's own derived lists. Getting
//! that backwards loses a long-term reference no slice happens to name, which is the
//! failure this program has already paid for on the DXVA side.

use std::ops::Range;

use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;

use crate::va::PicFieldsH264;
use crate::va::SeqFieldsH264;
use crate::va::VaIqMatrixBufferH264;
use crate::va::VaPictureH264;
use crate::va::VaPictureParameterBufferH264;
use crate::va::VaSliceParameterBufferH264;
use crate::va::VA_PICTURE_H264_LONG_TERM_REFERENCE;
use crate::va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::SlotError;
use crate::SlotMap;

/// `VAPictureParameterBufferH264::reference_frames` length, and the H.264 DPB
/// ceiling — the two coincide, which is why an overflow here means a malformed plan
/// rather than an expressiveness limit.
pub const REFERENCE_FRAMES_LEN: usize = 16;

/// `RefPicList0`/`RefPicList1` length.
pub const REF_PIC_LIST_LEN: usize = 32;

/// Everything one `vaBeginPicture`/`vaRenderPicture`/`vaEndPicture` sequence needs.
#[derive(Debug, Clone)]
pub struct DecodePlanVa {
    pub pic_params: VaPictureParameterBufferH264,
    pub iq_matrix: VaIqMatrixBufferH264,
    /// One record per slice, in bitstream order.
    pub slices: Vec<VaSliceParameterBufferH264>,
    /// Each slice's data range in the access unit, **start code excluded** — what
    /// the matching `VASliceDataBuffer` carries. Parallel to [`Self::slices`].
    pub slice_data: Vec<Range<usize>>,
    /// The DPB slot this picture decodes into; index it into the caller's surface
    /// table to get the `VASurfaceID`.
    pub setup_slot: u8,
}

/// Why a plan cannot be expressed as VAAPI buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaError {
    NoSlices,
    NoStoredId,
    /// FMO. Refused rather than ignored: the deprecated fields exist in the struct
    /// but no driver implements slice groups.
    SliceGroups {
        count: u32,
    },
    SeparateColourPlanes,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A slice named a reference the slot map does not hold.
    UnresolvedReference(PicId),
    /// More marked references than `reference_frames` can express.
    TooManyReferences(usize),
    /// A slice's derived list is longer than `RefPicList0`/`1`.
    RefListTooLong {
        slice: usize,
        len: usize,
    },
    /// The slot map's slot has no entry in the caller's surface table.
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    /// The picture is larger than the macroblock counters can express.
    DimensionOverflow {
        width_mbs: u32,
        height_mbs: u32,
    },
    /// A slice's byte range is not inside the access unit, or carries no Annex-B
    /// start code where one is required.
    SliceRange {
        slice: usize,
    },
    /// `header_bit_size` does not fit `slice_data_bit_offset`'s 16 bits. Only
    /// reachable from an absurd slice header, and an error rather than a truncation
    /// because a wrong bit offset decodes garbage.
    SliceBitOffsetOverflow {
        slice: usize,
        bits: usize,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaError {
    fn from(e: SlotError) -> Self {
        PlanToVaError::Slot(e)
    }
}

impl std::fmt::Display for PlanToVaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaError::NoSlices => write!(f, "the access unit planned no slices"),
            PlanToVaError::NoStoredId => write!(f, "the plan stored no picture id"),
            PlanToVaError::SliceGroups { count } => {
                write!(f, "slice groups (FMO) are outside the envelope: {count}")
            }
            PlanToVaError::SeparateColourPlanes => {
                write!(f, "separate colour planes are outside the envelope")
            }
            PlanToVaError::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, this stream needs {required}"
            ),
            PlanToVaError::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToVaError::TooManyReferences(n) => {
                write!(f, "{n} marked references exceed reference_frames[16]")
            }
            PlanToVaError::RefListTooLong { slice, len } => {
                write!(f, "slice {slice}: reference list of {len} exceeds 32")
            }
            PlanToVaError::SurfaceOutOfRange { slot, surfaces } => {
                write!(f, "DPB slot {slot} has no surface in a table of {surfaces}")
            }
            PlanToVaError::DimensionOverflow {
                width_mbs,
                height_mbs,
            } => write!(
                f,
                "picture of {width_mbs}x{height_mbs} macroblocks is too large"
            ),
            PlanToVaError::SliceRange { slice } => {
                write!(
                    f,
                    "slice {slice}: byte range is not a start-code-prefixed NAL"
                )
            }
            PlanToVaError::SliceBitOffsetOverflow { slice, bits } => {
                write!(
                    f,
                    "slice {slice}: header of {bits} bits exceeds 16-bit offset"
                )
            }
            PlanToVaError::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaError {}

/// The Annex-B start-code length at the front of `bytes` (3 or 4), or `None`.
pub(crate) fn start_code_len(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(&[0x00, 0x00, 0x00, 0x01]) {
        Some(4)
    } else if bytes.starts_with(&[0x00, 0x00, 0x01]) {
        Some(3)
    } else {
        None
    }
}

/// One `VAPictureH264` for a reference picture already resolved to a slot.
fn va_ref(rp: &RefPic, surface: u32) -> VaPictureH264 {
    VaPictureH264 {
        picture_id: surface,
        frame_idx: u32::from(rp.frame_num_or_lt_idx),
        flags: if rp.is_long_term {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        },
        top_field_order_cnt: rp.top_field_order_cnt,
        bottom_field_order_cnt: rp.bottom_field_order_cnt,
        va_reserved: [0; 4],
    }
}

/// Resolve `id` to its surface, or say which id could not be resolved.
fn surface_of(slots: &SlotMap, surfaces: &[u32], id: PicId) -> Result<(u8, u32), PlanToVaError> {
    let slot = slots
        .slot_of(id)
        .ok_or(PlanToVaError::UnresolvedReference(id))?;
    let surface = *surfaces
        .get(usize::from(slot))
        .ok_or(PlanToVaError::SurfaceOutOfRange {
            slot,
            surfaces: surfaces.len(),
        })?;
    Ok((slot, surface))
}

/// Convert one planned access unit.
///
/// `au` is the access unit the plan was built from — needed because the slice data
/// buffer must start at the NAL header byte, and the start-code prefix is three or
/// four bytes depending on the encoder. `surfaces` maps DPB slot to `VASurfaceID`
/// for the pictures the DPB already holds; this crate never allocates one.
///
/// # Why the decode target is a parameter and not `surfaces[setup_slot]`
///
/// The caller binds the target surface, at activation time, exactly as
/// `pf-vkdecode` binds a pool image when a DPB slot is activated. A slot ledger
/// is not a surface allocator: [`SlotMap::assign`] takes the lowest free slot, and
/// a slot freed by this AU's own removals is free by the time the setup picture
/// takes it — measured at **225 of the vendored vector's 250 access units**
/// (`the_setup_picture_routinely_inherits_a_just_freed_slot`). Reading the target
/// out of a slot-indexed table would therefore decode, on nine frames in ten,
/// into the surface holding the picture that was just displayed — which the
/// consumer may still be sampling. Zero-copy means the decoder cannot have that
/// surface back until the consumer says so, and only the caller knows.
///
/// `setup_surface` must be free in that sense: bound to no live picture and held
/// by no consumer. After a successful call the caller binds it to
/// [`DecodePlanVa::setup_slot`], so later access units resolve references to this
/// picture through `surfaces`.
///
/// Nothing mutates `slots` until every fallible step has passed.
pub fn plan_to_va(
    plan: &AuPlan,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVa, PlanToVaError> {
    if plan.slices.is_empty() {
        return Err(PlanToVaError::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToVaError::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if pps.num_slice_groups_minus1 != 0 {
        return Err(PlanToVaError::SliceGroups {
            count: pps.num_slice_groups_minus1 + 1,
        });
    }
    if sps.separate_colour_plane_flag {
        return Err(PlanToVaError::SeparateColourPlanes);
    }

    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVaError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // The caller binds `setup_surface` to the returned slot, so a table that cannot
    // express every slot is caught HERE — before anything mutates the ledger —
    // rather than as an out-of-range bind after a successful conversion. Checked
    // against the capacity, not the chosen slot, precisely so it stays a pre-check.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaError::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }

    // Height is expressed in FRAME macroblocks, so the map-units count doubles for a
    // non-frame-only SPS — unreachable inside pf-bitstream's progressive envelope,
    // written out so the expression says what the spec says.
    let width_mbs = u32::from(sps.pic_width_in_mbs_minus1) + 1;
    let height_mbs = (u32::from(sps.pic_height_in_map_units_minus1) + 1)
        * (2 - u32::from(sps.frame_mbs_only_flag));
    let (Ok(width_minus1), Ok(height_minus1)) = (
        u16::try_from(width_mbs.saturating_sub(1)),
        u16::try_from(height_mbs.saturating_sub(1)),
    ) else {
        return Err(PlanToVaError::DimensionOverflow {
            width_mbs,
            height_mbs,
        });
    };

    // --- read-only resolution, against the PRE-removal slot map -------------

    // The marked DPB, in the planner's order — `reference_frames` is a statement
    // about the DPB, not about this access unit (module docs).
    if plan.dpb_refs.len() > REFERENCE_FRAMES_LEN {
        return Err(PlanToVaError::TooManyReferences(plan.dpb_refs.len()));
    }
    let mut reference_frames = [VaPictureH264::invalid(); REFERENCE_FRAMES_LEN];
    for (slot_out, rp) in reference_frames.iter_mut().zip(&plan.dpb_refs) {
        let (_, surface) = surface_of(slots, surfaces, rp.id)?;
        *slot_out = va_ref(rp, surface);
    }

    // Per-slice derived lists, in 8.2.4.2 order.
    let mut slices = Vec::with_capacity(plan.slices.len());
    let mut slice_data = Vec::with_capacity(plan.slices.len());
    for (index, sp) in plan.slices.iter().enumerate() {
        let hdr = &sp.header;
        let mut rec = VaSliceParameterBufferH264::zeroed();

        let bytes = au
            .get(sp.data.clone())
            .ok_or(PlanToVaError::SliceRange { slice: index })?;
        let prefix = start_code_len(bytes).ok_or(PlanToVaError::SliceRange { slice: index })?;
        let payload = sp.data.start + prefix..sp.data.end;
        rec.slice_data_size = (payload.end - payload.start) as u32;
        rec.slice_data_offset = 0;
        rec.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        rec.slice_data_bit_offset = u16::try_from(hdr.header_bit_size).map_err(|_| {
            PlanToVaError::SliceBitOffsetOverflow {
                slice: index,
                bits: hdr.header_bit_size,
            }
        })?;
        slice_data.push(payload);

        rec.first_mb_in_slice = hdr.first_mb_in_slice as u16;
        rec.slice_type = hdr.slice_type as u8;
        rec.direct_spatial_mv_pred_flag = u8::from(hdr.direct_spatial_mv_pred_flag);
        rec.num_ref_idx_l0_active_minus1 = hdr.num_ref_idx_l0_active_minus1;
        rec.num_ref_idx_l1_active_minus1 = hdr.num_ref_idx_l1_active_minus1;
        rec.cabac_init_idc = hdr.cabac_init_idc;
        rec.slice_qp_delta = hdr.slice_qp_delta;
        rec.disable_deblocking_filter_idc = hdr.disable_deblocking_filter_idc;
        rec.slice_alpha_c0_offset_div2 = hdr.slice_alpha_c0_offset_div2;
        rec.slice_beta_offset_div2 = hdr.slice_beta_offset_div2;

        for (list_out, list_in) in [
            (&mut rec.ref_pic_list0, &sp.ref_list0),
            (&mut rec.ref_pic_list1, &sp.ref_list1),
        ] {
            if list_in.len() > REF_PIC_LIST_LEN {
                return Err(PlanToVaError::RefListTooLong {
                    slice: index,
                    len: list_in.len(),
                });
            }
            for (entry, rp) in list_out.iter_mut().zip(list_in) {
                // The marked snapshot is the authority for the marking and the
                // pair-key: a list entry may be a concealment substitute relabelled
                // short-term. Falling back to the entry's own copy is honest if the
                // DPB does not hold it.
                let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
                let (_, surface) = surface_of(slots, surfaces, rp.id)?;
                *entry = va_ref(marked.unwrap_or(rp), surface);
            }
        }

        // 7.3.3: an explicit weight table is parsed for L0 when the PPS enables
        // weighted P prediction on a P/SP slice, and for both lists when
        // weighted_bipred_idc == 1 on a B slice. Anywhere else the arrays are not
        // meaningful, and flagging them would hand the driver defaults as if the
        // stream had coded them.
        let pwt = &hdr.pred_weight_table;
        let explicit_l0 = (pps.weighted_pred_flag
            && (hdr.slice_type.is_p() || hdr.slice_type.is_sp()))
            || (pps.weighted_bipred_idc == 1 && hdr.slice_type.is_b());
        let explicit_l1 = pps.weighted_bipred_idc == 1 && hdr.slice_type.is_b();
        if explicit_l0 || explicit_l1 {
            rec.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
            rec.chroma_log2_weight_denom = pwt.chroma_log2_weight_denom;
        }
        if explicit_l0 {
            rec.luma_weight_l0_flag = 1;
            rec.chroma_weight_l0_flag = 1;
            rec.luma_weight_l0 = pwt.luma_weight_l0;
            // The vendored table stores L0 offsets as i8 and L1 offsets as i16 — an
            // upstream inconsistency, not a semantic difference; libva wants i16 for
            // both, so the narrow side widens.
            for (out, v) in rec.luma_offset_l0.iter_mut().zip(pwt.luma_offset_l0) {
                *out = i16::from(v);
            }
            rec.chroma_weight_l0 = pwt.chroma_weight_l0;
            for (out, v) in rec.chroma_offset_l0.iter_mut().zip(pwt.chroma_offset_l0) {
                *out = [i16::from(v[0]), i16::from(v[1])];
            }
        }
        if explicit_l1 {
            rec.luma_weight_l1_flag = 1;
            rec.chroma_weight_l1_flag = 1;
            rec.luma_weight_l1 = pwt.luma_weight_l1;
            rec.luma_offset_l1 = pwt.luma_offset_l1;
            rec.chroma_weight_l1 = pwt.chroma_weight_l1;
            for (out, v) in rec.chroma_offset_l1.iter_mut().zip(pwt.chroma_offset_l1) {
                *out = [i16::from(v[0]), i16::from(v[1])];
            }
        }

        slices.push(rec);
    }

    // --- mutations, after every fallible step -------------------------------

    // The AU's own picture can appear in `removed`: a non-reference picture with no
    // free frame buffer is stored and evicted within one plan. Its surface must
    // still exist for the decode, so it is assigned here and released right after.
    let setup_evicted = plan.dpb.removed.contains(&setup_id);
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        let _ = slots.release(id);
    }
    let setup_slot = slots.assign(setup_id)?;
    if setup_evicted {
        slots.release(setup_id);
    }

    let curr_pic = VaPictureH264 {
        picture_id: setup_surface,
        // For the current picture this is `frame_num`, not a long-term index.
        frame_idx: u32::from(pic.frame_num),
        flags: if pic.is_reference {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        } else {
            0
        },
        top_field_order_cnt: pic.top_field_order_cnt,
        bottom_field_order_cnt: pic.bottom_field_order_cnt,
        va_reserved: [0; 4],
    };

    let seq_fields = SeqFieldsH264 {
        chroma_format_idc: sps.chroma_format_idc,
        separate_colour_plane_flag: sps.separate_colour_plane_flag,
        gaps_in_frame_num_value_allowed_flag: sps.gaps_in_frame_num_value_allowed_flag,
        frame_mbs_only_flag: sps.frame_mbs_only_flag,
        mb_adaptive_frame_field_flag: sps.mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag: sps.direct_8x8_inference_flag,
        // A.3.3.2 is a level-derived constraint, and libavcodec's VAAPI backend
        // leaves it 0 for every stream it sends; matching the path drivers are
        // validated against beats deriving a value nobody consumes.
        min_luma_bi_pred_size8x8: false,
        log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
        pic_order_cnt_type: sps.pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
    };
    let pic_fields = PicFieldsH264 {
        entropy_coding_mode_flag: pps.entropy_coding_mode_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_idc: pps.weighted_bipred_idc,
        transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
        // Progressive envelope: pf-bitstream rejects field coding before a plan
        // exists, so this is a constant rather than a read.
        field_pic_flag: false,
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        pic_order_present_flag: pps.bottom_field_pic_order_in_frame_present_flag,
        deblocking_filter_control_present_flag: pps.deblocking_filter_control_present_flag,
        redundant_pic_cnt_present_flag: pps.redundant_pic_cnt_present_flag,
        reference_pic_flag: pic.is_reference,
    };

    let pic_params = VaPictureParameterBufferH264 {
        curr_pic,
        reference_frames,
        picture_width_in_mbs_minus1: width_minus1,
        picture_height_in_mbs_minus1: height_minus1,
        bit_depth_luma_minus8: pic.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: pic.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames,
        seq_fields: seq_fields.pack(),
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        pic_fields: pic_fields.pack(),
        frame_num: pic.frame_num,
        va_reserved: [0; 8],
    };

    // The PPS lists are the EFFECTIVE ones: the parser has already applied Table 7-2's
    // fallback rules, so no SPS/PPS merge happens here.
    let iq_matrix = VaIqMatrixBufferH264 {
        scaling_list4x4: pps.scaling_lists_4x4,
        // libva carries only the two 8x8 lists a 4:2:0 stream uses; the parser keeps
        // six (the 4:4:4 set).
        scaling_list8x8: [pps.scaling_lists_8x8[0], pps.scaling_lists_8x8[1]],
        va_reserved: [0; 4],
    };

    Ok(DecodePlanVa {
        pic_params,
        iq_matrix,
        slices,
        slice_data,
        setup_slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::va::VA_INVALID_SURFACE;

    /// Surface ids for the walks below: `SURFACE_BASE + access-unit index`, so every
    /// picture gets its own and none is ever reused. Well away from slot indices, so
    /// a mix-up shows as a value rather than a plausible off-by-one — and unique, so
    /// a stale or aliased reference cannot hide behind a surface that happens to be
    /// right again.
    const SURFACE_BASE: u32 = 0x9000;
    use crate::va::VA_PICTURE_H264_INVALID;

    /// The vendored conformance vector every other rung's parity legs decode: 250
    /// access units, two slice NALUs per picture, four IDRs, real reordering.
    const TEST_25FPS_H264: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// A punktfunk HOST's own output: 120 pictures of 640x480, `max_num_ref_frames = 3`
    /// alongside `max_dec_frame_buffering = 3` and `max_num_reorder_frames = 0`.
    ///
    /// Vendored beside `pf-vkdecode`'s per-frame goldens, and the only stream in the
    /// tree that produces the shape this module's exemption is about. The conformance
    /// vector above cannot: its level gives it a 7-frame DPB against 2 reference
    /// frames, so 8.2.5's sliding window unmarks a picture two access units before
    /// C.4.5.3's bump can evict it, and it reorders, which keeps an unmarked picture
    /// alive past the unit that unmarked it. Both are properties of that vector rather
    /// than of H.264, and between them they hid a defect that fired on 297 of 300
    /// access units of every stream we ship, on two other backends, for two milestones.
    const LOWDELAY_640X480: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h264");

    /// Minimal H.264 access-unit splitter. The production wire delivers whole access
    /// units, so pf-bitstream keeps its splitter test-only; this is the same rule —
    /// a new AU begins at a non-VCL NALU following slices, or at a slice declaring
    /// itself first-in-picture — and the access-unit count asserted below is what
    /// keeps it honest.
    fn split_aus(stream: &[u8]) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let (mut au_start, mut au_has_slice) = (0usize, false);
        let mut i = 0usize;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] != [0x00, 0x00, 0x01] {
                i += 1;
                continue;
            }
            let header = i + 3;
            let mut start = i;
            if start > 0 && stream[start - 1] == 0x00 {
                start -= 1;
            }
            let is_slice = matches!(stream[header] & 0x1f, 1 | 5);
            let first = is_slice && stream.get(header + 1).is_some_and(|b| b & 0x80 != 0);
            if au_has_slice && (!is_slice || first) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
            i += 3;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// Every access unit of a real stream converts, and the parts a driver reads are
    /// self-consistent.
    ///
    /// The unit tests above check one field at a time; this is the one that would
    /// notice a transaction ordering mistake, a slot exhausted mid-stream, or a
    /// slice range that walked off its access unit — none of which a synthetic
    /// single-picture case reaches.
    #[test]
    fn the_whole_vendored_vector_converts() {
        use pf_bitstream::h264::H264Planner;

        let aus = split_aus(TEST_25FPS_H264);
        assert_eq!(aus.len(), 250, "the vendored vector is 250 access units");

        let mut planner = H264Planner::new();
        // The caller's binding, modelled the way the rung does it: every picture is
        // given its OWN never-reused surface id, and the slot table is updated after
        // the conversion returns. Ids start well away from slot indices so a mix-up
        // shows up as a value rather than as a plausible-looking off-by-one, and
        // never reusing one means a stale or aliased reference cannot hide behind a
        // surface that happens to be right again.
        let mut surfaces: Vec<u32> = Vec::new();
        let mut slots: Option<SlotMap> = None;
        let mut converted = 0usize;
        let mut saw_multi_slice = false;
        let mut saw_references = false;

        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            surfaces.resize(map.capacity(), VA_INVALID_SURFACE);
            let setup_surface = SURFACE_BASE + index as u32;
            let out = plan_to_va(&plan, au, map, &surfaces, setup_surface)
                .unwrap_or_else(|e| panic!("AU {index}: conversion failed: {e}"));
            surfaces[usize::from(out.setup_slot)] = setup_surface;

            assert_eq!(
                out.slices.len(),
                plan.slices.len(),
                "AU {index}: one record per slice"
            );
            assert_eq!(out.slice_data.len(), out.slices.len());
            saw_multi_slice |= out.slices.len() > 1;

            for (n, (rec, range)) in out.slices.iter().zip(&out.slice_data).enumerate() {
                assert!(
                    range.end <= au.len() && range.start < range.end,
                    "AU {index} slice {n}: range {range:?} is not inside a {}-byte AU",
                    au.len()
                );
                assert_eq!(
                    rec.slice_data_size as usize,
                    range.end - range.start,
                    "AU {index} slice {n}: declared size must match the range"
                );
                // The payload begins at the NAL header byte: no start code left.
                assert_ne!(
                    &au[range.start..range.start + 3.min(range.end - range.start)],
                    &[0x00, 0x00, 0x01][..],
                    "AU {index} slice {n}: the start code was not trimmed"
                );
                assert!(
                    rec.slice_data_bit_offset > 0,
                    "AU {index} slice {n}: a slice header cannot be zero bits"
                );
                assert!(
                    usize::from(rec.slice_data_bit_offset) < (range.end - range.start) * 8,
                    "AU {index} slice {n}: the header cannot outrun the slice"
                );
            }

            // `reference_frames` mirrors the marked DPB exactly: as many valid
            // entries as the snapshot has, and every entry past it invalidated.
            let valid = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| e.flags & VA_PICTURE_H264_INVALID == 0)
                .count();
            assert_eq!(
                valid,
                plan.dpb_refs.len(),
                "AU {index}: reference_frames must carry the marked DPB and nothing else"
            );
            saw_references |= valid > 0;
            for e in out.pic_params.reference_frames.iter().take(valid) {
                assert!(
                    surfaces.contains(&e.picture_id),
                    "AU {index}: a reference names a surface outside the table"
                );
            }

            assert_eq!(out.pic_params.frame_num, plan.picture.frame_num);
            assert!(usize::from(out.setup_slot) < surfaces.len());
            converted += 1;
        }

        assert_eq!(converted, 250);
        assert!(
            saw_multi_slice,
            "this vector is two slices per picture — a run that never saw one is \
             splitting access units wrong"
        );
        assert!(
            saw_references,
            "a 250-frame vector must reference something"
        );
    }

    /// What one walk of a stream through [`plan_to_va`] measured.
    ///
    /// Every field is a count of ACCESS UNITS, so the four are directly comparable and
    /// each is bounded by [`Self::converted`].
    #[derive(Debug, Default)]
    struct AliasWalk {
        /// Access units planned and converted.
        converted: usize,
        /// The setup picture was assigned a slot this access unit's OWN removals had
        /// just freed. `SlotMap::assign` takes the lowest free slot, so this is the
        /// ordinary case rather than an edge one — and it is why a decode target read
        /// out of a slot-indexed table would be the surface of the picture just
        /// displayed.
        inherited_a_just_freed_slot: usize,
        /// This access unit's own `removed` list names a picture its `dpb_refs`
        /// snapshot also names: 8.2.5's sliding window unmarked a reference in the very
        /// unit whose C.4.5.3 bump evicted it. The aliasing PRECONDITION, and the shape
        /// the vendored conformance vector never produces.
        removed_and_referenced: usize,
        /// The setup picture took the slot of a picture this same access unit READS.
        /// This is the D3D11VA/Vulkan defect verbatim — `CurrPic` and a reference entry
        /// resolving through one slot — and on those two backends the surface followed
        /// the slot, so the submission aliased. Here the surface does not follow the
        /// slot, which is what [`Self::aliased`] measures.
        setup_took_a_read_pictures_slot: usize,
        /// The submission names the decode target as one of its own references, in
        /// `reference_frames` or in any slice's `RefPicList0`/`1`. Must be zero.
        aliased: usize,
    }

    /// Drive `stream` through the planner and [`plan_to_va`], modelling the caller the
    /// way the Linux rung is written, and count the four shapes above.
    ///
    /// The model is one line and it is the whole contract: the decode target is a
    /// surface that **is not in the table the conversion is handed**, and it enters
    /// that table only after the conversion returns. `video_vaapi_native`'s
    /// `the_low_delay_stream_never_hands_the_decoder_a_surface_it_is_predicting_from`
    /// is the same walk driven through the REAL `Session` pool, which is what says the
    /// rung honours the contract; this one says what the contract buys.
    fn walk_for_aliasing(stream: &[u8]) -> AliasWalk {
        use pf_bitstream::h264::H264Planner;

        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        // Slot to surface — precisely `Session::surface_table()` on the Linux rung.
        let mut table: Vec<u32> = Vec::new();
        let mut out = AliasWalk::default();

        for (index, au) in split_aus(stream).into_iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: this stream must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            assert_eq!(
                map.capacity(),
                plan.picture.max_dpb_frames + 1,
                "AU {index}: neither stream renegotiates its DPB depth mid-walk"
            );
            table.resize(map.capacity(), VA_INVALID_SURFACE);

            if plan
                .dpb
                .removed
                .iter()
                .any(|id| plan.dpb_refs.iter().any(|r| r.id == *id))
            {
                out.removed_and_referenced += 1;
            }
            // Which slots this AU's removals will free, read BEFORE the conversion
            // applies them — afterwards the ledger has forgotten.
            let freed: Vec<u8> = plan
                .dpb
                .removed
                .iter()
                .filter_map(|id| map.slot_of(*id))
                .collect();

            // Ids start well away from slot indices and are never reused, so a stale or
            // aliased reference shows up as a value rather than as a plausible-looking
            // off-by-one and cannot hide behind a surface that happens to be right
            // again. The assertion is the model's own precondition: a target the table
            // already names would beg the question this walk exists to answer.
            let setup_surface = SURFACE_BASE + index as u32;
            assert!(
                !table.contains(&setup_surface),
                "AU {index}: the model handed out a surface the table already names"
            );
            let displaced = table.clone();
            let converted = plan_to_va(&plan, au, map, &table, setup_surface)
                .unwrap_or_else(|e| panic!("AU {index}: conversion failed: {e}"));
            table[usize::from(converted.setup_slot)] = setup_surface;

            // Both readable sets, and they are not the same snapshot: `dpb_refs` is
            // taken after this AU's marking process, the per-slice lists before it.
            let named: Vec<u32> = converted
                .pic_params
                .reference_frames
                .iter()
                .chain(
                    converted
                        .slices
                        .iter()
                        .flat_map(|s| s.ref_pic_list0.iter().chain(s.ref_pic_list1.iter())),
                )
                .filter(|e| e.flags & VA_PICTURE_H264_INVALID == 0)
                .map(|e| e.picture_id)
                .collect();

            if freed.contains(&converted.setup_slot) {
                out.inherited_a_just_freed_slot += 1;
            }
            let evicted_surface = displaced[usize::from(converted.setup_slot)];
            if evicted_surface != VA_INVALID_SURFACE && named.contains(&evicted_surface) {
                out.setup_took_a_read_pictures_slot += 1;
            }
            assert_eq!(
                converted.pic_params.curr_pic.picture_id, setup_surface,
                "AU {index}: the current picture must be the surface the caller bound"
            );
            if named.contains(&setup_surface) {
                out.aliased += 1;
            }
            out.converted += 1;
        }
        out
    }

    /// The setup picture routinely inherits a slot its own access unit just freed —
    /// which is why the decode target is a PARAMETER and not `surfaces[setup_slot]`.
    ///
    /// `SlotMap::assign` takes the LOWEST free slot, and a slot freed by this access
    /// unit's own removals is free by the time the setup picture is assigned. Measured
    /// on the vendored vector that is not an edge case: **225 of 250** access units. A
    /// surface bound BY SLOT would therefore decode, on nine frames in ten, into the
    /// surface still holding the picture that was just displayed — which under
    /// zero-copy the consumer may still be sampling. Hence the pool model this crate's
    /// callers use, and hence `setup_surface`.
    ///
    /// ⚠ This test used to carry a second half asserting the decode target was never
    /// also a reference. It was VACUOUS: the walk hands every picture its own
    /// never-reused surface id, so distinct ids cannot collide and the assertion could
    /// not fail whatever the conversion did. The real question needs a surface pool
    /// that RECYCLES, and it is answered by the two tests below and by
    /// `video_vaapi_native`'s walk through the real one.
    #[test]
    fn the_setup_picture_routinely_inherits_a_just_freed_slot() {
        let walk = walk_for_aliasing(TEST_25FPS_H264);
        assert_eq!(walk.converted, 250);
        // A floor rather than the exact count, so a planner change that shifts it by a
        // frame does not fail — but one that made slot reuse RARE would, and would mean
        // the documentation citing this number has stopped being true.
        assert!(
            walk.inherited_a_just_freed_slot > 200,
            "the setup picture inherited a just-freed slot on only {} of 250 access \
             units — the reason `setup_surface` is a parameter no longer holds, and the \
             documentation that cites it needs re-measuring",
            walk.inherited_a_just_freed_slot
        );
    }

    /// The aliasing PRECONDITION, on both streams — the number that says the exemption
    /// below is being tested by something rather than merely passing.
    ///
    /// Two conditions have to coincide inside ONE access unit for a conversion that
    /// releases eagerly to hand the decode target a picture it is predicting from: the
    /// access unit must remove a picture, and that picture must still be in the
    /// `dpb_refs` snapshot the reference lists are built from. Low-delay H.264 is
    /// exactly what makes them coincide, and NVENC seals it by writing
    /// `max_num_ref_frames = 3` ALONGSIDE `max_dec_frame_buffering = 3` — a DPB exactly
    /// as deep as its reference count — while `max_num_reorder_frames = 0` means the
    /// evicted picture has already been output and is therefore evictable at all.
    ///
    /// The vendored conformance vector produces the shape ZERO times, which is why it
    /// proved nothing on two other backends for two milestones. If that zero ever moves
    /// the reasoning above is wrong and the 117 needs re-deriving before it means
    /// anything.
    #[test]
    fn the_low_delay_stream_reassigns_slots_whose_pictures_it_still_reads() {
        let vector = walk_for_aliasing(TEST_25FPS_H264);
        assert_eq!(vector.converted, 250);
        assert!(
            vector.inherited_a_just_freed_slot > 0,
            "no access unit of the vendored vector reused a freed slot, so the zeroes \
             below would be empty for a reason that has nothing to do with the hazard"
        );
        assert_eq!(
            vector.removed_and_referenced, 0,
            "the vendored vector is supposed to be BLIND to this shape"
        );
        assert_eq!(
            vector.setup_took_a_read_pictures_slot, 0,
            "and therefore never to hand the setup picture a slot it still reads"
        );

        let lowdelay = walk_for_aliasing(LOWDELAY_640X480);
        assert_eq!(lowdelay.converted, 120);
        assert_eq!(
            lowdelay.removed_and_referenced, 117,
            "the low-delay stream must still exercise the aliasing precondition on \
             nearly every access unit — if this drops to zero the exemption below is no \
             longer being TESTED by anything, whatever else still passes"
        );
        assert_eq!(
            lowdelay.setup_took_a_read_pictures_slot, 117,
            "and the slot really is handed straight back to the decode target: this is \
             the D3D11VA/Vulkan defect, present here, and harmless only because the \
             SURFACE does not follow the slot"
        );
    }

    /// The exemption itself: no submission names its decode target as one of its own
    /// references, on either stream.
    ///
    /// This conversion still releases its whole `removed` list inline, exactly as the
    /// two backends that had to grow a `release_after_decode` deferral once did. It is
    /// safe doing so for one reason, and it is a property of the INTERFACE rather than
    /// of any stream: `plan_to_va` never invents a surface. Every reference it can name
    /// is read out of the `surfaces` table it was handed, so a decode target that is
    /// not in that table cannot be named, whatever the ledger does with slots. A slot
    /// is not a surface here; on DXVA it was.
    ///
    /// ⚠ That makes this a statement about the CALLER's contract, so it is only half
    /// the proof. The other half — that the Linux rung really does pick its decode
    /// target from outside the table — cannot be made here, because the pool lives in
    /// `pf-client-core`. It is
    /// `video_vaapi_native`'s
    /// `the_low_delay_stream_never_hands_the_decoder_a_surface_it_is_predicting_from`,
    /// which drives this same stream through the real `Session`.
    #[test]
    fn no_submission_names_its_decode_target_as_one_of_its_own_references() {
        for (name, walk) in [
            ("the vendored vector", walk_for_aliasing(TEST_25FPS_H264)),
            ("the low-delay stream", walk_for_aliasing(LOWDELAY_640X480)),
        ] {
            assert_eq!(
                walk.aliased, 0,
                "{name}: {} of {} access units decode into a surface they predict from",
                walk.aliased, walk.converted
            );
        }
    }

    /// A decode target the caller took from INSIDE the slot table is named as its own
    /// reference — the counterfactual that gives the test above its teeth.
    ///
    /// Without this, `aliased == 0` would be consistent with a conversion that could
    /// never alias for reasons of its own, and a reader could not tell which. This
    /// picks the target the way the two broken backends effectively did — the surface
    /// sitting in the slot the setup picture is about to take — and shows the same walk
    /// then aliases on 117 of 120 access units of the low-delay stream. So the walk can
    /// see the defect; it does not see it because the contract holds.
    #[test]
    fn taking_the_decode_target_from_the_slot_table_aliases_on_the_low_delay_stream() {
        use pf_bitstream::h264::H264Planner;

        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut table: Vec<u32> = Vec::new();
        let (mut converted, mut aliased) = (0usize, 0usize);

        for (index, au) in split_aus(LOWDELAY_640X480).into_iter().enumerate() {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            table.resize(map.capacity(), VA_INVALID_SURFACE);
            // The bug, modelled: convert first to learn the slot, then re-run the same
            // access unit against the real ledger with the target read OUT of the
            // table. Two passes only because the slot is not known until the conversion
            // returns; the submission compared below is the second one.
            //
            // The probe's own `setup_surface` is arbitrary and deliberately so — the
            // slot is chosen by `SlotMap::assign` from the ledger alone and no
            // conversion consults the target to pick it, which is why one pass can
            // stand in for the other.
            let mut probe = map.clone();
            let peek = plan_to_va(&plan, au, &mut probe, &table, SURFACE_BASE)
                .expect("the low-delay stream converts");
            let target = table[usize::from(peek.setup_slot)];
            let target = if target == VA_INVALID_SURFACE {
                SURFACE_BASE + index as u32
            } else {
                target
            };
            let out = plan_to_va(&plan, au, map, &table, target).expect("the same conversion");
            table[usize::from(out.setup_slot)] = target;

            let names = |e: &VaPictureH264| {
                e.flags & VA_PICTURE_H264_INVALID == 0 && e.picture_id == target
            };
            if out.pic_params.reference_frames.iter().any(names)
                || out
                    .slices
                    .iter()
                    .any(|s| s.ref_pic_list0.iter().any(names) || s.ref_pic_list1.iter().any(names))
            {
                aliased += 1;
            }
            converted += 1;
        }

        assert_eq!(converted, 120);
        assert_eq!(
            aliased, 117,
            "binding the decode target BY SLOT is supposed to reproduce the defect on \
             this stream; if it no longer does, the exemption test above is passing for \
             a reason nobody has checked"
        );
    }

    #[test]
    fn start_code_len_reads_both_prefix_forms() {
        assert_eq!(start_code_len(&[0, 0, 1, 0x65]), Some(3));
        assert_eq!(start_code_len(&[0, 0, 0, 1, 0x65]), Some(4));
        // A NAL handed over WITHOUT its prefix must not be mistaken for one: the
        // bit offset is relative to the header byte, so trimming the wrong number
        // of bytes shifts every slice.
        assert_eq!(start_code_len(&[0x65, 0x88]), None);
        assert_eq!(start_code_len(&[0, 0, 2, 1]), None);
        assert_eq!(start_code_len(&[0, 0]), None);
    }

    #[test]
    fn a_long_term_reference_is_flagged_long_term() {
        let rp = RefPic {
            id: 7,
            top_field_order_cnt: 4,
            bottom_field_order_cnt: 4,
            is_long_term: true,
            frame_num_or_lt_idx: 2,
        };
        let e = va_ref(&rp, 0x1234);
        assert_eq!(e.flags, VA_PICTURE_H264_LONG_TERM_REFERENCE);
        assert_eq!(e.picture_id, 0x1234);
        // For a long-term picture this field carries LongTermFrameIdx, not frame_num.
        assert_eq!(e.frame_idx, 2);
    }

    #[test]
    fn a_short_term_reference_carries_its_frame_num() {
        let rp = RefPic {
            id: 3,
            top_field_order_cnt: -2,
            bottom_field_order_cnt: -2,
            is_long_term: false,
            frame_num_or_lt_idx: 9,
        };
        let e = va_ref(&rp, 5);
        assert_eq!(e.flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
        assert_eq!(e.frame_idx, 9);
        assert_eq!(e.top_field_order_cnt, -2);
        assert_ne!(e.flags & VA_PICTURE_H264_INVALID, VA_PICTURE_H264_INVALID);
    }
}
