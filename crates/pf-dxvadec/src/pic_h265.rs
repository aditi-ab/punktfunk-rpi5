//! Per-AU H.265 conversion: one [`AuPlan`] into the `DXVA_PicParams_HEVC`,
//! `DXVA_Qmatrix_HEVC` and slice-control records
//! `ID3D11VideoContext::SubmitDecoderBuffers` takes — [`crate::pic`] one codec
//! over, and the DXVA twin of [`pf_vkdecode::pic_h265`].
//!
//! The codec difference that shapes this module is the same one that shapes the
//! Vulkan H.265 converter: HEVC decode takes NO per-slice reference lists. The
//! hardware re-derives 8.3.4's lists itself from the slice bits, keyed by the
//! picture-level RPS index arrays (`RefPicSetStCurrBefore`/`StCurrAfter`/
//! `LtCurr`), which are INDICES INTO `RefPicList` — so nothing here expresses
//! per-slice list ORDER, unlike H.264. The per-slice lists still exist and are
//! used as a cross-check: every entry must be a member of the current sets, or
//! the conversion fails closed.
//!
//! **Surfaces are slots** carries over verbatim from [`crate::pic`] and is
//! documented there rather than repeated: a `DXVA_PicEntry_HEVC` carries the decode
//! texture array's `ArraySlice`, which is what lets this module drive the Vulkan
//! rung's [`SlotMap`] unchanged.
//!
//! # `RefPicList` is the MARKED DPB, indexed by the RPS arrays
//!
//! `RefPicList` holds every picture currently marked used for reference —
//! libavcodec's DXVA HEVC path walks its whole DPB for
//! `HEVC_FRAME_FLAG_{LONG,SHORT}_REF` — and the `RefPicSetStCurrBefore`/
//! `StCurrAfter`/`LtCurr` arrays are INDICES into it. The two questions are
//! therefore separate: which pictures the hardware may hold (the array), and which
//! of them THIS picture uses (the indices).
//!
//! The union of the three current sets is not the same thing, and the gap is
//! 8.3.2's *Foll* sets: a long-term anchor pinned for a later picture is marked in
//! the DPB while no current set names it. Binding only the current sets would drop
//! it from `RefPicList` for exactly the pictures between the pin and its use, and a
//! driver keeping per-reference state may treat that absence as a retirement — the
//! RFI failure shape. So the array is the plan's
//! [`dpb_refs`](pf_bitstream::h265::AuPlan::dpb_refs) snapshot, laid out with the
//! current sets first (which keeps the index arrays' values identical to what the
//! sets alone produced) and the remaining marked pictures appended.
//!
//! Vulkan's `pReferenceSlots` asks the other question — the slots THIS decode
//! operation uses — so the native Vulkan rung binds the current sets and is right
//! to; the two rungs differ here because the two specifications do.
//!
//! Concealment note: a lost reference is ABSENT from the plan's RPS sets (flagged
//! upstream via `PlanWarning::MissingReference`), so the index arrays compact past
//! it. That is deliberate — there is no surface to point at, `0xFF` padding keeps
//! the arrays well-formed, and the session layer has already been told to request
//! recovery. Fabricating an entry is the one thing this crate never does.

use std::ops::Range;

use cros_codecs::codec::h265::parser::Pps;
use cros_codecs::codec::h265::parser::Sps;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::RefPic;
use pf_vkdecode::SlotError;
use pf_vkdecode::SlotMap;
use tracing::trace;

use crate::dxva::HevcFormatFlags;
use crate::dxva::HevcPictureFlags;
use crate::dxva::HevcToolFlags;
use crate::dxva::PicEntry;
use crate::dxva::PicParamsHevc;
use crate::dxva::QmatrixHevc;
use crate::dxva::SliceHevcShort;
use crate::dxva::UNUSED_ENTRY;

/// `RefPicList`'s length in `DXVA_PicParams_HEVC`. Fifteen, not sixteen: the
/// current picture is named separately by `CurrPic`, so the array only ever
/// holds the other DPB members.
const REF_PIC_LIST_LEN: usize = 15;

/// Each `RefPicSet*` index array holds eight entries — the hard ceiling on how
/// many CURRENT references one set may carry through DXVA (H.265 itself allows
/// more; beyond eight is unexpressible here and rejected).
pub const RPS_LIST_SIZE: usize = 8;

/// One active reference of the AU: its surface index and the planner id it
/// resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaRefH265 {
    /// The DPB slot, which is the decode texture array's `ArraySlice`.
    pub slot: u8,
    pub id: PicId,
    pub is_long_term: bool,
    pub pic_order_cnt: i32,
}

/// Everything CPU-derivable of one AU's DXVA submission.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodePlanDxvaH265 {
    /// The picture parameters. Their `RefPicSetStCurrBefore`/`StCurrAfter`/
    /// `LtCurr` arrays hold INDICES INTO [`Self::refs`] (`0xFF` = unused), which
    /// is exactly how they index `RefPicList` — the two are laid out in the same
    /// order by construction.
    pub pic_params: PicParamsHevc,
    /// The inverse-quantization matrices, or `None` when the sequence does not
    /// enable scaling lists — in which case the buffer is NOT SUBMITTED AT ALL.
    ///
    /// That is libavcodec's own condition: `dxva2_hevc_end_frame` passes the
    /// qmatrix buffer only when `dwCodingParamToolFlags & 1`
    /// (`scaling_list_enabled_flag`). Submitting it unconditionally is not the
    /// harmless belt-and-braces it looks like — with scaling lists disabled the
    /// hardware is told to ignore the matrices, and any driver that honours a
    /// buffer it was handed anyway would dequantize every residual against
    /// whatever the buffer holds. Every punktfunk HEVC stream is in exactly that
    /// shape.
    pub qmatrix: Option<QmatrixHevc>,
    /// Byte ranges of the AU's slice segment NALUs, start code included, in plan
    /// order — what [`crate::pack::pack`] takes.
    pub slice_ranges: Vec<Range<usize>>,
    /// The surface the decoded picture is written into.
    pub setup_slot: u8,
    pub setup_id: PicId,
    /// Whether later pictures may reference the decoded picture (false for
    /// sub-layer non-reference NALU types).
    pub setup_is_reference: bool,
    /// The marked DPB, resolved to surfaces and laid out identically in
    /// `pic_params.RefPicList`: the plan's three current RPS sets first, in set
    /// order (StCurrBefore, StCurrAfter, LtCurr) and first appearance first, then
    /// every other marked picture the DPB holds (module docs).
    pub refs: Vec<DxvaRefH265>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades it
