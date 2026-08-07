//! H.264 decode capability query + derivation, plus the codec-agnostic pieces the
//! H.265 sibling ([`crate::caps_h265`]) reuses: the picture-format vocabulary, the
//! coincide/distinct/layered arrangement decision, and the profile chain every
//! Vulkan object of a session is created against.
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

use crate::caps_av1::Av1ProfileChain;
use crate::caps_av1::Av1ProfileKey;
use crate::caps_h265::H265ProfileChain;
use crate::caps_h265::H265ProfileKey;
use crate::device::DecodeDevice;

/// The 8-bit 4:2:0 semi-planar format every punktfunk H.264 session decodes to,
/// and the H.265 Main one ([`crate::caps_h265::output_format_for`] picks per SPS).
pub const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// 10-bit 4:2:0 (P010's Vulkan spelling): H.265 Main 10's picture format.
pub const P010: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;
/// 8-bit 4:4:4 two-plane: H.265 RExt 4:4:4 8-bit, where the device advertises it.
pub const YUV444_8: vk::Format = vk::Format::G8_B8R8_2PLANE_444_UNORM;
/// 10-bit 4:4:4 two-plane: H.265 RExt 4:4:4 10-bit, where the device advertises it.
pub const YUV444_10: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16;

/// EVERY picture format a pf-vkdecode session can deliver — this crate's whole
/// output vocabulary, in one place.
///
/// It exists so a CONSUMER's per-format table (the presenter's CSC bit-depth and
/// MSB-packing map) can be pinned against the PRODUCER rather than against a
/// hand-copied list of its own: a fifth format added here (12-bit RExt, say) breaks
/// the consumer's test instead of silently rendering through that consumer's
/// fallback. [`plane_formats`] and [`crate::caps_h265::output_format_for`] are both
/// tested to agree with it, so the vocabulary can only grow in one edit.
pub const OUTPUT_FORMATS: [vk::Format; 4] = [NV12, P010, YUV444_8, YUV444_10];

/// The `R*`/`R*G*` per-plane view formats the presenter's sampler path needs for
/// one picture format, or `None` for a format this crate has no plane mapping for.
///
/// Per-plane views exist only under `MUTABLE_FORMAT` and must be format-compatible
/// with the plane they alias (spec: "Compatible formats of planes of multi-planar
/// formats", table 49.1): the 8-bit two-plane families take `R8`/`R8G8`, the
/// 10-bit `3PACK16` families take `R10X6`/`R10X6G10X6` — sampling a 10-bit plane
/// through an `R8` view would silently read half the bits of every sample, which
/// is exactly the class of silent-wrongness this crate refuses to ship.
/// (Comparisons rather than a `match`: `vk::Format` is a newtype over `i32` whose
/// field is private to ash, so its constants are not structural-match patterns.)
pub fn plane_formats(format: vk::Format) -> Option<[vk::Format; 2]> {
    if format == NV12 || format == YUV444_8 {
        Some([vk::Format::R8_UNORM, vk::Format::R8G8_UNORM])
    } else if format == P010 || format == YUV444_10 {
        Some([
            vk::Format::R10X6_UNORM_PACK16,
            vk::Format::R10X6G10X6_UNORM_2PACK16,
        ])
    } else {
        None
    }
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub format: vk::Format,
    /// `imageUsageFlags` the driver supports for this format under the queried
    /// profile (a superset of the query's usage on a conformant driver).
    pub image_usage: vk::ImageUsageFlags,
    /// `imageCreateFlags` the driver allows — per-plane views require
    /// `MUTABLE_FORMAT` to appear here.
    ///
    /// ⚠ This field is also the ONLY gate on `VK_IMAGE_CREATE_EXTENDED_USAGE_BIT`,
    /// which is the spec's one escape hatch from `image_usage`: `supportedVideoFormat`
    /// (VUID-VkImageCreateInfo-pNext-06811) admits a usage bit outside `image_usage`
    /// only when `VkImageCreateInfo::flags` includes `EXTENDED_USAGE`, and admits that
    /// flag only when it is "also set in `VkVideoFormatPropertiesKHR::imageCreateFlags`"
    /// (or is `VIDEO_PROFILE_INDEPENDENT`, which needs `VK_KHR_video_maintenance1`).
    /// So an EMPTY value here closes the escape hatch as well as the door — measured on
    /// Intel Arc, where it is empty for every profile ([`crate::probe`] docs).
    pub image_create_flags: vk::ImageCreateFlags,
    /// `imageType` — the image type this format may be created with. Part of the
    /// `supportedVideoFormat` match (VUID-06811 compares it for EQUALITY), so it is
    /// recorded rather than assumed; every fleet driver reports `TYPE_2D`, which is
    /// what [`crate::images`] creates.
    pub image_type: vk::ImageType,
    /// `imageTiling` — likewise compared for equality by VUID-06811; every fleet
    /// driver reports `OPTIMAL`.
    pub image_tiling: vk::ImageTiling,
}

