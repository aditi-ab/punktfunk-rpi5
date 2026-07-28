//! Raw **Vulkan Video** HEVC + AV1 encoder (`VK_KHR_video_encode_h265` / `_av1`) with true
//! reference-frame invalidation — the open-stack AMD/Intel-Linux twin of the direct-NVENC RFI path.
//! The app owns the DPB, so loss recovery is a clean P-frame that re-references a known-good older
//! slot (no IDR): HEVC via an explicit short-term RPS, AV1 via `ref_frame_idx` + a
//! `primary_ref_frame = NONE` recovery anchor that also breaks the CDF chain.
//!
//! Capture delivers packed RGB (dmabuf/CPU); this backend imports it, runs an on-GPU RGB→4:2:0
//! compute CSC, then encodes — 8-bit BT.709 for an SDR session (`rgb2yuv.comp`), 10-bit BT.2020
//! for an HDR one (`rgb2yuv10.comp` + an HEVC Main10 session; a 10-bit AV1 session is routed to
//! VAAPI instead). Proven end-to-end in `punktfunk-planning/design/vkenc-probe-harness`.
//! Opt-in via `PUNKTFUNK_VULKAN_ENCODE`; gated to HEVC/AV1 + a device that advertises the encode op.
//! The AV1 encode structs our pinned `ash 0.38` predates are vendored in `vk_av1_encode.rs`.
#![allow(clippy::too_many_arguments)]

use super::vk_util::{
    color_range, find_mem, import_failure_feeds_latch, make_host_buffer, make_plain_image,
    make_view, normalize_cpu_rgb, pixel_to_vk,
};
use crate::{Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsRawFd;

const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// The 10-bit 4:2:0 picture/DPB format an HDR (HEVC Main10) session encodes from. `3PACK16`
/// stores each 10-bit sample in the HIGH bits of a 16-bit word — see `rgb2yuv10.comp`, whose
/// scratch planes are the size-compatible `R16`/`RG16` this gets `vkCmdCopyImage`d from.
const P010: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;

/// The session's 4:2:0 picture + DPB format for its bit depth. One function so the session
/// create-info, the DPB image, its views and the per-frame encode source cannot drift apart —
/// they must all name the SAME format or session creation fails.
const fn component_depth(ten_bit: bool) -> vk::VideoComponentBitDepthFlagsKHR {
    if ten_bit {
        vk::VideoComponentBitDepthFlagsKHR::TYPE_10
    } else {
        vk::VideoComponentBitDepthFlagsKHR::TYPE_8
    }
}

const fn h265_profile_idc(ten_bit: bool) -> vk::native::StdVideoH265ProfileIdc {
    if ten_bit {
        vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
    } else {
        vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
    }
}

const fn yuv_format(hdr: bool) -> vk::Format {
    if hdr {
        P010
    } else {
        NV12
    }
}
/// Max resident dmabuf imports (comfortably above any PipeWire pool depth; imports alias existing
/// buffers so this holds handles, not new allocations).
const IMPORT_CACHE_CAP: usize = 16;
// Prebuilt SPIR-V for the RGB→NV12 BT.709 compute CSC. Source is `rgb2yuv.comp` beside this file;
// regenerate with `glslangValidator -V rgb2yuv.comp -o rgb2yuv.spv` after editing the shader.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// The 10-bit HDR twin (`rgb2yuv10.comp`): packed 2:10:10:10 PQ/BT.2020 RGB → 10-bit 4:2:0,
/// BT.2020 NCL limited. Separate module rather than a spec constant because a shader's storage
/// image FORMAT (`r8`/`rg8` vs `r16`/`rg16`) is part of its layout, not specializable.
const CSC10_SPV: &[u8] = include_bytes!("rgb2yuv10.spv");
/// Fixed cursor-overlay texture size (px). Larger than any real pointer; the actual `w×h` uploads
/// into the top-left and the shader's push constant bounds sampling, so one allocation fits every
/// cursor and no per-size recreation is needed. See the CSC shader's `cursorTex`/push constant.
const CURSOR_MAX: u32 = 256;
/// DPB ring depth (well under the RADV `maxDpbSlots=17`); also the RFI recovery window.
const DPB_SLOTS: u32 = 8;
/// In-flight frame ring: how many captures may have GPU work outstanding at once. 2 overlaps a
/// frame's CSC+encode with the next capture (the throughput win) at the lowest possible added
/// latency — on-glass validated as rock-solid at 1080p@240, so it is the real-time default;
/// backpressure kicks in at the 2nd unread frame. Distinct from `DPB_SLOTS` (reference pool).
const RING_DEFAULT: usize = 2;

/// Ceiling on any blocking GPU fence wait on the encode thread (5 s). Generous against a real
/// encode (single-digit ms even on a loaded GPU) and against a driver hiccup, but finite: this is
/// the thread the stall watchdog's `reset()` runs on, so an unbounded wait would deadlock the very
/// path that recovers the session. Matches the Windows NVENC retrieve-thread budget.
const ENCODE_FENCE_TIMEOUT_NS: u64 = 5_000_000_000;
/// AV1 base quantizer index (0..=255) seeded into every frame. CBR rate control overrides it per
/// frame; it only matters as the starting point and for the (rate-control-ignored) constant-Q path.
const AV1_BASE_Q_IDX: u8 = 128;

/// Resolve the in-flight ring depth: `PUNKTFUNK_VULKAN_INFLIGHT` (clamped 2..=6), else `RING_DEFAULT`.
fn ring_depth() -> usize {
    std::env::var("PUNKTFUNK_VULKAN_INFLIGHT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(2, 6))
        .unwrap_or(RING_DEFAULT)
}

/// Requested Vulkan encode quality level (`PUNKTFUNK_VULKAN_QUALITY`, default 0), clamped to the
/// profile's `maxQualityLevels` at open. Levels are ordered fastest→best per the spec; on RADV
/// they select the VCN preset (0 SPEED, 1 BALANCE, 2 QUALITY, 3 HIGH_QUALITY — H265 on pre-RDNA4
/// pins 0 to BALANCE in the driver). Without an explicit `ENCODE_QUALITY_LEVEL` control RADV
/// never sends the VCN a preset op at all, leaving the firmware default — so the session always
/// installs the resolved level explicitly on its first frame.
fn quality_request() -> u32 {
    std::env::var("PUNKTFUNK_VULKAN_QUALITY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// `PUNKTFUNK_VULKAN_RGB_DIRECT` override for the RGB-direct encode source
/// (design/vulkan-rgb-direct-encode.md): the captured RGB frame is the encode source and the
/// VCN EFC front-end does the 709-narrow CSC inline — no compute CSC, no plane copies, one
/// queue submit per frame (unaligned modes go through the padded-copy staging blit). B2
/// default: ON wherever the probe passes, EXCEPT sessions that need the CSC's cursor blend
/// (see [`VulkanVideoEncoder::open`]). `=0` disables outright; `=1` forces it on non-cursor
/// sessions; unset = the default. A cursor-blend session IGNORES `=1` — EFC cannot composite
/// the pointer, and the caps-aware negotiation promised the client a composited one, so the
/// pointer outranks the lab pin (the open logs the override).
///
/// Parses like every sibling knob (`matches!(v.trim(), …)`): anything unrecognised — including
/// an empty value and a value that is only whitespace — falls back to the default rather than
/// being read as a force-on. The old `v == "0"` test made `"0 "` mean the *opposite* of what the
/// operator wrote, and turned a typo into a forced RGB-direct session with no cursor.
fn rgb_request() -> Option<bool> {
    parse_rgb_request(std::env::var("PUNKTFUNK_VULKAN_RGB_DIRECT").ok().as_deref())
}

/// The pure half of [`rgb_request`], so the accepted spellings are testable without mutating the
/// process environment (which no parallel test can do safely).
fn parse_rgb_request(raw: Option<&str>) -> Option<bool> {
    match raw?.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// True-extent RGB-direct at unaligned modes (default ON; `PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0`
/// restores the padded-copy staging): direct-import the visible-size capture with the TRUE-SIZE
/// source `codedExtent` — RADV derives nonzero VCN firmware padding from it, so the EFC is told
/// the source lacks the alignment rows (see [`RgbDirect::true_extent`]). Guarded-tested on Van
/// Gogh 2026-07-21 (kernel clean, and the fastest 1080p encode path measured); the EFC only
/// exists on Mesa ≥ 26, where the `codedExtent`-driven `session_init` is guaranteed.
fn rgb_true_extent_request() -> bool {
    std::env::var("PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT").as_deref() != Ok("0")
}

/// Live RGB-direct session config: the chroma-siting bits the session was created with
/// (chosen from what the driver advertises — see [`probe_rgb_direct`]).
struct RgbDirect {
    x_offset: u32, // vk_valve_rgb::CHROMA_OFFSET_*
    y_offset: u32,
    /// The mode is not 64x16-aligned, so the captured buffer cannot be the encode source
    /// under the session's ALIGNED source extent (the EFC read past it — the 2026-07-20 field
    /// GPU hang, when the source `codedExtent` was the aligned size and RADV therefore derived
    /// ZERO firmware padding). Each frame is copied into a per-slot ALIGNED BGRA staging image
    /// with the edge rows/columns duplicated into the padding (transfer-only, no shader) and
    /// encoded from there. Aligned modes keep the true zero-copy import.
    padded: bool,
    /// The default unaligned-mode source strategy (`PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0` falls
    /// back to `padded`): direct-import the visible-size buffer and pass the TRUE-SIZE source
    /// `codedExtent` — RADV then programs nonzero firmware padding from it (Mesa ≥ 24.2
    /// derives `session_init` padding from `srcPictureResource.codedExtent`; see
    /// [`VulkanVideoEncoder::native_nv12`]), telling the VCN the source lacks the alignment
    /// rows, which the hardware edge-extends internally. The session/SPS/DPB stay app-aligned.
    /// Guarded-tested on Van Gogh (kernel clean; fastest 1080p path measured).
    true_extent: bool,
}

/// Stack storage for a complete rgb-chained video profile. Profiled image creation AFTER open
/// (the per-PipeWire-buffer dmabuf imports, the CPU staging image) must present a profile
/// structurally identical to the session's — profile identity is by value, so rebuilding the
/// chain per call is spec-correct. `wire()` links the `p_next` chain into this struct's own
/// addresses (profile → codec profile → usage → rgb), so the value must not move between
/// `wire()` and the last use of `.profile`.
struct RgbProfileStack {
    rgb: super::vk_valve_rgb::VideoEncodeProfileRgbConversionInfoVALVE,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: super::vk_av1_encode::VideoEncodeAV1ProfileInfoKHR,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl RgbProfileStack {
    fn new(codec_op: vk::VideoCodecOperationFlagsKHR, ten_bit: bool) -> Self {
        use super::vk_av1_encode as av1b;
        use super::vk_valve_rgb as vrgb;
        Self {
            rgb: vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
                s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                perform_encode_rgb_conversion: vk::TRUE,
            },
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            h265: vk::VideoEncodeH265ProfileInfoKHR::default()
                .std_profile_idc(h265_profile_idc(ten_bit)),
            av1: av1b::VideoEncodeAV1ProfileInfoKHR {
                s_type: av1b::stype(av1b::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            },
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(codec_op)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(component_depth(ten_bit))
                .chroma_bit_depth(component_depth(ten_bit)),
        }
    }

    /// Wire the `p_next` chain into this value's final address; returns `&self.profile` for use.
    fn wire(&mut self, av1: bool) -> &vk::VideoProfileInfoKHR<'static> {
        self.usage.p_next = &self.rgb as *const _ as *const c_void;
        if av1 {
            self.av1.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.av1 as *const _ as *const c_void;
        } else {
            self.h265.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.h265 as *const _ as *const c_void;
        }
        &self.profile
    }
}

/// Non-RGB video profile rebuilt for native NV12 DMA-BUF imports. Image creation after `open`
/// must carry a profile identical by value to the session profile.
struct NativeProfileStack {
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: super::vk_av1_encode::VideoEncodeAV1ProfileInfoKHR,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl NativeProfileStack {
    fn new(codec_op: vk::VideoCodecOperationFlagsKHR, ten_bit: bool) -> Self {
        use super::vk_av1_encode as av1b;
        Self {
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            h265: vk::VideoEncodeH265ProfileInfoKHR::default()
                .std_profile_idc(h265_profile_idc(ten_bit)),
            av1: av1b::VideoEncodeAV1ProfileInfoKHR {
                s_type: av1b::stype(av1b::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                // AV1 Main covers 8 AND 10 bits — the depth rides `VideoProfileInfoKHR` alone.
                std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            },
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(codec_op)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(component_depth(ten_bit))
                .chroma_bit_depth(component_depth(ten_bit)),
        }
    }

    fn wire(&mut self, av1: bool) -> &vk::VideoProfileInfoKHR<'static> {
        if av1 {
            self.av1.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.av1 as *const _ as *const c_void;
        } else {
            self.h265.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.h265 as *const _ as *const c_void;
        }
        &self.profile
    }
}

/// The Vulkan codec-operation bit for our codec selection (shared by open and the per-import
/// profile rebuilds — the two must agree, profile identity is by value).
/// The physical device + encode queue family a session runs on: the FIRST device exposing a
/// `VIDEO_ENCODE` queue family that advertises `codec_op` (llvmpipe advertises none, so it drops
/// out implicitly). Shared by [`VulkanVideoEncoder::open_inner`] and [`probe_encode_caps`].
///
/// # Safety
/// `instance` must be a live `ash::Instance` and `devices` handles enumerated from it.
unsafe fn find_encode_device(
    instance: &ash::Instance,
    devices: &[vk::PhysicalDevice],
    codec_op: vk::VideoCodecOperationFlagsKHR,
) -> Option<(vk::PhysicalDevice, u32)> {
    for &pd in devices {
        let qf_len = instance.get_physical_device_queue_family_properties2_len(pd);
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); qf_len];
        let mut qf = vec![vk::QueueFamilyProperties2::default(); qf_len];
        for i in 0..qf_len {
            qf[i].p_next = &mut video[i] as *mut _ as *mut c_void;
        }
        instance.get_physical_device_queue_family_properties2(pd, &mut qf);
        for i in 0..qf_len {
            if qf[i]
                .queue_family_properties
                .queue_flags
                .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                && video[i].video_codec_operations.contains(codec_op)
            {
                return Some((pd, i as u32));
            }
        }
    }
    None
}

/// What this device's Vulkan Video **encode** stack can actually do for one codec — the caps
/// probe the negotiation stands on, so a session is never planned around a guess.
///
/// Two questions, and they are genuinely independent: an encode queue for the codec operation may
/// exist while the silicon declines the 10-bit profile (older VCN, an Intel generation without
/// Main10 encode). Answering only the first is what used to push every HDR session to libav VAAPI
/// — losing real RFI recovery and the compute CSC's cursor blend for nothing on hardware that
/// could do it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VulkanEncodeCaps {
    /// An encode queue advertises this codec's operation.
    pub supported: bool,
    /// The device accepts an 8-bit 4:2:0 profile for it (the ordinary SDR session).
    pub eight_bit: bool,
    /// …and a 10-bit one: HEVC Main10 / AV1 Main at 10 bits, the HDR session's profile.
    pub ten_bit: bool,
}

/// Probe [`VulkanEncodeCaps`] for `codec`. Uncached — [`crate::vulkan_encode_caps`] owns the
/// per-(GPU, codec) cache, exactly as it did for the old boolean.
///
/// The depth answers come from `vkGetPhysicalDeviceVideoCapabilitiesKHR` against a profile built
/// at that depth, which is the same query the session open makes: a `true` here means the open
/// will get past the profile gate, and a `false` means it would have failed. No second source of
/// truth to drift.
pub(crate) fn probe_encode_caps(codec: Codec) -> VulkanEncodeCaps {
    if !matches!(codec, Codec::H265 | Codec::Av1) {
        return VulkanEncodeCaps::default();
    }
    let av1 = codec == Codec::Av1;
    let codec_op = codec_op_for(av1);
    // SAFETY: creates one Vulkan instance and issues only physical-device queries against it, then
    // destroys it on EVERY path below before returning — no handle derived from it escapes, and
    // nothing outside this call observes it. `Entry::load` only dlopens the loader (a missing
    // libvulkan returns `Err`), touching no process state the rest of the crate relies on.
    // `find_encode_device` gets a live instance and handles enumerated from it, as it requires;
    // `depth_supported` gets that same live instance plus the physical device it returned.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return VulkanEncodeCaps::default();
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return VulkanEncodeCaps::default();
        };
        let found = match instance.enumerate_physical_devices() {
            Ok(devices) => find_encode_device(&instance, &devices, codec_op).map(|(pd, _)| pd),
            Err(_) => None,
        };
        let caps = match found {
            Some(pd) => {
                let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);
                VulkanEncodeCaps {
                    supported: true,
                    eight_bit: depth_supported(&vq_inst, pd, codec_op, av1, false),
                    ten_bit: depth_supported(&vq_inst, pd, codec_op, av1, true),
                }
            }
            None => VulkanEncodeCaps::default(),
        };
        instance.destroy_instance(None);
        caps
    }
}

/// Does `pd` accept an encode profile for `codec_op` at this bit depth? The profile chain is
/// byte-identical to the one [`VulkanVideoEncoder::open_inner`] builds — that is the point.
///
/// # Safety
/// `vq_inst` must wrap the live instance `pd` was enumerated from.
unsafe fn depth_supported(
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    codec_op: vk::VideoCodecOperationFlagsKHR,
    av1: bool,
    ten_bit: bool,
) -> bool {
    let mut ps = NativeProfileStack::new(codec_op, ten_bit);
    let profile = *ps.wire(av1);
    let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut av1_caps: super::vk_av1_encode::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
    av1_caps.s_type = super::vk_av1_encode::stype(super::vk_av1_encode::ST_CAPABILITIES);
    let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    if av1 {
        av1_caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
        caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
    } else {
        h265_caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
        caps.p_next = &mut h265_caps as *mut _ as *mut c_void;
    }
    (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps)
        == vk::Result::SUCCESS
}

fn codec_op_for(av1: bool) -> vk::VideoCodecOperationFlagsKHR {
    if av1 {
        vk::VideoCodecOperationFlagsKHR::from_raw(
            super::vk_av1_encode::VIDEO_CODEC_OPERATION_ENCODE_AV1,
        )
    } else {
        vk::VideoCodecOperationFlagsKHR::ENCODE_H265
    }
}

/// How `begin_encode_cmd` must acquire this frame's encode-source image.
#[derive(Clone, Copy, PartialEq)]
enum SrcAcquire {
    /// CSC path: `nv12_src` was written by this frame's compute batch (GENERAL layout; the
    /// csc_sem orders the queues).
    CscGeneral,
    /// First use of a DMA-BUF imported directly as the video source: acquire from the foreign
    /// producer (UNDEFINED preserves modifier-backed bytes) with a FOREIGN→encode-family transfer.
    DmabufFresh,
    /// Cached direct-source import: already VIDEO_ENCODE_SRC; visibility-only barrier for the
    /// producer's out-of-band rewrite of the bytes.
    DmabufCached,
    /// RGB-direct CPU upload: the compute queue copied the staging buffer in (semaphore
    /// ordered); transition TRANSFER_DST → VIDEO_ENCODE_SRC.
    CpuUpload,
}

/// Persistently mapped base of a frame slot's `bs_mem` (HOST_VISIBLE|HOST_COHERENT, mapped once
/// at build): `read_slot` copies an AU out of it every frame, so the previous per-frame
/// vkMapMemory/vkUnmapMemory round-trip was pure driver-call overhead. vkFreeMemory implicitly
/// unmaps, so teardown needs no explicit unmap.
struct BsPtr(*const u8);
// SAFETY: a Vulkan host mapping is a property of the allocation, not of the thread that mapped
// it (the spec places no thread affinity on mapped pointers); every dereference goes through the
// encoder's `&mut self`, so no concurrent access exists.
unsafe impl Send for BsPtr {}
impl Default for BsPtr {
    fn default() -> Self {
        Self(std::ptr::null())
    }
}

/// The trusted-reference view of `slot_wire` for the shared RFI policy ([`crate::rfi`]): resident
/// slots only — `-1` marks empty *or distrusted* (blanked by an earlier loss's taint sweep), and
/// excluding it here is exact because the loss start is validity-gated non-negative.
fn trusted_refs(slot_wire: &[i64]) -> Vec<(usize, i64)> {
    slot_wire
        .iter()
        .enumerate()
        .filter_map(|(s, &w)| (w >= 0).then_some((s, w)))
        .collect()
}

/// The S0 (past-reference) half of an HEVC short-term RPS that **retains every resident DPB
/// picture**, not just the one this frame predicts from. The RPS is the decoder's only retention
/// signal (HEVC 8.3.2: any DPB picture absent from the current RPS is marked "unused for
/// reference" and reclaimed) — an RPS naming only the active reference lets a conforming decoder
/// evict the rest, and the RFI recovery anchor then references a picture the client already
/// discarded: FFmpeg's HEVC parser (the Linux VAAPI/Vulkan and Windows D3D11VA clients) conceals
/// with a generated gray reference and every following frame chains off the corruption — exactly
/// at the moment the anchor claims the picture is clean. Listing all residents (with
/// `used_by_curr_pic` set only for the real reference) keeps the host and client DPBs in lockstep,
/// so any slot the RFI anchor pick ([`crate::rfi::pick_anchor`]) can pick is decodable by
/// construction.
///
/// `setup_idx` — the slot this frame reconstructs into — is excluded: its old occupant dies with
/// this frame on the host, so the decoder must drop it too (also keeping the retained count at
/// `DPB_SLOTS - 1` + the current picture = the SPS `max_dec_pic_buffering` budget).
///
/// Returns `(num_negative_pics, delta_poc_s0_minus1, used_by_curr_pic_s0_flag)`.
fn build_h265_rps_s0(
    slot_poc: &[i32],
    setup_idx: usize,
    ref_poc: i32,
    cur_poc: i32,
) -> (u8, [u16; 16], u16) {
    // Residents, newest first — S0 is ordered by descending POC (ascending delta from `cur_poc`).
    let mut pocs: Vec<i32> = slot_poc
        .iter()
        .enumerate()
        .filter(|&(s, &p)| s != setup_idx && p >= 0 && p < cur_poc)
        .map(|(_, &p)| p)
        .collect();
    pocs.sort_unstable_by(|a, b| b.cmp(a));
    pocs.truncate(16); // delta_poc_s0_minus1 capacity (STD_VIDEO_H265_MAX_DPB_SIZE)
    let mut deltas = [0u16; 16];
    let mut used = 0u16;
    let mut prev = cur_poc;
    for (i, &p) in pocs.iter().enumerate() {
        // delta_poc_s0_minus1[i] codes the gap to the PREVIOUS S0 entry (the spec's cumulative
        // DeltaPocS0 chain), not to the current picture.
        deltas[i] = (prev - p - 1) as u16;
        if p == ref_poc {
            used |= 1 << i;
        }
        prev = p;
    }
    (pocs.len() as u8, deltas, used)
}

