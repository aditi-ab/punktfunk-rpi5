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
//!
//! The NAME itself comes from the planner, not from counting: `AuPlan::refs` is
//! indexed by reference name and a lost reference leaves a hole there, so the loop
//! below reads its index off the iterator and skips the holes. Compacting the list
//! first — which is what it used to receive — renamed every reference after the
//! first loss.

use ash::vk::native as hh;
use pf_bitstream::av1::coded_cdef_sec_strength;
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
    /// Each tile group's byte range in the access unit as planned — whole OBUs.
    ///
    /// ⚠ NOT what is uploaded. The bitstream buffer holds the raw tile PAYLOADS
    /// found inside these OBUs and nothing else, and the recording layer walks
    /// them itself (`decoder_av1`'s `plan_bitstream`) because that walk needs the
    /// access-unit bytes, which a conversion never sees. Carried here so a caller
    /// can see what the frame was made of without re-parsing.
    pub tiles: Vec<TilePlan>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    pub setup_ref: hh::StdVideoDecodeAV1ReferenceInfo,
    pub setup_id: PicId,
    /// The unique referenced pictures of this frame, first appearance first. The
    /// backend lays `pReferenceSlots` out in THIS order.
    pub refs: Vec<VkRefAv1>,
    /// Pictures this frame's own `refresh_frame_flags` displaces from the store
    /// while THIS frame still reads them — their slots are released only once the
    /// decode op has been recorded, and the caller owes exactly that.
    ///
    /// AV1 applies `refresh_frame_flags` AFTER the frame is decoded (7.20), so a
    /// frame reading a slot and overwriting it is ordinary rather than exotic:
    /// `ref_frame_idx` resolves against the pre-decode store and the refresh lands
    /// behind it. The picture is therefore a LIVE reference for exactly this decode
    /// op and its DPB slot may not be recycled until the op is submitted. Releasing
    /// it inside this conversion — which is what the H.264 and H.265 siblings do
    /// with their whole `removed` list — hands its slot straight back to
    /// [`Self::setup_slot`], because the lowest free slot is the one just vacated:
    /// [`Self::refs`] then names the very slot the decode target activates, and the
    /// decoder's binding sync clears its image on the way past. Measured on the
    /// vendored vector at frame 6 of 274 (`slot_recycling_waits_for_the_decode_op`).
    ///
    /// Empty for the overwhelming majority of frames; the ids are always a subset
    /// of the plan's `dpb.removed`, so applying them completes that plan's
    /// bookkeeping and never invents a removal.
    pub release_after_decode: Vec<PicId>,
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

/// The parser's frame type as `StdVideoAV1FrameType`.
///
/// Written out rather than cast even though the four discriminants happen to
/// coincide: the coincidence is between a vendored crate's enum and a Vulkan
/// header, and neither is ours to keep in step. Both the picture info and every
/// reference info go through here, so the two can never disagree either.
fn std_frame_type(frame_type: pf_bitstream::av1::FrameType) -> hh::StdVideoAV1FrameType {
    match frame_type {
        pf_bitstream::av1::FrameType::KeyFrame => STD_FRAME_TYPE_KEY,
        pf_bitstream::av1::FrameType::InterFrame => STD_FRAME_TYPE_INTER,
        pf_bitstream::av1::FrameType::IntraOnlyFrame => STD_FRAME_TYPE_INTRA_ONLY,
        pf_bitstream::av1::FrameType::SwitchFrame => STD_FRAME_TYPE_SWITCH,
    }
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
    // `plan.refs` is indexed BY NAME and holes are real (a lost reference), so the
    // index is taken from the iterator and empty names are skipped rather than
    // shifting everything after them up one.
    let mut refs: Vec<VkRefAv1> = Vec::new();
    let mut reference_name_slot_indices = [REFERENCE_NAME_UNUSED; REFS_PER_FRAME];
    for (name, r) in plan.refs.iter().enumerate() {
        let Some(r) = r else { continue };
        let slot = slots
            .slot_of(r.id)
            .ok_or(PlanToVkAv1Error::UnresolvedReference(r.id))?;
        reference_name_slot_indices[name] = i32::from(slot);
        if !refs.iter().any(|existing| existing.id == r.id) {
            refs.push(VkRefAv1 {
                slot,
                // The REFERENCE's own state, never this frame's — see
                // `pf_bitstream::av1::RefState`.
                std: reference_info(&r.state)?,
                id: r.id,
            });
        }
    }
    if refs.len() > NUM_REF_SLOTS {
        return Err(PlanToVkAv1Error::TooManyReferences(refs.len()));
    }

    let pic = picture_info(header, &plan.sequence)?;
    // The picture being decoded activates a slot, so it needs the same answers a
    // reference does — and it is cached as that slot's reference info for later
    // frames (`decoder_av1`'s `slot_refs`), so it is built through the SAME
    // function the reference path uses. libavcodec leaves `SavedOrderHints` zero
    // here because it rebuilds a reference's info from scratch every frame and
    // never re-reads the setup entry; this rung caches, so filling them keeps the
    // cached copy equal to the one the reference path would build.
    let setup_ref = reference_info(&pf_bitstream::av1::RefState::of(header))?;

    // --- mutations, after every fallible step -----------------------------
    // A picture this frame READS may be displaced by this same frame's refresh —
    // see `DecodePlanVkAv1::release_after_decode` for why that is ordinary AV1 and
    // what releasing it here would cost. Its slot survives the assignment below and
    // is handed to the caller to release once the decode op is recorded.
    let release_after_decode: Vec<PicId> = plan
        .dpb
        .removed
        .iter()
        .copied()
        .filter(|id| *id != setup_id && refs.iter().any(|r| r.id == *id))
        .collect();
    for &id in &plan.dpb.removed {
        if id == setup_id || release_after_decode.contains(&id) {
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
        release_after_decode,
    })
}