impl Default for VideoFormat {
    /// The shape the pools create with (`TYPE_2D` + `OPTIMAL`), so a fixture that
    /// names only the interesting fields still describes a creatable image.
    fn default() -> Self {
        Self {
            format: vk::Format::UNDEFINED,
            image_usage: vk::ImageUsageFlags::empty(),
            image_create_flags: vk::ImageCreateFlags::empty(),
            image_type: vk::ImageType::TYPE_2D,
            image_tiling: vk::ImageTiling::OPTIMAL,
        }
    }
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

/// A device's decode level ceiling, tagged with the codec whose Std code space it
/// is stated in.
///
/// `StdVideoH264LevelIdc`, `StdVideoH265LevelIdc` and `StdVideoAV1Level` are all
/// `c_uint` aliases, so nothing stops one being assigned where another belongs —
/// the compiler is silent and the numbers even look plausible (H.264 level 4.1 and
/// H.265 level 4.1 are different code points; AV1 5.1 is 13 where H.265 5.1 is 12).
/// This is the confusion `DecodeProfile` was introduced to make unrepresentable for
/// profiles; the level ceiling gets the same treatment, so a caps derivation has to
/// SAY which codec's query it copied.
///
/// The gate itself stays a numeric comparison against [`Self::code_point`]: within
/// ONE codec the Std code points ascend with the level, which is exactly what makes
/// "stream level > device ceiling ⇒ refuse" sound. Across codecs the comparison is
/// meaningless, which is why the value carries its codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxLevelIdc {
    /// `VkVideoDecodeH264CapabilitiesKHR::maxLevelIdc`.
    H264(hh::StdVideoH264LevelIdc),
    /// `VkVideoDecodeH265CapabilitiesKHR::maxLevelIdc`.
    H265(hh::StdVideoH265LevelIdc),
    /// `VkVideoDecodeAV1CapabilitiesKHR::maxLevel`. Unlike the other two this code
    /// space is the BITSTREAM's own: `StdVideoAV1Level` is index-coded exactly like
    /// AV1's `seq_level_idx` (2.0 = 0, 2.1 = 1, … 7.3 = 23), so the decoder's gate
    /// compares the sequence header's value against it directly.
    Av1(hh::StdVideoAV1Level),
}

impl MaxLevelIdc {
    /// The raw Std code point, for the decoders' level gate and its error text.
    /// Compare it only against a code point of the SAME codec (the variant says
    /// which) — the tag is the whole point of the type.
    pub fn code_point(self) -> u32 {
        match self {
            MaxLevelIdc::H264(level) | MaxLevelIdc::H265(level) | MaxLevelIdc::Av1(level) => level,
        }
    }
}

