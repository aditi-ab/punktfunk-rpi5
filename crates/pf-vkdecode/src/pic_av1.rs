//! One AV1 [`AuPlan`] into the Vulkan decode structures — the CPU half of M7's
//! Vulkan rung.
//!
//! AV1 puts almost the whole frame header in the PICTURE info rather than in session
//! parameters, so `StdVideoDecodeAV1PictureInfo` carries eight pointers to blocks
//! that are per-frame: tile info, quantisation, segmentation, loop filter, CDEF, loop
//! restoration, global motion and film grain. Each is owned here, boxed, beside the
//! Std struct that points at it — the ownership contract [`crate::OwnedStdSps`]
//! documents.
//!
//! # The reference numbering, which is a THIRD convention again
//!
//! `VkVideoDecodeAV1PictureInfoKHR::referenceNameSlotIndices` is indexed by AV1
//! REFERENCE NAME — `LAST_FRAME` through `ALTREF_FRAME`, seven of them, matching
//! `ref_frame_idx[0..7]` — and each entry holds the **DPB SLOT INDEX** that name
//! resolves to, or `-1` for a name this frame does not use.
//!
//! That is not the same as a position in `pReferenceSlots`, and it is the same class
//! of mistake that made HEVC unplayable on every driver in this program: there, the
//! RPS arrays were filled with positions where the spec wanted slots, and the two
//! coincide right up until they do not. Here the trap is narrower but identical in
//! shape, so the plan carries slot indices and says so, and the backend lays
//! `pReferenceSlots` out in [`DecodePlanVkAv1::refs`] order independently.

use ash::vk::native as hh;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::REFS_PER_FRAME;
use pf_bitstream::av1::{TilePlan, NUM_REF_SLOTS};

use crate::slots::SlotError;
use crate::slots::SlotMap;

/// `StdVideoAV1FrameType`.
const STD_FRAME_TYPE_KEY: hh::StdVideoAV1FrameType = 0;
const STD_FRAME_TYPE_INTER: hh::StdVideoAV1FrameType = 1;
const STD_FRAME_TYPE_INTRA_ONLY: hh::StdVideoAV1FrameType = 2;
const STD_FRAME_TYPE_SWITCH: hh::StdVideoAV1FrameType = 3;

/// `referenceNameSlotIndices` entry for a reference name this frame does not use.
pub const REFERENCE_NAME_UNUSED: i32 = -1;

/// `SUPERRES_DENOM_MIN` (AV1 spec) — `coded_denom` is the denominator less this.
const SUPERRES_DENOM_MIN: u32 = 9;

/// One active reference: its DPB slot, its Std reference info, and the planner id it
/// resolves — the same shape the H.264 and H.265 conversions carry.
#[derive(Debug, Clone)]
pub struct VkRefAv1 {
    pub slot: u8,
    pub std: hh::StdVideoDecodeAV1ReferenceInfo,
    pub id: PicId,
}

/// Everything CPU-derivable of one AV1 frame's decode submission.
#[derive(Debug)]
pub struct DecodePlanVkAv1 {
    /// The Std picture info and everything its eight pointers target.
    pub pic: OwnedStdAv1PictureInfo,
    /// Per reference NAME (`LAST_FRAME`..`ALTREF_FRAME`), the DPB SLOT it resolves
    /// to, or [`REFERENCE_NAME_UNUSED`] — see the module docs. Not positions in
    /// [`Self::refs`].
    pub reference_name_slot_indices: [i32; REFS_PER_FRAME],
    /// Each tile group's byte range in the access unit as planned. The recording
    /// layer packs these into the bitstream buffer and rebases, exactly as the
    /// H.264/H.265 slice offsets are rebased.
    pub tiles: Vec<TilePlan>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    pub setup_ref: hh::StdVideoDecodeAV1ReferenceInfo,
    pub setup_id: PicId,
    /// The unique referenced pictures of this frame, first appearance first. The
    /// backend lays `pReferenceSlots` out in THIS order.
    pub refs: Vec<VkRefAv1>,
}

