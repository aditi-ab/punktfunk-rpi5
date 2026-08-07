//! One HEVC [`AuPlanH265`] into libva's buffers — the H.265 twin of [`crate::pic`],
//! with the same transaction discipline and the same refusal-over-narrowing posture.
//!
//! # What differs from the H.264 conversion, and why each one bites
//!
//! * **`ReferenceFrames` is 15 entries, not 16.**
//! * **The reference sets are FLAGS, not arrays.** `RefPicSetStCurrBefore/After/LtCurr`
//!   do not exist here: membership is ORed into each DPB entry's own `flags` as
//!   `VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE` / `_AFTER` / `_LT_CURR`. Vulkan wants slot
//!   indices in identically named arrays and DXVA wants list positions in them — this
//!   is the third convention, and confusing the first two is what made HEVC
//!   unplayable on every driver.
//! * **The per-slice lists are INDICES into `ReferenceFrames`**, not pictures and not
//!   surfaces, with `0xff` for an unused entry. So the DPB array must be built first
//!   and every list entry resolved through it — a picture a slice names that is not in
//!   the marked DPB is a refusal here rather than something to paper over.
//! * **The offset is in BYTES.** `slice_data()` is byte-aligned by `byte_alignment()`,
//!   so `header_bit_size / 8` is exact — asserted, not assumed, because a rounded
//!   offset would decode garbage from the first inter picture.
//! * **The IQ matrix is optional** and gated on `scaling_list_enabled_flag`, exactly
//!   as the DXVA rung gates its `qmatrix`. Submitting one built from an all-zero
//!   parser default is the defect review round 13 caught on the DXVA side: a driver
//!   MUST apply what it is handed, so every residual would dequantise to zero.

use std::ops::Range;

use cros_codecs::codec::h265::parser::SliceType as SliceTypeH265;
use pf_bitstream::h265::AuPlan as AuPlanH265;
use pf_bitstream::h265::PicId;

use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::va_h265::LongSliceFlagsH265;
use crate::va_h265::PicFieldsH265;
use crate::va_h265::SliceParsingFieldsH265;
use crate::va_h265::VaIqMatrixBufferHEVC;
use crate::va_h265::VaPictureHEVC;
use crate::va_h265::VaPictureParameterBufferHEVC;
use crate::va_h265::VaSliceParameterBufferHEVC;
use crate::va_h265::REFERENCE_FRAMES_LEN_H265;
use crate::va_h265::REF_PIC_LIST_LEN_H265;
use crate::va_h265::VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
use crate::va_h265::VA_PICTURE_HEVC_RPS_LT_CURR;
use crate::va_h265::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
use crate::va_h265::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
use crate::SlotError;
use crate::SlotMap;

/// Everything one HEVC `vaRenderPicture` sequence needs.
#[derive(Debug, Clone)]
pub struct DecodePlanVaH265 {
    pub pic_params: VaPictureParameterBufferHEVC,
    /// `None` unless the sequence enables scaling lists — the buffer is then not
    /// submitted at all (module docs).
    pub iq_matrix: Option<VaIqMatrixBufferHEVC>,
    pub slices: Vec<VaSliceParameterBufferHEVC>,
    /// Each slice's data range, start code excluded. Parallel to [`Self::slices`].
    pub slice_data: Vec<Range<usize>>,
    pub setup_slot: u8,
}

