//! H.264 decode capability query + derivation.
//!
//! Split on purpose: [`query_h264_caps`] is the one THIN function that talks to the
//! driver (`vkGetPhysicalDeviceVideoCapabilitiesKHR` + the three video-format-property
//! enumerations) and only COPIES facts into [`RawH264Caps`]; [`derive_caps`] turns
//! those facts into the [`DecodeCaps`] the session/image/ring modules consume and is
//! a pure function over a hand-buildable struct — every mode/format decision is
//! unit-tested without a GPU (the RADV-vs-NVIDIA coincide/distinct split is exactly
//! the driver variance the risk register names).

use ash::vk;
use ash::vk::native as hh;

use crate::device::DecodeDevice;

/// The 8-bit 4:2:0 semi-planar format every punktfunk H.264 session decodes to.
/// (P010 joins with the HEVC/10-bit milestone; H.264 in this program is 8-bit.)
pub const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

/// The usage the pools actually create with, per role — the format queries ask the
/// driver about EXACTLY these combinations (a query for less would validate an
/// image nobody creates):
///
/// distinct-mode DPB images: reference-only, never sampled.
pub const DPB_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
/// Distinct-mode output images: decode destination + presenter sampling.
pub const OUTPUT_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw() | vk::ImageUsageFlags::SAMPLED.as_raw(),
);
/// Coincide-mode images: DPB + decode destination + presenter sampling in one.
pub const COINCIDE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR.as_raw()
        | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw(),
);

/// One `VkVideoFormatPropertiesKHR` entry as this crate consumes it: the format
/// plus the driver's advertised usage/create-flag envelope for it — creation must
/// stay INSIDE that envelope (finding of the adversarial round: the flags used to
/// be assumed, not honoured).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoFormat {
    pub format: vk::Format,
    /// `imageUsageFlags` the driver supports for this format under the queried
    /// profile (a superset of the query's usage on a conformant driver).
    pub image_usage: vk::ImageUsageFlags,
    /// `imageCreateFlags` the driver allows — per-plane views require
    /// `MUTABLE_FORMAT` to appear here.
    pub image_create_flags: vk::ImageCreateFlags,
}

/// Everything the thin query copies out of the driver, hand-buildable for tests.
///
/// The three format lists correspond to the three REAL usage combinations the
/// pools create with ([`DPB_USAGE`], [`OUTPUT_USAGE`], [`COINCIDE_USAGE`] — the
/// presenter-facing ones include `SAMPLED`), in the exact shape the driver was
/// asked: a usage the implementation does not support yields an EMPTY list (the
/// thin query maps `VK_ERROR_FORMAT_NOT_SUPPORTED` /
/// `VK_ERROR_IMAGE_USAGE_NOT_SUPPORTED` for that usage to empty rather than
/// failing the whole probe).
#[derive(Debug, Clone, Default)]
pub struct RawH264Caps {
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
    /// `VkVideoDecodeH264CapabilitiesKHR::maxLevelIdc` (index-coded Std level).
    pub max_level_idc: hh::StdVideoH264LevelIdc,
    /// `VkVideoCapabilitiesKHR::stdHeaderVersion` — session creation echoes it back.
    pub std_header_version: vk::ExtensionProperties,
    /// Formats usable for DISTINCT-mode DPB images (queried with [`DPB_USAGE`]).
    pub dpb_formats: Vec<VideoFormat>,
    /// Formats usable for DISTINCT-mode outputs (queried with [`OUTPUT_USAGE`]).
    pub output_formats: Vec<VideoFormat>,
    /// Formats usable when DPB and output COINCIDE ([`COINCIDE_USAGE`]).
    pub coincide_formats: Vec<VideoFormat>,
}