impl std::fmt::Display for MaxLevelIdc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaxLevelIdc::H264(level) => write!(f, "H.264 Std level {level}"),
            MaxLevelIdc::H265(level) => write!(f, "H.265 Std level {level}"),
            MaxLevelIdc::Av1(level) => write!(f, "AV1 Std level {level}"),
        }
    }
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
    /// The device's `maxLevelIdc` for this session's codec, codec-TAGGED.
    pub max_level_idc: MaxLevelIdc,
    /// DPB image format (== `output_format` in coincide mode).
    pub dpb_format: vk::Format,
    /// Decode-output image format.
    pub output_format: vk::Format,
    /// The per-plane view formats of [`Self::output_format`] ([`plane_formats`]) —
    /// resolved at derivation so the pool never has to re-derive (or guess) them.
    pub plane_view_formats: [vk::Format; 2],
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
    /// The mode's format list does not contain the picture format the stream
    /// needs. `mode` names which list, `wanted` the format: [`NV12`] for H.264
    /// and H.265 Main, [`P010`] for Main 10, the 4:4:4 pair for RExt streams —
    /// the Main-10-on-an-8-bit-only-device and 4:4:4-on-a-4:2:0-only-device
    /// refusals both land here, BEFORE any session exists.
    NoFormat {
        mode: &'static str,
        wanted: vk::Format,
    },
    /// The picture format the stream needs has no per-plane view mapping in this
    /// crate ([`plane_formats`]) — unreachable for the four formats the envelope
    /// admits; a guard against a future format arriving without its plane views.
    NoPlaneMapping { format: vk::Format },
    /// The driver's entry for the wanted format in `mode` does not advertise every
    /// usage bit the pool would create with (`missing` names the gap) — creating
    /// anyway would be a silent VUID violation.
    UsageUnsupported {
        mode: &'static str,
        /// The picture format whose entry fell short — NOT always NV12 (a Main 10
        /// stream is refused about P010), which is what this used to say regardless.
        format: vk::Format,
        missing: vk::ImageUsageFlags,
    },
    /// The presenter-facing entry for `mode` does not allow `MUTABLE_FORMAT`, so
    /// the per-plane views the presenter samples through ([`plane_formats`])
    /// cannot exist on this device.
    NoMutableFormat {
        mode: &'static str,
        format: vk::Format,
    },
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
            CapsError::NoFormat { mode, wanted } => {
                write!(
                    f,
                    "no {wanted:?} in the {mode} video format properties for this profile"
                )
            }
            CapsError::NoPlaneMapping { format } => {
                write!(f, "no per-plane view mapping for {format:?}")
            }
            CapsError::UsageUnsupported {
                mode,
                format,
                missing,
            } => {
                write!(
                    f,
                    "the {mode} {format:?} entry does not advertise usage {missing:?}"
                )?;
                // Name the CONSEQUENCE for the one missing bit that is not a passing
                // driver quirk. Without `SAMPLED` nothing in a shader can read the
                // decoded picture, so the zero-copy path this rung exists for cannot be
                // built here at all — a fact worth stating in the log line rather than
                // leaving a field reporter to work out from a flag name. Measured on
                // Intel Arc (Windows 101.8861), where the whole advertised envelope is
                // TRANSFER_SRC|DECODE_DST|DECODE_DPB with no image create flags:
                // `punktfunk-session --probe-decode` prints the driver's own answer.
                if missing.contains(vk::ImageUsageFlags::SAMPLED) {
                    write!(
                        f,
                        " — no shader can read this device's decoded pictures, so the \
                         zero-copy path cannot exist on it (see --probe-decode)"
                    )?;
                }
                Ok(())
            }
            CapsError::NoMutableFormat { mode, format } => {
                write!(
                    f,
                    "the {mode} {format:?} entry does not allow MUTABLE_FORMAT (per-plane views)"
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

/// Derive the session-shaping facts from one raw H.264 query. Pure — the whole
/// coincide/distinct/layered decision table lives in `derive_arrangement` (shared
/// with the H.265 side) and in the tests below. H.264 in this program is 8-bit
/// 4:2:0, so the wanted picture format is always [`NV12`].
pub fn derive_caps(raw: &RawH264Caps) -> Result<DecodeCaps, CapsError> {
    let arrangement = derive_arrangement(
        raw.capability_flags,
        raw.decode_flags,
        NV12,
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
        MaxLevelIdc::H264(raw.max_level_idc),
        raw.std_header_version,
    ))
}

/// The codec-agnostic half of derivation: which DPB arrangement this device can
/// host, and which format lists satisfy the picture format `wanted`.
pub(crate) struct Arrangement {
    coincide: bool,
    layered_dpb: bool,
    dpb_format: vk::Format,
    output_format: vk::Format,
    plane_view_formats: [vk::Format; 2],
}

impl Arrangement {
    /// Fold in the codec-specific numbers the raw query carried. (One function
    /// rather than a shared raw-caps struct: the two raw structs differ only in
    /// which codec's `maxLevelIdc` they copied, and pinning that difference in the
    /// TYPE is worth more than saving these arguments — hence [`MaxLevelIdc`],
    /// which each codec's derivation has to name its own variant of.)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn into_caps(
        self,
        min_bitstream_offset_alignment: u64,
        min_bitstream_size_alignment: u64,
        picture_access_granularity: vk::Extent2D,
        min_coded_extent: vk::Extent2D,
        max_coded_extent: vk::Extent2D,
        max_dpb_slots: u32,
        max_active_references: u32,
        max_level_idc: MaxLevelIdc,
        std_header_version: vk::ExtensionProperties,
    ) -> DecodeCaps {
        DecodeCaps {
            coincide: self.coincide,
            layered_dpb: self.layered_dpb,
            min_bitstream_offset_alignment: min_bitstream_offset_alignment.max(1),
            min_bitstream_size_alignment: min_bitstream_size_alignment.max(1),
            picture_access_granularity,
            min_coded_extent,
            max_coded_extent,
            max_dpb_slots,
            max_active_references,
            max_level_idc,
            dpb_format: self.dpb_format,
            output_format: self.output_format,
            plane_view_formats: self.plane_view_formats,
            std_header_version,
        }
    }
}

