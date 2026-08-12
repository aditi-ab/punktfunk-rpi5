//! Parameter-set conversion: the vendored parser's [`Sps`]/[`Pps`] into the
//! `StdVideoH264*ParameterSet` structs a Vulkan Video session-parameters object is
//! created from (WP-B's `vkCreateVideoSessionParametersKHR`).
//!
//! The Std structs embed raw pointers (`pOffsetForRefFrame`, `pScalingLists`,
//! `pSequenceParameterSetVui`), so conversion returns OWNING wrappers instead of bare
//! structs — see [`OwnedStdSps`] for the aliasing/lifetime contract.
//!
//! VUI is deliberately not converted: a DECODE session consumes no VUI (it shapes
//! display, not reconstruction), so `vui_parameters_present_flag` stays 0 and
//! `pSequenceParameterSetVui` stays null. Colour handling rides
//! [`pf_bitstream::h264::PicturePlan`] into the presenter instead, exactly as the
//! FFmpeg-based path did.

use ash::vk::native as hh;
use cros_codecs::codec::h264::parser::Level;
pub use cros_codecs::codec::h264::parser::Pps;
pub use cros_codecs::codec::h264::parser::Sps;

/// A parameter set that cannot be represented as a StdVideo struct. All of these are
/// outside the punktfunk decode envelope (8-bit 4:2:0 streams from encoders we
/// control), so hitting one is a stream-integrity failure, not a feature gap —
/// reject-with-error rather than submit a half-truth to a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsError {
    /// `profile_idc` has no `StdVideoH264ProfileIdc` code point. Vulkan defines
    /// Baseline (66), Main (77), High (100) and High 4:4:4 Predictive (244);
    /// Extended/High10/High422 land here.
    UnmappableProfileIdc(u8),
    /// `chroma_format_idc` past 3 — not legal H.264 to begin with.
    InvalidChromaFormatIdc(u8),
    /// `pic_order_cnt_type` past 2 — not legal H.264 to begin with.
    InvalidPocType(u8),
    /// `weighted_bipred_idc` of 3: representable in the two-bit field, invalid per
    /// 7.4.2.2, and no `StdVideoH264WeightedBipredIdc` code point exists for it.
    InvalidWeightedBipredIdc(u8),
    /// FMO (`num_slice_groups_minus1 > 0`): `StdVideoH264PictureParameterSet` has no
    /// slice-group fields at all — Vulkan Video cannot express it.
    SliceGroups(u32),
}

impl std::fmt::Display for ParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamsError::UnmappableProfileIdc(idc) => {
                write!(
                    f,
                    "profile_idc {idc} has no StdVideoH264ProfileIdc code point"
                )
            }
            ParamsError::InvalidChromaFormatIdc(idc) => {
                write!(f, "invalid chroma_format_idc {idc}")
            }
            ParamsError::InvalidPocType(t) => write!(f, "invalid pic_order_cnt_type {t}"),
            ParamsError::InvalidWeightedBipredIdc(idc) => {
                write!(f, "invalid weighted_bipred_idc {idc}")
            }
            ParamsError::SliceGroups(n) => {
                write!(
                    f,
                    "FMO ({} slice groups) is not expressible in Vulkan Video",
                    n + 1
                )
            }
        }
    }
}

impl std::error::Error for ParamsError {}

/// The converted SPS plus the heap allocations its embedded pointers target.
///
/// `StdVideoH264SequenceParameterSet` points at data it does not contain: the
/// POC-type-1 offset array (`pOffsetForRefFrame`) and the scaling lists
/// (`pScalingLists`). This wrapper owns that data, and the ownership design is the
/// contract WP-B builds on:
///
/// - The backing is boxed, so the wrapper may be MOVED freely: moving it relocates
///   the `Box` handles (pointer values), never the heap blocks the Std struct's
///   pointers hold the addresses of. ⚠ The Std struct ITSELF is boxed for the same
///   reason and it is not decoration: `pStdSPSs` — the OUTER pointer the create/add
///   info carries — is [`Self::std`]'s address, and the session hands it over
///   BEFORE moving the wrapper into its stored parameters. Inline, that address
///   would be a moved-from slot; boxed, it is the one the object keeps
///   (`crate::session`'s `an_added_set_keeps_the_address_the_update_call_was_given`).
///   A driver retaining the outer pointer rather than an inner one would otherwise
///   reproduce the AV1 use-after-free exactly, with the same silent signature.
/// - [`Self::std`] hands the struct out by shared reference. The struct is `Copy`; a
///   copy taken out of the wrapper still points INTO the wrapper's backing and must
///   not outlive it. ⚠⚠ The obligation is NOT merely "keep the wrapper alive across
///   `vkCreateVideoSessionParametersKHR`", which is what this said and what the
///   spec reads like: NVIDIA 610.57.04 was measured retaining an embedded pointer
///   out of a Std set and dereferencing it at every `vkCmdDecodeVideoKHR`
///   ([`crate::session_av1`]). The wrapper must outlive the parameters OBJECT, and
///   [`crate::session`] is where that is enforced by construction.
/// - Nothing exposes mutation of the backing, so for the wrapper's lifetime the
///   pointed-to data is immutable and the `*const` aliasing rules hold trivially.
/// - Deliberately NOT `Clone`: a derived clone would duplicate the pointer VALUES but
///   not the backing, silently tying the clone's validity to the original's lifetime.
///   Re-convert from the `Sps` instead — conversion is cheap and pure.
#[derive(Debug)]
pub struct OwnedStdSps {
    /// Boxed so [`Self::std`]'s ADDRESS — what `pStdSPSs` points at — survives every
    /// move of the wrapper (type-level contract).
    std: Box<hh::StdVideoH264SequenceParameterSet>,
    /// `pOffsetForRefFrame`'s target (POC type 1 only, else `None`/null).
    _offset_backing: Option<Box<[i32]>>,
    /// `pScalingLists`' target (`seq_scaling_matrix_present_flag` only, else null).
    _scaling_backing: Option<Box<hh::StdVideoH264ScalingLists>>,
}