/// One in-flight frame's private GPU resources. The encoder keeps a small ring of these so a
/// frame's GPU work (CSC + encode) overlaps the CPU capturing and submitting the next one:
/// `submit()` records into a free slot and returns without blocking; `poll()` reads back the
/// oldest slot once its `fence` signals. Everything here is written by one frame and read by the
/// next-but-K, so it cannot be shared while a submission is outstanding.
///
/// [`Frame::default`] is the all-null placeholder `open_inner` pre-pushes into its unwind guard so
/// `make_frame` can build in place; destroying one is a no-op (`vkDestroy*` ignores null handles).
#[derive(Default)]
struct Frame {
    compute_cmd: vk::CommandBuffer, // CSC (compute+transfer)
    cmd: vk::CommandBuffer,         // encode queue
    csc_sem: vk::Semaphore,         // compute -> encode ordering (this frame only)
    fence: vk::Fence,               // signaled when this frame's encode completes
    query_pool: vk::QueryPool,      // bitstream offset/bytes feedback
    // GPU timestamps around the CSC batch (`PUNKTFUNK_PERF` only; null otherwise): [0]=batch
    // start, [1]=batch end — splits the fence wait into "compute CSC+copies" vs "ASIC encode".
    ts_pool: vk::QueryPool,
    // Whether the submission currently in this slot actually reset AND wrote `ts_pool`. A pool
    // existing is NOT proof that it was written: `with_ts` arms the pool for the CSC and
    // padded-RGB paths, but the padded-RGB path's CPU-upload arm records its own command buffer
    // and writes no timestamps. Reading an unreset/unwritten query with `WAIT` is undefined —
    // RADV happens to fail the read rather than block, another driver may hang — so `read_slot`
    // consults this instead of `ts_pool != null`.
    ts_written: bool,
    bs_buf: vk::Buffer,
    bs_mem: vk::DeviceMemory,
    bs_ptr: BsPtr, // persistent mapping of bs_mem (see BsPtr)
    // Padded-copy RGB staging (RGB-direct on an unaligned mode ONLY, see RgbDirect::padded):
    // an ALIGNED BGRA encode-src the visible frame is blitted into with edges duplicated.
    pad_img: vk::Image,
    pad_mem: vk::DeviceMemory,
    pad_view: vk::ImageView,
    csc_set: vk::DescriptorSet, // Y/UV bindings fixed; binding 0 (RGB) rewritten each use
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    /// The CSC's output picture and this frame's encode source: NV12, or the 10-bit
    /// `P010`/3PACK16 twin on an HDR session ([`yuv_format`]). The name predates the second
    /// depth; every use goes through the format the session was created with.
    nv12_src: vk::Image,
    nv12_mem: vk::DeviceMemory,
    nv12_view: vk::ImageView,
    // CPU-input staging (lazily sized; only the software-capture / smoke-test path uses it).
    // Keyed on (format, width, height) — NOT format alone. The CSC arm sizes this image to the
    // SOURCE frame while `cmd_copy_buffer_to_image` uses the CURRENT frame's extent, so a
    // same-format size change against a format-only key copies past the allocation (confirmed on
    // RADV as VUID-vkCmdCopyBufferToImage-imageSubresource-07971, with `submit` still returning Ok).
    cpu_img: Option<(
        vk::Image,
        vk::DeviceMemory,
        vk::ImageView,
        vk::Format,
        u32,
        u32,
    )>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
    // Per-slot cursor overlay (cursor-as-metadata): a fixed CURSOR_MAX² RGBA8 sampled image (bound
    // once at binding 3) + host staging. Re-uploaded only when the bitmap changes (`cursor_serial`);
    // `cursor_ready` records the one-time UNDEFINED→SHADER_READ_ONLY transition so binding 3 is a
    // valid layout even with no cursor. Per-slot (not shared) so a shape change never races a prior
    // frame's in-flight CSC read.
    cursor_img: vk::Image,
    cursor_mem: vk::DeviceMemory,
    cursor_view: vk::ImageView,
    cursor_stage: vk::Buffer,
    cursor_stage_mem: vk::DeviceMemory,
    cursor_serial: u64,
    cursor_ready: bool,
    // Frame metadata, set at submit and read back at poll (valid only while this slot is in flight).
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
}

pub struct VulkanVideoEncoder {
    // --- vulkan core (owned) ---
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    vq_dev: ash::khr::video_queue::Device,
    venc_dev: ash::khr::video_encode_queue::Device,
    encode_queue: vk::Queue,
    compute_queue: vk::Queue,
    encode_family: u32,
    compute_family: u32,
    /// The queue family the dmabuf-acquire barriers name as `src`: `QUEUE_FAMILY_FOREIGN_EXT`
    /// when `VK_EXT_queue_family_foreign` is advertised+enabled (Phase 8 — the barriers used it
    /// without the enable, tolerated by RADV), else the core-1.1 `QUEUE_FAMILY_EXTERNAL`
    /// conservative substitute (adds a same-driver precondition FOREIGN doesn't have — the
    /// pragmatic fallback for devices that were never valid targets before).
    foreign_qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,

    // --- codec ---
    codec: Codec, // H265 or Av1 — selects the Std-struct authoring + header framing

    // --- video session ---
    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    // Keyframe prefix: HEVC = VPS/SPS/PPS; AV1 = temporal-delimiter OBU + sequence-header OBU.
    header: Vec<u8>,
    // Per-(non-key)-frame prefix: empty for HEVC (headers ride keyframes only); AV1 = a
    // temporal-delimiter OBU that opens every temporal unit (Vulkan emits only the frame OBU).
    frame_prefix: Vec<u8>,

    // --- DPB ---
    dpb_image: vk::Image,
    dpb_mem: vk::DeviceMemory,
    dpb_views: Vec<vk::ImageView>,
    slot_wire: Vec<i64>, // wire index held per slot (-1 = empty) — RFI/loss domain
    slot_poc: Vec<i32>,  // HEVC POC held per slot — reference-delta domain
    prev_slot: usize,

    // --- CSC (RGB -> NV12), shared across the frame ring ---
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    // Per-buffer dmabuf-import cache, keyed by (st_dev, st_ino) — PipeWire cycles a small fixed pool,
    // so each underlying buffer is imported ONCE and reused (no per-frame VkImage create/import/destroy).
    // Imports are read-only per frame, so the ring shares them (concurrent frames read distinct buffers).
    import_cache: Vec<CachedImport>,

    // --- in-flight frame ring (pipelining) ---
    frames: Vec<Frame>,         // per-slot private resources
    ring: usize,                // next slot to record into (round-robin over `frames`)
    in_flight: VecDeque<usize>, // slots submitted but not yet read back, oldest first
    bs_size: u64,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,

    // --- rate control, rebuilt-safe ---
    bitrate: u64,
    fps: u32,
    /// Rate-control mode for every RC site, resolved ONCE at open from
    /// `VkVideoEncodeCapabilitiesKHR::rateControlModes` (previously hardcoded CBR with no
    /// capability check at all): VBR when the driver advertises it, else CBR. Always paired with
    /// `average_bitrate == max_bitrate`, so VBR here is not "spend less on average" — it is CBR's
    /// exact ceiling minus the *stuffing*: CBR must keep the CPB from overflowing on underspent
    /// frames and Vulkan exposes no filler-suppression control (AMF's `filler_data=false` /
    /// NVENC's default-off have no VK equivalent), measured on the 780M as 97 % filler NALs under
    /// a tight CBR window. VBR permits the underspend, so a tight window only ever BOUNDS a
    /// complex frame (WP6.3).
    rc_mode: vk::VideoEncodeRateControlModeFlagsKHR,
    /// HRD window `(virtualBufferSizeInMs, initialVirtualBufferSizeInMs)`: the house ~1-frame
    /// window ([`crate::vbv_window_ms`], `PUNKTFUNK_VBV_FRAMES` scales it) under VBR, the legacy
    /// loose `(1000, 500)` under the CBR fallback — where tightening it is the measured 36×
    /// filler regression, so the status quo is kept deliberately. Latched into a field rather
    /// than recomputed at each of the four `VideoEncodeRateControlInfoKHR` sites on purpose: two
    /// of them DECLARE the session's current rate-control state to `vkCmdBeginVideoCodingKHR` and
    /// two INSTALL it, and the spec requires the declaration to match what is installed at
    /// execution time (`VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08254`). One latched value
    /// cannot drift; four independent computations can.
    vbv_ms: (u32, u32),
    /// `VkVideoEncodeCapabilitiesKHR::maxBitrate`, the other field this struct's query used to
    /// drop on the floor. Bounds `bitrate` at open and every retarget (the spec bounds every
    /// layer's bitrates by it; an ABR climb on a low-cap driver would otherwise sail past it).
    /// A driver reporting 0 means "not filled in" — treated as unbounded.
    hw_max_bitrate: u64,
    /// Resolved encode quality level (see [`quality_request`]) — installed via the first frame's
    /// `ENCODE_QUALITY_LEVEL` control and baked into the session-parameters object (the spec
    /// requires the two to match).
    quality_level: u32,
    /// `PUNKTFUNK_PERF` pre-encode split: >0 ⇒ per-frame GPU timestamps bracket either the
    /// RGB→NV12 compute batch or the native-NV12 padded copy. The measured duration is logged
    /// separately from the host's fence wait; 0.0 means disabled or unsupported.
    ts_period_ns: f64,
    perf_at: std::time::Instant,
    /// Reused 3→4 expansion buffer for 24-bpp CPU payloads (`vk_util::normalize_cpu_rgb`).
    cpu_expand: Vec<u8>,
    /// RGB-direct (EFC) session config — `Some` ⇒ the session's picture format is BGRA, frames
    /// are handed to the encoder as RGB (dmabuf import or CPU upload) and the VCN front-end does
    /// the CSC; `None` ⇒ the compute-CSC path. Fixed per session (the picture format is baked
    /// into the video session).
    rgb: Option<RgbDirect>,
    /// Producer supplied native NV12 rather than packed RGB. EVERY mode encodes the imported
    /// visible-size buffer directly — safely, because native sessions use TRUE-SIZE headers:
    /// the SPS/sequence header is authored at the render size and every picture resource's
    /// `codedExtent` matches it, so RADV programs the VCN with `session_init` extent = the true
    /// size and a nonzero `padding_width/height`, and the FIRMWARE edge-extends the alignment
    /// padding internally (radv_video_enc.c `radv_enc_session_init`; the driver also rounds the
    /// bitstream SPS up itself and compensates with a conformance window —
    /// `radv_video_patch_encode_session_parameters`, per the VK_KHR_video_encode_h265 proposal's
    /// "implementations may override" clause). The source is never read past its extent — unlike
    /// the app-aligned-SPS convention the CSC/RGB paths use, where the coded extent is 64x16-
    /// aligned and an undersized direct source is the OOB-read class behind the 2026-07-20 field
    /// GPU reset (those paths keep their aligned-size sources/staging).
    native_nv12: bool,
    /// This is a 10-bit (HDR) session: Main10 / AV1-10 profile, `P010` picture + DPB, and either
    /// the BT.2020 compute CSC or the EFC's BT.2020 conversion. Every profile chain rebuilt after
    /// `open` (the per-buffer dmabuf imports) must present the SAME depth, so it is carried here
    /// rather than re-derived.
    ten_bit: bool,

    /// A [`reconfigure_bitrate`](Encoder::reconfigure_bitrate) rate not yet installed in the video
    /// session. The next `record_submit` emits an `ENCODE_RATE_CONTROL` control command carrying it
    /// (mid-stream) or folds it into the first frame's RESET+RC install, then promotes it into
    /// `bitrate` — which must keep naming the session's CURRENT state, because every begin-coding
    /// declares it (the spec requires the declared state to match).
    pending_bitrate: Option<u64>,

    // --- state ---
    width: u32,
    height: u32,
    render_w: u32, // real (pre-alignment) dimensions — AV1 render_size / HEVC conformance window
    render_h: u32,
    poc: i32,       // monotonic HEVC picture-order-count (reused as AV1 order_hint counter)
    enc_count: u64, // total frames encoded — drives the DPB ring cursor
    auto_wire: i64, // fallback wire index when submit() (not submit_indexed) is used
    first_frame: bool, // needs RESET + DPB layout transition + rate-control install + IDR
    /// Whether the SESSION OBJECT currently has a non-default rate-control mode installed.
    ///
    /// Deliberately NOT `!first_frame`. `first_frame` means "this frame must issue RESET + install
    /// CBR + quality", which is true both at open and after a mid-stream [`Encoder::reset`] — but
    /// `vkCmdBeginVideoCodingKHR` must declare the rate-control state the session ACTUALLY has at
    /// that moment, and a `reset()` does not touch the session object: the CBR installed before it
    /// is still current when the next frame begins its coding scope. Keying the declaration on
    /// `first_frame` therefore omitted it after every reset, which RADV's validation reports as
    /// VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08253 ("no VkVideoEncodeRateControlInfoKHR ... but
    /// the currently set mode is CBR"). Set once the install actually happens, and only cleared by
    /// building a new session.
    rc_installed: bool,
    force_kf: bool, // request_keyframe / non-recoverable loss -> next frame is IDR
    pending_loss: Option<i64>, // invalidate_ref_frames(first) -> recover on next frame
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: the encoder is used only from the single encode thread; all Vulkan handles are owned and
// never shared. Matches `NvencCudaEncoder`'s `unsafe impl Send`.
unsafe impl Send for VulkanVideoEncoder {}

impl VulkanVideoEncoder {
    /// Signature mirrors the other Linux backends' `open` plus `cursor_blend`: the session may
    /// hand this encoder cursor bitmaps to composite (cursor-as-metadata captures — every
    /// non-gamescope compositor). The EFC cannot blend, so such sessions default to the CSC
    /// path; everywhere else the RGB-direct source is the DEFAULT wherever the probe passes
    /// (B2). `PUNKTFUNK_VULKAN_RGB_DIRECT` overrides both ways (see [`rgb_request`]).
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        cursor_blend: bool,
    ) -> Result<Self> {
        let native_nv12 = format == PixelFormat::Nv12;
        // HDR: a packed 10-bit PQ/BT.2020 capture (a gamescope output off our `pipewire-hdr`
        // build, or the GNOME 50+ portal monitor mirror). BOTH codecs and BOTH encode sources
        // serve it — which of them this device can actually do is `probe_encode_caps`' answer,
        // consulted by the dispatcher before we get here and re-checked by the profile query
        // inside the open.
        let ten_bit = format.is_hdr_rgb10();
        // The RGB-direct (EFC) source needs the captured format as the session's picture format.
        // `pixel_to_vk` covers every format the capture can hand a GPU session; the BGRA default
        // only preserves the old behaviour for the CPU-only layouts, which never reach that arm.
        let src_rgb_fmt = pixel_to_vk(format).unwrap_or(vk::Format::B8G8R8A8_UNORM);
        // A cursor-blend session must keep the compute-CSC path — the only arm with the cursor
        // blend — so it outranks an explicit RGB-direct pin (EFC cannot composite; the
        // negotiation promised the client a composited pointer).
        if cursor_blend && rgb_request() == Some(true) {
            tracing::info!(
                "PUNKTFUNK_VULKAN_RGB_DIRECT=1 ignored for this session — it composites the \
                 pointer, which the EFC front-end cannot; using the compute-CSC path"
            );
        }
        let want_rgb = !native_nv12 && !cursor_blend && rgb_request().unwrap_or(true);
        Self::open_opts_inner(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            want_rgb,
            native_nv12,
            ten_bit,
            src_rgb_fmt,
        )
    }

    /// `open` with the RGB-direct request explicit instead of read from the env — the smoke
    /// tests use this (env mutation races parallel tests). `want_rgb` engages the RGB-direct
    /// source only if [`probe_rgb_direct`] also passes; otherwise the session opens on the CSC
    /// path with the verdict logged. Test-only: the production entry point is [`Self::open`],
    /// which resolves `want_rgb` from the env + the `cursor_blend` hint.
    #[cfg(test)]
    pub(crate) fn open_opts(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
    ) -> Result<Self> {
        Self::open_opts_depth(codec, width, height, fps, bitrate_bps, want_rgb, false)
    }

