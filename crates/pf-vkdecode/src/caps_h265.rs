//! H.265 decode capability query + derivation — [`crate::caps`] one codec over.
//!
//! [`query_h265_caps`] talks to the driver and only copies facts into
//! [`RawH265Caps`]. [`derive_caps_h265`] is pure over that struct and shares the
//! coincide/distinct/layered table (`derive_arrangement` in [`crate::caps`]).
//!
//! Picture format is the stream's, not a constant. SPS chroma and bit depth
//! (Main → NV12, Main 10 → P010, RExt 4:4:4 → the two-plane 4:4:4 formats) also
//! fill the `VkVideoProfileInfoKHR` every session object is created against.
//! Both come from one [`H265ProfileKey`]. A device that cannot host the
//! combination is refused before a session exists, so the ladder demotes with a
//! named reason rather than creating images the driver never advertised.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::derive_arrangement;
use crate::caps::CapsError;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps::MaxLevelIdc;
use crate::caps::VideoFormat;
use crate::caps::COINCIDE_USAGE;
use crate::caps::DPB_USAGE;
use crate::caps::NV12;
use crate::caps::OUTPUT_USAGE;
use crate::caps::P010;
use crate::caps::YUV444_10;
use crate::caps::YUV444_8;
use crate::device::DecodeDevice;
use crate::params_h265::profile_to_std;
use crate::params_h265::H265ParamsError;

/// Stream facts that fill `VkVideoProfileInfoKHR`.
///
/// Profile identity in Vulkan is by value across the caps query, the session,
/// every profile-listed image/buffer and the query pool. Each consumer rebuilds
/// a structurally identical chain from this `Copy` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H265ProfileKey {
    /// The four Vulkan expresses: Main (1), Main 10 (2), Main Still Picture (3), RExt (4).
    pub std_profile_idc: hh::StdVideoH265ProfileIdc,
    pub chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
}

impl H265ProfileKey {
    /// Build the key from one picture's SPS facts.
    ///
    /// The envelope is the same gate [`crate::params_h265`] applies — 4:2:0 or
    /// 4:4:4, no separate colour planes, 8 or 10 bits, luma depth == chroma depth
    /// — because the caps query needs a profile before any parameter-set
    /// conversion runs. A narrower copy here would drift and hand the driver a
    /// profile the SPS cannot match.
    pub fn from_stream(
        general_profile_idc: u8,
        chroma_format_idc: u8,
        separate_colour_plane_flag: bool,
        bit_depth_luma_minus8: u8,
        bit_depth_chroma_minus8: u8,
    ) -> Result<Self, H265ParamsError> {
        let std_profile_idc = profile_to_std(general_profile_idc)?;
        let chroma_subsampling = match chroma_format_idc {
            1 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
            3 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
            0 | 2 => {
                return Err(H265ParamsError::UnsupportedChromaFormat(chroma_format_idc));
            }
            other => return Err(H265ParamsError::InvalidChromaFormatIdc(other)),
        };
        // 4:4:4 with separate colour planes is ChromaArrayType 0: three
        // monochrome planes. `TYPE_444` would mis-state the bitstream.
        if chroma_format_idc == 3 && separate_colour_plane_flag {
            return Err(H265ParamsError::SeparateColourPlanes);
        }
        if bit_depth_luma_minus8 != bit_depth_chroma_minus8
            || !matches!(bit_depth_luma_minus8, 0 | 2)
        {
            return Err(H265ParamsError::UnsupportedBitDepth {
                luma_minus8: bit_depth_luma_minus8,
                chroma_minus8: bit_depth_chroma_minus8,
            });
        }
        let depth = if bit_depth_luma_minus8 == 0 {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        } else {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        };
        Ok(Self {
            std_profile_idc,
            chroma_subsampling,
            luma_bit_depth: depth,
            chroma_bit_depth: depth,
        })
    }