/// Decide the DPB arrangement and validate `wanted` against the format lists of
/// the roles that arrangement creates images in. Pure; shared by both codecs — the
/// only codec-dependent input is `wanted`, which the H.265 side derives from the
/// SPS's chroma format and bit depth ([`crate::caps_h265::output_format_for`]).
pub(crate) fn derive_arrangement(
    capability_flags: vk::VideoCapabilityFlagsKHR,
    decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    wanted: vk::Format,
    dpb_formats: &[VideoFormat],
    output_formats: &[VideoFormat],
    coincide_formats: &[VideoFormat],
) -> Result<Arrangement, CapsError> {
    let coincide =
        decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
    let distinct =
        decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
    if !coincide && !distinct {
        return Err(CapsError::NoDecodeMode);
    }

    let layered_dpb =
        !capability_flags.contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES);
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
        let entry = pick_format(coincide_formats, wanted, mode)?;
        require_usage(&entry, COINCIDE_USAGE, mode)?;
        require_mutable(&entry, mode)?;
        (entry.format, entry.format)
    } else {
        let dpb = pick_format(dpb_formats, wanted, "DPB")?;
        require_usage(&dpb, DPB_USAGE, "DPB")?;
        let out_mode = "output (DST|SAMPLED)";
        let output = pick_format(output_formats, wanted, out_mode)?;
        require_usage(&output, OUTPUT_USAGE, out_mode)?;
        require_mutable(&output, out_mode)?;
        (dpb.format, output.format)
    };
    let plane_view_formats = plane_formats(output_format).ok_or(CapsError::NoPlaneMapping {
        format: output_format,
    })?;

    Ok(Arrangement {
        coincide,
        layered_dpb,
        dpb_format,
        output_format,
        plane_view_formats,
    })
}

fn pick_format(
    formats: &[VideoFormat],
    wanted: vk::Format,
    mode: &'static str,
) -> Result<VideoFormat, CapsError> {
    formats
        .iter()
        .copied()
        .find(|f| f.format == wanted)
        .ok_or(CapsError::NoFormat { mode, wanted })
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
        // The entry's OWN format, not the caller's `wanted`: they are equal here (the
        // entry was picked by format), and taking it from the driver's record keeps the
        // message describing what the driver actually said.
        Err(CapsError::UsageUnsupported {
            mode,
            format: entry.format,
            missing,
        })
    }
}