    /// [`open_opts`](Self::open_opts) with the bit depth explicit — the 10-bit smokes' entry
    /// point. A 10-bit session takes the packed 2:10:10:10 source format the HDR capture
    /// negotiates (`xRGB_210LE` → `A2R10G10B10_UNORM_PACK32`; that is the one gamescope's node
    /// actually fixates, verified on RADV 2026-07-28), so the smoke drives the same formats a
    /// real session does.
    #[cfg(test)]
    pub(crate) fn open_opts_depth(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
        ten_bit: bool,
    ) -> Result<Self> {
        Self::open_opts_inner(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            want_rgb,
            false,
            ten_bit,
            if ten_bit {
                vk::Format::A2R10G10B10_UNORM_PACK32
            } else {
                vk::Format::B8G8R8A8_UNORM
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_opts_inner(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
        native_nv12: bool,
        ten_bit: bool,
        src_rgb_fmt: vk::Format,
    ) -> Result<Self> {
        if !matches!(codec, Codec::H265 | Codec::Av1) {
            bail!("vulkan-encode backend supports HEVC + AV1 only (got {codec:?})");
        }
        // align coded extent to the encode granularity (64x16 on RADV). HEVC crops the padding back
        // to (width,height) via a conformance window; AV1 signals it via render_size (see build).
        let w = (width + 63) & !63;
        let h = (height + 15) & !15;
        // SAFETY: `open_inner` only issues Vulkan calls whose preconditions it establishes itself
        // (valid instance/device, correctly-chained create-infos); all handles are freshly created
        // here and owned by the returned `Self`. No aliasing or outside invariants are involved.
        unsafe {
            Self::open_inner(
                codec,
                w,
                h,
                width,
                height,
                fps.max(1),
                bitrate_bps.max(1_000_000),
                want_rgb,
                native_nv12,
                ten_bit,
                src_rgb_fmt,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open_inner(
        codec: Codec,
        w: u32,
        h: u32,
        rw: u32,
        rh: u32,
        fps: u32,
        bitrate: u64,
        want_rgb: bool,
        native_nv12: bool,
        // NOT `hdr`: this fn already binds that name to the parameter-set HEADER bytes below.
        ten_bit: bool,
        src_rgb_fmt: vk::Format,
    ) -> Result<Self> {
        use super::vk_av1_encode as av1b;
        use super::vk_valve_rgb as vrgb;
        let av1 = codec == Codec::Av1;
        let codec_op = codec_op_for(av1);
        let entry = ash::Entry::load().context("load vulkan loader")?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .context("create instance")?;
        // From here on, every created object is mirrored into `guard` the moment it exists, so any
        // early `?`/`bail!` unwinds exactly what was built (see [`VkTeardown`]). The locals keep
        // aliasing the handles for the rest of the build; only the `Ok(Self)` hand-off at the
        // bottom disarms the guard.
        let mut guard = VkTeardown::new(instance.clone());

        let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);

        // pick the physical device + encode queue family (skip llvmpipe). The scan itself lives in
        // [`find_encode_device`] so the negotiation-time probe asks the SAME question this open
        // asks — a probe that MIRRORS a dispatch goes stale the first time the dispatch grows a
        // case, which is the failure `open_video`'s backend-label note already records.
        let (pd, encode_family) =
            find_encode_device(&instance, &instance.enumerate_physical_devices()?, codec_op)
                .context("no VK_KHR_video_encode queue for the requested codec on any device")?;
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // a compute queue family for the CSC (usually family 0) + its timestamp support (the
        // PUNKTFUNK_PERF CSC/encode split; valid_bits==0 ⇒ no timestamps on this family)
        let (compute_family, compute_ts_bits) = {
            let qf = instance.get_physical_device_queue_family_properties(pd);
            let fam = qf
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .context("no compute queue")?;
            (fam as u32, qf[fam].timestamp_valid_bits)
        };
        // PUNKTFUNK_PERF (same parse as punktfunk-core: "0" = off) arms the sampled CSC-vs-ASIC
        // split; ts_period_ns==0.0 keeps every timestamp site compiled out of the hot path.
        let ts_period_ns =
            if std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0") && compute_ts_bits > 0 {
                instance
                    .get_physical_device_properties(pd)
                    .limits
                    .timestamp_period as f64
            } else {
                0.0
            };

        // Resolve the encode source before building the profile: EFC RGB conversion changes
        // profile identity; producer-native NV12 uses the ordinary 4:2:0 profile.
        let aligned = rw == w && rh == h;
        let rgb_probe = if native_nv12 {
            Err("not-probed(native NV12 source selected)")
        } else {
            probe_rgb_direct(&instance, &vq_inst, pd, codec_op, av1, ten_bit, src_rgb_fmt)
        };
        let rgb_cfg: Option<RgbDirect> = match (&rgb_probe, want_rgb) {
            (Ok((x, y)), true) => {
                let true_extent = !aligned && rgb_true_extent_request();
                Some(RgbDirect {
                    x_offset: *x,
                    y_offset: *y,
                    padded: !aligned && !true_extent,
                    true_extent,
                })
            }
            _ => None,
        };
        if native_nv12 {
            tracing::info!(
                native_nv12 = "active(direct-import)",
                source_width = rw,
                source_height = rh,
                fw_padding_width = w - rw,
                fw_padding_height = h - rh,
                "vulkan-encode: producer-native NV12 encode source (true-size headers: the \
                 driver aligns the bitstream SPS itself and the firmware edge-extends the \
                 padding — the source is never read past its extent)"
            );
        } else {
            tracing::info!(
                rgb_direct = match (&rgb_probe, want_rgb, &rgb_cfg) {
                    (
                        _,
                        _,
                        Some(RgbDirect {
                            true_extent: true, ..
                        }),
                    ) =>
                        "active(true-extent: unaligned mode, direct import with the true-size \
                         source codedExtent — RADV firmware padding covers the alignment rows; \
                         PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0 restores the padded copy)",
                    (_, _, Some(RgbDirect { padded: false, .. })) => "active",
                    (_, _, Some(RgbDirect { padded: true, .. })) =>
                        "active(padded-copy: mode is not 64x16-aligned — staging blit + edge \
                         duplication instead of the direct import)",
                    (Ok(_), false, None) =>
                        "available(off: PUNKTFUNK_VULKAN_RGB_DIRECT=0, or a cursor-blend session \
                         — =1 forces)",
                    (Err(e), _, None) => e,
                    (Ok(_), true, None) => unreachable!("rgb gate and cfg disagree"),
                },
                "vulkan-encode: EFC RGB-direct encode source (design/vulkan-rgb-direct-encode.md)"
            );
        }

        // the encode profile — H265 Main, or AV1 Main; chained raw and uniformly (vendored AV1 +
        // rgb structs can't `push_next`, and the chain must match [`RgbProfileStack::wire`]'s
        // exactly when rgb is active — profile identity is by value):
        // profile → codec profile → usage (→ rgb-conversion when active).
        let rgb_info = vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
            s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
            p_next: std::ptr::null(),
            perform_encode_rgb_conversion: vk::TRUE,
        };
        let mut h265_profile =
            vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(h265_profile_idc(ten_bit));
        let mut av1_profile = av1b::VideoEncodeAV1ProfileInfoKHR {
            s_type: av1b::stype(av1b::ST_PROFILE_INFO),
            p_next: std::ptr::null(),
            std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
        };
        let mut usage = vk::VideoEncodeUsageInfoKHR::default()
            .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
            .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
            .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
        if rgb_cfg.is_some() {
            usage.p_next = &rgb_info as *const _ as *const c_void;
        }
        // A device that cannot encode 10-bit fails `get_physical_device_video_capabilities`
        // below with VIDEO_PROFILE_FORMAT_NOT_SUPPORTED, which fails the open — and a failed
        // Vulkan open falls back to libav VAAPI in `open_amd_intel`. That is the whole 10-bit
        // capability gate: no separate probe, and no way to reach a half-configured session.
        let depth = component_depth(ten_bit);
        let mut profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(codec_op)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(depth)
            .chroma_bit_depth(depth);
        if av1 {
            av1_profile.p_next = &usage as *const _ as *const c_void;
            profile.p_next = &av1_profile as *const _ as *const c_void;
        } else {
            h265_profile.p_next = &usage as *const _ as *const c_void;
            profile.p_next = &h265_profile as *const _ as *const c_void;
        }

        // capabilities (codec chain required for encode) -> std header version, coded alignment, RC modes
        let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
        let mut av1_caps: av1b::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
        av1_caps.s_type = av1b::stype(av1b::ST_CAPABILITIES);
        let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut enc_caps);
        if av1 {
            av1_caps.p_next = caps.p_next;
            caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
        } else {
            caps = caps.push_next(&mut h265_caps);
        }
        let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
        if r != vk::Result::SUCCESS {
            bail!("get_physical_device_video_capabilities: {r:?}");
        }
        // Copy every needed capability out now — `caps` holds `&mut` borrows of the chained
        // codec/encode caps structs, so no field of those may be touched while it lives.
        let std_hdr = caps.std_header_version;
        let min_bs_align = caps.min_bitstream_buffer_size_alignment.max(1);
        let max_quality_levels = enc_caps.max_quality_levels;
        let rate_control_modes = enc_caps.rate_control_modes;
        let hw_max_bitrate = match enc_caps.max_bitrate {
            0 => u64::MAX, // not filled in — treat as unbounded
            n => n,
        };
        let av1_superblock128 = av1 && (av1_caps.superblock_sizes & av1b::SUPERBLOCK_SIZE_128 != 0);
        // Resolve the encode quality level against the profile's cap (spec: valid levels are
        // 0..maxQualityLevels, ordered fastest→best; maxQualityLevels >= 1 always). INFO so field
        // logs show which VCN preset tier a session ran — the default is the fastest one.
        let quality_level = quality_request().min(max_quality_levels.saturating_sub(1));
        tracing::info!(
            quality_level,
            max_quality_levels,
            "vulkan-encode: quality level (0 = fastest preset; PUNKTFUNK_VULKAN_QUALITY overrides)"
        );
        // WP6.3: rate-control mode + HRD window, from the capability bits this query used to
        // ignore. VBR — always with average == max, i.e. capped at the target — is what stops
        // CBR's bit-stuffing: a calm CBR stream overflows the CPB once the initial fill drains
        // and the driver must then pad EVERY frame to the exact rate share, forever (measured on
        // the 780M: 97.5 % HEVC / 99.6 % AV1 filler over a calm 64-frame run — under the shipped
        // 1000 ms window, not just a tight one; the earlier "loose CBR = no filler" reading came
        // from an 8-frame smoke shorter than the fill-drain time). Vulkan exposes no
        // filler-suppression control, so the MODE is the only lever. The tight house window
        // rides along as the driver's RC input, but measured on RADV it does NOT bound complex
        // frames (burst A/B byte-identical against 1000 ms CBR — on this silicon the window had
        // no measurable effect on frame sizes; the MODE alone decided the stuffing), so no
        // pacing claim is made for it. A CBR-only driver keeps
        // the legacy loose (1000 ms, 500 ms) window on purpose: tightening it under CBR just
        // starts the stuffing ~30 frames earlier.
        let vbr_advertised =
            rate_control_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::VBR);
        if !vbr_advertised
            && !rate_control_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        {
            // Every real encode driver advertises CBR and/or VBR; one with neither would need
            // the DEFAULT-mode no-layer shape this backend does not speak. Keep CBR (the
            // pre-WP6.3 behaviour) but say so — this WARN firing names a driver worth knowing.
            tracing::warn!(
                modes = rate_control_modes.as_raw(),
                "vulkan-encode: driver advertises neither CBR nor VBR — installing CBR anyway \
                 (pre-existing behaviour; may fail validation on this driver)"
            );
        }
        // `PUNKTFUNK_VULKAN_RC=cbr|vbr`: field escape hatch + on-box A/B control (two prior
        // versions of this rate-control shape were withdrawn after review — a rebuild-free
        // fallback is cheap insurance). `vbr` is a request, honoured only when advertised;
        // anything else means auto.
        let vbr = match std::env::var("PUNKTFUNK_VULKAN_RC")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "cbr" => false,
            _ => vbr_advertised,
        };
        let (rc_mode, vbv_ms) = if vbr {
            (
                vk::VideoEncodeRateControlModeFlagsKHR::VBR,
                crate::vbv_window_ms(fps),
            )
        } else {
            (
                vk::VideoEncodeRateControlModeFlagsKHR::CBR,
                (1000u32, 500u32),
            )
        };
        // INFO because the mode and window used to be hardwired constants — a field log has to be
        // able to say which shape a session actually ran.
        tracing::info!(
            rc_mode = if vbr { "VBR (capped at target)" } else { "CBR" },
            vbv_window_ms = vbv_ms.0,
            vbv_initial_ms = vbv_ms.1,
            fps,
            hw_max_bitrate,
            "vulkan-encode: rate control (VBR when the driver offers it — CBR must stuff filler \
             on calm content; PUNKTFUNK_VULKAN_RC overrides, PUNKTFUNK_VBV_FRAMES scales the VBR \
             window)"
        );
        let bitrate = if bitrate > hw_max_bitrate {
            tracing::warn!(
                requested = bitrate,
                cap = hw_max_bitrate,
                "vulkan-encode: requested bitrate exceeds the driver's maxBitrate — clamping"
            );
            hw_max_bitrate
        } else {
            bitrate
        };
        // VK_EXT_queue_family_foreign: enable when advertised so the dmabuf-acquire barriers'
        // FOREIGN src family is spec-legal (Phase 8; `pf-presenter/dmabuf.rs` precedent). The
        // rgb probe's enumerate is a probe-local (and skipped on native-NV12), so this is a
        // fresh open-time query.
        let dev_ext_props = instance
            .enumerate_device_extension_properties(pd)
            .unwrap_or_default();
        let foreign_ok =
            crate::vk_util::ext_advertised(&dev_ext_props, ash::ext::queue_family_foreign::NAME);
        let foreign_qfi = if foreign_ok {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            tracing::warn!(
                "VK_EXT_queue_family_foreign not advertised — dmabuf acquires use the core \
                 QUEUE_FAMILY_EXTERNAL substitute (this arm has no fleet hardware; report it)"
            );
            vk::QUEUE_FAMILY_EXTERNAL
        };
        // logical device: encode + compute queues + video extensions (AV1 ext name is raw — ash lacks it)
        let mut dev_exts = vec![
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_encode_queue::NAME.as_ptr(),
            if av1 {
                av1b::EXTENSION_NAME.as_ptr()
            } else {
                ash::khr::video_encode_h265::NAME.as_ptr()
            },
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
        ];
        if rgb_cfg.is_some() {
            dev_exts.push(vrgb::EXTENSION_NAME.as_ptr());
        }
        if foreign_ok {
            dev_exts.push(ash::ext::queue_family_foreign::NAME.as_ptr());
        }
        let prio = [1.0f32];
        let mut qcis = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(encode_family)
            .queue_priorities(&prio)];
        if compute_family != encode_family {
            qcis.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(compute_family)
                    .queue_priorities(&prio),
            );
        }
        let mut sync2 =
            vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
        let mut device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qcis)
            .enabled_extension_names(&dev_exts)
            .push_next(&mut sync2);
        // The AV1-encode feature gate: `videoEncodeAV1` must be enabled for any ENCODE_AV1 use
        // (spec requirement; vendored struct since ash 0.38 predates it — chained raw like the
        // profile above).
        let mut av1_features = av1b::PhysicalDeviceVideoEncodeAV1FeaturesKHR {
            s_type: av1b::stype(av1b::ST_PHYSICAL_DEVICE_FEATURES),
            p_next: std::ptr::null_mut(),
            video_encode_av1: vk::TRUE,
        };
        if av1 {
            av1_features.p_next = device_ci.p_next as *mut c_void;
            device_ci.p_next = &av1_features as *const _ as *const c_void;
        }
        // The rgb-conversion feature gate (spec: must be enabled to chain the rgb profile).
        let mut rgb_features = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
            s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
            p_next: std::ptr::null_mut(),
            video_encode_rgb_conversion: vk::TRUE,
        };
        if rgb_cfg.is_some() {
            rgb_features.p_next = device_ci.p_next as *mut c_void;
            device_ci.p_next = &rgb_features as *const _ as *const c_void;
        }
        let device = instance
            .create_device(pd, &device_ci, None)
            .context("create device")?;
        let encode_queue = device.get_device_queue(encode_family, 0);
        let compute_queue = device.get_device_queue(compute_family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let vq_dev = ash::khr::video_queue::Device::new(&instance, &device);
        let venc_dev = ash::khr::video_encode_queue::Device::new(&instance, &device);
        guard.device = Some(device.clone());
        guard.vq_dev = Some(vq_dev.clone());

        // ---- video session ---- (AV1 pins the max level from caps via a chained create-info)
        let av1_sci = av1b::VideoEncodeAV1SessionCreateInfoKHR {
            s_type: av1b::stype(av1b::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            use_max_level: vk::TRUE,
            max_level: av1_caps.max_level,
        };
        // RGB-direct: the session's picture (encode-src) format is the captured RGB itself; the
        // chained create-info selects the conversion EFC performs. The DPB stays NV12 — the
        // reconstruction is YUV either way. Built unconditionally (function scope outlives the
        // create call); chained only when active.
        let mut rgb_sci = vrgb::VideoEncodeSessionRgbConversionCreateInfoVALVE {
            s_type: vrgb::stype(vrgb::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            rgb_model: rgb_model_for(ten_bit),
            rgb_range: vrgb::RANGE_NARROW,
            x_chroma_offset: rgb_cfg.as_ref().map_or(0, |c| c.x_offset),
            y_chroma_offset: rgb_cfg.as_ref().map_or(0, |c| c.y_offset),
        };
        let mut session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(encode_family)
            .video_profile(&profile)
            .picture_format(if rgb_cfg.is_some() {
                src_rgb_fmt
            } else {
                yuv_format(ten_bit)
            })
            .max_coded_extent(vk::Extent2D {
                width: w,
                height: h,
            })
            .reference_picture_format(yuv_format(ten_bit))
            .max_dpb_slots(DPB_SLOTS + 1)
            .max_active_reference_pictures(1)
            .std_header_version(&std_hdr);
        if av1 {
            session_ci.p_next = &av1_sci as *const _ as *const c_void;
        }
        if rgb_cfg.is_some() {
            // chain ahead of whatever is already there (the AV1 create-info keeps its place)
            rgb_sci.p_next = session_ci.p_next;
            session_ci.p_next = &rgb_sci as *const _ as *const c_void;
        }
        let mut session = vk::VideoSessionKHR::null();
        let r = (vq_dev.fp().create_video_session_khr)(
            device.handle(),
            &session_ci,
            std::ptr::null(),
            &mut session,
        );
        if r != vk::Result::SUCCESS {
            bail!("create_video_session: {r:?}");
        }
        guard.session = session;
        // bind session memory
        let get_mem = vq_dev.fp().get_video_session_memory_requirements_khr;
        let mut n = 0u32;
        let _ = get_mem(device.handle(), session, &mut n, std::ptr::null_mut());
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        let _ = get_mem(device.handle(), session, &mut n, reqs.as_mut_ptr());
        let mut binds = Vec::new();
        for rq in &reqs {
            let mr = rq.memory_requirements;
            let ti = find_mem(
                &mem_props,
                mr.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            );
            let m = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(mr.size)
                    .memory_type_index(ti),
                None,
            )?;
            guard.session_mem.push(m);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(rq.memory_bind_index)
                    .memory(m)
                    .memory_offset(0)
                    .memory_size(mr.size),
            );
        }
        let r = (vq_dev.fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            binds.len() as u32,
            binds.as_ptr(),
        );
        if r != vk::Result::SUCCESS {
            bail!("bind_video_session_memory: {r:?}");
        }

        // ---- session parameters + header framing (HEVC: VPS/SPS/PPS on keyframes; AV1: a
        //      temporal-delimiter OBU per frame + a sequence-header OBU on keyframes) ----
        // Native NV12 authors TRUE-SIZE headers (SPS/seq at the render size, no app-side
        // conformance window): RADV rounds the bitstream SPS up itself and, keyed off the
        // matching true-size codedExtent, programs the VCN with firmware padding so the source
        // is never read past its extent. The CSC/RGB paths keep the app-aligned convention
        // (their sources genuinely cover the aligned extent).
        // AV1 + RGB true-extent joins the true-size camp for the same reason: true-extent shrinks
        // `srcPictureResource.codedExtent` to the render size, and AV1 forbids the sequence header
        // (`flags-10324`) or the reference slots (`flags-10325`) disagreeing with the source unless
        // the GPU advertises FRAME_SIZE_OVERRIDE / MOTION_VECTOR_SCALING — RADV PHOENIX advertises
        // neither. Rather than give up the EFC fast path, make all three agree at the RENDER size:
        // the coded frame genuinely IS the visible size (an unpadded coded size is valid on this
        // hardware), so nothing codes alignment padding at all and the reference slots follow the
        // source extent below.
        //
        // HEVC deliberately stays on the app-aligned convention: it has no equivalent constraint
        // (its crop rides the conformance window) and that path is the validated one.
        let (hdr_w, hdr_h) =
            if native_nv12 || (av1 && rgb_cfg.as_ref().is_some_and(|c| c.true_extent)) {
                (rw, rh)
            } else {
                (w, h)
            };
        let (params, header, frame_prefix) = if av1 {
            build_parameters_av1(
                &device,
                &vq_dev,
                session,
                hdr_w,
                hdr_h,
                rw,
                rh,
                av1_caps.max_level,
                av1_superblock128,
                quality_level,
                ten_bit,
            )?
        } else {
            let (p, hdr) = build_parameters_h265(
                &device,
                &vq_dev,
                &venc_dev,
                session,
                hdr_w,
                hdr_h,
                rw,
                rh,
                quality_level,
                ten_bit,
            )?;
            (p, hdr, Vec::new())
        };
        guard.params = params;

        // ---- DPB image (NV12 OPTIMAL, ring of slots) — encode queue only ----
        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(&profile));
        let (dpb_image, dpb_mem) = make_video_image(
            &device,
            &mem_props,
            yuv_format(ten_bit),
            w,
            h,
            DPB_SLOTS,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            &mut profile_list,
            &[],
        )?;
        guard.dpb_image = dpb_image;
        guard.dpb_mem = dpb_mem;
        for slot in 0..DPB_SLOTS {
            guard
                .dpb_views
                .push(make_view(&device, dpb_image, yuv_format(ten_bit), slot)?);
        }

        // NV12 encode-src, CSC scratch (Y/UV), bitstream, query and command buffers are all per
        // in-flight frame (built in `make_frame` below); only the queue-family list is shared here.
        let fams = if compute_family == encode_family {
            vec![]
        } else {
            vec![compute_family, encode_family]
        };

        // ---- CSC compute pipeline (shared across the frame ring) ----
        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        guard.sampler = sampler;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(if ten_bit {
            CSC10_SPV
        } else {
            CSC_SPV
        }))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
        guard.shader = shader;
        let sb = |b: u32, t: vk::DescriptorType| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            sb(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
            sb(1, vk::DescriptorType::STORAGE_IMAGE),
            sb(2, vk::DescriptorType::STORAGE_IMAGE),
            sb(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER), // cursor overlay
        ];
        let csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        guard.csc_dsl = csc_dsl;
        let dsls = [csc_dsl];
        // Push constant: cursor {ivec2 origin, ivec2 size} = 16 bytes (size.x<=0 disables the blend).
        let pc_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(16)];
        let csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&dsls)
                .push_constant_ranges(&pc_ranges),
            None,
        )?;
        guard.csc_layout = csc_layout;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let csc_pipe = device
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .layout(csc_layout)
                    .stage(stage)],
                None,
            )
            .map_err(|(_, e)| e)?[0];
        guard.csc_pipe = csc_pipe;
        device.destroy_shader_module(shader, None);
        // The shader is gone — null the guard's copy so a later failure doesn't unwind it again.
        guard.shader = vk::ShaderModule::null();
        // One CSC descriptor set + its own Y/UV/NV12/bitstream per in-flight frame.
        let nframes = ring_depth();
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                // binding 0 (RGB) + binding 3 (cursor) per set.
                .descriptor_count(2 * nframes as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * nframes as u32),
        ];
        let csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(nframes as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        guard.csc_pool = csc_pool;

        // ---- bitstream size (shared) + shared command pools ----
        let bs_size = align_up(3 * w as u64 * h as u64 + (1 << 16), min_bs_align);
        let cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(encode_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        guard.cmd_pool = cmd_pool;
        let compute_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(compute_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        guard.compute_pool = compute_pool;

        // ---- build the in-flight frame ring ----
        for _ in 0..nframes {
            // Pre-push a null Frame and build it in place, so a mid-`make_frame` failure leaves
            // the partial handles in the guard rather than losing them with the Err.
            guard.frames.push(Frame::default());
            make_frame(
                &device,
                &mem_props,
                w,
                h,
                &fams,
                &profile,
                &mut profile_list,
                csc_dsl,
                csc_pool,
                cmd_pool,
                compute_pool,
                bs_size,
                sampler,
                ts_period_ns > 0.0
                    && ((rgb_cfg.is_none() && !native_nv12)
                        || rgb_cfg.as_ref().is_some_and(|c| c.padded)),
                rgb_cfg.is_none() && !native_nv12,
                rgb_cfg
                    .as_ref()
                    .is_some_and(|c| c.padded)
                    .then_some(src_rgb_fmt),
                ten_bit,
                guard.frames.last_mut().expect("frame just pushed"),
            )?;
        }

        // Fully constructed: move the built collections out and disarm the guard — from here every
        // handle is owned by `Self`, whose own `Drop` is the (only) teardown path.
        let session_mem = std::mem::take(&mut guard.session_mem);
        let dpb_views = std::mem::take(&mut guard.dpb_views);
        let frames = std::mem::take(&mut guard.frames);
        std::mem::forget(guard);

        Ok(Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            vq_dev,
            venc_dev,
            encode_queue,
            compute_queue,
            encode_family,
            foreign_qfi,
            compute_family,
            mem_props,
            codec,
            session,
            session_mem,
            params,
            header,
            frame_prefix,
            dpb_image,
            dpb_mem,
            dpb_views,
            slot_wire: vec![-1; DPB_SLOTS as usize],
            slot_poc: vec![-1; DPB_SLOTS as usize],
            prev_slot: 0,
            csc_pipe,
            csc_layout,
            csc_dsl,
            csc_pool,
            sampler,
            import_cache: Vec::new(),
            frames,
            ring: 0,
            in_flight: VecDeque::new(),
            bs_size,
            cmd_pool,
            compute_pool,
            bitrate,
            fps,
            rc_mode,
            vbv_ms,
            hw_max_bitrate,
            quality_level,
            ts_period_ns,
            perf_at: std::time::Instant::now(),
            cpu_expand: Vec::new(),
            rgb: rgb_cfg,
            native_nv12,
            ten_bit,
            pending_bitrate: None,
            width: w,
            height: h,
            render_w: rw,
            render_h: rh,
            poc: 0,
            enc_count: 0,
            auto_wire: 0,
            first_frame: true,
            // No rate control installed on a brand-new session: its mode is DEFAULT until the
            // first frame's control command, so frame 0 must NOT declare one at begin-coding.
            rc_installed: false,
            force_kf: false,
            pending_loss: None,
            pending: VecDeque::new(),
        })
    }
}