    /// Key for a stream whose chroma/depth the session already negotiated, before
    /// any SPS ([`crate::VkH265Decoder::probe_stream_support`]).
    ///
    /// Profile idc is not in that pair, so it is derived: 4:2:0 8-bit → Main,
    /// 4:2:0 10-bit → Main 10, 4:4:4 → RExt (4:4:4 is only RExt). Once an SPS
    /// arrives, [`Self::from_stream`] is the authority; this path never admits a
    /// combination that gate refuses.
    pub fn from_negotiated(
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
    ) -> Result<Self, H265ParamsError> {
        let general_profile_idc = match (chroma_format_idc, bit_depth_luma_minus8) {
            (1, 0) => 1,
            (1, 2) => 2,
            (3, _) => 4,
            // Outside the envelope: a profile idc that cannot rescue it, so
            // `from_stream` is the one gate that produces the error.
            _ => 4,
        };
        Self::from_stream(
            general_profile_idc,
            chroma_format_idc,
            false,
            bit_depth_luma_minus8,
            bit_depth_luma_minus8,
        )
    }

    /// `None` is outside the envelope [`Self::from_stream`] already refused.
    pub fn output_format(&self) -> Option<vk::Format> {
        let ten_bit = self.luma_bit_depth == vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
        if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_420 {
            Some(if ten_bit { P010 } else { NV12 })
        } else if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_444 {
            Some(if ten_bit { YUV444_10 } else { YUV444_8 })
        } else {
            None
        }
    }
}

/// Same mapping as [`H265ProfileKey::output_format`], without building a key.
pub fn output_format_for(chroma_format_idc: u8, bit_depth_luma_minus8: u8) -> Option<vk::Format> {
    match (chroma_format_idc, bit_depth_luma_minus8) {
        (1, 0) => Some(NV12),
        (1, 2) => Some(P010),
        (3, 0) => Some(YUV444_8),
        (3, 2) => Some(YUV444_10),
        _ => None,
    }
}

/// One H.265 decode profile chain. [`Self::wire`] points `profile.p_next` at this
/// struct's own `h265` field; do not move the value between `wire()` and the last
/// use of the returned reference.
pub(crate) struct H265ProfileChain {
    h265: vk::VideoDecodeH265ProfileInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl H265ProfileChain {
    pub(crate) fn new(key: H265ProfileKey) -> Self {
        Self {
            h265: vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(key.std_profile_idc),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H265)
                .chroma_subsampling(key.chroma_subsampling)
                .luma_bit_depth(key.luma_bit_depth)
                .chroma_bit_depth(key.chroma_bit_depth),
        }
    }

    /// Wire the internal `p_next` chain and hand out the profile root. Do not move
    /// `self` while the returned reference (or any pointer taken from it) lives.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'static> {
        self.profile.p_next = (&self.h265 as *const vk::VideoDecodeH265ProfileInfoKHR<'_>).cast();
        &self.profile
    }
}

/// Driver facts the thin H.265 query copies out, hand-buildable for tests.
/// `max_level_idc` is an H.265 Std level so it cannot be mixed with H.264's
/// `c_uint` of the same width.
#[derive(Debug, Clone, Default)]
pub struct RawH265Caps {
    pub capability_flags: vk::VideoCapabilityFlagsKHR,
    pub decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    pub min_bitstream_buffer_offset_alignment: u64,
    pub min_bitstream_buffer_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// Std H.265 level (index-coded), not a Vulkan enum.
    pub max_level_idc: hh::StdVideoH265LevelIdc,
    /// Echoed back at session creation (`VkVideoCapabilitiesKHR::stdHeaderVersion`).
    pub std_header_version: vk::ExtensionProperties,
    /// Formats usable for DISTINCT-mode DPB images (queried with [`DPB_USAGE`]).
    pub dpb_formats: Vec<VideoFormat>,
    /// Formats usable for DISTINCT-mode outputs (queried with [`OUTPUT_USAGE`]).
    pub output_formats: Vec<VideoFormat>,
    /// Formats usable when DPB and output COINCIDE ([`COINCIDE_USAGE`]).
    pub coincide_formats: Vec<VideoFormat>,
}