/// to [`pf_bitstream::h265::PlanWarning`]s upstream. (`PlanError::RaslSkipped`
/// also never reaches this layer: it is an error OF planning, handled as an
/// Ok-skip by the client wiring, and no plan exists to convert.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaH265Error {
    /// The plan holds no slices; there is nothing to submit.
    NoSlices,
    /// The plan's `DpbUpdate.stored` is `None`.
    NoStoredId,
    /// An RPS entry's id holds no slot: an earlier plan never went through this
    /// [`SlotMap`].
    UnresolvedReference(PicId),
    /// A slice reference-list entry names a picture outside the plan's current
    /// RPS sets. 8.3.4 builds every list FROM those sets, so this is a planner
    /// contract violation — and no amount of `RefPicList` residency fixes it,
    /// because the hardware derives its lists from the RPS INDEX ARRAYS, which
    /// only the current sets populate.
    ReferenceOutsideRps(PicId),
    Slot(SlotError),
    /// A current RPS set holds more entries than the index arrays' eight.
    RpsSetOverflow {
        set: &'static str,
        len: usize,
    },
    /// The AU references more distinct pictures than `RefPicList` holds (15).
    TooManyReferences(usize),
    /// The first slice's inline `st_ref_pic_set()` predicts from an SPS
    /// candidate that does not exist — `ucNumDeltaPocsOfRefRpsIdx` cannot be
    /// derived, and the hardware would misparse the slice header.
    InvalidRefRpsIdx {
        curr_rps_idx: u8,
        delta_idx_minus1: u8,
    },
    /// The inline `st_ref_pic_set()`'s bit count exceeds `u16`
    /// (`wNumBitsForShortTermRPSInSlice`) — a header that large is corrupt.
    StRpsBitsOverflow(u32),
    /// The predicted-from candidate's `NumDeltaPocs` exceeds `u8`. Impossible
    /// off a real parse (≤ 32); an error rather than a clamp, because a clamped
    /// count makes the hardware misparse the slice header.
    NumDeltaPocsOverflow(u32),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB; rebuild decoder,
    /// pool and map.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A picture dimension in minimum coding blocks exceeds the `USHORT` the
    /// picture parameters carry.
    DimensionOverflow {
        width: u32,
        height: u32,
    },
    /// `separate_colour_plane_flag`. Refused upstream by pf-bitstream's envelope
    /// gate; checked again because the picture layout for it is a different
    /// shape entirely.
    SeparateColourPlanes,
}

impl std::fmt::Display for PlanToDxvaH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaH265Error::NoSlices => write!(f, "the plan holds no slices"),
            PlanToDxvaH265Error::NoStoredId => write!(
                f,
                "the plan stores no picture (flush updates go to SlotMap::apply)"
            ),
            PlanToDxvaH265Error::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToDxvaH265Error::ReferenceOutsideRps(id) => write!(
                f,
                "a slice list names picture {id}, which is not in the AU's RPS sets"
            ),
            PlanToDxvaH265Error::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToDxvaH265Error::RpsSetOverflow { set, len } => {
                write!(f, "{set} holds {len} entries; DXVA's index arrays hold 8")
            }
            PlanToDxvaH265Error::TooManyReferences(count) => {
                write!(f, "{count} references exceed DXVA's RefPicList of 15")
            }
            PlanToDxvaH265Error::InvalidRefRpsIdx {
                curr_rps_idx,
                delta_idx_minus1,
            } => write!(
                f,
                "inline RPS {curr_rps_idx} predicts from delta_idx_minus1 \
                 {delta_idx_minus1}, which names no SPS candidate"
            ),
            PlanToDxvaH265Error::StRpsBitsOverflow(bits) => {
                write!(f, "inline st_ref_pic_set of {bits} bits exceeds u16")
            }
            PlanToDxvaH265Error::NumDeltaPocsOverflow(count) => {
                write!(f, "candidate NumDeltaPocs {count} exceeds u8")
            }
            PlanToDxvaH265Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the plan needs {required} slots but the map holds {capacity} — \
                 an SPS renegotiation resized the DPB; rebuild decoder and map"
            ),
            PlanToDxvaH265Error::DimensionOverflow { width, height } => write!(
                f,
                "a {width}x{height} picture in min-CBs exceeds the DXVA picture parameters"
            ),
            PlanToDxvaH265Error::SeparateColourPlanes => {
                write!(f, "separate_colour_plane_flag is outside this backend")
            }
        }
    }
}

impl std::error::Error for PlanToDxvaH265Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToDxvaH265Error::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToDxvaH265Error {
    fn from(err: SlotError) -> Self {
        PlanToDxvaH265Error::Slot(err)
    }
}

/// `ucNumDeltaPocsOfRefRpsIdx`: when the first slice's inline `st_ref_pic_set()`
/// uses inter-RPS prediction, the hardware re-parses those slice bits and needs
/// `NumDeltaPocs[RefRpsIdx]` of the SOURCE candidate to size the
/// `used_by_curr_pic_flag`/`use_delta_flag` loop (7.4.8); otherwise 0.
///
/// Byte-for-byte the derivation `pf_vkdecode::pic_h265` makes for Vulkan's
/// `NumDeltaPocsOfRefRpsIdx` — the same field under a different name. It is
/// duplicated rather than shared because it is private there and the two crates
/// have no business exporting each other's internals; if it ever changes, both
/// copies' tests plan the same vendored vector and will disagree.
fn num_delta_pocs_of_ref_rps_idx(plan: &AuPlan) -> Result<u8, PlanToDxvaH265Error> {
    let hdr = &plan
        .slices
        .first()
        .expect("caller validated the plan holds slices")
        .header;
    // Inline means CurrRpsIdx == num_short_term_ref_pic_sets (8.3.2 NOTE 2); an
    // SPS-indexed RPS re-parses nothing in the slice header.
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
        .ok_or(PlanToDxvaH265Error::InvalidRefRpsIdx {
            curr_rps_idx: hdr.curr_rps_idx,
            delta_idx_minus1: delta,
        })?;
    u8::try_from(source.num_delta_pocs)
        .map_err(|_| PlanToDxvaH265Error::NumDeltaPocsOverflow(source.num_delta_pocs))
}

/// One marked DPB picture, resolved to its surface.
fn dxva_ref(slot: u8, rp: &RefPic) -> DxvaRefH265 {
    DxvaRefH265 {
        slot,
        id: rp.id,
        is_long_term: rp.is_long_term,
        pic_order_cnt: rp.pic_order_cnt,
    }
}