impl VulkanVideoEncoder {
    /// Point a slot's CSC descriptor binding 0 at the current frame's RGB image view.
    unsafe fn bind_rgb(&self, csc_set: vk::DescriptorSet, rgb_view: vk::ImageView) {
        let ii0 = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii0)],
            &[],
        );
    }

    /// Cursor-as-metadata: bring slot `slot`'s cursor image up to date for this frame and return the
    /// shader push constant `[origin_x, origin_y, size_w, size_h]` (size 0 ⇒ the CSC skips the blend).
    /// Records the small upload (only when the bitmap `serial` changed) + layout transition into
    /// `compute_cmd`, ahead of the CSC dispatch that samples binding 3. Per-slot, so no cross-frame
    /// race; the first use of a slot always transitions the image to a valid SHADER_READ_ONLY layout.
    unsafe fn prep_cursor(
        &mut self,
        slot: usize,
        compute_cmd: vk::CommandBuffer,
        cursor: Option<&pf_frame::CursorOverlay>,
    ) -> Result<[i32; 4]> {
        let dev = self.device.clone();
        let img = self.frames[slot].cursor_img;
        let ready = self.frames[slot].cursor_ready;
        let barrier = |old: vk::ImageLayout, new: vk::ImageLayout, ss, sa, ds, da| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ss)
                .src_access_mask(sa)
                .dst_stage_mask(ds)
                .dst_access_mask(da)
                .old_layout(old)
                .new_layout(new)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        match cursor {
            Some(c) if !c.rgba.is_empty() => {
                let cw = c.w.min(CURSOR_MAX);
                let ch = c.h.min(CURSOR_MAX);
                if self.frames[slot].cursor_serial != c.serial {
                    let stage = self.frames[slot].cursor_stage;
                    let stage_mem = self.frames[slot].cursor_stage_mem;
                    let bytes = (cw as usize) * (ch as usize) * 4;
                    let ptr =
                        dev.map_memory(stage_mem, 0, bytes as u64, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(
                        c.rgba.as_ptr(),
                        ptr as *mut u8,
                        bytes.min(c.rgba.len()),
                    );
                    dev.unmap_memory(stage_mem);
                    let old = if ready {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    } else {
                        vk::ImageLayout::UNDEFINED
                    };
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            old,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                        )]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        compute_cmd,
                        stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width: cw,
                                height: ch,
                                depth: 1,
                            })],
                    );
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.frames[slot].cursor_serial = c.serial;
                    self.frames[slot].cursor_ready = true;
                }
                Ok([c.x, c.y, cw as i32, ch as i32])
            }
            _ => {
                if !ready {
                    // No cursor uploaded yet — transition UNDEFINED→READ_ONLY once so binding 3 is a
                    // valid layout for the (guarded, never-sampled) shader read.
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.frames[slot].cursor_ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Import a DMA-BUF VkImage with usage/profile matching this session's source mode. Native
    /// NV12 and aligned RGB-direct are profiled `VIDEO_ENCODE_SRC` images. Padded RGB-direct
    /// imports the producer allocation as transfer-source only.
    unsafe fn import_dmabuf(
        &self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
        if self.native_nv12 {
            let mut ps =
                NativeProfileStack::new(codec_op_for(self.codec == Codec::Av1), self.ten_bit);
            let profile = *ps.wire(self.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            return super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
                Some(&mut plist),
            );
        }
        if self.rgb.as_ref().is_some_and(|r| r.padded) {
            // Padded-copy mode: the import is only ever a transfer SOURCE (the blit into the
            // aligned staging image) — plain TRANSFER_SRC, no video profile involved.
            super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::TRANSFER_SRC,
                None,
            )
        } else if self.rgb.is_some() {
            let mut ps = RgbProfileStack::new(codec_op_for(self.codec == Codec::Av1), self.ten_bit);
            let profile = *ps.wire(self.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
                Some(&mut plist),
            )
        } else {
            super::vk_util::import_rgb_dmabuf(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
            )
        }
    }

    /// Import a dmabuf, reusing a cached per-buffer import when the same underlying buffer recurs
    /// (PipeWire cycles a small fixed pool). Keyed by `(st_dev, st_ino)` because each `DmabufFrame`
    /// owns a fresh *dup* — a new fd number, same inode. Returns `(image, view, fresh)`; `fresh` is
    /// true only on a first import (caller uses UNDEFINED old-layout to preserve modifier-tiled data).
    unsafe fn import_cached(
        &mut self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let mut st: libc::stat = std::mem::zeroed();
        let key = if libc::fstat(d.fd.as_raw_fd(), &mut st) == 0 {
            (st.st_dev as u64, st.st_ino as u64)
        } else {
            // fstat failed → uncacheable; a per-frame-unique sentinel key never matches, so this
            // frame imports fresh (as before) but is still owned by the cache and freed on evict/Drop.
            (u64::MAX, self.enc_count)
        };
        if let Some(pos) = self.import_cache.iter().position(|e| e.key == key) {
            let e = &self.import_cache[pos];
            if e.extent == (cw, ch) {
                return Ok((e.img, e.view, false));
            }
            // Key hit, wrong extent: the (st_dev, st_ino) now names a different allocation than
            // the one imported. Unreachable while every submit path guards frame-vs-session
            // dimensions first — but the cache must never hand out a stale-sized image if a
            // future path forgets, so evict and fall through to a fresh import. In-flight frames
            // may still read the old image (same hazard as the FIFO eviction below), so idle the
            // device before destroying. At most once per reallocation.
            let _ = self.device.device_wait_idle();
            let e = self.import_cache.remove(pos);
            self.device.destroy_image_view(e.view, None);
            self.device.destroy_image(e.img, None);
            self.device.free_memory(e.mem, None);
        }
        // Feed pf-zerocopy's raw-dmabuf degrade latch (wired for the libav path in 3efbe416):
        // a deterministic import refusal repeats identically forever, and the latch — capture
        // negotiates CPU delivery from the next session — is its only recovery; the CPU path
        // serves every format capture produces (`normalize_cpu_rgb`). Excluded: transient
        // allocation OOM (`import_failure_feeds_latch`), and native-NV12 sessions entirely — an
        // NV12-layout quirk should cost the NV12 preference, not ALL dmabuf capture, and that
        // narrower response doesn't exist yet (noted in the phase-5 handoff as follow-up).
        let (img, mem, view) = match self.import_dmabuf(d, cw, ch) {
            Ok(t) => {
                if !self.native_nv12 {
                    pf_zerocopy::note_raw_dmabuf_import_ok();
                }
                t
            }
            Err(e) => {
                if !self.native_nv12 && import_failure_feeds_latch(&e) {
                    pf_zerocopy::note_raw_dmabuf_import_failure(&format!("{e:#}"));
                }
                return Err(e);
            }
        };
        // Bound the cache; evict oldest (FIFO). A stable PipeWire pool never trips this in steady state
        // (all imports resident); it only cycles across a pool change (which also rebuilds the session).
        // Up to `ring_depth - 1` submitted frames may still be executing against a cached image
        // (`enqueue` only drains down to `frames.len()`, and `record_submit` imports before it
        // records), so destroying an evicted import here is a GPU-side use-after-free. `Drop` and
        // `reset()` both idle the device first; this was the one unguarded destroy. Guarded on the
        // length test so the steady-state path — where the cache is resident and never evicts —
        // pays nothing.
        if self.import_cache.len() >= IMPORT_CACHE_CAP {
            let _ = self.device.device_wait_idle();
        }
        while self.import_cache.len() >= IMPORT_CACHE_CAP {
            let e = self.import_cache.remove(0);
            self.device.destroy_image_view(e.view, None);
            self.device.destroy_image(e.img, None);
            self.device.free_memory(e.mem, None);
        }
        self.import_cache.push(CachedImport {
            key,
            extent: (cw, ch),
            img,
            mem,
            view,
        });
        // Fires once per distinct pool buffer then goes quiet in steady state — the signal the cache
        // is hitting (a per-frame log here would mean inode keying failed and we're re-importing).
        tracing::debug!(
            resident = self.import_cache.len(),
            "vulkan-encode: imported a new dmabuf buffer"
        );
        Ok((img, view, true))
    }

    /// Reusable RGB image + staging buffer for software (CPU) capture, private to one frame slot;
    /// (re)created on format change. Only the software-capture / smoke-test path exercises this.
    unsafe fn ensure_cpu_rgb(
        &mut self,
        slot: usize,
        fmt: vk::Format,
        bytes: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // CSC mode: the image is the SHADER'S sampling source — size it to the real frame so
        // the clamp-to-edge duplicates true content (the aligned-size image made the clamp land
        // on unwritten rows: black garbage at unaligned modes, the pre-B2 smoke-baseline bug).
        // RGB mode: the image IS the encode source — aligned dims, CPU-side padding below.
        let (iw, ih) = if self.rgb.is_some() {
            (w, h)
        } else {
            (src_w, src_h)
        };
        // Widened BEFORE the multiply, not after: `(iw * ih * 4) as u64` multiplied in u32 and
        // widened the result, so it agreed with the usize slice length below only while
        // `iw * ih <= 2^30`. The 8192 dimension cap leaves 16x of margin, but the padded-write arm
        // now sizes a raw-pointer slice from these numbers, and `read_slot` already applies exactly
        // this discipline to driver-reported sizes for exactly this reason.
        let need = iw as u64 * ih as u64 * 4;
        if self.frames[slot]
            .cpu_img
            .map(|(_, _, _, f, iw, ih)| (f, iw, ih))
            != Some((fmt, iw, ih))
        {
            if let Some((i, m, v, ..)) = self.frames[slot].cpu_img.take() {
                dev.destroy_image_view(v, None);
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            let (i, m, v) = if self.rgb.is_some() {
                // RGB-direct: the uploaded RGB image IS the encode source — profiled, encode
                // usage, and shared with the encode queue (the compute queue only copies into
                // it; the semaphore orders the hand-off, CONCURRENT avoids a QFOT).
                let av1 = self.codec == Codec::Av1;
                let mut ps = RgbProfileStack::new(codec_op_for(av1), self.ten_bit);
                let profile = *ps.wire(av1);
                let arr = [profile];
                let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
                let fams: &[u32] = if self.encode_family == self.compute_family {
                    &[]
                } else {
                    &[self.encode_family, self.compute_family]
                };
                let fams = fams.to_vec();
                let (i, m) = make_video_image(
                    &dev,
                    &self.mem_props,
                    fmt,
                    w,
                    h,
                    1,
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
                    &mut plist,
                    &fams,
                )?;
                // `make_video_image` unwinds internally, but a view failure here leaked the
                // image+memory pair it had just handed over.
                let v = match make_view(&dev, i, fmt, 0) {
                    Ok(v) => v,
                    Err(e) => {
                        dev.destroy_image(i, None);
                        dev.free_memory(m, None);
                        return Err(e);
                    }
                };
                (i, m, v)
            } else {
                make_plain_image(
                    &dev,
                    &self.mem_props,
                    fmt,
                    iw,
                    ih,
                    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                )?
            };
            self.frames[slot].cpu_img = Some((i, m, v, fmt, iw, ih));
        }
        if self.frames[slot]
            .cpu_stage
            .map(|(_, _, s)| s < need)
            .unwrap_or(true)
        {
            if let Some((b, m, _)) = self.frames[slot].cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let (buf, mem) = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            self.frames[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.frames[slot].cpu_stage.unwrap();
        // RGB-direct sessions upload the image the ENCODER reads directly, so an undersized
        // source (unaligned mode) must be padded — rows/columns duplicated from the edge, matching
        // the CSC shader's clamped reads (and `record_pad_blit`'s GPU-side equivalent). The CSC
        // path keeps the raw copy: its shader clamps at sample time, and it DELIBERATELY supports a
        // source smaller than the encode extent — so none of the guards below may be hoisted out of
        // this branch, they are only sound because `self.rgb.is_some()`.
        //
        // Every fallible step stays ABOVE the map: an error raised between `map_memory` and
        // `unmap_memory` would strand the mapping for the life of this slot's staging buffer.
        let pad = if self.rgb.is_some() && (src_w != w || src_h != h) {
            let (sw, sh) = (src_w as usize, src_h as usize);
            // A zero axis makes `sh - 1` below underflow. It cannot arrive from a real capturer,
            // but refusing it by name is what lets the padding write be infallible under the map.
            if src_w == 0 || src_h == 0 {
                bail!("vulkan-encode (rgb-direct): CPU frame has a zero axis ({src_w}x{src_h})");
            }
            // Extent FIRST, deliberately: the payload-length check below multiplies `sw * sh * 4`
            // in usize on caller-supplied dimensions, so bounding them against the encode extent
            // first is what keeps that multiply from overflowing on a garbage frame header.
            // The padding write below only ever GROWS the source into the aligned extent. A source
            // larger than the encode extent is a contract violation (the Dmabuf arms already bail
            // on it) and would panic on the row `copy_from_slice`, so refuse it by name instead of
            // unwinding out of the encode thread with an index message.
            if src_w > w || src_h > h {
                bail!(
                    "vulkan-encode (rgb-direct): CPU frame {}x{} exceeds the encode extent {}x{} \
                     — the RGB-direct source must fit inside the aligned encode size",
                    src_w,
                    src_h,
                    w,
                    h
                );
            }
            if bytes.len() < sw * sh * 4 {
                bail!(
                    "vulkan-encode (rgb-direct): CPU frame {}x{} needs {} bytes, got {}",
                    src_w,
                    src_h,
                    sw * sh * 4,
                    bytes.len()
                );
            }
            // The `unsafe` slice below is sized from `(w, h)` while the staging buffer is sized
            // from `need`. That equality is the precondition of the whole write, so CHECK it —
            // here, above the map, in release. It was a `debug_assert!` below the map, which was
            // wrong twice over: it made the one guard on the slice length vanish in the builds that
            // ship, and a fired assert would have unwound past `unmap_memory` and stranded the
            // mapping for the life of the slot (the next frame's `map_memory` then violates
            // VUID-vkMapMemory-memory-00678). Unreachable under the 8192 dimension cap either way —
            // but "every fallible step stays above the map" has to be true, not nearly true.
            let dst_len = (w as usize)
                .checked_mul(h as usize)
                .and_then(|n| n.checked_mul(4))
                .filter(|&n| n as u64 == need)
                .context("vulkan-encode (rgb-direct): staging buffer is not the encode extent")?;
            Some((sw, sh, dst_len))
        } else {
            None
        };
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        match pad {
            // WP6.2(a): build the padded frame STRAIGHT into the staging memory. This used to
            // allocate and zero-fill a whole padded frame every submit (8 MB at 1080p — 4K UHD is
            // 64x16-aligned and never reaches this branch; the big case is an ultrawide like
            // 3440x1440 -> 3456x1440, ~20 MB) and then memcpy it in here — with the zero-fill
            // entirely dead, because the loop writes every byte of every row. Now: no per-frame
            // allocation, no zero-fill, no page-fault storm over a fresh mapping, and one
            // full-frame copy instead of two. Nothing reads back from `dst`, so writing into
            // (possibly write-combined) host memory costs nothing extra — the row source is always
            // `bytes`, never the destination.
            Some((sw, sh, dst_len)) => {
                let (dw, dh) = (w as usize, h as usize);
                // SAFETY: `dst_len == dw * dh * 4 == need`, checked above the map, and the staging
                // buffer is only kept when its size is >= `need`, so the mapping covers this slice.
                // `make_host_buffer` allocates HOST_COHERENT memory, so the writes need no explicit
                // flush before the transfer submitted below reads them. The guards above make every
                // index here in-bounds by construction.
                let dst = std::slice::from_raw_parts_mut(p, dst_len);
                for y in 0..dh {
                    let sy = y.min(sh - 1);
                    let srow = &bytes[sy * sw * 4..][..sw * 4];
                    let drow = &mut dst[y * dw * 4..][..dw * 4];
                    drow[..sw * 4].copy_from_slice(srow);
                    let mut last = [0u8; 4];
                    last.copy_from_slice(&srow[(sw - 1) * 4..]);
                    for x in sw..dw {
                        drow[x * 4..(x + 1) * 4].copy_from_slice(&last);
                    }
                }
            }
            None => {
                let n = bytes.len().min(need as usize);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
            }
        }
        dev.unmap_memory(m);
        Ok(self.frames[slot].cpu_img.unwrap().2)
    }

    /// Record one frame's CSC + HEVC encode (+ RFI) into ring `slot` and submit it WITHOUT waiting.
    /// The slot's fence is polled later (`read_slot`) so this frame's GPU work overlaps the next
    /// capture. `slot` must be free (its prior submission already read back).
    unsafe fn record_submit(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        wire: i64,
    ) -> Result<()> {
        let (w, h_px) = (self.width, self.height);
        // Copy this slot's Vulkan handles into locals (all `vk::*` handles are Copy) so the rest of
        // the function can still borrow `&mut self` for the import/CSC helpers without aliasing
        // `self.frames`. Shared objects (csc_pipe/layout, dpb, session) stay on `self`.
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let ts_pool = self.frames[slot].ts_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let csc_set = self.frames[slot].csc_set;
        let y_img = self.frames[slot].y_img;
        let uv_img = self.frames[slot].uv_img;
        let nv12_src = self.frames[slot].nv12_src;
        let nv12_view = self.frames[slot].nv12_view;

        // ---- 1. decide frame type + reference (RFI) ----
        // A pending rate retarget (`reconfigure_bitrate`) stays PENDING through recording on
        // every path: the record fns declare the session's current state at begin-coding and
        // install the pending rate via ENCODE_RATE_CONTROL (folded into the first frame's RESET
        // install, or a standalone mid-stream control), and step 5 promotes it. Promoting it
        // here instead — which the first-frame path used to do — made the begin declaration name
        // a rate the session had not installed whenever a `reset()` preceded this frame
        // (`rc_installed` survives a reset; the session object keeps the OLD rate): a one-frame
        // VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08254 violation on exactly the frames where
        // an ABR retarget and the stall watchdog coincide.
        let mut is_idr = self.first_frame || self.force_kf;
        let mut ref_slot = self.prev_slot;
        let mut recovery = false;
        if let Some(lf) = self.pending_loss.take() {
            if !is_idr {
                // Pick-only re-run of the shared policy: the taint sweep already happened in
                // `invalidate_ref_frames`; the arm carries the loss start, not the slot, so the
                // anchor is resolved against the table as it stands NOW.
                match crate::rfi::pick_anchor(&trusted_refs(&self.slot_wire), lf) {
                    Some((s, _)) => {
                        ref_slot = s;
                        recovery = true;
                        tracing::debug!(
                            loss_first = lf,
                            anchor_slot = s,
                            anchor_wire = self.slot_wire[s],
                            "vulkan-encode: emitting clean recovery-anchor P-frame (references a known-good frame older than the loss, no IDR)"
                        );
                    }
                    None => {
                        is_idr = true;
                        tracing::debug!(loss_first = lf, "vulkan-encode: no resident reference older than the loss — forcing IDR");
                    }
                }
            }
        }
        let poc: i32 = if is_idr { 0 } else { self.poc };
        let mut setup_idx = (self.enc_count % DPB_SLOTS as u64) as usize;
        if !is_idr && setup_idx == ref_slot {
            setup_idx = (setup_idx + 1) % DPB_SLOTS as usize;
        }

        // ---- 2..4 diverge by encode source; native NV12 and RGB-direct return through the
        //      shared bookkeeping tail ----
        if self.native_nv12 {
            self.record_submit_nv12(slot, frame, is_idr, recovery, ref_slot, setup_idx, poc)?;
            self.post_submit_bookkeeping(
                slot,
                frame.pts_ns,
                wire,
                is_idr,
                recovery,
                setup_idx,
                poc,
            );
            return Ok(());
        }
        if self.rgb.is_some() {
            self.record_submit_rgb(slot, frame, is_idr, recovery, ref_slot, setup_idx, poc)?;
            self.post_submit_bookkeeping(
                slot,
                frame.pts_ns,
                wire,
                is_idr,
                recovery,
                setup_idx,
                poc,
            );
            return Ok(());
        }

        // ---- 2. RGB source -> compute_cmd: prep barriers + CSC + copy into nv12_src ----
        // The CSC twin of the sibling guards (native NV12, RGB-direct's two arms, every other
        // backend's submit): the shader samples the source with clamped 1:1 texelFetch, so a
        // mismatched frame doesn't fault — it silently streams a cropped/edge-padded picture.
        // Refuse it into the encoder-rebuild path instead.
        if frame.width != self.render_w || frame.height != self.render_h {
            bail!(
                "vulkan-encode (csc): frame {}x{} != mode {}x{} — refusing a mismatched CSC \
                 source",
                frame.width,
                frame.height,
                self.render_w,
                self.render_h
            );
        }
        let dev = self.device.clone(); // cheap handle clone -> lets us also call &mut self helpers
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        // The fallible recording prefix (cursor prep, dmabuf import, CPU staging) runs in one
        // closure whose error arm resets `compute_cmd`: an early return used to leave the buffer
        // RECORDING, and the next frame's `begin` on a RECORDING buffer violates
        // VUID-vkBeginCommandBuffer-commandBuffer-00049 — the pool's implicit reset covers only
        // INITIAL/EXECUTABLE/INVALID. Never PENDING here: nothing is submitted until stage 4, so
        // the error-arm reset is legal on every path.
        let prefix = (|| -> Result<([i32; 4], vk::ImageView)> {
            // PUNKTFUNK_PERF: bracket the whole compute batch (import barriers + cursor prep + CSC
            // dispatch + plane copies) with timestamps — read back in `read_slot` as `csc_us`, the
            // half of the fence wait that is NOT the ASIC encode.
            self.frames[slot].ts_written = false;
            if self.ts_period_ns > 0.0 {
                dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
                dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
                // This batch resets the pool and closes it below, so `read_slot` may read it.
                self.frames[slot].ts_written = true;
            }

            // Cursor-as-metadata: refresh this slot's cursor image (only when the bitmap changed)
            // and get the shader push constant. Recorded into `compute_cmd` before the CSC
            // dispatch samples it.
            let cursor_pc = self.prep_cursor(slot, compute_cmd, frame.cursor.as_ref())?;

            let rgb_view = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    // Reuse the per-buffer import (PipeWire cycles a small pool) — no per-frame VkImage
                    // create/import/destroy. The producer wrote new content out-of-band, so still acquire
                    // from FOREIGN each frame; a fresh import starts UNDEFINED (preserves modifier-tiled
                    // data), a cached one is already SHADER_READ_ONLY_OPTIMAL.
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    // First import: acquire from the foreign producer (UNDEFINED preserves the modifier-tiled
                    // bytes). Cached re-read: we still own it, so no queue-family transfer — just a visibility
                    // barrier so the shader read sees the content the producer wrote out-of-band this frame
                    // (single-GPU coherent; the capture layer guarantees the buffer is ready at hand-off).
                    let (old, src_qf, dst_qf) = if fresh {
                        (
                            vk::ImageLayout::UNDEFINED,
                            self.foreign_qfi,
                            self.compute_family,
                        )
                    } else {
                        (
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::QUEUE_FAMILY_IGNORED,
                            vk::QUEUE_FAMILY_IGNORED,
                        )
                    };
                    let acq = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(old)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(src_qf)
                        .dst_queue_family_index(dst_qf)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    view
                }
                FramePayload::Cpu(bytes) => {
                    // 24-bpp Rgb/Bgr expands 3→4 first (see `normalize_cpu_rgb`) — refusing it here
                    // used to kill the session at its first frame, and the RGB-direct padding branch
                    // in `ensure_cpu_rgb` does 4-bpp index math on the raw bytes, so the expansion
                    // must happen BEFORE any of it sees the payload.
                    let mut scratch = std::mem::take(&mut self.cpu_expand);
                    let (norm_fmt, norm_bytes) =
                        normalize_cpu_rgb(frame.format, bytes, &mut scratch, false);
                    let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                    let view = match fmt {
                        Ok(f) => {
                            self.ensure_cpu_rgb(slot, f, norm_bytes, frame.width, frame.height)
                        }
                        Err(e) => Err(e),
                    };
                    self.cpu_expand = scratch;
                    let view = view?;
                    let (img, ..) = self.frames[slot].cpu_img.unwrap();
                    let (stage, ..) = self.frames[slot].cpu_stage.unwrap();
                    let to_dst = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        compute_cmd,
                        stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            // The staging buffer holds the REAL frame tightly packed and the image
                            // is source-sized (see ensure_cpu_rgb) — an aligned-size extent here
                            // sheared rows against the packed buffer and left garbage rows at
                            // unaligned modes.
                            .image_extent(vk::Extent3D {
                                width: frame.width,
                                height: frame.height,
                                depth: 1,
                            })],
                    );
                    let to_read = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    view
                }
                _ => bail!("vulkan-encode: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            Ok((cursor_pc, rgb_view))
        })();
        let (cursor_pc, rgb_view) = match prefix {
            Ok(v) => v,
            Err(e) => {
                // SAFETY-ADJACENT: RECORDING (never submitted in this fn yet), pool allows reset.
                let _ = dev.reset_command_buffer(compute_cmd, vk::CommandBufferResetFlags::empty());
                return Err(e);
            }
        };
        self.bind_rgb(csc_set, rgb_view);

        // y/uv -> GENERAL (shader write); nv12_src -> GENERAL (transfer dst, discard prior)
        let to_general = |img, dst_stage, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(dst_stage)
                .dst_access_mask(dst_access)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        let pre = [
            to_general(
                y_img,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            ),
            to_general(
                uv_img,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            ),
            to_general(
                nv12_src,
                vk::PipelineStageFlags2::ALL_TRANSFER,
                vk::AccessFlags2::TRANSFER_WRITE,
            ),
        ];
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre),
        );

        dev.cmd_bind_pipeline(compute_cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
        dev.cmd_bind_descriptor_sets(
            compute_cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.csc_layout,
            0,
            &[csc_set],
            &[],
        );
        let mut pc_bytes = [0u8; 16];
        for (i, v) in cursor_pc.iter().enumerate() {
            pc_bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
        }
        dev.cmd_push_constants(
            compute_cmd,
            self.csc_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &pc_bytes,
        );
        dev.cmd_dispatch(compute_cmd, (w / 2).div_ceil(8), (h_px / 2).div_ceil(8), 1);

        // y/uv shader-write -> transfer-read (stay GENERAL); then copy into nv12 planes
        let yuv_rd = |img| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[yuv_rd(y_img), yuv_rd(uv_img)]),
        );
        let plane_copy = |src_aspect, dst_aspect, ew, eh| {
            vk::ImageCopy::default()
                .src_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(src_aspect)
                        .layer_count(1),
                )
                .dst_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(dst_aspect)
                        .layer_count(1),
                )
                .extent(vk::Extent3D {
                    width: ew,
                    height: eh,
                    depth: 1,
                })
        };
        dev.cmd_copy_image(
            compute_cmd,
            y_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_0,
                w,
                h_px,
            )],
        );
        dev.cmd_copy_image(
            compute_cmd,
            uv_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_1,
                w / 2,
                h_px / 2,
            )],
        );
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        dev.end_command_buffer(compute_cmd)?;

        // ---- 3. record encode into `cmd`: codec-specific Std authoring + begin/encode/end ----
        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev,
                cmd,
                query_pool,
                bs_buf,
                nv12_src,
                nv12_view,
                SrcAcquire::CscGeneral,
                is_idr,
                recovery,
                ref_slot,
                setup_idx,
                poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev,
                cmd,
                query_pool,
                bs_buf,
                nv12_src,
                nv12_view,
                SrcAcquire::CscGeneral,
                is_idr,
                ref_slot,
                setup_idx,
                poc,
            )?;
        }

        // ---- 4. submit compute (signal csc_sem) then encode (wait csc_sem, signal fence).
        //         Non-blocking: `fence` is polled later so this frame's CSC+encode overlaps the next
        //         capture/submit. Per-slot cmd/sem/fence make ring frames independent; the DPB
        //         barrier above orders slot N's reconstruct-write before N+1's reference-read. ----
        dev.reset_fences(&[fence])?;
        let ccmds = [compute_cmd];
        let sems = [csc_sem];
        dev.queue_submit(
            self.compute_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ccmds)
                .signal_semaphores(&sems)],
            vk::Fence::null(),
        )?;
        let ecmds = [cmd];
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        dev.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ecmds)
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&wait_stages)],
            fence,
        )?;
        self.post_submit_bookkeeping(slot, frame.pts_ns, wire, is_idr, recovery, setup_idx, poc);
        Ok(())
    }

    /// Padded-copy blit (unaligned-mode RGB-direct or native NV12): record — into `compute_cmd`
    /// — the visible frame copy from the imported capture image into the aligned staging image,
    /// plus the edge duplication into the 64x16 padding (the same edge semantics the CSC shader
    /// implements with clamped reads). Transfer-only, no shader. The staging image lives in
    /// GENERAL — it is both copy dst and, for the right-column pass, copy src — and the encode
    /// acquires it via [`SrcAcquire::CscGeneral`] (content produced on the compute queue,
    /// csc_sem-ordered).
    ///
    /// `planes` lists the copy aspects with their subsampling divisor — `[(COLOR, 1)]` for
    /// packed RGB, `[(PLANE_0, 1), (PLANE_1, 2)]` for NV12 (multi-planar copy regions are in
    /// each plane's own coordinate space; barriers on non-disjoint images stay COLOR-aspect).
    /// Every divisor must divide the visible and aligned extents (4:2:0 frames are even, the
    /// coded extent is 64x16-aligned).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_pad_blit(
        &self,
        dev: &ash::Device,
        compute_cmd: vk::CommandBuffer,
        src: vk::Image,
        src_fresh: bool,
        pad: vk::Image,
        ts_pool: vk::QueryPool,
        planes: &[(vk::ImageAspectFlags, u32)],
    ) -> Result<()> {
        let (rw, rh) = (self.render_w, self.render_h);
        let (w, h) = (self.width, self.height);
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        if self.ts_period_ns > 0.0 {
            dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
            dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
        }
        // Acquire the imported capture buffer for transfer reads (FOREIGN hand-off on first
        // import — UNDEFINED preserves the modifier-tiled bytes — visibility-only afterwards),
        // and the staging image for transfer writes (prior contents discarded).
        let src_acq = if src_fresh {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(self.foreign_qfi)
                .dst_queue_family_index(self.compute_family)
        } else {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        }
        .image(src)
        .subresource_range(color_range(0));
        let pad_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(pad)
            .subresource_range(color_range(0));
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[src_acq, pad_dst]),
        );
        let region =
            |aspect: vk::ImageAspectFlags, sx: i32, sy: i32, dx: i32, dy: i32, cw: u32, ch: u32| {
                let layers = vk::ImageSubresourceLayers::default()
                    .aspect_mask(aspect)
                    .layer_count(1);
                vk::ImageCopy::default()
                    .src_subresource(layers)
                    .dst_subresource(layers)
                    .src_offset(vk::Offset3D { x: sx, y: sy, z: 0 })
                    .dst_offset(vk::Offset3D { x: dx, y: dy, z: 0 })
                    .extent(vk::Extent3D {
                        width: cw,
                        height: ch,
                        depth: 1,
                    })
            };
        // Pass 1 — from the capture, per plane: the visible region, then each bottom padding row
        // as a copy of the last visible row (1080p: 8 luma + 4 chroma rows). One call, disjoint
        // regions.
        let mut regions = Vec::new();
        for &(aspect, div) in planes {
            let (rw, rh, h) = (rw / div, rh / div, h / div);
            regions.push(region(aspect, 0, 0, 0, 0, rw, rh));
            for y in rh..h {
                regions.push(region(aspect, 0, rh as i32 - 1, 0, y as i32, rw, 1));
            }
        }
        dev.cmd_copy_image(
            compute_cmd,
            src,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            pad,
            vk::ImageLayout::GENERAL,
            &regions,
        );
        // Pass 2 — right padding columns (width alignment, e.g. 1366→1408): duplicate the
        // staging image's own last visible column over the FULL aligned height (valid after
        // pass 1 filled the bottom rows). Self-copy in GENERAL, W→R barrier between, regions
        // disjoint from their source column.
        if w > rw {
            let self_dep = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(pad)
                .subresource_range(color_range(0));
            dev.cmd_pipeline_barrier2(
                compute_cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[self_dep]),
            );
            let mut cols = Vec::new();
            for &(aspect, div) in planes {
                let (rw, w, h) = (rw / div, w / div, h / div);
                for x in rw..w {
                    cols.push(region(aspect, rw as i32 - 1, 0, x as i32, 0, 1, h));
                }
            }
            dev.cmd_copy_image(
                compute_cmd,
                pad,
                vk::ImageLayout::GENERAL,
                pad,
                vk::ImageLayout::GENERAL,
                &cols,
            );
        }
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        dev.end_command_buffer(compute_cmd)?;
        Ok(())
    }

    /// Producer-native NV12 submit: import the producer's visible-size buffer directly as the
    /// encode source. Safe at every mode because native sessions run true-size headers — the
    /// picture resources' codedExtent equals the source extent and the VCN firmware edge-extends
    /// the alignment padding internally (see the [`Self::native_nv12`] field docs).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_submit_nv12(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        if frame.format != PixelFormat::Nv12 {
            bail!(
                "vulkan-encode (native NV12): negotiated NV12 but received {:?}",
                frame.format
            );
        }
        if frame.width != self.render_w || frame.height != self.render_h {
            bail!(
                "vulkan-encode (native NV12): frame {}x{} != mode {}x{}",
                frame.width,
                frame.height,
                self.render_w,
                self.render_h
            );
        }
        if frame.width % 2 != 0 || frame.height % 2 != 0 {
            bail!("vulkan-encode (native NV12): 4:2:0 frame dimensions must be even");
        }
        let FramePayload::Dmabuf(d) = &frame.payload else {
            bail!("vulkan-encode (native NV12): producer frame is not a DMA-BUF");
        };
        if d.fourcc != pf_frame::drm_fourcc(PixelFormat::Nv12).expect("NV12 FourCC") {
            bail!(
                "vulkan-encode (native NV12): DMA-BUF FourCC {:#x} is not NV12",
                d.fourcc
            );
        }
        if d.modifier != 0 {
            bail!(
                "vulkan-encode (native NV12): only LINEAR is supported, got modifier {:#x}",
                d.modifier
            );
        }
        // No compositing stage exists here (like RGB-direct/EFC) — and none is needed: the
        // session plan negotiates native NV12 only for a non-cursor-blend session
        // (`SessionPlan::output_format` gates `nv12_native` on `!cursor_blend`), so no cursor
        // bitmap ever reaches this arm. Gamescope embeds its pointer in the produced pixels.
        let dev = self.device.clone();
        let cmd = self.frames[slot].cmd;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs_buf;
        // The frame-size check above proved the buffer covers the (true-size) coded extent —
        // the direct import is the encode source at every mode.
        let (src_img, src_view, fresh) = self.import_cached(d, frame.width, frame.height)?;
        let acquire = if fresh {
            SrcAcquire::DmabufFresh
        } else {
            SrcAcquire::DmabufCached
        };
        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, recovery,
                ref_slot, setup_idx, poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, ref_slot,
                setup_idx, poc,
            )?;
        }
        dev.reset_fences(&[fence])?;
        // The whole frame is one submit: the encoder reads the imported NV12 directly.
        let ecmds = [cmd];
        dev.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default().command_buffers(&ecmds)],
            fence,
        )?;
        Ok(())
    }

    /// RGB-direct twin of [`record_submit`]'s steps 2–4 (step 1 and the bookkeeping tail are
    /// shared): resolve the RGB encode source — the imported capture dmabuf itself, or the CPU
    /// staging upload — record the encode, and submit. There is no compute CSC: the VCN EFC
    /// converts inline during the encode; only the CPU path touches the compute queue (for the
    /// staging copy, semaphore-ordered ahead of the encode).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_submit_rgb(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        let dev = self.device.clone();
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let ts_pool = self.frames[slot].ts_pool;
        // EFC cannot composite a cursor bitmap — and never has to: `open` refuses the RGB-direct
        // shape for a cursor-blend session (the pin override above), so no cursor bitmap ever
        // reaches this arm. Gamescope, the flagship, embeds its pointer in the produced pixels.
        let padded = self.rgb.as_ref().is_some_and(|r| r.padded);
        // Only the padded Dmabuf arm below records timestamps (via `record_pad_blit`); the
        // CPU-upload arm records its own command buffer and writes none. Default to "not written"
        // so `read_slot` can never read an unreset query — see `Frame::ts_written`.
        self.frames[slot].ts_written = false;
        let (src_img, src_view, acquire, compute_active) = match &frame.payload {
            FramePayload::Dmabuf(d) if !padded => {
                // Defense in depth for the OOB class the alignment gate closes at open: the
                // imported buffer must cover the source extent the encode declares — the FULL
                // aligned coded extent normally (or the EFC reads past it: VM faults → VCN
                // ring hang → GPU reset, the 2026-07-20 field report), the render size in
                // true-extent mode (where the declared source codedExtent shrinks with it and
                // RADV's firmware padding covers the alignment rows). A mismatched frame
                // (mid-flight mode change, odd capture) errors out here and takes the
                // encoder-rebuild path instead of faulting the GPU.
                let (need_w, need_h) = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
                    (self.render_w, self.render_h)
                } else {
                    (self.width, self.height)
                };
                if frame.width != need_w || frame.height != need_h {
                    bail!(
                        "vulkan-encode (rgb-direct): frame {}x{} does not cover the declared \
                         source extent {}x{} — refusing an out-of-bounds encode source",
                        frame.width,
                        frame.height,
                        need_w,
                        need_h
                    );
                }
                let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let acq = if fresh {
                    SrcAcquire::DmabufFresh
                } else {
                    SrcAcquire::DmabufCached
                };
                (img, view, acq, false)
            }
            FramePayload::Dmabuf(d) => {
                // Padded-copy mode (unaligned mode, e.g. 1080p): blit the visible frame into
                // the per-slot ALIGNED staging image and duplicate the edge into the 64x16
                // padding, all on the transfer path of the compute queue (no shader). The
                // encode reads the staging image — never the capture buffer — so the EFC can
                // never read past a producer allocation again.
                if frame.width != self.render_w || frame.height != self.render_h {
                    bail!(
                        "vulkan-encode (rgb-direct/padded): frame {}x{} != mode {}x{} — \
                         refusing a mismatched blit source",
                        frame.width,
                        frame.height,
                        self.render_w,
                        self.render_h
                    );
                }
                let (img, _view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let pad_img = self.frames[slot].pad_img;
                let pad_view = self.frames[slot].pad_view;
                self.record_pad_blit(
                    &dev,
                    compute_cmd,
                    img,
                    fresh,
                    pad_img,
                    ts_pool,
                    &[(vk::ImageAspectFlags::COLOR, 1)],
                )?;
                // `record_pad_blit` reset and wrote both timestamps when PERF is armed.
                self.frames[slot].ts_written = self.ts_period_ns > 0.0;
                // The staging image ends the blit in GENERAL with the csc_sem ordering the
                // hand-off — exactly the CscGeneral acquire contract.
                (pad_img, pad_view, SrcAcquire::CscGeneral, true)
            }
            FramePayload::Cpu(bytes) => {
                // 24-bpp Rgb/Bgr expands 3→4 BEFORE the staging/padding math (which is all
                // 4-bpp) — see `normalize_cpu_rgb`. BGRA-forced: this image is the ENCODE
                // source, and the session's `pictureFormat` is B8G8R8A8, so an R-first source
                // violates VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207 (caught on RADV by
                // `vulkan_smoke_rgb_cpu24`; true for plain `Rgbx` CPU sources before the 24-bpp
                // work too). This arm's `begin_command_buffer` comes AFTER the fallible steps,
                // so no reset-on-error wrap is needed here.
                let mut scratch = std::mem::take(&mut self.cpu_expand);
                let (norm_fmt, norm_bytes) =
                    normalize_cpu_rgb(frame.format, bytes, &mut scratch, true);
                let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                let view = match fmt {
                    Ok(f) => self.ensure_cpu_rgb(slot, f, norm_bytes, frame.width, frame.height),
                    Err(e) => Err(e),
                };
                self.cpu_expand = scratch;
                let view = view?;
                let (img, ..) = self.frames[slot].cpu_img.expect("ensure_cpu_rgb built it");
                let (stage, ..) = self.frames[slot]
                    .cpu_stage
                    .expect("ensure_cpu_rgb built it");
                // compute_cmd carries ONLY the staging→image copy; the encode submit waits on
                // csc_sem exactly like the CSC path's hand-off.
                dev.begin_command_buffer(
                    compute_cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let to_dst = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(color_range(0));
                dev.cmd_pipeline_barrier2(
                    compute_cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                );
                dev.cmd_copy_buffer_to_image(
                    compute_cmd,
                    stage,
                    img,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: self.width,
                            height: self.height,
                            depth: 1,
                        })],
                );
                dev.end_command_buffer(compute_cmd)?;
                (img, view, SrcAcquire::CpuUpload, true)
            }
            _ => bail!(
                "vulkan-encode (rgb-direct): unsupported FramePayload (need Dmabuf or Cpu RGB)"
            ),
        };
        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, recovery,
                ref_slot, setup_idx, poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, ref_slot,
                setup_idx, poc,
            )?;
        }
        dev.reset_fences(&[fence])?;
        let ecmds = [cmd];
        if compute_active {
            let ccmds = [compute_cmd];
            let sems = [csc_sem];
            dev.queue_submit(
                self.compute_queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&ccmds)
                    .signal_semaphores(&sems)],
                vk::Fence::null(),
            )?;
            let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            dev.queue_submit(
                self.encode_queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&ecmds)
                    .wait_semaphores(&sems)
                    .wait_dst_stage_mask(&wait_stages)],
                fence,
            )?;
        } else {
            // The whole frame is one submit: EFC reads the imported RGB directly.
            dev.queue_submit(
                self.encode_queue,
                &[vk::SubmitInfo::default().command_buffers(&ecmds)],
                fence,
            )?;
        }
        Ok(())
    }

    /// Shared tail of both submit paths: stash the metadata `read_slot` needs once the fence
    /// signals, then advance the DPB/GOP bookkeeping (in submission order).
    fn post_submit_bookkeeping(
        &mut self,
        slot: usize,
        pts_ns: u64,
        wire: i64,
        is_idr: bool,
        recovery: bool,
        setup_idx: usize,
        poc: i32,
    ) {
        self.frames[slot].pts_ns = pts_ns;
        self.frames[slot].keyframe = is_idr;
        self.frames[slot].recovery_anchor = recovery;
        if is_idr {
            self.slot_wire.iter_mut().for_each(|s| *s = -1);
            self.slot_poc.iter_mut().for_each(|s| *s = -1);
        }
        self.slot_wire[setup_idx] = wire;
        self.slot_poc[setup_idx] = poc;
        self.prev_slot = setup_idx;
        self.poc = poc + 1;
        self.enc_count += 1;
        // The record fns key the RESET-install fold on `first_frame`, NOT on `is_idr` — a
        // forced-keyframe or loss-forced IDR mid-stream installs a pending retarget via the
        // standalone ENCODE_RATE_CONTROL like any other mid-stream frame. Capture the flag
        // before clearing it so the telemetry below reports which install path actually ran.
        let was_first_frame = self.first_frame;
        self.first_frame = false;
        // The frame just recorded carried the RESET + rate-control install (what `first_frame`
        // gated), so from here on the session HAS a non-default rate-control mode — and unlike
        // `first_frame` this survives `reset()`, because a reset does not rebuild the session.
        self.rc_installed = true;
        self.force_kf = false;
        if let Some(nb) = self.pending_bitrate.take() {
            // The retarget control command is recorded (execution follows submission order): the
            // session's RC state IS the new rate from this frame on — later begins declare it.
            self.bitrate = nb;
            tracing::debug!(
                mbps = nb / 1_000_000,
                folded_into_reset = was_first_frame,
                "vulkan-encode: rate control retargeted"
            );
        }
    }

    /// Begin `cmd` and record the pre-encode setup shared by both codecs: the query-pool reset,
    /// the source-image acquire (mode-specific — see [`SrcAcquire`]), and the DPB transition —
    /// on the first frame a whole-image UNDEFINED → DPB init; afterwards the
    /// cross-command-buffer pipelining barrier that orders the previous frame's
    /// reconstruct-write before this frame's reference read/write (the in-flight ring records
    /// frame N+1 while N still encodes; the barrier's first scope covers all prior-submitted
    /// encode work on this queue, spanning the separate command buffers).
    unsafe fn begin_encode_cmd(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        src_img: vk::Image,
        acquire: SrcAcquire,
    ) -> Result<()> {
        dev.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        dev.cmd_reset_query_pool(cmd, query_pool, 0, 1);
        let dpb_barrier = if self.first_frame {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        } else {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(
                    vk::AccessFlags2::VIDEO_ENCODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                )
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        }
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(self.dpb_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: DPB_SLOTS,
        });
        // Source acquire, mode-specific. All variants end at VIDEO_ENCODE_SRC for the encode
        // read; they differ in where the content came from (see [`SrcAcquire`]).
        let src_base = vk::ImageMemoryBarrier2::default()
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_img)
            .subresource_range(color_range(0));
        let src_barrier = match acquire {
            SrcAcquire::CscGeneral => src_base
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::GENERAL),
            SrcAcquire::DmabufFresh => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .src_queue_family_index(self.foreign_qfi)
                .dst_queue_family_index(self.encode_family),
            SrcAcquire::DmabufCached => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR),
            SrcAcquire::CpuUpload => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL),
        };
        let pre_enc = [src_barrier, dpb_barrier];
        dev.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre_enc),
        );
        Ok(())
    }

    /// Author the HEVC Std structs + record begin/encode/end for one frame into `cmd` — the HEVC
    /// twin of [`record_coding_av1`]. RFI lever: a recovery anchor is an ordinary P whose
    /// `RefPicList0` names the known-good slot; what makes it decodable is the FULL short-term RPS
    /// ([`build_h265_rps_s0`]) every P-frame carries, which keeps all resident DPB pictures alive
    /// at the decoder so any slot the anchor references is still there.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_h265(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        use ash::vk::native as h;
        // Setup/reference extent: the aligned size for app-aligned sessions (CSC, RGB — it
        // pairs with their aligned SPS), the render size for native NV12's true-size headers.
        let ext2d = if self.native_nv12 {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            vk::Extent2D {
                width: self.width,
                height: self.height,
            }
        };
        // Source extent additionally drops to the render size in RGB true-extent mode: RADV
        // derives the VCN firmware padding from srcPictureResource.codedExtent (Mesa ≥ 24.2),
        // so the visible-size import is never read past its extent (see RgbDirect::true_extent).
        let src_extent = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            ext2d
        };
        let ref_poc = if is_idr { 0 } else { self.slot_poc[ref_slot] };

        let mut pic_flags: h::StdVideoEncodeH265PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_is_reference(1);
        if is_idr {
            pic_flags.set_IrapPicFlag(1);
        }
        pic_flags.set_pic_output_flag(1);
        let mut std_pic: h::StdVideoEncodeH265PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.pic_type = if is_idr {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        std_pic.PicOrderCntVal = poc;
        let (num_neg, deltas, used) = build_h265_rps_s0(&self.slot_poc, setup_idx, ref_poc, poc);
        // A P-frame's active reference must be one of the retained pictures — `ref_slot` is always
        // resident and never the setup slot (record_submit bumps a collision), so a miss here means
        // the DPB bookkeeping desynced.
        debug_assert!(is_idr || used != 0, "reference POC missing from the RPS");
        let mut rps: h::StdVideoH265ShortTermRefPicSet = std::mem::zeroed();
        rps.num_negative_pics = num_neg;
        rps.delta_poc_s0_minus1 = deltas;
        rps.used_by_curr_pic_s0_flag = used;
        let mut ref_lists: h::StdVideoEncodeH265ReferenceListsInfo = std::mem::zeroed();
        ref_lists.RefPicList0 = [0xff; 15];
        ref_lists.RefPicList1 = [0xff; 15];
        ref_lists.RefPicList0[0] = ref_slot as u8;
        if !is_idr {
            std_pic.pShortTermRefPicSet = &rps;
            std_pic.pRefLists = &ref_lists;
        }
        let mut sh_flags: h::StdVideoEncodeH265SliceSegmentHeaderFlags = std::mem::zeroed();
        sh_flags.set_first_slice_segment_in_pic_flag(1);
        sh_flags.set_slice_loop_filter_across_slices_enabled_flag(1);
        let mut std_sh: h::StdVideoEncodeH265SliceSegmentHeader = std::mem::zeroed();
        std_sh.flags = sh_flags;
        std_sh.slice_type = if is_idr {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
        } else {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
        };
        std_sh.MaxNumMergeCand = 5;
        let slice = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
            .constant_qp(0)
            .std_slice_segment_header(&std_sh);
        let slices = [slice];
        let mut h265_pic = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(&slices)
            .std_picture_info(&std_pic);

        // setup slot (reconstruct into) + reference slot (read from)
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        setup_std.pic_type = std_pic.pic_type;
        setup_std.PicOrderCntVal = poc;
        let mut setup_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let mut setup_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_a);
        let begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_b);

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        ref_std.pic_type = if ref_poc == 0 {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        ref_std.PicOrderCntVal = ref_poc;
        let mut ref_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let mut ref_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_a);
        let ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_b);
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        // Rate control (chained manually; push_next would clobber rc.p_next). Mode + window are
        // the values latched at open (see the `rc_mode`/`vbv_ms` field docs); this struct DECLARES
        // the session's CURRENT state (`self.bitrate`) — never a pending retarget, which is only
        // what the install below moves to (VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08254).
        let rc_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate)
            .max_bitrate(self.bitrate)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1)];
        let h265_rc = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP)
            .gop_frame_count(u32::MAX)
            .idr_period(u32::MAX)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(self.rc_mode)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(self.vbv_ms.0)
            .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
        rc.p_next = &h265_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        self.begin_encode_cmd(dev, cmd, query_pool, src_img, acquire)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        // Declare the session's ACTUAL current rate-control state, not "not the first frame" —
        // a mid-stream `reset()` re-arms `first_frame` while the session keeps its installed CBR.
        if self.rc_installed {
            begin.p_next = rc_ptr;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + rate-control install + explicit quality level. Without ENCODE_QUALITY_LEVEL,
            // RADV never sends the VCN a preset op at all (the firmware default preset runs);
            // installing it makes the preset deterministic on every driver and selects the
            // fastest tier by default (see `quality_request`). The quality struct chains ahead
            // of the rate-control state — the spec requires a quality-level change to carry
            // ENCODE_RATE_CONTROL, which the RESET install provides anyway.
            //
            // The install moves to the EFFECTIVE rate — a pending retarget folds in HERE, not
            // into `self.bitrate` before recording. After a mid-stream `reset()` the session
            // still HAS the old rate (a reset never touches the session object), and the begin
            // above must declare that old rate; promoting the pending rate first made the
            // declaration name a rate the session had not installed — a one-frame
            // VUID-...-08254 violation on exactly the frames where ABR retarget and the stall
            // watchdog coincide. `post_submit_bookkeeping` promotes after recording.
            let nb = self.pending_bitrate.unwrap_or(self.bitrate);
            let install_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut install_rc = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&install_layer)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            install_rc.p_next = &h265_rc as *const _ as *const c_void;
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = &install_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
            );
            ctrl.p_next = &q as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        } else if let Some(nb) = self.pending_bitrate {
            // Mid-stream retarget (`reconfigure_bitrate`): `begin` above declared the session's
            // CURRENT rate-control state (the spec requires the match); this control command
            // installs the NEW rate — the same shape with only the bitrate moved. No RESET,
            // no IDR: the DPB and reference chain carry straight on. `record_submit` promotes
            // `nb` into `self.bitrate` after recording, so later begins declare the new state.
            let rc_layer2 = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut rc2 = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            rc2.p_next = &h265_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(src_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(&setup_slot)
            .push_next(&mut h265_pic);
        if !is_idr {
            enc = enc.reference_slots(&enc_refs);
        }
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        dev.end_command_buffer(cmd)?;
        Ok(())
    }

    /// Author the AV1 Std structs + record begin/encode/end for one frame into `cmd` — the AV1
    /// twin of [`record_coding_h265`]. RFI lever: an IDR **or** a recovery frame breaks the CDF
    /// chain (`primary_ref_frame = PRIMARY_REF_NONE` + `error_resilient_mode`) so it decodes
    /// independent of the lost frames' probability context, while a normal P inherits context
    /// (name 0 → `ref_slot`). Unlike HEVC, reference retention needs no per-frame syntax: AV1's 8
    /// virtual reference slots persist until `refresh_frame_flags` overwrites them, mirroring the
    /// host's DPB ring by construction.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_av1(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        order: i32,
    ) -> Result<()> {
        use super::vk_av1_encode as av1;
        use ash::vk::native as h;
        // Setup/reference extent: the aligned size for app-aligned sessions (CSC, RGB — it
        // pairs with their aligned SPS), the render size for native NV12's true-size headers.
        let ext2d = if self.native_nv12 {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            vk::Extent2D {
                width: self.width,
                height: self.height,
            }
        };
        // Source extent additionally drops to the render size in RGB true-extent mode: RADV
        // derives the VCN firmware padding from srcPictureResource.codedExtent (Mesa ≥ 24.2),
        // so the visible-size import is never read past its extent (see RgbDirect::true_extent).
        let src_extent = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            ext2d
        };

        // ---- required AV1 frame sub-structs (single tile; no CDEF/LR/segmentation/global-motion) ----
        let mut tile_flags: h::StdVideoAV1TileInfoFlags = std::mem::zeroed();
        tile_flags.set_uniform_tile_spacing_flag(1);
        let mut tile_info: h::StdVideoAV1TileInfo = std::mem::zeroed();
        tile_info.flags = tile_flags;
        tile_info.TileCols = 1;
        tile_info.TileRows = 1;

        let mut quant: h::StdVideoAV1Quantization = std::mem::zeroed();
        quant.base_q_idx = AV1_BASE_Q_IDX;

        let mut loop_filter: h::StdVideoAV1LoopFilter = std::mem::zeroed();
        // AV1 default_loop_filter_ref_deltas (spec 7.14.1): intra +1, golden/bwd/altref2/altref -1.
        loop_filter.loop_filter_ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];

        let cdef: h::StdVideoAV1CDEF = std::mem::zeroed();

        let mut lr: h::StdVideoAV1LoopRestoration = std::mem::zeroed();
        lr.FrameRestorationType =
            [h::StdVideoAV1FrameRestorationType_STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE; 3];

        let gm: h::StdVideoAV1GlobalMotion = std::mem::zeroed();

        // Order hints of the 8 physical reference buffers (DPB slots), 0 where empty.
        let mut ref_order_hint = [0u8; 8];
        for (i, &poc) in self.slot_poc.iter().enumerate().take(8) {
            ref_order_hint[i] = poc.max(0) as u8;
        }

        // ---- Std picture info ----
        // A recovery anchor (or IDR) is error-resilient + inherits no CDF context, so it decodes
        // independent of the (possibly lost) frames since its reference — the AV1 RFI lever. Normal
        // P-frames inherit context from their reference (primary_ref = name 0 → `ref_slot`) for
        // compression, exactly like the HEVC path's reference chain.
        let independent = is_idr || recovery;
        let mut pic_flags: av1::StdVideoEncodeAV1PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_show_frame(1);
        if independent {
            pic_flags.set_error_resilient_mode(1);
        }
        // AV1 IGNORES render_width/height_minus_1 unless this flag says the render size differs
        // from the coded frame size. We always wrote the render size (below) but never the flag, so
        // at any mode that needed 64x16 alignment the decoder fell back to the CODED size and the
        // client displayed our alignment padding — e.g. 1080p arriving as 1088 rows, the bottom 8
        // being duplicated edge pixels.
        if self.render_w != src_extent.width || self.render_h != src_extent.height {
            pic_flags.set_render_and_frame_size_different(1);
        }
        let mut std_pic: av1::StdVideoEncodeAV1PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.frame_type = if is_idr {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        std_pic.order_hint = order as u8;
        std_pic.primary_ref_frame = if independent {
            av1::PRIMARY_REF_NONE
        } else {
            0
        };
        std_pic.refresh_frame_flags = if is_idr { 0xff } else { 1u8 << setup_idx };
        std_pic.render_width_minus_1 = (self.render_w - 1) as u16;
        std_pic.render_height_minus_1 = (self.render_h - 1) as u16;
        std_pic.interpolation_filter = 0; // EIGHTTAP
        std_pic.TxMode = h::StdVideoAV1TxMode_STD_VIDEO_AV1_TX_MODE_SELECT;
        std_pic.ref_order_hint = ref_order_hint;
        if !is_idr {
            // single-reference P: every reference name maps to the (recovery or previous) DPB slot.
            std_pic.ref_frame_idx = [ref_slot as i8; 7];
        }
        std_pic.pTileInfo = &tile_info;
        std_pic.pQuantization = &quant;
        std_pic.pLoopFilter = &loop_filter;
        std_pic.pCDEF = &cdef;
        std_pic.pLoopRestoration = &lr;
        // pSegmentation MUST be NULL for an AV1 encode operation
        // (VUID-vkCmdEncodeVideoKHR-pStdPictureInfo-10350). It used to point at a zeroed
        // segmentation struct, which RADV's validation rejects on every frame; `std_pic` is
        // zeroed, so leaving it unset is both spec-correct and the same "segmentation disabled".
        std_pic.pGlobalMotion = &gm;

        // ---- KHR picture info ----
        let av1_pic = av1::VideoEncodeAV1PictureInfoKHR {
            s_type: av1::stype(av1::ST_PICTURE_INFO),
            p_next: std::ptr::null(),
            prediction_mode: if is_idr {
                av1::PREDICTION_MODE_INTRA_ONLY
            } else {
                av1::PREDICTION_MODE_SINGLE_REFERENCE
            },
            rate_control_group: if is_idr {
                av1::RC_GROUP_INTRA
            } else {
                av1::RC_GROUP_PREDICTIVE
            },
            // MUST be zero whenever the rate control mode is not DISABLED
            // (VUID-vkCmdEncodeVideoKHR-constantQIndex-10320) — this session installs CBR, so the
            // driver owns Q. Passing `base_q_idx` here was rejected by validation on every frame;
            // the value still reaches the encoder through `pQuantization` for the header.
            constant_q_index: 0,
            p_std_picture_info: &std_pic,
            reference_name_slot_indices: if is_idr {
                [-1; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            } else {
                [ref_slot as i32; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            },
            primary_reference_cdf_only: 0,
            generate_obu_extension_header: 0,
        };

        // ---- setup (reconstruct into) + reference (read from) DPB slots ----
        // DPB slots carry the SOURCE extent, not the aligned one. Without
        // MOTION_VECTOR_SCALING every reference slot's `codedExtent` must equal the source's
        // (`VUID-vkCmdEncodeVideoKHR-flags-10325`), and in true-extent mode the source is the
        // render size. `src_extent` already collapses to `ext2d` whenever true-extent is off, so
        // this is a no-op for every other configuration.
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        setup_ref_std.frame_type = std_pic.frame_type;
        setup_ref_std.OrderHint = order as u8;
        let setup_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &setup_ref_std,
        };
        let mut setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res);
        setup_slot.p_next = &setup_dpb as *const _ as *const c_void;
        let mut begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res);
        begin_setup.p_next = &setup_dpb as *const _ as *const c_void;

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        ref_ref_std.frame_type = if self.slot_poc[ref_slot] == 0 {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        ref_ref_std.OrderHint = self.slot_poc[ref_slot].max(0) as u8;
        let ref_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &ref_ref_std,
        };
        let mut ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_begin.p_next = &ref_dpb as *const _ as *const c_void;
        let mut ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_enc.p_next = &ref_dpb as *const _ as *const c_void;
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        // ---- rate control (generic layer + AV1 codec info chained manually) ----
        // Mode + window are the values latched at open; this struct DECLARES the session's
        // CURRENT state (`self.bitrate`) — see the HEVC twin for the 08254 state discipline.
        let rc_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate)
            .max_bitrate(self.bitrate)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1)];
        let av1_rc = av1::VideoEncodeAV1RateControlInfoKHR {
            s_type: av1::stype(av1::ST_RATE_CONTROL_INFO),
            p_next: std::ptr::null(),
            flags: 0,
            gop_frame_count: 0,
            key_frame_period: 0,
            consecutive_bipredictive_frame_count: 0,
            temporal_layer_count: 1,
        };
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(self.rc_mode)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(self.vbv_ms.0)
            .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
        rc.p_next = &av1_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        // ---- record cmd: begin + shared pre-encode barriers, then begin/encode/end coding ----
        self.begin_encode_cmd(dev, cmd, query_pool, src_img, acquire)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        // See the h265 twin: declare what the session actually has, not `!first_frame`.
        if self.rc_installed {
            begin.p_next = rc_ptr;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + rate-control install + explicit quality level — see the HEVC twin for both
            // disciplines: why ENCODE_QUALITY_LEVEL is explicit, and why the install (not the
            // declaration) is what a pending retarget folds into (VUID-...-08254).
            let nb = self.pending_bitrate.unwrap_or(self.bitrate);
            let install_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut install_rc = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&install_layer)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            install_rc.p_next = &av1_rc as *const _ as *const c_void;
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = &install_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
            );
            ctrl.p_next = &q as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        } else if let Some(nb) = self.pending_bitrate {
            // Mid-stream retarget (`reconfigure_bitrate`) — see the HEVC twin for the state
            // discipline (begin declares CURRENT, this control installs NEW, `record_submit`
            // promotes after recording). No RESET, no IDR.
            let rc_layer2 = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut rc2 = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            rc2.p_next = &av1_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(src_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(&setup_slot);
        if !is_idr {
            enc = enc.reference_slots(&enc_refs);
        }
        enc.p_next = &av1_pic as *const _ as *const c_void;
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        dev.end_command_buffer(cmd)?;
        Ok(())
    }

    /// Read one completed slot's bitstream into an `EncodedFrame`, prepending the header framing:
    /// HEVC keyframes carry VPS/SPS/PPS; AV1 opens every temporal unit with a TD OBU and prepends the
    /// sequence-header OBU on keyframes. Caller must have confirmed the slot's fence is signaled.
    unsafe fn read_slot(&mut self, slot: usize) -> Result<EncodedFrame> {
        let dev = self.device.clone();
        let f = &self.frames[slot];
        // Ask for the operation status alongside the two feedback words: without it a FAILED encode
        // is indistinguishable from a successful one, and its offset/bytes-written are read as if
        // they described real bitstream. The status rides as a trailing element (signed:
        // `VkQueryResultStatusKHR` is >0 COMPLETE, 0 NOT_READY, <0 error).
        let mut fb = [[0i32; 3]; 1];
        dev.get_query_pool_results(
            f.query_pool,
            0,
            &mut fb,
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR,
        )?;
        let status = fb[0][2];
        if status <= 0 {
            anyhow::bail!(
                "vulkan-encode: encode feedback for slot {slot} reports status {status} \
                 (not COMPLETE) — dropping the frame rather than shipping its bitstream"
            );
        }
        let fb = [[fb[0][0] as u32, fb[0][1] as u32]];
        // The (offset, bytes-written) pair is driver-reported: validate it against the bitstream
        // allocation BEFORE mapping, or the `from_raw_parts` below reads outside the buffer and
        // ships whatever it finds straight onto the wire. Checked in u64 so the add cannot wrap,
        // and before `map_memory` so there is no unmap to unwind on the error path.
        let (off64, len64) = (fb[0][0] as u64, fb[0][1] as u64);
        if off64.saturating_add(len64) > self.bs_size {
            anyhow::bail!(
                "vulkan-encode: driver reported bitstream feedback offset={off64} \
                 bytes_written={len64}, outside the {} byte bitstream buffer — the encode likely \
                 overflowed its destination range",
                self.bs_size
            );
        }
        let (off, len) = (off64 as usize, len64 as usize);
        // PUNKTFUNK_PERF pre-encode split (best-effort): the fence signaled, so the compute/transfer
        // timestamps are available. This is a device duration, not permission to label the
        // remaining host fence wait as pure VCN time; queueing and synchronization remain in it.
        if self.ts_period_ns > 0.0
            && f.ts_pool != vk::QueryPool::null()
            && f.ts_written
            && self.perf_at.elapsed() >= std::time::Duration::from_secs(2)
        {
            let mut ts = [0u64; 2];
            if dev
                .get_query_pool_results(
                    f.ts_pool,
                    0,
                    &mut ts,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                )
                .is_ok()
            {
                self.perf_at = std::time::Instant::now();
                let pre_encode_us =
                    (ts[1].saturating_sub(ts[0]) as f64 * self.ts_period_ns) / 1000.0;
                if self.rgb.as_ref().is_some_and(|r| r.padded) {
                    tracing::info!(
                        rgb_copy_us = format!("{pre_encode_us:.0}"),
                        au_bytes = len,
                        "vulkan-encode split (sampled): padded RGB copy device time before EFC; \
                         remaining fence wait still includes queue synchronization + RGB→YUV EFC \
                         + video encode"
                    );
                } else {
                    tracing::info!(
                        csc_us = format!("{pre_encode_us:.0}"),
                        au_bytes = len,
                        "vulkan-encode split (sampled): RGB→NV12 compute batch device time; \
                         remaining fence wait still includes queue synchronization + video encode"
                    );
                }
            }
        }
        let f = &self.frames[slot];
        let p = f.bs_ptr.0;
        debug_assert!(!p.is_null(), "bs_mem persistent mapping missing");
        let prefix: &[u8] = if f.keyframe {
            &self.header
        } else {
            &self.frame_prefix
        };
        let mut data = Vec::with_capacity(prefix.len() + len);
        data.extend_from_slice(prefix);
        data.extend_from_slice(std::slice::from_raw_parts(p.add(off), len));
        Ok(EncodedFrame {
            data,
            pts_ns: f.pts_ns,
            keyframe: f.keyframe,
            recovery_anchor: f.recovery_anchor,
            chunk_aligned: false,
        })
    }

    /// Acquire a free ring slot (blocking-draining the oldest if the ring is full), record+submit
    /// this frame into it without waiting, and track it as in-flight (FIFO).
    unsafe fn enqueue(&mut self, frame: &CapturedFrame, wire: i64) -> Result<()> {
        // Backpressure: if every slot is outstanding, block on the oldest, read it into `pending`,
        // and free it — that oldest slot is exactly the round-robin `ring` cursor we reuse next.
        while self.in_flight.len() >= self.frames.len() {
            let slot = self.in_flight.pop_front().unwrap();
            // Bounded, not `u64::MAX`: this runs ON the host encode thread, which is also the
            // thread the stall watchdog's `reset()` would run on. An infinite wait against a
            // wedged GPU/driver therefore parks the one thread that could recover the session —
            // it never errors, never resets, and teardown blocks joining it. Surfacing expiry as
            // an error hands control back to the existing recovery path (same convention as the
            // pyrowave and Windows NVENC backends).
            match self.device.wait_for_fences(
                &[self.frames[slot].fence],
                true,
                ENCODE_FENCE_TIMEOUT_NS,
            ) {
                Ok(()) => {}
                Err(vk::Result::TIMEOUT) => anyhow::bail!(
                    "vulkan-encode: fence for slot {slot} did not signal within {} ms — GPU or \
                     driver wedged; failing the submit so the session can reset",
                    ENCODE_FENCE_TIMEOUT_NS / 1_000_000
                ),
                Err(e) => return Err(e.into()),
            }
            let done = self.read_slot(slot)?;
            self.pending.push_back(done);
        }
        let slot = self.ring;
        self.ring = (self.ring + 1) % self.frames.len();
        self.record_submit(slot, frame, wire)?;
        self.in_flight.push_back(slot);
        Ok(())
    }
}

