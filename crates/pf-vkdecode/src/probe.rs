//! What the driver ACTUALLY answers about video image formats, verbatim — the
//! physical-device-only probe behind `punktfunk-session --probe-decode`.
//!
//! # Why this exists
//!
//! Twice in a row an Intel Arc refusal was diagnosed from punktfunk's OWN error text
//! and twice the conclusion ("Intel driver bug") was wrong — the bug was ours, in the
//! `pNext` order of the capability query. What broke it open both times was logging
//! the driver's answer with no interpretation in front of it. This module makes that
//! the DEFAULT rather than a debugging afterthought: for every decode profile the
//! client can negotiate, it asks the driver the same question the session's caps query
//! asks, in every usage combination the image pools would create with, and records
//! what came back — including the failures, spelled as the `VkResult` the driver
//! returned.
//!
//! It shares [`crate::caps::query_formats_on`] with the real caps path on purpose. A
//! probe with its own copy of the query is a probe that eventually disagrees with the
//! code it is meant to explain, which is worse than no probe at all.
//!
//! # What it measured
//!
//! Intel Arc (Windows driver 101.8861, 2026-08): every one of the 26 decode profiles
//! reports the SAME envelope for its NV12/P010 picture format —
//! `TRANSFER_SRC | VIDEO_DECODE_DST | VIDEO_DECODE_DPB`, and `imageCreateFlags` EMPTY —
//! with `DPB_AND_OUTPUT_COINCIDE` as the only decode mode. No `SAMPLED`, so a shader
//! cannot read the decoded picture; and no `MUTABLE_FORMAT`/`EXTENDED_USAGE`, which
//! closes the spec's only escape hatch (see [`crate::caps::VideoFormat::
//! image_create_flags`]). `TRANSFER_SRC` is the sole way out of the image — i.e. a
//! copy. NVIDIA (596.41) answers the same queries with
//! `TRANSFER_SRC|TRANSFER_DST|SAMPLED|DECODE_DST|DECODE_DPB|ENCODE_SRC|ENCODE_DPB` and
//! `MUTABLE_FORMAT|EXTENDED_USAGE`, which is what makes the zero-copy path work there.
//!
//! The Intel answers carry one genuine conformance bug, which is what made the refusal
//! read oddly: the driver ignores the REQUESTED `imageUsage` completely. Asked for
//! `SAMPLED` alone it still returns that same decode envelope, where the spec says the
//! returned `imageUsageFlags` "will contain at least the same set of image usage flags"
//! and `VK_ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR` is the documented refusal. That is why
//! derivation reported "the NV12 entry does not advertise SAMPLED" (an entry was found)
//! rather than "no NV12 in the coincide list" (nothing returned). It changes the error
//! text, not the answer.
//!
//! # What the cross-check is, and is NOT
//!
//! [`UsageProbe::image_format_support`] asks `vkGetPhysicalDeviceImageFormatProperties2`
//! the same question with the same profile list chained. It is deliberately kept, and
//! deliberately NOT treated as authority, because measuring it settled what it is worth:
//! on BOTH vendors it answers "creatable" for combinations the video-format query
//! rejects — NVIDIA included, for `SAMPLED` alone, which is not a legal video image
//! usage at all. So that entry point does not fully honour the video profile list on any
//! driver measured here, and a "creatable" from it is NOT evidence that a decode picture
//! can be sampled. It is reported because the question is otherwise re-asked by every
//! person who reads the refusal; the answer is on the record instead.
//!
//! `vkGetPhysicalDeviceVideoFormatPropertiesKHR` remains the authority, and two
//! independent implementations of it — this probe and `vulkaninfo --show-video-props` —
//! agree on every value above.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::query_formats_on;
use crate::caps::DecodeProfile;
use crate::caps::VideoFormat;
use crate::caps::COINCIDE_USAGE;
use crate::caps::DPB_USAGE;
use crate::caps::NV12;
use crate::caps::OUTPUT_USAGE;
use crate::caps::P010;
use crate::caps_av1::Av1ProfileKey;
use crate::caps_h265::H265ProfileKey;

/// The usage combinations the probe asks about, widest question first.
///
/// The first three are the REAL ones — exactly what [`crate::images`] creates with, so
/// their answers are the ones derivation acts on. The rest exist to localise a refusal:
/// when `DPB|DST|SAMPLED` fails, `DPB|DST` says whether the decode roles alone are fine
/// (i.e. the gap is sampling) and `SAMPLED` alone says whether the format is sampleable
/// under this profile at all. Without them, a single failed query leaves "which half is
/// missing?" to inference — which is how this device got misdiagnosed twice.
const USAGE_MATRIX: [(&str, vk::ImageUsageFlags); 6] = [
    ("coincide DPB|DST|SAMPLED", COINCIDE_USAGE),
    ("distinct DPB", DPB_USAGE),
    ("distinct DST|SAMPLED", OUTPUT_USAGE),
    ("DPB|DST (no SAMPLED)", DECODE_ONLY_USAGE),
    ("SAMPLED alone", vk::ImageUsageFlags::SAMPLED),
    ("DST alone", vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR),
];