fn require_mutable(entry: &VideoFormat, mode: &'static str) -> Result<(), CapsError> {
    if entry
        .image_create_flags
        .contains(vk::ImageCreateFlags::MUTABLE_FORMAT)
    {
        Ok(())
    } else {
        Err(CapsError::NoMutableFormat {
            mode,
            format: entry.format,
        })
    }
}

/// A complete H.264 decode profile chain in one movable value, mirroring the
/// encoder's `RgbProfileStack`: profile identity in Vulkan is BY VALUE, so every
/// consumer (caps query, session create, image/buffer create, query pool create)
/// rebuilds a structurally identical chain rather than sharing pointers.
///
/// [`Self::wire`] links `profile.p_next` to this struct's OWN `h264` field; the
/// value must not move between `wire()` and the last use of the returned reference
/// — **or of any raw pointer taken from it**, which is the half that does not come
/// for free. `wire` borrows `self` for the reference's life, so wherever the
/// reference is passed on AS a reference the borrow checker does pin the chain: a
/// `profiles(std::slice::from_ref(profile))` builder carries the borrow in its own
/// lifetime parameter, and so does handing `profile` straight to an entry point.
/// Where a `*const` is taken instead, the borrow ENDS at that line and nothing but
/// inspection keeps the chain still — [`crate::decoder`]'s query pool must do
/// exactly that (`push_next` there would clobber the profile's own `p_next`), so it
/// holds the reference across the call in a helper's SIGNATURE
/// (`OpRing::create_status_query_pool`) rather than relying on this sentence.
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

/// Which codec profile a session — and therefore every image, buffer and query
/// pool created for it — is built against. A plain `Copy` descriptor rather than a
/// chain, because profile identity in Vulkan is BY VALUE: each consumer rebuilds
/// its own structurally identical chain from this, and nothing shares pointers.
///
/// It is also the reason this type exists at all: `StdVideoH264ProfileIdc`,
/// `StdVideoH265ProfileIdc` and `StdVideoAV1Profile` are ALL `c_uint`, so a bare
/// idc parameter would let one codec's profile silently build another's chain —
/// the images and the session would then disagree about the profile and the driver
/// would reject (or worse, accept) at submit time. The enum makes that mistake
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeProfile {
    H264(hh::StdVideoH264ProfileIdc),
    H265(H265ProfileKey),
    /// AV1's key additionally carries `filmGrainSupport`, which is part of the
    /// profile — so an image pool built for a grain stream is a different pool
    /// from one built for a grain-less one, by construction
    /// ([`crate::caps_av1`] module docs).
    Av1(Av1ProfileKey),
}

impl DecodeProfile {
    /// A fresh, UNWIRED chain for this profile. Call [`ProfileChain::wire`] on the
    /// returned value and keep it immobile for as long as the wired pointers live.
    pub(crate) fn chain(self) -> ProfileChain {
        match self {
            DecodeProfile::H264(idc) => ProfileChain::H264(H264ProfileChain::new(idc)),
            DecodeProfile::H265(key) => ProfileChain::H265(H265ProfileChain::new(key)),
            DecodeProfile::Av1(key) => ProfileChain::Av1(Av1ProfileChain::new(key)),
        }
    }
}

/// One codec's profile chain, type-erased for the shared creation paths (images,
/// bitstream ring, query pool). Same immobility contract as the three variants.
pub(crate) enum ProfileChain {
    H264(H264ProfileChain),
    H265(H265ProfileChain),
    Av1(Av1ProfileChain),
}