/// Why an HEVC plan cannot be expressed as VAAPI buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaH265Error {
    NoSlices,
    NoStoredId,
    SeparateColourPlanes,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A slice list, or an RPS set, named a picture the marked DPB does not hold —
    /// and HEVC's lists are indices INTO that array, so there is nothing to fall
    /// back to.
    UnresolvedReference(PicId),
    /// More marked references than `ReferenceFrames[15]` can express.
    TooManyReferences(usize),
    RefListTooLong {
        slice: usize,
        len: usize,
    },
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    SliceRange {
        slice: usize,
    },
    /// `header_bit_size` is not a whole number of bytes. `slice_data()` is
    /// byte-aligned, so this means the parser and this conversion disagree about
    /// where the header ended — never something to round.
    UnalignedSliceHeader {
        slice: usize,
        bits: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaH265Error {
    fn from(e: SlotError) -> Self {
        PlanToVaH265Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToVaH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaH265Error::NoSlices => write!(f, "the access unit planned no slices"),
            PlanToVaH265Error::NoStoredId => write!(f, "the plan stored no picture id"),
            PlanToVaH265Error::SeparateColourPlanes => {
                write!(f, "separate colour planes are outside the envelope")
            }
            PlanToVaH265Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, this stream needs {required}"
            ),
            PlanToVaH265Error::UnresolvedReference(id) => {
                write!(f, "picture {id} is not in the marked DPB array")
            }
            PlanToVaH265Error::TooManyReferences(n) => {
                write!(f, "{n} marked references exceed ReferenceFrames[15]")
            }
            PlanToVaH265Error::RefListTooLong { slice, len } => {
                write!(f, "slice {slice}: reference list of {len} exceeds 15")
            }
            PlanToVaH265Error::SurfaceOutOfRange { slot, surfaces } => {
                write!(f, "DPB slot {slot} has no surface in a table of {surfaces}")
            }
            PlanToVaH265Error::SliceRange { slice } => {
                write!(
                    f,
                    "slice {slice}: byte range is not a start-code-prefixed NAL"
                )
            }
            PlanToVaH265Error::UnalignedSliceHeader { slice, bits } => write!(
                f,
                "slice {slice}: a {bits}-bit header is not byte-aligned, so \
                 slice_data_byte_offset cannot be exact"
            ),
            PlanToVaH265Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaH265Error {}

/// Convert one planned HEVC access unit. See [`crate::pic::plan_to_va`] for the
/// parameter contract — `au`, `surfaces` and `setup_surface` mean the same things,
/// including the reason the decode target is bound by the caller rather than read
/// out of a slot-indexed table.
pub fn plan_to_va_h265(
    plan: &AuPlanH265,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVaH265, PlanToVaH265Error> {
    if plan.slices.is_empty() {
        return Err(PlanToVaH265Error::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToVaH265Error::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if sps.separate_colour_plane_flag {
        return Err(PlanToVaH265Error::SeparateColourPlanes);
    }
    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVaH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // See the H.264 twin: a pre-check, so the caller's post-call bind of
    // `setup_surface` to the returned slot is always in range.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaH265Error::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }
    if plan.dpb_refs.len() > REFERENCE_FRAMES_LEN_H265 {
        return Err(PlanToVaH265Error::TooManyReferences(plan.dpb_refs.len()));
    }

    // --- the DPB array, and the index every slice list will speak in ---------
    //
    // Built FIRST because the per-slice lists are indices into it. `dpb_refs` is
    // the marked DPB — a superset of the three current sets, since RefPicSet*Foll
    // pictures stay marked for later access units.
    let mut reference_frames = [VaPictureHEVC::invalid(); REFERENCE_FRAMES_LEN_H265];
    let mut index_of: Vec<(PicId, u8)> = Vec::with_capacity(plan.dpb_refs.len());
    for (slot_out, rp) in reference_frames.iter_mut().zip(&plan.dpb_refs) {
        let slot = slots
            .slot_of(rp.id)
            .ok_or(PlanToVaH265Error::UnresolvedReference(rp.id))?;
        let surface =
            *surfaces
                .get(usize::from(slot))
                .ok_or(PlanToVaH265Error::SurfaceOutOfRange {
                    slot,
                    surfaces: surfaces.len(),
                })?;
        *slot_out = VaPictureHEVC {
            picture_id: surface,
            pic_order_cnt: rp.pic_order_cnt,
            flags: if rp.is_long_term {
                VA_PICTURE_HEVC_LONG_TERM_REFERENCE
            } else {
                0
            },
            va_reserved: [0; 4],
        };
        index_of.push((rp.id, index_of.len() as u8));
    }

    // Membership flags, ORed onto the entries the three current sets name. This is
    // VAAPI's whole expression of the RPS — there is no array to fill.
    for (set, flag) in [
        (&plan.rps.st_curr_before, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE),
        (&plan.rps.st_curr_after, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER),
        (&plan.rps.lt_curr, VA_PICTURE_HEVC_RPS_LT_CURR),
    ] {
        for rp in set {
            let idx = index_of
                .iter()
                .find(|(id, _)| *id == rp.id)
                .map(|(_, i)| usize::from(*i))
                .ok_or(PlanToVaH265Error::UnresolvedReference(rp.id))?;
            reference_frames[idx].flags |= flag;
        }
    }

    let find_index = |id: PicId| -> Result<u8, PlanToVaH265Error> {
        index_of
            .iter()
            .find(|(other, _)| *other == id)
            .map(|(_, i)| *i)
            .ok_or(PlanToVaH265Error::UnresolvedReference(id))
    };

    // --- per-slice records ---------------------------------------------------

    let mut slices = Vec::with_capacity(plan.slices.len());
    let mut slice_data = Vec::with_capacity(plan.slices.len());
    for (index, sp) in plan.slices.iter().enumerate() {
        let hdr = &sp.header;
        let mut rec = VaSliceParameterBufferHEVC::zeroed();

        let bytes = au
            .get(sp.data.clone())
            .ok_or(PlanToVaH265Error::SliceRange { slice: index })?;
        let prefix = crate::pic::start_code_len(bytes)
            .ok_or(PlanToVaH265Error::SliceRange { slice: index })?;
        let payload = sp.data.start + prefix..sp.data.end;
        rec.slice_data_size = (payload.end - payload.start) as u32;
        rec.slice_data_offset = 0;
        rec.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        if hdr.header_bit_size % 8 != 0 {
            return Err(PlanToVaH265Error::UnalignedSliceHeader {
                slice: index,
                bits: hdr.header_bit_size,
            });
        }
        rec.slice_data_byte_offset = hdr.header_bit_size / 8;
        rec.slice_data_num_emu_prevn_bytes = hdr.n_emulation_prevention_bytes as u16;
        slice_data.push(payload);

        rec.slice_segment_address = hdr.segment_address;
        rec.collocated_ref_idx = hdr.collocated_ref_idx;
        rec.num_ref_idx_l0_active_minus1 = hdr.num_ref_idx_l0_active_minus1;
        rec.num_ref_idx_l1_active_minus1 = hdr.num_ref_idx_l1_active_minus1;
        rec.slice_qp_delta = hdr.qp_delta;
        rec.slice_cb_qp_offset = hdr.cb_qp_offset;
        rec.slice_cr_qp_offset = hdr.cr_qp_offset;
        rec.slice_beta_offset_div2 = hdr.beta_offset_div2;
        rec.slice_tc_offset_div2 = hdr.tc_offset_div2;
        rec.five_minus_max_num_merge_cand = hdr.five_minus_max_num_merge_cand;
        rec.num_entry_point_offsets = hdr.num_entry_point_offsets as u16;

        rec.long_slice_flags = LongSliceFlagsH265 {
            // The plan is one picture, so the last record IS the last slice of it.
            last_slice_of_pic: index + 1 == plan.slices.len(),
            dependent_slice_segment_flag: hdr.dependent_slice_segment_flag,
            // H.265's own numbering (B=0, P=1, I=2), which is what libva's two bits
            // take — no remap, unlike H.264's.
            slice_type: hdr.type_ as u8,
            color_plane_id: 0,
            slice_sao_luma_flag: hdr.sao_luma_flag,
            slice_sao_chroma_flag: hdr.sao_chroma_flag,
            mvd_l1_zero_flag: hdr.mvd_l1_zero_flag,
            cabac_init_flag: hdr.cabac_init_flag,
            slice_temporal_mvp_enabled_flag: hdr.temporal_mvp_enabled_flag,
            slice_deblocking_filter_disabled_flag: hdr.deblocking_filter_disabled_flag,
            collocated_from_l0_flag: hdr.collocated_from_l0_flag,
            slice_loop_filter_across_slices_enabled_flag: hdr
                .loop_filter_across_slices_enabled_flag,
        }
        .pack();

        for (list_out, list_in) in [(0usize, &sp.ref_list0), (1usize, &sp.ref_list1)] {
            if list_in.len() > REF_PIC_LIST_LEN_H265 {
                return Err(PlanToVaH265Error::RefListTooLong {
                    slice: index,
                    len: list_in.len(),
                });
            }
            for (n, rp) in list_in.iter().enumerate() {
                rec.ref_pic_list[list_out][n] = find_index(rp.id)?;
            }
        }

        // 7.3.6.1: an explicit weight table is only coded for a P slice when the PPS
        // enables weighted P prediction, or a B slice when it enables weighted
        // bi-prediction. Copying it anywhere else hands the driver parser defaults as
        // though the stream had coded them.
        let weighted = (pps.weighted_pred_flag && hdr.type_ == SliceTypeH265::P)
            || (pps.weighted_bipred_flag && hdr.type_ == SliceTypeH265::B);
        if weighted {
            let pwt = &hdr.pred_weight_table;
            rec.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
            rec.delta_chroma_log2_weight_denom = pwt.delta_chroma_log2_weight_denom;
            rec.delta_luma_weight_l0 = pwt.delta_luma_weight_l0;
            rec.luma_offset_l0 = pwt.luma_offset_l0;
            rec.delta_chroma_weight_l0 = pwt.delta_chroma_weight_l0;
            rec.delta_luma_weight_l1 = pwt.delta_luma_weight_l1;
            rec.luma_offset_l1 = pwt.luma_offset_l1;
            rec.delta_chroma_weight_l1 = pwt.delta_chroma_weight_l1;

            // libva takes the DERIVED ChromaOffsetLX; the parser stores the coded
            // delta. Clamped into a legal shift because a malformed denominator must
            // not panic a decode thread.
            let denom = (i32::from(pwt.luma_log2_weight_denom)
                + i32::from(pwt.delta_chroma_log2_weight_denom))
            .clamp(0, 7);
            let half_range = if sps.range_extension.high_precision_offsets_enabled_flag {
                1i32 << (i32::from(sps.bit_depth_chroma_minus8) + 8 - 1)
            } else {
                128
            };
            rec.chroma_offset_l0 = chroma_offsets(
                &pwt.delta_chroma_weight_l0,
                &pwt.delta_chroma_offset_l0,
                denom,
                half_range,
            );
            rec.chroma_offset_l1 = chroma_offsets(
                &pwt.delta_chroma_weight_l1,
                &pwt.delta_chroma_offset_l1,
                denom,
                half_range,
            );
        }

        slices.push(rec);
    }

    // --- mutations, after every fallible step --------------------------------

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
    let pic_params = VaPictureParameterBufferHEVC {
        curr_pic: VaPictureHEVC {
            picture_id: setup_surface,
            pic_order_cnt: pic.pic_order_cnt,
            flags: 0,
            va_reserved: [0; 4],
        },
        reference_frames,
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples,
        pic_fields: PicFieldsH265 {
            chroma_format_idc: sps.chroma_format_idc,
            separate_colour_plane_flag: sps.separate_colour_plane_flag,
            pcm_enabled_flag: sps.pcm_enabled_flag,
            scaling_list_enabled_flag: sps.scaling_list_enabled_flag,
            transform_skip_enabled_flag: pps.transform_skip_enabled_flag,
            amp_enabled_flag: sps.amp_enabled_flag,
            strong_intra_smoothing_enabled_flag: sps.strong_intra_smoothing_enabled_flag,
            sign_data_hiding_enabled_flag: pps.sign_data_hiding_enabled_flag,
            constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
            cu_qp_delta_enabled_flag: pps.cu_qp_delta_enabled_flag,
            weighted_pred_flag: pps.weighted_pred_flag,
            weighted_bipred_flag: pps.weighted_bipred_flag,
            transquant_bypass_enabled_flag: pps.transquant_bypass_enabled_flag,
            tiles_enabled_flag: pps.tiles_enabled_flag,
            entropy_coding_sync_enabled_flag: pps.entropy_coding_sync_enabled_flag,
            pps_loop_filter_across_slices_enabled_flag: pps.loop_filter_across_slices_enabled_flag,
            loop_filter_across_tiles_enabled_flag: pps.loop_filter_across_tiles_enabled_flag,
            pcm_loop_filter_disabled_flag: sps.pcm_loop_filter_disabled_flag,
            // Both are DERIVED hints a decoder may optimise on. libavcodec's VAAPI
            // backend leaves them 0 for every stream, and a wrong "no reordering"
            // claim is a correctness bug rather than a slow path — so 0 it is.
            no_pic_reordering_flag: false,
            no_bi_pred_flag: false,
        }
        .pack(),
        sps_max_dec_pic_buffering_minus1: sps.max_dec_pic_buffering_minus1
            [usize::from(sps.max_sub_layers_minus1)],
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps
            .log2_diff_max_min_pcm_luma_coding_block_size,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        init_qp_minus26: pps.init_qp_minus26,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: pps.cb_qp_offset,
        pps_cr_qp_offset: pps.cr_qp_offset,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2,
        num_tile_columns_minus1: pps.num_tile_columns_minus1,
        num_tile_rows_minus1: pps.num_tile_rows_minus1,
        column_width_minus1: narrow_19(&pps.column_width_minus1),
        row_height_minus1: narrow_21(&pps.row_height_minus1),
        slice_parsing_fields: SliceParsingFieldsH265 {
            lists_modification_present_flag: pps.lists_modification_present_flag,
            long_term_ref_pics_present_flag: sps.long_term_ref_pics_present_flag,
            sps_temporal_mvp_enabled_flag: sps.temporal_mvp_enabled_flag,
            cabac_init_present_flag: pps.cabac_init_present_flag,
            output_flag_present_flag: pps.output_flag_present_flag,
            dependent_slice_segments_enabled_flag: pps.dependent_slice_segments_enabled_flag,
            pps_slice_chroma_qp_offsets_present_flag: pps.slice_chroma_qp_offsets_present_flag,
            sample_adaptive_offset_enabled_flag: sps.sample_adaptive_offset_enabled_flag,
            deblocking_filter_override_enabled_flag: pps.deblocking_filter_override_enabled_flag,
            pps_disable_deblocking_filter_flag: pps.deblocking_filter_disabled_flag,
            slice_segment_header_extension_present_flag: pps
                .slice_segment_header_extension_present_flag,
            rap_pic_flag: pic.is_irap,
            idr_pic_flag: pic.is_idr,
            // An IRAP picture is intra by definition; nothing in our envelope codes
            // an intra-only non-IRAP picture.
            intra_pic_flag: pic.is_irap,
        }
        .pack(),
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets,
        num_long_term_ref_pic_sps: sps.num_long_term_ref_pics_sps,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        pps_beta_offset_div2: pps.beta_offset_div2,
        pps_tc_offset_div2: pps.tc_offset_div2,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        st_rps_bits: pic.short_term_ref_pic_set_size_bits,
        va_reserved: [0; 8],
    };

    // The DXVA rung's rule, for the same reason: PPS lists win unless only the SPS
    // carried them. Gated on scaling_list_enabled_flag so a stream that codes none
    // gets no buffer at all rather than a table of parser defaults.
    let iq_matrix = sps.scaling_list_enabled_flag.then(|| {
        let sl = if sps.scaling_list_data_present_flag && !pps.scaling_list_data_present_flag {
            &sps.scaling_list
        } else {
            &pps.scaling_list
        };
        VaIqMatrixBufferHEVC {
            scaling_list4x4: sl.scaling_list_4x4,
            scaling_list8x8: sl.scaling_list_8x8,
            scaling_list16x16: sl.scaling_list_16x16,
            // Only matrixIds 0 and 3 exist at 32x32; the parser keeps six slots.
            scaling_list32x32: [sl.scaling_list_32x32[0], sl.scaling_list_32x32[3]],
            // libva takes the VALUE, the parser stores `minus8`.
            scaling_list_dc16x16: std::array::from_fn(|i| {
                (sl.scaling_list_dc_coef_minus8_16x16[i] + 8) as u8
            }),
            scaling_list_dc32x32: [
                (sl.scaling_list_dc_coef_minus8_32x32[0] + 8) as u8,
                (sl.scaling_list_dc_coef_minus8_32x32[3] + 8) as u8,
            ],
            va_reserved: [0; 4],
        }
    });

    Ok(DecodePlanVaH265 {
        pic_params,
        iq_matrix,
        slices,
        slice_data,
        setup_slot,
    })
}

/// `ChromaOffsetLX` per equation 7-56.
///
/// libva takes the derived value; the vendored parser stores the coded
/// `delta_chroma_offset_lX`, so the derivation happens here rather than being
/// mistaken for a straight copy — which would put a delta where a driver expects an
/// offset and tint every weighted-predicted block.
fn chroma_offsets(
    delta_weight: &[[i8; 2]; 15],
    delta_offset: &[[i16; 2]; 15],
    chroma_log2_weight_denom: i32,
    half_range: i32,
) -> [[i8; 2]; 15] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let weight = (1i32 << chroma_log2_weight_denom) + i32::from(delta_weight[i][j]);
            let offset = half_range + i32::from(delta_offset[i][j])
                - ((half_range * weight) >> chroma_log2_weight_denom);
            offset.clamp(-half_range, half_range - 1).clamp(-128, 127) as i8
        })
    })
}