/// The derived facts the rest of the crate keys off. One value per session profile;
/// rebuilt only when the stream renegotiates to a different profile.
#[derive(Debug, Clone)]
pub struct DecodeCaps {
    /// Chosen DPB/output arrangement: `true` = the decode output IS the DPB image
    /// (RADV's shape), `false` = separate DPB array + output images (NVIDIA's).
    /// When a driver advertises both, coincide wins — half the images, and the
    /// mode field data trusts most on the fleet's AMD boxes.
    pub coincide: bool,
    /// `true` when the driver does NOT advertise `SEPARATE_REFERENCE_IMAGES`: every
    /// DPB slot must then be a layer of ONE image array. When separate references
    /// are allowed this stays `false` and each slot gets its own image (simpler
    /// lifetime story; nothing downstream requires the layered arrangement).
    pub layered_dpb: bool,
    /// Bitstream buffer alignments, normalized to at least 1 so ring math never
    /// divides by the zero an uninitialized fixture would carry.
    pub min_bitstream_offset_alignment: u64,
    pub min_bitstream_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    pub max_level_idc: hh::StdVideoH264LevelIdc,
    /// DPB image format (== `output_format` in coincide mode).
    pub dpb_format: vk::Format,
    /// Decode-output image format.
    pub output_format: vk::Format,
    pub std_header_version: vk::ExtensionProperties,
}

impl DecodeCaps {
    /// `coded` rounded up to the device's `pictureAccessGranularity` — the extent
    /// pool IMAGES are created at (the per-picture `codedExtent` stays the stream's
    /// coded size; only the backing store rounds up). A zero granularity axis (an
    /// uninitialized fixture) degrades to 1.
    pub fn aligned_extent(&self, coded: vk::Extent2D) -> vk::Extent2D {
        let round = |value: u32, granularity: u32| -> u32 {
            let granularity = granularity.max(1);
            value.div_ceil(granularity) * granularity
        };
        vk::Extent2D {
            width: round(coded.width, self.picture_access_granularity.width),
            height: round(coded.height, self.picture_access_granularity.height),
        }
    }
}

/// Raw caps that do not add up to a usable decoder. All of these are device gaps
/// the caller demotes on (the ladder's next rung), not stream conditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsError {
    /// The driver advertises neither COINCIDE nor DISTINCT — no way to arrange a
    /// DPB at all (a broken driver; the spec requires at least one).
    NoDecodeMode,
    /// The mode's format list does not contain [`NV12`]. `mode` names which list.
    NoNv12Format { mode: &'static str },
    /// The driver's NV12 entry for `mode` does not advertise every usage bit the
    /// pool would create with (`missing` names the gap) — creating anyway would be
    /// a silent VUID violation.
    UsageUnsupported {
        mode: &'static str,
        missing: vk::ImageUsageFlags,
    },
    /// The presenter-facing NV12 entry for `mode` does not allow `MUTABLE_FORMAT`,
    /// so the per-plane `R8`/`R8G8` views the presenter samples through cannot
    /// exist on this device.
    NoMutableFormat { mode: &'static str },
    /// The driver forces COINCIDE mode AND a layered DPB (one image array, no
    /// `SEPARATE_REFERENCE_IMAGES`): the picture-pool model — a re-activated slot
    /// binding a fresh free image, so delivered pictures are never decode targets
    /// — cannot exist when every slot is a fixed layer of one array. No fleet
    /// device has this shape (NVIDIA = distinct, RADV = separate reference
    /// images); a device that does demotes to the next decoder rung rather than
    /// getting a degraded copy path built for it.
    CoincideLayeredDpb,
}

impl std::fmt::Display for CapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsError::NoDecodeMode => {
                write!(
                    f,
                    "driver advertises neither DPB_AND_OUTPUT_COINCIDE nor DISTINCT"
                )
            }
            CapsError::NoNv12Format { mode } => {
                write!(f, "no NV12 in the {mode} video format properties")
            }
            CapsError::UsageUnsupported { mode, missing } => {
                write!(
                    f,
                    "the {mode} NV12 entry does not advertise usage {missing:?}"
                )
            }
            CapsError::NoMutableFormat { mode } => {
                write!(
                    f,
                    "the {mode} NV12 entry does not allow MUTABLE_FORMAT (per-plane views)"
                )
            }
            CapsError::CoincideLayeredDpb => {
                write!(
                    f,
                    "coincide mode with a layered DPB (no SEPARATE_REFERENCE_IMAGES) — \
                     the picture-pool model needs per-slot images; demote this device"
                )
            }
        }
    }
}

