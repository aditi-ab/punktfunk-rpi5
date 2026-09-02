//! AV1 decode capability query + derivation — [`crate::caps_h265`] one codec over.
//!
//! `query_av1_caps` talks to the driver and only COPIES facts into [`RawAv1Caps`].
//! [`derive_caps_av1`] is pure and shares `derive_arrangement` with H.264/H.265.
//!
//! Film grain is part of the DECODE PROFILE (`filmGrainSupport` beside
//! `stdProfile`). Vulkan identity is BY VALUE, so a grain stream is a different
//! profile from a grain-less one. Passing `VK_FALSE` would present pictures the
//! encoder never intended; the hardware synthesizes grain or the ladder demotes.
//!
//! Picture format follows the stream as in H.265. Monochrome, 4:2:2 and 12-bit
//! are refused before a session — [`crate::OUTPUT_FORMATS`] is the vocabulary.

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
use crate::caps::OUTPUT_USAGE;
use crate::caps_h265::output_format_for;
use crate::device::DecodeDevice;
use crate::params_av1::ParamsAv1Error;
use crate::params_av1::STD_PROFILE_HIGH;
use crate::params_av1::STD_PROFILE_MAIN;
use crate::params_av1::STD_PROFILE_PROFESSIONAL;

/// Each field is a `VkVideoProfileInfoKHR` / `VkVideoDecodeAV1ProfileInfoKHR`
/// value. Vulkan identity is BY VALUE, so consumers rebuild the chain from this
/// `Copy` key rather than sharing one wired struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Av1ProfileKey {
    pub std_profile: hh::StdVideoAV1Profile,
    pub chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    /// Sequence `film_grain_params_present`, not a frame's `apply_grain`.
    /// A session is one profile for its lifetime; a frame cannot apply grain the
    /// sequence never declared (`crate::pic_av1`'s `pFilmGrain` gate requires both).
    pub film_grain: bool,
}