impl Encoder for VulkanVideoEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        let wire = self.auto_wire;
        self.auto_wire += 1;
        // SAFETY: `enqueue` records/submits into a free ring slot owned by this encoder without
        // blocking on GPU completion (poll() does); `&mut self` guarantees exclusive access.
        unsafe { self.enqueue(frame, wire) }
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.auto_wire = wire_index as i64 + 1;
        // SAFETY: see `submit` — exclusive `&mut self`, all Vulkan work confined to owned objects.
        unsafe { self.enqueue(frame, wire_index as i64) }
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            // Only the CSC path composites the metadata cursor (`prep_cursor` feeds the compute
            // shader). The RGB-direct/EFC front-end and the native-NV12 source have no compositing
            // stage at all — both merely warn once that the pointer is being dropped — so this is
            // the encoder's real answer, not a static one.
            blends_cursor: self.rgb.is_none() && !self.native_nv12,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        // Nonsense range → decline (same contract as the NVENC/AMF backends).
        if first_frame < 0 || first_frame > last_frame {
            return false;
        }
        // The taint-sweep + anchor-pick POLICY lives in `rfi::plan_slot_recovery` (one decision
        // shared with AMF and QSV — the fecbec2d sweep reached those two a commit before this
        // backend was carved out, and the hand-copy here shipped without it). Why the sweep:
        // "resident and older than THIS loss" is not the same as "the client decoded it" — after
        // an earlier loss [a,b] was recovered at wire r, everything in [a, r-1] is undecodable at
        // the client, yet those wires stay anchor candidates until the 8-slot ring rolls them
        // out, so a LATER loss could anchor on one and ship corruption tagged `recovery_anchor` —
        // the client's definitive re-anchor signal (reanchor.rs).
        //
        // This backend's mechanism: distrust = blank `slot_wire` ONLY. `slot_poc` must keep
        // naming every physically-resident DPB picture for `build_h265_rps_s0`, or a conforming
        // decoder evicts them and the anchor references a picture the client already dropped.
        // `slot_wire` is the RFI/loss domain; `slot_poc` is the reference-delta domain.
        // `prev_slot` and the normal P-frame path are indices, not wires, so ordinary prediction
        // is unaffected.
        let plan = crate::rfi::plan_slot_recovery(&trusted_refs(&self.slot_wire), first_frame);
        for (s, w) in self.slot_wire.iter_mut().enumerate() {
            if plan.tainted & (1 << s) != 0 {
                *w = -1;
            }
        }
        // Can we anchor a clean P-frame to a resident slot strictly older than the loss?
        // (A sweep that empties every candidate yields `None` here and declines the RFI, matching
        // `qsv_live_ltr_rfi_taint_sweep_declines`.)
        match plan.anchor {
            Some(_) => {
                self.pending_loss = Some(first_frame);
                true
            }
            None => {
                // Decline WITHOUT self-arming an IDR: the caller owns the fallback, and its
                // keyframe path is cooldown-coalesced — arming `force_kf` here would bypass that
                // and turn a storm of hopeless RFI requests into one full IDR per request.
                // `pending_loss` is DELIBERATELY left armed (unlike AMF/QSV's pending_force
                // clear): a stale arm is re-resolved at frame-build, where a failed re-pick
                // forces the IDR that heals the stream — clearing it here would ship an untagged
                // plain P during the caller's RFI-echo window instead.
                tracing::debug!(
                    first_frame,
                    last_frame,
                    "vulkan-encode RFI declined: no resident reference older than the loss — \
                     caller falls back to its (coalesced) keyframe path"
                );
                false
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Backpressure-drained frames (already read, oldest) come out first, then the oldest slot
        // still in flight — both in submission order. BLOCKING, per the depth-1 pump contract
        // (`Capturer::pipeline_depth`: "capture → submit → poll-blocks", the convention the sync
        // NVENC backend's `lock_bitstream` follows): the pump polls right after submit and treats
        // `None` as "the backend holds the frame internally, re-poll next tick" (true only of the
        // libav AMF/QSV wrappers). A `get_fence_status` probe here therefore deferred every AU to
        // the NEXT tick — shipped a full frame period after the ASIC finished, so a ~5 ms VCN
        // encode read as `encode_us`≈interval (~17 ms at 60 Hz, the AMD field report) and cost one
        // frame of avoidable glass-to-glass latency. `None` now only means "nothing submitted".
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let Some(&slot) = self.in_flight.front() else {
            return Ok(None);
        };
        // Bounded like `enqueue`'s backpressure wait, and for the same reason: this is the thread
        // the stall recovery runs on, so a wedged GPU must surface as an error, not park it.
        // SAFETY: waiting a fence owned by this encoder's slot under `&mut self`.
        match unsafe {
            self.device
                .wait_for_fences(&[self.frames[slot].fence], true, ENCODE_FENCE_TIMEOUT_NS)
        } {
            Ok(()) => {}
            Err(vk::Result::TIMEOUT) => anyhow::bail!(
                "vulkan-encode: fence for slot {slot} did not signal within {} ms — GPU or \
                 driver wedged; failing the poll so the session can reset",
                ENCODE_FENCE_TIMEOUT_NS / 1_000_000
            ),
            Err(e) => return Err(e.into()),
        }
        self.in_flight.pop_front();
        // SAFETY: fence signaled ⟹ this slot's CSC+encode is complete; read its bitstream.
        Ok(Some(unsafe { self.read_slot(slot)? }))
    }

    fn reset(&mut self) -> bool {
        // Abandon everything in flight: wait the GPU idle, discard unread slots + queued output, and
        // restart GOP/DPB state so the next frame is a fresh IDR.
        //
        // The wait is BOUNDED, like every other wait on this thread (`ENCODE_FENCE_TIMEOUT_NS`,
        // same reasoning): reset() runs precisely because something upstream looks wedged, and an
        // untimed `device_wait_idle` would park the recovery path on the suspect device until the
        // kernel's GPU reset — if that ever comes. A timeout means the GPU really is wedged:
        // report "no in-place rebuild" so the caller ends the session with a real error instead.
        // (`Drop` still waits unbounded — teardown must be memory-safe even against a wedged
        // device, and by then the session is already over.)
        let fences: Vec<vk::Fence> = self
            .in_flight
            .iter()
            .map(|&s| self.frames[s].fence)
            .collect();
        if !fences.is_empty() {
            // SAFETY: every in-flight slot's fence was submitted with its batch (`enqueue` pushes
            // only after a successful submit), and we hold `&mut self`.
            match unsafe {
                self.device
                    .wait_for_fences(&fences, true, ENCODE_FENCE_TIMEOUT_NS)
            } {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(
                        error = ?e,
                        "vulkan-encode: in-flight work did not go idle within {} ms — GPU or \
                         driver wedged; in-place rebuild abandoned",
                        ENCODE_FENCE_TIMEOUT_NS / 1_000_000
                    );
                    return false;
                }
            }
        }
        // The fences cover the encode queue, and each encode waited its slot's `csc_sem`, so the
        // paired compute batches are done too. The residual — a compute batch whose encode-queue
        // submit failed (so it was never fenced or enqueued) — is what this sweep-up covers; it
        // is instant unless that rare orphan is itself wedged (then the kernel's GPU reset bounds
        // it, as before). An error here is a lost device: no rebuild on top of that.
        // SAFETY: waiting this encoder's own device idle under `&mut self`.
        if unsafe { self.device.device_wait_idle() }.is_err() {
            return false;
        }
        // Drop the dmabuf import cache while the device is provably idle (the only safe point
        // outside teardown): whatever wedged the GPU may be a capture-side reallocation, and the
        // rebuilt stream must re-import from live buffers rather than trust images imported for
        // the old pool.
        // SAFETY: device idle (waits above); each entry is owned by the cache, destroyed once.
        unsafe {
            for e in std::mem::take(&mut self.import_cache) {
                self.device.destroy_image_view(e.view, None);
                self.device.destroy_image(e.img, None);
                self.device.free_memory(e.mem, None);
            }
        }
        self.in_flight.clear();
        self.pending.clear();
        self.ring = 0;
        self.first_frame = true;
        self.force_kf = false;
        self.pending_loss = None;
        self.poc = 0;
        self.slot_wire.iter_mut().for_each(|s| *s = -1);
        self.slot_poc.iter_mut().for_each(|s| *s = -1);
        // A pending `reconfigure_bitrate` rate deliberately survives: the restart's first frame
        // folds it into the fresh RESET + rate-control install.
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // The RC block is re-declared on every recorded frame, so the retarget is just a staged
        // rate: the next `record_submit` emits an ENCODE_RATE_CONTROL control command carrying it
        // — no session churn, no IDR. Same floor as `open` (a 0-rate layer is rejected) and the
        // same driver ceiling (the spec bounds every layer's bitrates by the profile's
        // `maxBitrate`; an ABR climb must not sail past it). Any clamp here is visible to the
        // session loop through `applied_bitrate_bps`, so the controller climbs from the real
        // rate, not the requested one.
        let clamped = bps.max(1_000_000).min(self.hw_max_bitrate);
        if clamped < bps {
            tracing::debug!(
                requested = bps,
                cap = self.hw_max_bitrate,
                "vulkan-encode: retarget clamped to the driver's maxBitrate"
            );
        }
        self.pending_bitrate = Some(clamped);
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // The encoder-side truth after the `hw_max_bitrate` clamp (open + retarget). Without
        // this, a clamped retarget leaves the session loop trusting the REQUESTED rate: the
        // send pacer drains at a phantom rate, the ceiling cache never learns, and the client
        // controller climbs from a base the ASIC never targets (§ABR overdrive — the trait doc
        // names this exact trap). A pending retarget is reported as applied: the very next
        // recorded frame installs it, and callers read this right after `reconfigure_bitrate`.
        Some(self.pending_bitrate.unwrap_or(self.bitrate))
    }

    fn flush(&mut self) -> Result<()> {
        // Drain every outstanding slot in order into `pending` so a following poll-loop returns them.
        while let Some(slot) = self.in_flight.pop_front() {
            // SAFETY: wait this slot's fence, then read back its own owned bitstream objects.
            unsafe {
                self.device
                    .wait_for_fences(&[self.frames[slot].fence], true, u64::MAX)?;
                let done = self.read_slot(slot)?;
                self.pending.push_back(done);
            }
        }
        Ok(())
    }
}