/// Decode roles with sampling deliberately withheld — the discriminator between "this
/// device cannot decode this profile" and "it can decode it but not let anyone read it".
const DECODE_ONLY_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR.as_raw()
        | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw(),
);

/// One decode profile's worth of driver answers.
#[derive(Debug, Clone)]
pub struct ProfileProbe {
    /// The profile as a human would name it ("H.265 Main 4:2:0 8-bit").
    pub profile: &'static str,
    /// The picture format this profile decodes to — the entry the probe looks for.
    pub wanted: vk::Format,
    /// One row per [`USAGE_MATRIX`] entry, in that order.
    pub usages: Vec<UsageProbe>,
}

/// The driver's answer for ONE (profile, usage) question.
#[derive(Debug, Clone)]
pub struct UsageProbe {
    pub label: &'static str,
    pub usage: vk::ImageUsageFlags,
    /// `vkGetPhysicalDeviceVideoFormatPropertiesKHR`: every entry it returned, or the
    /// `VkResult` it failed with. An EMPTY vector is itself an answer — the two
    /// "no formats for this combination" results
    /// (`ERROR_FORMAT_NOT_SUPPORTED`/`ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR`) are mapped
    /// to it by the shared query, exactly as derivation sees them.
    pub formats: Result<Vec<VideoFormat>, vk::Result>,
    /// `vkGetPhysicalDeviceImageFormatProperties2` for the SAME profile list, format
    /// and usage. `Ok(())` means that call says an image of the shape is creatable —
    /// which, as the module docs record, is a WEAKER claim than it looks: measured on
    /// both vendors, this entry point does not fully honour the chained video profile
    /// list, so it must not be read as permission to create a video image.
    pub image_format_support: Result<(), vk::Result>,
}

impl UsageProbe {
    /// The entry for the profile's picture format, if the driver returned one.
    pub fn wanted_entry(&self, wanted: vk::Format) -> Option<VideoFormat> {
        self.formats
            .as_ref()
            .ok()?
            .iter()
            .copied()
            .find(|f| f.format == wanted)
    }
}

/// The profiles worth probing: one per codec rung the client can negotiate, plus the
/// 10-bit legs, because an HDR stream picks a DIFFERENT Vulkan profile from an SDR one
/// and a device may well host one and not the other.
///
/// Deliberately NOT every profile the driver supports — this answers "can punktfunk
/// decode here", and a list padded with profiles no rung ever requests makes the row
/// that matters harder to find. `vulkaninfo --show-video-props` is the tool for the
/// exhaustive sweep.
fn probed_profiles() -> Vec<(&'static str, DecodeProfile, vk::Format)> {
    // Every key is built through the SAME constructor the decoders negotiate with
    // (`from_negotiated`), so a profile the client could never request cannot appear
    // here — and a combination those constructors refuse simply drops out of the list
    // instead of being hand-rolled into existence for the probe's benefit.
    // 4:2:0 is chroma_format_idc 1 in both codecs' vocabulary; H.265 states depth as
    // `bit_depth_luma_minus8`, AV1 in whole bits.
    let mut out: Vec<(&'static str, DecodeProfile, vk::Format)> = vec![(
        "H.264 High 4:2:0 8-bit",
        DecodeProfile::H264(hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH),
        NV12,
    )];
    if let Ok(key) = H265ProfileKey::from_negotiated(1, 0) {
        out.push(("H.265 Main 4:2:0 8-bit", DecodeProfile::H265(key), NV12));
    }
    if let Ok(key) = H265ProfileKey::from_negotiated(1, 2) {
        out.push(("H.265 Main 10 4:2:0 10-bit", DecodeProfile::H265(key), P010));
    }
    // AV1 without film grain: `filmGrainSupport` is part of the Vulkan PROFILE, and
    // grain-less is what a punktfunk host encodes.
    if let Ok(key) = Av1ProfileKey::from_negotiated(1, 8, false) {
        out.push(("AV1 Main 4:2:0 8-bit", DecodeProfile::Av1(key), NV12));
    }
    if let Ok(key) = Av1ProfileKey::from_negotiated(1, 10, false) {
        out.push(("AV1 Main 4:2:0 10-bit", DecodeProfile::Av1(key), P010));
    }
    out
}