impl Av1ProfileKey {
    /// Bit depth is in bits (8/10/12), not minus8.
    ///
    /// Refused here, before any query: monochrome, 4:2:2, 4:4:0, 12-bit — none have
    /// a two-plane format in [`crate::OUTPUT_FORMATS`].
    pub fn from_stream(
        seq_profile: u8,
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<Self, ParamsAv1Error> {
        let std_profile = match seq_profile {
            0 => STD_PROFILE_MAIN,
            1 => STD_PROFILE_HIGH,
            2 => STD_PROFILE_PROFESSIONAL,
            other => return Err(ParamsAv1Error::UnsupportedProfile(other)),
        };
        let chroma_subsampling = match chroma_format_idc {
            1 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
            3 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
            // Vulkan can express MONOCHROME; this crate still refuses it.
            // Every delivered picture format is two-plane and the presenter
            // samples both. Refused rather than half-supported.
            other => return Err(ParamsAv1Error::UnsupportedChromaFormat(other)),
        };
        let depth = match bit_depth {
            8 => vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
            10 => vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
            other => return Err(ParamsAv1Error::UnsupportedBitDepth(other)),
        };
        // AV1 codes one bit depth for the sequence; there is no chroma depth
        // that can disagree with luma (the H.265 extra clause has no counterpart).
        Ok(Self {
            std_profile,
            chroma_subsampling,
            luma_bit_depth: depth,
            chroma_bit_depth: depth,
            film_grain,
        })
    }

    /// Probe key when sampling/depth/grain are negotiated but the sequence header
    /// has not arrived ([`crate::VkAv1Decoder::probe_stream_support`]).
    ///
    /// Negotiation does not carry `seq_profile`, so it is derived: 4:2:0 → Main,
    /// 4:4:4 → High (a host encodes 4:4:4 as High). Anything else goes to
    /// Professional, which cannot rescue a combination [`Self::from_stream`] refuses.
    pub fn from_negotiated(
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<Self, ParamsAv1Error> {
        let seq_profile = match chroma_format_idc {
            1 => 0,
            3 => 1,
            _ => 2,
        };
        Self::from_stream(seq_profile, chroma_format_idc, bit_depth, film_grain)
    }

    /// `None` outside the envelope (unreachable off [`Self::from_stream`]).
    pub fn output_format(&self) -> Option<vk::Format> {
        let ten_bit = self.luma_bit_depth == vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
        let depth_minus8 = if ten_bit { 2 } else { 0 };
        if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_420 {
            output_format_for(1, depth_minus8)
        } else if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_444 {
            output_format_for(3, depth_minus8)
        } else {
            None
        }
    }
}

/// AV1 decode profile chain — [`crate::caps_h265::H265ProfileChain`]'s twin.
///
/// [`Self::wire`] points `profile.p_next` at this struct's own `av1` field; do
/// not move the value between `wire()` and the last use of the returned reference.
pub(crate) struct Av1ProfileChain {
    av1: vk::VideoDecodeAV1ProfileInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl Av1ProfileChain {
    pub(crate) fn new(key: Av1ProfileKey) -> Self {
        Self {
            av1: vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(key.std_profile)
                // From the SEQUENCE, never forced false to make a query pass:
                // a grain-less profile would decode a grain stream into pictures
                // the encoder never intended (module docs).
                .film_grain_support(key.film_grain),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_AV1)
                .chroma_subsampling(key.chroma_subsampling)
                .luma_bit_depth(key.luma_bit_depth)
                .chroma_bit_depth(key.chroma_bit_depth),
        }
    }

    /// Do not move `self` while the returned reference (or any pointer taken
    /// from it) lives.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'static> {
        self.profile.p_next = (&self.av1 as *const vk::VideoDecodeAV1ProfileInfoKHR<'_>).cast();
        &self.profile
    }
}

/// Hand-buildable driver facts. Field-for-field [`crate::RawH265Caps`], except
/// `max_level` is an AV1 Std level code point.
#[derive(Debug, Clone, Default)]
pub struct RawAv1Caps {
    pub capability_flags: vk::VideoCapabilityFlagsKHR,
    /// Coincide/distinct advertisement (`VkVideoDecodeCapabilitiesKHR::flags`).
    pub decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    pub min_bitstream_buffer_offset_alignment: u64,
    pub min_bitstream_buffer_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// `VkVideoDecodeAV1CapabilitiesKHR::maxLevel` — Std code points 0…23, the same
    /// numbering as bitstream `seq_level_idx` over that range.
    ///
    /// `seq_level_idx` is 5 bits: 24…30 reserved, 31 is Annex A's "maximum
    /// parameters" sentinel (not constrained to a level). 31 has no Std code point
    /// and is not ordered above 7.3. The decoder treats a stream above this ceiling
    /// as advisory (`VkAv1Decoder::ensure_state`), not as a numeric compare.
    pub max_level: hh::StdVideoAV1Level,
    /// `VkVideoCapabilitiesKHR::stdHeaderVersion` — session creation echoes it back.
    pub std_header_version: vk::ExtensionProperties,
    /// Distinct-mode DPB images, queried with [`DPB_USAGE`].
    pub dpb_formats: Vec<VideoFormat>,
    /// Distinct-mode outputs, queried with [`OUTPUT_USAGE`].
    pub output_formats: Vec<VideoFormat>,
    /// Coincide-mode images, queried with [`COINCIDE_USAGE`].
    pub coincide_formats: Vec<VideoFormat>,
}

/// Same refusals as H.265: no advertised entry for `wanted` yields
/// [`CapsError::NoFormat`] with the mode named. Nothing is created — a
/// pre-session demote, never a silent fallback that would lose bits.
pub fn derive_caps_av1(raw: &RawAv1Caps, wanted: vk::Format) -> Result<DecodeCaps, CapsError> {
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
        MaxLevelIdc::Av1(raw.max_level),
        raw.std_header_version,
    ))
}

