//! AV1 session parameters: the sequence header, converted to `StdVideoAV1SequenceHeader`.
//!
//! AV1's parameter surface is far smaller than H.264's or H.265's — there is no PPS
//! and no VPS, and `VkVideoDecodeAV1SessionParametersCreateInfoKHR` carries exactly
//! ONE sequence header. Everything else a frame needs (tiles, quantisation,
//! segmentation, loop filter, CDEF, loop restoration, global motion, film grain)
//! rides on the PICTURE info, which is why [`crate::pic_av1`] is the large half of
//! this codec and this module is the small one.
//!
//! Ownership contract as [`crate::OwnedStdSps`]: boxed backing for the two embedded
//! pointers, movable wrapper, no mutation, deliberately not `Clone`.

use ash::vk::native as hh;
use cros_codecs::codec::av1::parser::SequenceHeaderObu;

/// `StdVideoAV1Profile` values (`vk_video/vulkan_video_codec_av1std.h`).
pub const STD_PROFILE_MAIN: hh::StdVideoAV1Profile = 0;
pub const STD_PROFILE_HIGH: hh::StdVideoAV1Profile = 1;
pub const STD_PROFILE_PROFESSIONAL: hh::StdVideoAV1Profile = 2;

/// Why a sequence header cannot be expressed to Vulkan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamsAv1Error {
    /// A profile outside the Std enumeration.
    UnsupportedProfile(u8),
    /// A field wider than the Std struct's type for it.
    FieldOverflow { field: &'static str, value: u32 },
}

impl std::fmt::Display for ParamsAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamsAv1Error::UnsupportedProfile(p) => {
                write!(f, "AV1 seq_profile {p} has no Std enumerator")
            }
            ParamsAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its Std field")
            }
        }
    }
}

impl std::error::Error for ParamsAv1Error {}

/// The converted sequence header plus the heap allocations its pointers target.
#[derive(Debug)]
pub struct OwnedStdAv1SequenceHeader {
    std: hh::StdVideoAV1SequenceHeader,
    _color_backing: Box<hh::StdVideoAV1ColorConfig>,
    /// `pTimingInfo` is null unless the stream carries timing info: a decoder needs
    /// none of it, and a zeroed block behind a non-null pointer would claim a frame
    /// rate the stream never stated.
    _timing_backing: Option<Box<hh::StdVideoAV1TimingInfo>>,
}

impl OwnedStdAv1SequenceHeader {
    /// The Std struct, valid for as long as `self` lives (see [`crate::OwnedStdSps`]).
    pub fn std(&self) -> &hh::StdVideoAV1SequenceHeader {
        &self.std
    }
}