/// The Std picture info plus the heap allocations its eight pointers target.
///
/// Ownership contract as [`crate::OwnedStdSps`]: boxed backing, movable wrapper, no
/// mutation, deliberately not `Clone`.
#[derive(Debug)]
pub struct OwnedStdAv1PictureInfo {
    std: hh::StdVideoDecodeAV1PictureInfo,
    _tile_info: Box<hh::StdVideoAV1TileInfo>,
    /// `StdVideoAV1TileInfo`'s own four arrays, behind ITS pointers — a second level
    /// of backing, and the reason this wrapper exists rather than a plain struct.
    _tile_arrays: TileArrays,
    _quantization: Box<hh::StdVideoAV1Quantization>,
    _segmentation: Box<hh::StdVideoAV1Segmentation>,
    _loop_filter: Box<hh::StdVideoAV1LoopFilter>,
    _cdef: Box<hh::StdVideoAV1CDEF>,
    _loop_restoration: Box<hh::StdVideoAV1LoopRestoration>,
    _global_motion: Box<hh::StdVideoAV1GlobalMotion>,
    /// Only present where the stream codes film grain; null otherwise, because a
    /// zeroed block behind a live pointer would ask the decoder to synthesise grain
    /// the stream never described.
    _film_grain: Option<Box<hh::StdVideoAV1FilmGrain>>,
}

impl OwnedStdAv1PictureInfo {
    /// The Std struct, valid for as long as `self` lives.
    pub fn std(&self) -> &hh::StdVideoDecodeAV1PictureInfo {
        &self.std
    }
}

/// The tile-info arrays, boxed so `StdVideoAV1TileInfo`'s pointers stay valid.
#[derive(Debug)]
struct TileArrays {
    _mi_col_starts: Box<[u16]>,
    _mi_row_starts: Box<[u16]>,
    _width_in_sbs_minus_1: Box<[u16]>,
    _height_in_sbs_minus_1: Box<[u16]>,
}

/// Why a plan cannot be expressed as Vulkan AV1 structures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkAv1Error {
    /// A `show_existing_frame` plan: it decodes nothing, so it has no submission.
    /// The backend displays the named picture instead of calling this.
    NoDecode,
    /// A reference the slot map does not hold.
    UnresolvedReference(PicId),
    /// More distinct references than the DPB can bind.
    TooManyReferences(usize),
    /// A field wider than its Std type.
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVkAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToVkAv1Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToVkAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkAv1Error::NoDecode => {
                write!(f, "a show_existing_frame plan has no decode submission")
            }
            PlanToVkAv1Error::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToVkAv1Error::TooManyReferences(n) => {
                write!(f, "{n} distinct references exceed the DPB")
            }
            PlanToVkAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its Std field")
            }
            PlanToVkAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVkAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToVkAv1Error> {
    u8::try_from(value).map_err(|_| PlanToVkAv1Error::FieldOverflow { field, value })
}

