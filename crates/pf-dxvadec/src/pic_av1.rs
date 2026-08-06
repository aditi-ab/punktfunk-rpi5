//! One AV1 [`AuPlan`] into the DXVA structures — M7's Windows conversion.
//!
//! The layouts it fills are measured against the Windows SDK's own `dxva.h`
//! ([`crate::dxva_av1`]); this module is where the AV1 frame header's meaning is
//! mapped onto them, and where the three places DXVA disagrees with the other
//! backends are handled.
//!
//! # The reference numbering — a FOURTH convention
//!
//! This program has now written down four spellings of "which pictures does this
//! frame use":
//!
//! * Vulkan H.265: DPB **slot** indices in `RefPicSetStCurr*`;
//! * DXVA H.265: **positions into `RefPicList[]`** in identically named arrays;
//! * VAAPI H.265: membership **flags** ORed onto each DPB entry;
//! * **DXVA AV1: two arrays that mean different things at once.**
//!   `frame_refs[7]` is indexed by reference NAME (`LAST`..`ALTREF`) and each entry
//!   carries a **surface index** plus that reference's own global motion, while
//!   `RefFrameMapTextureIndex[8]` is indexed by **reference SLOT** and states the
//!   whole reference store. Vulkan spells the first of those as slot indices in
//!   `referenceNameSlotIndices`; here it is the surface. Getting them the wrong way
//!   round is not a refusal, it is a frame predicted from the wrong picture.
//!
//! # Global motion lives per reference
//!
//! Vulkan hangs one `StdVideoAV1GlobalMotion` block off the picture info, with an
//! eight-entry array inside it. DXVA puts each reference's warp parameters in that
//! reference's own `DXVA_PicEntry_AV1`. Both are indexed by reference NAME —
//! `global_motion_params()` loops `ref = LAST_FRAME..ALTREF_FRAME` — so the
//! Vulkan block is a straight copy and DXVA's per-entry read is
//! `gm_params[LAST_FRAME + name]`. Reading it by DPB SLOT instead is the exact
//! transposition that silently gives every warped reference somebody else's warp;
//! it agrees with the truth only while reference `i` happens to sit in slot `i+1`.

use std::ops::Range;

use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::FrameType;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::av1::REFS_PER_FRAME;

use crate::dxva_av1::CdefAv1;
use crate::dxva_av1::CdefFlagsAv1;
use crate::dxva_av1::CdefStrength;
use crate::dxva_av1::CodingFlagsAv1;
use crate::dxva_av1::FilmGrainAv1;
use crate::dxva_av1::FilmGrainFlagsAv1;
use crate::dxva_av1::FormatFlagsAv1;
use crate::dxva_av1::GlobalMotionFlags;
use crate::dxva_av1::LoopFilterAv1;
use crate::dxva_av1::LoopFilterFlagsAv1;
use crate::dxva_av1::PicEntryAv1;
use crate::dxva_av1::PicParamsAv1;
use crate::dxva_av1::QuantizationAv1;
use crate::dxva_av1::QuantizationFlagsAv1;
use crate::dxva_av1::SegmentFeatureMask;
use crate::dxva_av1::SegmentationAv1;
use crate::dxva_av1::SegmentationFlagsAv1;
use crate::dxva_av1::TileAv1;
use crate::dxva_av1::TilesAv1;
use crate::dxva_av1::UNUSED_INDEX;
use crate::SlotError;
use crate::SlotMap;

/// `DXVA_PicParams_AV1::tiles` holds at most 64 column and 64 row sizes.
pub const MAX_TILE_DIM: usize = 64;

/// `log2_restoration_unit_size` on a frame that restores nothing.
///
/// Not a meaningful size — every plane's `frame_restoration_type` is NONE and a
/// driver reading the field at all has nothing to apply it to. It is 8 because
/// that is what libavcodec's `dxva2_av1.c` writes, and 8 is the top of the range
/// `dxva.h` documents (6..8); the parser's own array is still zero here, whose
/// `trailing_zeros` would be 16.
const LOG2_RESTORATION_UNIT_SIZE_UNUSED: u16 = 8;

/// `LAST_FRAME` (AV1 spec): the first reference NAME, and the offset between a
/// position in `ref_frame_idx` and the index the spec's per-reference arrays
/// (global motion, order hints, sign bias) use. `INTRA_FRAME` is 0.
const LAST_FRAME: usize = 1;

