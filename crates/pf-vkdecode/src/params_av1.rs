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
///
/// The last two variants are the ENVELOPE gate rather than the conversion's:
/// [`crate::caps_av1::Av1ProfileKey::from_stream`] builds the Vulkan profile from
/// the same sequence header and has to refuse the sampling/depth combinations this
/// crate has no picture format for. They live here, with the other sequence-header
/// refusals, for the reason `H265ParamsError` carries its own pair — one error type
/// per codec's parameter surface, so a caller matches on one enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamsAv1Error {
    /// A profile outside the Std enumeration.
    UnsupportedProfile(u8),
    /// A field wider than the Std struct's type for it.
    FieldOverflow { field: &'static str, value: u32 },
    /// The sequence's sampling, in H.264's `chroma_format_idc` vocabulary (the
    /// planner's translation): 0 = monochrome, 2 = 4:2:2, 4 = the 4:4:0 shape no
    /// AV1 profile has. None of them has a picture format in this crate.
    UnsupportedChromaFormat(u8),
    /// 12-bit — legal in AV1 Professional, with no output format here.
    UnsupportedBitDepth(u8),
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
            ParamsAv1Error::UnsupportedChromaFormat(c) => {
                write!(f, "AV1 chroma format {c} has no picture format here")
            }
            ParamsAv1Error::UnsupportedBitDepth(d) => {
                write!(f, "{d}-bit AV1 has no picture format here")
            }
        }
    }
}

impl std::error::Error for ParamsAv1Error {}

/// The converted sequence header plus the heap allocations its pointers target.
///
/// ⚠⚠ **This must outlive the `VkVideoSessionParametersKHR` it is handed to, not
/// merely the create call.** A driver in this fleet keeps `pColorConfig` and reads
/// it at every decode; `session_av1::StoredParamsAv1` is where that is enforced and
/// where the measurement lives. Boxed backing (rather than inline arrays) is what
/// makes storing the wrapper enough — moving it does not move the blocks, which
/// `moving_the_wrapper_leaves_the_driver_s_pointers_put` pins.
///
/// ⚠⚠ The Std struct ITSELF is boxed for the same reason, one level out:
/// `pStdSequenceHeader` is [`Self::std`]'s address, and `ensure_parameters` hands
/// it to the create call BEFORE moving the wrapper into the stored parameters.
/// Inline, that address would be a moved-from stack slot the instant the function
/// returned — the original bug's exact shape, differing only in WHICH pointer a
/// driver chose to retain (`session_av1`'s
/// `the_sequence_header_address_the_create_call_is_given_survives_being_stored`).
#[derive(Debug)]
pub struct OwnedStdAv1SequenceHeader {
    std: Box<hh::StdVideoAV1SequenceHeader>,
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
        std: Box::new(std),
        _color_backing: color_backing,
        _timing_backing: timing_backing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper may be MOVED — into the session's stored parameters, out of a
    /// `Result`, into a struct literal — without disturbing the addresses a driver
    /// has already been given.
    ///
    /// Not a Rust triviality worth skipping: it is the whole reason
    /// [`crate::session_av1`] can fix its use-after-free by storing this value
    /// rather than by boxing it or pinning it. It holds because the two blocks are
    /// `Box`ed; an "optimisation" that inlined either one as a field would keep
    /// every other test in this crate green, keep compiling, and hand the driver a
    /// pointer into a moved-from stack slot. The AV1 parity leg would catch it on
    /// hardware — this catches it in ordinary CI.
    #[test]
    fn moving_the_wrapper_leaves_the_driver_s_pointers_put() {
        let seq = SequenceHeaderObu {
            max_frame_width_minus_1: 319,
            max_frame_height_minus_1: 239,
            timing_info_present_flag: true,
            ..Default::default()
        };
        let owned = sequence_to_std(&seq).expect("a plain 8-bit header converts");
        let (colour, timing) = (owned.std().pColorConfig, owned.std().pTimingInfo);
        assert!(!colour.is_null(), "pColorConfig is always attached");
        assert!(!timing.is_null(), "this header states timing info");

        // Every move a session parameters object's creation puts it through.
        let moved = owned;
        let boxed = Box::new(moved);
        let stored = (*boxed, 0u8);
        let owned = stored.0;

        assert_eq!(owned.std().pColorConfig, colour);
        assert_eq!(owned.std().pTimingInfo, timing);
        // And they still address the wrapper's own live blocks, not stale copies.
        // SAFETY: `owned` is alive here and owns both blocks.
        let subsampling = unsafe { ((*colour).subsampling_x, (*colour).subsampling_y) };
        assert_eq!(
            subsampling,
            (0, 0),
            "the fixture's colour config, read back"
        );
    }

    /// A stream without timing info gets a NULL `pTimingInfo`, and that is a
    /// deliberate statement rather than an omission.
    ///
    /// Measured on NVIDIA 610.57.04: with the backing held for the parameters
    /// object's life, all 250 frames of the vendored vector are bit-identical to
    /// libavcodec with this pointer NULL. libavcodec always attaches a zeroed block
    /// instead; both work. Attaching one here would claim a frame rate the stream
    /// never stated, so the null stays — but if a future driver refuses it, this is
    /// the line to change and the sentence to delete.
    #[test]
    fn a_stream_without_timing_info_sends_no_timing_block() {
        let seq = SequenceHeaderObu {
            timing_info_present_flag: false,
            ..Default::default()
        };
        let owned = sequence_to_std(&seq).expect("converts");
        assert!(owned.std().pTimingInfo.is_null());
        assert_eq!(owned.std().flags.timing_info_present_flag(), 0);
    }
}