impl OwnedStdSps {
    /// The Std struct, valid for as long as `self` lives (see the type-level
    /// contract; do not let a `Copy` of it outlive the wrapper).
    pub fn std(&self) -> &hh::StdVideoH264SequenceParameterSet {
        &self.std
    }

    /// Lower `level_idc` to `max` when the stream declares a higher one. The
    /// declared level is a claim encoders over-state in the wild, and a set above
    /// the device's `maxLevelIdc` is invalid usage; the stream's real demands are
    /// enforced by the session's coded extent and DPB depth. The "no mutation"
    /// contract above is about a LIVE object's blocks — this runs before handover.
    pub(crate) fn clamp_level(&mut self, max: hh::StdVideoH264LevelIdc) {
        if self.std.level_idc > max {
            self.std.level_idc = max;
        }
    }
}

/// The converted PPS plus the scaling-list allocation its `pScalingLists` targets.
/// Same ownership contract as [`OwnedStdSps`], with the one pointer.
#[derive(Debug)]
pub struct OwnedStdPps {
    /// Boxed for [`OwnedStdSps`]'s reason: `pStdPPSs` is this field's address.
    std: Box<hh::StdVideoH264PictureParameterSet>,
    _scaling_backing: Option<Box<hh::StdVideoH264ScalingLists>>,
}

impl OwnedStdPps {
    /// The Std struct, valid for as long as `self` lives (see [`OwnedStdSps`]).
    pub fn std(&self) -> &hh::StdVideoH264PictureParameterSet {
        &self.std
    }
}

/// H.264 `level_idc` (value-coded: 10 ⇒ 1.0) to Vulkan's index-coded
/// `StdVideoH264LevelIdc`. The Std code points ascend with the level, so the
/// decoder's `maxLevelIdc` gate compares them numerically.
pub(crate) const fn level_to_std(level: Level) -> hh::StdVideoH264LevelIdc {
    match level {
        Level::L1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        // Vulkan has no 1b code point. 1b is signalled on the wire as level_idc 11
        // plus constraint_set3_flag — the flag is mapped, so 1.1 is the faithful cap.
        Level::L1B => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        Level::L1_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        Level::L1_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_2,
        Level::L1_3 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_3,
        Level::L2_0 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_0,
        Level::L2_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_1,
        Level::L2_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_2,
        Level::L3 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_0,
        Level::L3_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
        Level::L3_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2,
        Level::L4 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
        Level::L4_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1,
        Level::L4_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2,
        Level::L5 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0,
        Level::L5_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
        Level::L5_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2,
        Level::L6 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
        Level::L6_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1,
        Level::L6_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
    }
}

/// Pack the parser's scaling-list arrays into the Std layout.
///
/// The vendored parser has already run 7.3.2.1.1.1 plus the Table 7-2 default and
/// fallback rules, so the arrays hold the fully RESOLVED lists. Every resolved list is
/// therefore declared present verbatim (`scaling_list_present_mask` set,
/// `use_default_scaling_matrix_mask` 0) and the driver applies no further inference —
/// simpler and equivalent to re-encoding which lists came from the bitstream.
///
/// `num_8x8` is the count of 8x8 lists the parser actually resolved: 2 for 4:2:0/4:2:2
/// (Y intra/inter), 6 for 4:4:4, and 0 for a PPS without `transform_8x8_mode_flag`
/// (whose 8x8 arrays are untouched zeros and must not be declared present).
fn scaling_lists_to_std(
    lists_4x4: &[[u8; 16]; 6],
    lists_8x8: &[[u8; 64]; 6],
    num_8x8: u16,
) -> hh::StdVideoH264ScalingLists {
    // SAFETY: StdVideoH264ScalingLists is a plain-C bindgen struct of two u16 masks
    // and two byte arrays; the all-zero bit pattern is a valid value for every field.
    let mut std: hh::StdVideoH264ScalingLists = unsafe { std::mem::zeroed() };
    std.scaling_list_present_mask = 0x3F | (((1u16 << num_8x8) - 1) << 6);
    std.use_default_scaling_matrix_mask = 0;
    std.ScalingList4x4 = *lists_4x4;
    // All six 8x8 arrays are copied even when only two are declared present; the
    // driver ignores entries whose mask bit is clear.
    std.ScalingList8x8 = *lists_8x8;
    std
}

