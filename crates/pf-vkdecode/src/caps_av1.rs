//! AV1 decode capability query + derivation — [`crate::caps_h265`] one codec over.
//!
//! Same split as the other two: `query_av1_caps` is the one THIN function that
//! talks to the driver and only COPIES facts into [`RawAv1Caps`];
//! [`derive_caps_av1`] is pure over a hand-buildable struct and shares the whole
//! coincide/distinct/layered decision table with H.264 and H.265
//! (`derive_arrangement`, in [`crate::caps`]).
//!
//! What AV1 adds to the H.265 shape is FILM GRAIN, and it is not a detail. Grain
//! synthesis is part of the DECODE PROFILE — `VkVideoDecodeAV1ProfileInfoKHR`
//! carries `filmGrainSupport` beside `stdProfile`, and profile identity in Vulkan
//! is BY VALUE across the caps query, the session, every profile-listed
//! image/buffer and the query pool. So a session for a stream whose sequence
//! header enables grain is a DIFFERENT profile from one that does not, and a
//! device that cannot host the grain-enabled profile answers the caps query with a
//! `VK_ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR`-class result.
//!
//! That refusal is the whole point, and it is why [`Av1ProfileKey`] carries the
//! flag rather than the decoder passing `VK_FALSE` and hoping: a decoder that
//! silently asked for a grain-less profile would decode the stream's pictures
//! correctly and then present them WITHOUT the grain the encoder relied on — a
//! plausible-looking, measurably wrong picture, which is the class this crate
//! exists to refuse. The stream's grain is either synthesized by the hardware or
//! the device demotes to the next decoder rung.
//!
//! The picture format is the stream's, as in H.265: 4:2:0 8-bit → NV12, 4:2:0
//! 10-bit → P010, 4:4:4 (AV1 High) → the two-plane 4:4:4 formats. Monochrome,
//! 4:2:2 and 12-bit are refused BEFORE a session exists — this crate has no
//! output plumbing for any of them ([`crate::OUTPUT_FORMATS`] is the whole
//! vocabulary).

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

/// The stream facts that identify an AV1 decode profile, as Vulkan states them.
///
/// Every one of these is a `VkVideoProfileInfoKHR`/`VkVideoDecodeAV1ProfileInfoKHR`
/// field, and profile identity in Vulkan is BY VALUE — so this small `Copy` key is
/// what gets passed around, and each consumer rebuilds a structurally identical
/// chain from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Av1ProfileKey {
    /// `StdVideoAV1Profile`: Main (0), High (1), Professional (2).
    pub std_profile: hh::StdVideoAV1Profile,
    pub chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    /// `VkVideoDecodeAV1ProfileInfoKHR::filmGrainSupport` — the SEQUENCE's
    /// `film_grain_params_present`, not any one frame's `apply_grain`. A session
    /// is created against one profile and lives across frames, so the sequence
    /// flag is the only honest answer; a frame cannot apply grain a sequence
    /// never declared (`crate::pic_av1`'s `pFilmGrain` gate requires both).
    pub film_grain: bool,
}

impl Av1ProfileKey {
    /// Build the key from one sequence header's facts: `seq_profile`, the
    /// sampling in the planner's `chroma_format_idc` vocabulary, the bit depth in
    /// BITS (8/10/12, as [`pf_bitstream::av1::PicturePlan::bit_depth`] states it)
    /// and whether the sequence enables film grain.
    ///
    /// Every combination this crate has no picture format for is refused HERE,
    /// before any query or session: monochrome and 4:2:2 (and the 4:4:0 shape the
    /// planner reports as 4) have no two-plane format in [`crate::OUTPUT_FORMATS`],
    /// and 12-bit has none either.
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
            // Monochrome IS expressible as a Vulkan profile
            // (`VideoChromaSubsamplingFlagsKHR::MONOCHROME`) and this crate still
            // refuses it: every picture format it delivers is two-plane, and the
            // presenter samples both planes. Refused rather than half-supported.
            other => return Err(ParamsAv1Error::UnsupportedChromaFormat(other)),
        };
        let depth = match bit_depth {
            8 => vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
            10 => vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
            other => return Err(ParamsAv1Error::UnsupportedBitDepth(other)),
        };
        // AV1 codes ONE bit depth for the whole sequence — there is no separate
        // chroma depth to disagree with luma (the H.265 gate's extra clause has no
        // counterpart here).
        Ok(Self {
            std_profile,
            chroma_subsampling,
            luma_bit_depth: depth,
            chroma_bit_depth: depth,
            film_grain,
        })
    }

    /// The key for a stream whose shape the SESSION already negotiated but whose
    /// sequence header has not arrived — the construction-time probe's entry point
    /// ([`crate::VkAv1Decoder::probe_stream_support`]).
    ///
    /// `seq_profile` is the one thing the negotiation does not carry, so it is
    /// derived from the pair: 4:2:0 → Main, 4:4:4 → High (4:4:4 is only
    /// expressible in High or Professional, and a punktfunk host encodes it as
    /// High). Everything else goes to Professional, which cannot rescue a
    /// combination [`Self::from_stream`] refuses — ONE gate produces the error.
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

    /// The picture format a session on this profile decodes to, or `None` for a
    /// combination outside the envelope (unreachable off [`Self::from_stream`],
    /// which already gated it).
    ///
    /// Resolved through [`output_format_for`], the crate's one (sampling, depth) →
    /// format map, so the AV1 rung can only ever deliver formats
    /// [`crate::OUTPUT_FORMATS`] already names.
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