/// `DXVA_Qmatrix_HEVC` for a sequence that enables scaling lists.
///
/// # Which lists
///
/// 7.4.5's activation, in the order the spec resolves it:
///
/// 1. the PPS's, when it codes scaling list data;
/// 2. otherwise the SPS's, when IT codes scaling list data;
/// 3. otherwise the Table 7-5/7-6 DEFAULT lists.
///
/// libavcodec writes the first two legs as one ternary and stops, because its own
/// parser seeds an SPS that codes nothing with the defaults — so leg 3 never has to
/// be spelled out there. The vendored cros-codecs parser does NOT: `ScalingLists`
/// defaults to all zeros and an SPS only ever fills it under
/// `scaling_list_enabled_flag && sps_scaling_list_data_present_flag`. Copying
/// libavcodec's ternary verbatim therefore hands the driver 64 zeros per list on a
/// stream that says "enabled, nothing coded, use the defaults" — a legal and
/// entirely ordinary shape — and every residual dequantizes to nothing.
///
/// Leg 3 is served by the PPS's lists, which the same parser DOES default-fill
/// (Table 7-5/7-6 plus a DC of 8 + 8 = 16) whenever the PPS codes none. So:
/// SPS-coded data wins only when the PPS coded none, and the PPS's copy carries
/// both leg 1 and leg 3.
fn quantization_matrices(sps: &Sps, pps: &Pps) -> QmatrixHevc {
    let sl = if sps.scaling_list_data_present_flag && !pps.scaling_list_data_present_flag {
        &sps.scaling_list
    } else {
        &pps.scaling_list
    };
    let mut qm = QmatrixHevc::zeroed();
    qm.ucScalingLists0 = sl.scaling_list_4x4;
    qm.ucScalingLists1 = sl.scaling_list_8x8;
    qm.ucScalingLists2 = sl.scaling_list_16x16;
    // sizeId 3 codes only matrixId 0 and 3 (its loop steps by three), and DXVA
    // carries exactly those two — so index k here is the parser's k * 3.
    qm.ucScalingLists3[0] = sl.scaling_list_32x32[0];
    qm.ucScalingLists3[1] = sl.scaling_list_32x32[3];
    // The DC entries are the ScalingFactor DC VALUE (`…_minus8 + 8`), not the
    // coded delta. Clamped rather than wrapped: the parser bounds the coded
    // value to -7..=247, so the sum is 1..=255 and the clamp is unreachable.
    for (dst, src) in qm
        .ucScalingListDCCoefSizeID2
        .iter_mut()
        .zip(sl.scaling_list_dc_coef_minus8_16x16)
    {
        *dst = (i32::from(src) + 8).clamp(0, 255) as u8;
    }
    for (k, dst) in qm.ucScalingListDCCoefSizeID3.iter_mut().enumerate() {
        let src = sl.scaling_list_dc_coef_minus8_32x32[k * 3];
        *dst = (i32::from(src) + 8).clamp(0, 255) as u8;
    }
    qm
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `status_id` becomes `StatusReportFeedbackNumber` — see [`crate::pic::plan_to_dxva`].
///
/// Atomicity contract (identical to the H.264 module): every fallible step runs
/// before any mutation of `slots`, so an error leaves the map exactly as it was.
pub fn plan_to_dxva_h265(
    plan: &AuPlan,
    slots: &mut SlotMap,
    status_id: u32,
) -> Result<DecodePlanDxvaH265, PlanToDxvaH265Error> {
    if plan.slices.is_empty() {
        return Err(PlanToDxvaH265Error::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaH265Error::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if sps.separate_colour_plane_flag {
        return Err(PlanToDxvaH265Error::SeparateColourPlanes);
    }

    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToDxvaH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // The AU-level binding set: the union of the three current RPS sets, first
    // appearance first, plus the index arrays that point into it.
    let mut refs: Vec<DxvaRefH265> = Vec::new();
    let mut index_arrays = [[UNUSED_ENTRY; RPS_LIST_SIZE]; 3];
    let sets: [(&'static str, &[RefPic]); 3] = [
        ("RefPicSetStCurrBefore", &plan.rps.st_curr_before),
        ("RefPicSetStCurrAfter", &plan.rps.st_curr_after),
        ("RefPicSetLtCurr", &plan.rps.lt_curr),
    ];
    for (array, (name, set)) in index_arrays.iter_mut().zip(sets) {
        if set.len() > RPS_LIST_SIZE {
            return Err(PlanToDxvaH265Error::RpsSetOverflow {
                set: name,
                len: set.len(),
            });
        }
        for (position, rp) in set.iter().enumerate() {
            let index = match refs.iter().position(|existing| existing.id == rp.id) {
                // A picture that appears in two sets binds ONCE and both index
                // arrays point at that one entry.
                Some(index) => index,
                None => {
                    let slot = slots
                        .slot_of(rp.id)
                        .ok_or(PlanToDxvaH265Error::UnresolvedReference(rp.id))?;
                    // The DPB snapshot is the authority for the marking: an
                    // `RefPicSetLtCurr` index into a short-term-marked entry is an
                    // inconsistent DPB, and hardware treats the two differently.
                    // The set's own copy is the fallback and cannot be reached off
                    // a real plan (8.3.2 marks every set member before the sets
                    // are reported).
                    let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
                    if marked.is_none() {
                        trace!(
                            id = rp.id,
                            "an RPS entry names a picture the marked DPB does not hold"
                        );
                    }
                    refs.push(dxva_ref(slot, marked.unwrap_or(rp)));
                    refs.len() - 1
                }
            };
            // The RPS-set overflow check above bounds this well inside u8 (and
            // below the 0xFF sentinel).
            array[position] = index as u8;
        }
    }
    // The current sets are what the index arrays MUST be able to name, so their
    // overflow is a refusal.
    if refs.len() > REF_PIC_LIST_LEN {
        return Err(PlanToDxvaH265Error::TooManyReferences(refs.len()));
    }

    // Cross-check, against the CURRENT sets alone — which is what `refs` holds at
    // this exact point, and why the check runs before the *Foll* pictures are
    // appended. 8.3.4 builds every slice list from the current sets, so an entry
    // outside them is a planner-contract violation: the hardware re-derives its
    // lists from the RPS INDEX ARRAYS, and a picture merely present in
    // `RefPicList` is not reachable by that derivation.
    let current_set_refs = refs.len();
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if !refs[..current_set_refs]
                .iter()
                .any(|existing| existing.id == rp.id)
            {
                return Err(PlanToDxvaH265Error::ReferenceOutsideRps(rp.id));
            }
        }
    }

    // Then the rest of the marked DPB — the *Foll* pictures (module docs) — in the
    // planner's DPB order, which is libavcodec's. Overflow past the array is
    // dropped rather than refused: nothing here is referenced by this picture, so
    // the decode is unaffected and refusing would cost the whole frame. `RefPicList`
    // holds fifteen while the DPB holds up to sixteen, so this is reachable, if
    // only on a stream that has filled its DPB entirely with references.
    for rp in &plan.dpb_refs {
        if refs.len() == REF_PIC_LIST_LEN {
            trace!(
                marked = plan.dpb_refs.len(),
                "the marked DPB exceeds RefPicList; the tail is not expressible"
            );
            break;
        }
        if refs.iter().any(|existing| existing.id == rp.id) {
            continue;
        }
        match slots.slot_of(rp.id) {
            Some(slot) => refs.push(dxva_ref(slot, rp)),
            None => trace!(id = rp.id, "a marked DPB picture holds no slot in this map"),
        }
    }

    // Everything else fallible, before any mutation.
    let num_delta_pocs = num_delta_pocs_of_ref_rps_idx(plan)?;
    let st_rps_bits = u16::try_from(pic.short_term_ref_pic_set_size_bits).map_err(|_| {
        PlanToDxvaH265Error::StRpsBitsOverflow(pic.short_term_ref_pic_set_size_bits)
    })?;
    let min_cb = sps.min_cb_log2_size_y;
    let width_in_min_cbs = u32::from(sps.pic_width_in_luma_samples) >> min_cb;
    let height_in_min_cbs = u32::from(sps.pic_height_in_luma_samples) >> min_cb;
    let (Ok(width_in_min_cbs), Ok(height_in_min_cbs)) = (
        u16::try_from(width_in_min_cbs),
        u16::try_from(height_in_min_cbs),
    ) else {
        return Err(PlanToDxvaH265Error::DimensionOverflow {
            width: width_in_min_cbs,
            height: height_in_min_cbs,
        });
    };

    let mut pp = PicParamsHevc::zeroed();
    pp.PicWidthInMinCbsY = width_in_min_cbs;
    pp.PicHeightInMinCbsY = height_in_min_cbs;
    pp.wFormatAndSequenceInfoFlags = HevcFormatFlags {
        chroma_format_idc: pic.chroma_format_idc,
        separate_colour_plane_flag: false, // refused above
        bit_depth_luma_minus8: pic.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: pic.bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
    }
    .pack();
    // The DPB depth of the HIGHEST temporal sub-layer — the only one whose
    // buffering covers the whole stream. `max_sub_layers_minus1` indexes a
    // seven-entry array, so the read cannot go out of bounds off any parse.
    pp.sps_max_dec_pic_buffering_minus1 = sps
        .max_dec_pic_buffering_minus1
        .get(usize::from(sps.max_sub_layers_minus1))
        .copied()
        .unwrap_or(0);
    pp.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
    pp.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
    pp.log2_min_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
    pp.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_luma_transform_block_size;
    pp.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
    pp.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
    pp.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
    pp.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
    pp.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    pp.init_qp_minus26 = pps.init_qp_minus26;
    pp.ucNumDeltaPocsOfRefRpsIdx = num_delta_pocs;
    // 0 when the RPS came from the SPS by index — exactly DXVA's convention for
    // this field, and pf-bitstream's for the value it derives from.
    pp.wNumBitsForShortTermRPSInSlice = st_rps_bits;
    pp.dwCodingParamToolFlags = HevcToolFlags {
        scaling_list_enabled_flag: sps.scaling_list_enabled_flag,
        amp_enabled_flag: sps.amp_enabled_flag,
        sample_adaptive_offset_enabled_flag: sps.sample_adaptive_offset_enabled_flag,
        pcm_enabled_flag: sps.pcm_enabled_flag,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps
            .log2_diff_max_min_pcm_luma_coding_block_size,
        pcm_loop_filter_disabled_flag: sps.pcm_loop_filter_disabled_flag,
        long_term_ref_pics_present_flag: sps.long_term_ref_pics_present_flag,
        sps_temporal_mvp_enabled_flag: sps.temporal_mvp_enabled_flag,
        strong_intra_smoothing_enabled_flag: sps.strong_intra_smoothing_enabled_flag,
        dependent_slice_segments_enabled_flag: pps.dependent_slice_segments_enabled_flag,
        output_flag_present_flag: pps.output_flag_present_flag,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        sign_data_hiding_enabled_flag: pps.sign_data_hiding_enabled_flag,
        cabac_init_present_flag: pps.cabac_init_present_flag,
    }
    .pack();
    pp.dwCodingSettingPicturePropertyFlags = HevcPictureFlags {
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        transform_skip_enabled_flag: pps.transform_skip_enabled_flag,
        cu_qp_delta_enabled_flag: pps.cu_qp_delta_enabled_flag,
        pps_slice_chroma_qp_offsets_present_flag: pps.slice_chroma_qp_offsets_present_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_flag: pps.weighted_bipred_flag,
        transquant_bypass_enabled_flag: pps.transquant_bypass_enabled_flag,
        tiles_enabled_flag: pps.tiles_enabled_flag,
        entropy_coding_sync_enabled_flag: pps.entropy_coding_sync_enabled_flag,
        uniform_spacing_flag: pps.uniform_spacing_flag,
        loop_filter_across_tiles_enabled_flag: pps.loop_filter_across_tiles_enabled_flag,
        pps_loop_filter_across_slices_enabled_flag: pps.loop_filter_across_slices_enabled_flag,
        deblocking_filter_override_enabled_flag: pps.deblocking_filter_override_enabled_flag,
        pps_deblocking_filter_disabled_flag: pps.deblocking_filter_disabled_flag,
        lists_modification_present_flag: pps.lists_modification_present_flag,
        slice_segment_header_extension_present_flag: pps
            .slice_segment_header_extension_present_flag,
        irap_pic_flag: pic.is_irap,
        idr_pic_flag: pic.is_idr,
        // HEVC states intra-ness at the picture level: an IRAP picture is
        // intra-only by definition, and nothing else is guaranteed to be. Same
        // derivation libavcodec's DXVA HEVC path makes.
        intra_pic_flag: pic.is_irap,
    }
    .pack();
    pp.pps_cb_qp_offset = pps.cb_qp_offset;
    pp.pps_cr_qp_offset = pps.cr_qp_offset;
    if pps.tiles_enabled_flag {
        pp.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        pp.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        if !pps.uniform_spacing_flag {
            // Only the non-uniform case codes explicit widths; the uniform case
            // leaves them zero and the hardware derives its own grid.
            // `saturating_as` on each: a tile edge past 65535 min-CBs cannot
            // come off a real PPS, and a clamp here is strictly better than a
            // panic on a corrupt one.
            for (dst, src) in pp
                .column_width_minus1
                .iter_mut()
                .zip(pps.column_width_minus1)
            {
                *dst = u16::try_from(src).unwrap_or(u16::MAX);
            }
            for (dst, src) in pp.row_height_minus1.iter_mut().zip(pps.row_height_minus1) {
                *dst = u16::try_from(src).unwrap_or(u16::MAX);
            }
        }
    }
    pp.diff_cu_qp_delta_depth = if pps.cu_qp_delta_enabled_flag {
        pps.diff_cu_qp_delta_depth
    } else {
        0
    };
    pp.pps_beta_offset_div2 = pps.beta_offset_div2;
    pp.pps_tc_offset_div2 = pps.tc_offset_div2;
    pp.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
    pp.CurrPicOrderCntVal = pic.pic_order_cnt;
    pp.StatusReportFeedbackNumber = status_id;

    pp.RefPicList = [PicEntry::UNUSED; REF_PIC_LIST_LEN];
    for (i, r) in refs.iter().enumerate() {
        pp.RefPicList[i] = PicEntry::new(r.slot, r.is_long_term);
        pp.PicOrderCntValList[i] = r.pic_order_cnt;
    }
    [
        pp.RefPicSetStCurrBefore,
        pp.RefPicSetStCurrAfter,
        pp.RefPicSetLtCurr,
    ] = index_arrays;

    // The quantization matrices — built ONLY when the sequence enables scaling
    // lists, because that is the only case in which the buffer is submitted (see
    // `DecodePlanDxvaH265::qmatrix`).
    let qm = sps
        .scaling_list_enabled_flag
        .then(|| quantization_matrices(sps, pps));

    let slice_ranges: Vec<Range<usize>> = plan.slices.iter().map(|s| s.data.clone()).collect();

    // Mutations LAST, after every fallible step above. Removals first — they were
    // real regardless of this AU's fate — then the setup assignment, released
    // immediately when this very plan already evicted the stored picture (the
    // surface must still exist for the decode itself).
    let setup_evicted = plan.dpb.removed.contains(&setup_id);
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        if !slots.release(id) {
            trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
        }
    }
    let setup_slot = slots.assign(setup_id)?;
    if setup_evicted {
        slots.release(setup_id);
    }
    pp.CurrPic = PicEntry::new(setup_slot, false);

    Ok(DecodePlanDxvaH265 {
        pic_params: pp,
        qmatrix: qm,
        slice_ranges,
        setup_slot,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
    })
}

/// The slice-control records for a packed AU — [`crate::pic::slice_control`]'s
/// HEVC twin.
pub fn slice_control_h265(records: &[crate::pack::SliceRecord]) -> Vec<SliceHevcShort> {
    records
        .iter()
        .map(|r| SliceHevcShort {
            BSNALunitDataLocation: r.location,
            SliceBytesInBuffer: r.bytes,
            wBadSliceChopping: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use cros_codecs::codec::h265::parser::Nalu;
    use pf_bitstream::h265::H265Planner;

    use super::*;

    /// The same vendored vectors pf-bitstream's and pf-vkdecode's h265 tests
    /// plan, included from the same path.
    const TEST_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );
    const TEST_64X64_I_P_B_P: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/64x64-I-P-B-P.h265"
    );

    /// Test-only AU splitter, mirroring pf-vkdecode's (which mirrors
    /// pf-bitstream's `#[cfg(test)]`-private helper).
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

    fn convert_stream(stream: &[u8]) -> Vec<(AuPlan, DecodePlanDxvaH265)> {
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut out = Vec::new();
        for (i, au) in split_into_aus(stream).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            if map.capacity() != plan.picture.max_dpb_frames + 1 {
                *map = SlotMap::new(plan.picture.max_dpb_frames);
            }
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("conversion");
            out.push((plan, dxva));
        }
        out
    }

    #[test]
    fn the_whole_vendored_25fps_vector_converts_without_a_refusal() {
        let converted = convert_stream(TEST_25FPS);
        assert_eq!(converted.len(), 250);
    }

    #[test]
    fn the_index_arrays_point_at_the_ref_pic_list_entries_the_rps_sets_name() {
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            for (array, set) in [
                (
                    &dxva.pic_params.RefPicSetStCurrBefore,
                    &plan.rps.st_curr_before,
                ),
                (
                    &dxva.pic_params.RefPicSetStCurrAfter,
                    &plan.rps.st_curr_after,
                ),
                (&dxva.pic_params.RefPicSetLtCurr, &plan.rps.lt_curr),
            ] {
                for (position, entry) in array.iter().enumerate() {
                    match set.get(position) {
                        Some(rp) => {
                            let index = usize::from(*entry);
                            // The index array points into RefPicList, and
                            // RefPicList is laid out in `refs` order — so both
                            // must agree about the picture.
                            assert_eq!(dxva.refs[index].id, rp.id);
                            assert_eq!(dxva.pic_params.PicOrderCntValList[index], rp.pic_order_cnt);
                            assert_eq!(
                                dxva.pic_params.RefPicList[index].index(),
                                dxva.refs[index].slot
                            );
                        }
                        None => assert_eq!(*entry, UNUSED_ENTRY),
                    }
                }
            }
        }
    }

    /// The unique pictures the plan's three CURRENT sets name, in the order this
    /// conversion binds them (set order, first appearance first).
    fn current_set_ids(plan: &AuPlan) -> Vec<PicId> {
        let mut ids = Vec::new();
        for rp in plan
            .rps
            .st_curr_before
            .iter()
            .chain(&plan.rps.st_curr_after)
            .chain(&plan.rps.lt_curr)
        {
            if !ids.contains(&rp.id) {
                ids.push(rp.id);
            }
        }
        ids
    }

    #[test]
    fn the_reference_list_holds_every_marked_picture_and_the_current_sets_lead_it() {
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            let mut listed: Vec<PicId> = dxva.refs.iter().map(|r| r.id).collect();
            let mut marked: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            listed.sort_unstable();
            marked.sort_unstable();
            assert_eq!(listed, marked, "RefPicList must be the marked DPB");
            // The current sets lead, so the index arrays never point past them —
            // which is what makes appending the rest of the DPB safe.
            let current = current_set_ids(&plan);
            assert_eq!(
                dxva.refs
                    .iter()
                    .take(current.len())
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                current
            );
        }
    }

    #[test]
    fn a_marked_picture_no_current_set_names_is_appended_after_them() {
        // 8.3.2's *Foll* shape, which the vendored vectors never produce (their RPS
        // names every marked picture they hold) and which every long-term anchor
        // lives in: a picture the DPB keeps marked for a LATER picture. Injected
        // into the plan's snapshot rather than synthesised as a bitstream, because
        // the thing under test is what this conversion does with a snapshot wider
        // than the current sets.
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let plans: Vec<AuPlan> = aus
            .iter()
            .take(3)
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        let baseline: Vec<DecodePlanDxvaH265> = plans
            .iter()
            .enumerate()
            .map(|(i, plan)| plan_to_dxva_h265(plan, &mut slots, i as u32 + 1).expect("convert"))
            .collect();
        let last = baseline.last().expect("three conversions");
        let current_sets = last.refs.len();
        assert!(current_sets > 0, "the third AU must reference something");

        // Re-plan and re-convert with the third AU's snapshot widened by one marked
        // picture that no current set names.
        let mut planner = H265Planner::new();
        let mut plans: Vec<AuPlan> = aus
            .iter()
            .take(3)
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        for (i, plan) in plans.iter().take(2).enumerate() {
            plan_to_dxva_h265(plan, &mut slots, i as u32 + 1).expect("convert");
        }
        // A picture the map already holds and the third AU does not name, else a
        // fresh id parked in a free slot — either is a marked DPB entry with a
        // surface, which is all `RefPicList` needs.
        let setup = plans[2].dpb.stored.expect("stored");
        let named = current_set_ids(&plans[2]);
        let existing = slots
            .held()
            .map(|(_, id)| id)
            .find(|id| *id != setup && !named.contains(id));
        let foll = match existing {
            Some(id) => id,
            None => {
                let id = 9_999;
                slots
                    .assign(id)
                    .expect("a free slot for the synthetic entry");
                id
            }
        };
        plans[2].dpb_refs.push(RefPic {
            id: foll,
            pic_order_cnt: -4242,
            is_long_term: true,
        });
        let dxva = plan_to_dxva_h265(&plans[2], &mut slots, 3).expect("convert");

        assert_eq!(dxva.refs.len(), current_sets + 1, "appended, not merged");
        let appended = &dxva.refs[current_sets];
        assert_eq!(appended.id, foll);
        assert!(appended.is_long_term);
        assert_eq!(appended.pic_order_cnt, -4242);
        // …and it reaches the wire as a long-term entry with its own POC.
        assert_eq!(
            dxva.pic_params.RefPicList[current_sets],
            PicEntry::new(appended.slot, true)
        );
        assert_eq!(dxva.pic_params.PicOrderCntValList[current_sets], -4242);
        // The index arrays are untouched by the append: every live index still
        // points inside the current sets, which is the property that makes the
        // whole-DPB `RefPicList` a safe superset.
        for array in [
            dxva.pic_params.RefPicSetStCurrBefore,
            dxva.pic_params.RefPicSetStCurrAfter,
            dxva.pic_params.RefPicSetLtCurr,
        ] {
            for entry in array {
                assert!(
                    entry == UNUSED_ENTRY || usize::from(entry) < current_sets,
                    "an index array reached the appended entry"
                );
            }
        }
    }

    #[test]
    fn unused_ref_pic_list_entries_are_the_sentinel_with_a_zero_poc() {
        for (_, dxva) in convert_stream(TEST_25FPS) {
            for i in dxva.refs.len()..REF_PIC_LIST_LEN {
                assert_eq!(dxva.pic_params.RefPicList[i], PicEntry::UNUSED);
                assert_eq!(dxva.pic_params.PicOrderCntValList[i], 0);
            }
        }
    }

    /// HEVC's freedom from the aliasing that cost AV1 and H.264 a deferral, and the
    /// PLANNER property that grants it.
    ///
    /// Both other codecs release a removed picture's slot and then let
    /// [`SlotMap::assign`] hand it straight back to the decode target, so `CurrPic`
    /// and a `RefPicList` entry name one surface. This conversion still releases its
    /// whole `removed` list inline, and is safe doing so for one reason: `H265Planner`
    /// snapshots `dpb_refs` AFTER `decode_rps` has updated the DPB, so a picture this
    /// AU's RPS dropped is never in the set `RefPicList` is built from, and nothing
    /// later in the AU unmarks anything. `H264Planner` and `Av1Planner` both snapshot
    /// BEFORE their marking, and both needed the deferral.
    ///
    /// The second assertion is that argument made falsifiable. The first is only the
    /// consequence, and on this vector the consequence would hold even if the argument
    /// stopped being true — the vendored H.264 vector taught that lesson expensively
    /// (it measured zero aliasing for two milestones while every stream we ship
    /// aliased on 99% of its frames). Moving `dpb_snapshot()` above `decode_rps` would
    /// leave the first assertion passing and break the second on the first AU whose
    /// RPS drops a picture, which on this vector is most of them.
    ///
    /// ⚠ No low-delay HEVC stream is vendored, so unlike H.264 this is not backed by a
    /// hardware leg on our own encoder's output. It was MEASURED once, 2026-08-07, on
    /// a 300-picture 1080p low-delay HEVC stream from a punktfunk host: 0 access units
    /// with a `removed ∩ dpb_refs` intersection, against 297 of 300 for H.264 from the
    /// same host and the same run. Vendoring that stream is the way to make this a
    /// standing guarantee rather than a re-derivable argument.
    #[test]
    fn the_current_picture_is_named_by_curr_pic_and_never_aliases_a_reference() {
        let mut aus_with_removals = 0usize;
        let mut both = 0usize;
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            assert!(!dxva.pic_params.CurrPic.associated());
            assert_eq!(
                dxva.pic_params.CurrPicOrderCntVal,
                plan.picture.pic_order_cnt
            );
            for r in &dxva.refs {
                assert_ne!(r.slot, dxva.setup_slot, "a reference aliases the target");
            }
            if !plan.dpb.removed.is_empty() {
                aus_with_removals += 1;
            }
            both += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
        }
        assert!(
            aus_with_removals > 0,
            "no AU of this vector removed anything, so the zero below would be empty \
             for a reason that has nothing to do with the property being asserted"
        );
        assert_eq!(
            both, 0,
            "{both} picture(s) are in an AU's own reference set AND removed by it. \
             That is the H.264/AV1 aliasing precondition, and HEVC is supposed to be \
             structurally incapable of it — so the snapshot in `H265Planner` has moved \
             ahead of `decode_rps`. Restore the ordering, or give this conversion the \
             `release_after_decode` deferral the other two carry; do NOT relax this \
             number"
        );
    }

    #[test]
    fn the_irap_picture_sets_all_three_picture_type_flags_and_the_others_set_none() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, first) = &converted[0];
        assert!(plan.picture.is_irap && plan.picture.is_idr);
        let flags = first.pic_params.dwCodingSettingPicturePropertyFlags;
        assert_ne!(flags & (1 << 16), 0, "IrapPicFlag");
        assert_ne!(flags & (1 << 17), 0, "IdrPicFlag");
        assert_ne!(flags & (1 << 18), 0, "IntraPicFlag");
        assert!(first.refs.is_empty());

        let (plan, second) = &converted[1];
        assert!(!plan.picture.is_irap);
        let flags = second.pic_params.dwCodingSettingPicturePropertyFlags;
        assert_eq!(flags & (0b111 << 16), 0);
        assert!(!second.refs.is_empty());
    }

    #[test]
    fn the_picture_parameters_carry_the_active_sps_and_pps_verbatim() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        let pp = &dxva.pic_params;
        let sps = &plan.sps;
        let pps = &plan.pps;
        assert_eq!(
            u32::from(pp.PicWidthInMinCbsY) << sps.min_cb_log2_size_y,
            u32::from(sps.pic_width_in_luma_samples)
        );
        assert_eq!(
            u32::from(pp.PicHeightInMinCbsY) << sps.min_cb_log2_size_y,
            u32::from(sps.pic_height_in_luma_samples)
        );
        assert_eq!(
            pp.log2_min_luma_coding_block_size_minus3,
            sps.log2_min_luma_coding_block_size_minus3
        );
        assert_eq!(
            pp.log2_diff_max_min_luma_coding_block_size,
            sps.log2_diff_max_min_luma_coding_block_size
        );
        assert_eq!(
            pp.log2_min_transform_block_size_minus2,
            sps.log2_min_luma_transform_block_size_minus2
        );
        assert_eq!(
            pp.log2_diff_max_min_transform_block_size,
            sps.log2_diff_max_min_luma_transform_block_size
        );
        assert_eq!(
            pp.max_transform_hierarchy_depth_inter,
            sps.max_transform_hierarchy_depth_inter
        );
        assert_eq!(
            pp.max_transform_hierarchy_depth_intra,
            sps.max_transform_hierarchy_depth_intra
        );
        assert_eq!(
            pp.num_short_term_ref_pic_sets,
            sps.num_short_term_ref_pic_sets
        );
        assert_eq!(
            pp.num_long_term_ref_pics_sps,
            sps.num_long_term_ref_pics_sps
        );
        assert_eq!(pp.init_qp_minus26, pps.init_qp_minus26);
        assert_eq!(pp.pps_cb_qp_offset, pps.cb_qp_offset);
        assert_eq!(pp.pps_cr_qp_offset, pps.cr_qp_offset);
        assert_eq!(pp.pps_beta_offset_div2, pps.beta_offset_div2);
        assert_eq!(pp.pps_tc_offset_div2, pps.tc_offset_div2);
        assert_eq!(
            pp.log2_parallel_merge_level_minus2,
            pps.log2_parallel_merge_level_minus2
        );
        assert_eq!(
            pp.sps_max_dec_pic_buffering_minus1,
            sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)]
        );
        assert_eq!(pp.StatusReportFeedbackNumber, 1);
        // Reserved fields stay zero, as both the spec and every driver expect.
        assert_eq!(pp.ReservedBits2, 0);
        assert_eq!(pp.ReservedBits5, 0);
        assert_eq!(pp.ReservedBits6, 0);
        assert_eq!(pp.ReservedBits7, 0);
    }

    #[test]
    fn the_format_word_carries_the_streams_chroma_format_and_bit_depths() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        let expected = HevcFormatFlags {
            chroma_format_idc: plan.picture.chroma_format_idc,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
            log2_max_pic_order_cnt_lsb_minus4: plan.sps.log2_max_pic_order_cnt_lsb_minus4,
        }
        .pack();
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags, expected);
        // 8-bit 4:2:0: chroma_format_idc 1 at bit 0, both depths zero.
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags & 0x3, 1);
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags >> 3 & 0x7, 0);
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags >> 6 & 0x7, 0);
    }

    /// H.265 Table 7-6's default 8x8 list for INTRA prediction (matrixId 0..2), in
    /// the up-right diagonal order both the coded syntax and DXVA use.
    ///
    /// Transcribed from the specification rather than imported from the vendored
    /// parser's own constant: a test that reads the same array the code reads
    /// cannot tell "the defaults" from "whatever that array happens to hold", and
    /// the shape this guards — an enabled sequence that codes no list anywhere — is
    /// exactly the one where a wrong default is a picture drifting to flat grey.
    const DEFAULT_INTRA_8X8: [u8; 64] = [
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19,
        20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35,
        35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
    ];
    /// Table 7-6's default 8x8 list for INTER prediction (matrixId 3..5).
    const DEFAULT_INTER_8X8: [u8; 64] = [
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20,
        20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28,
        28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
    ];

    /// The vector's first plan with its parameter sets rewritten to a chosen
    /// scaling-list shape.
    ///
    /// The parameter sets are the REAL ones the parser produced and only the
    /// scaling-list fields move, which is what makes the "coded nowhere" shape
    /// meaningful: `pps.scaling_list` then still holds the parser's own default
    /// fill, and `sps.scaling_list` still holds the all-zero `ScalingLists::
    /// default()` an uncoded SPS is left with. Synthesising a whole HEVC parameter
    /// set would only put those two facts back by hand.
    fn plan_with_scaling_lists(
        enabled: bool,
        sps_coded: Option<u8>,
        pps_coded: Option<u8>,
    ) -> (AuPlan, DecodePlanDxvaH265) {
        let mut planner = H265Planner::new();
        let aus = split_into_aus(TEST_25FPS);
        let mut plan = planner.plan_au(aus[0]).expect("plan");

        let mut sps = (*plan.sps).clone();
        sps.scaling_list_enabled_flag = enabled;
        sps.scaling_list_data_present_flag = sps_coded.is_some();
        if let Some(fill) = sps_coded {
            sps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
            sps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
            sps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
        }
        let mut pps = (*plan.pps).clone();
        pps.scaling_list_data_present_flag = pps_coded.is_some();
        if let Some(fill) = pps_coded {
            pps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
            pps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
            pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
        }
        plan.sps = std::rc::Rc::new(sps);
        plan.pps = std::rc::Rc::new(pps);

        let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
        let dxva = plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
        (plan, dxva)
    }

    #[test]
    fn a_sequence_that_disables_scaling_lists_submits_no_quantization_matrix_at_all() {
        // The shape every punktfunk HEVC stream is in, and the vendored vector
        // with it. libavcodec's `dxva2_hevc_end_frame` passes the buffer only when
        // `dwCodingParamToolFlags & 1`; handing a driver a matrix it was told to
        // ignore is a bet on the driver ignoring it.
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        assert!(!plan.sps.scaling_list_enabled_flag);
        assert_eq!(dxva.pic_params.dwCodingParamToolFlags & 1, 0);
        assert_eq!(dxva.qmatrix, None);
        for (_, dxva) in &converted {
            assert_eq!(dxva.qmatrix, None);
        }
    }

    #[test]
    fn an_enabled_sequence_that_codes_no_list_anywhere_gets_the_table_7_5_and_7_6_defaults() {
        // The bug this guards: the vendored parser leaves an SPS that codes no
        // scaling list data with an ALL-ZERO `ScalingLists` (unlike libavcodec's,
        // which seeds the defaults), so selecting the SPS here would dequantize
        // every residual to nothing while bit 0 of dwCodingParamToolFlags tells the
        // driver the matrix is authoritative.
        let (plan, dxva) = plan_with_scaling_lists(true, None, None);
        assert!(plan.sps.scaling_list.scaling_list_4x4[0]
            .iter()
            .all(|&v| v == 0));
        assert_eq!(dxva.pic_params.dwCodingParamToolFlags & 1, 1);
        let qm = dxva
            .qmatrix
            .expect("an enabled sequence submits the matrix");

        // Table 7-5: every 4x4 list is flat 16.
        for list in qm.ucScalingLists0 {
            assert_eq!(list, [16u8; 16]);
        }
        // Table 7-6: matrixId 0..2 are the intra default, 3..5 the inter one, at
        // every size from 8x8 up.
        for (m, list) in qm.ucScalingLists1.iter().enumerate() {
            let want = if m < 3 {
                DEFAULT_INTRA_8X8
            } else {
                DEFAULT_INTER_8X8
            };
            assert_eq!(*list, want, "8x8 matrixId {m}");
        }
        for (m, list) in qm.ucScalingLists2.iter().enumerate() {
            let want = if m < 3 {
                DEFAULT_INTRA_8X8
            } else {
                DEFAULT_INTER_8X8
            };
            assert_eq!(*list, want, "16x16 matrixId {m}");
        }
        // sizeId 3 carries only matrixId 0 (intra) and 3 (inter).
        assert_eq!(qm.ucScalingLists3[0], DEFAULT_INTRA_8X8);
        assert_eq!(qm.ucScalingLists3[1], DEFAULT_INTER_8X8);
        // The inferred DC is 8, and DXVA takes the VALUE — 8 + 8.
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [16u8; 6]);
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [16u8; 2]);
    }

    #[test]
    fn a_coded_pps_list_wins_and_a_coded_sps_list_is_taken_only_when_the_pps_codes_none() {
        // 7.4.5's activation order, with a distinct fill per source so the
        // selection is visible rather than inferred.
        let (_, pps_wins) = plan_with_scaling_lists(true, Some(7), Some(9));
        let qm = pps_wins.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists0[0], [9u8; 16]);
        assert_eq!(qm.ucScalingLists1[2], [9u8; 64]);
        assert_eq!(qm.ucScalingLists2[5], [9u8; 64]);
        assert_eq!(qm.ucScalingLists3[0], [9u8; 64]);
        assert_eq!(qm.ucScalingLists3[1], [9u8; 64]);
        // The DC entries are the coded delta plus 8, and sizeId 3 takes the
        // parser's matrixId 0 and 3 rather than 0 and 1.
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [17u8; 6]);
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [17u8; 2]);

        let (_, sps_wins) = plan_with_scaling_lists(true, Some(7), None);
        let qm = sps_wins.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists0[0], [7u8; 16]);
        assert_eq!(qm.ucScalingLists1[2], [7u8; 64]);
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [15u8; 6]);

        // …and neither source reaches the driver when the sequence disables
        // scaling lists, however much data the parameter sets carry.
        let (_, disabled) = plan_with_scaling_lists(false, Some(7), Some(9));
        assert_eq!(disabled.qmatrix, None);
    }

    #[test]
    fn the_sizeid_3_entries_take_the_parsers_matrix_ids_0_and_3() {
        // The index arithmetic (`k * 3`) that would otherwise be invisible: with
        // matrixId 0 and 3 given different values, taking 0 and 1 reads the wrong
        // matrix for the inter slot.
        let (_, dxva) = {
            let mut planner = H265Planner::new();
            let aus = split_into_aus(TEST_25FPS);
            let mut plan = planner.plan_au(aus[0]).expect("plan");
            let mut sps = (*plan.sps).clone();
            sps.scaling_list_enabled_flag = true;
            let mut pps = (*plan.pps).clone();
            pps.scaling_list_data_present_flag = true;
            let mut lists = [[0u8; 64]; 6];
            let mut dc = [0i16; 6];
            for (m, list) in lists.iter_mut().enumerate() {
                *list = [(m as u8 + 1) * 10; 64];
                dc[m] = m as i16 + 1;
            }
            pps.scaling_list.scaling_list_32x32 = lists;
            pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = dc;
            plan.sps = std::rc::Rc::new(sps);
            plan.pps = std::rc::Rc::new(pps);
            let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
            let dxva = plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
            (plan, dxva)
        };
        let qm = dxva.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists3[0], [10u8; 64], "matrixId 0");
        assert_eq!(qm.ucScalingLists3[1], [40u8; 64], "matrixId 3");
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [1 + 8, 4 + 8]);
    }

    #[test]
    fn the_b_frame_vector_converts_and_binds_both_directions_of_its_rps() {
        let converted = convert_stream(TEST_64X64_I_P_B_P);
        assert!(!converted.is_empty());
        // The B picture of I-P-B-P has references on both sides, so at least one
        // AU must populate both StCurrBefore and StCurrAfter.
        let both = converted.iter().any(|(plan, _)| {
            !plan.rps.st_curr_before.is_empty() && !plan.rps.st_curr_after.is_empty()
        });
        assert!(both, "the I-P-B-P vector must exercise bidirectional RPS");
        for (plan, dxva) in &converted {
            // Everything the slices name is bound; that is the cross-check the
            // conversion enforces, asserted here from the outside.
            for slice in &plan.slices {
                for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
                    assert!(dxva.refs.iter().any(|r| r.id == rp.id));
                }
            }
        }
    }

    #[test]
    fn slice_ranges_ride_through_in_plan_order_on_start_code_boundaries() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        for (i, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("plan");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("convert");
            assert_eq!(dxva.slice_ranges.len(), plan.slices.len());
            for range in &dxva.slice_ranges {
                let at = &au[range.start..];
                assert!(at.starts_with(&[0, 0, 1]) || at.starts_with(&[0, 0, 0, 1]));
            }
        }
    }

    #[test]
    fn a_capacity_mismatch_is_refused_and_leaves_the_map_untouched() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let plan = planner.plan_au(aus[0]).expect("plan");
        let mut slots = SlotMap::new(plan.picture.max_dpb_frames + 1);
        assert_eq!(
            plan_to_dxva_h265(&plan, &mut slots, 1),
            Err(PlanToDxvaH265Error::CapacityMismatch {
                required: plan.picture.max_dpb_frames + 1,
                capacity: plan.picture.max_dpb_frames + 2,
            })
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn a_reference_the_map_never_saw_is_refused_and_leaves_the_map_untouched() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let second = planner.plan_au(aus[1]).expect("plan 1");
        let mut slots = SlotMap::new(second.picture.max_dpb_frames);
        let missing = second.rps.st_curr_before[0].id;
        assert_eq!(first.dpb.stored, Some(missing));
        assert_eq!(
            plan_to_dxva_h265(&second, &mut slots, 1),
            Err(PlanToDxvaH265Error::UnresolvedReference(missing))
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn slice_control_records_carry_the_packers_locations_verbatim() {
        let records = [
            crate::pack::SliceRecord {
                location: 0,
                bytes: 128,
            },
            crate::pack::SliceRecord {
                location: 128,
                bytes: 256,
            },
        ];
        let control = slice_control_h265(&records);
        // Read by VALUE, in braces: the record is `#[repr(C, packed)]` (ten bytes),
        // so a reference to a `u32` member would be unaligned — and `assert_eq!`
        // takes references. See `dxva.rs`'s alignment section.
        assert_eq!({ control[0].BSNALunitDataLocation }, 0);
        assert_eq!({ control[0].SliceBytesInBuffer }, 128);
        assert_eq!({ control[0].wBadSliceChopping }, 0);
        // A SECOND record, because the ten-vs-twelve byte defect is invisible on a
        // single-record buffer — the vendored HEVC vector is one slice segment per
        // picture, which is precisely the shape that hid it.
        assert_eq!({ control[1].BSNALunitDataLocation }, 128);
        let bytes = crate::dxva::slice_bytes(&control);
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[10..14], &128u32.to_le_bytes());
        assert_eq!(&bytes[14..18], &256u32.to_le_bytes());
    }

    #[test]
    fn a_slot_is_reused_only_after_its_picture_leaves_the_dpb() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut live: Vec<(PicId, u8)> = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let removed = plan.dpb.removed.clone();
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("convert");
            live.retain(|&(id, _)| !removed.contains(&id));
            assert!(
                live.iter().all(|&(_, slot)| slot != dxva.setup_slot),
                "AU {i} decodes into a surface a live picture still holds"
            );
            live.push((dxva.setup_id, dxva.setup_slot));
        }
    }
}