/// One cached dmabuf import (see `import_cached`): keyed by `(st_dev, st_ino)`, carrying the
/// extent it was imported at — a key hit must also prove the cached image still matches the
/// caller's frame — plus the owning Vulkan objects.
struct CachedImport {
    key: (u64, u64),
    extent: (u32, u32),
    img: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
}

/// Every destructible Vulkan object the encoder owns, with the one `Drop` that destroys them in
/// dependency order. Both teardown paths run through it so they cannot drift:
///
/// - `open_inner` mirrors each object into one as it is created, so any early `?`/`bail!` (or
///   panic) unwinds exactly what was built — previously every open failure leaked all prior
///   objects (a `VkDevice` + GPU memory per retried open). The `Ok(Self)` hand-off disarms the
///   guard with `mem::forget` after moving the collections out.
/// - [`VulkanVideoEncoder`]'s `Drop` rebuilds one from its fields and drops it.
///
/// Handles a failed build never reached stay null, and `vkDestroy*`/`vkFree*` are defined no-ops
/// on `VK_NULL_HANDLE`, so the full sequence is safe to run against any prefix of the build.
struct VkTeardown {
    instance: Option<ash::Instance>,
    // `device` and `vq_dev` are set together (the wrapper constructors after `create_device` are
    // infallible), so device-level objects can only exist once both are `Some`.
    device: Option<ash::Device>,
    vq_dev: Option<ash::khr::video_queue::Device>,
    import_cache: Vec<CachedImport>,
    frames: Vec<Frame>,
    compute_pool: vk::CommandPool,
    cmd_pool: vk::CommandPool,
    // Transient: alive only between its creation and the post-pipeline destroy in `open_inner`
    // (which nulls this); always null when rebuilt from the encoder's `Drop`.
    shader: vk::ShaderModule,
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_pool: vk::DescriptorPool,
    csc_dsl: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    dpb_views: Vec<vk::ImageView>,
    dpb_image: vk::Image,
    dpb_mem: vk::DeviceMemory,
    params: vk::VideoSessionParametersKHR,
    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
}