/// One picture's `StdVideoDecodeAV1ReferenceInfo`, from THAT picture's own header
/// state.
///
/// Every field here is about the reference, and answering any of them from the
/// frame currently being decoded is a silent mispredict rather than an error. The
/// set matches libavcodec's `vulkan_av1.c` field for field (`vk_av1_fill_pict`);
/// `RefFrameSignBias` and `SavedOrderHints` are the two RADV reads
/// (`radv_video.c`, `av1->ref_frames[i].ref_frame_sign_bias`).
fn reference_info(
    state: &pf_bitstream::av1::RefState,
) -> Result<hh::StdVideoDecodeAV1ReferenceInfo, PlanToVkAv1Error> {
    // SAFETY: StdVideoDecodeAV1ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word, three small integers and a byte array; all-zero is valid for
    // every field.
    let mut std: hh::StdVideoDecodeAV1ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_disable_frame_end_update_cdf(state.disable_frame_end_update_cdf.into());
    std.flags
        .set_segmentation_enabled(state.segmentation_enabled.into());
    std.frame_type = narrow("frame_type", std_frame_type(state.frame_type))?;
    std.RefFrameSignBias = state.ref_frame_sign_bias;
    std.OrderHint = narrow("OrderHint", state.order_hint)?;
    for (dst, hint) in std
        .SavedOrderHints
        .iter_mut()
        .zip(state.saved_order_hints.iter())
    {
        // Order hints are `order_hint_bits` wide and that is at most 8, so the
        // truncation is unreachable — and it is the same cast `OrderHints` in the
        // picture info takes, kept identical on purpose.
        *dst = *hint as u8;
    }
    Ok(std)
}