/// Copies driver facts into [`RawAv1Caps`]. Derivation is [`derive_caps_av1`].
///
/// A device that cannot host the profile — grain-enabled, most importantly —
/// fails the first call with a profile-unsupported result. The caller turns
/// that into the ladder demote ([`crate::VkAv1Decoder::probe_stream_support`]).
///
/// # Safety
///
/// `dev` wraps live handles per the [`crate::DeviceHandles`] contract (this
/// calls instance-level functions against its physical device).
pub(crate) unsafe fn query_av1_caps(
    dev: &DecodeDevice,
    key: Av1ProfileKey,
) -> Result<RawAv1Caps, vk::Result> {
    let mut chain = Av1ProfileChain::new(key);
    let profile = chain.wire();

    let mut av1_caps = vk::VideoDecodeAV1CapabilitiesKHR::default();
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    // ORDER IS LOAD-BEARING. `push_next` prepends: codec struct first leaves
    // VkVideoDecodeCapabilitiesKHR directly after the base. Reverse that and
    // some drivers fill by position (`query_h265_caps`).
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push_next(&mut av1_caps)
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
    // Copy out before the chained &mut borrows end.
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
    let max_level = av1_caps.max_level;

    // Real creation usages (SAMPLED for presenter-facing roles) so the answers
    // match the images the pools will build.
    let decode_profile = DecodeProfile::Av1(key);
    // SAFETY: same liveness as above; the helper wires its own chain (this and
    // the two calls below).
    let dpb_formats = unsafe { crate::caps::query_formats(dev, decode_profile, DPB_USAGE)? };
    // SAFETY: as above.
    let output_formats = unsafe { crate::caps::query_formats(dev, decode_profile, OUTPUT_USAGE)? };
    // SAFETY: as above.
    let coincide_formats =
        unsafe { crate::caps::query_formats(dev, decode_profile, COINCIDE_USAGE)? };

    Ok(RawAv1Caps {
        capability_flags,
        decode_flags,
        min_bitstream_buffer_offset_alignment,
        min_bitstream_buffer_size_alignment,
        picture_access_granularity,
        min_coded_extent,
        max_coded_extent,
        max_dpb_slots,
        max_active_reference_pictures,
        max_level,
        std_header_version,
        dpb_formats,
        output_formats,
        coincide_formats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::NV12;
    use crate::caps::P010;
    use crate::caps::YUV444_10;
    use crate::caps::YUV444_8;

    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
            ..Default::default()
        }
    }

    fn coincide_device(coincide: Vec<VideoFormat>) -> RawAv1Caps {
        RawAv1Caps {
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
            max_dpb_slots: 9,
            max_active_reference_pictures: 8,
            max_level: hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1,
            coincide_formats: coincide,
            ..Default::default()
        }
    }

    #[test]
    fn the_profile_is_built_from_the_sequences_sampling_depth_and_grain_flag() {
        let main = Av1ProfileKey::from_stream(0, 1, 8, false).unwrap();
        assert_eq!(main.std_profile, STD_PROFILE_MAIN);
        assert_eq!(
            main.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            main.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        );
        assert_eq!(main.chroma_bit_depth, main.luma_bit_depth);
        assert!(!main.film_grain);
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = Av1ProfileKey::from_stream(0, 1, 10, false).unwrap();
        assert_eq!(
            main10.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert_eq!(main10.output_format(), Some(P010));

        let high8 = Av1ProfileKey::from_stream(1, 3, 8, false).unwrap();
        assert_eq!(high8.std_profile, STD_PROFILE_HIGH);
        assert_eq!(
            high8.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
        );
        assert_eq!(high8.output_format(), Some(YUV444_8));
        assert_eq!(
            Av1ProfileKey::from_stream(1, 3, 10, false)
                .unwrap()
                .output_format(),
            Some(YUV444_10)
        );

        let grainy = Av1ProfileKey::from_stream(0, 1, 8, true).unwrap();
        assert_ne!(grainy, main);
        assert!(grainy.film_grain);
        assert_eq!(
            grainy.output_format(),
            main.output_format(),
            "grain changes the profile, never the picture format"
        );
    }

    #[test]
    fn sequence_facts_outside_the_envelope_are_refused_by_the_profile_builder() {
        assert_eq!(
            Av1ProfileKey::from_stream(3, 1, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedProfile(3),
            "there is no AV1 seq_profile 3"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(0, 0, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(0),
            "monochrome has no two-plane picture format here"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 2, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(2),
            "4:2:2 is legal AV1 Professional with no punktfunk output plumbing"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 4, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(4),
            "the planner's 4:4:0 sentinel is refused, not read as 4:4:4"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 1, 12, false).unwrap_err(),
            ParamsAv1Error::UnsupportedBitDepth(12)
        );
    }

    #[test]
    fn the_negotiated_shape_picks_the_profile_a_host_encodes_it_with() {
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 8, false).unwrap(),
            Av1ProfileKey::from_stream(0, 1, 8, false).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 10, false).unwrap(),
            Av1ProfileKey::from_stream(0, 1, 10, false).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(3, 8, true).unwrap(),
            Av1ProfileKey::from_stream(1, 3, 8, true).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(0, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 12, false).unwrap_err(),
            ParamsAv1Error::UnsupportedBitDepth(12)
        );
    }

    #[test]
    fn the_av1_profile_chain_wires_the_codec_struct_behind_the_root_profile() {
        let key = Av1ProfileKey::from_stream(0, 1, 10, true).unwrap();
        let mut chain = Av1ProfileChain::new(key);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1
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
        // SAFETY: wire() pointed p_next at chain's own av1 field, which lives for
        // this whole scope and is a valid VideoDecodeAV1ProfileInfoKHR.
        let av1 = unsafe {
            &*profile
                .p_next
                .cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>()
        };
        assert_eq!(av1.std_profile, STD_PROFILE_MAIN);
        assert_eq!(
            av1.film_grain_support,
            vk::TRUE,
            "the query the device answers is the GRAIN-enabled one"
        );

        let plain = Av1ProfileKey::from_stream(0, 1, 10, false).unwrap();
        let mut chain = Av1ProfileChain::new(plain);
        let profile = chain.wire();
        // SAFETY: as above.
        let av1 = unsafe {
            &*profile
                .p_next
                .cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>()
        };
        assert_eq!(av1.film_grain_support, vk::FALSE);

        // DecodeProfile must build the same chain; a profile-idc mix-up would
        // show up here.
        let mut erased = DecodeProfile::Av1(key).chain();
        let profile = erased.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1
        );
        // SAFETY: as above — the erased chain wires its own AV1 struct.
        let av1 = unsafe {
            &*profile
                .p_next
                .cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>()
        };
        assert_eq!(av1.film_grain_support, vk::TRUE);
    }

    #[test]
    fn a_main_stream_derives_nv12_on_a_coincide_device() {
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        let caps = derive_caps_av1(&raw, NV12).unwrap();
        assert!(caps.coincide);
        assert!(!caps.layered_dpb);
        assert_eq!(caps.output_format, NV12);
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(
            caps.plane_view_formats,
            [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]
        );
        assert_eq!(caps.max_dpb_slots, 9);
        assert_eq!(caps.min_bitstream_offset_alignment, 256);
    }

    #[test]
    fn the_level_ceiling_derived_here_is_tagged_av1_not_another_codec() {
        // All three `StdVideo*LevelIdc` types are `c_uint` and the code spaces
        // disagree (AV1 5.1 is 13, H.265 5.1 is 12, H.264 5.1 is 51). The tag
        // is what makes the decoder's numeric gate honest.
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        let caps = derive_caps_av1(&raw, NV12).unwrap();
        assert_eq!(
            caps.max_level_idc,
            MaxLevelIdc::Av1(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1)
        );
        assert_eq!(
            caps.max_level_idc.code_point(),
            hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1,
            "the gate still compares the raw code point"
        );
        assert_ne!(
            caps.max_level_idc,
            MaxLevelIdc::H265(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1),
            "same number, different codec — not the same ceiling"
        );
    }

    #[test]
    fn a_ten_bit_stream_on_an_eight_bit_only_device_is_refused_before_any_session() {
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        assert_eq!(
            derive_caps_av1(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: P010
            }
        );

        let raw = coincide_device(vec![
            entry(NV12, COINCIDE_USAGE),
            entry(P010, COINCIDE_USAGE),
        ]);
        let caps = derive_caps_av1(&raw, P010).unwrap();
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
    fn a_distinct_device_missing_the_format_on_one_half_names_that_half() {
        // Distinct only, layered DPB: DPB lists P010, output does not — the
        // refusal must name the missing half.
        let raw = RawAv1Caps {
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
            derive_caps_av1(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "output (DST|SAMPLED)",
                wanted: P010
            }
        );

        // Distinct derives when both halves list it. DPB needs neither SAMPLED
        // nor MUTABLE_FORMAT — reference images are never sampled.
        let raw = RawAv1Caps {
            output_formats: vec![entry(P010, OUTPUT_USAGE)],
            ..raw
        };
        let caps = derive_caps_av1(&raw, P010).unwrap();
        assert!(!caps.coincide);
        assert!(caps.layered_dpb);
        assert_eq!(caps.output_format, P010);
    }

    #[test]
    fn an_av1_entry_missing_a_creation_usage_bit_is_refused_naming_the_gap() {
        // Format listed but without SAMPLED: the presenter could never read it.
        let raw = coincide_device(vec![entry(
            NV12,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )]);
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: NV12,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        let raw = coincide_device(vec![VideoFormat {
            format: NV12,
            image_usage: COINCIDE_USAGE,
            image_create_flags: vk::ImageCreateFlags::empty(),
            ..Default::default()
        }]);
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: NV12,
            }
        );
    }

    #[test]
    fn an_av1_device_with_no_decode_mode_at_all_is_a_hard_error() {
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::NoDecodeMode
        );

        // Coincide with a layered DPB stays unsupported: the picture pool needs
        // per-slot images, whatever the codec.
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::CoincideLayeredDpb
        );
    }
}