impl VkTeardown {
    /// A fresh guard owning only the instance — every other handle starts null/empty. Written out
    /// field by field because struct-update syntax is not allowed on a `Drop` type (E0509).
    fn new(instance: ash::Instance) -> Self {
        Self {
            instance: Some(instance),
            device: None,
            vq_dev: None,
            import_cache: Vec::new(),
            frames: Vec::new(),
            compute_pool: vk::CommandPool::null(),
            cmd_pool: vk::CommandPool::null(),
            shader: vk::ShaderModule::null(),
            csc_pipe: vk::Pipeline::null(),
            csc_layout: vk::PipelineLayout::null(),
            csc_pool: vk::DescriptorPool::null(),
            csc_dsl: vk::DescriptorSetLayout::null(),
            sampler: vk::Sampler::null(),
            dpb_views: Vec::new(),
            dpb_image: vk::Image::null(),
            dpb_mem: vk::DeviceMemory::null(),
            params: vk::VideoSessionParametersKHR::null(),
            session: vk::VideoSessionKHR::null(),
            session_mem: Vec::new(),
        }
    }
}

impl Drop for VkTeardown {
    fn drop(&mut self) {
        // SAFETY: `device_wait_idle` first guarantees no GPU work still references any object, so
        // every handle destroyed below is idle and owned solely by `self`; each is freed exactly
        // once (the takes prevent a double free) and in dependency order (views before images
        // before memory, per-frame objects before their shared pools, session params before
        // session, session memory after the session, the device before the instance). Null handles
        // (a build prefix from a failed `open_inner`) are no-ops per the Vulkan spec.
        unsafe {
            if let Some(device) = self.device.take() {
                let _ = device.device_wait_idle();
                for e in std::mem::take(&mut self.import_cache) {
                    device.destroy_image_view(e.view, None);
                    device.destroy_image(e.img, None);
                    device.free_memory(e.mem, None);
                }
                // Per-frame ring resources (command buffers, descriptor sets freed with their pools).
                for f in std::mem::take(&mut self.frames) {
                    device.destroy_semaphore(f.csc_sem, None);
                    device.destroy_fence(f.fence, None);
                    device.destroy_query_pool(f.query_pool, None);
                    device.destroy_query_pool(f.ts_pool, None);
                    device.destroy_image_view(f.pad_view, None);
                    device.destroy_image(f.pad_img, None);
                    device.free_memory(f.pad_mem, None);
                    device.destroy_buffer(f.bs_buf, None);
                    // bs_mem is persistently mapped (f.bs_ptr); vkFreeMemory implicitly unmaps.
                    device.free_memory(f.bs_mem, None);
                    for (img, mem, view) in [
                        (f.y_img, f.y_mem, f.y_view),
                        (f.uv_img, f.uv_mem, f.uv_view),
                        (f.nv12_src, f.nv12_mem, f.nv12_view),
                    ] {
                        device.destroy_image_view(view, None);
                        device.destroy_image(img, None);
                        device.free_memory(mem, None);
                    }
                    if let Some((i, m, v, ..)) = f.cpu_img {
                        device.destroy_image_view(v, None);
                        device.destroy_image(i, None);
                        device.free_memory(m, None);
                    }
                    if let Some((b, m, _)) = f.cpu_stage {
                        device.destroy_buffer(b, None);
                        device.free_memory(m, None);
                    }
                    device.destroy_image_view(f.cursor_view, None);
                    device.destroy_image(f.cursor_img, None);
                    device.free_memory(f.cursor_mem, None);
                    device.destroy_buffer(f.cursor_stage, None);
                    device.free_memory(f.cursor_stage_mem, None);
                }
                device.destroy_command_pool(self.compute_pool, None);
                device.destroy_command_pool(self.cmd_pool, None);
                device.destroy_shader_module(self.shader, None);
                device.destroy_pipeline(self.csc_pipe, None);
                device.destroy_pipeline_layout(self.csc_layout, None);
                device.destroy_descriptor_pool(self.csc_pool, None);
                device.destroy_descriptor_set_layout(self.csc_dsl, None);
                device.destroy_sampler(self.sampler, None);
                for &v in &self.dpb_views {
                    device.destroy_image_view(v, None);
                }
                device.destroy_image(self.dpb_image, None);
                device.free_memory(self.dpb_mem, None);
                if let Some(vq_dev) = self.vq_dev.take() {
                    (vq_dev.fp().destroy_video_session_parameters_khr)(
                        device.handle(),
                        self.params,
                        std::ptr::null(),
                    );
                    (vq_dev.fp().destroy_video_session_khr)(
                        device.handle(),
                        self.session,
                        std::ptr::null(),
                    );
                }
                for &m in &self.session_mem {
                    device.free_memory(m, None);
                }
                device.destroy_device(None);
            }
            if let Some(instance) = self.instance.take() {
                instance.destroy_instance(None);
            }
        }
    }
}

impl Drop for VulkanVideoEncoder {
    fn drop(&mut self) {
        // The whole teardown sequence lives in `VkTeardown` (shared with `open_inner`'s failure
        // unwind): rebuild one from our fields and let its Drop run it.
        drop(VkTeardown {
            instance: Some(self.instance.clone()),
            device: Some(self.device.clone()),
            vq_dev: Some(self.vq_dev.clone()),
            import_cache: std::mem::take(&mut self.import_cache),
            frames: std::mem::take(&mut self.frames),
            compute_pool: self.compute_pool,
            cmd_pool: self.cmd_pool,
            shader: vk::ShaderModule::null(),
            csc_pipe: self.csc_pipe,
            csc_layout: self.csc_layout,
            csc_pool: self.csc_pool,
            csc_dsl: self.csc_dsl,
            sampler: self.sampler,
            dpb_views: std::mem::take(&mut self.dpb_views),
            dpb_image: self.dpb_image,
            dpb_mem: self.dpb_mem,
            params: self.params,
            session: self.session,
            session_mem: std::mem::take(&mut self.session_mem),
        });
    }
}

// Session/frame construction + parameter-set builders live in `vk_build.rs` (WP7.5) — the
// amf_sys.rs shape: a #[path] child module sees this file's private items (Frame and
// friends), so the split costs no visibility churn.
#[path = "vk_build.rs"]
mod build;
use self::build::{
    align_up, build_parameters_av1, build_parameters_h265, make_frame, make_video_image,
    probe_rgb_direct, rgb_model_for,
};

#[cfg(test)]
mod tests {
    use super::{build_h265_rps_s0, parse_rgb_request, VulkanVideoEncoder};
    use crate::{Codec, Encoder};
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