/// One frame header (plus the sequence header, for the film-grain gate) into the
/// Std picture info and everything its eight pointers target.
///
/// Takes the two headers rather than the whole [`AuPlan`] so a hand-built header —
/// film grain, say, which no vendored vector codes — can be converted in a unit
/// test without inventing a plan around it.
fn picture_info(
    p: &pf_bitstream::av1::ParsedFrameHeader,
    sequence: &pf_bitstream::av1::ParsedSequenceHeader,
) -> Result<OwnedStdAv1PictureInfo, PlanToVkAv1Error> {
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
    //
    // ⚠ The SECONDARY strengths are the CODED two-bit values, and the parser does
    // not hold them: AV1 5.9.19 mutates the syntax element in place (`== 3` becomes
    // 4) and cros-codecs follows the spec, while libavcodec sends CBS's unmodified
    // two-bit read and every driver was validated against that. Sending 4 overflows
    // the two bits VA-API, NVDEC and DXVA all give the field, so the strongest
    // secondary filter reads back as NO filter. `coded_cdef_sec_strength` is the
    // inverse, and its docs carry the four-API evidence.
    let c = &p.cdef_params;
    // SAFETY: see above.
    let mut c_std: hh::StdVideoAV1CDEF = unsafe { std::mem::zeroed() };
    c_std.cdef_damping_minus_3 = narrow("cdef_damping_minus_3", c.cdef_damping.saturating_sub(3))?;
    c_std.cdef_bits = narrow("cdef_bits", c.cdef_bits)?;
    for i in 0..8 {
        c_std.cdef_y_pri_strength[i] = c.cdef_y_pri_strength[i] as u8;
        c_std.cdef_y_sec_strength[i] = coded_cdef_sec_strength(c.cdef_y_sec_strength[i]);
        c_std.cdef_uv_pri_strength[i] = c.cdef_uv_pri_strength[i] as u8;
        c_std.cdef_uv_sec_strength[i] = coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]);
    }
    let cdef = Box::new(c_std);

    // Loop restoration.
    //
    // ⚠ `LoopRestorationSize` is NOT the size in pixels. The Vulkan field carries
    // the CODED value — RADV names its destination `log2_restoration_size_minus5`
    // (`radv_video.c`) and libavcodec sends `1 + lr_unit_shift` (luma) and
    // `1 + lr_unit_shift - lr_uv_shift` (chroma) — while the vendored parser
    // records the pixel size, 64/128/256. Sending 64 where a driver expects 1 asks
    // for a restoration unit of 2^69 pixels.
    let lr = &p.loop_restoration_params;
    // SAFETY: see above.
    let mut lr_std: hh::StdVideoAV1LoopRestoration = unsafe { std::mem::zeroed() };
    let luma_size = 1 + u16::from(lr.lr_unit_shift);
    // `lr_uv_shift` is one coded bit (0 or 1) and `luma_size` is at least 1, so the
    // saturation is unreachable; it is here so a malformed parse cannot wrap to
    // 65535, which a driver would read as log2(size) − 5.
    let chroma_size = luma_size.saturating_sub(u16::from(lr.lr_uv_shift));
    for i in 0..3 {
        lr_std.FrameRestorationType[i] = lr.frame_restoration_type[i] as u32;
        lr_std.LoopRestorationSize[i] = if i == 0 { luma_size } else { chroma_size };
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
    let film_grain = if sequence.film_grain_params_present && p.film_grain_params.apply_grain {
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
        // The chroma scaling function's six coefficients (7.18.3.5 `scaling_lut`
        // for the chroma planes). Nothing else describes how luma feeds chroma
        // grain, so leaving them zero synthesises grey-drifting chroma noise on
        // any stream that codes grain — libavcodec sets all six, and so does this
        // program's DXVA conversion.
        fg_std.cb_mult = fg.cb_mult;
        fg_std.cb_luma_mult = fg.cb_luma_mult;
        fg_std.cb_offset = fg.cb_offset;
        fg_std.cr_mult = fg.cr_mult;
        fg_std.cr_luma_mult = fg.cr_luma_mult;
        fg_std.cr_offset = fg.cr_offset;

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
    // The four that CHANGE RECONSTRUCTION and were missing until the M7 review.
    // Measured incidence on the vendored 274-frame vector:
    //
    // * `allow_screen_content_tools` — 274/274 frames, and RADV reads it
    //   (`av1->pic_flags.allow_screen_content_tools`). It also has to be set for
    //   `allow_intrabc` below to be coherent: intra block copy is only codeable
    //   when screen-content tools are on, so the two disagreeing is a contradiction
    //   a driver is free to resolve either way;
    // * `allow_warped_motion` — 273/274;
    // * `is_filter_switchable` — 172/274;
    // * `force_integer_mv` — 1/274 (the key frame: the parser applies the spec's
    //   `frame_is_intra ⇒ 1` rule, as libavcodec does for `cur_frame`).
    std.flags
        .set_allow_screen_content_tools(u32::from(p.allow_screen_content_tools != 0));
    std.flags
        .set_allow_warped_motion(p.allow_warped_motion.into());
    std.flags
        .set_is_filter_switchable(p.is_filter_switchable.into());
    std.flags
        .set_force_integer_mv(u32::from(p.force_integer_mv != 0));
    // The four informational ones libavcodec also sends. No driver in this fleet is
    // known to act on them, but they are coded facts about the frame and a decoder
    // is entitled to check them against its own parse.
    std.flags
        .set_render_and_frame_size_different(p.render_and_frame_size_different.into());
    std.flags
        .set_frame_size_override_flag(p.frame_size_override_flag.into());
    std.flags
        .set_buffer_removal_time_present_flag(p.buffer_removal_time_present_flag.into());
    std.flags
        .set_frame_refs_short_signaling(p.frame_refs_short_signaling.into());
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
    // `usesChromaLr` is deliberately LEFT ZERO, and this is not an oversight.
    //
    // The AV1 spec's `UsesChromaLr` is `FrameRestorationType[1] != NONE ||
    // FrameRestorationType[2] != NONE` — the vendored parser even computes it
    // (`LoopRestorationParams::uses_chroma_lr`). libavcodec's `vulkan_av1.c` sets
    // neither, and libavcodec is the implementation every driver in this fleet was
    // validated against: a driver that reads the field at all reads it as zero
    // today, and sending a truthful 1 would be the FIRST implementation to do so.
    // That is not a bet to take blind on a rung with no on-glass mileage. Revisit
    // with a driver-by-driver measurement, not by "fixing" it.
    // Kept in step with the `pFilmGrain` gate above by construction: the flag says
    // grain is applied exactly when a block describing it is attached.
    std.flags.set_apply_grain(u32::from(film_grain.is_some()));

    std.frame_type = std_frame_type(p.frame_type);
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

    /// Convert one plan the way a BACKEND must: the conversion, then the releases
    /// it defers past the decode op ([`DecodePlanVkAv1::release_after_decode`]).
    ///
    /// Not a convenience — it is the caller's half of the contract. A loop that
    /// converts without it leaks a slot on nearly every frame of this vector (268
    /// of 274) and runs the nine-slot ledger dry inside ten frames, so any test
    /// walking the vector has to speak it.
    fn convert(plan: &AuPlan, slots: &mut SlotMap) -> DecodePlanVkAv1 {
        let vk = plan_to_vk_av1(plan, slots).expect("the clean vector converts");
        for &id in &vk.release_after_decode {
            assert!(
                slots.release(id),
                "a deferred release named picture {id}, which holds no slot"
            );
        }
        vk
    }

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
                let vk = convert(&plan, &mut slots);
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
                // No reference may share the decode target's slot — the assertion
                // the H.264 and H.265 conversion tests have carried since M2, and
                // the one this file was missing. A frame whose own refresh
                // displaces a picture it READS had its slot recycled straight into
                // `setup_slot`, so `refs` named the slot the decode target
                // activates: the hardware would predict from the picture it is in
                // the middle of writing. Frame 6 of this vector does it.
                for r in &vk.refs {
                    assert_ne!(
                        r.slot, vk.setup_slot,
                        "frame {frames}: reference (picture {}) aliases the setup slot",
                        r.id
                    );
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

    /// A frame that READS a slot its own refresh overwrites keeps that slot until
    /// the decode op is recorded — and the incidence is pinned, because it is the
    /// ordinary case rather than the exotic one.
    ///
    /// AV1 applies `refresh_frame_flags` after decoding (7.20), so `ref_frame_idx`
    /// resolves against the store as it stood BEFORE the frame. Cycling eight slots
    /// in a low-delay stream therefore means almost every frame displaces something
    /// it is reading: **268 of this vector's 274 frames**, first at frame 6. The
    /// H.264 and H.265 planners can produce the same shape — `plan_to_vk`'s own
    /// docs name the sliding window evicting a picture the slices reference — but
    /// neither vendored vector ever does it (measured: zero on the 250-AU H.264
    /// clip), which is why the hole survived two hardware-proven codecs and opened
    /// on the first AV1 frame that was not a key frame's neighbour.
    #[test]
    fn a_reference_this_frame_displaces_keeps_its_slot_until_after_the_decode() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut deferring, mut deferred_ids) = (0u32, 0u32, 0u32);
        let mut peak_active = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = plan_to_vk_av1(&plan, &mut slots).expect("converts");
                frames += 1;
                peak_active = peak_active.max(slots.active());

                // Every deferred id is one this plan really removed AND this frame
                // really reads — never an invented removal, never a live picture.
                for &id in &vk.release_after_decode {
                    assert!(
                        plan.dpb.removed.contains(&id),
                        "frame {frames}: deferred picture {id} is not in this plan's \
                         removed list"
                    );
                    assert!(
                        vk.refs.iter().any(|r| r.id == id),
                        "frame {frames}: picture {id} is deferred without being read — \
                         only a reference of THIS frame earns the reprieve"
                    );
                    assert!(
                        slots.slot_of(id).is_some(),
                        "frame {frames}: a deferred picture must still hold its slot"
                    );
                }
                // And the whole point: the slot it still holds is not the one the
                // decode target just took.
                for r in &vk.refs {
                    assert_ne!(r.slot, vk.setup_slot);
                }
                if !vk.release_after_decode.is_empty() {
                    deferring += 1;
                    deferred_ids += vk.release_after_decode.len() as u32;
                }
                for &id in &vk.release_after_decode {
                    assert!(slots.release(id));
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            deferring, 268,
            "268 of 274 frames of this vector displace a picture they are reading; \
             at zero this test compares an empty list against itself and the \
             deferral could be deleted without a single assertion noticing"
        );
        assert_eq!(deferred_ids, 268, "one displaced reference per frame here");
        assert!(
            peak_active <= slots.capacity(),
            "deferring a release must not overrun the ledger"
        );
        // The nine-slot ledger is `NUM_REF_SLOTS + 1` and holding a displaced
        // reference one frame longer is exactly what that spare slot is for. If
        // this ever reaches capacity the sizing argument needs re-reading, not a
        // bigger number.
        eprintln!("frames {frames} · deferring {deferring} · peak slots held {peak_active}");
    }

    /// Every `StdVideoDecodeAV1PictureInfoFlags` bit this conversion is responsible
    /// for, checked against the parsed header on all 274 frames — with the
    /// INCIDENCE of each pinned, so a bit that silently stopped being written
    /// fails here.
    ///
    /// Nine of these were unset when M7 first landed, and four of them change
    /// reconstruction. A test that only asserted "flag == header field" would have
    /// passed just as happily against a conversion that wrote neither, which is why
    /// the counts below are assertions and not `eprintln!`s.
    #[test]
    fn every_picture_info_flag_matches_the_header_and_the_incidence_is_pinned() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;
        let (mut screen, mut warped, mut switchable, mut integer_mv) = (0u32, 0u32, 0u32, 0u32);
        let (mut informational, mut intrabc) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                let f = &vk.pic.std().flags;
                let p = &*plan.header;
                frames += 1;

                let bit = |b: bool| u32::from(b);
                // --- the four that change reconstruction ---
                assert_eq!(
                    f.allow_screen_content_tools(),
                    bit(p.allow_screen_content_tools != 0)
                );
                assert_eq!(f.allow_warped_motion(), bit(p.allow_warped_motion));
                assert_eq!(f.is_filter_switchable(), bit(p.is_filter_switchable));
                assert_eq!(f.force_integer_mv(), bit(p.force_integer_mv != 0));
                // Intra block copy is only codeable where screen-content tools are
                // on, so a frame claiming intrabc without them is a contradiction a
                // driver resolves however it likes.
                if f.allow_intrabc() == 1 {
                    assert_eq!(
                        f.allow_screen_content_tools(),
                        1,
                        "allow_intrabc without allow_screen_content_tools"
                    );
                    intrabc += 1;
                }
                // --- the four informational ones libavcodec also sends ---
                assert_eq!(
                    f.render_and_frame_size_different(),
                    bit(p.render_and_frame_size_different)
                );
                assert_eq!(
                    f.frame_size_override_flag(),
                    bit(p.frame_size_override_flag)
                );
                assert_eq!(
                    f.buffer_removal_time_present_flag(),
                    bit(p.buffer_removal_time_present_flag)
                );
                assert_eq!(
                    f.frame_refs_short_signaling(),
                    bit(p.frame_refs_short_signaling)
                );
                informational += f.render_and_frame_size_different()
                    + f.frame_size_override_flag()
                    + f.buffer_removal_time_present_flag()
                    + f.frame_refs_short_signaling();
                // --- the twenty that were already right ---
                assert_eq!(f.error_resilient_mode(), bit(p.error_resilient_mode));
                assert_eq!(f.disable_cdf_update(), bit(p.disable_cdf_update));
                assert_eq!(f.use_superres(), bit(p.use_superres));
                assert_eq!(f.allow_high_precision_mv(), bit(p.allow_high_precision_mv));
                assert_eq!(
                    f.is_motion_mode_switchable(),
                    bit(p.is_motion_mode_switchable)
                );
                assert_eq!(f.use_ref_frame_mvs(), bit(p.use_ref_frame_mvs));
                assert_eq!(
                    f.disable_frame_end_update_cdf(),
                    bit(p.disable_frame_end_update_cdf)
                );
                assert_eq!(f.reduced_tx_set(), bit(p.reduced_tx_set));
                assert_eq!(f.reference_select(), bit(p.reference_select));
                assert_eq!(f.skip_mode_present(), bit(p.skip_mode_present));
                assert_eq!(
                    f.segmentation_enabled(),
                    bit(p.segmentation_params.segmentation_enabled)
                );
                // ⚠ `usesChromaLr` is deliberately zero even where the spec would
                // want it — see picture_info. Asserted so "fixing" it trips here
                // and the reasoning gets read.
                assert_eq!(
                    f.usesChromaLr(),
                    0,
                    "usesChromaLr is deliberately left at libavcodec's zero"
                );

                screen += f.allow_screen_content_tools();
                warped += f.allow_warped_motion();
                switchable += f.is_filter_switchable();
                integer_mv += f.force_integer_mv();
            }
        }

        assert_eq!(frames, 274);
        // Measured on this vector. These are what make the four assertions above
        // real: a conversion that never set them would report zero.
        assert_eq!(screen, 274, "allow_screen_content_tools: 274/274");
        assert_eq!(warped, 273, "allow_warped_motion: 273/274");
        assert_eq!(switchable, 172, "is_filter_switchable: 172/274");
        assert_eq!(integer_mv, 1, "force_integer_mv: the key frame only");
        assert!(intrabc <= frames);
        // ⚠ Honest gap: this vector codes none of the four informational flags, so
        // their assertions above compare 0 against 0. They are covered by review
        // and by the libavcodec cross-read, not by this measurement.
        assert_eq!(
            informational, 0,
            "if this ever fires, the informational flags ARE exercised — say so \
             here rather than deleting the count"
        );
    }

    /// `StdVideoAV1LoopFilter` carries FOUR levels, and the last two are chroma.
    ///
    /// ⚠ **Read the history before trusting an older comment about this field.**
    /// The AV1 rung's frame-0 divergence was `luma IDENTICAL, chroma 319/38400
    /// bytes differ, max |delta| 4` on an RTX 5070 Ti (610.57.04), and re-decoding
    /// frame 0 in software with `loop_filter_level[2]` and `[3]` forced to zero
    /// reproduced it exactly — same 319 bytes, same `1:219 2:64 3-4:36` histogram,
    /// same first six differing bytes. That reading was right about the SYMPTOM
    /// and wrong about the cause: the conclusion drawn from it, that the driver
    /// ignores these two levels, is **refuted**. It reads them. What it also read,
    /// at every `vkCmdDecodeVideoKHR`, was a FREED sequence header whose recycled
    /// bytes happened to say `mono_chrome = 1` — so it deblocked the frame as
    /// monochrome, which skips exactly `loop_filter_level[2..3]` (7.14) and
    /// nothing else. [`crate::session_av1`] carries that measurement and the fix;
    /// with the backing held, all 250 frames are bit-identical to libavcodec.
    ///
    /// So this stays, as the guard it always was rather than as evidence for a
    /// driver claim. The conversion is a whole-array assignment and the test
    /// therefore looks tautological. It is not: `loop_filter_level` is the ONE Std
    /// array whose entries mean different things at different indices — `[0]` and
    /// `[1]` are the luma passes, `[2]` and `[3]` are U and V — and both of the
    /// other rungs spell it as a two-entry array plus two named fields
    /// (`filter_level_u` / `filter_level_v` in DXVA, the same in VA-API). A
    /// conversion that copied "the levels" as a pair is the natural mistake, it is
    /// what the DXVA layout invites, and nothing else in this file would notice.
    /// The values below are additionally confirmed against what libavcodec's own
    /// Vulkan hwaccel puts on the wire for this vector, captured at the API with a
    /// layer: `03 00 00 00 01 07 08 0c 00 00 01 00 00 00 ff 00 ff ff 00 00 …`.
    #[test]
    fn the_chroma_deblocking_levels_are_the_last_two_of_four() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_chroma_lf) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let lf = &plan.header.loop_filter_params;
                // SAFETY: `pLoopFilter` points at the boxed block `vk.pic` owns,
                // alive for as long as `vk` is.
                let sent = unsafe { *vk.pic.std().pLoopFilter };
                assert_eq!(
                    sent.loop_filter_level, lf.loop_filter_level,
                    "frame {frames}: all four levels, in order — [0] and [1] the \
                     luma passes, [2] U, [3] V"
                );
                assert_eq!(sent.loop_filter_sharpness, lf.loop_filter_sharpness);
                assert_eq!(sent.loop_filter_ref_deltas, lf.loop_filter_ref_deltas);
                assert_eq!(sent.loop_filter_mode_deltas, lf.loop_filter_mode_deltas);
                if lf.loop_filter_level[2] != 0 || lf.loop_filter_level[3] != 0 {
                    with_chroma_lf += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        sent.loop_filter_level,
                        [1, 7, 8, 12],
                        "frame 0's levels, and the four bytes libavcodec's Vulkan \
                         hwaccel was captured sending for this same frame"
                    );
                    assert_eq!(
                        sent.loop_filter_ref_deltas,
                        [1, 0, 0, 0, -1, 0, -1, -1],
                        "a PRIMARY_REF_NONE frame gets the spec's defaults from \
                         setup_past_independence, and they bump every level by one — \
                         luma included, which is how the parity leg's bit-exact luma \
                         proves the driver read the deltas and the first two levels"
                    );
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            with_chroma_lf, 123,
            "123 of 274 frames of this vector deblock chroma; at zero every level \
             compared above would be zero on both sides and a conversion that sent \
             only the luma pair would pass"
        );
    }

    /// `StdVideoAV1CDEF`'s secondary strengths carry the CODED two-bit value.
    ///
    /// The defect this pins is the twin of the `LoopRestorationSize` one below:
    /// the vendored parser stores what the AV1 SPEC leaves in the variable after
    /// its in-place fixup (`== 3` becomes 4), and every decode API — Vulkan
    /// included, because libavcodec sends CBS's unmodified two-bit read — wants the
    /// value BEFORE it. It is worse than the loop-restoration one in exactly one
    /// way: 4 is not an absurd number a driver would reject, it is a number that
    /// overflows a two-bit field into 0, so the strongest secondary CDEF filter
    /// becomes NO secondary filter and the frame is merely slightly wrong.
    ///
    /// Frame 0 of the vector codes it, which is why this was the AV1 parity leg's
    /// FIRST divergent frame, and CDEF is in-loop, which is why every frame after
    /// it diverged too.
    #[test]
    fn cdef_secondary_strengths_are_the_coded_value_not_the_spec_fixup() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut corrected_frames) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let raw = &plan.header.cdef_params;
                // SAFETY: `pCDEF` points at the boxed block `vk.pic` owns, alive
                // for as long as `vk` is.
                let sent = unsafe { *vk.pic.std().pCDEF };
                let mut corrected = false;
                for i in 0..8 {
                    // Two bits is all any hardware API gives this field.
                    assert!(
                        sent.cdef_y_sec_strength[i] <= 3 && sent.cdef_uv_sec_strength[i] <= 3,
                        "frame {frames}: a secondary strength above 3 overflows the \
                         two bits VA-API, NVDEC and DXVA pack it into"
                    );
                    // The primaries are NOT fixed up by the spec and must reach the
                    // driver untouched — a correction applied to the wrong one of
                    // the four arrays would be just as silent.
                    assert_eq!(
                        u32::from(sent.cdef_y_pri_strength[i]),
                        raw.cdef_y_pri_strength[i]
                    );
                    assert_eq!(
                        u32::from(sent.cdef_uv_pri_strength[i]),
                        raw.cdef_uv_pri_strength[i]
                    );
                    if raw.cdef_y_sec_strength[i] == 4 || raw.cdef_uv_sec_strength[i] == 4 {
                        corrected = true;
                    }
                }
                if corrected {
                    corrected_frames += 1;
                }
                if frames == 1 {
                    let coded = 1usize << raw.cdef_bits;
                    assert_eq!(coded, 4, "frame 0 codes cdef_bits = 2");
                    assert_eq!(
                        (
                            &sent.cdef_y_sec_strength[..coded],
                            &sent.cdef_uv_sec_strength[..coded]
                        ),
                        (&[1u8, 2, 0, 3][..], &[3u8, 0, 0, 0][..]),
                        "frame 0's secondary strengths as libavcodec sends them — \
                         the parser holds 4 where these read 3"
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

    /// `LoopRestorationSize` carries the CODED value, not the pixel size.
    ///
    /// Three frames of the vector switch loop restoration on, at
    /// `lr_unit_shift = 1` / `lr_uv_shift = 0` — a 128-pixel unit whose coded value
    /// is 2. Sending 128 (what the parser stores, and what this conversion sent
    /// until the M7 review) asks a driver that reads the field as
    /// `log2_restoration_size_minus5` for a restoration unit of 2^133 pixels.
    #[test]
    fn loop_restoration_size_is_the_coded_value_not_the_pixel_size() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_lr) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let lr = &plan.header.loop_restoration_params;
                // SAFETY: `pLoopRestoration` points at the boxed block `vk.pic`
                // owns, alive for as long as `vk` is.
                let sizes = unsafe { (*vk.pic.std().pLoopRestoration).LoopRestorationSize };
                assert_eq!(
                    sizes[0],
                    1 + u16::from(lr.lr_unit_shift),
                    "luma: libavcodec sends 1 + lr_unit_shift"
                );
                let chroma = 1 + u16::from(lr.lr_unit_shift) - u16::from(lr.lr_uv_shift);
                assert_eq!(sizes[1], chroma);
                assert_eq!(sizes[2], chroma);
                if lr.uses_lr {
                    with_lr += 1;
                    assert_eq!(lr.loop_restoration_size, [128, 128, 128]);
                    assert_eq!(sizes, [2, 2, 2]);
                    assert_ne!(
                        sizes[0], lr.loop_restoration_size[0],
                        "the coded value and the pixel size must differ here, or \
                         this test cannot tell them apart"
                    );
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            with_lr, 3,
            "three frames of this vector use loop restoration; at zero the \
             assertions above only ever saw the off state"
        );
    }

    /// A reference's Std info must describe the REFERENCE, not the frame reading
    /// it — and `RefFrameSignBias` must actually carry the future references this
    /// vector is full of.
    #[test]
    fn reference_info_describes_the_reference_and_not_the_current_frame() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;
        let (mut mixed_types, mut biased, mut with_saved_hints) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let current_type = plan.header.frame_type as u8;

                for r in &vk.refs {
                    let by_id = plan
                        .refs
                        .iter()
                        .flatten()
                        .find(|p| p.id == r.id)
                        .expect("every vk ref came from a named plan reference");
                    assert_eq!(r.std.frame_type, by_id.state.frame_type as u8);
                    assert_eq!(r.std.RefFrameSignBias, by_id.state.ref_frame_sign_bias);
                    assert_eq!(
                        r.std.flags.disable_frame_end_update_cdf(),
                        u32::from(by_id.state.disable_frame_end_update_cdf)
                    );
                    assert_eq!(
                        r.std.flags.segmentation_enabled(),
                        u32::from(by_id.state.segmentation_enabled)
                    );
                    assert_eq!(r.std.OrderHint, by_id.state.order_hint as u8);
                    for (sent, want) in r
                        .std
                        .SavedOrderHints
                        .iter()
                        .zip(by_id.state.saved_order_hints.iter())
                    {
                        assert_eq!(u32::from(*sent), *want);
                    }

                    if r.std.frame_type != current_type {
                        mixed_types += 1;
                    }
                    if r.std.RefFrameSignBias != 0 {
                        biased += 1;
                    }
                    if r.std.SavedOrderHints.iter().any(|h| *h != 0) {
                        with_saved_hints += 1;
                    }
                }
                // The setup picture activates a slot and is cached as that slot's
                // reference info, so it must carry the current frame's own state
                // through the very same path.
                let own = pf_bitstream::av1::RefState::of(&plan.header);
                assert_eq!(vk.setup_ref.frame_type, own.frame_type as u8);
                assert_eq!(vk.setup_ref.RefFrameSignBias, own.ref_frame_sign_bias);
                assert_eq!(vk.setup_ref.OrderHint, own.order_hint as u8);
            }
        }

        assert_eq!(frames, 274);
        assert!(
            mixed_types > 0,
            "no reference ever had a different frame type from the frame reading \
             it, so handing every reference the CURRENT type would have passed"
        );
        assert!(
            biased > 0,
            "no reference carried a sign bias: this is the hidden-ALTREF vector, \
             so a zero here means the mask never reached the Std struct and every \
             future reference reads as past"
        );
        assert!(with_saved_hints > 0, "SavedOrderHints never carried a hint");
        eprintln!(
            "refs with a foreign frame type {mixed_types} · with a sign bias \
             {biased} · with saved order hints {with_saved_hints}"
        );
    }

    /// Film grain's six chroma-scaling coefficients reach the Std block.
    ///
    /// ⚠ The vendored vector codes NO film grain (`film_grain_params_present` is
    /// false on all 274 frames), so this is a hand-built header — the only way the
    /// grain path is exercised at all. It is also why the six fields could go
    /// missing unnoticed: nothing that runs on the vector touches them.
    #[test]
    fn film_grain_carries_the_chroma_scaling_coefficients() {
        let mut sequence = pf_bitstream::av1::ParsedSequenceHeader {
            film_grain_params_present: true,
            ..Default::default()
        };

        let mut header = pf_bitstream::av1::ParsedFrameHeader::default();
        let fg = &mut header.film_grain_params;
        fg.apply_grain = true;
        fg.grain_seed = 0x1234;
        fg.num_y_points = 2;
        fg.num_cb_points = 1;
        fg.num_cr_points = 1;
        fg.cb_mult = 128;
        fg.cb_luma_mult = 192;
        fg.cb_offset = 256;
        fg.cr_mult = 129;
        fg.cr_luma_mult = 193;
        fg.cr_offset = 257;

        let pic = picture_info(&header, &sequence).expect("a grain header converts");
        assert_eq!(pic.std().flags.apply_grain(), 1);
        assert!(!pic.std().pFilmGrain.is_null());
        // SAFETY: `pFilmGrain` points at the boxed block `pic` owns, alive here.
        let grain = unsafe { *pic.std().pFilmGrain };
        assert_eq!(grain.grain_seed, 0x1234);
        assert_eq!(
            (
                grain.cb_mult,
                grain.cb_luma_mult,
                grain.cb_offset,
                grain.cr_mult,
                grain.cr_luma_mult,
                grain.cr_offset
            ),
            (128, 192, 256, 129, 193, 257),
            "the six chroma-scaling coefficients: nothing else describes how luma \
             feeds chroma grain, and zeroes are not 'less grain', they are \
             different grain"
        );

        // And the gate still holds: a sequence that never declared grain gets a
        // null block whatever the frame says.
        sequence.film_grain_params_present = false;
        let pic = picture_info(&header, &sequence).expect("converts");
        assert!(pic.std().pFilmGrain.is_null());
        assert_eq!(pic.std().flags.apply_grain(), 0);
    }
}
