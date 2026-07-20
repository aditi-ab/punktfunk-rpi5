//! Raw **Vulkan Video** HEVC + AV1 encoder (`VK_KHR_video_encode_h265` / `_av1`) with true
//! reference-frame invalidation — the open-stack AMD/Intel-Linux twin of the direct-NVENC RFI path.
//! The app owns the DPB, so loss recovery is a clean P-frame that re-references a known-good older
//! slot (no IDR): HEVC via an explicit short-term RPS, AV1 via `ref_frame_idx` + a
//! `primary_ref_frame = NONE` recovery anchor that also breaks the CDF chain.
//!
//! Capture delivers packed RGB (dmabuf/CPU); this backend imports it, runs an on-GPU RGB→NV12
//! BT.709 compute CSC, then encodes. Proven end-to-end in `punktfunk-planning/design/vkenc-probe-harness`.
//! Opt-in via `PUNKTFUNK_VULKAN_ENCODE`; gated to HEVC/AV1 + a device that advertises the encode op.
//! The AV1 encode structs our pinned `ash 0.38` predates are vendored in `vk_av1_encode.rs`.
#![allow(clippy::too_many_arguments)]

use super::vk_util::{color_range, find_mem, make_plain_image, make_view, pixel_to_vk};
use crate::{Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use pf_frame::{CapturedFrame, FramePayload};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsRawFd;

const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// Max resident dmabuf imports (comfortably above any PipeWire pool depth; imports alias existing
/// buffers so this holds handles, not new allocations).
const IMPORT_CACHE_CAP: usize = 16;
// Prebuilt SPIR-V for the RGB→NV12 BT.709 compute CSC. Source is `rgb2yuv.comp` beside this file;
// regenerate with `glslangValidator -V rgb2yuv.comp -o rgb2yuv.spv` after editing the shader.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
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

/// Newest resident DPB slot whose wire index is strictly older than the loss — the clean anchor.
fn pick_recovery_slot(slot_wire: &[i64], loss_first: i64) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_wire = -1i64;
    for (i, &w) in slot_wire.iter().enumerate() {
        if w >= 0 && w < loss_first && w > best_wire {
            best = Some(i);
            best_wire = w;
        }
    }
    best
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
/// so any slot [`pick_recovery_slot`] can pick is decodable by construction.
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
    bs_buf: vk::Buffer,
    bs_mem: vk::DeviceMemory,
    bs_ptr: BsPtr,              // persistent mapping of bs_mem (see BsPtr)
    csc_set: vk::DescriptorSet, // Y/UV bindings fixed; binding 0 (RGB) rewritten each use
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    nv12_src: vk::Image,
    nv12_mem: vk::DeviceMemory,
    nv12_view: vk::ImageView,
    // CPU-input staging (lazily sized; only the software-capture / smoke-test path uses it).
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
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
    compute_family: u32,
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
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,

    // --- in-flight frame ring (pipelining) ---
    frames: Vec<Frame>,         // per-slot private resources
    ring: usize,                // next slot to record into (round-robin over `frames`)
    in_flight: VecDeque<usize>, // slots submitted but not yet read back, oldest first
    bs_size: u64,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,

    // --- rate control (CBR), rebuilt-safe ---
    bitrate: u64,
    fps: u32,
    /// Resolved encode quality level (see [`quality_request`]) — installed via the first frame's
    /// `ENCODE_QUALITY_LEVEL` control and baked into the session-parameters object (the spec
    /// requires the two to match).
    quality_level: u32,
    /// `PUNKTFUNK_PERF` CSC/encode split: >0 ⇒ per-frame GPU timestamps are recorded around the
    /// compute batch and a sampled `csc_us` line is logged; the value is the device timestamp
    /// period in ns/tick. 0.0 ⇒ off (env unset, or the compute family has no timestamp support).
    ts_period_ns: f64,
    perf_at: std::time::Instant, // last sampled csc_us log (2 s cadence)
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
    first_frame: bool, // needs RESET + DPB layout transition + CBR install + IDR
    force_kf: bool, // request_keyframe / non-recoverable loss -> next frame is IDR
    pending_loss: Option<i64>, // invalidate_ref_frames(first) -> recover on next frame
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: the encoder is used only from the single encode thread; all Vulkan handles are owned and
// never shared. Matches `NvencCudaEncoder`'s `unsafe impl Send`.
unsafe impl Send for VulkanVideoEncoder {}

impl VulkanVideoEncoder {
    /// Signature mirrors the other Linux backends' `open` (see `nvenc_cuda::NvencCudaEncoder::open`).
    pub fn open(codec: Codec, width: u32, height: u32, fps: u32, bitrate_bps: u64) -> Result<Self> {
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
            )
        }
    }

    unsafe fn open_inner(
        codec: Codec,
        w: u32,
        h: u32,
        rw: u32,
        rh: u32,
        fps: u32,
        bitrate: u64,
    ) -> Result<Self> {
        use super::vk_av1_encode as av1b;
        let av1 = codec == Codec::Av1;
        let codec_op = if av1 {
            vk::VideoCodecOperationFlagsKHR::from_raw(av1b::VIDEO_CODEC_OPERATION_ENCODE_AV1)
        } else {
            vk::VideoCodecOperationFlagsKHR::ENCODE_H265
        };
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

        // pick the physical device + encode queue family (skip llvmpipe)
        let (pd, encode_family) = {
            let mut found = None;
            for pd in instance.enumerate_physical_devices()? {
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
                        found = Some((pd, i as u32));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.context("no VK_KHR_video_encode queue for the requested codec on any device")?
        };
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

        // the encode profile — H265 Main, or AV1 Main (AV1 profile chained raw since ash 0.38 lacks it)
        let mut h265_profile = vk::VideoEncodeH265ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
        let mut av1_profile = av1b::VideoEncodeAV1ProfileInfoKHR {
            s_type: av1b::stype(av1b::ST_PROFILE_INFO),
            p_next: std::ptr::null(),
            std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
        };
        let mut usage = vk::VideoEncodeUsageInfoKHR::default()
            .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
            .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
            .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
        let mut profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(codec_op)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut usage);
        if av1 {
            // prepend the AV1 profile into the p_next chain (it can't `push_next` — vendored struct)
            av1_profile.p_next = profile.p_next;
            profile.p_next = &av1_profile as *const _ as *const c_void;
        } else {
            profile = profile.push_next(&mut h265_profile);
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
        // B0 telemetry (design/vulkan-rgb-direct-encode.md): could this host skip the compute
        // CSC entirely and hand the captured RGB dmabuf straight to the VCN EFC? Logged only —
        // the encode input stays the CSC until B1 lands.
        let rgb_direct = probe_rgb_direct(&instance, &vq_inst, pd, codec_op, av1);
        tracing::info!(
            rgb_direct,
            "vulkan-encode: EFC RGB-direct probe (telemetry only; encode input stays compute CSC)"
        );

        // logical device: encode + compute queues + video extensions (AV1 ext name is raw — ash lacks it)
        let dev_exts = [
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
        let mut session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(encode_family)
            .video_profile(&profile)
            .picture_format(NV12)
            .max_coded_extent(vk::Extent2D {
                width: w,
                height: h,
            })
            .reference_picture_format(NV12)
            .max_dpb_slots(DPB_SLOTS + 1)
            .max_active_reference_pictures(1)
            .std_header_version(&std_hdr);
        if av1 {
            session_ci.p_next = &av1_sci as *const _ as *const c_void;
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
        let (params, header, frame_prefix) = if av1 {
            build_parameters_av1(
                &device,
                &vq_dev,
                session,
                w,
                h,
                rw,
                rh,
                av1_caps.max_level,
                av1_superblock128,
                quality_level,
            )?
        } else {
            let (p, hdr) = build_parameters_h265(
                &device,
                &vq_dev,
                &venc_dev,
                session,
                w,
                h,
                rw,
                rh,
                quality_level,
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
            NV12,
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
                .push(make_view(&device, dpb_image, NV12, slot)?);
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
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(CSC_SPV))?;
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
                ts_period_ns > 0.0,
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
            quality_level,
            ts_period_ns,
            perf_at: std::time::Instant::now(),
            pending_bitrate: None,
            width: w,
            height: h,
            render_w: rw,
            render_h: rh,
            poc: 0,
            enc_count: 0,
            auto_wire: 0,
            first_frame: true,
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

    /// Import a packed-RGB dmabuf as a SAMPLED VkImage (explicit DRM modifier). Caller destroys.
    unsafe fn import_dmabuf(
        &self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
        super::vk_util::import_rgb_dmabuf(&self.device, &self.ext_fd, &self.mem_props, d, cw, ch)
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
        if let Some(&(_, _, img, _, view)) = self.import_cache.iter().find(|e| (e.0, e.1) == key) {
            return Ok((img, view, false));
        }
        let (img, mem, view) = self.import_dmabuf(d, cw, ch)?;
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
            let (_, _, oi, om, ov) = self.import_cache.remove(0);
            self.device.destroy_image_view(ov, None);
            self.device.destroy_image(oi, None);
            self.device.free_memory(om, None);
        }
        self.import_cache.push((key.0, key.1, img, mem, view));
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
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        let need = (w * h * 4) as u64;
        if self.frames[slot].cpu_img.map(|(_, _, _, f)| f) != Some(fmt) {
            if let Some((i, m, v, _)) = self.frames[slot].cpu_img.take() {
                dev.destroy_image_view(v, None);
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            let (i, m, v) = make_plain_image(
                &dev,
                &self.mem_props,
                fmt,
                w,
                h,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            self.frames[slot].cpu_img = Some((i, m, v, fmt));
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
            let buf = dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(need)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )?;
            let req = dev.get_buffer_memory_requirements(buf);
            let mem = dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(find_mem(
                        &self.mem_props,
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )),
                None,
            )?;
            dev.bind_buffer_memory(buf, mem, 0)?;
            self.frames[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.frames[slot].cpu_stage.unwrap();
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        let n = bytes.len().min(need as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
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
        // Mid-stream rate retarget (`reconfigure_bitrate`): a first frame installs its RC state
        // fresh (RESET + ENCODE_RATE_CONTROL in the record fns), so a pending rate folds straight
        // into it; mid-stream it stays pending — the record fns emit an ENCODE_RATE_CONTROL
        // control command against the declared current state, and step 5 promotes it.
        if self.first_frame {
            if let Some(nb) = self.pending_bitrate.take() {
                self.bitrate = nb;
            }
        }
        let mut is_idr = self.first_frame || self.force_kf;
        let mut ref_slot = self.prev_slot;
        let mut recovery = false;
        if let Some(lf) = self.pending_loss.take() {
            if !is_idr {
                match pick_recovery_slot(&self.slot_wire, lf) {
                    Some(s) => {
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

        // ---- 2. RGB source -> compute_cmd: prep barriers + CSC + copy into nv12_src ----
        let dev = self.device.clone(); // cheap handle clone -> lets us also call &mut self helpers
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        // PUNKTFUNK_PERF: bracket the whole compute batch (import barriers + cursor prep + CSC
        // dispatch + plane copies) with timestamps — read back in `read_slot` as `csc_us`, the
        // half of the fence wait that is NOT the ASIC encode.
        if self.ts_period_ns > 0.0 {
            dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
            dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
        }

        // Cursor-as-metadata: refresh this slot's cursor image (only when the bitmap changed) and
        // get the shader push constant. Recorded into `compute_cmd` before the CSC dispatch samples it.
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
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
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
                let fmt = pixel_to_vk(frame.format).context("unsupported CPU pixel format")?;
                let view = self.ensure_cpu_rgb(slot, fmt, bytes)?;
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
                        .image_extent(vk::Extent3D {
                            width: self.width,
                            height: self.height,
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
                &dev, cmd, query_pool, bs_buf, nv12_src, nv12_view, is_idr, recovery, ref_slot,
                setup_idx, poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev, cmd, query_pool, bs_buf, nv12_src, nv12_view, is_idr, ref_slot, setup_idx,
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
        // Stash the metadata `read_slot` needs once `fence` signals.
        self.frames[slot].pts_ns = frame.pts_ns;
        self.frames[slot].keyframe = is_idr;
        self.frames[slot].recovery_anchor = recovery;

        // ---- 5. advance DPB bookkeeping (in submission order, before returning) ----
        if is_idr {
            self.slot_wire.iter_mut().for_each(|s| *s = -1);
            self.slot_poc.iter_mut().for_each(|s| *s = -1);
        }
        self.slot_wire[setup_idx] = wire;
        self.slot_poc[setup_idx] = poc;
        self.prev_slot = setup_idx;
        self.poc = poc + 1;
        self.enc_count += 1;
        self.first_frame = false;
        self.force_kf = false;
        if let Some(nb) = self.pending_bitrate.take() {
            // The retarget control command is recorded (execution follows submission order): the
            // session's RC state IS the new rate from this frame on — later begins declare it.
            self.bitrate = nb;
            tracing::debug!(
                mbps = nb / 1_000_000,
                "vulkan-encode: rate control retargeted in place (no IDR)"
            );
        }
        Ok(())
    }

    /// Begin `cmd` and record the pre-encode setup shared by both codecs: the query-pool reset,
    /// nv12_src GENERAL → VIDEO_ENCODE_SRC (the CSC semaphore already ordered the copy before
    /// this), and the DPB transition — on the first frame a whole-image UNDEFINED → DPB init;
    /// afterwards the cross-command-buffer pipelining barrier that orders the previous frame's
    /// reconstruct-write before this frame's reference read/write (the in-flight ring records
    /// frame N+1 while N still encodes; the barrier's first scope covers all prior-submitted
    /// encode work on this queue, spanning the separate command buffers).
    unsafe fn begin_encode_cmd(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        nv12_src: vk::Image,
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
        let pre_enc = [
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(nv12_src)
                .subresource_range(color_range(0)),
            dpb_barrier,
        ];
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
        nv12_src: vk::Image,
        nv12_view: vk::ImageView,
        is_idr: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        use ash::vk::native as h;
        let ext2d = vk::Extent2D {
            width: self.width,
            height: self.height,
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

        // CBR rate control (chained manually; push_next would clobber rc.p_next)
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
            .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(1000)
            .initial_virtual_buffer_size_in_ms(500);
        rc.p_next = &h265_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        self.begin_encode_cmd(dev, cmd, query_pool, nv12_src)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        if !self.first_frame {
            begin.p_next = rc_ptr;
        } // CBR is current state after frame 0's control
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + CBR install + explicit quality level. Without ENCODE_QUALITY_LEVEL, RADV
            // never sends the VCN a preset op at all (the firmware default preset runs);
            // installing it makes the preset deterministic on every driver and selects the
            // fastest tier by default (see `quality_request`). The quality struct chains ahead
            // of the rate-control state — the spec requires a quality-level change to carry
            // ENCODE_RATE_CONTROL, which the RESET install provides anyway.
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = rc_ptr;
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
            // installs the NEW rate — the same CBR shape with only the bitrate moved. No RESET,
            // no IDR: the DPB and reference chain carry straight on. `record_submit` promotes
            // `nb` into `self.bitrate` after recording, so later begins declare the new state.
            let rc_layer2 = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut rc2 = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(1000)
                .initial_virtual_buffer_size_in_ms(500);
            rc2.p_next = &h265_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(nv12_view);
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
        nv12_src: vk::Image,
        nv12_view: vk::ImageView,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        order: i32,
    ) -> Result<()> {
        use super::vk_av1_encode as av1;
        use ash::vk::native as h;
        let ext2d = vk::Extent2D {
            width: self.width,
            height: self.height,
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

        let seg: h::StdVideoAV1Segmentation = std::mem::zeroed();
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
        std_pic.pSegmentation = &seg;
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
            constant_q_index: quant.base_q_idx as u32,
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
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
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
            .coded_extent(ext2d)
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

        // ---- CBR rate control (generic layer + AV1 codec info chained manually) ----
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
            .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(1000)
            .initial_virtual_buffer_size_in_ms(500);
        rc.p_next = &av1_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        // ---- record cmd: begin + shared pre-encode barriers, then begin/encode/end coding ----
        self.begin_encode_cmd(dev, cmd, query_pool, nv12_src)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        if !self.first_frame {
            begin.p_next = rc_ptr;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + CBR install + explicit quality level. Without ENCODE_QUALITY_LEVEL, RADV
            // never sends the VCN a preset op at all (the firmware default preset runs);
            // installing it makes the preset deterministic on every driver and selects the
            // fastest tier by default (see `quality_request`). The quality struct chains ahead
            // of the rate-control state — the spec requires a quality-level change to carry
            // ENCODE_RATE_CONTROL, which the RESET install provides anyway.
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = rc_ptr;
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
                .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(1000)
                .initial_virtual_buffer_size_in_ms(500);
            rc2.p_next = &av1_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(nv12_view);
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
        // PUNKTFUNK_PERF CSC split (best-effort): the fence signaled, so the compute batch that
        // wrote these timestamps completed long ago — WAIT is a formality. Sampled to one log
        // line per ~2 s; `wait_us` in the pump's stage perf minus this ≈ the ASIC encode.
        if self.ts_period_ns > 0.0 {
            let mut ts = [0u64; 2];
            if dev
                .get_query_pool_results(
                    f.ts_pool,
                    0,
                    &mut ts,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                )
                .is_ok()
                && self.perf_at.elapsed() >= std::time::Duration::from_secs(2)
            {
                self.perf_at = std::time::Instant::now();
                let csc_us = (ts[1].saturating_sub(ts[0]) as f64 * self.ts_period_ns) / 1000.0;
                tracing::info!(
                    csc_us = format!("{csc_us:.0}"),
                    au_bytes = len,
                    "vulkan-encode split (sampled): csc=GPU compute batch (import barriers + \
                     CSC + plane copies); ASIC encode ≈ stage-perf wait_us − csc"
                );
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
        // Taint sweep BEFORE picking the anchor (the fecbec2d fix AMF and QSV got; this backend was
        // carved out one commit later and never received it). "Resident and older than THIS loss" is
        // not the same as "the client decoded it": after an earlier loss [a,b] was recovered at wire
        // r, everything in [a, r-1] is undecodable at the client — the lost frames plus every frame
        // that predicted through the gap. Those wires stay valid anchor candidates here until the
        // 8-slot ring rolls them out, so a LATER loss can anchor on one and ship corruption tagged
        // `recovery_anchor` — which is the client's definitive re-anchor signal (reanchor.rs), so it
        // lifts the post-loss freeze onto a picture built from a reference it never had.
        //
        // Blank `slot_wire` ONLY. `slot_poc` must keep naming every physically-resident DPB picture
        // for `build_h265_rps_s0`, or a conforming decoder evicts them and the anchor references a
        // picture the client already dropped. `slot_wire` is the RFI/loss domain; `slot_poc` is the
        // reference-delta domain. `prev_slot` and the normal P-frame path are indices, not wires, so
        // ordinary prediction is unaffected.
        for w in self.slot_wire.iter_mut() {
            if *w >= first_frame {
                *w = -1;
            }
        }
        // Can we anchor a clean P-frame to a resident slot strictly older than the loss?
        // (A sweep that empties every candidate yields `None` here and declines the RFI, matching
        // `qsv_live_ltr_rfi_taint_sweep_declines`.)
        match pick_recovery_slot(&self.slot_wire, first_frame) {
            Some(_) => {
                self.pending_loss = Some(first_frame);
                true
            }
            None => {
                // Decline WITHOUT self-arming an IDR: the caller owns the fallback, and its
                // keyframe path is cooldown-coalesced — arming `force_kf` here would bypass that
                // and turn a storm of hopeless RFI requests into one full IDR per request.
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
        // SAFETY: `device_wait_idle` guarantees no slot's fence is still pending before we drop them.
        unsafe {
            let _ = self.device.device_wait_idle();
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
        // — no session churn, no IDR. Same floor as `open` (a 0-rate CBR layer is rejected).
        self.pending_bitrate = Some(bps.max(1_000_000));
        true
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
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,
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
                for (_, _, img, mem, view) in std::mem::take(&mut self.import_cache) {
                    device.destroy_image_view(view, None);
                    device.destroy_image(img, None);
                    device.free_memory(mem, None);
                }
                // Per-frame ring resources (command buffers, descriptor sets freed with their pools).
                for f in std::mem::take(&mut self.frames) {
                    device.destroy_semaphore(f.csc_sem, None);
                    device.destroy_fence(f.fence, None);
                    device.destroy_query_pool(f.query_pool, None);
                    device.destroy_query_pool(f.ts_pool, None);
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
                    if let Some((i, m, v, _)) = f.cpu_img {
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

// ---------- free helpers ----------

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// B0 probe for the RGB-direct encode source (design/vulkan-rgb-direct-encode.md): telemetry
/// only — reports whether this device could take the captured RGB dmabuf directly, with the VCN
/// EFC front-end doing the 709-narrow CSC, via `VK_VALVE_video_encode_rgb_conversion` (RADV
/// since Mesa 26.0, gated on EFC hardware). Returns the verdict logged at open: `"available"`,
/// or the first missing requirement. Nothing changes behavior yet.
unsafe fn probe_rgb_direct(
    instance: &ash::Instance,
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    codec_op: vk::VideoCodecOperationFlagsKHR,
    av1: bool,
) -> &'static str {
    use super::vk_av1_encode as av1b;
    use super::vk_valve_rgb as vrgb;
    // 1. The device extension must exist (Mesa >= 26.0 AND the VCN has an EFC block).
    let Ok(exts) = instance.enumerate_device_extension_properties(pd) else {
        return "probe-failed(ext-enum)";
    };
    if !exts
        .iter()
        .any(|e| std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) == vrgb::EXTENSION_NAME)
    {
        return "no-ext(mesa<26.0-or-no-efc)";
    }
    // 2. Feature bit.
    let mut feat = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
        s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
        p_next: std::ptr::null_mut(),
        video_encode_rgb_conversion: vk::FALSE,
    };
    let mut f2 = vk::PhysicalDeviceFeatures2 {
        p_next: &mut feat as *mut _ as *mut c_void,
        ..Default::default()
    };
    instance.get_physical_device_features2(pd, &mut f2);
    if feat.video_encode_rgb_conversion == vk::FALSE {
        return "no-feature";
    }
    // 3. Capabilities under the rgb-chained profile — the conversion must cover the compute
    //    CSC's exact math (rgb2yuv.comp: BT.709, narrow range, 2x2 average = midpoint siting),
    //    or the wire output would shift colors against today's path. Chains are raw
    //    (`p_next` assignments) because the rgb structs are vendored.
    let rgb_info = vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
        s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
        p_next: std::ptr::null(),
        perform_encode_rgb_conversion: vk::TRUE,
    };
    let mut usage = vk::VideoEncodeUsageInfoKHR::default()
        .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
        .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
        .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
    usage.p_next = &rgb_info as *const _ as *const c_void;
    let mut h265_profile = vk::VideoEncodeH265ProfileInfoKHR::default()
        .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
    let mut av1_profile = av1b::VideoEncodeAV1ProfileInfoKHR {
        s_type: av1b::stype(av1b::ST_PROFILE_INFO),
        p_next: std::ptr::null(),
        std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
    };
    let mut profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(codec_op)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    if av1 {
        av1_profile.p_next = &usage as *const _ as *const c_void;
        profile.p_next = &av1_profile as *const _ as *const c_void;
    } else {
        h265_profile.p_next = &usage as *const _ as *const c_void;
        profile.p_next = &h265_profile as *const _ as *const c_void;
    }
    let mut rgb_caps = vrgb::VideoEncodeRgbConversionCapabilitiesVALVE {
        s_type: vrgb::stype(vrgb::ST_CAPABILITIES),
        p_next: std::ptr::null_mut(),
        rgb_models: 0,
        rgb_ranges: 0,
        x_chroma_offsets: 0,
        y_chroma_offsets: 0,
    };
    let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut av1_caps: av1b::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
    av1_caps.s_type = av1b::stype(av1b::ST_CAPABILITIES);
    let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    if av1 {
        av1_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
    } else {
        h265_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut h265_caps as *mut _ as *mut c_void;
    }
    caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
    let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
    if r != vk::Result::SUCCESS {
        return "no-rgb-profile(caps)";
    }
    if rgb_caps.rgb_models & vrgb::MODEL_YCBCR_709 == 0
        || rgb_caps.rgb_ranges & vrgb::RANGE_NARROW == 0
        || rgb_caps.x_chroma_offsets & vrgb::CHROMA_OFFSET_MIDPOINT == 0
        || rgb_caps.y_chroma_offsets & vrgb::CHROMA_OFFSET_MIDPOINT == 0
    {
        return "no-709-narrow-midpoint";
    }
    // 4. The encode-src format set under this profile must offer BGRA with DRM-modifier tiling —
    //    the capture hands LINEAR BGRx dmabufs (fourcc XR24), which import as B8G8R8A8_UNORM.
    let profile_arr = [profile];
    let plist = vk::VideoProfileListInfoKHR::default().profiles(&profile_arr);
    let mut fmt_info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR);
    fmt_info.p_next = &plist as *const _ as *const c_void;
    let get_fmt = vq_inst.fp().get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    let r = get_fmt(pd, &fmt_info, &mut count, std::ptr::null_mut());
    if r != vk::Result::SUCCESS || count == 0 {
        return "no-rgb-format";
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let r = get_fmt(pd, &fmt_info, &mut count, props.as_mut_ptr());
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return "no-rgb-format";
    }
    if !props[..count as usize].iter().any(|p| {
        p.format == vk::Format::B8G8R8A8_UNORM
            && p.image_tiling == vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
    }) {
        return "no-bgra-modifier-tiling";
    }
    "available"
}

unsafe fn make_video_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    layers: u32,
    usage: vk::ImageUsageFlags,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    concurrent: &[u32],
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let mut ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(fmt)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(profile_list);
    if concurrent.len() >= 2 {
        ci = ci
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(concurrent);
    } else {
        ci = ci.sharing_mode(vk::SharingMode::EXCLUSIVE);
    }
    let img = device.create_image(&ci, None)?;
    let req = device.get_image_memory_requirements(img);
    // Unwind on failure: callers (the open path) only ever see the completed pair.
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )),
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_image(img, None);
            return Err(e.into());
        }
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        device.destroy_image(img, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    Ok((img, mem))
}

/// Build one in-flight frame's private resources: NV12 encode-src, Y/UV CSC scratch, its CSC
/// descriptor set (Y/UV bound now, RGB per use), the bitstream buffer + feedback query, and the
/// per-frame command buffers + sync. `profile_list`/`profile` are borrowed only during creation.
///
/// Builds in place into `f` — a [`Frame::default`] the caller has already parked in its
/// [`VkTeardown`] guard — so every handle is owned by the unwind the moment it exists and a
/// mid-build failure leaks nothing.
unsafe fn make_frame(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fams: &[u32],
    profile: &vk::VideoProfileInfoKHR,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,
    bs_size: u64,
    sampler: vk::Sampler,
    with_ts: bool,
    f: &mut Frame,
) -> Result<()> {
    // "no cursor uploaded yet" sentinel — a real serial may be 0 (see `prep_cursor`).
    f.cursor_serial = u64::MAX;
    // NV12 encode-src (filled by the CSC copy) — concurrent compute+encode.
    (f.nv12_src, f.nv12_mem) = make_video_image(
        device,
        mem_props,
        NV12,
        w,
        h,
        1,
        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
        profile_list,
        fams,
    )?;
    f.nv12_view = make_view(device, f.nv12_src, NV12, 0)?;
    // CSC scratch (Y R8 full-res, UV RG8 half-res).
    (f.y_img, f.y_mem, f.y_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8_UNORM,
        w,
        h,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    (f.uv_img, f.uv_mem, f.uv_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8G8_UNORM,
        w / 2,
        h / 2,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    // Cursor overlay: fixed CURSOR_MAX² RGBA8 sampled image + host staging (cursor-as-metadata). The
    // view/descriptor is static (bound at binding 3 below); only the image *content* changes, and
    // only when the pointer bitmap does — see `prep_cursor`.
    (f.cursor_img, f.cursor_mem, f.cursor_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8G8B8A8_UNORM,
        CURSOR_MAX,
        CURSOR_MAX,
        vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
    )?;
    f.cursor_stage = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size((CURSOR_MAX * CURSOR_MAX * 4) as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC),
        None,
    )?;
    let cs_req = device.get_buffer_memory_requirements(f.cursor_stage);
    f.cursor_stage_mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(cs_req.size)
            .memory_type_index(find_mem(
                mem_props,
                cs_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    )?;
    device.bind_buffer_memory(f.cursor_stage, f.cursor_stage_mem, 0)?;
    // Descriptor set — Y/UV storage bindings fixed; binding 0 (RGB) rewritten per use; binding 3
    // (cursor) points at the static cursor image (its layout is SHADER_READ_ONLY once prepped).
    let dsls = [csc_dsl];
    f.csc_set = device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(csc_pool)
            .set_layouts(&dsls),
    )?[0];
    let y_info = [vk::DescriptorImageInfo::default()
        .image_view(f.y_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let uv_info = [vk::DescriptorImageInfo::default()
        .image_view(f.uv_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let cur_info = [vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(f.cursor_view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    device.update_descriptor_sets(
        &[
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&uv_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&cur_info),
        ],
        &[],
    );
    // Bitstream buffer + feedback query.
    f.bs_buf = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(bs_size)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .push_next(profile_list),
        None,
    )?;
    let bs_req = device.get_buffer_memory_requirements(f.bs_buf);
    f.bs_mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(bs_req.size)
            .memory_type_index(find_mem(
                mem_props,
                bs_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    )?;
    device.bind_buffer_memory(f.bs_buf, f.bs_mem, 0)?;
    // Map once for the slot's lifetime — read_slot copies AUs straight out of this (coherent
    // memory, no per-frame map/unmap); vkFreeMemory implicitly unmaps at teardown.
    f.bs_ptr = BsPtr(
        device.map_memory(f.bs_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *const u8,
    );
    // PUNKTFUNK_PERF: a 2-slot timestamp pool bracketing this slot's compute batch (CSC split).
    if with_ts {
        f.ts_pool = device.create_query_pool(
            &vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2),
            None,
        )?;
    }
    let mut fb_ci = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
        vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
            | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
    );
    fb_ci.p_next = profile as *const _ as *const c_void;
    let mut query_ci = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
        .query_count(1);
    query_ci.p_next = &fb_ci as *const _ as *const c_void;
    f.query_pool = device.create_query_pool(&query_ci, None)?;
    // Command buffers + per-frame sync.
    f.cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(1),
    )?[0];
    f.compute_cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(compute_pool)
            .command_buffer_count(1),
    )?[0];
    f.csc_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
    f.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
    Ok(())
}

/// Author VPS/SPS/PPS (Main, level 4.0, low-latency, conformance-window crop) and return the
/// session-parameters object + the encoded header bytes (VPS+SPS+PPS NALs) for keyframes.
unsafe fn build_parameters_h265(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    venc_dev: &ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
    quality_level: u32,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>)> {
    use ash::vk::native as hh;
    let mut ptl: hh::StdVideoH265ProfileTierLevel = std::mem::zeroed();
    ptl.flags.set_general_progressive_source_flag(1);
    ptl.flags.set_general_frame_only_constraint_flag(1);
    ptl.general_profile_idc = hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN;
    ptl.general_level_idc = hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0;

    let mut dpbm: hh::StdVideoH265DecPicBufMgr = std::mem::zeroed();
    dpbm.max_dec_pic_buffering_minus1[0] = (DPB_SLOTS - 1) as u8;
    dpbm.max_num_reorder_pics[0] = 0;
    dpbm.max_latency_increase_plus1[0] = 0;

    let mut vps: hh::StdVideoH265VideoParameterSet = std::mem::zeroed();
    vps.flags.set_vps_temporal_id_nesting_flag(1);
    vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
    vps.pDecPicBufMgr = &dpbm;
    vps.pProfileTierLevel = &ptl;

    let mut sps: hh::StdVideoH265SequenceParameterSet = std::mem::zeroed();
    sps.flags.set_sps_temporal_id_nesting_flag(1);
    sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
    sps.chroma_format_idc = hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
    sps.pic_width_in_luma_samples = w;
    sps.pic_height_in_luma_samples = h;
    sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
    sps.log2_diff_max_min_luma_coding_block_size = 3;
    sps.log2_diff_max_min_luma_transform_block_size = 3;
    sps.max_transform_hierarchy_depth_inter = 4;
    sps.max_transform_hierarchy_depth_intra = 4;
    sps.pProfileTierLevel = &ptl;
    sps.pDecPicBufMgr = &dpbm;
    if w != rw || h != rh {
        sps.flags.set_conformance_window_flag(1);
        sps.conf_win_right_offset = (w - rw) / 2; // 4:2:0 SubWidthC = 2
        sps.conf_win_bottom_offset = (h - rh) / 2; // 4:2:0 SubHeightC = 2
    }

    let mut pps: hh::StdVideoH265PictureParameterSet = std::mem::zeroed();
    pps.flags.set_cu_qp_delta_enabled_flag(1);
    pps.flags.set_pps_loop_filter_across_slices_enabled_flag(1);

    let vps_arr = [vps];
    let sps_arr = [sps];
    let pps_arr = [pps];
    let add = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
        .std_vp_ss(&vps_arr)
        .std_sp_ss(&sps_arr)
        .std_pp_ss(&pps_arr);
    let mut h265_ci = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
        .max_std_vps_count(1)
        .max_std_sps_count(1)
        .max_std_pps_count(1)
        .parameters_add_info(&add);
    // Bake the session's quality level into the parameters object — the spec requires it to match
    // the level the first frame's ENCODE_QUALITY_LEVEL control installs.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    let ci = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut h265_ci)
        .push_next(&mut q_info);
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters: {r:?}");
    }

    let mut get_h265 = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
        .write_std_vps(true)
        .write_std_sps(true)
        .write_std_pps(true)
        .std_vps_id(0)
        .std_sps_id(0)
        .std_pps_id(0);
    let get = vk::VideoEncodeSessionParametersGetInfoKHR::default()
        .video_session_parameters(params)
        .push_next(&mut get_h265);
    let get_fn = venc_dev.fp().get_encoded_video_session_parameters_khr;
    let mut fb = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default();
    let mut size: usize = 0;
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        std::ptr::null_mut(),
    );
    if r != vk::Result::SUCCESS {
        // `params` is live but not yet the caller's guard's to unwind — destroy before bailing.
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header size: {r:?}");
    }
    let mut buf = vec![0u8; size];
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != vk::Result::SUCCESS {
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header bytes: {r:?}");
    }
    buf.truncate(size);
    Ok((params, buf))
}

/// AV1 low-overhead OBU bit-writer (MSB-first), used to hand-pack the sequence-header OBU that
/// Vulkan AV1 encode (unlike H26x) never emits itself.
struct Av1BitWriter {
    buf: Vec<u8>,
    cur: u8,
    fill: u8,
}
impl Av1BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            fill: 0,
        }
    }
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.fill += 1;
        if self.fill == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.fill = 0;
        }
    }
    fn put(&mut self, val: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.bit((val >> i) & 1);
        }
    }
    /// Flush, zero-padding the final partial byte (OBU size field delimits the payload).
    fn finish(mut self) -> Vec<u8> {
        if self.fill > 0 {
            self.cur <<= 8 - self.fill;
            self.buf.push(self.cur);
        }
        self.buf
    }
}

/// AV1 leb128 (little-endian base-128) encoding of an OBU size.
fn leb128(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
    out
}

/// Bit-pack a `sequence_header_obu` (AV1 spec §5.5) into a size-delimited OBU. The field values here
/// MUST mirror the `StdVideoAV1SequenceHeader` handed to the driver in `build_parameters_av1` so the
/// driver-emitted frame OBUs parse against this header. Single operating point, 8-bit 4:2:0,
/// order-hint on, CDEF+restoration+filter-intra allowed, everything exotic (compound/warp/superres)
/// disabled — the profile our single-reference P-frame encoder actually uses.
fn av1_sequence_header_obu(
    sb128: bool,
    fwb: u32,
    fhb: u32,
    max_w_m1: u32,
    max_h_m1: u32,
    order_hint_bits_minus_1: u32,
    seq_level_idx: u32,
) -> Vec<u8> {
    let mut w = Av1BitWriter::new();
    w.put(0, 3); // seq_profile = MAIN
    w.bit(0); // still_picture
    w.bit(0); // reduced_still_picture_header
    w.bit(0); // timing_info_present_flag
    w.bit(0); // initial_display_delay_present_flag
    w.put(0, 5); // operating_points_cnt_minus_1 = 0
    w.put(0, 12); // operating_point_idc[0]
    w.put(seq_level_idx, 5); // seq_level_idx[0]
    if seq_level_idx > 7 {
        w.bit(0); // seq_tier[0] = 0
    }
    w.put(fwb, 4); // frame_width_bits_minus_1
    w.put(fhb, 4); // frame_height_bits_minus_1
    w.put(max_w_m1, fwb + 1); // max_frame_width_minus_1
    w.put(max_h_m1, fhb + 1); // max_frame_height_minus_1
    w.bit(0); // frame_id_numbers_present_flag
    w.bit(sb128 as u32); // use_128x128_superblock
    w.bit(0); // enable_filter_intra
    w.bit(0); // enable_intra_edge_filter
    w.bit(0); // enable_interintra_compound
    w.bit(0); // enable_masked_compound
    w.bit(0); // enable_warped_motion
    w.bit(0); // enable_dual_filter
    w.bit(1); // enable_order_hint
    w.bit(0); // enable_jnt_comp
    w.bit(0); // enable_ref_frame_mvs
    w.bit(1); // seq_choose_screen_content_tools -> seq_force_screen_content_tools = SELECT
    w.bit(1); // seq_choose_integer_mv -> seq_force_integer_mv = SELECT
    w.put(order_hint_bits_minus_1, 3); // order_hint_bits_minus_1
    w.bit(0); // enable_superres
    w.bit(0); // enable_cdef
    w.bit(0); // enable_restoration
              // color_config(): 8-bit 4:2:0, unspecified primaries/transfer/matrix, limited range
    w.bit(0); // high_bitdepth
    w.bit(0); // mono_chrome
    w.bit(0); // color_description_present_flag
    w.bit(0); // color_range (studio/limited)
    w.put(0, 2); // chroma_sample_position = CSP_UNKNOWN (subsampling_x==subsampling_y==1 for profile 0)
    w.bit(0); // separate_uv_delta_q
    w.bit(0); // film_grain_params_present

    // trailing_bits(): a stop `1` bit then zero-pad to a byte (the size field delimits the OBU, but
    // the parser still requires the trailing_one_bit — dav1d/cbs reject a plain zero pad).
    w.bit(1);
    let payload = w.finish();
    let mut obu = vec![0x0au8]; // obu_header: type=OBU_SEQUENCE_HEADER(1), has_size_field=1
    obu.extend_from_slice(&leb128(payload.len() as u64));
    obu.extend_from_slice(&payload);
    obu
}

/// AV1 session parameters + header framing. Vulkan AV1 encode emits only the per-frame OBU, so we
/// return the app-owned prefixes: a temporal-delimiter OBU that opens every temporal unit
/// (`frame_prefix`), and TD + the bit-packed sequence-header OBU for keyframes (`header`).
#[allow(clippy::too_many_arguments)]
unsafe fn build_parameters_av1(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    _rw: u32,
    _rh: u32,
    max_level: ash::vk::native::StdVideoAV1Level,
    sb128: bool,
    quality_level: u32,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>, Vec<u8>)> {
    use super::vk_av1_encode as av1;
    use ash::vk::native as hh;

    let fwb = 31 - w.leading_zeros(); // av_log2(w): enough bits for max_frame_width_minus_1 = w-1
    let fhb = 31 - h.leading_zeros();
    let order_hint_bits_minus_1: u32 = 7; // OrderHintBits = 8
    let seq_level_idx = max_level; // StdVideoAV1Level's numeric value IS the AV1 seq_level_idx

    // ---- Std sequence header (must match the OBU packed below) ----
    let mut cc_flags: hh::StdVideoAV1ColorConfigFlags = std::mem::zeroed();
    let _ = &mut cc_flags; // all zero: mono_chrome/color_range/description/separate_uv_delta_q = 0
    let mut cc: hh::StdVideoAV1ColorConfig = std::mem::zeroed();
    cc.flags = cc_flags;
    cc.BitDepth = 8;
    cc.subsampling_x = 1;
    cc.subsampling_y = 1;
    cc.color_primaries = hh::StdVideoAV1ColorPrimaries_STD_VIDEO_AV1_COLOR_PRIMARIES_BT_UNSPECIFIED;
    cc.transfer_characteristics =
        hh::StdVideoAV1TransferCharacteristics_STD_VIDEO_AV1_TRANSFER_CHARACTERISTICS_UNSPECIFIED;
    cc.matrix_coefficients =
        hh::StdVideoAV1MatrixCoefficients_STD_VIDEO_AV1_MATRIX_COEFFICIENTS_UNSPECIFIED;
    cc.chroma_sample_position =
        hh::StdVideoAV1ChromaSamplePosition_STD_VIDEO_AV1_CHROMA_SAMPLE_POSITION_UNKNOWN;

    // Match FFmpeg's Vulkan AV1 encoder (proven on this RADV/VCN path): the ONLY coding tools
    // enabled are order-hint and (per caps) 128x128 superblocks. CDEF, loop restoration, filter-
    // intra, warped/compound motion, superres all OFF — enabling them made VCN emit frame-header
    // sections whose bit layout our sequence header didn't match, desyncing every inter frame.
    let mut sh_flags: hh::StdVideoAV1SequenceHeaderFlags = std::mem::zeroed();
    if sb128 {
        sh_flags.set_use_128x128_superblock(1);
    }
    sh_flags.set_enable_order_hint(1);
    let mut sh: hh::StdVideoAV1SequenceHeader = std::mem::zeroed();
    sh.flags = sh_flags;
    sh.seq_profile = hh::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN;
    sh.frame_width_bits_minus_1 = fwb as u8;
    sh.frame_height_bits_minus_1 = fhb as u8;
    sh.max_frame_width_minus_1 = (w - 1) as u16;
    sh.max_frame_height_minus_1 = (h - 1) as u16;
    sh.order_hint_bits_minus_1 = order_hint_bits_minus_1 as u8;
    sh.seq_force_integer_mv = 2; // SELECT
    sh.seq_force_screen_content_tools = 2; // SELECT
    sh.pColorConfig = &cc;

    // ---- single operating point conveying the level/tier the driver targets ----
    let op = av1::StdVideoEncodeAV1OperatingPointInfo {
        flags: std::mem::zeroed(),
        operating_point_idc: 0,
        seq_level_idx: seq_level_idx as u8,
        seq_tier: 0,
        decoder_buffer_delay: 0,
        encoder_buffer_delay: 0,
        initial_display_delay_minus_1: 0,
    };
    let ops = [op];
    let av1_spci = av1::VideoEncodeAV1SessionParametersCreateInfoKHR {
        s_type: av1::stype(av1::ST_SESSION_PARAMETERS_CREATE_INFO),
        p_next: std::ptr::null(),
        p_std_sequence_header: &sh,
        p_std_decoder_model_info: std::ptr::null(),
        std_operating_point_count: 1,
        p_std_operating_points: ops.as_ptr() as *const c_void,
    };
    // Bake the session's quality level into the parameters object (must match the level the first
    // frame's ENCODE_QUALITY_LEVEL control installs); chained raw ahead of the vendored AV1 struct.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    q_info.p_next = &av1_spci as *const _ as *const c_void;
    let mut ci = vk::VideoSessionParametersCreateInfoKHR::default().video_session(session);
    ci.p_next = &q_info as *const _ as *const c_void;
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters (av1): {r:?}");
    }

    // ---- header framing: TD every temporal unit; TD + seq-header OBU on keyframes ----
    let td = vec![0x12u8, 0x00]; // temporal_delimiter OBU (type=2, size=0)
    let seq_obu = av1_sequence_header_obu(
        sb128,
        fwb,
        fhb,
        w - 1,
        h - 1,
        order_hint_bits_minus_1,
        seq_level_idx,
    );
    let mut keyframe_prefix = td.clone();
    keyframe_prefix.extend_from_slice(&seq_obu);
    Ok((params, keyframe_prefix, td))
}

#[cfg(test)]
mod tests {
    use super::{build_h265_rps_s0, pick_recovery_slot, VulkanVideoEncoder};
    use crate::{Codec, Encoder};
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

    /// The RFI anchor picker: newest resident wire strictly older than the loss; empty/newer
    /// slots never qualify.
    #[test]
    fn recovery_slot_picks_newest_pre_loss() {
        // slots hold wires 5..12 (ring position arbitrary); loss starts at 9 → anchor = wire 8.
        let wires = [8i64, 9, 10, 11, 12, 5, 6, 7];
        assert_eq!(pick_recovery_slot(&wires, 9), Some(0));
        // loss older than everything resident → no anchor (caller keyframes).
        assert_eq!(pick_recovery_slot(&wires, 5), None);
        // empty slots (-1) are skipped.
        assert_eq!(pick_recovery_slot(&[-1, 3, -1, 4], 5), Some(3));
        assert_eq!(pick_recovery_slot(&[-1; 8], 5), None);
    }

    /// The taint sweep (fecbec2d's fix, ported here): a slot encoded inside an EARLIER, still
    /// unrepaired loss window must not become the "known-good" anchor of a LATER loss. Without the
    /// sweep, `pick_recovery_slot` accepts it — it is resident and its wire is below the second
    /// loss start — and the frame ships tagged `recovery_anchor`, lifting the client's freeze onto
    /// a reference it never decoded.
    #[test]
    fn taint_sweep_excludes_slots_from_an_earlier_loss() {
        // Apply the sweep exactly as `invalidate_ref_frames` does.
        fn sweep(wires: &mut [i64], loss_first: i64) {
            for w in wires.iter_mut() {
                if *w >= loss_first {
                    *w = -1;
                }
            }
        }

        // Slots hold wires 0..7. Loss 1 starts at wire 4, so wires 4..7 are undecodable at the
        // client. A second loss report arrives at wire 6 while they are all still resident.
        let tainted = [4i64, 5, 6, 7];

        // WITHOUT the sweep this is the bug: the newest wire below 6 is wire 5 — squarely inside
        // loss 1's unrepaired window — and it would be served as the "known-good" anchor.
        let unswept = [0i64, 1, 2, 3, 4, 5, 6, 7];
        let picked = pick_recovery_slot(&unswept, 6).expect("unswept picks something");
        assert!(
            tainted.contains(&unswept[picked]),
            "precondition: without the sweep the anchor comes from the earlier loss window"
        );

        // WITH the sweep, loss 1 blanks 4..7, so loss 2 can only reach genuinely clean wires.
        let mut wires = unswept;
        sweep(&mut wires, 4);
        assert_eq!(wires, [0, 1, 2, 3, -1, -1, -1, -1]);
        let picked = pick_recovery_slot(&wires, 6).expect("clean wires remain");
        assert_eq!(picked, 3, "newest clean survivor is wire 3");
        assert!(!tainted.contains(&wires[picked]));

        // Encoding resumes after recovery; wires 8..11 refill the swept slots and are clean. A
        // later loss at wire 10 legitimately anchors on wire 9 — the sweep must not over-reject.
        wires[4] = 8;
        wires[5] = 9;
        wires[6] = 10;
        wires[7] = 11;
        sweep(&mut wires, 10);
        assert_eq!(
            pick_recovery_slot(&wires, 10),
            Some(5),
            "wire 9 is post-recovery, clean"
        );

        // A loss covering every live wire leaves nothing clean → decline, caller serves an IDR.
        let mut all = [5i64, 6, 7, 8, 9, 10, 11, 12];
        sweep(&mut all, 5);
        assert_eq!(pick_recovery_slot(&all, 5), None);
    }

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
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc = VulkanVideoEncoder::open(codec, w, h, 60, 10_000_000).expect("open");
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
        aus
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
}