/// Session-shaping facts from one raw H.265 query, for a stream whose SPS asks
/// for `wanted` ([`H265ProfileKey::output_format`]).
///
/// Missing `wanted` under the advertised mode is [`CapsError::NoFormat`] with
/// mode and format named. Nothing is created and there is no fallback to a
/// shallower format that would lose bits.
pub fn derive_caps_h265(raw: &RawH265Caps, wanted: vk::Format) -> Result<DecodeCaps, CapsError> {
    let arrangement = derive_arrangement(
        raw.capability_flags,
        raw.decode_flags,
        wanted,
        &raw.dpb_formats,
        &raw.output_formats,
        &raw.coincide_formats,
    )?;
    Ok(arrangement.into_caps(
        raw.min_bitstream_buffer_offset_alignment,
        raw.min_bitstream_buffer_size_alignment,
        raw.picture_access_granularity,
        raw.min_coded_extent,
        raw.max_coded_extent,
        raw.max_dpb_slots,
        raw.max_active_reference_pictures,
        MaxLevelIdc::H265(raw.max_level_idc),
        raw.std_header_version,
    ))
}

/// Driver query for one H.265 profile: video capabilities plus the three
/// format-property enumerations. Copies facts out; derivation is
/// [`derive_caps_h265`].
///
/// # Safety
///
/// `dev` wraps live handles per the [`crate::DeviceHandles`] contract (this calls
/// instance-level functions against its physical device).
pub(crate) unsafe fn query_h265_caps(
    dev: &DecodeDevice,
    key: H265ProfileKey,
) -> Result<RawH265Caps, vk::Result> {
    let mut chain = H265ProfileChain::new(key);
    let profile = chain.wire();

    let mut h265_caps = vk::VideoDecodeH265CapabilitiesKHR::default();
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    // `push_next` prepends. Push the codec struct last so decode-caps sits first
    // after the base. Do not reverse it: a position-filling driver then reads a
    // Std level as a flag bitmask, and neither COINCIDE nor DISTINCT appears set.
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push_next(&mut h265_caps)
        .push_next(&mut decode_caps);
    // SAFETY: physical device is live (DeviceHandles contract); `profile` roots a
    // fully wired, immovable chain; `caps` chains driver-fillable structs that all
    // outlive the call.
    let r = unsafe {
        (dev.video_queue_instance()
            .fp()
            .get_physical_device_video_capabilities_khr)(
            dev.physical_device(), profile, &mut caps
        )
    };
    if r != vk::Result::SUCCESS {
        return Err(r);
    }
    // Copy everything out before the chained &mut borrows end (encoder precedent).
    let capability_flags = caps.flags;
    let min_bitstream_buffer_offset_alignment = caps.min_bitstream_buffer_offset_alignment;
    let min_bitstream_buffer_size_alignment = caps.min_bitstream_buffer_size_alignment;
    let picture_access_granularity = caps.picture_access_granularity;
    let min_coded_extent = caps.min_coded_extent;
    let max_coded_extent = caps.max_coded_extent;
    let max_dpb_slots = caps.max_dpb_slots;
    let max_active_reference_pictures = caps.max_active_reference_pictures;
    let std_header_version = caps.std_header_version;
    let decode_flags = decode_caps.flags;
    let max_level_idc = h265_caps.max_level_idc;

    // Verbatim driver fill, before interpretation. Populated `max_dpb_slots` next
    // to `decode_flags` 0 is a real "no DPB mode"; zeros on both mean the query
    // never landed.
    tracing::debug!(
        codec = "H.265",
        ?capability_flags,
        ?decode_flags,
        decode_flags_raw = decode_flags.as_raw(),
        max_level_idc,
        max_dpb_slots,
        max_active_reference_pictures,
        ?min_coded_extent,
        ?max_coded_extent,
        ?picture_access_granularity,
        "driver video capabilities, verbatim"
    );

    // The three queries carry the REAL creation usages (SAMPLED included for the
    // presenter-facing roles) so the answers validate the images the pools build.
    let decode_profile = DecodeProfile::H265(key);
    // SAFETY: same liveness as above; the helper wires its own chain (this and
    // the two calls below).
    let dpb_formats = unsafe { crate::caps::query_formats(dev, decode_profile, DPB_USAGE)? };
    // SAFETY: as above.
    let output_formats = unsafe { crate::caps::query_formats(dev, decode_profile, OUTPUT_USAGE)? };
    // SAFETY: as above.
    let coincide_formats =
        unsafe { crate::caps::query_formats(dev, decode_profile, COINCIDE_USAGE)? };

    Ok(RawH265Caps {
        capability_flags,
        decode_flags,
        min_bitstream_buffer_offset_alignment,
        min_bitstream_buffer_size_alignment,
        picture_access_granularity,
        min_coded_extent,
        max_coded_extent,
        max_dpb_slots,
        max_active_reference_pictures,
        max_level_idc,
        std_header_version,
        dpb_formats,
        output_formats,
        coincide_formats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
            ..Default::default()
        }
    }

    fn coincide_device(coincide: Vec<VideoFormat>) -> RawH265Caps {
        RawH265Caps {
            capability_flags: vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES,
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE,
            min_bitstream_buffer_offset_alignment: 256,
            min_bitstream_buffer_size_alignment: 256,
            picture_access_granularity: vk::Extent2D {
                width: 1,
                height: 1,
            },
            min_coded_extent: vk::Extent2D {
                width: 16,
                height: 16,
            },
            max_coded_extent: vk::Extent2D {
                width: 8192,
                height: 8192,
            },
            max_dpb_slots: 17,
            max_active_reference_pictures: 16,
            max_level_idc: hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
            coincide_formats: coincide,
            ..Default::default()
        }
    }

    #[test]
    fn the_profile_is_built_from_the_streams_chroma_format_and_bit_depth() {
        let main = H265ProfileKey::from_stream(1, 1, false, 0, 0).unwrap();
        assert_eq!(
            main.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
        );
        assert_eq!(
            main.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            main.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        );
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap();
        assert_eq!(
            main10.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );
        assert_eq!(
            main10.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert_eq!(main10.chroma_bit_depth, main10.luma_bit_depth);
        assert_eq!(main10.output_format(), Some(P010));

        let rext8 = H265ProfileKey::from_stream(4, 3, false, 0, 0).unwrap();
        assert_eq!(
            rext8.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
        );
        assert_eq!(rext8.output_format(), Some(YUV444_8));
        let rext10 = H265ProfileKey::from_stream(4, 3, false, 2, 2).unwrap();
        assert_eq!(rext10.output_format(), Some(YUV444_10));

        for (chroma, depth, format) in [
            (1u8, 0u8, NV12),
            (1, 2, P010),
            (3, 0, YUV444_8),
            (3, 2, YUV444_10),
        ] {
            assert_eq!(output_format_for(chroma, depth), Some(format));
        }
        assert_eq!(output_format_for(2, 0), None, "4:2:2 has no output format");
    }

    #[test]
    fn the_negotiated_pair_picks_the_profile_a_host_encodes_it_with() {
        let main = H265ProfileKey::from_negotiated(1, 0).unwrap();
        assert_eq!(
            main,
            H265ProfileKey::from_stream(1, 1, false, 0, 0).unwrap()
        );
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = H265ProfileKey::from_negotiated(1, 2).unwrap();
        assert_eq!(
            main10,
            H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap()
        );
        assert_eq!(main10.output_format(), Some(P010));

        let rext8 = H265ProfileKey::from_negotiated(3, 0).unwrap();
        assert_eq!(
            rext8,
            H265ProfileKey::from_stream(4, 3, false, 0, 0).unwrap()
        );
        assert_eq!(rext8.output_format(), Some(YUV444_8));
        let rext10 = H265ProfileKey::from_negotiated(3, 2).unwrap();
        assert_eq!(rext10.output_format(), Some(YUV444_10));

        assert_eq!(
            H265ProfileKey::from_negotiated(2, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(2)
        );
        assert_eq!(
            H265ProfileKey::from_negotiated(0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            H265ProfileKey::from_negotiated(1, 4).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 4,
                chroma_minus8: 4
            }
        );
    }

    #[test]
    fn stream_facts_outside_the_envelope_are_refused_by_the_profile_builder() {
        assert_eq!(
            H265ProfileKey::from_stream(9, 1, false, 0, 0).unwrap_err(),
            H265ParamsError::UnmappableProfileIdc(9),
            "High Throughput/SCC profiles have no Vulkan code point"
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 2, false, 0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(2),
            "4:2:2 is legal H.265 with no punktfunk output plumbing"
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 0, false, 0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 4, false, 0, 0).unwrap_err(),
            H265ParamsError::InvalidChromaFormatIdc(4)
        );
        // Same envelope as `params_h265`: separate planes at 4:4:4 are
        // ChromaArrayType 0, not interleaved 4:4:4. This `pub` constructor is
        // reachable without the planner.
        assert_eq!(
            H265ProfileKey::from_stream(4, 3, true, 0, 0).unwrap_err(),
            H265ParamsError::SeparateColourPlanes
        );
        // The flag is only defined at 4:4:4 (7.4.3.2.1); it must not disturb 4:2:0.
        assert!(H265ProfileKey::from_stream(1, 1, true, 0, 0).is_ok());
        assert_eq!(
            H265ProfileKey::from_stream(4, 1, false, 4, 4).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 4,
                chroma_minus8: 4
            },
            "12-bit has no output format"
        );
        assert_eq!(
            H265ProfileKey::from_stream(4, 1, false, 0, 2).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 0,
                chroma_minus8: 2
            },
            "disagreeing luma/chroma depths have no output format"
        );
    }

    #[test]
    fn the_h265_profile_chain_wires_the_codec_struct_behind_the_root_profile() {
        let key = H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap();
        let mut chain = H265ProfileChain::new(key);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265
        );
        assert_eq!(
            profile.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            profile.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert!(!profile.p_next.is_null());
        // SAFETY: wire() pointed p_next at chain's own h265 field, which lives for
        // this whole scope and is a valid VideoDecodeH265ProfileInfoKHR.
        let h265 = unsafe {
            &*profile
                .p_next
                .cast::<vk::VideoDecodeH265ProfileInfoKHR<'_>>()
        };
        assert_eq!(
            h265.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );

        // Same chain via the type-erased dispatch — an H.264 idc cannot build this.
        let mut erased = DecodeProfile::H265(key).chain();
        let profile = erased.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265
        );
    }

    #[test]
    fn a_main_stream_derives_nv12_on_a_coincide_device() {
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        let caps = derive_caps_h265(&raw, NV12).unwrap();
        assert!(caps.coincide);
        assert!(!caps.layered_dpb);
        assert_eq!(caps.output_format, NV12);
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(
            caps.plane_view_formats,
            [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]
        );
        assert_eq!(caps.max_dpb_slots, 17);
        assert_eq!(caps.min_bitstream_offset_alignment, 256);
    }

    #[test]
    fn the_level_ceiling_derived_here_is_tagged_h265_not_h264() {
        // Both Std level types are `c_uint`. H.265 6.2 is 15, H.264 6.2 is 19;
        // the tag keeps the numeric gate from comparing the wrong code space.
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        let caps = derive_caps_h265(&raw, NV12).unwrap();
        assert_eq!(
            caps.max_level_idc,
            MaxLevelIdc::H265(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2)
        );
        assert_eq!(
            caps.max_level_idc.code_point(),
            hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
            "the gate still compares the raw code point"
        );
        assert_ne!(
            caps.max_level_idc,
            MaxLevelIdc::H264(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2),
            "same number, different codec — not the same ceiling"
        );
    }

    #[test]
    fn a_main10_stream_on_an_eight_bit_only_device_is_refused_before_any_session() {
        // NV12 is advertised; the stream is 10-bit and there is no P010. Refuse
        // by name — falling back to NV12 would decode 10-bit content into 8-bit.
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        assert_eq!(
            derive_caps_h265(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: P010
            }
        );

        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(P010, COINCIDE_USAGE),
        ]);
        let caps = derive_caps_h265(&raw, P010).unwrap();
        assert_eq!(caps.output_format, P010);
        assert_eq!(
            caps.plane_view_formats,
            [
                vk::Format::R10X6_UNORM_PACK16,
                vk::Format::R10X6G10X6_UNORM_2PACK16
            ]
        );
    }

    #[test]
    fn a_444_stream_is_refused_where_caps_stop_at_420_and_derives_where_they_do_not() {
        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(P010, COINCIDE_USAGE),
        ]);
        assert_eq!(
            derive_caps_h265(&raw, YUV444_8).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: YUV444_8
            }
        );

        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(YUV444_10, COINCIDE_USAGE),
        ]);
        let caps = derive_caps_h265(&raw, YUV444_10).unwrap();
        assert_eq!(caps.output_format, YUV444_10);
        assert_eq!(
            caps.plane_view_formats,
            [
                vk::Format::R10X6_UNORM_PACK16,
                vk::Format::R10X6G10X6_UNORM_2PACK16
            ]
        );
    }

    #[test]
    fn a_distinct_device_missing_the_format_on_one_half_names_that_half() {
        // Distinct, layered DPB. P010 on the DPB half only — the error must name output.
        let raw = RawH265Caps {
            capability_flags: vk::VideoCapabilityFlagsKHR::empty(),
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT,
            dpb_formats: vec![VideoFormat {
                format: P010,
                image_usage: DPB_USAGE,
                image_create_flags: vk::ImageCreateFlags::empty(),
                ..Default::default()
            }],
            output_formats: vec![entry(NV12, OUTPUT_USAGE)],
            ..coincide_device(vec![])
        };
        assert_eq!(
            derive_caps_h265(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "output (DST|SAMPLED)",
                wanted: P010
            }
        );

        // DPB references are never sampled, so that half needs neither SAMPLED
        // nor MUTABLE_FORMAT.
        let raw = RawH265Caps {
            output_formats: vec![entry(P010, OUTPUT_USAGE)],
            ..raw
        };
        let caps = derive_caps_h265(&raw, P010).unwrap();
        assert!(!caps.coincide);
        assert!(caps.layered_dpb);
        assert_eq!(caps.output_format, P010);
    }

    #[test]
    fn an_h265_entry_missing_a_creation_usage_bit_is_refused_naming_the_gap() {
        // Format listed without SAMPLED: the presenter could never read it.
        let raw = coincide_device(vec![entry(
            P010,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )]);
        assert_eq!(
            derive_caps_h265(&raw, P010).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: P010,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        let raw = coincide_device(vec![VideoFormat {
            format: P010,
            image_usage: COINCIDE_USAGE,
            image_create_flags: vk::ImageCreateFlags::empty(),
            ..Default::default()
        }]);
        assert_eq!(
            derive_caps_h265(&raw, P010).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: P010,
            }
        );
    }

    #[test]
    fn an_h265_device_with_no_decode_mode_at_all_is_a_hard_error() {
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps_h265(&raw, NV12).unwrap_err(),
            CapsError::NoDecodeMode
        );

        // Coincide with a layered DPB is unsupported: the picture pool needs
        // per-slot images, whatever the codec.
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps_h265(&raw, NV12).unwrap_err(),
            CapsError::CoincideLayeredDpb
        );
    }
}