/// Convert one SPS into the Std struct (owning wrapper), mapping every field the
/// H.264 decode profile consumes. VUI is skipped by design (module docs).
pub fn sps_to_std(sps: &Sps) -> Result<OwnedStdSps, ParamsError> {
    // StdVideoH264ProfileIdc code points equal the profile_idc values they name, so
    // recognised ones pass through; everything else has no representation.
    let profile_idc = match u32::from(sps.profile_idc) {
        p @ (66 | 77 | 100 | 244) => p,
        _ => return Err(ParamsError::UnmappableProfileIdc(sps.profile_idc)),
    };
    if sps.chroma_format_idc > 3 {
        return Err(ParamsError::InvalidChromaFormatIdc(sps.chroma_format_idc));
    }
    if sps.pic_order_cnt_type > 2 {
        return Err(ParamsError::InvalidPocType(sps.pic_order_cnt_type));
    }

    // SAFETY: StdVideoH264SequenceParameterSet is a plain-C bindgen struct of
    // integers, a bitfield word and const pointers; all-zero is a valid value for
    // every field (null for the pointers) and is the "everything absent" baseline the
    // field writes below build on. Same idiom as pf-encode's vk_build.rs.
    let mut std: hh::StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_constraint_set0_flag(u32::from(sps.constraint_set0_flag));
    std.flags
        .set_constraint_set1_flag(u32::from(sps.constraint_set1_flag));
    std.flags
        .set_constraint_set2_flag(u32::from(sps.constraint_set2_flag));
    std.flags
        .set_constraint_set3_flag(u32::from(sps.constraint_set3_flag));
    std.flags
        .set_constraint_set4_flag(u32::from(sps.constraint_set4_flag));
    std.flags
        .set_constraint_set5_flag(u32::from(sps.constraint_set5_flag));
    std.flags
        .set_direct_8x8_inference_flag(u32::from(sps.direct_8x8_inference_flag));
    std.flags
        .set_mb_adaptive_frame_field_flag(u32::from(sps.mb_adaptive_frame_field_flag));
    // 1 under the punktfunk envelope (pf-bitstream rejects interlaced SPSes), but the
    // conversion itself is faithful, not envelope-coupled.
    std.flags
        .set_frame_mbs_only_flag(u32::from(sps.frame_mbs_only_flag));
    std.flags
        .set_delta_pic_order_always_zero_flag(u32::from(sps.delta_pic_order_always_zero_flag));
    std.flags
        .set_separate_colour_plane_flag(u32::from(sps.separate_colour_plane_flag));
    std.flags
        .set_gaps_in_frame_num_value_allowed_flag(u32::from(
            sps.gaps_in_frame_num_value_allowed_flag,
        ));
    std.flags
        .set_qpprime_y_zero_transform_bypass_flag(u32::from(
            sps.qpprime_y_zero_transform_bypass_flag,
        ));
    std.flags
        .set_frame_cropping_flag(u32::from(sps.frame_cropping_flag));
    std.flags
        .set_seq_scaling_matrix_present_flag(u32::from(sps.seq_scaling_matrix_present_flag));
    // vui_parameters_present_flag stays 0: decode sessions consume no VUI (module docs).

    std.profile_idc = profile_idc;
    std.level_idc = level_to_std(sps.level_idc);
    // Chroma format code points equal the chroma_format_idc values (0..3).
    std.chroma_format_idc = u32::from(sps.chroma_format_idc);
    std.seq_parameter_set_id = sps.seq_parameter_set_id;
    std.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
    std.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
    std.log2_max_frame_num_minus4 = sps.log2_max_frame_num_minus4;
    // POC type code points equal the pic_order_cnt_type values (0..2).
    std.pic_order_cnt_type = u32::from(sps.pic_order_cnt_type);
    std.offset_for_non_ref_pic = sps.offset_for_non_ref_pic;
    std.offset_for_top_to_bottom_field = sps.offset_for_top_to_bottom_field;
    std.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
    std.max_num_ref_frames = sps.max_num_ref_frames;
    std.pic_width_in_mbs_minus1 = u32::from(sps.pic_width_in_mbs_minus1);
    std.pic_height_in_map_units_minus1 = u32::from(sps.pic_height_in_map_units_minus1);
    std.frame_crop_left_offset = sps.frame_crop_left_offset;
    std.frame_crop_right_offset = sps.frame_crop_right_offset;
    std.frame_crop_top_offset = sps.frame_crop_top_offset;
    std.frame_crop_bottom_offset = sps.frame_crop_bottom_offset;

    // POC type 1's offset array: exactly num_ref_frames_in_pic_order_cnt_cycle
    // entries, boxed so the pointer survives moves of the wrapper. The COUNT field
    // and the pointer derive from this one condition so they can never disagree — a
    // stale cycle count on a type-0/2 SPS must not become a nonzero count over a
    // null array (both stay zeroed instead).
    let offset_backing =
        (sps.pic_order_cnt_type == 1 && sps.num_ref_frames_in_pic_order_cnt_cycle > 0).then(|| {
            let cycle = usize::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
            Box::<[i32]>::from(&sps.offset_for_ref_frame[..cycle])
        });
    if let Some(backing) = &offset_backing {
        std.num_ref_frames_in_pic_order_cnt_cycle = sps.num_ref_frames_in_pic_order_cnt_cycle;
        std.pOffsetForRefFrame = backing.as_ptr();
    }

    let scaling_backing = sps.seq_scaling_matrix_present_flag.then(|| {
        let num_8x8 = if sps.chroma_format_idc == 3 { 6 } else { 2 };
        Box::new(scaling_lists_to_std(
            &sps.scaling_lists_4x4,
            &sps.scaling_lists_8x8,
            num_8x8,
        ))
    });
    if let Some(backing) = &scaling_backing {
        std.pScalingLists = &**backing;
    }

    Ok(OwnedStdSps {
        std: Box::new(std),
        _offset_backing: offset_backing,
        _scaling_backing: scaling_backing,
    })
}