/// A complete AV1 decode profile chain in one movable value —
/// [`crate::caps_h265::H265ProfileChain`]'s twin.
///
/// [`Self::wire`] links `profile.p_next` to this struct's OWN `av1` field; the
/// value must not move between `wire()` and the last use of the returned reference.
pub(crate) struct Av1ProfileChain {
    av1: vk::VideoDecodeAV1ProfileInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl Av1ProfileChain {
    /// Build the (unwired) chain for one stream profile.
    pub(crate) fn new(key: Av1ProfileKey) -> Self {
        Self {
            av1: vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(key.std_profile)
                // Stated from the SEQUENCE, never softened to false to make a
                // query pass: a grain-less profile decodes a grain stream into
                // pictures the encoder never intended (module docs).
                .film_grain_support(key.film_grain),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_AV1)
                .chroma_subsampling(key.chroma_subsampling)
                .luma_bit_depth(key.luma_bit_depth)
                .chroma_bit_depth(key.chroma_bit_depth),
        }
    }

    /// Wire the internal `p_next` chain and hand out the profile root. Do not move
    /// `self` while the returned reference (or any pointer taken from it) lives.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'static> {
        self.profile.p_next = (&self.av1 as *const vk::VideoDecodeAV1ProfileInfoKHR<'_>).cast();
        &self.profile
    }
}

/// Everything the thin AV1 query copies out of the driver, hand-buildable for
/// tests. Field-for-field [`crate::RawH265Caps`], except `max_level` carries an
/// AV1 Std level code point.
#[derive(Debug, Clone, Default)]
pub struct RawAv1Caps {
    /// `VkVideoCapabilitiesKHR::flags`.
    pub capability_flags: vk::VideoCapabilityFlagsKHR,
    /// `VkVideoDecodeCapabilitiesKHR::flags` (the coincide/distinct advertisement).
    pub decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    pub min_bitstream_buffer_offset_alignment: u64,
    pub min_bitstream_buffer_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// `VkVideoDecodeAV1CapabilitiesKHR::maxLevel` (index-coded Std level — the
    /// SAME numbering as the bitstream's `seq_level_idx`, which is what makes the
    /// decoder's level gate a plain comparison).
    pub max_level: hh::StdVideoAV1Level,
    /// `VkVideoCapabilitiesKHR::stdHeaderVersion` — session creation echoes it back.
    pub std_header_version: vk::ExtensionProperties,
    /// Formats usable for DISTINCT-mode DPB images (queried with [`DPB_USAGE`]).
    pub dpb_formats: Vec<VideoFormat>,
    /// Formats usable for DISTINCT-mode outputs (queried with [`OUTPUT_USAGE`]).
    pub output_formats: Vec<VideoFormat>,
    /// Formats usable when DPB and output COINCIDE ([`COINCIDE_USAGE`]).
    pub coincide_formats: Vec<VideoFormat>,
}

/// Derive the session-shaping facts from one raw AV1 query, for a stream whose
/// sequence header asks for `wanted` ([`Av1ProfileKey::output_format`]).
///
/// The refusal semantics are the H.265 ones, unchanged: a device advertising AV1
/// decode but listing no [`crate::P010`] entry under a 10-bit profile yields
/// [`CapsError::NoFormat`] here, with the mode and the format named, and NOTHING
/// is created — a clean pre-session demote to the next ladder rung, never a silent
/// fallback to a format that would lose bits.
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