/// `column_width_minus1` is `u32` in the parser and `u16` in libva; a value past
/// 65535 columns is impossible for any real picture, so saturating is honest here
/// and a wrap would not be.
fn narrow_19(src: &[u32; 19]) -> [u16; 19] {
    std::array::from_fn(|i| u16::try_from(src[i]).unwrap_or(u16::MAX))
}

fn narrow_21(src: &[u32; 21]) -> [u16; 21] {
    std::array::from_fn(|i| u16::try_from(src[i]).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::va_h265::REF_PIC_LIST_UNUSED;
    use crate::va_h265::VA_PICTURE_HEVC_INVALID;

    /// `SURFACE_BASE + access-unit index` — see the H.264 twin's constant.
    const SURFACE_BASE: u32 = 0xa000;

    const TEST_25FPS_H265: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );
    const TEST_MAIN10_H265: &[u8] = include_bytes!("../../pf-vkdecode/tests/data/test-main10.h265");

    /// HEVC access-unit splitter — two-byte NAL header, so
    /// `first_slice_segment_in_pic_flag` is the top bit at `+2` where H.264 reads
    /// `+1`, and "is a slice" is the range `< 32` rather than an enum pair.
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
            let is_slice = (stream[header] >> 1) & 0x3f < 32;
            let first = is_slice && stream.get(header + 2).is_some_and(|b| b & 0x80 != 0);
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

    fn walk(stream: &[u8], expect_aus: usize, label: &str) {
        use pf_bitstream::h265::H265Planner;

        let aus = split_aus(stream);
        assert_eq!(aus.len(), expect_aus, "{label}: access-unit count");

        let mut planner = H265Planner::new();
        // The caller's binding model — see the H.264 twin: one never-reused surface
        // id per picture, bound to its slot after the conversion returns.
        let mut surfaces: Vec<u32> = Vec::new();
        let mut slots: Option<SlotMap> = None;
        let mut saw_rps_flags = false;
        let mut saw_list_entries = false;

        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("{label} AU {index}: must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            surfaces.resize(map.capacity(), crate::va::VA_INVALID_SURFACE);
            let setup_surface = SURFACE_BASE + index as u32;
            let out = plan_to_va_h265(&plan, au, map, &surfaces, setup_surface)
                .unwrap_or_else(|e| panic!("{label} AU {index}: conversion failed: {e}"));
            surfaces[usize::from(out.setup_slot)] = setup_surface;

            assert_eq!(out.slices.len(), plan.slices.len());
            for (n, (rec, range)) in out.slices.iter().zip(&out.slice_data).enumerate() {
                assert!(range.end <= au.len() && range.start < range.end);
                assert_eq!(rec.slice_data_size as usize, range.end - range.start);
                assert_ne!(
                    &au[range.start..range.start + 3.min(range.end - range.start)],
                    &[0x00, 0x00, 0x01][..],
                    "{label} AU {index} slice {n}: start code not trimmed"
                );
                assert!(rec.slice_data_byte_offset > 0);
                assert!((rec.slice_data_byte_offset as usize) < range.end - range.start);
                // Every used list entry must index a VALID DPB array slot — HEVC's
                // lists are indices, so a stale 0xff or an out-of-range index is a
                // silently wrong reference rather than a refusal.
                for list in &rec.ref_pic_list {
                    for &idx in list.iter().filter(|&&i| i != REF_PIC_LIST_UNUSED) {
                        saw_list_entries = true;
                        let e = out.pic_params.reference_frames[usize::from(idx)];
                        assert_eq!(
                            e.flags & VA_PICTURE_HEVC_INVALID,
                            0,
                            "{label} AU {index}: a list entry indexes an invalid DPB slot"
                        );
                    }
                }
            }

            let valid = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| e.flags & VA_PICTURE_HEVC_INVALID == 0)
                .count();
            assert_eq!(valid, plan.dpb_refs.len(), "{label} AU {index}: DPB count");

            let rps_marked = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| {
                    e.flags
                        & (VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
                            | VA_PICTURE_HEVC_RPS_ST_CURR_AFTER
                            | VA_PICTURE_HEVC_RPS_LT_CURR)
                        != 0
                })
                .count();
            assert_eq!(
                rps_marked,
                plan.rps.st_curr_before.len()
                    + plan.rps.st_curr_after.len()
                    + plan.rps.lt_curr.len(),
                "{label} AU {index}: exactly the current sets carry RPS flags"
            );
            saw_rps_flags |= rps_marked > 0;
            assert_eq!(
                out.pic_params.curr_pic.pic_order_cnt,
                plan.picture.pic_order_cnt
            );
        }
        assert!(
            saw_rps_flags,
            "{label}: no picture ever carried an RPS flag"
        );
        assert!(saw_list_entries, "{label}: no slice ever named a reference");
    }

    #[test]
    fn the_whole_vendored_vector_converts() {
        walk(TEST_25FPS_H265, 250, "8-bit");
    }

    /// The Main 10 vector too — the pic-params carry depth, and a conversion that
    /// only ever saw 8-bit would not notice a depth field wired to a constant.
    #[test]
    fn the_main10_vector_converts() {
        walk(TEST_MAIN10_H265, 50, "Main 10");
    }
}