/// Convert one PPS into the Std struct (owning wrapper), mapping every field the
/// H.264 decode profile consumes.
///
/// `num_slice_groups_minus1` has no Std field at all; a PPS carrying FMO is rejected
/// rather than converted into a struct that silently claims there is none.
pub fn pps_to_std(pps: &Pps) -> Result<OwnedStdPps, ParamsError> {
    if pps.num_slice_groups_minus1 != 0 {
        return Err(ParamsError::SliceGroups(pps.num_slice_groups_minus1));
    }
    if pps.weighted_bipred_idc > 2 {
        return Err(ParamsError::InvalidWeightedBipredIdc(
            pps.weighted_bipred_idc,
        ));
    }

    // SAFETY: StdVideoH264PictureParameterSet is a plain-C bindgen struct of
    // integers, a bitfield word and one const pointer; all-zero is a valid value for
    // every field (null for the pointer) and is the baseline the writes below fill.
    let mut std: hh::StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_transform_8x8_mode_flag(u32::from(pps.transform_8x8_mode_flag));
    std.flags
        .set_redundant_pic_cnt_present_flag(u32::from(pps.redundant_pic_cnt_present_flag));
    std.flags
        .set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred_flag));
    std.flags
        .set_deblocking_filter_control_present_flag(u32::from(
            pps.deblocking_filter_control_present_flag,
        ));
    std.flags
        .set_weighted_pred_flag(u32::from(pps.weighted_pred_flag));
    std.flags
        .set_bottom_field_pic_order_in_frame_present_flag(u32::from(
            pps.bottom_field_pic_order_in_frame_present_flag,
        ));
    std.flags
        .set_entropy_coding_mode_flag(u32::from(pps.entropy_coding_mode_flag));
    std.flags
        .set_pic_scaling_matrix_present_flag(u32::from(pps.pic_scaling_matrix_present_flag));

    std.seq_parameter_set_id = pps.seq_parameter_set_id;
    std.pic_parameter_set_id = pps.pic_parameter_set_id;
    std.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    std.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    // Code points equal the weighted_bipred_idc values (0..2), validated above.
    std.weighted_bipred_idc = u32::from(pps.weighted_bipred_idc);
    std.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    std.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    std.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    std.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;

    let scaling_backing = pps.pic_scaling_matrix_present_flag.then(|| {
        // The parser resolves a PPS's 8x8 lists only under transform_8x8_mode_flag
        // (7.3.2.2 reads them only then); without it the arrays are untouched zeros
        // and must not be declared present.
        let num_8x8 = match (pps.transform_8x8_mode_flag, pps.sps.chroma_format_idc == 3) {
            (false, _) => 0,
            (true, false) => 2,
            (true, true) => 6,
        };
        Box::new(scaling_lists_to_std(
            &pps.scaling_lists_4x4,
            &pps.scaling_lists_8x8,
            num_8x8,
        ))
    });
    if let Some(backing) = &scaling_backing {
        std.pScalingLists = &**backing;
    }

    Ok(OwnedStdPps {
        std: Box::new(std),
        _scaling_backing: scaling_backing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An SPS exercising every mapped field with distinct values. The sixteen flags
    /// follow the Std bitfield order and strictly ALTERNATE false/true, so a swap of
    /// any two adjacent flag mappings fails; `vui_parameters_present_flag` is true at
    /// the SOURCE precisely because the conversion must NOT copy it (VUI skip).
    fn full_sps() -> Sps {
        Sps {
            seq_parameter_set_id: 3,
            profile_idc: 100,
            // Flags, in Std bit order 0..15: F T F T F T F T F T F T F T F (T).
            constraint_set1_flag: true,
            constraint_set3_flag: true,
            constraint_set5_flag: true,
            mb_adaptive_frame_field_flag: true,
            delta_pic_order_always_zero_flag: true,
            gaps_in_frame_num_value_allowed_flag: true,
            frame_cropping_flag: true,
            vui_parameters_present_flag: true,
            // constraint_set0/2/4, direct_8x8_inference, frame_mbs_only,
            // separate_colour_plane, qpprime_y_zero_transform_bypass and
            // seq_scaling_matrix_present stay false via ..Default.
            level_idc: Level::L4_1,
            chroma_format_idc: 1,
            bit_depth_luma_minus8: 2,
            bit_depth_chroma_minus8: 3,
            log2_max_frame_num_minus4: 5,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 6,
            offset_for_non_ref_pic: -7,
            offset_for_top_to_bottom_field: 3,
            max_num_ref_frames: 4,
            pic_width_in_mbs_minus1: 119,
            pic_height_in_map_units_minus1: 67,
            frame_crop_left_offset: 1,
            frame_crop_right_offset: 2,
            frame_crop_top_offset: 3,
            frame_crop_bottom_offset: 4,
            ..Default::default()
        }
    }

    /// A PPS over `sps` exercising every mapped field with distinct values. The
    /// eight flags follow the Std bitfield order and strictly ALTERNATE true/false,
    /// so a swap of any two adjacent flag mappings fails.
    fn full_pps(sps: Sps) -> Pps {
        Pps {
            pic_parameter_set_id: 5,
            seq_parameter_set_id: 3,
            // Flags, in Std bit order 0..7: T F T F T F T F.
            transform_8x8_mode_flag: true,
            redundant_pic_cnt_present_flag: false,
            constrained_intra_pred_flag: true,
            deblocking_filter_control_present_flag: false,
            weighted_pred_flag: true,
            bottom_field_pic_order_in_frame_present_flag: false,
            entropy_coding_mode_flag: true,
            pic_scaling_matrix_present_flag: false,
            num_slice_groups_minus1: 0,
            num_ref_idx_l0_default_active_minus1: 2,
            num_ref_idx_l1_default_active_minus1: 1,
            weighted_bipred_idc: 2,
            pic_init_qp_minus26: -3,
            pic_init_qs_minus26: 4,
            chroma_qp_index_offset: -2,
            scaling_lists_4x4: [[0; 16]; 6],
            scaling_lists_8x8: [[0; 64]; 6],
            second_chroma_qp_index_offset: 6,
            sps: std::rc::Rc::new(sps),
        }
    }

    #[test]
    fn every_mapped_sps_field_and_flag_round_trips_exactly() {
        let sps = full_sps();
        let owned = sps_to_std(&sps).unwrap();
        let std = owned.std();

        // The strictly alternating pattern of the fixture, bit for bit.
        assert_eq!(std.flags.constraint_set0_flag(), 0);
        assert_eq!(std.flags.constraint_set1_flag(), 1);
        assert_eq!(std.flags.constraint_set2_flag(), 0);
        assert_eq!(std.flags.constraint_set3_flag(), 1);
        assert_eq!(std.flags.constraint_set4_flag(), 0);
        assert_eq!(std.flags.constraint_set5_flag(), 1);
        assert_eq!(std.flags.direct_8x8_inference_flag(), 0);
        assert_eq!(std.flags.mb_adaptive_frame_field_flag(), 1);
        assert_eq!(std.flags.frame_mbs_only_flag(), 0);
        assert_eq!(std.flags.delta_pic_order_always_zero_flag(), 1);
        assert_eq!(std.flags.separate_colour_plane_flag(), 0);
        assert_eq!(std.flags.gaps_in_frame_num_value_allowed_flag(), 1);
        assert_eq!(std.flags.qpprime_y_zero_transform_bypass_flag(), 0);
        assert_eq!(std.flags.frame_cropping_flag(), 1);
        assert_eq!(std.flags.seq_scaling_matrix_present_flag(), 0);
        assert_eq!(
            std.flags.vui_parameters_present_flag(),
            0,
            "true at the source, skipped by design"
        );

        assert_eq!(
            std.profile_idc,
            hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
        );
        assert_eq!(
            std.level_idc,
            hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1
        );
        assert_eq!(
            std.chroma_format_idc,
            hh::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420
        );
        assert_eq!(std.seq_parameter_set_id, 3);
        assert_eq!(std.bit_depth_luma_minus8, 2);
        assert_eq!(std.bit_depth_chroma_minus8, 3, "distinct from luma");
        assert_eq!(std.log2_max_frame_num_minus4, 5);
        assert_eq!(
            std.pic_order_cnt_type,
            hh::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_0
        );
        assert_eq!(std.offset_for_non_ref_pic, -7);
        assert_eq!(std.offset_for_top_to_bottom_field, 3);
        assert_eq!(std.log2_max_pic_order_cnt_lsb_minus4, 6);
        assert_eq!(std.num_ref_frames_in_pic_order_cnt_cycle, 0);
        assert_eq!(std.max_num_ref_frames, 4);
        assert_eq!(std.pic_width_in_mbs_minus1, 119);
        assert_eq!(std.pic_height_in_map_units_minus1, 67);
        assert_eq!(std.frame_crop_left_offset, 1);
        assert_eq!(std.frame_crop_right_offset, 2);
        assert_eq!(std.frame_crop_top_offset, 3);
        assert_eq!(std.frame_crop_bottom_offset, 4);

        assert!(
            std.pOffsetForRefFrame.is_null(),
            "POC type 0 carries no offset array"
        );
        assert!(std.pScalingLists.is_null());
        assert!(std.pSequenceParameterSetVui.is_null());
    }

    #[test]
    fn poc_type_1_offsets_are_owned_and_survive_moving_the_wrapper() {
        let mut sps = full_sps();
        sps.pic_order_cnt_type = 1;
        sps.num_ref_frames_in_pic_order_cnt_cycle = 3;
        sps.offset_for_ref_frame[0] = 2;
        sps.offset_for_ref_frame[1] = -1;
        sps.offset_for_ref_frame[2] = 4;

        // Box the wrapper AFTER conversion: a move that relocates the wrapper itself
        // must not invalidate the pointer, because the backing is heap-pinned.
        let owned = Box::new(sps_to_std(&sps).unwrap());
        let std = owned.std();
        assert_eq!(
            std.pic_order_cnt_type,
            hh::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_1
        );
        assert_eq!(std.num_ref_frames_in_pic_order_cnt_cycle, 3);
        assert!(!std.pOffsetForRefFrame.is_null());
        // SAFETY: pOffsetForRefFrame points into `owned`'s boxed backing of exactly
        // num_ref_frames_in_pic_order_cnt_cycle i32s, alive for this whole scope.
        let offsets = unsafe { std::slice::from_raw_parts(std.pOffsetForRefFrame, 3) };
        assert_eq!(offsets, [2, -1, 4]);
    }

    /// Scribble over the stack the conversion's frames just used.
    ///
    /// The discriminator in the move tests is READING a block back, and pointer
    /// equality cannot stand in for it: the Std struct carries its pointers by
    /// VALUE, so a stale one is copied along with the struct and still compares
    /// equal. An inlined backing therefore shows up only as wrong CONTENT — and
    /// only if the dead slot has actually been reused by then. This makes that
    /// certain instead of lucky: after it runs, a pointer into a dead local reads
    /// back `0xA5`s rather than, by chance, its old contents.
    #[inline(never)]
    fn clobber_the_dead_stack() {
        let mut scratch = [0xA5u8; 16 * 1024];
        std::hint::black_box(&mut scratch);
    }

    /// Both wrappers may be MOVED — into the session's stored parameters, out of a
    /// `Result`, into a `Vec` that later reallocates — without disturbing the
    /// addresses a driver has already been given.
    ///
    /// Not a Rust triviality worth skipping: it is the whole reason
    /// [`crate::session`] can fix its use-after-free by STORING these values
    /// alongside the parameters object rather than by boxing or pinning them. It
    /// holds because every backing is `Box`ed; an "optimisation" that inlined any
    /// one of them as a field would keep every other test in this crate green, keep
    /// compiling, and hand the driver a pointer into a moved-from stack slot. The
    /// H.264 parity leg would catch it on hardware — this catches it in ordinary CI.
    /// (`params_av1::moving_the_wrapper_leaves_the_driver_s_pointers_put` is the
    /// same test one codec over; `params_h265`'s is the third.)
    #[test]
    fn moving_the_wrapper_leaves_the_driver_s_pointers_put() {
        // An SPS carrying BOTH of its embedded pointers: the POC-type-1 offset
        // array and the scaling lists. (The vendored `Sps` is not `Clone`, so the
        // fixture is a builder rather than a value.)
        let pointer_bearing_sps = || {
            let mut sps = full_sps();
            sps.pic_order_cnt_type = 1;
            sps.num_ref_frames_in_pic_order_cnt_cycle = 3;
            sps.offset_for_ref_frame[0] = 2;
            sps.offset_for_ref_frame[1] = -1;
            sps.offset_for_ref_frame[2] = 4;
            sps.seq_scaling_matrix_present_flag = true;
            sps.scaling_lists_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
            sps
        };
        // And a PPS carrying its one.
        let mut pps = full_pps(pointer_bearing_sps());
        pps.pic_scaling_matrix_present_flag = true;
        pps.scaling_lists_4x4 = std::array::from_fn(|i| [60 + i as u8; 16]);

        let owned_sps = sps_to_std(&pointer_bearing_sps()).expect("a High-profile SPS converts");
        let owned_pps = pps_to_std(&pps).expect("its PPS converts");
        let (offsets, sps_lists) = (
            owned_sps.std().pOffsetForRefFrame,
            owned_sps.std().pScalingLists,
        );
        let pps_lists = owned_pps.std().pScalingLists;
        assert!(!offsets.is_null(), "POC type 1 attaches the offset array");
        assert!(!sps_lists.is_null(), "the SPS declares scaling lists");
        assert!(!pps_lists.is_null(), "so does the PPS");

        // Every move the session's stored parameters put them through: out of the
        // conversion, into a `Vec`, through a reallocation of that `Vec` as later
        // Adds push more sets in, and along with the whole `StoredParams` value as
        // it is installed by `mem::replace`.
        let stored_sps = vec![owned_sps];
        let mut stored_pps = vec![owned_pps];
        for id in 1..crate::session::MAX_STD_PPS as u8 {
            let mut more = full_pps(pointer_bearing_sps());
            more.pic_parameter_set_id = id;
            stored_pps.push(pps_to_std(&more).expect("converts"));
        }
        assert!(
            stored_pps.capacity() > 1,
            "the pushes reallocated, which is the case being pinned"
        );
        let stored = (stored_sps, stored_pps, 0u8);
        let (stored_sps, stored_pps, _) = stored;

        assert_eq!(stored_sps[0].std().pOffsetForRefFrame, offsets);
        assert_eq!(stored_sps[0].std().pScalingLists, sps_lists);
        assert_eq!(stored_pps[0].std().pScalingLists, pps_lists);

        // The assertions that actually bite. Pointer equality above cannot fail —
        // the Std struct carries the value, so a stale pointer is copied along with
        // it — but an inlined backing leaves those pointers addressing dead locals
        // in `sps_to_std`/`pps_to_std`'s returned frames, which this has just
        // overwritten.
        clobber_the_dead_stack();
        // They still address live blocks holding the fixture's own values, not
        // stale copies.
        // SAFETY: `stored_sps`/`stored_pps` are alive here and own every block.
        let (read_offsets, read_sps_lists, read_pps_lists) = unsafe {
            (
                std::slice::from_raw_parts(offsets, 3),
                &*sps_lists,
                &*pps_lists,
            )
        };
        assert_eq!(read_offsets, [2, -1, 4]);
        assert_eq!(read_sps_lists.ScalingList4x4[5], [15; 16]);
        assert_eq!(read_pps_lists.ScalingList4x4[5], [65; 16]);
    }

    #[test]
    fn sps_scaling_lists_convert_when_present_and_stay_absent_when_not() {
        let mut sps = full_sps();
        assert!(sps_to_std(&sps).unwrap().std().pScalingLists.is_null());

        sps.seq_scaling_matrix_present_flag = true;
        // Each list gets a DISTINCT fill byte: a permutation of lists, or an
        // intra/inter reinterleave, cannot pass.
        sps.scaling_lists_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
        sps.scaling_lists_8x8 = std::array::from_fn(|i| [20 + i as u8; 64]);
        let owned = sps_to_std(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.flags.seq_scaling_matrix_present_flag(), 1);
        assert!(!std.pScalingLists.is_null());
        // SAFETY: pScalingLists points at `owned`'s boxed StdVideoH264ScalingLists,
        // alive for this whole scope.
        let lists = unsafe { &*std.pScalingLists };
        // 4:2:0: bits 0-5 (the six 4x4 lists) + bits 6-7 (the two resolved 8x8
        // lists), none deferred to driver-side defaults (the parser already
        // resolved them).
        assert_eq!(lists.scaling_list_present_mask, 0xFF);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
        for i in 0..6 {
            assert_eq!(lists.ScalingList4x4[i], [10 + i as u8; 16], "4x4 list {i}");
            assert_eq!(lists.ScalingList8x8[i], [20 + i as u8; 64], "8x8 list {i}");
        }

        // 4:4:4 resolves all six 8x8 lists.
        sps.chroma_format_idc = 3;
        let owned = sps_to_std(&sps).unwrap();
        // SAFETY: as above — the pointer targets `owned`'s boxed backing.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0xFFF);
    }

    #[test]
    fn every_mapped_pps_field_and_flag_round_trips_exactly() {
        let pps = full_pps(full_sps());
        let owned = pps_to_std(&pps).unwrap();
        let std = owned.std();

        // The strictly alternating pattern of the fixture, bit for bit.
        assert_eq!(std.flags.transform_8x8_mode_flag(), 1);
        assert_eq!(std.flags.redundant_pic_cnt_present_flag(), 0);
        assert_eq!(std.flags.constrained_intra_pred_flag(), 1);
        assert_eq!(std.flags.deblocking_filter_control_present_flag(), 0);
        assert_eq!(std.flags.weighted_pred_flag(), 1);
        assert_eq!(std.flags.bottom_field_pic_order_in_frame_present_flag(), 0);
        assert_eq!(std.flags.entropy_coding_mode_flag(), 1);
        assert_eq!(std.flags.pic_scaling_matrix_present_flag(), 0);

        assert_eq!(std.seq_parameter_set_id, 3);
        assert_eq!(std.pic_parameter_set_id, 5);
        assert_eq!(std.num_ref_idx_l0_default_active_minus1, 2);
        assert_eq!(std.num_ref_idx_l1_default_active_minus1, 1);
        assert_eq!(
            std.weighted_bipred_idc,
            hh::StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_IMPLICIT
        );
        assert_eq!(std.pic_init_qp_minus26, -3);
        assert_eq!(std.pic_init_qs_minus26, 4);
        assert_eq!(std.chroma_qp_index_offset, -2);
        assert_eq!(std.second_chroma_qp_index_offset, 6);
        assert!(std.pScalingLists.is_null());
    }

    #[test]
    fn pps_scaling_lists_declare_8x8_present_only_under_transform_8x8_mode() {
        let mut pps = full_pps(full_sps());
        pps.pic_scaling_matrix_present_flag = true;
        // Distinct fill bytes per list, as in the SPS test.
        pps.scaling_lists_4x4 = std::array::from_fn(|i| [30 + i as u8; 16]);
        pps.scaling_lists_8x8 = std::array::from_fn(|i| [40 + i as u8; 64]);

        let owned = pps_to_std(&pps).unwrap();
        // SAFETY: pScalingLists points at `owned`'s boxed backing, alive here.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0xFF);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
        for i in 0..6 {
            assert_eq!(lists.ScalingList4x4[i], [30 + i as u8; 16], "4x4 list {i}");
            assert_eq!(lists.ScalingList8x8[i], [40 + i as u8; 64], "8x8 list {i}");
        }

        // Without transform_8x8_mode the parser never resolved the 8x8 arrays: only
        // the six 4x4 lists may be declared present.
        pps.transform_8x8_mode_flag = false;
        let owned = pps_to_std(&pps).unwrap();
        // SAFETY: as above — the pointer targets `owned`'s boxed backing.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0x3F);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
    }

    #[test]
    fn a_stale_cycle_count_on_a_type_0_sps_converts_to_zero_offsets() {
        let mut sps = full_sps();
        sps.pic_order_cnt_type = 0;
        // A stale/corrupt count with no POC-type-1 semantics behind it: the Std
        // struct must not claim a cycle over a null array.
        sps.num_ref_frames_in_pic_order_cnt_cycle = 5;
        let owned = sps_to_std(&sps).unwrap();
        assert_eq!(
            owned.std().num_ref_frames_in_pic_order_cnt_cycle,
            0,
            "count and pointer derive from one condition"
        );
        assert!(owned.std().pOffsetForRefFrame.is_null());
    }

    #[test]
    fn the_25fps_vectors_own_parameter_sets_convert_cleanly() {
        use std::io::Cursor;

        use cros_codecs::codec::h264::parser::Nalu;
        use cros_codecs::codec::h264::parser::NaluType;
        use cros_codecs::codec::h264::parser::Parser;

        // The same vendored vector pf-bitstream's tests plan, same relative path.
        const TEST_25FPS: &[u8] = include_bytes!(
            "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
        );

        let mut cursor = Cursor::new(TEST_25FPS);
        let mut parser = Parser::default();
        let (mut sps_seen, mut pps_seen) = (false, false);
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::Sps if !sps_seen => {
                    let sps = parser.parse_sps(&nalu).expect("the vector's SPS parses");
                    let owned = sps_to_std(sps).expect("the vector's SPS converts");
                    let std = owned.std();
                    // The vector's own goldens: 320x240 progressive Main profile.
                    assert_eq!(
                        std.profile_idc,
                        hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN
                    );
                    assert_eq!((std.pic_width_in_mbs_minus1 + 1) * 16, 320);
                    assert_eq!((std.pic_height_in_map_units_minus1 + 1) * 16, 240);
                    assert_eq!(std.flags.frame_mbs_only_flag(), 1);
                    assert_eq!(std.flags.vui_parameters_present_flag(), 0);
                    sps_seen = true;
                }
                NaluType::Pps if !pps_seen => {
                    let pps = parser.parse_pps(&nalu).expect("the vector's PPS parses");
                    let owned = pps_to_std(pps).expect("the vector's PPS converts");
                    assert_eq!(owned.std().pic_parameter_set_id, 0);
                    pps_seen = true;
                }
                _ => {}
            }
            if sps_seen && pps_seen {
                break;
            }
        }
        assert!(sps_seen && pps_seen, "the vector opens with SPS + PPS");
    }

    #[test]
    fn unrepresentable_parameter_sets_are_rejected_not_approximated() {
        let mut sps = full_sps();
        sps.profile_idc = 110; // High10: no StdVideoH264ProfileIdc code point.
        assert_eq!(
            sps_to_std(&sps).unwrap_err(),
            ParamsError::UnmappableProfileIdc(110)
        );

        let mut pps = full_pps(full_sps());
        pps.num_slice_groups_minus1 = 1;
        assert_eq!(pps_to_std(&pps).unwrap_err(), ParamsError::SliceGroups(1));

        let mut pps = full_pps(full_sps());
        pps.weighted_bipred_idc = 3;
        assert_eq!(
            pps_to_std(&pps).unwrap_err(),
            ParamsError::InvalidWeightedBipredIdc(3)
        );
    }

    /// The over-declared-level clamp ([`OwnedStdSps::clamp_level`]): lowering
    /// writes the ceiling into the Std SPS; a ceiling at or above the declared
    /// level changes nothing.
    #[test]
    fn clamp_level_lowers_and_only_lowers() {
        let sps = full_sps();
        let declared = level_to_std(sps.level_idc);

        let mut owned = sps_to_std(&sps).unwrap();
        assert_eq!(owned.std().level_idc, declared);
        // A ceiling above the declared level is a no-op.
        owned.clamp_level(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2);
        assert_eq!(owned.std().level_idc, declared);
        // A ceiling below it is written through.
        let ceiling = hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1;
        assert!(ceiling < declared, "fixture declares above 3.1");
        owned.clamp_level(ceiling);
        assert_eq!(owned.std().level_idc, ceiling);
    }
}