/// Convert one planned AV1 frame.
///
/// Nothing mutates `slots` until every fallible step has passed — the same
/// transaction discipline the other two conversions keep, for the same reason: a
/// half-applied DPB update is the shape of a corrupt reference.
pub fn plan_to_vk_av1(
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanVkAv1, PlanToVkAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToVkAv1Error::NoDecode)?;
    let header = &*plan.header;

    // --- resolve, before any mutation ------------------------------------
    // The unique references, first appearance first, plus the per-NAME slot table.
    let mut refs: Vec<VkRefAv1> = Vec::new();
    let mut reference_name_slot_indices = [REFERENCE_NAME_UNUSED; REFS_PER_FRAME];
    for (name, r) in plan.refs.iter().enumerate().take(REFS_PER_FRAME) {
        let slot = slots
            .slot_of(r.id)
            .ok_or(PlanToVkAv1Error::UnresolvedReference(r.id))?;
        reference_name_slot_indices[name] = i32::from(slot);
        if !refs.iter().any(|existing| existing.id == r.id) {
            refs.push(VkRefAv1 {
                slot,
                std: reference_info(r.order_hint, header.frame_type as u32)?,
                id: r.id,
            });
        }
    }
    if refs.len() > NUM_REF_SLOTS {
        return Err(PlanToVkAv1Error::TooManyReferences(refs.len()));
    }

    let pic = picture_info(plan)?;
    let setup_ref = reference_info(header.order_hint, header.frame_type as u32)?;

    // --- mutations, after every fallible step -----------------------------
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        let _ = slots.release(id);
    }
    let setup_slot = match slots.slot_of(setup_id) {
        // A frame may refresh a slot it already occupies; re-planning must not
        // double-assign.
        Some(existing) => existing,
        None => slots.assign(setup_id)?,
    };

    Ok(DecodePlanVkAv1 {
        pic,
        reference_name_slot_indices,
        tiles: plan.tiles.clone(),
        setup_slot,
        setup_ref,
        setup_id,
        refs,
    })
}

fn reference_info(
    order_hint: u32,
    frame_type: u32,
) -> Result<hh::StdVideoDecodeAV1ReferenceInfo, PlanToVkAv1Error> {
    // SAFETY: StdVideoDecodeAV1ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word, three small integers and a byte array; all-zero is valid for
    // every field.
    let mut std: hh::StdVideoDecodeAV1ReferenceInfo = unsafe { std::mem::zeroed() };
    std.frame_type = narrow("frame_type", frame_type)?;
    std.OrderHint = narrow("OrderHint", order_hint)?;
    Ok(std)
}