/// The one function that asks the driver about an AV1 profile: video capabilities
/// (with the decode + AV1 capability structs chained) plus the three
/// format-property enumerations. Copies facts out and returns; derivation happens
/// in [`derive_caps_av1`].
///
/// A device that cannot host the profile AT ALL — most importantly the
/// film-grain-enabled one — fails the FIRST call here with a profile-unsupported
/// result, before anything is created. The caller turns that into the ladder's
/// named demote (see [`crate::VkAv1Decoder::probe_stream_support`]).
///
/// # Safety
///
/// `dev` wraps live handles per the [`crate::DeviceHandles`] contract (this calls
/// instance-level functions against its physical device).
pub(crate) unsafe fn query_av1_caps(
    dev: &DecodeDevice,
    key: Av1ProfileKey,
) -> Result<RawAv1Caps, vk::Result> {
    let mut chain = Av1ProfileChain::new(key);
    let profile = chain.wire();

    let mut av1_caps = vk::VideoDecodeAV1CapabilitiesKHR::default();
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    // ⚠ ORDER IS LOAD-BEARING — see the measured Intel Arc swap in
    // [`crate::caps_h265::query_h265_caps`]. `push_next` prepends, so pushing the codec
    // struct FIRST leaves VkVideoDecodeCapabilitiesKHR directly after the base struct.
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
    let max_level = av1_caps.max_level;

    // The three queries carry the REAL creation usages (SAMPLED included for the
    // presenter-facing roles) so the answers validate the images the pools build.
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

    /// A format entry advertising `usage` plus the mutable-format allowance.
    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
            ..Default::default()
        }
    }

    /// RADV's shape (coincide, separate reference images) advertising exactly the
    /// formats in `coincide`.
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
        // Main 4:2:0 8-bit → NV12.
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

        // Main 4:2:0 10-bit → P010, and the profile SAYS 10-bit (a profile
        // claiming 8 would have the driver hand back an 8-bit surface).
        let main10 = Av1ProfileKey::from_stream(0, 1, 10, false).unwrap();
        assert_eq!(
            main10.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert_eq!(main10.output_format(), Some(P010));

        // High is AV1's 4:4:4 profile, both depths.
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

        // The grain flag is part of the PROFILE, so two otherwise identical
        // streams are two different profiles — which is exactly what makes the
        // caps query a real film-grain probe rather than a formality.
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

    /// The negotiated-facts constructor: the client knows the sampling, depth and
    /// grain flag from the host's Welcome long before the first sequence header,
    /// and that is enough to PROBE the device before it commits to this rung.
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
        // It never admits what `from_stream` refuses: outside-envelope shapes come
        // back typed, so the probe REFUSES rather than guessing a profile.
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

        // A grain-less key states VK_FALSE — the two queries are genuinely
        // different questions, which is the whole mechanism.
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

        // The type-erased dispatch builds the SAME chain (the profile-idc
        // confusion `DecodeProfile` exists to prevent would show up right here).
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
        // All three `StdVideo*LevelIdc` types are `c_uint`, and the three code
        // spaces disagree (AV1 5.1 is 13, H.265 5.1 is 12, H.264 5.1 is 51). The
        // tag is what makes the decoder's numeric gate honest.
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
        // The device decodes AV1 and advertises NV12 — but the stream is 10-bit
        // and there is no P010 entry. Refuse by name; do NOT fall back to NV12
        // (that would decode 10-bit content into an 8-bit surface).
        let raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        assert_eq!(
            derive_caps_av1(&raw, P010).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: P010
            }
        );

        // With the P010 entry present it derives, plane views and all.
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
        // NVIDIA's shape: distinct only, layered DPB. The DPB half advertises
        // P010, the OUTPUT half does not — the refusal must say which.
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

        // With both halves carrying it, distinct derives (the DPB entry needs
        // neither SAMPLED nor MUTABLE_FORMAT — reference images are never sampled).
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
        // The Intel-refusal shape, one codec over: the format is listed but not
        // for SAMPLED, so the presenter could never read it.
        let raw = coincide_device(vec![entry(
            NV12,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )]);
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        // And a presenter-facing entry without MUTABLE_FORMAT has no plane views.
        let raw = coincide_device(vec![VideoFormat {
            format: NV12,
            image_usage: COINCIDE_USAGE,
            image_create_flags: vk::ImageCreateFlags::empty(),
            ..Default::default()
        }]);
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)"
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

        // Coincide with a layered DPB stays unsupported here too (the picture-pool
        // model needs per-slot images, whatever the codec).
        let mut raw = coincide_device(vec![entry(NV12, COINCIDE_USAGE)]);
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps_av1(&raw, NV12).unwrap_err(),
            CapsError::CoincideLayeredDpb
        );
    }
}