/// Ask one physical device every question in the matrix, for every probed profile.
///
/// Never fails as a whole: a driver that refuses a profile outright is a ROW in the
/// output, not an error return — the point is to come back with the full picture even
/// when most of it is refusals.
///
/// # Safety
///
/// `instance` must be a live `VkInstance` created through `entry`, and
/// `physical_device` one of its physical devices. Nothing here creates or destroys
/// anything; every call is a physical-device query.
pub unsafe fn probe_video_formats(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<ProfileProbe> {
    let video_queue_instance = ash::khr::video_queue::Instance::new(entry, instance);
    probed_profiles()
        .into_iter()
        .map(|(profile, decode_profile, wanted)| {
            let usages = USAGE_MATRIX
                .iter()
                .map(|(label, usage)| {
                    // SAFETY: caller contract — live instance the video_queue table was
                    // loaded from, and one of its physical devices.
                    let formats = unsafe {
                        query_formats_on(
                            &video_queue_instance,
                            physical_device,
                            decode_profile,
                            *usage,
                        )
                    };
                    // SAFETY: as above.
                    let image_format_support = unsafe {
                        image_format_supported(
                            instance,
                            physical_device,
                            decode_profile,
                            wanted,
                            *usage,
                        )
                    };
                    UsageProbe {
                        label,
                        usage: *usage,
                        formats,
                        image_format_support,
                    }
                })
                .collect();
            ProfileProbe {
                profile,
                wanted,
                usages,
            }
        })
        .collect()
}

/// The second opinion: can an image of (`format`, `usage`) exist for this video profile,
/// according to `vkGetPhysicalDeviceImageFormatProperties2`?
///
/// This is the same question `vkCreateImage` will be validated against
/// (VUID-VkImageCreateInfo-pNext-06811 routes through the video format properties, but
/// the general image-format query is what reports whether the combination is creatable
/// at all), asked through a DIFFERENT entry point. Where it disagrees with the video
/// format properties, one of the two driver paths is wrong — and knowing which is the
/// difference between a bug report to Intel and a fix in this repository.
///
/// # Safety
///
/// As [`probe_video_formats`].
unsafe fn image_format_supported(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    decode_profile: DecodeProfile,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Result<(), vk::Result> {
    let mut chain = decode_profile.chain();
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .push_next(&mut profile_list);
    let mut props = vk::ImageFormatProperties2::default();
    // SAFETY: caller contract (live instance + one of its physical devices); `info`
    // roots a wired chain of locals that outlive the call, and `props` is a local the
    // driver fills.
    unsafe {
        instance.get_physical_device_image_format_properties2(physical_device, &info, &mut props)
    }
}

/// `usage` as `NAME|NAME (0xHEX)`, with any bit this build cannot name kept VISIBLE.
///
/// The raw value is always printed beside the words for the same reason the codec-op
/// line prints its mask: a reader must be able to check the names against the number,
/// and a bit the tool has no word for must not silently vanish from a mask it reports.
pub fn describe_usage(usage: vk::ImageUsageFlags) -> String {
    // The encode trio is here because NVIDIA advertises it on DECODE pictures (measured:
    // 0xC000 beside the decode bits), and a mask printed as "unrecognised" invites the
    // reader to wonder whether the tool is out of date rather than reading the answer.
    const BITS: [(vk::ImageUsageFlags, &str); 12] = [
        (vk::ImageUsageFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (vk::ImageUsageFlags::TRANSFER_DST, "TRANSFER_DST"),
        (vk::ImageUsageFlags::SAMPLED, "SAMPLED"),
        (vk::ImageUsageFlags::STORAGE, "STORAGE"),
        (vk::ImageUsageFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::ImageUsageFlags::INPUT_ATTACHMENT, "INPUT_ATTACHMENT"),
        (vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR, "DECODE_DST"),
        (vk::ImageUsageFlags::VIDEO_DECODE_SRC_KHR, "DECODE_SRC"),
        (vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR, "DECODE_DPB"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_DST_KHR, "ENCODE_DST"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR, "ENCODE_SRC"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR, "ENCODE_DPB"),
    ];
    describe_mask(usage.as_raw(), &BITS.map(|(f, n)| (f.as_raw(), n)))
}

/// `imageCreateFlags` as `NAME|NAME (0xHEX)`, same accounting rule as
/// [`describe_usage`].
pub fn describe_create_flags(flags: vk::ImageCreateFlags) -> String {
    const BITS: [(vk::ImageCreateFlags, &str); 5] = [
        (vk::ImageCreateFlags::MUTABLE_FORMAT, "MUTABLE_FORMAT"),
        (vk::ImageCreateFlags::EXTENDED_USAGE, "EXTENDED_USAGE"),
        (vk::ImageCreateFlags::ALIAS, "ALIAS"),
        (vk::ImageCreateFlags::DISJOINT, "DISJOINT"),
        (vk::ImageCreateFlags::PROTECTED, "PROTECTED"),
    ];
    describe_mask(flags.as_raw(), &BITS.map(|(f, n)| (f.as_raw(), n)))
}

/// The shared naming rule: named bits joined by `|`, then the raw value, then any
/// leftover bits called out as unrecognised rather than dropped.
fn describe_mask(raw: u32, bits: &[(u32, &str)]) -> String {
    if raw == 0 {
        return "(none) (0x0)".to_string();
    }
    let mut names: Vec<&str> = bits
        .iter()
        .filter(|(bit, _)| raw & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    let named: u32 = bits.iter().map(|(bit, _)| bit).sum();
    let leftover = raw & !named;
    let extra;
    if leftover != 0 {
        extra = format!("unrecognised 0x{leftover:X}");
        names.push(&extra);
    }
    format!("{} (0x{raw:X})", names.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matrix must contain the three combinations the pools REALLY create with —
    /// a probe that answers about usages nobody creates explains nothing.
    #[test]
    fn the_matrix_covers_every_usage_the_pools_create_with() {
        for real in [COINCIDE_USAGE, DPB_USAGE, OUTPUT_USAGE] {
            assert!(
                USAGE_MATRIX.iter().any(|(_, u)| *u == real),
                "{real:?} is created by the image pools but never probed"
            );
        }
        // And the two discriminators that localise a refusal.
        assert!(USAGE_MATRIX.iter().any(|(_, u)| *u == DECODE_ONLY_USAGE));
        assert!(USAGE_MATRIX
            .iter()
            .any(|(_, u)| *u == vk::ImageUsageFlags::SAMPLED));
    }

    /// Every profile the client can negotiate gets a row, each with the picture format
    /// its caps derivation will look for — a probe that reported a 10-bit profile
    /// against NV12 would "find" nothing and read as a device gap.
    #[test]
    fn every_probed_profile_names_the_format_derivation_wants() {
        let profiles = probed_profiles();
        assert!(
            profiles.len() >= 5,
            "expected H.264 + H.265 8/10-bit + AV1 8/10-bit, got {}",
            profiles.len()
        );
        for (name, _, wanted) in &profiles {
            assert!(
                crate::caps::OUTPUT_FORMATS.contains(wanted),
                "{name} wants {wanted:?}, which is outside this crate's output vocabulary"
            );
        }
        assert!(profiles.iter().any(|(_, _, w)| *w == P010), "no 10-bit leg");
    }

    /// The mask printers must never drop a bit: names AND the raw value AND anything
    /// unnamed. This is the accounting rule the codec-op line already follows, and the
    /// reason it exists is that a silently-dropped bit reads as a capability the device
    /// does not have (or worse, hides one it does).
    #[test]
    fn mask_descriptions_account_for_every_bit_they_print() {
        assert_eq!(
            describe_usage(COINCIDE_USAGE),
            "SAMPLED|DECODE_DST|DECODE_DPB (0x1404)"
        );
        assert_eq!(describe_usage(vk::ImageUsageFlags::empty()), "(none) (0x0)");
        // The Intel Arc envelope, verbatim — the string a field report will contain.
        let intel = vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
            | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        assert_eq!(
            describe_usage(intel),
            "TRANSFER_SRC|DECODE_DST|DECODE_DPB (0x1401)"
        );
        // A bit with no name still shows up.
        let unknown = vk::ImageUsageFlags::from_raw(0x8000_0000);
        assert_eq!(
            describe_usage(unknown),
            "unrecognised 0x80000000 (0x80000000)"
        );
        assert_eq!(
            describe_create_flags(
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE
            ),
            "MUTABLE_FORMAT|EXTENDED_USAGE (0x108)"
        );
        assert_eq!(
            describe_create_flags(vk::ImageCreateFlags::empty()),
            "(none) (0x0)"
        );
    }

    /// `wanted_entry` picks by FORMAT, so a driver that returns several entries cannot
    /// hide the one derivation will act on behind a different format.
    #[test]
    fn the_wanted_entry_is_found_by_format_among_others() {
        let probe = UsageProbe {
            label: "x",
            usage: COINCIDE_USAGE,
            formats: Ok(vec![
                VideoFormat {
                    format: P010,
                    image_usage: COINCIDE_USAGE,
                    ..Default::default()
                },
                VideoFormat {
                    format: NV12,
                    image_usage: DPB_USAGE,
                    ..Default::default()
                },
            ]),
            image_format_support: Ok(()),
        };
        assert_eq!(probe.wanted_entry(NV12).unwrap().image_usage, DPB_USAGE);
        assert!(probe.wanted_entry(crate::caps::YUV444_8).is_none());
        // A failed query has no entry at all — distinct from "returned nothing".
        let failed = UsageProbe {
            formats: Err(vk::Result::ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR),
            ..probe
        };
        assert!(failed.wanted_entry(NV12).is_none());
    }
}