fn picture_info(plan: &AuPlan) -> Result<OwnedStdAv1PictureInfo, PlanToVkAv1Error> {
    let p = &*plan.header;

    // Tile info, and its four arrays.
    let tile = &p.tile_info;
    let mi_col_starts: Box<[u16]> = tile.mi_col_starts.iter().map(|v| *v as u16).collect();
    let mi_row_starts: Box<[u16]> = tile.mi_row_starts.iter().map(|v| *v as u16).collect();
    let width_in_sbs: Box<[u16]> = tile
        .width_in_sbs_minus_1
        .iter()
        .map(|v| *v as u16)
        .collect();
    let height_in_sbs: Box<[u16]> = tile
        .height_in_sbs_minus_1
        .iter()
        .map(|v| *v as u16)
        .collect();
    // SAFETY: plain-C bindgen structs throughout this function — a bitfield word,
    // integers, fixed arrays and const pointers. All-zero is valid for every field,
    // and every pointer is assigned before use.
    let mut tile_std: hh::StdVideoAV1TileInfo = unsafe { std::mem::zeroed() };
    tile_std
        .flags
        .set_uniform_tile_spacing_flag(tile.uniform_tile_spacing_flag.into());
    tile_std.TileCols = narrow("TileCols", tile.tile_cols)?;
    tile_std.TileRows = narrow("TileRows", tile.tile_rows)?;
    tile_std.context_update_tile_id = tile.context_update_tile_id as u16;
    tile_std.tile_size_bytes_minus_1 = narrow(
        "tile_size_bytes_minus_1",
        tile.tile_size_bytes.saturating_sub(1),
    )?;
    tile_std.pMiColStarts = mi_col_starts.as_ptr();
    tile_std.pMiRowStarts = mi_row_starts.as_ptr();
    tile_std.pWidthInSbsMinus1 = width_in_sbs.as_ptr();
    tile_std.pHeightInSbsMinus1 = height_in_sbs.as_ptr();
    let tile_info = Box::new(tile_std);
    let tile_arrays = TileArrays {
        _mi_col_starts: mi_col_starts,
        _mi_row_starts: mi_row_starts,
        _width_in_sbs_minus_1: width_in_sbs,
        _height_in_sbs_minus_1: height_in_sbs,
    };

    // Quantisation.
    let q = &p.quantization_params;
    // SAFETY: see above.
    let mut q_std: hh::StdVideoAV1Quantization = unsafe { std::mem::zeroed() };
    q_std.flags.set_using_qmatrix(q.using_qmatrix.into());
    q_std.flags.set_diff_uv_delta(q.diff_uv_delta.into());
    q_std.base_q_idx = narrow("base_q_idx", q.base_q_idx)?;
    q_std.DeltaQYDc = q.delta_q_y_dc as i8;
    q_std.DeltaQUDc = q.delta_q_u_dc as i8;
    q_std.DeltaQUAc = q.delta_q_u_ac as i8;
    q_std.DeltaQVDc = q.delta_q_v_dc as i8;
    q_std.DeltaQVAc = q.delta_q_v_ac as i8;
    q_std.qm_y = narrow("qm_y", q.qm_y)?;
    q_std.qm_u = narrow("qm_u", q.qm_u)?;
    q_std.qm_v = narrow("qm_v", q.qm_v)?;
    let quantization = Box::new(q_std);

    // Segmentation: an 8x8 enable matrix and its data.
    let s = &p.segmentation_params;
    // SAFETY: see above.
    let mut s_std: hh::StdVideoAV1Segmentation = unsafe { std::mem::zeroed() };
    for (seg, enabled) in s.feature_enabled.iter().enumerate() {
        let mut bits = 0u8;
        for (feature, on) in enabled.iter().enumerate() {
            if *on {
                bits |= 1 << feature;
            }
        }
        s_std.FeatureEnabled[seg] = bits;
        s_std.FeatureData[seg] = s.feature_data[seg];
    }
    let segmentation = Box::new(s_std);

    // Loop filter.
    let lf = &p.loop_filter_params;
    // SAFETY: see above.
    let mut lf_std: hh::StdVideoAV1LoopFilter = unsafe { std::mem::zeroed() };
    lf_std
        .flags
        .set_loop_filter_delta_enabled(lf.loop_filter_delta_enabled.into());
    lf_std
        .flags
        .set_loop_filter_delta_update(lf.loop_filter_delta_update.into());
    lf_std.loop_filter_level = lf.loop_filter_level;
    lf_std.loop_filter_sharpness = lf.loop_filter_sharpness;
    lf_std.loop_filter_ref_deltas = lf.loop_filter_ref_deltas;
    lf_std.loop_filter_mode_deltas = lf.loop_filter_mode_deltas;
    let loop_filter = Box::new(lf_std);

    // CDEF.
    let c = &p.cdef_params;
    // SAFETY: see above.
    let mut c_std: hh::StdVideoAV1CDEF = unsafe { std::mem::zeroed() };
    c_std.cdef_damping_minus_3 = narrow("cdef_damping_minus_3", c.cdef_damping.saturating_sub(3))?;
    c_std.cdef_bits = narrow("cdef_bits", c.cdef_bits)?;
    for i in 0..8 {
        c_std.cdef_y_pri_strength[i] = c.cdef_y_pri_strength[i] as u8;
        c_std.cdef_y_sec_strength[i] = c.cdef_y_sec_strength[i] as u8;
        c_std.cdef_uv_pri_strength[i] = c.cdef_uv_pri_strength[i] as u8;
        c_std.cdef_uv_sec_strength[i] = c.cdef_uv_sec_strength[i] as u8;
    }
    let cdef = Box::new(c_std);

    // Loop restoration.
    let lr = &p.loop_restoration_params;
    // SAFETY: see above.
    let mut lr_std: hh::StdVideoAV1LoopRestoration = unsafe { std::mem::zeroed() };
    for i in 0..3 {
        lr_std.FrameRestorationType[i] = lr.frame_restoration_type[i] as u32;
        lr_std.LoopRestorationSize[i] = lr.loop_restoration_size[i];
    }
    let loop_restoration = Box::new(lr_std);

    // Global motion.
    let gm = &p.global_motion_params;
    // SAFETY: see above.
    let mut gm_std: hh::StdVideoAV1GlobalMotion = unsafe { std::mem::zeroed() };
    for i in 0..NUM_REF_SLOTS {
        gm_std.GmType[i] = gm.gm_type[i] as u8;
        gm_std.gm_params[i] = gm.gm_params[i];
    }
    let global_motion = Box::new(gm_std);

    // Film grain: only where the SEQUENCE enables it and this frame applies it.
    // The gate is deliberately both — a zeroed block behind a live pointer would ask
    // the decoder to synthesise grain the stream never described.
    let film_grain = if plan.sequence.film_grain_params_present && p.film_grain_params.apply_grain {
        let fg = &p.film_grain_params;
        // SAFETY: see above.
        let mut fg_std: hh::StdVideoAV1FilmGrain = unsafe { std::mem::zeroed() };
        fg_std
            .flags
            .set_chroma_scaling_from_luma(fg.chroma_scaling_from_luma.into());
        fg_std.flags.set_overlap_flag(fg.overlap_flag.into());
        fg_std
            .flags
            .set_clip_to_restricted_range(fg.clip_to_restricted_range.into());
        fg_std.flags.set_update_grain(fg.update_grain.into());
        fg_std.grain_scaling_minus_8 = fg.grain_scaling_minus_8;
        fg_std.ar_coeff_lag = narrow("ar_coeff_lag", fg.ar_coeff_lag)?;
        fg_std.ar_coeff_shift_minus_6 = fg.ar_coeff_shift_minus_6;
        fg_std.grain_scale_shift = fg.grain_scale_shift;
        fg_std.grain_seed = fg.grain_seed;
        fg_std.film_grain_params_ref_idx = fg.film_grain_params_ref_idx;

        // ⚠ The PARSER's point arrays are 16 entries; the Std ones are 14 (luma) and
        // 10 (chroma), which are the spec's own maxima. So the counts are checked
        // against the STD capacity and the copy is bounded by them — a blind
        // array-to-array assignment does not compile here, and a blind
        // `copy_from_slice` of 16 into 14 would panic at runtime on a malformed
        // stream. Refused rather than truncated: a decoder given fewer scaling
        // points than the stream declared synthesises different grain.
        let points =
            |name: &'static str, count: u8, cap: usize| -> Result<usize, PlanToVkAv1Error> {
                if usize::from(count) > cap {
                    return Err(PlanToVkAv1Error::FieldOverflow {
                        field: name,
                        value: u32::from(count),
                    });
                }
                Ok(usize::from(count))
            };
        let ny = points("num_y_points", fg.num_y_points, fg_std.point_y_value.len())?;
        let ncb = points(
            "num_cb_points",
            fg.num_cb_points,
            fg_std.point_cb_value.len(),
        )?;
        let ncr = points(
            "num_cr_points",
            fg.num_cr_points,
            fg_std.point_cr_value.len(),
        )?;
        fg_std.num_y_points = fg.num_y_points;
        fg_std.num_cb_points = fg.num_cb_points;
        fg_std.num_cr_points = fg.num_cr_points;
        fg_std.point_y_value[..ny].copy_from_slice(&fg.point_y_value[..ny]);
        fg_std.point_y_scaling[..ny].copy_from_slice(&fg.point_y_scaling[..ny]);
        fg_std.point_cb_value[..ncb].copy_from_slice(&fg.point_cb_value[..ncb]);
        fg_std.point_cb_scaling[..ncb].copy_from_slice(&fg.point_cb_scaling[..ncb]);
        fg_std.point_cr_value[..ncr].copy_from_slice(&fg.point_cr_value[..ncr]);
        fg_std.point_cr_scaling[..ncr].copy_from_slice(&fg.point_cr_scaling[..ncr]);
        for (dst, src) in fg_std
            .ar_coeffs_y_plus_128
            .iter_mut()
            .zip(fg.ar_coeffs_y_plus_128.iter())
        {
            *dst = *src as i8;
        }
        for (dst, src) in fg_std
            .ar_coeffs_cb_plus_128
            .iter_mut()
            .zip(fg.ar_coeffs_cb_plus_128.iter())
        {
            *dst = *src as i8;
        }
        for (dst, src) in fg_std
            .ar_coeffs_cr_plus_128
            .iter_mut()
            .zip(fg.ar_coeffs_cr_plus_128.iter())
        {
            *dst = *src as i8;
        }
        Some(Box::new(fg_std))
    } else {
        None
    };

    // The picture info itself.
    // SAFETY: see above.
    let mut std: hh::StdVideoDecodeAV1PictureInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_error_resilient_mode(p.error_resilient_mode.into());
    std.flags
        .set_disable_cdf_update(p.disable_cdf_update.into());
    std.flags.set_use_superres(p.use_superres.into());
    std.flags.set_allow_intrabc(p.allow_intrabc.into());
    std.flags
        .set_allow_high_precision_mv(p.allow_high_precision_mv.into());
    std.flags
        .set_is_motion_mode_switchable(p.is_motion_mode_switchable.into());
    std.flags.set_use_ref_frame_mvs(p.use_ref_frame_mvs.into());
    std.flags
        .set_disable_frame_end_update_cdf(p.disable_frame_end_update_cdf.into());
    std.flags.set_reduced_tx_set(p.reduced_tx_set.into());
    std.flags.set_reference_select(p.reference_select.into());
    std.flags.set_skip_mode_present(p.skip_mode_present.into());
    std.flags
        .set_delta_q_present(p.quantization_params.delta_q_present.into());
    std.flags
        .set_delta_lf_present(p.loop_filter_params.delta_lf_present.into());
    std.flags
        .set_delta_lf_multi(p.loop_filter_params.delta_lf_multi.into());
    std.flags
        .set_segmentation_enabled(p.segmentation_params.segmentation_enabled.into());
    std.flags
        .set_segmentation_update_map(p.segmentation_params.segmentation_update_map.into());
    std.flags.set_segmentation_temporal_update(
        p.segmentation_params.segmentation_temporal_update.into(),
    );
    std.flags
        .set_segmentation_update_data(p.segmentation_params.segmentation_update_data.into());
    // `UsesLr` is derived, not coded: loop restoration is in use when any plane's
    // restoration type is something other than NONE (0).
    std.flags.set_UsesLr(u32::from(
        lr.frame_restoration_type.iter().any(|t| *t as u32 != 0),
    ));
    // Kept in step with the `pFilmGrain` gate above by construction: the flag says
    // grain is applied exactly when a block describing it is attached.
    std.flags.set_apply_grain(u32::from(film_grain.is_some()));

    std.frame_type = match p.frame_type {
        pf_bitstream::av1::FrameType::KeyFrame => STD_FRAME_TYPE_KEY,
        pf_bitstream::av1::FrameType::InterFrame => STD_FRAME_TYPE_INTER,
        pf_bitstream::av1::FrameType::IntraOnlyFrame => STD_FRAME_TYPE_INTRA_ONLY,
        pf_bitstream::av1::FrameType::SwitchFrame => STD_FRAME_TYPE_SWITCH,
    };
    std.current_frame_id = p.current_frame_id;
    std.OrderHint = narrow("OrderHint", p.order_hint)?;
    std.primary_ref_frame = narrow("primary_ref_frame", p.primary_ref_frame)?;
    std.refresh_frame_flags = narrow("refresh_frame_flags", p.refresh_frame_flags)?;
    std.interpolation_filter = p.interpolation_filter as u32;
    std.TxMode = p.tx_mode as u32;
    std.delta_q_res = narrow("delta_q_res", p.quantization_params.delta_q_res)?;
    std.delta_lf_res = p.loop_filter_params.delta_lf_res;
    std.SkipModeFrame = [
        narrow("SkipModeFrame[0]", p.skip_mode_frame[0])?,
        narrow("SkipModeFrame[1]", p.skip_mode_frame[1])?,
    ];
    // `coded_denom` is the superres denominator as CODED — the spec writes it
    // `SUPERRES_DENOM_MIN` less than the real one, and it is only meaningful where
    // superres is actually in use.
    std.coded_denom = if p.use_superres {
        narrow(
            "coded_denom",
            p.superres_denom.saturating_sub(SUPERRES_DENOM_MIN),
        )?
    } else {
        0
    };
    for (i, hint) in p.order_hints.iter().enumerate().take(NUM_REF_SLOTS) {
        std.OrderHints[i] = *hint as u8;
    }
    std.pTileInfo = &*tile_info;
    std.pQuantization = &*quantization;
    std.pSegmentation = &*segmentation;
    std.pLoopFilter = &*loop_filter;
    std.pCDEF = &*cdef;
    std.pLoopRestoration = &*loop_restoration;
    std.pGlobalMotion = &*global_motion;
    std.pFilmGrain = film_grain
        .as_ref()
        .map_or(std::ptr::null(), |g| &**g as *const _);

    Ok(OwnedStdAv1PictureInfo {
        std,
        _tile_info: tile_info,
        _tile_arrays: tile_arrays,
        _quantization: quantization,
        _segmentation: segmentation,
        _loop_filter: loop_filter,
        _cdef: cdef,
        _loop_restoration: loop_restoration,
        _global_motion: global_motion,
        _film_grain: film_grain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;

    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// Convert every frame of the vendored vector and check the parts a driver
    /// reads against each other.
    ///
    /// The load-bearing assertion is the last one. `referenceNameSlotIndices` holds
    /// DPB SLOT indices, not positions in `refs`, and the two coincide for as long
    /// as references happen to land in slots `0..refs.len()` in `refs` order — which
    /// on a freshly keyed stream they do. That is exactly how the HEVC RPS defect
    /// shipped: correct for the first few access units, silently wrong afterwards.
    /// So this measures how often the two numberings actually DISAGREE on a real
    /// stream, and fails if the answer is never — because then the test is proving
    /// nothing and the distinction would be free to rot.
    #[test]
    fn every_frame_converts_and_slot_indices_are_not_positions() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_refs) = (0u32, 0u32);
        let mut disagreements = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue; // show_existing_frame: no submission
                }
                let vk = plan_to_vk_av1(&plan, &mut slots).expect("the clean vector converts");
                frames += 1;

                // Every named slot must be one the decode op will bind.
                for name in vk.reference_name_slot_indices {
                    if name == REFERENCE_NAME_UNUSED {
                        continue;
                    }
                    let slot = u8::try_from(name).expect("a slot index is small and positive");
                    assert!(
                        vk.refs.iter().any(|r| r.slot == slot),
                        "reference name resolves to slot {slot}, which this frame's \
                         reference list does not bind"
                    );
                }
                if !vk.refs.is_empty() {
                    with_refs += 1;
                    // Would reading these as POSITIONS have given the same answer?
                    for (name, entry) in vk.reference_name_slot_indices.iter().enumerate() {
                        if *entry == REFERENCE_NAME_UNUSED {
                            continue;
                        }
                        let as_position = vk.refs.get(name).map(|r| i32::from(r.slot));
                        if as_position != Some(*entry) {
                            disagreements += 1;
                        }
                    }
                }
                // The setup picture must hold the slot the plan says it does.
                assert_eq!(slots.slot_of(vk.setup_id), Some(vk.setup_slot));
            }
        }

        assert_eq!(frames, 274, "every frame of the vector must convert");
        assert!(with_refs > 0, "a 274-frame vector must reference something");
        assert!(
            disagreements > 0,
            "slot indices and reference-list positions never disagreed on this \
             vector, so this test cannot tell the two conventions apart — the same \
             blind spot that let the HEVC RPS defect ship"
        );
        eprintln!(
            "frames {frames} · with refs {with_refs} · slot-vs-position disagreements \
             {disagreements}"
        );
    }
}