/// Everything one AV1 `SubmitDecoderBuffers` call needs.
#[derive(Debug, Clone)]
pub struct DecodePlanDxvaAv1 {
    pub pic_params: PicParamsAv1,
    /// One record per tile group, in plan order. Their `DataOffset`/`DataSize` are
    /// AU-relative here and are REBASED by the packer, exactly as the H.264 and
    /// H.265 slice-control records are.
    pub tiles: Vec<TileAv1>,
    /// Each tile group's byte range in the access unit — what the packer copies.
    pub tile_ranges: Vec<Range<usize>>,
    pub setup_slot: u8,
    pub setup_id: PicId,
}

/// Why a plan cannot be expressed as DXVA AV1 buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaAv1Error {
    /// A `show_existing_frame` plan decodes nothing and has no submission.
    NoDecode,
    NoTiles,
    /// A reference the slot map does not hold.
    UnresolvedReference(PicId),
    /// More tile columns or rows than the picture parameters can express.
    TooManyTiles {
        cols: u32,
        rows: u32,
    },
    /// A field wider than its DXVA type.
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToDxvaAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToDxvaAv1Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToDxvaAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaAv1Error::NoDecode => {
                write!(f, "a show_existing_frame plan has no decode submission")
            }
            PlanToDxvaAv1Error::NoTiles => write!(f, "the frame carried no tile group"),
            PlanToDxvaAv1Error::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToDxvaAv1Error::TooManyTiles { cols, rows } => write!(
                f,
                "{cols}x{rows} tiles exceed the {MAX_TILE_DIM}-entry picture parameters"
            ),
            PlanToDxvaAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its DXVA field")
            }
            PlanToDxvaAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToDxvaAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToDxvaAv1Error> {
    u8::try_from(value).map_err(|_| PlanToDxvaAv1Error::FieldOverflow { field, value })
}