impl std::error::Error for CapsError {}

/// Derive the session-shaping facts from one raw query. Pure — the whole
/// coincide/distinct/layered decision table lives here and in the tests below.
pub fn derive_caps(raw: &RawH264Caps) -> Result<DecodeCaps, CapsError> {
    let coincide = raw
        .decode_flags
        .contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
    let distinct = raw
        .decode_flags
        .contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
    if !coincide && !distinct {
        return Err(CapsError::NoDecodeMode);
    }

    let layered_dpb = !raw
        .capability_flags
        .contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES);
    // Coincide preferred when both are offered (struct docs). Each picked entry is
    // validated against the EXACT usage/create-flags its pool will use: presenter-
    // facing images (coincide pool, distinct outputs) additionally need
    // MUTABLE_FORMAT for their per-plane views; the distinct DPB needs neither
    // sampling nor plane views.
    let (dpb_format, output_format) = if coincide {
        if layered_dpb {
            // The picture-pool model needs per-slot images (a slot re-binds a
            // fresh image at activation); one fixed layer per slot cannot do that.
            return Err(CapsError::CoincideLayeredDpb);
        }
        let mode = "coincide (DPB|DST|SAMPLED)";
        let entry = pick_nv12(&raw.coincide_formats, mode)?;
        require_usage(&entry, COINCIDE_USAGE, mode)?;
        require_mutable(&entry, mode)?;
        (entry.format, entry.format)
    } else {
        let dpb = pick_nv12(&raw.dpb_formats, "DPB")?;
        require_usage(&dpb, DPB_USAGE, "DPB")?;
        let out_mode = "output (DST|SAMPLED)";
        let output = pick_nv12(&raw.output_formats, out_mode)?;
        require_usage(&output, OUTPUT_USAGE, out_mode)?;
        require_mutable(&output, out_mode)?;
        (dpb.format, output.format)
    };

    Ok(DecodeCaps {
        coincide,
        layered_dpb,
        min_bitstream_offset_alignment: raw.min_bitstream_buffer_offset_alignment.max(1),
        min_bitstream_size_alignment: raw.min_bitstream_buffer_size_alignment.max(1),
        picture_access_granularity: raw.picture_access_granularity,
        min_coded_extent: raw.min_coded_extent,
        max_coded_extent: raw.max_coded_extent,
        max_dpb_slots: raw.max_dpb_slots,
        max_active_references: raw.max_active_reference_pictures,
        max_level_idc: raw.max_level_idc,
        dpb_format,
        output_format,
        std_header_version: raw.std_header_version,
    })
}

fn pick_nv12(formats: &[VideoFormat], mode: &'static str) -> Result<VideoFormat, CapsError> {
    formats
        .iter()
        .copied()
        .find(|f| f.format == NV12)
        .ok_or(CapsError::NoNv12Format { mode })
}

/// The pool's creation usage must sit inside the driver's advertised envelope.
fn require_usage(
    entry: &VideoFormat,
    usage: vk::ImageUsageFlags,
    mode: &'static str,
) -> Result<(), CapsError> {
    let missing = usage & !entry.image_usage;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(CapsError::UsageUnsupported { mode, missing })
    }
}

fn require_mutable(entry: &VideoFormat, mode: &'static str) -> Result<(), CapsError> {
    if entry
        .image_create_flags
        .contains(vk::ImageCreateFlags::MUTABLE_FORMAT)
    {
        Ok(())
    } else {
        Err(CapsError::NoMutableFormat { mode })
    }
}