    /// The full-retention RPS: every resident picture is listed (so the decoder keeps it), the
    /// setup slot's dying occupant is not, and `used_by_curr_pic` marks exactly the real reference.
    #[test]
    fn h265_rps_retains_all_residents() {
        // Steady state: slots hold POCs 8..15, current POC 16, reconstructing over the slot that
        // holds POC 8 (the oldest), referencing POC 15 (the newest).
        let slot_poc = [8i32, 9, 10, 11, 12, 13, 14, 15];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 15, 16);
        assert_eq!(n, 7, "all residents except the dying setup occupant");
        // S0 is newest-first with cumulative deltas: POCs 15,14,...,9 → every step is 1.
        assert_eq!(&deltas[..7], &[0u16; 7], "delta_minus1 chain of 1-steps");
        assert_eq!(used, 1 << 0, "only the newest (POC 15) is actively used");

        // Recovery shape: reference an OLDER picture (POC 12) while newer residents stay listed.
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 12, 16);
        assert_eq!(n, 7);
        assert_eq!(used, 1 << 3, "POC 12 is 4th-newest → S0 index 3");
        assert_eq!(&deltas[..7], &[0u16; 7]);

        // Sparse DPB right after an IDR: only POCs 0..2 resident, gaps encoded in the deltas.
        let slot_poc = [0i32, 1, 2, -1, -1, -1, -1, -1];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 3, 2, 3);
        assert_eq!(n, 3);
        assert_eq!(&deltas[..3], &[0, 0, 0]);
        assert_eq!(used, 1 << 0);

        // Non-adjacent POCs: current 10, residents {9, 6, 2} → deltas-minus1 {0, 2, 3}.
        let slot_poc = [2i32, -1, 6, -1, 9, -1, -1, -1];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 7, 6, 10);
        assert_eq!(n, 3);
        assert_eq!(&deltas[..3], &[0, 2, 3]);
        assert_eq!(used, 1 << 1, "POC 6 is the 2nd-newest → S0 index 1");
    }

    fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        CapturedFrame {
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// Index of the wire frame the smoke run "loses" and drops from the client-view dump.
    const SMOKE_LOST: usize = 4;
    /// Index of the recovery-anchor frame — the RFI fires just before this submission, and one
    /// normal P (frame 5, referencing the lost frame 4) is encoded IN BETWEEN, mirroring a real
    /// session where the loss report round-trips while the encoder keeps producing. That fed
    /// post-loss frame is what makes the dump exercise reference RETENTION: a conforming decoder
    /// processes its RPS before the anchor arrives, so the anchor's reference (frame 3) survives
    /// only because every P-frame's RPS lists all resident DPB pictures ([`build_h265_rps_s0`]).
    const SMOKE_ANCHOR: usize = 6;

    /// Full `open` → IDR → P-frames → RFI-recovery path through the real [`VulkanVideoEncoder`],
    /// codec-parameterized. Exercises the CPU→NV12 compute CSC, the NV12 plane copy, the DPB ring and
    /// the reference-slot RFI end-to-end; returns the AUs. Wire frame [`SMOKE_LOST`] is "lost", one
    /// normal P referencing it is still encoded (the in-flight window), then frame [`SMOKE_ANCHOR`]
    /// is the clean recovery anchor referencing pre-loss frame 3 (no IDR).
    fn run_smoke(codec: Codec) -> Vec<crate::EncodedFrame> {
        run_smoke_opts(codec, false).expect("smoke")
    }

    /// `run_smoke` with the RGB-direct source explicit. `None` = requested but unavailable on
    /// this driver (probe declined) — the rgb test soft-skips instead of failing.
    fn run_smoke_opts(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc =
            VulkanVideoEncoder::open_opts(codec, w, h, 60, 10_000_000, rgb).expect("open");
        if rgb && enc.rgb.is_none() {
            eprintln!("run_smoke_opts: RGB-direct unavailable on this driver — skipping");
            return None;
        }
        assert!(enc.caps().supports_rfi, "must advertise RFI");

        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [200, 200, 40, 255],
            [40, 200, 200, 255],
            [200, 40, 200, 255],
            [120, 200, 80, 255],
            [80, 120, 200, 255],
        ];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            if i == SMOKE_ANCHOR {
                // The client reports wire frame SMOKE_LOST lost → the next frame must re-anchor
                // on a resident pre-loss reference (newest older than the loss = frame 3).
                assert!(
                    enc.invalidate_ref_frames(SMOKE_LOST as i64, SMOKE_LOST as i64),
                    "RFI should find an older-than-loss slot"
                );
            }
            enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            // The encoder is pipelined now: submit() no longer blocks, so drain whatever completed
            // (FIFO = submission order) and finish the tail via flush below.
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");

        let (mut keyframes, mut anchors) = (0usize, 0usize);
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.data.is_empty(), "AU {i} empty");
            keyframes += au.keyframe as usize;
            anchors += au.recovery_anchor as usize;
            if i == 0 {
                assert!(au.keyframe, "frame 0 must be IDR");
            }
            if i == SMOKE_ANCHOR {
                assert!(
                    au.recovery_anchor && !au.keyframe,
                    "frame {SMOKE_ANCHOR} must be a clean recovery P-frame, not IDR"
                );
            }
        }
        assert_eq!(keyframes, 1, "exactly one IDR (frame 0)");
        assert_eq!(
            anchors, 1,
            "exactly one recovery anchor (frame {SMOKE_ANCHOR})"
        );
        Some(aus)
    }

    /// Dump the full stream + a client-view stream with AU [`SMOKE_LOST`] removed to
    /// `$HOME/vkenc-host-smoke*.{ext}` for an out-of-band `ffmpeg` decode check. The full stream
    /// must decode 0-error. The dropped one mirrors what a real client feeds its decoder: expect
    /// exactly ONE missing-reference complaint (frame 5 referencing the lost frame 4 — the
    /// concealment the client's freeze hides) and NONE at the anchor — a complaint about the
    /// anchor's reference (frame 3 / POC 3) means reference retention regressed and the "clean"
    /// re-anchor ships corruption.
    fn dump_smoke(aus: &[crate::EncodedFrame], ext: &str) {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
        let p1 = format!("{home}/vkenc-host-smoke.{ext}");
        let _ = std::fs::write(&p1, &full);
        eprintln!(
            "run_smoke: wrote {p1} ({} bytes, {} AUs)",
            full.len(),
            aus.len()
        );
        let dropped: Vec<u8> = aus
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != SMOKE_LOST)
            .flat_map(|(_, a)| a.data.iter().copied())
            .collect();
        let p2 = format!("{home}/vkenc-host-smoke-dropped.{ext}");
        let _ = std::fs::write(&p2, &dropped);
        eprintln!(
            "run_smoke: wrote {p2} (frame {SMOKE_LOST} dropped; frame 5 conceals, \
             recovery@{SMOKE_ANCHOR} anchors to frame 3 and must decode clean)"
        );
    }

    /// HEVC smoke. `#[ignore]`d so it only runs where a real `VK_KHR_video_encode_h265` driver exists
    /// — build in the distrobox, run on the host:
    ///   cargo test -p punktfunk-host --features vulkan-encode --no-run
    ///   <host> target/debug/deps/punktfunk_host-<hash> --ignored --nocapture vulkan_smoke
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke() {
        dump_smoke(&run_smoke(Codec::H265), "h265");
    }

    /// AV1 smoke — same path over `VK_KHR_video_encode_av1`. Dumps `.obu` (low-overhead OBU stream:
    /// our TD + seq-header prefixes ahead of each Vulkan-emitted frame OBU) for `ffmpeg` to decode.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_av1 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke_av1() {
        dump_smoke(&run_smoke(Codec::Av1), "obu");
    }

    /// RGB-direct (EFC) smoke — the same full path with the session opened on the RGB encode
    /// source (`VK_VALVE_video_encode_rgb_conversion`): the CPU frames go through the staging
    /// upload into a profiled BGRA encode-src and the VCN front-end does the 709-narrow CSC.
    /// Soft-skips (prints + passes) where the extension/probe is unavailable, so it can sit in
    /// the same `--ignored` run as the other smokes on any RADV box.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb() {
        if let Some(aus) = run_smoke_opts(Codec::H265, true) {
            dump_smoke(&aus, "rgb.h265");
        }
    }

    /// RGB-direct AV1 twin of [`vulkan_smoke_rgb`].
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb_av1() {
        if let Some(aus) = run_smoke_opts(Codec::Av1, true) {
            dump_smoke(&aus, "rgb.obu");
        }
    }

    /// A packed 2:10:10:10 (`xRGB_210LE` / `PixelFormat::X2Rgb10`) CPU frame — the layout a
    /// gamescope HDR capture delivers, and the one its node actually fixates. Channels are given
    /// as 10-bit code values so the test can speak in the units the PQ container uses.
    fn cpu_frame_rgb10(w: u32, h: u32, pts_ns: u64, rgb10: [u16; 3]) -> CapturedFrame {
        // x:R:G:B 2:10:10:10 little-endian — B in bits 0-9, G in 10-19, R in 20-29.
        let word = ((rgb10[0] as u32 & 0x3FF) << 20)
            | ((rgb10[1] as u32 & 0x3FF) << 10)
            | (rgb10[2] as u32 & 0x3FF);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&word.to_le_bytes());
        }
        CapturedFrame {
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// The 10-bit twin of [`run_smoke_opts`]: a full HDR session end to end — Main10 / AV1-at-10
    /// profile, `G10X6…3PACK16` picture + DPB, `rgb2yuv10.comp` (or the EFC's BT.2020 conversion
    /// when `rgb`), and headers carrying the depth + BT.2020/PQ signalling.
    ///
    /// This is the least-exercised path in the backend — nothing had ever created a 10-bit video
    /// session here — so the assertions stay where a smoke test can be honest: every submit
    /// encodes, the AU count matches, and the depth the encoder settled on is the one asked for.
    /// Colour truth is out of band (the `dump_smoke` + ffmpeg recipe), because a shader that
    /// wrote the 10 bits into the wrong end of the word still produces a decodable stream.
    ///
    /// `None` = the device declined the 10-bit profile (or, with `rgb`, the BT.2020 EFC
    /// conversion): a soft skip, not a failure — that is the same verdict `open_amd_intel`
    /// consults to route such a session to VAAPI.
    fn run_smoke_10bit(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc =
            match VulkanVideoEncoder::open_opts_depth(codec, w, h, 60, 10_000_000, rgb, true) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("run_smoke_10bit({codec:?}, rgb={rgb}): open declined — {e:#}");
                    return None;
                }
            };
        if rgb && enc.rgb.is_none() {
            eprintln!("run_smoke_10bit: BT.2020 RGB-direct unavailable on this driver — skipping");
            return None;
        }
        assert!(enc.ten_bit, "a 10-bit session must report 10-bit");

        // 10-bit code values spanning the range, so a wrong shift shows up as a wildly wrong
        // luminance in the dump rather than a plausible-looking picture.
        let colors: [[u16; 3]; 8] = [
            [160, 160, 800],
            [160, 800, 160],
            [800, 160, 160],
            [800, 800, 160],
            [160, 800, 800],
            [800, 160, 800],
            [480, 800, 320],
            [320, 480, 800],
        ];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            enc.submit_indexed(&cpu_frame_rgb10(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");
        assert!(aus[0].keyframe, "frame 0 must be IDR");
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.data.is_empty(), "AU {i} empty");
        }
        Some(aus)
    }

    /// HEVC **Main10** through the compute CSC — the AMD/Intel HDR path a gamescope session takes.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device with a 10-bit profile"]
    fn vulkan_smoke_10bit() {
        if let Some(aus) = run_smoke_10bit(Codec::H265, false) {
            dump_smoke(&aus, "10bit.h265");
        }
    }

    /// AV1 at 10 bits — the codec whose driver coverage is thinner, hence its own smoke.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_av1 device with a 10-bit profile"]
    fn vulkan_smoke_10bit_av1() {
        if let Some(aus) = run_smoke_10bit(Codec::Av1, false) {
            dump_smoke(&aus, "10bit.obu");
        }
    }

    /// HDR with **no host CSC at all**: the EFC does the BT.2020 conversion off the packed 10-bit
    /// source. Soft-skips where the hardware advertises no `MODEL_YCBCR_2020`, which is the one
    /// capability bit in this whole path that has never been seen reported by a real device.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion with the BT.2020 model at 10-bit"]
    fn vulkan_smoke_rgb_10bit() {
        if let Some(aus) = run_smoke_10bit(Codec::H265, true) {
            dump_smoke(&aus, "rgb.10bit.h265");
        }
    }

    /// 24-bpp packed CPU frame (`PixelFormat::Rgb`/`Bgr` — the PipeWire portal's CPU
    /// negotiation). `rgb` is (r, g, b) regardless of `fmt`'s byte order.
    fn cpu_frame_24(w: u32, h: u32, pts_ns: u64, rgb: [u8; 3], fmt: PixelFormat) -> CapturedFrame {
        let px = match fmt {
            PixelFormat::Rgb => [rgb[0], rgb[1], rgb[2]],
            PixelFormat::Bgr => [rgb[2], rgb[1], rgb[0]],
            _ => unreachable!("24-bpp helper"),
        };
        let mut buf = vec![0u8; (w * h * 3) as usize];
        for p in buf.chunks_exact_mut(3) {
            p.copy_from_slice(&px);
        }
        CapturedFrame {
            width: w,
            height: h,
            pts_ns,
            format: fmt,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// Drive one whole 24-bpp CPU session (WP5.4: served via `normalize_cpu_rgb`, not refused).
    /// Frames alternate Rgb/Bgr, which also crosses the staging image's format re-key each
    /// frame. Returns the AUs; colour truth is checked out-of-band (the dump + ffmpeg bars
    /// recipe from WP1.4) — in-tree the load-bearing assertions are that every submit encodes
    /// and, on-glass, that the validation layers stay silent through the expand + upload.
    fn run_smoke_cpu24(rgb_direct: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc = VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, rgb_direct)
            .expect("open");
        if rgb_direct && enc.rgb.is_none() {
            eprintln!("run_smoke_cpu24: RGB-direct unavailable on this driver — skipping");
            return None;
        }
        let colors: [[u8; 3]; 4] = [[200, 40, 40], [40, 200, 40], [40, 40, 200], [200, 200, 40]];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for i in 0..8usize {
            let fmt = if i % 2 == 0 {
                PixelFormat::Rgb
            } else {
                PixelFormat::Bgr
            };
            let frame = cpu_frame_24(w, h, i as u64 * 16_666_667, colors[i / 2], fmt);
            enc.submit_indexed(&frame, i as u32).expect("submit 24-bpp");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), 8, "one AU per 24-bpp frame");
        assert!(aus[0].keyframe, "frame 0 must be IDR");
        Some(aus)
    }

    /// WP5.4 smoke — 24-bpp CPU frames through the CSC path.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke_cpu_rgb24() {
        let aus = run_smoke_cpu24(false).expect("CSC mode never soft-skips");
        if let Ok(home) = std::env::var("HOME") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p = format!("{home}/vkenc-host-smoke-cpu24.h265");
            let _ = std::fs::write(&p, &full);
            eprintln!("vulkan_smoke_cpu_rgb24: wrote {p} ({} bytes)", full.len());
        }
    }

    /// WP5.4 smoke, RGB-direct twin — the expanded R8G8B8A8/B8G8R8A8 image IS the encode source
    /// here, so this also answers whether the EFC profile accepts an RGBA-order source on this
    /// hardware (critique F7: unverified until run). Soft-skips without the VALVE extension.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb_cpu24() {
        if let Some(aus) = run_smoke_cpu24(true) {
            if let Ok(home) = std::env::var("HOME") {
                let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
                let p = format!("{home}/vkenc-host-smoke-rgb-cpu24.h265");
                let _ = std::fs::write(&p, &full);
                eprintln!("vulkan_smoke_rgb_cpu24: wrote {p} ({} bytes)", full.len());
            }
        }
    }

    /// The CSC arm REFUSES a source that doesn't match the session mode (the e3354b6d guard —
    /// the check every sibling arm always had; a clamped-texelFetch mismatch used to stream a
    /// silently cropped/edge-padded picture). This test previously DROVE mismatched sizes through
    /// the lenient arm to exercise the per-slot `cpu_img` staging across a size change (the
    /// format-only-keyed cache wrote out of bounds while `submit` returned `Ok`); the guard makes
    /// that scenario unrepresentable through `submit`, structurally retiring the hazard — the
    /// size-keyed staging from that fix stays as belt-and-braces. What's left to pin:
    /// refusal in BOTH directions, and that a refused submit does not WEDGE the session — the
    /// bail happens after step 1's frame-type bookkeeping, so "the next well-sized frame still
    /// encodes" is a real property, not a formality (the host routes the error to its
    /// encoder-rebuild path and the session must be able to continue if that path retries).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
    fn vulkan_csc_refuses_a_mismatched_source() {
        // CSC mode (rgb=false) — the arm the guard covers.
        let mut enc = VulkanVideoEncoder::open_opts(Codec::H265, 512, 512, 60, 10_000_000, false)
            .expect("open");
        enc.submit_indexed(&cpu_frame(512, 512, 0, [40, 40, 200, 255]), 0)
            .expect("well-sized baseline");
        while enc.poll().expect("poll").is_some() {}
        // Smaller AND larger both refuse — the guard is equality on the MODE (render size), not
        // a ceiling; the render-vs-CODED padding tolerance lives in the direct arms, untouched.
        let e = enc
            .submit_indexed(&cpu_frame(128, 128, 16_666_667, [200, 40, 40, 255]), 1)
            .expect_err("smaller source must refuse");
        assert!(e.to_string().contains("mismatched"), "{e:#}");
        let e = enc
            .submit_indexed(&cpu_frame(640, 640, 33_333_334, [200, 40, 40, 255]), 2)
            .expect_err("larger source must refuse");
        assert!(e.to_string().contains("mismatched"), "{e:#}");
        // The refusals must not wedge the session: a well-sized frame still encodes and an AU
        // still comes out the other end.
        let mut got_au = false;
        for i in 3..11u64 {
            enc.submit_indexed(
                &cpu_frame(512, 512, i * 16_666_667, [40, 200, 40, 255]),
                i as u32,
            )
            .expect("well-sized after refusal");
            while let Ok(Some(_)) = enc.poll() {
                got_au = true;
            }
        }
        assert!(got_au, "no AU after the refused submits — session wedged");
        eprintln!("done — under validation layers this run must report ZERO VUID errors");
    }

    /// A mid-stream [`Encoder::reset`] must not change what `vkCmdBeginVideoCodingKHR` declares
    /// about the session's rate-control state. `reset()` re-arms `first_frame` (the next frame
    /// re-issues RESET + install), but it does NOT rebuild the session, so the CBR installed
    /// earlier is still the session's current mode when that frame opens its coding scope —
    /// which is why the declaration keys on `rc_installed`, not `!first_frame`.
    ///
    /// Phase 2 also stages a `reconfigure_bitrate` BEFORE the reset — the pending rate survives
    /// a reset by design, and the post-reset first frame used to promote it into the begin
    /// declaration before the session had installed it (`VUID-...-08254`, the `<declared> !=
    /// <current>` sibling of the 08253 case below).
    ///
    /// Only the validation layers can see either mismatch, so run as:
    /// `VK_LOADER_LAYERS_ENABLE='*validation*' cargo test ... -- --ignored`. Confirmed on RADV
    /// PHOENIX: 08253 before its fix, zero after (2026-07-25); and with this retarget phase,
    /// exactly one 08254 on the pre-fix build vs zero on the fixed one (same day, same box).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
    fn vulkan_reset_keeps_the_declared_rate_control_state() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, false).expect("open");
        eprintln!("phase 1: 4 frames (installs CBR on frame 0)");
        for i in 0..4u64 {
            enc.submit_indexed(
                &cpu_frame(w, h, i * 16_666_667, [40, 40, 200, 255]),
                i as u32,
            )
            .expect("submit");
            while enc.poll().expect("poll").is_some() {}
        }
        eprintln!("phase 2: reconfigure_bitrate() then reset() — the 08254 coincidence");
        // The ABR retarget and the stall watchdog live in the same encode loop and correlate
        // (congestion is when you retarget AND when the encoder wedges), so the pending rate
        // must survive the reset — and the NEXT frame's begin must still declare the OLD rate,
        // because a reset never touches the session object. Promoting the pending rate before
        // recording declared the new rate ahead of its install:
        // VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08254, one frame, exactly here.
        assert!(enc.reconfigure_bitrate(20_000_000), "retarget should stage");
        assert!(enc.reset(), "reset should succeed");
        eprintln!("phase 3: 4 more frames — first one re-declares the OLD rate, installs the NEW");
        for i in 4..8u64 {
            enc.submit_indexed(
                &cpu_frame(w, h, i * 16_666_667, [200, 40, 40, 255]),
                i as u32,
            )
            .expect("submit after reset");
            while enc.poll().expect("poll").is_some() {}
        }
        eprintln!("done — under validation layers this run must report ZERO VUID errors");
    }

    /// `PUNKTFUNK_VULKAN_RGB_DIRECT` accepts the same spellings as every sibling knob, trimmed.
    #[test]
    fn rgb_direct_knob_accepts_the_house_spellings() {
        for on in ["1", "true", "yes", "on", " 1", "1 ", "\ton\n"] {
            assert_eq!(parse_rgb_request(Some(on)), Some(true), "{on:?}");
        }
        for off in ["0", "false", "no", "off", " 0", "0 ", "\toff\n"] {
            assert_eq!(parse_rgb_request(Some(off)), Some(false), "{off:?}");
        }
    }

    /// Unset, empty and unrecognised values all mean "use the default" — never a force-on. The
    /// pre-fix parse read anything-but-`"0"` as a force, so a trailing space on `PUNKTFUNK_...=0 `
    /// silently enabled the very path the operator was disabling, cursor and all.
    #[test]
    fn rgb_direct_knob_never_force_enables_on_an_unrecognised_value() {
        assert_eq!(parse_rgb_request(None), None);
        for junk in ["", "   ", "2", "maybe", "0x0", "enabled"] {
            assert_eq!(parse_rgb_request(Some(junk)), None, "{junk:?}");
        }
    }
}