/// Convert one planned AV1 frame.
///
/// `status_id` is the caller's `StatusReportFeedbackNumber`. Nothing mutates
/// `slots` until every fallible step has passed.
pub fn plan_to_dxva_av1(
    plan: &AuPlan,
    slots: &mut SlotMap,
    status_id: u32,
) -> Result<DecodePlanDxvaAv1, PlanToDxvaAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaAv1Error::NoDecode)?;
    if plan.tiles.is_empty() {
        return Err(PlanToDxvaAv1Error::NoTiles);
    }
    let h = &*plan.header;
    let seq = &*plan.sequence;

    // --- resolve, before any mutation ------------------------------------
    // The reference store, by SLOT. This is `RefFrameMapTextureIndex`, and it is a
    // statement about the whole store — the same thing `RefFrameList` is for the
    // other two codecs, and the reason an LTR no slice names still has to appear.
    let mut ref_frame_map = [UNUSED_INDEX; NUM_REF_SLOTS];
    for r in &plan.dpb_refs {
        let slot = slots
            .slot_of(r.id)
            .ok_or(PlanToDxvaAv1Error::UnresolvedReference(r.id))?;
        ref_frame_map[usize::from(r.slot)] = slot;
    }

    // The seven reference NAMES. Each carries a surface AND that reference's own
    // global motion (module docs).
    //
    // `plan.refs` is indexed BY NAME and a lost reference leaves a hole, so the
    // name comes off the iterator and holes are skipped — they keep DXVA's
    // `UNUSED_INDEX`. A compacted list (which is what this loop used to receive)
    // renamed every reference after the first loss.
    let mut frame_refs = [PicEntryAv1::zeroed(); REFS_PER_FRAME];
    let inter = !matches!(
        h.frame_type,
        FrameType::KeyFrame | FrameType::IntraOnlyFrame
    );
    if inter {
        for (name, r) in plan.refs.iter().enumerate() {
            let Some(r) = r else { continue };
            let slot = slots
                .slot_of(r.id)
                .ok_or(PlanToDxvaAv1Error::UnresolvedReference(r.id))?;
            // ⚠ Global motion is indexed by reference NAME, never by DPB slot.
            // AV1's `global_motion_params()` loops `ref = LAST_FRAME..ALTREF_FRAME`
            // and the vendored parser stores it that way; libavcodec's
            // `dxva2_av1.c` reads `gm_params[AV1_REF_FRAME_LAST + i]` for
            // `frame_refs[i]`. Reading by slot instead happens to agree only while
            // reference `i` sits in slot `i + 1`, and silently hands every warped
            // reference somebody else's warp the moment it does not.
            let gm_name = LAST_FRAME + name;
            let gm = &h.global_motion_params;
            frame_refs[name] = PicEntryAv1 {
                width: h.upscaled_width,
                height: h.frame_height,
                wmmat: gm.gm_params[gm_name],
                global_motion_flags: GlobalMotionFlags {
                    // `warp_valid` is the parser's `setup_shear` verdict — a warp
                    // whose shear parameters are out of range is unusable, and
                    // DXVA's flag is the inverse.
                    wminvalid: !gm.warp_valid[gm_name],
                    wmtype: gm.gm_type[gm_name] as u8,
                }
                .pack(),
                index: slot,
                reserved16: 0,
            };
        }
    }

    // --- tiles ------------------------------------------------------------
    let t = &h.tile_info;
    if t.tile_cols as usize > MAX_TILE_DIM || t.tile_rows as usize > MAX_TILE_DIM {
        return Err(PlanToDxvaAv1Error::TooManyTiles {
            cols: t.tile_cols,
            rows: t.tile_rows,
        });
    }
    let mut tiles = TilesAv1::zeroed();
    tiles.cols = narrow("tiles.cols", t.tile_cols)?;
    tiles.rows = narrow("tiles.rows", t.tile_rows)?;
    tiles.context_update_id = t.context_update_tile_id as u16;
    // `widths`/`heights` are the per-tile sizes in superblocks, which the parser
    // records as `*_in_sbs_minus_1`. DXVA wants the same minus-one values libav
    // sends, so they ride across unchanged.
    for i in 0..t.tile_cols as usize {
        tiles.widths[i] = t.width_in_sbs_minus_1[i] as u16;
    }
    for i in 0..t.tile_rows as usize {
        tiles.heights[i] = t.height_in_sbs_minus_1[i] as u16;
    }

    let mut tile_records = Vec::with_capacity(plan.tiles.len());
    let mut tile_ranges = Vec::with_capacity(plan.tiles.len());
    for tg in &plan.tiles {
        tile_records.push(TileAv1 {
            // AU-relative; the packer rebases (field docs).
            data_offset: u32::try_from(tg.data.start).map_err(|_| {
                PlanToDxvaAv1Error::FieldOverflow {
                    field: "tile.DataOffset",
                    value: u32::MAX,
                }
            })?,
            data_size: u32::try_from(tg.data.end - tg.data.start).map_err(|_| {
                PlanToDxvaAv1Error::FieldOverflow {
                    field: "tile.DataSize",
                    value: u32::MAX,
                }
            })?,
            row: (tg.tg_start / t.tile_cols.max(1)) as u16,
            column: (tg.tg_start % t.tile_cols.max(1)) as u16,
            reserved16: 0,
            anchor_frame: UNUSED_INDEX,
            reserved8: 0,
        });
        tile_ranges.push(tg.data.clone());
    }

    // --- the blocks -------------------------------------------------------
    let lf = &h.loop_filter_params;
    let mut loop_filter = LoopFilterAv1::zeroed();
    loop_filter.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
    loop_filter.filter_level_u = lf.loop_filter_level[2];
    loop_filter.filter_level_v = lf.loop_filter_level[3];
    loop_filter.sharpness_level = lf.loop_filter_sharpness;
    loop_filter.control_flags = LoopFilterFlagsAv1 {
        mode_ref_delta_enabled: lf.loop_filter_delta_enabled,
        mode_ref_delta_update: lf.loop_filter_delta_update,
        delta_lf_multi: lf.delta_lf_multi,
        delta_lf_present: lf.delta_lf_present,
    }
    .pack();
    loop_filter.ref_deltas = lf.loop_filter_ref_deltas;
    loop_filter.mode_deltas = lf.loop_filter_mode_deltas;
    loop_filter.delta_lf_res = lf.delta_lf_res;
    // Loop restoration.
    //
    // ⚠ DXVA wants the LOG2 of the unit size where the parser records the size
    // itself — and the parser records NOTHING when restoration is off. AV1 5.9.20
    // only computes `LoopRestorationSize` inside `if ( UsesLr )`, so on a frame with
    // every plane's restoration type NONE the vendored parser's array is still
    // `[0, 0, 0]`, and `0u16.trailing_zeros()` is **16** — a restoration unit of
    // 65536 samples, in a field `dxva.h` documents as 6, 7 or 8. That is 271 of the
    // vendored vector's 274 frames.
    //
    // libavcodec's `dxva2_av1.c` sends `uses_lr ? 6 + lr_unit_shift : 8` for luma
    // and `uses_lr ? 6 + lr_unit_shift - lr_uv_shift : 8` for the two chroma planes,
    // and libavcodec is the implementation every driver was validated against, so
    // the OFF value is 8 rather than 0 or 16. With restoration on, the parser's own
    // `loop_restoration_size[i]` already carries the per-plane `>> lr_uv_shift`, so
    // its `trailing_zeros` IS `6 + lr_unit_shift - lr_uv_shift` — the two agree
    // wherever the field is read at all.
    let lr = &h.loop_restoration_params;
    for i in 0..3 {
        loop_filter.frame_restoration_type[i] = lr.frame_restoration_type[i] as u8;
        loop_filter.log2_restoration_unit_size[i] = if lr.uses_lr {
            lr.loop_restoration_size[i].trailing_zeros() as u16
        } else {
            LOG2_RESTORATION_UNIT_SIZE_UNUSED
        };
    }

    let q = &h.quantization_params;
    let mut quantization = QuantizationAv1::zeroed();
    quantization.control_flags = QuantizationFlagsAv1 {
        delta_q_present: q.delta_q_present,
        delta_q_res: narrow("delta_q_res", q.delta_q_res)?,
    }
    .pack();
    quantization.base_qindex = narrow("base_qindex", q.base_q_idx)?;
    quantization.y_dc_delta_q = q.delta_q_y_dc as i8;
    quantization.u_dc_delta_q = q.delta_q_u_dc as i8;
    quantization.v_dc_delta_q = q.delta_q_v_dc as i8;
    quantization.u_ac_delta_q = q.delta_q_u_ac as i8;
    quantization.v_ac_delta_q = q.delta_q_v_ac as i8;
    quantization.qm_y = narrow("qm_y", q.qm_y)?;
    quantization.qm_u = narrow("qm_u", q.qm_u)?;
    quantization.qm_v = narrow("qm_v", q.qm_v)?;

    let c = &h.cdef_params;
    let mut cdef = CdefAv1::zeroed();
    cdef.control_flags = CdefFlagsAv1 {
        damping: narrow("cdef_damping", c.cdef_damping.saturating_sub(3))?,
        bits: narrow("cdef_bits", c.cdef_bits)?,
    }
    .pack();
    // Two fields to a byte (module docs) — not the parallel arrays AV1's syntax
    // and Vulkan's Std block use.
    //
    // ⚠ `secondary` gets TWO bits here, and the parser's value does not fit them:
    // AV1 5.9.19 rewrites the syntax element in place (a coded 3 becomes 4) and
    // cros-codecs follows the spec, while `DXVA_PicParams_AV1` — like VA-API,
    // NVDEC and Vulkan — wants the coded two-bit read, which is what libavcodec's
    // `dxva2_av1.c` sends. Passing the parser's 4 through `pack`'s `& 0x3` would
    // turn the STRONGEST secondary filter into NO filter, silently, on every frame
    // that codes one. `coded_cdef_sec_strength` is the inverse; its docs carry the
    // evidence.
    for i in 0..8 {
        cdef.y_strengths[i] = CdefStrength {
            primary: c.cdef_y_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_y_sec_strength[i]),
        }
        .pack();
        cdef.uv_strengths[i] = CdefStrength {
            primary: c.cdef_uv_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]),
        }
        .pack();
    }

    let s = &h.segmentation_params;
    let mut segmentation = SegmentationAv1::zeroed();
    segmentation.control_flags = SegmentationFlagsAv1 {
        enabled: s.segmentation_enabled,
        update_map: s.segmentation_update_map,
        update_data: s.segmentation_update_data,
        temporal_update: s.segmentation_temporal_update,
    }
    .pack();
    for seg in 0..8 {
        let e = &s.feature_enabled[seg];
        segmentation.feature_mask[seg] = SegmentFeatureMask {
            alt_q: e[0],
            alt_lf_y_v: e[1],
            alt_lf_y_h: e[2],
            alt_lf_u: e[3],
            alt_lf_v: e[4],
            ref_frame: e[5],
            skip: e[6],
            globalmv: e[7],
        }
        .pack();
        segmentation.feature_data[seg] = s.feature_data[seg];
    }

    // Film grain: only where the sequence enables it AND the frame applies it —
    // the same gate the Vulkan conversion uses, and for the same reason.
    let fg_on = seq.film_grain_params_present && h.film_grain_params.apply_grain;
    let mut film_grain = FilmGrainAv1::zeroed();
    if fg_on {
        let fg = &h.film_grain_params;
        film_grain.control_flags = FilmGrainFlagsAv1 {
            apply_grain: true,
            scaling_shift_minus8: fg.grain_scaling_minus_8,
            chroma_scaling_from_luma: fg.chroma_scaling_from_luma,
            ar_coeff_lag: narrow("ar_coeff_lag", fg.ar_coeff_lag)?,
            ar_coeff_shift_minus6: fg.ar_coeff_shift_minus_6,
            grain_scale_shift: fg.grain_scale_shift,
            overlap_flag: fg.overlap_flag,
            clip_to_restricted_range: fg.clip_to_restricted_range,
            matrix_coeff_is_identity: seq.color_config.matrix_coefficients as u32 == 0,
        }
        .pack();
        film_grain.grain_seed = fg.grain_seed;
        // ⚠ DXVA wants [value, scaling] PAIRS where the parser (and Vulkan) keep
        // two parallel arrays. The counts are bounded by the DXVA capacity, and an
        // over-count is refused rather than truncated: fewer scaling points than
        // the stream declared is different grain, not less grain.
        let pts = |name: &'static str, n: u8, cap: usize| -> Result<usize, PlanToDxvaAv1Error> {
            if usize::from(n) > cap {
                return Err(PlanToDxvaAv1Error::FieldOverflow {
                    field: name,
                    value: u32::from(n),
                });
            }
            Ok(usize::from(n))
        };
        let ny = pts(
            "num_y_points",
            fg.num_y_points,
            film_grain.scaling_points_y.len(),
        )?;
        let ncb = pts(
            "num_cb_points",
            fg.num_cb_points,
            film_grain.scaling_points_cb.len(),
        )?;
        let ncr = pts(
            "num_cr_points",
            fg.num_cr_points,
            film_grain.scaling_points_cr.len(),
        )?;
        for i in 0..ny {
            film_grain.scaling_points_y[i] = [fg.point_y_value[i], fg.point_y_scaling[i]];
        }
        for i in 0..ncb {
            film_grain.scaling_points_cb[i] = [fg.point_cb_value[i], fg.point_cb_scaling[i]];
        }
        for i in 0..ncr {
            film_grain.scaling_points_cr[i] = [fg.point_cr_value[i], fg.point_cr_scaling[i]];
        }
        film_grain.num_y_points = fg.num_y_points;
        film_grain.num_cb_points = fg.num_cb_points;
        film_grain.num_cr_points = fg.num_cr_points;
        film_grain
            .ar_coeffs_y
            .copy_from_slice(&fg.ar_coeffs_y_plus_128[..24]);
        film_grain
            .ar_coeffs_cb
            .copy_from_slice(&fg.ar_coeffs_cb_plus_128[..25]);
        film_grain
            .ar_coeffs_cr
            .copy_from_slice(&fg.ar_coeffs_cr_plus_128[..25]);
        film_grain.cb_mult = fg.cb_mult;
        film_grain.cb_luma_mult = fg.cb_luma_mult;
        film_grain.cr_mult = fg.cr_mult;
        film_grain.cr_luma_mult = fg.cr_luma_mult;
        film_grain.cb_offset = fg.cb_offset as i16;
        film_grain.cr_offset = fg.cr_offset as i16;
    }

    // --- mutations, after every fallible step -----------------------------
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        let _ = slots.release(id);
    }
    let setup_slot = match slots.slot_of(setup_id) {
        Some(existing) => existing,
        None => slots.assign(setup_id)?,
    };

    let color = &seq.color_config;
    let mut pic_params = PicParamsAv1::zeroed();
    pic_params.width = h.upscaled_width;
    pic_params.height = h.frame_height;
    pic_params.max_width = u32::from(seq.max_frame_width_minus_1) + 1;
    pic_params.max_height = u32::from(seq.max_frame_height_minus_1) + 1;
    pic_params.curr_pic_texture_index = setup_slot;
    // The superres denominator as DXVA wants it: the real one, not the coded one,
    // and SUPERRES_NUM when superres is off.
    pic_params.superres_denom = if h.use_superres {
        narrow("superres_denom", h.superres_denom)?
    } else {
        SUPERRES_NUM
    };
    pic_params.bitdepth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    pic_params.seq_profile = seq.seq_profile as u8;
    pic_params.tiles = tiles;
    pic_params.coding = CodingFlagsAv1 {
        use_128x128_superblock: seq.use_128x128_superblock,
        intra_edge_filter: seq.enable_intra_edge_filter,
        interintra_compound: seq.enable_interintra_compound,
        masked_compound: seq.enable_masked_compound,
        warped_motion: h.allow_warped_motion,
        dual_filter: seq.enable_dual_filter,
        jnt_comp: seq.enable_jnt_comp,
        screen_content_tools: h.allow_screen_content_tools != 0,
        integer_mv: h.force_integer_mv != 0,
        cdef: seq.enable_cdef,
        restoration: seq.enable_restoration,
        film_grain: seq.film_grain_params_present,
        intrabc: h.allow_intrabc,
        high_precision_mv: h.allow_high_precision_mv,
        switchable_motion_mode: h.is_motion_mode_switchable,
        filter_intra: seq.enable_filter_intra,
        disable_frame_end_update_cdf: h.disable_frame_end_update_cdf,
        disable_cdf_update: h.disable_cdf_update,
        reference_mode: h.reference_select,
        skip_mode: h.skip_mode_present,
        reduced_tx_set: h.reduced_tx_set,
        superres: h.use_superres,
        tx_mode: h.tx_mode as u8,
        use_ref_frame_mvs: h.use_ref_frame_mvs,
        enable_ref_frame_mvs: seq.enable_ref_frame_mvs,
        // The current frame writes at least one reference slot.
        reference_frame_update: h.refresh_frame_flags != 0,
    }
    .pack();
    pic_params.format = FormatFlagsAv1 {
        frame_type: h.frame_type as u8,
        show_frame: h.show_frame,
        showable_frame: h.showable_frame,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        mono_chrome: color.mono_chrome,
    }
    .pack();
    pic_params.primary_ref_frame = narrow("primary_ref_frame", h.primary_ref_frame)?;
    pic_params.order_hint = narrow("order_hint", h.order_hint)?;
    pic_params.order_hint_bits = if seq.enable_order_hint {
        // The parser types this one signed; a negative value would be a parse bug,
        // and turning it into a huge unsigned one here would hide that.
        narrow(
            "order_hint_bits",
            u32::try_from(seq.order_hint_bits_minus_1).map_err(|_| {
                PlanToDxvaAv1Error::FieldOverflow {
                    field: "order_hint_bits_minus_1",
                    value: 0,
                }
            })? + 1,
        )?
    } else {
        0
    };
    pic_params.frame_refs = frame_refs;
    pic_params.ref_frame_map_texture_index = ref_frame_map;
    pic_params.loop_filter = loop_filter;
    pic_params.quantization = quantization;
    pic_params.cdef = cdef;
    pic_params.interp_filter = h.interpolation_filter as u8;
    pic_params.segmentation = segmentation;
    pic_params.film_grain = film_grain;
    pic_params.status_report_feedback_number = status_id;

    Ok(DecodePlanDxvaAv1 {
        pic_params,
        tiles: tile_records,
        tile_ranges,
        setup_slot,
        setup_id,
    })
}