/// A complete H.264 decode profile chain in one movable value, mirroring the
/// encoder's `RgbProfileStack`: profile identity in Vulkan is BY VALUE, so every
/// consumer (caps query, session create, image/buffer create, query pool create)
/// rebuilds a structurally identical chain rather than sharing pointers.
///
/// [`Self::wire`] links `profile.p_next` to this struct's OWN `h264` field; the
/// value must not move between `wire()` and the last use of the returned reference
/// (the borrow checker pins it — `wire` borrows `self` for the reference's life).
pub(crate) struct H264ProfileChain {
    h264: vk::VideoDecodeH264ProfileInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl H264ProfileChain {
    /// Build the (unwired) chain for one SPS profile. `std_profile_idc` is the
    /// value WP-A's conversion validated (66/77/100/244 pass-through); H.264 here
    /// is 8-bit 4:2:0 progressive by the program envelope.
    pub(crate) fn new(std_profile_idc: hh::StdVideoH264ProfileIdc) -> Self {
        Self {
            h264: vk::VideoDecodeH264ProfileInfoKHR::default()
                .std_profile_idc(std_profile_idc)
                .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8),
        }
    }

    /// Wire the internal `p_next` chain and hand out the profile root. Do not move
    /// `self` while the returned reference (or any pointer taken from it) lives.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'static> {
        self.profile.p_next = (&self.h264 as *const vk::VideoDecodeH264ProfileInfoKHR<'_>).cast();
        &self.profile
    }
}