/// Convert one parsed sequence header.
pub fn sequence_to_std(
    seq: &SequenceHeaderObu,
) -> Result<OwnedStdAv1SequenceHeader, ParamsAv1Error> {
    let seq_profile = match seq.seq_profile as u8 {
        0 => STD_PROFILE_MAIN,
        1 => STD_PROFILE_HIGH,
        2 => STD_PROFILE_PROFESSIONAL,
        other => return Err(ParamsAv1Error::UnsupportedProfile(other)),
    };

    let narrow = |field: &'static str, value: i64| -> Result<u8, ParamsAv1Error> {
        u8::try_from(value).map_err(|_| ParamsAv1Error::FieldOverflow {
            field,
            value: value as u32,
        })
    };

    let color = &seq.color_config;
    // SAFETY: StdVideoAV1ColorConfig is a plain-C bindgen struct of a bitfield word,
    // small integers and enum ints; all-zero is a valid value for every field, and
    // every one that matters is assigned below.
    let mut color_std: hh::StdVideoAV1ColorConfig = unsafe { std::mem::zeroed() };
    color_std.flags.set_mono_chrome(color.mono_chrome.into());
    color_std.flags.set_color_range(color.color_range.into());
    color_std
        .flags
        .set_separate_uv_delta_q(color.separate_uv_delta_q.into());
    color_std
        .flags
        .set_color_description_present_flag(color.color_description_present_flag.into());
    color_std.BitDepth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    color_std.subsampling_x = u8::from(color.subsampling_x);
    color_std.subsampling_y = u8::from(color.subsampling_y);
    color_std.color_primaries = color.color_primaries as u32;
    color_std.transfer_characteristics = color.transfer_characteristics as u32;
    color_std.matrix_coefficients = color.matrix_coefficients as u32;
    color_std.chroma_sample_position = color.chroma_sample_position as u32;
    let color_backing = Box::new(color_std);

    let timing_backing = if seq.timing_info_present_flag {
        // SAFETY: as above — a bitfield word and three integers.
        let mut t: hh::StdVideoAV1TimingInfo = unsafe { std::mem::zeroed() };
        t.flags
            .set_equal_picture_interval(seq.timing_info.equal_picture_interval.into());
        t.num_units_in_display_tick = seq.timing_info.num_units_in_display_tick;
        t.time_scale = seq.timing_info.time_scale;
        t.num_ticks_per_picture_minus_1 = seq.timing_info.num_ticks_per_picture_minus_1;
        Some(Box::new(t))
    } else {
        None
    };

    // SAFETY: as above — a bitfield word, integers and two const pointers, both of
    // which are assigned below.
    let mut std: hh::StdVideoAV1SequenceHeader = unsafe { std::mem::zeroed() };
    std.flags.set_still_picture(seq.still_picture.into());
    std.flags
        .set_reduced_still_picture_header(seq.reduced_still_picture_header.into());
    std.flags
        .set_use_128x128_superblock(seq.use_128x128_superblock.into());
    std.flags
        .set_enable_filter_intra(seq.enable_filter_intra.into());
    std.flags
        .set_enable_intra_edge_filter(seq.enable_intra_edge_filter.into());
    std.flags
        .set_enable_interintra_compound(seq.enable_interintra_compound.into());
    std.flags
        .set_enable_masked_compound(seq.enable_masked_compound.into());
    std.flags
        .set_enable_warped_motion(seq.enable_warped_motion.into());
    std.flags
        .set_enable_dual_filter(seq.enable_dual_filter.into());
    std.flags
        .set_enable_order_hint(seq.enable_order_hint.into());
    std.flags.set_enable_jnt_comp(seq.enable_jnt_comp.into());
    std.flags
        .set_enable_ref_frame_mvs(seq.enable_ref_frame_mvs.into());
    std.flags
        .set_frame_id_numbers_present_flag(seq.frame_id_numbers_present_flag.into());
    std.flags.set_enable_superres(seq.enable_superres.into());
    std.flags.set_enable_cdef(seq.enable_cdef.into());
    std.flags
        .set_enable_restoration(seq.enable_restoration.into());
    std.flags
        .set_film_grain_params_present(seq.film_grain_params_present.into());
    std.flags
        .set_timing_info_present_flag(seq.timing_info_present_flag.into());
    std.flags
        .set_initial_display_delay_present_flag(seq.initial_display_delay_present_flag.into());

    std.seq_profile = seq_profile;
    std.frame_width_bits_minus_1 = seq.frame_width_bits_minus_1;
    std.frame_height_bits_minus_1 = seq.frame_height_bits_minus_1;
    std.max_frame_width_minus_1 = seq.max_frame_width_minus_1;
    std.max_frame_height_minus_1 = seq.max_frame_height_minus_1;
    std.delta_frame_id_length_minus_2 = narrow(
        "delta_frame_id_length_minus_2",
        i64::from(seq.delta_frame_id_length_minus_2),
    )?;
    std.additional_frame_id_length_minus_1 = narrow(
        "additional_frame_id_length_minus_1",
        i64::from(seq.additional_frame_id_length_minus_1),
    )?;
    std.order_hint_bits_minus_1 = narrow(
        "order_hint_bits_minus_1",
        i64::from(seq.order_hint_bits_minus_1),
    )?;
    std.seq_force_integer_mv = narrow("seq_force_integer_mv", i64::from(seq.seq_force_integer_mv))?;
    std.seq_force_screen_content_tools = narrow(
        "seq_force_screen_content_tools",
        i64::from(seq.seq_force_screen_content_tools),
    )?;
    std.pColorConfig = &*color_backing;
    std.pTimingInfo = timing_backing
        .as_ref()
        .map_or(std::ptr::null(), |t| &**t as *const _);

    Ok(OwnedStdAv1SequenceHeader {
        std,
        _color_backing: color_backing,
        _timing_backing: timing_backing,
    })
}