/// `SUPERRES_NUM` (AV1 spec): the denominator that means "no upscaling".
const SUPERRES_NUM: u8 = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;

    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// Convert every frame of the vendored vector and check what a driver reads.
    ///
    /// The anti-vacuity assertions matter as much as the checks: a run that never
    /// saw an inter frame, or never saw the reference store hold a picture the
    /// frame does not name, would pass every check below while exercising none of
    /// the code that makes them interesting.
    #[test]
    fn the_whole_vendored_vector_converts() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut inter, mut store_beyond_refs) = (0u32, 0u32, 0u32);
        let mut gm_by_slot_would_differ = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx =
                    plan_to_dxva_av1(&plan, &mut slots, frames).expect("the clean vector converts");
                frames += 1;

                // Tile records must describe ranges inside the access unit.
                assert_eq!(dx.tiles.len(), dx.tile_ranges.len());
                for (rec, range) in dx.tiles.iter().zip(&dx.tile_ranges) {
                    assert_eq!(rec.data_offset as usize, range.start);
                    assert_eq!(rec.data_size as usize, range.end - range.start);
                    assert!(range.end <= packet.len());
                }

                // The store: every named slot resolves to a real surface, and any
                // slot with no picture stays UNUSED. `0` is a valid surface, so a
                // slot left at 0 by accident would point at a live picture.
                let named = dx
                    .pic_params
                    .ref_frame_map_texture_index
                    .iter()
                    .filter(|i| **i != UNUSED_INDEX)
                    .count();
                assert_eq!(named, plan.dpb_refs.len());
                let referenced = plan.refs.iter().flatten().count();
                if named > referenced {
                    store_beyond_refs += 1;
                }

                if referenced > 0 {
                    inter += 1;
                    // Every reference NAME must carry a surface the store also has,
                    // and each one's global motion must be the entry the AV1 syntax
                    // codes for THAT name.
                    for (name, r) in plan.refs.iter().enumerate() {
                        let e = dx.pic_params.frame_refs[name];
                        let Some(named_ref) = r else {
                            assert_eq!(
                                e.index, UNUSED_INDEX,
                                "an unnamed reference must stay unused, not read as \
                                 surface 0"
                            );
                            continue;
                        };
                        assert_ne!(
                            e.index, UNUSED_INDEX,
                            "reference name {name} carries no surface"
                        );
                        assert!(dx.pic_params.ref_frame_map_texture_index.contains(&e.index));
                        let gm = &plan.header.global_motion_params;
                        // `PicEntryAv1` is `#[repr(packed)]`, so its fields are
                        // copied out before being compared — a reference to one
                        // may be unaligned.
                        let (wmmat, flags) = (e.wmmat, e.global_motion_flags);
                        assert_eq!(
                            wmmat,
                            gm.gm_params[LAST_FRAME + name],
                            "reference name {name} must carry gm_params[LAST_FRAME \
                             + {name}], not the entry at its DPB slot"
                        );
                        assert_eq!(
                            flags,
                            GlobalMotionFlags {
                                wminvalid: !gm.warp_valid[LAST_FRAME + name],
                                wmtype: gm.gm_type[LAST_FRAME + name] as u8,
                            }
                            .pack()
                        );
                        // Would reading by DPB SLOT have given the same answer?
                        let slot = usize::from(named_ref.slot);
                        if gm.gm_params[LAST_FRAME + name] != gm.gm_params[slot]
                            || gm.gm_type[LAST_FRAME + name] != gm.gm_type[slot]
                        {
                            gm_by_slot_would_differ += 1;
                        }
                    }
                }
                assert_eq!(dx.pic_params.curr_pic_texture_index, dx.setup_slot);
            }
        }

        assert_eq!(frames, 274);
        eprintln!("gm reads where name and slot disagree: {gm_by_slot_would_differ}");
        assert!(
            gm_by_slot_would_differ > 0,
            "reading global motion by DPB SLOT never disagreed with reading it by \
             reference NAME on this vector, so the assertions above cannot tell the \
             two apart — which is how the slot read shipped in the first place"
        );
        assert!(inter > 0, "a 274-frame vector must have inter frames");
        assert!(
            store_beyond_refs > 0,
            "the reference store never held a picture the frame did not name — so \
             this run never exercised the difference between RefFrameMapTextureIndex \
             (the whole store) and frame_refs (what this frame uses), which is the \
             distinction the Ally X class of bug lives in"
        );
    }

    /// The CHROMA deblocking levels reach `filter_level_u` / `filter_level_v`, and
    /// `log2_restoration_unit_size` is never the parser's silence.
    ///
    /// Two halves of the same block, both measured on hardware rather than argued.
    ///
    /// The levels first. ⚠ The AV1 Vulkan rung's frame-0 parity leg came back `luma
    /// IDENTICAL, chroma 319/38400 bytes differ` and that signature was reproduced
    /// EXACTLY — count, `|delta|` histogram and the first six differing bytes with
    /// their values — by decoding the vector's frame 0 with `loop_filter_level[2]`
    /// and `[3]` forced to zero in the bitstream. **That divergence turned out NOT
    /// to be a levels bug** (it was a freed sequence header making the driver treat
    /// the frame as monochrome — `pf_vkdecode::session_av1`), so do not cite it as
    /// evidence that a rung got the pair wrong. What it does establish, and what
    /// keeps this test, is the SIGNATURE: frame 0 codes `[1, 7, 8, 12]`, two luma
    /// levels and two chroma ones, and dropping only the chroma pair is invisible
    /// to luma and to every other plane statistic. A rung that lost the pair would
    /// fail Windows parity in a way nothing else here would notice, and this rung's
    /// `[2]` and `[3]` reads are four characters from `[0]` and `[1]`.
    ///
    /// Then the restoration unit size, which is a units defect the vendored parser
    /// invites: `LoopRestorationSize` is only computed inside `if ( UsesLr )`
    /// (5.9.20), so the array is `[0, 0, 0]` on a frame that restores nothing and
    /// `trailing_zeros` turns that into **16** — 271 of these 274 frames, in a field
    /// `dxva.h` documents as 6..8. libavcodec sends 8.
    #[test]
    fn the_chroma_loop_filter_levels_and_the_restoration_unit_size_reach_the_driver() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_chroma_lf, mut with_lr) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = plan_to_dxva_av1(&plan, &mut slots, frames).expect("converts");
                frames += 1;
                let lf = &plan.header.loop_filter_params;
                // `#[repr(packed)]` — copy the block out before reading its fields.
                let sent = dx.pic_params.loop_filter;
                assert_eq!(
                    (
                        sent.filter_level[0],
                        sent.filter_level[1],
                        sent.filter_level_u,
                        sent.filter_level_v
                    ),
                    (
                        lf.loop_filter_level[0],
                        lf.loop_filter_level[1],
                        lf.loop_filter_level[2],
                        lf.loop_filter_level[3]
                    ),
                    "frame {frames}: DXVA splits AV1's four levels into a luma PAIR \
                     plus two named chroma fields — U is index 2 and V is index 3"
                );
                if lf.loop_filter_level[2] != 0 || lf.loop_filter_level[3] != 0 {
                    with_chroma_lf += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (sent.filter_level, sent.filter_level_u, sent.filter_level_v),
                        ([1, 7], 8, 12),
                        "frame 0's levels, and the pair whose loss the Vulkan rung's \
                         frame-0 divergence was reproduced from"
                    );
                }

                let lr = &plan.header.loop_restoration_params;
                let sizes = sent.log2_restoration_unit_size;
                if lr.uses_lr {
                    with_lr += 1;
                    assert_eq!(lr.loop_restoration_size, [128, 128, 128]);
                    assert_eq!(sizes, [7, 7, 7], "6 + lr_unit_shift, per plane");
                } else {
                    assert_eq!(
                        sizes, [LOG2_RESTORATION_UNIT_SIZE_UNUSED; 3],
                        "frame {frames}: restores nothing, so the size is \
                         libavcodec's 8 — never the parser's zero read as 16"
                    );
                }
                assert!(
                    sizes.iter().all(|s| (6..=8).contains(s)),
                    "frame {frames}: log2_restoration_unit_size is 6, 7 or 8"
                );
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            with_chroma_lf, 123,
            "123 of 274 frames of this vector deblock chroma; at zero the levels \
             above are all zero anyway and this test could not tell a dropped pair \
             from a carried one"
        );
        assert_eq!(
            with_lr, 3,
            "three frames use loop restoration, so both branches of the size are \
             exercised"
        );
    }

    /// The packed CDEF strength bytes carry the CODED secondary strength.
    ///
    /// `CdefStrength::pack` gives `secondary` TWO bits, and the vendored parser
    /// holds the AV1 spec's post-fixup value — 4 where the stream coded 3 (5.9.19
    /// rewrites the syntax element in place). `& 0x3` then turns the STRONGEST
    /// secondary filter into no filter at all, silently, on 68 of this vector's 274
    /// frames including the first. libavcodec's `dxva2_av1.c` assigns CBS's
    /// unmodified two-bit read into the same bitfield, which is the convention every
    /// driver was validated against.
    ///
    /// Asserted against the packed BYTE rather than the intermediate struct,
    /// because the truncation is what `pack` does and a test that stopped at the
    /// struct would not have seen it.
    #[test]
    fn cdef_secondary_strengths_survive_the_two_bit_pack() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut corrected_frames) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = plan_to_dxva_av1(&plan, &mut slots, frames).expect("converts");
                frames += 1;
                let raw = &plan.header.cdef_params;
                // `#[repr(packed)]` — copy the arrays out before indexing them.
                let cdef = dx.pic_params.cdef;
                let (y, uv) = (cdef.y_strengths, cdef.uv_strengths);
                let mut corrected = false;
                for i in 0..8 {
                    let want_y = coded_cdef_sec_strength(raw.cdef_y_sec_strength[i]);
                    let want_uv = coded_cdef_sec_strength(raw.cdef_uv_sec_strength[i]);
                    assert_eq!(
                        (y[i] >> 6, uv[i] >> 6),
                        (want_y, want_uv),
                        "frame {frames}: the secondary strength must survive the \
                         two-bit field — the parser's 4 packs to 0"
                    );
                    assert_eq!(
                        (y[i] & 0x3f, uv[i] & 0x3f),
                        (
                            raw.cdef_y_pri_strength[i] as u8,
                            raw.cdef_uv_pri_strength[i] as u8
                        ),
                        "the PRIMARY strengths are not fixed up by the spec and must \
                         reach the driver untouched"
                    );
                    if raw.cdef_y_sec_strength[i] == 4 || raw.cdef_uv_sec_strength[i] == 4 {
                        corrected = true;
                    }
                }
                if corrected {
                    corrected_frames += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (y[3] >> 6, uv[0] >> 6),
                        (3, 3),
                        "frame 0 codes the strongest secondary strength twice, and \
                         the uncorrected conversion packed both as 0"
                    );
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            corrected_frames, 68,
            "68 of 274 frames of this vector need the correction; at zero this test \
             compares an untouched conversion against itself"
        );
    }

    /// A key frame names no reference, and must say so with the unused sentinel
    /// rather than with surface 0.
    #[test]
    fn a_key_frame_names_no_reference() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plans = planner.plan_au(first).expect("the first unit plans");
        let plan = plans.first().expect("a frame");
        assert!(plan.picture.is_key, "the vector opens on a key frame");
        let dx = plan_to_dxva_av1(plan, &mut slots, 0).expect("converts");
        assert!(dx
            .pic_params
            .frame_refs
            .iter()
            .all(|e| e.index == UNUSED_INDEX));
    }
}