impl ProfileChain {
    /// Wire the chain and hand out the profile root. Do not move `self` while the
    /// returned reference (or any pointer taken from it) lives.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'static> {
        match self {
            ProfileChain::H264(chain) => chain.wire(),
            ProfileChain::H265(chain) => chain.wire(),
            ProfileChain::Av1(chain) => chain.wire(),
        }
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
    // ⚠ ORDER IS LOAD-BEARING — see the measured Intel Arc swap in
    // [`crate::caps_h265::query_h265_caps`]. `push_next` prepends, so pushing the codec
    // struct FIRST leaves VkVideoDecodeCapabilitiesKHR directly after the base struct.
    let mut caps = vk::VideoCapabilitiesKHR::default()
        .push_next(&mut h264_caps)
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
    let max_level_idc = h264_caps.max_level_idc;

    // The three queries carry the REAL creation usages (SAMPLED included for the
    // presenter-facing roles) so the answers validate the images the pools build.
    let profile = DecodeProfile::H264(std_profile_idc);
    // SAFETY: same liveness as above; the helper wires its own chain (this and
    // the two calls below).
    let dpb_formats = unsafe { query_formats(dev, profile, DPB_USAGE)? };
    // SAFETY: as above.
    let output_formats = unsafe { query_formats(dev, profile, OUTPUT_USAGE)? };
    // SAFETY: as above.
    let coincide_formats = unsafe { query_formats(dev, profile, COINCIDE_USAGE)? };

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
pub(crate) unsafe fn query_formats(
    dev: &DecodeDevice,
    decode_profile: DecodeProfile,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<VideoFormat>, vk::Result> {
    // SAFETY: the caller's DeviceHandles contract makes these two live, which is
    // exactly what the physical-device form needs.
    unsafe {
        query_formats_on(
            dev.video_queue_instance(),
            dev.physical_device(),
            decode_profile,
            usage,
        )
    }
}

/// [`query_formats`] against a bare physical device — no `VkDevice` in sight.
///
/// Split out so [`crate::probe`] enumerates through the SAME code the session's caps
/// query runs, rather than a second copy that would drift (the probe's whole value is
/// that its answer is the one derivation will see). `vkGetPhysicalDeviceVideoFormat-
/// PropertiesKHR` is an instance-level command over a physical device, so nothing here
/// ever needed the logical device the old signature demanded.
///
/// # Safety
///
/// `video_queue_instance` must be loaded against a live `VkInstance`, and
/// `physical_device` must be one of that instance's physical devices.
pub(crate) unsafe fn query_formats_on(
    video_queue_instance: &ash::khr::video_queue::Instance,
    physical_device: vk::PhysicalDevice,
    decode_profile: DecodeProfile,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<VideoFormat>, vk::Result> {
    let mut chain = decode_profile.chain();
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(usage)
        .push_next(&mut profile_list);

    let fp = video_queue_instance
        .fp()
        .get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    // SAFETY: live physical device; `info` roots a wired chain outliving the call;
    // null properties pointer is the spec's count-query form.
    let r = unsafe { fp(physical_device, &info, &mut count, std::ptr::null_mut()) };
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
    let r = unsafe { fp(physical_device, &info, &mut count, props.as_mut_ptr()) };
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
            image_type: p.image_type,
            image_tiling: p.image_tiling,
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
            ..Default::default()
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
            coincide_formats: vec![entry(NV12, COINCIDE_USAGE), entry(P010, COINCIDE_USAGE)],
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
                ..Default::default()
            }],
            output_formats: vec![entry(NV12, OUTPUT_USAGE)],
            coincide_formats: vec![],
            ..radv_like()
        }
    }

    /// [`OUTPUT_FORMATS`] is the vocabulary a CONSUMER pins its own per-format
    /// table against, so it has to be the whole of what this crate can deliver —
    /// no more (a format listed here but unmappable would fail a pool build) and
    /// no less (a format produced but unlisted is exactly the silent
    /// wrong-colour-math case the listing exists to stop).
    #[test]
    fn the_output_format_vocabulary_is_the_whole_of_what_this_crate_delivers() {
        for format in OUTPUT_FORMATS {
            assert!(
                plane_formats(format).is_some(),
                "{format:?} is advertised as an output but has no plane views"
            );
        }
        // Every (chroma, depth) pair the H.265 envelope admits resolves INTO the
        // vocabulary — the one producer that picks a format from stream facts.
        for chroma in 0u8..=4 {
            for depth in 0u8..=4 {
                if let Some(f) = crate::caps_h265::output_format_for(chroma, depth) {
                    assert!(
                        OUTPUT_FORMATS.contains(&f),
                        "output_format_for({chroma}, {depth}) = {f:?} is outside \
                         OUTPUT_FORMATS"
                    );
                }
            }
        }
        // H.264 is the 8-bit 4:2:0 envelope — its one format is in there too.
        assert!(OUTPUT_FORMATS.contains(&NV12));
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
        assert_eq!(
            caps.max_level_idc,
            MaxLevelIdc::H264(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2),
            "an H.264 query yields an H.264-tagged ceiling — the tag is what keeps \
             the decoders' numeric level gate comparing like with like"
        );
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
        raw.coincide_formats = vec![entry(P010, COINCIDE_USAGE)];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: NV12
            }
        );

        // Distinct mode reports which HALF is missing NV12.
        let mut raw = nvidia_like();
        raw.output_formats = vec![];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoFormat {
                mode: "output (DST|SAMPLED)",
                wanted: NV12
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
                format: NV12,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );
        // The missing bit is SAMPLED, so the message says what that COSTS — a field
        // report carrying this line should not need a second round trip to learn that
        // the device cannot host the rung at all.
        assert!(
            derive_caps(&raw)
                .unwrap_err()
                .to_string()
                .contains("zero-copy path cannot exist"),
            "a missing SAMPLED must name its consequence: {}",
            derive_caps(&raw).unwrap_err()
        );

        // Same on the distinct output half.
        let mut raw = nvidia_like();
        raw.output_formats = vec![entry(NV12, vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR)];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "output (DST|SAMPLED)",
                format: NV12,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        // A 10-bit stream is refused about P010, not about NV12 — the message used to
        // say "NV12" whatever the stream was, which sends a reader looking at the wrong
        // format's support.
        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(
            P010,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )];
        let err = crate::caps_h265::derive_caps_h265(
            &crate::caps_h265::RawH265Caps {
                capability_flags: raw.capability_flags,
                decode_flags: raw.decode_flags,
                coincide_formats: raw.coincide_formats.clone(),
                ..Default::default()
            },
            P010,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: P010,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );
        assert!(err.to_string().contains("G10X6"), "{err}");
    }

    #[test]
    fn a_presenter_facing_entry_without_mutable_format_is_refused() {
        let mut raw = radv_like();
        raw.coincide_formats = vec![VideoFormat {
            format: NV12,
            image_usage: COINCIDE_USAGE,
            image_create_flags: vk::ImageCreateFlags::empty(),
            ..Default::default()
        }];
        assert_eq!(
            derive_caps(&raw).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: NV12,
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
    fn plane_view_formats_follow_the_picture_format_bit_depth() {
        // The 8-bit families sample through R8/R8G8; the 10-bit 3PACK16 families
        // MUST use the R10X6 pair — an R8 view over a 10-bit plane reads half of
        // every sample and produces a plausible-looking wrong picture.
        assert_eq!(
            plane_formats(NV12),
            Some([vk::Format::R8_UNORM, vk::Format::R8G8_UNORM])
        );
        assert_eq!(plane_formats(YUV444_8), plane_formats(NV12));
        assert_eq!(
            plane_formats(P010),
            Some([
                vk::Format::R10X6_UNORM_PACK16,
                vk::Format::R10X6G10X6_UNORM_2PACK16
            ])
        );
        assert_eq!(plane_formats(YUV444_10), plane_formats(P010));
        // Anything else has no mapping — derivation refuses rather than guesses.
        assert_eq!(plane_formats(vk::Format::R8G8B8A8_UNORM), None);

        // And the derived caps carry the resolved pair, so pools never re-derive.
        let caps = derive_caps(&radv_like()).unwrap();
        assert_eq!(
            caps.plane_view_formats,
            [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]
        );
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
