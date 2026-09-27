//! H.265 decode profile: the key and chain [`crate::caps`] queries and derives
//! against.
//!
//! Picture format is the stream's, not a constant. SPS chroma and bit depth
//! (Main → NV12, Main 10 → P010, RExt 4:4:4 → the two-plane 4:4:4 formats) also
//! fill the `VkVideoProfileInfoKHR` every session object is created against.
//! Both come from one [`H265ProfileKey`]. A device that cannot host the
//! combination is refused before a session exists, so the ladder demotes with a
//! named reason rather than creating images the driver never advertised.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::NV12;
use crate::caps::P010;
use crate::caps::YUV444_10;
use crate::caps::YUV444_8;
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
    /// Decode usage hints between the profile and the codec struct. Optional by the
    /// spec, but Intel's Windows driver walks the chain expecting it and faults on
    /// the first parameters create without it; FFmpeg always chains it.
    usage: vk::VideoDecodeUsageInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl H265ProfileChain {
    pub(crate) fn new(key: H265ProfileKey) -> Self {
        Self {
            h265: vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(key.std_profile_idc),
            usage: vk::VideoDecodeUsageInfoKHR::default(),
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
        self.usage.p_next = (&self.h265 as *const vk::VideoDecodeH265ProfileInfoKHR<'_>).cast();
        self.profile.p_next = (&self.usage as *const vk::VideoDecodeUsageInfoKHR<'_>).cast();
        &self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::derive_caps;
    use crate::caps::CapsError;
    use crate::caps::DecodeProfile;
    use crate::caps::MaxLevelIdc;
    use crate::caps::RawCaps;
    use crate::caps::VideoFormat;
    use crate::caps::COINCIDE_USAGE;
    use crate::caps::DPB_USAGE;
    use crate::caps::OUTPUT_USAGE;

    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
            ..Default::default()
        }
    }

    fn coincide_device(coincide: Vec<VideoFormat>) -> RawCaps {
        RawCaps {
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
            max_level: MaxLevelIdc::H265(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2),
            std_header_version: vk::ExtensionProperties::default(),
            dpb_formats: vec![],
            output_formats: vec![],
            coincide_formats: coincide,
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
        // SAFETY: wire() pointed p_next at chain's own usage field, which lives for
        // this whole scope and is a valid VideoDecodeUsageInfoKHR.
        let usage = unsafe { &*profile.p_next.cast::<vk::VideoDecodeUsageInfoKHR<'_>>() };
        assert_eq!(usage.s_type, vk::StructureType::VIDEO_DECODE_USAGE_INFO_KHR);
        assert_eq!(
            usage.video_usage_hints,
            vk::VideoDecodeUsageFlagsKHR::DEFAULT
        );
        // SAFETY: wire() pointed the usage struct's p_next at chain's own h265 field,
        // which lives for this whole scope and is a valid VideoDecodeH265ProfileInfoKHR.
        let h265 = unsafe { &*usage.p_next.cast::<vk::VideoDecodeH265ProfileInfoKHR<'_>>() };
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
        let caps = derive_caps(&raw, NV12).unwrap();
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
    fn a_main10_stream_on_an_eight_bit_only_device_is_refused_before_any_session() {
        // NV12 is advertised; the stream is 10-bit and there is no P010. Refuse
        // by name — falling back to NV12 would decode 10-bit content into 8-bit.
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        assert_eq!(
            derive_caps(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: P010
            }
        );

        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(P010, COINCIDE_USAGE),
        ]);
        let caps = derive_caps(&raw, P010).unwrap();
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
            derive_caps(&raw, YUV444_8).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: YUV444_8
            }
        );

        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(YUV444_10, COINCIDE_USAGE),
        ]);
        let caps = derive_caps(&raw, YUV444_10).unwrap();
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
        let raw = RawCaps {
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
            derive_caps(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "output (DST|SAMPLED)",
                wanted: P010
            }
        );

        // DPB references are never sampled, so that half needs neither SAMPLED
        // nor MUTABLE_FORMAT.
        let raw = RawCaps {
            output_formats: vec![entry(P010, OUTPUT_USAGE)],
            ..raw
        };
        let caps = derive_caps(&raw, P010).unwrap();
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
            derive_caps(&raw, P010).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: P010,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        let raw = coincide_device(vec![VideoFormat {
            format: P010,
            image_usage: COINCIDE_USAGE,
            // A non-empty report that lacks MUTABLE_FORMAT: an explicit envelope, refused.
            image_create_flags: vk::ImageCreateFlags::ALIAS,
            ..Default::default()
        }]);
        assert_eq!(
            derive_caps(&raw, P010).unwrap_err(),
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
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::NoDecodeMode
        );
    }

    #[test]
    fn an_h265_layered_coincide_device_derives_a_picture_array() {
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        let caps = derive_caps(&raw, NV12).unwrap();
        assert!(caps.coincide && caps.layered_dpb);
    }
}