/// The one function that asks the driver: video capabilities (with the decode +
/// H.264 capability structs chained) plus the three format-property enumerations.
/// Copies facts out and returns; derivation happens in [`derive_caps`].
///
/// # Safety
///
/// `dev` wraps live handles per the [`crate::DeviceHandles`] contract (this calls
/// instance-level functions against its physical device).
pub(crate) unsafe fn query_h264_caps(
    dev: &DecodeDevice,
    std_profile_idc: hh::StdVideoH264ProfileIdc,
) -> Result<RawH264Caps, vk::Result> {
    let mut chain = H264ProfileChain::new(std_profile_idc);
    let profile = chain.wire();

    let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push_next(&mut decode_caps)
        .push_next(&mut h264_caps);
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
    let max_level_idc = h264_caps.max_level_idc;

    // The three queries carry the REAL creation usages (SAMPLED included for the
    // presenter-facing roles) so the answers validate the images the pools build.
    // SAFETY: same liveness as above; the helper wires its own chain (this and
    // the two calls below).
    let dpb_formats = unsafe { query_formats(dev, std_profile_idc, DPB_USAGE)? };
    // SAFETY: as above.
    let output_formats = unsafe { query_formats(dev, std_profile_idc, OUTPUT_USAGE)? };
    // SAFETY: as above.
    let coincide_formats = unsafe { query_formats(dev, std_profile_idc, COINCIDE_USAGE)? };

    Ok(RawH264Caps {
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

/// Enumerate the video format properties for one usage combination. A usage the
/// implementation rejects outright maps to an EMPTY list (that is the driver saying
/// "not this arrangement", which [`derive_caps`] then routes around).
///
/// # Safety
///
/// As [`query_h264_caps`].
unsafe fn query_formats(
    dev: &DecodeDevice,
    std_profile_idc: hh::StdVideoH264ProfileIdc,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<VideoFormat>, vk::Result> {
    let mut chain = H264ProfileChain::new(std_profile_idc);
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(usage)
        .push_next(&mut profile_list);

    let fp = dev
        .video_queue_instance()
        .fp()
        .get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    // SAFETY: live physical device; `info` roots a wired chain outliving the call;
    // null properties pointer is the spec's count-query form.
    let r = unsafe {
        fp(
            dev.physical_device(),
            &info,
            &mut count,
            std::ptr::null_mut(),
        )
    };
    match r {
        vk::Result::SUCCESS => {}
        // "This usage/profile combination has no formats" — an arrangement gap,
        // not a failure (derive_caps decides whether a usable mode remains).
        vk::Result::ERROR_FORMAT_NOT_SUPPORTED
        | vk::Result::ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR => return Ok(Vec::new()),
        err => return Err(err),
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    // SAFETY: as above, with a properties array of exactly the driver-reported count.
    let r = unsafe { fp(dev.physical_device(), &info, &mut count, props.as_mut_ptr()) };
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return Err(r);
    }
    props.truncate(count as usize);
    Ok(props
        .iter()
        .map(|p| VideoFormat {
            format: p.format,
            image_usage: p.image_usage_flags,
            image_create_flags: p.image_create_flags,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A format entry advertising `usage` plus the mutable-format allowance.
    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT
                | vk::ImageCreateFlags::ALIAS
                | vk::ImageCreateFlags::EXTENDED_USAGE,
        }
    }

    /// A raw-caps fixture in RADV's shape: coincide advertised, separate reference
    /// images allowed, sane alignments.
    fn radv_like() -> RawH264Caps {
        RawH264Caps {
            capability_flags: vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES,
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE,
            min_bitstream_buffer_offset_alignment: 128,
            min_bitstream_buffer_size_alignment: 128,
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
            max_level_idc: hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
            std_header_version: vk::ExtensionProperties::default(),
            dpb_formats: vec![],
            output_formats: vec![],
            coincide_formats: vec![
                entry(NV12, COINCIDE_USAGE),
                entry(
                    vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
                    COINCIDE_USAGE,
                ),
            ],
        }
    }

    /// NVIDIA's shape: distinct only, NO separate reference images (layered DPB
    /// array), and only the distinct-mode format lists populated. The DPB entry
    /// deliberately advertises NEITHER sampling nor mutable formats — reference
    /// arrays need neither, and requiring them there would fail real devices.
    fn nvidia_like() -> RawH264Caps {
        RawH264Caps {
            capability_flags: vk::VideoCapabilityFlagsKHR::empty(),
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT,
            dpb_formats: vec![VideoFormat {
                format: NV12,
                image_usage: DPB_USAGE,
                image_create_flags: vk::ImageCreateFlags::empty(),
            }],
            output_formats: vec![entry(NV12, OUTPUT_USAGE)],
            coincide_formats: vec![],
            ..radv_like()
        }
    }

    #[test]
    fn a_coincide_device_derives_coincide_with_one_shared_format() {
        let caps = derive_caps(&radv_like()).unwrap();
        assert!(caps.coincide);
        assert!(
            !caps.layered_dpb,
            "separate reference images advertised — per-slot images"
        );
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(caps.output_format, NV12);
        assert_eq!(caps.max_dpb_slots, 17);
        assert_eq!(caps.min_bitstream_offset_alignment, 128);
    }

    #[test]
    fn a_distinct_device_derives_distinct_with_a_layered_dpb() {
        let caps = derive_caps(&nvidia_like()).unwrap();
        assert!(!caps.coincide);
        assert!(
            caps.layered_dpb,
            "no SEPARATE_REFERENCE_IMAGES — one image array carries every slot"
        );
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(caps.output_format, NV12);
    }

    #[test]
    fn a_device_advertising_both_modes_prefers_coincide() {
        let mut raw = radv_like();
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE
            | vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT;
        raw.dpb_formats = vec![entry(NV12, DPB_USAGE)];
        raw.output_formats = vec![entry(NV12, OUTPUT_USAGE)];
        let caps = derive_caps(&raw).unwrap();
        assert!(caps.coincide, "coincide wins when both are offered");
    }

    #[test]
    fn no_mode_and_no_nv12_are_distinct_hard_errors() {
        let mut raw = radv_like();
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::empty();
        assert_eq!(derive_caps(&raw).unwrap_err(), CapsError::NoDecodeMode);

        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            COINCIDE_USAGE,
        )];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoNv12Format {
                mode: "coincide (DPB|DST|SAMPLED)"
            }
        );

        // Distinct mode reports which HALF is missing NV12.
        let mut raw = nvidia_like();
        raw.output_formats = vec![];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoNv12Format {
                mode: "output (DST|SAMPLED)"
            }
        );
    }

    #[test]
    fn an_advertised_usage_missing_a_creation_bit_is_an_error_naming_the_gap() {
        // A coincide entry that supports decode but NOT sampling: the presenter
        // cannot read it, so derivation must refuse rather than create anyway.
        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(
            NV12,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        // Same on the distinct output half.
        let mut raw = nvidia_like();
        raw.output_formats = vec![entry(NV12, vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR)];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "output (DST|SAMPLED)",
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );
    }

    #[test]
    fn a_presenter_facing_entry_without_mutable_format_is_refused() {
        let mut raw = radv_like();
        raw.coincide_formats = vec![VideoFormat {
            format: NV12,
            image_usage: COINCIDE_USAGE,
            image_create_flags: vk::ImageCreateFlags::empty(),
        }];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)"
            }
        );

        // The distinct DPB entry needs NO mutable-format allowance (nvidia_like's
        // DPB entry has empty create flags and derives fine).
        assert!(derive_caps(&nvidia_like()).is_ok());
    }

    #[test]
    fn extents_round_up_to_the_picture_access_granularity() {
        let mut raw = radv_like();
        raw.picture_access_granularity = vk::Extent2D {
            width: 64,
            height: 16,
        };
        let caps = derive_caps(&raw).unwrap();
        // 1920x1080: width already aligned, height rounds to 1088 — the exact
        // padded shape the old smeared-rows class came from, now explicit.
        let aligned = caps.aligned_extent(vk::Extent2D {
            width: 1920,
            height: 1080,
        });
        assert_eq!((aligned.width, aligned.height), (1920, 1088));

        // Granularity 1 is the identity; a zero axis degrades to 1, not a panic.
        let mut raw = radv_like();
        raw.picture_access_granularity = vk::Extent2D {
            width: 0,
            height: 1,
        };
        let caps = derive_caps(&raw).unwrap();
        let aligned = caps.aligned_extent(vk::Extent2D {
            width: 321,
            height: 241,
        });
        assert_eq!((aligned.width, aligned.height), (321, 241));
    }

    #[test]
    fn coincide_with_a_layered_dpb_is_unsupported_not_worked_around() {
        // A driver forcing coincide AND a single layered DPB array: the pool
        // model (fresh image per activation) cannot exist there, and no fleet
        // device has this shape — refuse so the ladder demotes.
        let mut raw = radv_like();
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::CoincideLayeredDpb
        );
    }

    #[test]
    fn zero_alignments_normalize_to_one_so_ring_math_never_divides_by_zero() {
        let mut raw = radv_like();
        raw.min_bitstream_buffer_offset_alignment = 0;
        raw.min_bitstream_buffer_size_alignment = 0;
        let caps = derive_caps(&raw).unwrap();
        assert_eq!(caps.min_bitstream_offset_alignment, 1);
        assert_eq!(caps.min_bitstream_size_alignment, 1);
    }

    #[test]
    fn the_profile_chain_wires_h264_behind_the_root_profile() {
        let mut chain =
            H264ProfileChain::new(hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H264
        );
        assert!(!profile.p_next.is_null());
        // SAFETY: wire() pointed p_next at chain's own h264 field, which lives for
        // this whole scope and is a valid VideoDecodeH264ProfileInfoKHR.
        let h264 = unsafe {
            &*profile
                .p_next
                .cast::<vk::VideoDecodeH264ProfileInfoKHR<'_>>()
        };
        assert_eq!(
            h264.std_profile_idc,
            hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN
        );
        assert_eq!(
            h264.picture_layout,
            vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE
        );
    }
}
