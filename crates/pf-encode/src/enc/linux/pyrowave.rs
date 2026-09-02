//! PyroWave host encoder: intra-only CDF 9/7 wavelet over a private Vulkan 1.3
//! compute device (`pyrowave-sys`). Every frame is a keyframe, so IDR/RFI recovery
//! is unused. Opt-in via `PUNKTFUNK_ENCODER=pyrowave`; no shipping client decodes
//! this until `CODEC_PYROWAVE` negotiation lands.
//!
//! `pyrowave_create_device` retains the original instance/device create-infos for
//! the device's lifetime — [`DeviceHold`] pins them. Frames enter as capture dmabufs
//! (DRM modifiers, cached per buffer) or CPU RGB; `rgb2yuv.comp` writes BT.709-limited
//! R8 luma + RG8 chroma that pyrowave samples via R/G view swizzles. Ingest, CSC and
//! encode record into one command buffer (`pyrowave_device_set_command_buffer`).
//!
//! The AU is one pyrowave packet (boundary = buffer size), `keyframe = true`, through
//! the normal FEC/packetizer path. Evidence: `design/pyrowave-codec-plan.md`.
// `unsafe_op_in_unsafe_fn` off: this file is pyrowave-sys + ash calls. Clearing it
// means deleting markers with no caller contract, not wrapping each call.
#![allow(unsafe_op_in_unsafe_fn)]

// Every unsafe block in this module carries a `// SAFETY:` proof (parent module enforces it).

use super::vk_util::{
    color_range, import_failure_feeds_latch, import_rgb_dmabuf, make_host_buffer, make_plain_image,
    normalize_cpu_rgb, pixel_to_vk,
};
use crate::{EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use ash::vk::Handle as _;
use pf_frame::{CapturedFrame, FramePayload};
use pyrowave_sys as pw;
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::raw::c_char;

/// Shared RGB→(Y, interleaved-UV) BT.709-limited CSC. PyroWave carries no VUI, so
/// the client CSC must assume BT.709 limited range.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// Per-pixel 4:4:4 twin of `CSC_SPV`; same BT.709-limited coefficients.
const CSC444_SPV: &[u8] = include_bytes!("rgb2yuv444.spv");
/// Cursor overlay cap (px). The CSC shader bounds sampling by push constant, so one
/// allocation fits every pointer bitmap.
const CURSOR_MAX: u32 = 256;
/// Max resident dmabuf imports. PipeWire cycles a small fixed pool.
const IMPORT_CACHE_CAP: usize = 16;
/// Headroom over the per-frame rate budget for block headers + meta; the rate
/// controller itself never exceeds the budget.
const BS_SLACK: usize = 256 * 1024;

/// DRM modifiers this device can import as a SAMPLED packed-RGB image. Advertised
/// to capture instead of VAAPI's LINEAR-only policy — tiled dmabufs import via
/// `VK_EXT_image_drm_format_modifier`. Probed per session (instance + PD only).
pub(crate) fn capture_modifiers(fourcc: u32) -> Vec<u64> {
    let Some(fmt) = super::vk_util::fourcc_to_vk(fourcc) else {
        return Vec::new();
    };
    // SAFETY: fresh instance, plain physical-device property queries, destroyed before
    // returning; nothing borrows across the call.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Vec::new();
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return Vec::new();
        };
        // Same selector as `open_inner`: these modifiers are what capture allocates against.
        let pd = select_physical_device(&instance).ok().map(|p| p.pd);
        let mods = pd
            .map(|pd| {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(pd, fmt, &mut fp2);
                let n = list.drm_format_modifier_count as usize;
                let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
                list.p_drm_format_modifier_properties = props.as_mut_ptr();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(pd, fmt, &mut fp2);
                props.truncate(list.drm_format_modifier_count as usize);
                props
                    .into_iter()
                    .filter(|p| {
                        p.drm_format_modifier_tiling_features
                            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                            // Capture hands one fd/offset/stride.
                            && p.drm_format_modifier_plane_count == 1
                    })
                    .map(|p| p.drm_format_modifier)
                    .collect()
            })
            .unwrap_or_default();
        instance.destroy_instance(None);
        mods
    }
}

/// Render node named beside the picked device: `PUNKTFUNK_RENDER_NODE` else
/// `/dev/dri/renderD128`. Log-only — [`select_physical_device`] must not use this.
fn capture_anchor_node() -> std::path::PathBuf {
    pf_gpu::render_node_env().unwrap_or_else(|| std::path::PathBuf::from("/dev/dri/renderD128"))
}

/// `(major, minor)` of a device node in the encoding `VkPhysicalDeviceDrmPropertiesEXT`
/// uses (glibc `gnu_dev_major`/`gnu_dev_minor`).
fn node_rdev(path: &std::path::Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfffu64);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xffu64);
    Some((major as i64, minor as i64))
}

/// Node PCI `(domain, bus, device, function)` from sysfs — fallback when the
/// driver lacks `VK_EXT_physical_device_drm`.
fn node_pci_address(path: &std::path::Path) -> Option<(u32, u32, u32, u32)> {
    let node = path.file_name()?.to_str()?;
    let dev = std::fs::canonicalize(format!("/sys/class/drm/{node}/device")).ok()?;
    let addr = dev.file_name()?.to_str()?;
    let (rest, func) = addr.rsplit_once('.')?;
    let mut parts = rest.split(':');
    let domain = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((domain, bus, device, u32::from_str_radix(func, 16).ok()?))
}

/// Features pyrowave.h requires (shaderInt16, storageBuffer8BitAccess, timeline
/// semaphores, subgroup size control). shaderFloat16 is optional. Checked after
/// selection: folding this into the pick can land on a GPU that cannot import the
/// capturer's buffers and trip the process-wide raw-dmabuf latch. A hard `bail!`
/// is latch-free and the session layer renegotiates.
///
/// # Safety
/// `instance` must be live; issues only physical-device feature queries.
unsafe fn missing_features(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Vec<&'static str> {
    let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut have13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut have2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut have12)
        .push_next(&mut have13);
    instance.get_physical_device_features2(pd, &mut have2);
    [
        (have2.features.shader_int16 == vk::TRUE, "shaderInt16"),
        (
            have12.storage_buffer8_bit_access == vk::TRUE,
            "storageBuffer8BitAccess",
        ),
        (have12.timeline_semaphore == vk::TRUE, "timelineSemaphore"),
        (
            have13.subgroup_size_control == vk::TRUE,
            "subgroupSizeControl",
        ),
        (
            have13.compute_full_subgroups == vk::TRUE,
            "computeFullSubgroups",
        ),
        (have13.synchronization2 == vk::TRUE, "synchronization2"),
    ]
    .iter()
    .filter(|(ok, _)| !ok)
    .map(|(_, n)| *n)
    .collect()
}

/// Whether `pd` owns the anchor render node. DRM render major/minor is primary
/// (disambiguates twin-model GPUs); PCI bus info is the fallback. Neither
/// extension advertised → never matches.
///
/// # Safety
/// `instance` must be live; issues only physical-device property queries.
unsafe fn device_owns_node(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    rdev: Option<(i64, i64)>,
    pci: Option<(u32, u32, u32, u32)>,
) -> bool {
    let exts = instance
        .enumerate_device_extension_properties(pd)
        .unwrap_or_default();
    let has_ext =
        |name: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
    if let Some((major, minor)) = rdev {
        if has_ext(ash::ext::physical_device_drm::NAME) {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(pd, &mut p2);
            return drm.has_render == vk::TRUE
                && drm.render_major == major
                && drm.render_minor == minor;
        }
    }
    if let Some((domain, bus, device, function)) = pci {
        if has_ext(ash::ext::pci_bus_info::NAME) {
            let mut pcip = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut pcip);
            instance.get_physical_device_properties2(pd, &mut p2);
            return pcip.pci_domain == domain
                && pcip.pci_bus == bus
                && pcip.pci_device == device
                && pcip.pci_function == function;
        }
    }
    false
}

struct PickedDevice {
    pd: vk::PhysicalDevice,
    /// Graphics+compute queue family. Pyrowave's device create-info requires graphics;
    /// CSC + codec run on it.
    family: u32,
    vendor_id: u32,
    device_id: u32,
}

/// First non-CPU Vulkan device with a graphics+compute family.
///
/// Do not switch this to `pf_gpu::selected_gpu()`: that picks "the NVIDIA GPU"
/// whenever `/dev/nvidiactl` exists, which on an Intel-compositor + NVIDIA-present
/// laptop is the GPU that cannot import the compositor's dmabufs and trips the
/// process-wide raw-dmabuf latch. Do not anchor on `/dev/dri/renderD128`: render
/// minors are driver bind-order, not display topology (amdgpu binds first → idle
/// iGPU while the compositor allocates on NVIDIA).
///
/// The right oracle is which device allocated the capture buffers; that plumbing
/// is not here. Shared with [`capture_modifiers`] so capture and encode never
/// disagree about the device across an in-place resize that does not renegotiate.
///
/// # Safety
/// `instance` must be live; only physical-device property/queue queries.
unsafe fn select_physical_device(instance: &ash::Instance) -> Result<PickedDevice> {
    for pd in instance.enumerate_physical_devices()? {
        let props = instance.get_physical_device_properties(pd);
        if props.device_type == vk::PhysicalDeviceType::CPU {
            continue;
        }
        let Some(family) = instance
            .get_physical_device_queue_family_properties(pd)
            .iter()
            .position(|q| {
                q.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
        else {
            continue;
        };
        return Ok(PickedDevice {
            pd,
            family: family as u32,
            vendor_id: props.vendor_id,
            device_id: props.device_id,
        });
    }
    Err(anyhow::anyhow!(
        "no Vulkan GPU with a graphics+compute queue"
    ))
}

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

/// Create-infos `pyrowave_create_device` requires to outlive the `pyrowave_device`.
/// Boxes pin heap locations; moving `DeviceHold` moves only the box pointers.
struct DeviceHold {
    _app_info: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _queue_prio: Box<[f32; 1]>,
    _queue_ci: Box<[vk::DeviceQueueCreateInfo<'static>; 1]>,
    /// Global-priority request chained into `_queue_ci[0].p_next`. Boxed because
    /// `pyrowave_create_device` retains `device_ci` and Granite re-reads the chain;
    /// the ladder must write its final state back here (null `p_next` if the
    /// no-priority attempt won). A chain the device was not created with is a lie.
    _queue_gp: Box<[vk::DeviceQueueGlobalPriorityCreateInfoKHR<'static>; 1]>,
    // Vec, not `Box<[_; N]>`: `queue_family_foreign` is pushed conditionally.
    // `as_ptr()` is move-stable like the Boxes.
    _dev_exts: Vec<*const c_char>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

/// Nearest-rank percentile of a sorted sample. The encode spike is a tail event
/// (mean barely moves); a mean-only readout would report "fine".
fn pct(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Global-priority classes for `PYROWAVE_QUEUE_PRIORITY`, character-identical to
/// `patches/0005-global-priority-queue.patch`: unset → realtime ladder;
/// ASCII-lowercased; `off` → none; `high` → `[HIGH]`; junk → `[REALTIME, HIGH]`.
/// The same env var drives Windows (patch live) and Linux (we pass create-infos,
/// Granite takes `inherit_info`). `off` is the only disable spelling; `0` is not.
/// Do not change this without the patch in the same commit.
fn queue_priority_candidates(raw: Option<&str>) -> Vec<vk::QueueGlobalPriorityKHR> {
    let want = raw.map(|s| s.to_ascii_lowercase());
    match want.as_deref() {
        Some("off") => Vec::new(),
        Some("high") => vec![vk::QueueGlobalPriorityKHR::HIGH],
        _ => vec![
            vk::QueueGlobalPriorityKHR::REALTIME,
            vk::QueueGlobalPriorityKHR::HIGH,
        ],
    }
}

/// `create_device` error that means "this priority class was refused" — walk the
/// ladder down rather than failing the open. `ERROR_NOT_PERMITTED_KHR` is specified;
/// `ERROR_INITIALIZATION_FAILED` matches pf-zerocopy's VkBridge. A PyroWave open is
/// a negotiated session, so a hard error here is a dead stream.
fn priority_refused(e: vk::Result) -> bool {
    matches!(
        e,
        vk::Result::ERROR_NOT_PERMITTED_KHR | vk::Result::ERROR_INITIALIZATION_FAILED
    )
}

/// Independent per-frame resource sets. Two: Granite's device defaults to
/// `init_frame_contexts(2)` and `next_frame_context()` waits the context it rotates
/// into, so frame N may not begin until N-2 completed. A third slot needs a
/// vendored `init_frame_contexts(3)`, which is not exposed. `max_inflight` (still 1)
/// is how many are live; this is capacity only.
const SLOTS: usize = 2;

/// Exclusive resources for one in-flight frame. Overlap on a shared copy is a
/// correctness bug: `csc_set` rewritten while still PENDING
/// (VUID-vkUpdateDescriptorSets-None-03047), CSC of N+1 writing images N is sampling,
/// cursor host-write racing N's sampled read, recording into a PENDING `cmd`, CPU
/// staging written while N's copy is pending. `bitstream` stays shared (packetize is
/// poll-side, one frame at a time). `import_cache` retains `VkImage`/`VkDeviceMemory`
/// per dmabuf inode so dropping a `CapturedFrame` is not a use-after-free.
struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    csc_set: vk::DescriptorSet,
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    cursor_img: vk::Image,
    cursor_mem: vk::DeviceMemory,
    cursor_view: vk::ImageView,
    cursor_stage: vk::Buffer,
    cursor_stage_mem: vk::DeviceMemory,
    /// Per-slot: a bitmap change uploads once per slot. A global serial would leave
    /// the other slot showing the previous pointer.
    cursor_serial: u64,
    cursor_ready: bool,
    /// CPU-input staging, lazily (re)created on format change.
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
}

impl Slot {
    /// All-null so `Drop` on a partially-built slot is sound: `vkDestroy*` of
    /// `VK_NULL_HANDLE` is the spec no-op.
    fn null() -> Self {
        Self {
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            csc_set: vk::DescriptorSet::null(),
            y_img: vk::Image::null(),
            y_mem: vk::DeviceMemory::null(),
            y_view: vk::ImageView::null(),
            uv_img: vk::Image::null(),
            uv_mem: vk::DeviceMemory::null(),
            uv_view: vk::ImageView::null(),
            cursor_img: vk::Image::null(),
            cursor_mem: vk::DeviceMemory::null(),
            cursor_view: vk::ImageView::null(),
            cursor_stage: vk::Buffer::null(),
            cursor_stage_mem: vk::DeviceMemory::null(),
            cursor_serial: u64::MAX,
            cursor_ready: false,
            cpu_img: None,
            cpu_stage: None,
        }
    }
}

/// One submitted frame whose fence has not been waited and whose bitstream has
/// not been packetized yet.
#[derive(Clone, Copy)]
struct InFlight {
    /// Slot this frame owns. Carried, not recomputed: waiting the wrong fence looks
    /// like corruption, not an error.
    slot: usize,
    /// Capture timestamp for this AU. The `CapturedFrame` is the caller's and is
    /// gone by packetize.
    pts_ns: u64,
    /// Bitstream cap this frame was encoded against (`frame_budget + BS_SLACK`).
    /// Snapshotted at submit: `reconfigure_bitrate` can land before poll, and in
    /// dense mode the boundary is this number — a shrunk budget would make
    /// `compute_num_packets` return more than one packet.
    cap: usize,
    /// Sequence stamped into this frame's block headers (`wait_and_packetize` check).
    seq: u8,
    /// Datagram alignment this frame was encoded for. `set_wire_chunking` can land
    /// mid-flight; packetizing at a different boundary would mis-set `chunk_aligned`.
    wire_chunk: Option<usize>,
    /// When `submit` started. The summary measures submit→AU, not just the wait.
    t0: std::time::Instant,
}

pub struct PyroWaveEncoder {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    family: u32,
    /// `src` family for the fresh-dmabuf acquire barrier: FOREIGN when the extension
    /// is enabled, else the core EXTERNAL substitute.
    foreign_qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    _hold: DeviceHold,

    // Destroyed before the VkDevice they borrow.
    pw_dev: pw::pyrowave_device,
    /// One `pyrowave_encoder` per [`Slot`]. `Encoder::Impl` owns one wavelet/scratch
    /// set and `Impl::encode` opens by discarding it (`UNDEFINED` old layout + buffer
    /// fills). Two encodes on one handle have no Vulkan execution dependency, so N+1
    /// would overwrite bands N is still packing. Overlap is two handles; within a
    /// handle, encodes stay serialized so patch 0004's scratch-pool stays intact.
    pw_encs: Vec<pw::pyrowave_encoder>,
    /// Wire sequence, kept here not in the encoder objects. Each handle has its own
    /// `sequence_count`, so alternating them emits 1,1,2,2… The decoder restarts a
    /// frame only when the value changes, so a repeat is swallowed as more blocks of
    /// the same frame. `patches/0007-encoder-sequence-override.patch` stamps this
    /// counter regardless of which handle encodes.
    wire_seq: u32,

    // Shared CSC pipeline + sampler: immutable once built, read-only while recording.
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,

    // Per-buffer dmabuf-import cache keyed by (st_dev, st_ino). Not per-slot: it
    // retains the VkImage/VkDeviceMemory per inode, which is what makes two slots
    // sampling the same imported buffer safe.
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,
    /// 3→4 expansion for 24-bpp CPU payloads. Consumed inside `submit_frame` before
    /// return, so no GPU work reads it.
    cpu_expand: Vec<u8>,

    cmd_pool: vk::CommandPool,
    slots: Vec<Slot>,
    next_slot: usize,
    /// Submitted frames whose fence has not been waited. `reset()` keys its bounded
    /// wait on this being non-empty: a never-submitted fence starts unsignaled and
    /// would read as wedged.
    inflight: VecDeque<InFlight>,
    /// Submitted-but-not-polled cap. Still 1: a pyrowave `Encoder` cannot hold two
    /// frames (single wavelet/scratch; `Impl::encode` discards with UNDEFINED). Never
    /// exceeds `SLOTS`.
    max_inflight: usize,

    width: u32,
    height: u32,
    fps: u32,
    /// Session chroma: 4:4:4 = full-res RG8 + per-pixel CSC + `Chroma444` objects.
    chroma444: bool,
    /// Ladder outcome, reported to the host (the process that owns the log pipeline).
    priority: super::worker::PriorityOutcome,
    /// Opened `deviceName`. On a multi-GPU host the worker's GPU is otherwise invisible.
    device_name: String,
    /// Per-frame bitstream budget (hard CBR): `bitrate / (8 * fps)`.
    frame_budget: usize,
    /// `PUNKTFUNK_PERF` reservoir of submit→AU durations. Other backends already log
    /// a submit split; this is the number the priority lever exists to protect.
    perf_us: Vec<u32>,
    perf_logged_at: Option<std::time::Instant>,
    /// Datagram-aligned packetize boundary; packets pad to it so each shard carries
    /// whole self-delimiting packets. `None` = one packet per AU.
    wire_chunk: Option<usize>,
    /// Windowing inflation → rate-budget deflation so the pin holds on the wire.
    wire_budget: crate::pyrowave_wire::WireBudget,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
    /// AU being handed out in streamed chunks (`Some` between `first` and `last`).
    /// Encode is synchronous, so the AU is complete before the first chunk leaves.
    chunker: Option<crate::pyrowave_wire::AuChunker>,
    frame_count: u64,
}

// SAFETY: encode thread only; Vulkan handles are owned and never shared. Pyrowave
// handles are touched from that thread, and it only submits GPU work inside our API calls.
unsafe impl Send for PyroWaveEncoder {}

fn budget_for(bitrate_bps: u64, fps: u32) -> usize {
    ((bitrate_bps / (8 * fps.max(1) as u64)) as usize).max(64 * 1024)
}

impl PyroWaveEncoder {
    /// `PUNKTFUNK_PERF`: record one encode duration and summarise on a slow cadence.
    /// Sample is stamped when `submit` starts and taken when the AU is readable
    /// (CSC + encode + fence wait + packetize). At depth > 1 this grows by about one
    /// loop period: N's AU is not retrieved until after N+1 has been submitted.
    fn note_encode_us(&mut self, us: u32) {
        if !pf_host_config::config().perf {
            return;
        }
        self.perf_us.push(us);
        let now = std::time::Instant::now();
        let since = self.perf_logged_at.map(|t| now.duration_since(t));
        // 2 s matches the other backends' submit-split cadence; 30 samples so p99 means something.
        if self.perf_us.len() < 30 || since.is_some_and(|d| d.as_secs() < 2) {
            if self.perf_logged_at.is_none() {
                self.perf_logged_at = Some(now);
            }
            return;
        }
        self.perf_logged_at = Some(now);
        let mut s = std::mem::take(&mut self.perf_us);
        s.sort_unstable();
        let n = s.len() as u64;
        let mean = s.iter().map(|&v| u64::from(v)).sum::<u64>() / n.max(1);
        tracing::info!(
            frames = n,
            mean_us = mean,
            p50_us = pct(&s, 0.50),
            p99_us = pct(&s, 0.99),
            max_us = *s.last().unwrap_or(&0),
            depth = self.max_inflight,
            "pyrowave encode, submit->AU (CSC + encode + fence wait + packetize). Under a \
             GPU-bound game this is the number the global-priority queue exists to protect — \
             watch p99, not the mean. At depth > 1 it includes one loop period of pipelining"
        );
    }

    /// Ladder outcome the worker reports so the host can log the grant (or refusal)
    /// once, naming the right binary.
    pub(crate) fn priority_outcome(&self) -> super::worker::PriorityOutcome {
        self.priority
    }

    pub(crate) fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma: crate::ChromaFormat,
    ) -> Result<Self> {
        // In-process path reads intent from its own environment and owns the inert warn.
        let intent = std::env::var("PYROWAVE_QUEUE_PRIORITY").ok();
        Self::open_checked(
            width,
            height,
            fps,
            bitrate_bps,
            chroma.is_444(),
            intent.as_deref(),
            true,
        )
    }

    /// [`Self::open`] as `punktfunk-encode-worker` runs it. Same open path; two
    /// differences: `intent` comes from the host handshake (the worker strips
    /// `PYROWAVE_QUEUE_PRIORITY` at startup), and the inert warn is left to the host
    /// so it names the worker binary, not the host.
    pub(crate) fn open_in_worker(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        intent: Option<&str>,
    ) -> Result<Self> {
        Self::open_checked(width, height, fps, bitrate_bps, chroma444, intent, false)
    }

    fn open_checked(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        intent: Option<&str>,
        warn_inert: bool,
    ) -> Result<Self> {
        if !chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // Against the chroma actually being opened, not hardcoded 4:4:4. 4:2:0 block
        // count is still unbounded (8192×6144 4:2:0 = 73728 > u16::MAX), and a 4:4:4 →
        // 4:2:0 downgrade would skip a `chroma.is_444()`-gated check. Wrapping the index
        // lets resolve over-credit and `packetize` overshoot (Release strips the assert).
        if !crate::pyrowave_mode_fits_rdo(width, height, chroma444) {
            bail!(
                "pyrowave {} at {width}x{height} exceeds the rate controller's 16-bit block \
                 index (see pyrowave-sys patches/0002 note) — lower the resolution",
                if chroma444 { "4:4:4" } else { "4:2:0" }
            );
        }
        // SAFETY: `open_inner` only issues Vulkan/pyrowave calls whose preconditions it
        // establishes (valid instance/device, create-infos `DeviceHold` keeps alive);
        // all handles are freshly created and owned by the result.
        unsafe {
            Self::open_inner(
                width,
                height,
                fps.max(1),
                bitrate_bps.max(1_000_000),
                chroma444,
                intent,
                warn_inert,
            )
        }
    }

    /// `intent` is the raw `PYROWAVE_QUEUE_PRIORITY` (`None` = default ladder), resolved
    /// by the caller. `warn_inert` is whether this process emits the "every class refused"
    /// warning — see [`Self::open_in_worker`].
    #[allow(clippy::too_many_arguments)]
    unsafe fn open_inner(
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u64,
        chroma444: bool,
        intent: Option<&str>,
        warn_inert: bool,
    ) -> Result<Self> {
        let entry = ash::Entry::load().context("load vulkan loader")?;

        let mut hold = DeviceHold {
            _app_info: Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3)),
            instance_ci: Box::new(vk::InstanceCreateInfo::default()),
            _queue_prio: Box::new([1.0f32]),
            _queue_ci: Box::new([vk::DeviceQueueCreateInfo::default()]),
            _queue_gp: Box::new([vk::DeviceQueueGlobalPriorityCreateInfoKHR::default()]),
            _dev_exts: vec![
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ],
            _feat2: Box::new(vk::PhysicalDeviceFeatures2::default()),
            _v12: Box::new(vk::PhysicalDeviceVulkan12Features::default()),
            _v13: Box::new(vk::PhysicalDeviceVulkan13Features::default()),
            device_ci: Box::new(vk::DeviceCreateInfo::default()),
        };
        hold.instance_ci.p_application_info = &*hold._app_info;
        let instance = entry
            .create_instance(&hold.instance_ci, None)
            .context("create instance")?;

        // Between `create_instance` and `create_device` the only live resource is the
        // instance: one fallible block, one destroy on error. From the device on, `Drop`
        // is the unwind path.

        // SAFETY: physical-device queries on the live instance; `create_device` create-infos
        // are pinned in `hold`. Retries only change `global_priority` and, on the last
        // attempt, `_queue_ci[0].p_next`. Both live in `hold`'s Boxes, so `device_ci`
        // pointers stay valid; a failed `create_device` does not consume its create-info.
        let selected = (|| unsafe {
            // Same selector as `capture_modifiers` so the two never disagree about the device.
            let picked = select_physical_device(&instance)?;
            let (pd, family) = (picked.pd, picked.family);
            // Log-only: picked device beside the two guesses. A mismatch is not evidence of a
            // wrong pick (loader first-device vs renderD128 disagree on hybrid hosts), so no
            // arm is a WARN.
            let anchor = capture_anchor_node();
            let anchor_owner = {
                let rdev = node_rdev(&anchor);
                let pci = node_pci_address(&anchor);
                if rdev.is_none() && pci.is_none() {
                    "unresolved".to_string()
                } else {
                    instance
                        .enumerate_physical_devices()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|&o| device_owns_node(&instance, o, rdev, pci))
                        .map(|o| {
                            let p = instance.get_physical_device_properties(o);
                            format!("{:04x}:{:04x}", p.vendor_id, p.device_id)
                        })
                        .unwrap_or_else(|| "unmatched".to_string())
                }
            };
            let selected_gpu = pf_gpu::selected_gpu()
                .map(|s| format!("{:04x}:{:04x}", s.info.vendor_id, s.info.device_id))
                .unwrap_or_else(|| "none".to_string());
            tracing::info!(
                vendor_id = format_args!("{:04x}", picked.vendor_id),
                device_id = format_args!("{:04x}", picked.device_id),
                anchor = %anchor.display(),
                anchor_owner = %anchor_owner,
                selected_gpu = %selected_gpu,
                "pyrowave: encoding on the first usable Vulkan GPU (a wrong-device report on a \
                 multi-GPU host needs these fields)"
            );

            // Pyrowave's documented encoder requirements; the re-query below mirrors optionals
            // (shaderFloat16, vulkanMemoryModel, maintenance4) into the create-info.
            let missing = missing_features(&instance, pd);
            if !missing.is_empty() {
                bail!("GPU lacks pyrowave-required Vulkan features: {missing:?}");
            }
            let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
            let mut have13 = vk::PhysicalDeviceVulkan13Features::default();
            let mut have2 = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut have12)
                .push_next(&mut have13);
            instance.get_physical_device_features2(pd, &mut have2);

            hold._feat2.features.shader_int16 = vk::TRUE;
            hold._v12.storage_buffer8_bit_access = vk::TRUE;
            hold._v12.timeline_semaphore = vk::TRUE;
            hold._v12.shader_float16 = have12.shader_float16;
            hold._v12.vulkan_memory_model = have12.vulkan_memory_model;
            hold._v12.vulkan_memory_model_device_scope = have12.vulkan_memory_model_device_scope;
            hold._v13.subgroup_size_control = vk::TRUE;
            hold._v13.compute_full_subgroups = vk::TRUE;
            hold._v13.synchronization2 = vk::TRUE;
            hold._v13.maintenance4 = have13.maintenance4;
            hold._feat2.p_next = &mut *hold._v12 as *mut _ as *mut std::ffi::c_void;
            hold._v12.p_next = &mut *hold._v13 as *mut _ as *mut std::ffi::c_void;

            // Fresh-import acquire names FOREIGN as src when advertised, else
            // QUEUE_FAMILY_EXTERNAL. Push before the count/as_ptr wiring below.
            let dev_ext_props = instance
                .enumerate_device_extension_properties(pd)
                .unwrap_or_default();
            let foreign_qfi = if crate::vk_util::ext_advertised(
                &dev_ext_props,
                ash::ext::queue_family_foreign::NAME,
            ) {
                hold._dev_exts
                    .push(ash::ext::queue_family_foreign::NAME.as_ptr());
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                tracing::warn!(
                    "pyrowave: VK_EXT_queue_family_foreign not advertised — dmabuf acquires \
                     use the core QUEUE_FAMILY_EXTERNAL substitute (no fleet hardware takes \
                     this arm; report it)"
                );
                vk::QUEUE_FAMILY_EXTERNAL
            };
            // Encode shares shader cores with the game; process priority only orders
            // submission. The vendored patch is gated `if (!inherit_info)` and Linux
            // passes its own create-infos, so Granite takes the inherit branch.
            let gp_candidates = queue_priority_candidates(intent);
            // KHR is the promoted name; match pf-zerocopy's VkBridge probe so spellings agree.
            let gp_ext =
                if crate::vk_util::ext_advertised(&dev_ext_props, vk::KHR_GLOBAL_PRIORITY_NAME) {
                    Some(vk::KHR_GLOBAL_PRIORITY_NAME)
                } else if crate::vk_util::ext_advertised(
                    &dev_ext_props,
                    vk::EXT_GLOBAL_PRIORITY_NAME,
                ) {
                    Some(vk::EXT_GLOBAL_PRIORITY_NAME)
                } else {
                    None
                };
            let gp = gp_ext.filter(|_| !gp_candidates.is_empty());
            if let Some(name) = gp {
                hold._dev_exts.push(name.as_ptr());
            }

            hold._queue_ci[0] = vk::DeviceQueueCreateInfo::default().queue_family_index(family);
            hold._queue_ci[0].queue_count = 1;
            hold._queue_ci[0].p_queue_priorities = hold._queue_prio.as_ptr();
            if gp.is_some() {
                hold._queue_ci[0].p_next = &*hold._queue_gp as *const _ as *const std::ffi::c_void;
            }
            hold.device_ci.p_next = &*hold._feat2 as *const _ as *const std::ffi::c_void;
            hold.device_ci.queue_create_info_count = 1;
            hold.device_ci.p_queue_create_infos = hold._queue_ci.as_ptr();
            hold.device_ci.enabled_extension_count = hold._dev_exts.len() as u32;
            hold.device_ci.pp_enabled_extension_names = hold._dev_exts.as_ptr();

            // Try each class; step down only on a refusal; if every class is refused, create
            // with no global priority. A refused class must not fail the open: this path is a
            // negotiated PyroWave session, so a hard error is a dead stream.
            let mut chosen = None;
            let mut device = None;
            for want in &gp_candidates {
                if gp.is_none() {
                    break;
                }
                hold._queue_gp[0].global_priority = *want;
                match instance.create_device(pd, &hold.device_ci, None) {
                    Ok(d) => {
                        chosen = Some(*want);
                        device = Some(d);
                        break;
                    }
                    Err(e) if priority_refused(e) => {
                        tracing::debug!(
                            priority = ?want,
                            error = ?e,
                            "pyrowave: global queue priority not permitted — downgrading"
                        );
                    }
                    Err(e) => {
                        return Err(e).context("create device");
                    }
                }
            }
            let device = match device {
                Some(d) => {
                    tracing::info!(
                        priority = ?chosen,
                        ext = ?gp,
                        "pyrowave: elevated global queue priority (the encode dispatch preempts a \
                         GPU-bound game where the driver honors it)"
                    );
                    d
                }
                None => {
                    // Nothing requested, or every class refused. The retained create-info must
                    // describe a device created without a priority chain: Granite re-reads it
                    // via `get_existing_create_info()`. The extension stays enabled (it is on
                    // the device); only the request is dropped.
                    hold._queue_ci[0].p_next = std::ptr::null();
                    if !gp_candidates.is_empty() && gp.is_some() && warn_inert {
                        // Unprivileged hosts are refused every class; `cap_sys_nice+ep` is
                        // granted REALTIME. The worker reports the outcome to its parent, which
                        // logs naming the worker binary — do not `setcap` the host.
                        tracing::warn!(
                            "pyrowave: every global queue priority class was refused — encoding \
                             at default priority. The GPU-preemption lever is INERT without \
                             CAP_SYS_NICE on the host binary (measured on both NVIDIA and RADV); \
                             PYROWAVE_QUEUE_PRIORITY=off silences this"
                        );
                    }
                    instance
                        .create_device(pd, &hold.device_ci, None)
                        .context("create device")?
                }
            };
            // Candidates are only REALTIME or HIGH, so `Some(_)` is High (ash models the
            // class as a newtype, not a Rust enum).
            let priority = match chosen {
                Some(c) if c == vk::QueueGlobalPriorityKHR::REALTIME => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::Realtime)
                }
                Some(_) => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::High)
                }
                None if !gp_candidates.is_empty() && gp.is_some() => {
                    super::worker::PriorityOutcome::Refused
                }
                None => super::worker::PriorityOutcome::NotRequested,
            };
            let device_name = instance
                .get_physical_device_properties(pd)
                .device_name_as_c_str()
                .ok()
                .and_then(|s| s.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            Ok((pd, family, device, foreign_qfi, priority, device_name))
        })();
        let (pd, family, device, foreign_qfi, priority, device_name) = match selected {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        let queue = device.get_device_queue(family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // Construct `Self` now with later resources null and assign as they come up. Any `?`
        // drops `me`; `Drop` tears down the prefix: idle first, null-guard `pw_encs`
        // (`pyrowave_encoder_destroy` dereferences), `pyrowave_device_destroy(null)` is
        // `delete nullptr`, and `vkDestroy*` of VK_NULL_HANDLE is a spec no-op.
        let mut me = Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            queue,
            family,
            foreign_qfi,
            mem_props,
            _hold: hold,
            pw_dev: std::ptr::null_mut(),
            pw_encs: vec![std::ptr::null_mut(); SLOTS],
            wire_seq: 0,
            csc_pipe: vk::Pipeline::null(),
            csc_layout: vk::PipelineLayout::null(),
            csc_dsl: vk::DescriptorSetLayout::null(),
            csc_pool: vk::DescriptorPool::null(),
            sampler: vk::Sampler::null(),
            import_cache: Vec::new(),
            cpu_expand: Vec::new(),
            cmd_pool: vk::CommandPool::null(),
            slots: (0..SLOTS).map(|_| Slot::null()).collect(),
            next_slot: 0,
            inflight: VecDeque::new(),
            max_inflight: 1,
            width: w,
            height: h,
            fps,
            chroma444,
            priority,
            device_name,
            frame_budget: budget_for(bitrate, fps),
            perf_us: Vec::new(),
            perf_logged_at: None,
            wire_chunk: None,
            wire_budget: crate::pyrowave_wire::WireBudget::new(),
            bitstream: Vec::new(),
            pending: VecDeque::new(),
            chunker: None,
            frame_count: 0,
        };

        // Create-infos stay pinned in `me._hold`: pyrowave retains the pointers for the
        // device's lifetime, and the Boxes' heap data does not move when `Self` does.
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: me.queue.as_raw() as pw::VkQueue,
            familyIndex: family,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            // SAFETY: ash's loader entry and bindgen's PFN are the same C function pointer.
            GetInstanceProcAddr: Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    ash::vk::Instance,
                    *const c_char,
                ) -> Option<unsafe extern "system" fn()>,
                unsafe extern "C" fn(pw::VkInstance, *const c_char) -> pw::PFN_vkVoidFunction,
            >(me._entry.static_fn().get_instance_proc_addr)),
            instance: me.instance.handle().as_raw() as usize as pw::VkInstance,
            physical_device: pd.as_raw() as usize as pw::VkPhysicalDevice,
            device: me.device.handle().as_raw() as usize as pw::VkDevice,
            instance_create_info: &*me._hold.instance_ci as *const vk::InstanceCreateInfo
                as *const pw::VkInstanceCreateInfo,
            device_create_info: &*me._hold.device_ci as *const vk::DeviceCreateInfo
                as *const pw::VkDeviceCreateInfo,
            queue_info: &mut queue_info,
            queue_info_count: 1,
            // Encode thread only; pyrowave submits only inside our API calls — no lock.
            queue_lock_callback: None,
            queue_unlock_callback: None,
            userdata: std::ptr::null_mut(),
        };
        pw_check(
            pw::pyrowave_create_device(&create, &mut me.pw_dev),
            "create_device",
        )?;
        let _ =
            pw::pyrowave_device_set_queue_type(me.pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT);

        let einfo = pw::pyrowave_encoder_create_info {
            device: me.pw_dev,
            width: w as i32,
            height: h as i32,
            chroma: if chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
        };
        for i in 0..SLOTS {
            pw_check(
                pw::pyrowave_encoder_create(&einfo, &mut me.pw_encs[i]),
                "encoder_create",
            )?;
        }

        let device = me.device.clone(); // fn-table clone; lets `me.*` assignments interleave
        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };

        me.sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(if chroma444 {
            CSC444_SPV
        } else {
            CSC_SPV
        }))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
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
            sb(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
        ];
        me.csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [me.csc_dsl];
        // Cursor {ivec2 origin, ivec2 size} = 16 bytes; matches the shared CSC shader.
        let pc_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(16)];
        me.csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&dsls)
                .push_constant_ranges(&pc_ranges),
            None,
        )?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let pipe_res = device.create_compute_pipelines(
            vk::PipelineCache::null(),
            &[vk::ComputePipelineCreateInfo::default()
                .layout(me.csc_layout)
                .stage(stage)],
            None,
        );
        // Destroy the module before `?`ing: it lives in no field. On failure the batch-of-1
        // out array is all VK_NULL_HANDLE; a multi-entry batch could not assume that.
        device.destroy_shader_module(shader, None);
        me.csc_pipe = pipe_res.map_err(|(_, e)| e)?[0];

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(2 * SLOTS as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * SLOTS as u32),
        ];
        me.csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(SLOTS as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        me.cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;

        // One complete `Slot` per iteration; a mid-loop failure leaves earlier slots formed
        // and the rest null — `Drop` handles VK_NULL_HANDLE.
        for i in 0..SLOTS {
            let (y_img, y_mem, y_view) = make_plain_image(
                &device,
                &me.mem_props,
                vk::Format::R8_UNORM,
                w,
                h,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            me.slots[i].y_img = y_img;
            me.slots[i].y_mem = y_mem;
            me.slots[i].y_view = y_view;
            let (uv_img, uv_mem, uv_view) = make_plain_image(
                &device,
                &me.mem_props,
                vk::Format::R8G8_UNORM,
                cw,
                ch,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            me.slots[i].uv_img = uv_img;
            me.slots[i].uv_mem = uv_mem;
            me.slots[i].uv_view = uv_view;
            let (cursor_img, cursor_mem, cursor_view) = make_plain_image(
                &device,
                &me.mem_props,
                vk::Format::R8G8B8A8_UNORM,
                CURSOR_MAX,
                CURSOR_MAX,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            me.slots[i].cursor_img = cursor_img;
            me.slots[i].cursor_mem = cursor_mem;
            me.slots[i].cursor_view = cursor_view;
            let (cursor_stage, cursor_stage_mem) = make_host_buffer(
                &device,
                &me.mem_props,
                (CURSOR_MAX * CURSOR_MAX * 4) as u64,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            me.slots[i].cursor_stage = cursor_stage;
            me.slots[i].cursor_stage_mem = cursor_stage_mem;
            let csc_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(me.csc_pool)
                    .set_layouts(&dsls),
            )?[0];
            me.slots[i].csc_set = csc_set;
            // Bindings 1–3 are fixed for the slot's life; only binding 0 is rewritten per
            // frame — that is why each slot needs its own set.
            let yi = [vk::DescriptorImageInfo::default()
                .image_view(y_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let uvi = [vk::DescriptorImageInfo::default()
                .image_view(uv_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let curi = [vk::DescriptorImageInfo::default()
                .sampler(me.sampler)
                .image_view(cursor_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&yi),
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&uvi),
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&curi),
                ],
                &[],
            );
            me.slots[i].cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(me.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            me.slots[i].fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
        }

        // Driver-reported slot size (not an estimate). CPU staging is excluded: lazy, software
        // capture only.
        let slot_bytes: u64 = [
            me.slots[0].y_img,
            me.slots[0].uv_img,
            me.slots[0].cursor_img,
        ]
        .iter()
        .map(|&i| device.get_image_memory_requirements(i).size)
        .sum::<u64>()
            + device
                .get_buffer_memory_requirements(me.slots[0].cursor_stage)
                .size;

        let props = me.instance.get_physical_device_properties(pd);
        tracing::info!(
            gpu = %props.device_name_as_c_str().unwrap_or(c"?").to_string_lossy(),
            mode = %format!("{w}x{h}@{fps}"),
            budget_kib = me.frame_budget / 1024,
            chroma = if chroma444 { "4:4:4" } else { "4:2:0" },
            slots = SLOTS,
            slot_kib = slot_bytes / 1024,
            slots_kib = slot_bytes * SLOTS as u64 / 1024,
            "PyroWave encoder open (intra-only wavelet, BT.709 limited)"
        );

        Ok(me)
    }

    /// Point slot `slot`'s CSC binding 0 at this frame's RGB view.
    /// Writing a set still bound by a PENDING command buffer violates
    /// VUID-vkUpdateDescriptorSets-None-03047. Safe here: this slot's previous frame
    /// was retired before `submit` chose it.
    unsafe fn bind_rgb(&self, slot: usize, rgb_view: vk::ImageView) {
        let ii = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(self.slots[slot].csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii)],
            &[],
        );
    }

    /// Bring the cursor image up to date and return `[origin_x, origin_y, size_w, size_h]`
    /// (size 0 ⇒ CSC skips the blend). Upload only when `serial` changed. Per-slot:
    /// a shared image races the previous frame's sampled read.
    unsafe fn prep_cursor(
        &mut self,
        slot: usize,
        cursor: Option<&pf_frame::CursorOverlay>,
    ) -> Result<[i32; 4]> {
        let dev = self.device.clone();
        let cmd = self.slots[slot].cmd;
        let img = self.slots[slot].cursor_img;
        let stage = self.slots[slot].cursor_stage;
        let stage_mem = self.slots[slot].cursor_stage_mem;
        let ready = self.slots[slot].cursor_ready;
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
                if self.slots[slot].cursor_serial != c.serial {
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
                        cmd,
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
                        cmd,
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
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.slots[slot].cursor_serial = c.serial;
                    self.slots[slot].cursor_ready = true;
                }
                Ok([c.x, c.y, cw as i32, ch as i32])
            }
            _ => {
                if !ready {
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.slots[slot].cursor_ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Per-buffer dmabuf import cache; same policy as `vulkan_video.rs`.
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
            (u64::MAX, self.frame_count)
        };
        if let Some(&(_, _, img, _, view)) = self.import_cache.iter().find(|e| (e.0, e.1) == key) {
            return Ok((img, view, false));
        }
        // Feed pf-zerocopy's raw-dmabuf latch: a driver that refuses compositor buffers
        // refuses them forever, and only the latch (CPU capture next session) recovers.
        // Transient OOM is excluded (`import_failure_feeds_latch`).
        let (img, mem, view) =
            match import_rgb_dmabuf(&self.device, &self.ext_fd, &self.mem_props, d, cw, ch) {
                Ok(t) => {
                    pf_zerocopy::note_raw_dmabuf_import_ok();
                    t
                }
                Err(e) => {
                    if import_failure_feeds_latch(&e) {
                        pf_zerocopy::note_raw_dmabuf_import_failure(&format!("{e:#}"));
                    }
                    return Err(e);
                }
            };
        while self.import_cache.len() >= IMPORT_CACHE_CAP {
            let (_, _, oi, om, ov) = self.import_cache.remove(0);
            self.device.destroy_image_view(ov, None);
            self.device.destroy_image(oi, None);
            self.device.free_memory(om, None);
        }
        self.import_cache.push((key.0, key.1, img, mem, view));
        tracing::debug!(
            resident = self.import_cache.len(),
            "pyrowave: imported a new dmabuf buffer"
        );
        Ok((img, view, true))
    }

    /// CPU RGB staging. Per-slot: a host write while the previous frame's copy is
    /// still pending would race that copy.
    unsafe fn ensure_cpu_rgb(
        &mut self,
        slot: usize,
        fmt: vk::Format,
        bytes: &[u8],
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        let need = (w * h * 4) as u64;
        if self.slots[slot].cpu_img.map(|(_, _, _, f)| f) != Some(fmt) {
            if let Some((i, m, v, _)) = self.slots[slot].cpu_img.take() {
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
            self.slots[slot].cpu_img = Some((i, m, v, fmt));
        }
        if self.slots[slot]
            .cpu_stage
            .map(|(_, _, s)| s < need)
            .unwrap_or(true)
        {
            if let Some((b, m, _)) = self.slots[slot].cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let (buf, mem) = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            self.slots[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.slots[slot].cpu_stage.unwrap();
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        let n = bytes.len().min(need as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        dev.unmap_memory(m);
        Ok(self.slots[slot].cpu_img.unwrap().2)
    }

    /// Per-frame rate-control budget: `frame_budget`, deflated by windowing inflation
    /// when datagram-aligned. The pin is the wire, not the raw bitstream.
    fn rate_budget(&self) -> usize {
        match self.wire_chunk {
            Some(_) => self.wire_budget.deflate(self.frame_budget).max(64 * 1024),
            None => self.frame_budget,
        }
    }

    /// Ingest → CSC → encode, recorded into our command buffer → queue-submit → return.
    /// Fence wait and packetize are [`wait_and_packetize`]. Success pushes one [`InFlight`];
    /// failure pushes nothing and resets the command buffer.
    unsafe fn submit_frame(&mut self, frame: &CapturedFrame, t0: std::time::Instant) -> Result<()> {
        // A failed `reset()` leaves the encoder destroyed and null. A null here is a
        // use-after-free inside pyrowave, so fail loudly.
        anyhow::ensure!(
            self.pw_encs.iter().all(|e| !e.is_null()),
            "pyrowave: encode after a failed reset (encoder was destroyed and not rebuilt)"
        );
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // No alignment: a mismatch smears (`rgb2yuv.comp` clamps; CPU uploads min(len,need)).
        // `import_cached` keys on inode without rechecking extent. A PipeWire size change
        // is not always transient (`reset()` reopens at the same dimensions).
        if frame.width != w || frame.height != h {
            bail!(
                "pyrowave: frame {}x{} != session mode {w}x{h} — refusing a mismatched encode \
                 source",
                frame.width,
                frame.height
            );
        }
        // `begin` through `queue_submit` in one closure whose error arm resets `cmd`.
        // Never PENDING on those arms. Failures after must not reset: a fence timeout
        // leaves PENDING (VUID-vkResetCommandBuffer-commandBuffer-00045).
        let rate_budget = self.rate_budget(); // before the closure (mutably borrows `self`)
        let slot = self.next_slot;
        let seq = self.wire_seq;
        let cmd = self.slots[slot].cmd;
        let fence = self.slots[slot].fence;
        let record_and_submit = (|| -> Result<()> {
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let cursor_pc = self.prep_cursor(slot, frame.cursor.as_ref())?;

            let rgb_view = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    let (old, src_qf, dst_qf) = if fresh {
                        (vk::ImageLayout::UNDEFINED, self.foreign_qfi, self.family)
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
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    view
                }
                FramePayload::Cpu(bytes) => {
                    // 24-bpp Rgb/Bgr expands 3→4 first (`normalize_cpu_rgb`).
                    let mut scratch = std::mem::take(&mut self.cpu_expand);
                    let (norm_fmt, norm_bytes) =
                        normalize_cpu_rgb(frame.format, bytes, &mut scratch, false);
                    let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                    let view = match fmt {
                        Ok(f) => self.ensure_cpu_rgb(slot, f, norm_bytes),
                        Err(e) => Err(e),
                    };
                    self.cpu_expand = scratch;
                    let view = view?;
                    let (img, ..) = self.slots[slot].cpu_img.unwrap();
                    let (stage, ..) = self.slots[slot].cpu_stage.unwrap();
                    let to_dst = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        cmd,
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
                                width: w,
                                height: h,
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
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    view
                }
                _ => bail!("pyrowave: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            self.bind_rgb(slot, rgb_view);

            // y/uv → GENERAL for CSC storage writes. This slot's previous frame was retired
            // before `submit` chose it (the execution barrier pyrowave asks for).
            let (y_img, uv_img) = (self.slots[slot].y_img, self.slots[slot].uv_img);
            let to_general = |img| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(img)
                    .subresource_range(color_range(0))
            };
            dev.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_general(y_img), to_general(uv_img)]),
            );
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
            dev.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.csc_layout,
                0,
                &[self.slots[slot].csc_set],
                &[],
            );
            let mut pc_bytes = [0u8; 16];
            for (i, v) in cursor_pc.iter().enumerate() {
                pc_bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            dev.cmd_push_constants(
                cmd,
                self.csc_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &pc_bytes,
            );
            // 4:2:0: one invocation per 2×2 luma block; 4:4:4: per pixel.
            if self.chroma444 {
                dev.cmd_dispatch(cmd, w.div_ceil(8), h.div_ceil(8), 1);
            } else {
                dev.cmd_dispatch(cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);
            }

            // CSC writes → pyrowave sampled reads. Stay GENERAL (pyrowave's GPU-buffer layout).
            let to_sampled = |img| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(img)
                    .subresource_range(color_range(0))
            };
            dev.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_sampled(y_img), to_sampled(uv_img)]),
            );

            let plane = |image: vk::Image,
                         pw_w: u32,
                         pw_h: u32,
                         fmt: pw::VkFormat,
                         swizzle: pw::VkComponentSwizzle| {
                pw::pyrowave_image_view {
                    image: image.as_raw() as usize as pw::VkImage,
                    width: pw_w,
                    height: pw_h,
                    image_format: fmt,
                    view_format: fmt,
                    mip_level: 0,
                    layer: 0,
                    aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
                    swizzle,
                    layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
                }
            };
            let r8 = pw::VkFormat_VK_FORMAT_R8_UNORM;
            let rg8 = pw::VkFormat_VK_FORMAT_R8G8_UNORM;
            let buffers = pw::pyrowave_gpu_buffers {
                planes: [
                    plane(
                        y_img,
                        w,
                        h,
                        r8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                    ),
                    // RG chroma: R/G swizzles synthesize Cb/Cr. Extent is this image's mip0
                    // (separate image, not a planar aspect): half-res 4:2:0, full-res 4:4:4.
                    plane(
                        uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        rg8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                    ),
                    plane(
                        uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        rg8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_G,
                    ),
                ],
            };
            let rc = pw::pyrowave_rate_control {
                maximum_bitstream_size: rate_budget,
            };
            pw::pyrowave_device_set_command_buffer(
                self.pw_dev,
                cmd.as_raw() as usize as pw::VkCommandBuffer,
            );
            // Stamp our monotonic counter before encode, or alternating handles emit
            // 1,1,2,2… and the decoder swallows repeats as more blocks of the same frame.
            // Needs `patches/0007-encoder-sequence-override.patch`.
            pw_check(
                pw::pyrowave_encoder_set_next_sequence(
                    self.pw_encs[slot],
                    seq & pw::PYROWAVE_SEQUENCE_MASK,
                ),
                "set_next_sequence",
            )?;
            let enc_res = pw::pyrowave_encoder_encode_gpu_synchronous(
                self.pw_encs[slot],
                std::ptr::null(),
                std::ptr::null(),
                &buffers,
                &rc,
            );
            pw::pyrowave_device_set_command_buffer(self.pw_dev, std::ptr::null_mut());
            pw_check(enc_res, "encode_gpu_synchronous")?;

            dev.end_command_buffer(cmd)?;
            dev.reset_fences(&[fence])?;
            let cmds = [cmd];
            dev.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                fence,
            )?;
            Ok(())
        })();
        if let Err(e) = record_and_submit {
            // SAFETY: every closure error arm is RECORDING/INVALID/EXECUTABLE — never PENDING
            // (nothing was enqueued) — and the pool allows the reset.
            let _ = dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
            return Err(e);
        }
        // GPU may be executing: do not touch `cmd`, y/uv, or `csc_set` until retired.
        self.next_slot = (slot + 1) % SLOTS;
        // Advance only on success: a gap reads as a restart, which is right for a dropped
        // frame and wrong for one that was never emitted.
        self.wire_seq = self.wire_seq.wrapping_add(1);
        self.inflight.push_back(InFlight {
            slot,
            seq: (seq & pw::PYROWAVE_SEQUENCE_MASK) as u8,
            pts_ns: frame.pts_ns,
            cap: self.frame_budget + BS_SLACK,
            wire_chunk: self.wire_chunk,
            t0,
        });
        Ok(())
    }

    /// Wait the oldest in-flight fence, then packetize into `pending`. Failure does not
    /// reset the command buffer (timeout leaves PENDING —
    /// VUID-vkResetCommandBuffer-commandBuffer-00045) and does not pop the entry: that is
    /// what tells `reset()` there is still live GPU work.
    unsafe fn wait_and_packetize(&mut self) -> Result<()> {
        let Some(fr) = self.inflight.front().copied() else {
            return Ok(());
        };
        let dev = self.device.clone();
        dev.wait_for_fences(&[self.slots[fr.slot].fence], true, 5_000_000_000)
            .context("pyrowave encode fence")?;
        // One-time submit: command buffer is INVALID; next `begin` may implicitly reset.
        self.inflight.pop_front();

        // Dense: boundary = whole buffer → one packet. Datagram-aligned: boundary = shard
        // payload; packets pad so a lost shard costs only those blocks. Use `fr.cap` /
        // `fr.wire_chunk`, not the live fields: bitrate/chunking can land mid-flight.
        let cap = fr.cap;
        self.bitstream.resize(cap, 0);
        // Chunked mode reserves the 4-byte window prefix from the packetize boundary.
        let boundary = crate::pyrowave_wire::packet_boundary(fr.wire_chunk, cap);
        let mut n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_compute_num_packets(self.pw_encs[fr.slot], boundary, &mut n),
            "compute_num_packets",
        )?;
        if n == 0 || (fr.wire_chunk.is_none() && n != 1) {
            bail!("pyrowave: unexpected packet count {n} at boundary {boundary}");
        }
        let mut packets = vec![pw::pyrowave_packet { offset: 0, size: 0 }; n];
        let mut out_n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_packetize(
                self.pw_encs[fr.slot],
                packets.as_mut_ptr(),
                boundary,
                &mut out_n,
                self.bitstream.as_mut_ptr() as *mut std::ffi::c_void,
                cap,
            ),
            "packetize",
        )?;
        packets.truncate(out_n.max(1));
        // Pyrowave's VUI signals ycbcr_range=FULL; our CSC emits BT.709 limited. Stamp the
        // bits honest so VUI-honoring clients don't wash out blacks.
        if let Some(p) = packets.first() {
            crate::pyrowave_wire::stamp_color_bits(&mut self.bitstream, p.offset, false);
            // Without patch 0007 the two handles count independently and clients swallow
            // repeats. A re-vendor that loses the patch still builds. Once per process.
            if crate::pyrowave_wire::wire_sequence(&self.bitstream, p.offset) != Some(fr.seq) {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::error!(
                        expected = fr.seq,
                        got = ?crate::pyrowave_wire::wire_sequence(&self.bitstream, p.offset),
                        "pyrowave: the wire sequence counter is NOT what we stamped — \
                         patches/0007-encoder-sequence-override.patch is missing or ineffective. \
                         With two alternating encoder handles this silently halves the frame rate \
                         on every client"
                    );
                }
            }
        }
        let pkts: Vec<(usize, usize)> = packets.iter().map(|p| (p.offset, p.size)).collect();
        let au = crate::pyrowave_wire::build_au(&pkts, &self.bitstream, fr.wire_chunk);
        if fr.wire_chunk.is_some() {
            let raw: usize = pkts.iter().map(|&(_, s)| s).sum();
            self.wire_budget.observe(raw, au.len());
        }
        self.frame_count += 1;
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: fr.pts_ns,
            keyframe: true,
            recovery_anchor: false,
            chunk_aligned: fr.wire_chunk.is_some(),
        });
        self.note_encode_us(fr.t0.elapsed().as_micros() as u32);
        Ok(())
    }

    unsafe fn drain_to(&mut self, keep: usize) -> Result<()> {
        while self.inflight.len() > keep {
            self.wait_and_packetize()?;
        }
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // Kept above SAFETY so that comment stays attached to the block it proves.
        let t0 = std::time::Instant::now();
        // SAFETY: single-threaded encoder; both halves work on handles this struct owns.
        // `submit_frame` resets the buffer on every pre-submit failure. A fence-wait
        // failure (buffer possibly PENDING) must not reset
        // (VUID-vkResetCommandBuffer-commandBuffer-00045). Recovery idles the device first.
        unsafe {
            // At most `max_inflight - 1` still in flight, so `next_slot` is free.
            self.drain_to(self.max_inflight.saturating_sub(1))?;
            self.submit_frame(frame, t0)
        }
    }

    fn caps(&self) -> EncoderCaps {
        // Every frame is intra. Report the opened chroma: `default()` would mis-report
        // 4:4:4 as 4:2:0 and fire a spurious Welcome-chroma warn.
        EncoderCaps {
            blends_cursor: true,
            chroma_444: self.chroma444,
            ..EncoderCaps::default()
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Each AU is drained through one method. Erroring beats double-emitting bytes the
        // chunk cursor already handed out. Check before the fence wait.
        if self.chunker.is_some() {
            bail!("pyrowave: poll() on an AU already being drained through poll_chunk");
        }
        if self.pending.is_empty() && !self.inflight.is_empty() {
            // SAFETY: single-threaded encoder, waiting its own fence and reading its own
            // bitstream; failure leaves the entry in flight for `reset()` to re-wait.
            unsafe { self.wait_and_packetize()? };
        }
        Ok(self.pending.pop_front())
    }

    fn supports_chunked_poll(&self) -> bool {
        crate::pyrowave_wire::stream_chunk_step(self.wire_chunk).is_some()
    }

    fn poll_chunk(&mut self) -> Result<Option<crate::AuChunk>> {
        // Finish the AU already in flight: `handle_chunk` keys off `first`/`last`.
        if let Some(c) = self.chunker.as_mut() {
            if let Some(chunk) = c.next() {
                return Ok(Some(chunk));
            }
            self.chunker = None;
        }
        let Some(f) = self.pending.pop_front() else {
            return Ok(None);
        };
        // No wait: `submit` already ran the encode, so an AU in `pending` is complete.
        match crate::pyrowave_wire::stream_chunk_step(self.wire_chunk) {
            Some(step) => Ok(self
                .chunker
                .insert(crate::pyrowave_wire::AuChunker::new(f, step))
                .next()),
            None => Ok(Some(crate::AuChunk::whole(f))),
        }
    }

    fn reset(&mut self) -> bool {
        // Rebuild forfeits in-flight frames, including a half-handed-out AU. Drop the
        // cursor first so the next `poll_chunk` cannot splice a dead tail onto a fresh AU.
        self.chunker = None;
        // Recreate the pyrowave encoder object only (no RC history). Bounded wait first:
        // an untimed `device_wait_idle` would park recovery on a wedged GPU. Destroying
        // the encoder under live GPU work is a use-after-free.
        if !self.inflight.is_empty() {
            // Every in-flight fence, not just the oldest: destroying under any live GPU
            // work is a use-after-free.
            let fences: Vec<vk::Fence> = self
                .inflight
                .iter()
                .map(|f| self.slots[f.slot].fence)
                .collect();
            // SAFETY: waiting this encoder's own fences under `&mut self`.
            if unsafe { self.device.wait_for_fences(&fences, true, 5_000_000_000) }.is_err() {
                tracing::error!(
                    "pyrowave: in-flight encode did not complete within the reset budget — GPU \
                     or driver wedged; in-place rebuild abandoned"
                );
                self.pending.clear();
                return false;
            }
            // Bitstream lives in the encoder about to be destroyed; GPU is done with them.
            self.inflight.clear();
        }
        // SAFETY: device idle for this encoder's work; pyrowave device outlives the encoder
        // being swapped.
        unsafe {
            self.device.device_wait_idle().ok();
            let einfo = pw::pyrowave_encoder_create_info {
                device: self.pw_dev,
                width: self.width as i32,
                height: self.height as i32,
                chroma: if self.chroma444 {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
                } else {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
                },
            };
            for i in 0..SLOTS {
                pw::pyrowave_encoder_destroy(self.pw_encs[i]);
                // Null immediately: create below is fallible. `pyrowave_encoder_destroy` is
                // a plain `delete` with no null check, so `Drop` on a stale handle is a
                // double free.
                self.pw_encs[i] = std::ptr::null_mut();
                let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
                let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
                if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                    tracing::error!(result = ?r, slot = i, "pyrowave: encoder rebuild failed");
                    // Stays null — `Drop` and `submit_frame` both guard on it. Drop queued AUs
                    // rather than shipping output from a dead encoder.
                    self.pending.clear();
                    return false;
                }
                self.pw_encs[i] = enc;
            }
            // Fresh handles start at 0; the client's `last_seq` does not. Keep counting so
            // a gap tells the decoder to restart.
            self.next_slot = 0;
        }
        self.pending.clear();
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Per-frame byte budget — in-place retarget is free (no IDR, nothing in flight).
        self.frame_budget = budget_for(bps, self.fps);
        tracing::debug!(
            mbps = bps / 1_000_000,
            budget_kib = self.frame_budget / 1024,
            "pyrowave: per-frame rate budget retargeted in place"
        );
        true
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        // Below one block header + payload word is meaningless.
        if shard_payload >= 64 {
            self.wire_chunk = Some(shard_payload);
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization on (partial-frame loss mode)"
            );
        }
    }

    fn flush(&mut self) -> Result<()> {
        // Retire submitted-but-unwaited frames so `poll`-until-`None` returns every AU.
        // SAFETY: single-threaded encoder, waiting its own fence.
        unsafe { self.drain_to(0) }
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // SAFETY: owned handles, destroyed once, GPU idled first; pyrowave objects go
        // before the VkDevice they borrow. Also `open_inner`'s unwind: a failed open runs
        // this against a partial prefix. `pyrowave_device_destroy(null)` is `delete nullptr`;
        // `vkDestroy*` of VK_NULL_HANDLE is a spec no-op; `pw_encs` are not null-safe.
        unsafe {
            self.device.device_wait_idle().ok();
            // Null when a failed `reset()` already destroyed it.
            for &e in &self.pw_encs {
                if !e.is_null() {
                    pw::pyrowave_encoder_destroy(e);
                }
            }
            pw::pyrowave_device_destroy(self.pw_dev);
            for (_, _, i, m, v) in self.import_cache.drain(..) {
                self.device.destroy_image_view(v, None);
                self.device.destroy_image(i, None);
                self.device.free_memory(m, None);
            }
            // Failed open leaves a partial prefix; `vkDestroy*(VK_NULL_HANDLE)` is a no-op.
            for sl in std::mem::take(&mut self.slots) {
                if let Some((i, m, v, _)) = sl.cpu_img {
                    self.device.destroy_image_view(v, None);
                    self.device.destroy_image(i, None);
                    self.device.free_memory(m, None);
                }
                if let Some((b, m, _)) = sl.cpu_stage {
                    self.device.destroy_buffer(b, None);
                    self.device.free_memory(m, None);
                }
                self.device.destroy_fence(sl.fence, None);
                self.device.destroy_image_view(sl.y_view, None);
                self.device.destroy_image(sl.y_img, None);
                self.device.free_memory(sl.y_mem, None);
                self.device.destroy_image_view(sl.uv_view, None);
                self.device.destroy_image(sl.uv_img, None);
                self.device.free_memory(sl.uv_mem, None);
                self.device.destroy_image_view(sl.cursor_view, None);
                self.device.destroy_image(sl.cursor_img, None);
                self.device.free_memory(sl.cursor_mem, None);
                self.device.destroy_buffer(sl.cursor_stage, None);
                self.device.free_memory(sl.cursor_stage_mem, None);
            }
            // Command buffers and descriptor sets are freed with their pools.
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_descriptor_pool(self.csc_pool, None);
            self.device.destroy_pipeline(self.csc_pipe, None);
            self.device.destroy_pipeline_layout(self.csc_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.csc_dsl, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::PixelFormat;

    fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// BT.709 limited-range YCbCr of an 8-bit RGB fill — same math as `rgb2yuv.comp`.
    fn bt709(fill: [u8; 4]) -> (f64, f64, f64) {
        let (b, g, r) = (fill[0] as f64, fill[1] as f64, fill[2] as f64); // BGRA
        (
            16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b,
            128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b,
            128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b,
        )
    }

    /// Decode an AU with a standalone pyrowave decoder to planar YUV. Oracle for smoke
    /// plane-means and the Apple Metal PSNR fixtures (`pyrowave_dump_golden`).
    unsafe fn decode_planes_chroma(
        w: u32,
        h: u32,
        au: &[u8],
        chroma444: bool,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut dev: pw::pyrowave_device = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_create_default_device(&mut dev),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        let dinfo = pw::pyrowave_decoder_create_info {
            device: dev,
            width: w as i32,
            height: h as i32,
            chroma: if chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
            fragment_path: false,
        };
        let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_decoder_create(&dinfo, &mut dec),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        assert_eq!(
            pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        assert!(pw::pyrowave_decoder_decode_is_ready(dec, false));

        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };
        let mut y = vec![0u8; (w * h) as usize];
        let mut cb = vec![0u8; (cw * ch) as usize];
        let mut cr = vec![0u8; (cw * ch) as usize];
        let mut buf: pw::pyrowave_cpu_buffer = std::mem::zeroed();
        buf.format = if chroma444 {
            pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV444P
        } else {
            pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV420P
        };
        buf.width = w as i32;
        buf.height = h as i32;
        buf.data = [
            y.as_mut_ptr() as *mut _,
            cb.as_mut_ptr() as *mut _,
            cr.as_mut_ptr() as *mut _,
        ];
        buf.row_stride_in_bytes = [w as usize, cw as usize, cw as usize];
        buf.plane_size_in_bytes = [y.len(), cb.len(), cr.len()];
        assert_eq!(
            pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &buf),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        pw::pyrowave_decoder_destroy(dec);
        pw::pyrowave_device_destroy(dev);
        (y, cb, cr)
    }

    unsafe fn decode_planes(w: u32, h: u32, au: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        // SAFETY: same contract as the caller.
        unsafe { decode_planes_chroma(w, h, au, false) }
    }

    unsafe fn decode_plane_means(w: u32, h: u32, au: &[u8], chroma444: bool) -> (f64, f64, f64) {
        // SAFETY: same contract as the caller.
        let (y, cb, cr) = unsafe { decode_planes_chroma(w, h, au, chroma444) };
        let mean = |v: &[u8]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
        (mean(&y), mean(&cb), mean(&cr))
    }

    /// Open → CSC → GPU encode → packetize, then CPU-decode each AU and check plane
    /// means against the CSC's BT.709 math. Needs a Vulkan 1.3 GPU.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_smoke() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open");
        assert!(!enc.caps().supports_rfi);

        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [128, 128, 128, 255],
        ];
        for (i, c) in colors.iter().enumerate() {
            enc.submit(&cpu_frame(w, h, i as u64 * 16_666_667, *c))
                .expect("submit");
            let au = enc.poll().expect("poll").expect("one AU per frame");
            assert!(au.keyframe, "every pyrowave AU is a keyframe");
            assert!(!au.data.is_empty());
            assert!(
                au.data.len() <= enc.frame_budget + BS_SLACK,
                "AU exceeds rate budget"
            );
            // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
            let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, false) };
            let (ye, cbe, cre) = bt709(*c);
            assert!(
                (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
                "frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
                 expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
            );
        }

        // Datagram-aligned: AU is a whole number of framed windows. Walk + reassemble
        // must reproduce a decodable packet stream.
        enc.set_wire_chunking(1408);
        enc.submit(&cpu_frame(w, h, 500, [90, 60, 30, 255]))
            .expect("chunked submit");
        let au = enc.poll().expect("poll").expect("chunked AU");
        assert!(au.chunk_aligned);
        assert_eq!(au.data.len() % 1408, 0, "AU is a whole number of windows");
        // SAFETY: test-only FFI with locally-owned buffers.
        unsafe {
            let mut dev: pw::pyrowave_device = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_create_default_device(&mut dev),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let dinfo = pw::pyrowave_decoder_create_info {
                device: dev,
                width: w as i32,
                height: h as i32,
                chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
                fragment_path: false,
            };
            let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_decoder_create(&dinfo, &mut dec),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let mut frag: Vec<u8> = Vec::new();
            let mut pushed = 0usize;
            for win in au.data.chunks(1408) {
                let used = u16::from_le_bytes([win[0], win[1]]) as usize;
                let kind = u16::from_le_bytes([win[2], win[3]]);
                assert!(4 + used <= win.len(), "window overrun");
                assert!(win[4 + used..].iter().all(|&b| b == 0), "non-zero padding");
                let body = &win[4..4 + used];
                match kind {
                    0 => {
                        assert_eq!(
                            pw::pyrowave_decoder_push_packet(
                                dec,
                                body.as_ptr() as *const _,
                                body.len()
                            ),
                            pw::pyrowave_result_PYROWAVE_SUCCESS
                        );
                        pushed += body.len();
                    }
                    1 => frag = body.to_vec(),
                    2 => frag.extend_from_slice(body),
                    3 => {
                        frag.extend_from_slice(body);
                        assert_eq!(
                            pw::pyrowave_decoder_push_packet(
                                dec,
                                frag.as_ptr() as *const _,
                                frag.len()
                            ),
                            pw::pyrowave_result_PYROWAVE_SUCCESS
                        );
                        pushed += frag.len();
                        frag.clear();
                    }
                    k => panic!("unknown window kind {k}"),
                }
            }
            assert!(pushed > 0, "chunked AU carries real packets");
            assert!(
                pw::pyrowave_decoder_decode_is_ready(dec, false),
                "chunked AU incomplete after framed walk"
            );
            pw::pyrowave_decoder_destroy(dec);
            pw::pyrowave_device_destroy(dev);
        }
        enc.set_wire_chunking(0); // below the floor — back to dense
        assert!(enc.reconfigure_bitrate(100_000_000));
        assert!(enc.reset());
        enc.submit(&cpu_frame(w, h, 999, [10, 20, 30, 255]))
            .expect("submit after reset");
        assert!(enc.poll().expect("poll").is_some());
    }

    /// Packed 24-bpp CPU frame. `rgb` is (r, g, b) regardless of `fmt`'s byte order.
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
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: fmt,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// 24-bpp CPU payloads expand 3→4, not refuse. Channel order is load-bearing: a
    /// swapped R/B or misplaced pad moves chroma means by tens of codes.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_smoke_cpu_rgb24() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open");
        let colors: [[u8; 3]; 3] = [[200, 40, 40], [40, 200, 40], [40, 40, 200]];
        for fmt in [PixelFormat::Rgb, PixelFormat::Bgr] {
            for (i, c) in colors.iter().enumerate() {
                enc.submit(&cpu_frame_24(w, h, i as u64 * 16_666_667, *c, fmt))
                    .expect("submit 24-bpp");
                let au = enc.poll().expect("poll").expect("one AU per frame");
                // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
                let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, false) };
                let (ye, cbe, cre) = bt709([c[2], c[1], c[0], 255]);
                assert!(
                    (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
                    "{fmt:?} frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) \
                     vs expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
                );
            }
        }
    }

    /// Driver-reported VRAM per slot at the modes that decide affordability. Prints rather
    /// than asserting a GPU-wide cap; asserts the slot is neither free nor absurd so a
    /// refactor that made bitstream/import-cache per-slot fails visibly.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn slot_vram_cost_is_reported() {
        for (w, h, chroma, name) in [
            (1920u32, 1080u32, crate::ChromaFormat::Yuv420, "1080p 4:2:0"),
            (3840, 2160, crate::ChromaFormat::Yuv420, "4K 4:2:0"),
            (3840, 2160, crate::ChromaFormat::Yuv444, "4K 4:4:4"),
        ] {
            let enc = PyroWaveEncoder::open(w, h, 60, 40_000_000, chroma).expect("open");
            // SAFETY: plain memory-requirement queries on images this encoder owns.
            let per_slot: u64 = unsafe {
                [
                    enc.slots[0].y_img,
                    enc.slots[0].uv_img,
                    enc.slots[0].cursor_img,
                ]
                .iter()
                .map(|&i| enc.device.get_image_memory_requirements(i).size)
                .sum::<u64>()
                    + enc
                        .device
                        .get_buffer_memory_requirements(enc.slots[0].cursor_stage)
                        .size
            };
            eprintln!(
                "{name}: {} KiB per slot, {SLOTS} slots = {} KiB total",
                per_slot / 1024,
                per_slot * SLOTS as u64 / 1024
            );
            assert!(per_slot > 0, "{name}: a slot must own real memory");
            assert!(
                per_slot < 512 * 1024 * 1024,
                "{name}: {per_slot} bytes per slot — something large became per-slot that should \
                 not have (bitstream? import cache?)"
            );
        }
    }

    /// A frame that is not the session mode must be refused. PyroWave applies no
    /// alignment; without this check CSC clamps and CPU uploads `min(len, need)`, so it
    /// smears. Refusal is before record, so a correctly-sized frame after still encodes.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_refuses_a_frame_that_is_not_the_mode() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open");
        for (fw, fh) in [(w - 2, h), (w, h - 2), (w + 2, h + 2)] {
            let err = enc
                .submit(&cpu_frame(fw, fh, 0, [200, 40, 40, 255]))
                .expect_err("a frame that is not the mode must be refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("session mode"),
                "the refusal must name the mismatch, got: {msg}"
            );
        }
        // Refusal must enqueue nothing. Check before the good frame: `pending` is a queue.
        assert!(
            enc.poll().expect("poll").is_none(),
            "a refused frame must not enqueue an AU"
        );
        enc.submit(&cpu_frame(w, h, 16_666_667, [40, 200, 40, 255]))
            .expect("a correctly-sized frame after a refusal must still encode");
        assert!(
            enc.poll().expect("poll").is_some(),
            "the session must survive the refusals"
        );
        assert!(
            enc.poll().expect("poll").is_none(),
            "exactly one AU for one accepted frame"
        );
    }

    /// A failed dmabuf import must leak neither the dup'd fd nor the VkImage. Drives
    /// `import_rgb_dmabuf` directly so deliberate failures cannot trip the raw-dmabuf latch.
    /// Garbage modifier fails at `create_image`; LINEAR memfd fails at `allocate_memory`.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn import_failure_leaks_no_fds() {
        use std::os::fd::FromRawFd;
        let enc = PyroWaveEncoder::open(64, 64, 60, 5_000_000, crate::ChromaFormat::Yuv420)
            .expect("open");
        let memfd_frame = |modifier: u64| {
            // SAFETY: plain memfd_create; the fresh descriptor is immediately owned below.
            let raw = unsafe { libc::memfd_create(c"pf-import-leak".as_ptr(), 0) };
            assert!(raw >= 0, "memfd_create failed");
            // SAFETY: `raw` is a freshly-created descriptor this closure owns.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            // SAFETY: size the owned memfd so an mmap-happy driver sees real pages.
            unsafe { libc::ftruncate(fd.as_raw_fd(), 64 * 64 * 4) };
            pf_frame::DmabufFrame {
                fd,
                fourcc: 0x3432_5258, // XR24 — maps, so failure lands past the fourcc gate
                modifier,
                plane1: None,
                offset: 0,
                stride: 64 * 4,
                hold: None,
            }
        };
        let fd_count = || std::fs::read_dir("/proc/self/fd").expect("procfs").count();
        // Warm lazily-opened driver/loader descriptors before the baseline.
        for modifier in [u64::MAX - 1, 0] {
            let d = memfd_frame(modifier);
            // SAFETY: live device/ext_fd/mem_props owned by `enc`; the frame is locally owned.
            let _ =
                unsafe { import_rgb_dmabuf(&enc.device, &enc.ext_fd, &enc.mem_props, &d, 64, 64) };
        }
        let baseline = fd_count();
        for i in 0..32 {
            let modifier = if i % 2 == 0 { u64::MAX - 1 } else { 0 };
            let d = memfd_frame(modifier);
            // SAFETY: live device/ext_fd/mem_props owned by `enc`; the frame is locally owned.
            let r =
                unsafe { import_rgb_dmabuf(&enc.device, &enc.ext_fd, &enc.mem_props, &d, 64, 64) };
            assert!(
                r.is_err(),
                "a memfd/garbage-modifier import must fail (iteration {i})"
            );
        }
        assert_eq!(
            fd_count(),
            baseline,
            "fd count drifted across 32 failed imports — the unwind leaks"
        );
    }

    /// 4:4:4 must stay within budget, decode, and be run-to-run deterministic
    /// (`patches/0001-payload-data-444-sizing.patch`).
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_smoke_444() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv444).expect("open");
        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [128, 128, 128, 255],
        ];
        for (i, c) in colors.iter().enumerate() {
            enc.submit(&cpu_frame(w, h, i as u64 * 16_666_667, *c))
                .expect("submit");
            let au = enc.poll().expect("poll").expect("one AU per frame");
            assert!(au.keyframe);
            assert!(
                au.data.len() <= enc.frame_budget + BS_SLACK,
                "AU exceeds rate budget"
            );
            // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
            let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, true) };
            let (ye, cbe, cre) = bt709(*c);
            assert!(
                (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
                "frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
                 expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
            );
        }

        // Busy content at ~2.6 bpp — the regime that overran 4:2:0-sized payload staging.
        let budget_bps = w as u64 * h as u64 * 60 * 26 / 10;
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, budget_bps, crate::ChromaFormat::Yuv444).expect("open");
        let mut sizes = Vec::new();
        for _ in 0..3 {
            enc.submit(&test_card(w, h, 7)).expect("busy submit");
            let au = enc.poll().expect("poll").expect("busy AU");
            assert!(
                au.data.len() <= enc.frame_budget + BS_SLACK,
                "busy 4:4:4 AU exceeds rate budget ({} > {})",
                au.data.len(),
                enc.frame_budget + BS_SLACK
            );
            // SAFETY: test-only FFI with locally-owned buffers.
            let _ = unsafe { decode_planes_chroma(w, h, &au.data, true) };
            sizes.push(au.data.len());
        }
        assert!(
            sizes.windows(2).all(|s| s[0] == s[1]),
            "identical input produced varying AU sizes (the Phase-0 overrun signature): {sizes:?}"
        );
    }

    /// Deterministic busy BGRA card (gradients + checker + LCG). Flat fills miss the
    /// entropy decoder; this hits every subband.
    fn test_card(w: u32, h: u32, seed: u32) -> CapturedFrame {
        let mut rng = seed | 1;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                let i = ((y * w + x) * 4) as usize;
                let checker = if (x / 16 + y / 16) % 2 == 0 { 48 } else { 0 };
                let noise = (rng >> 24) as u8 / 8;
                buf[i] = ((x * 255 / w) as u8).saturating_add(noise);
                buf[i + 1] = ((y * 255 / h) as u8).saturating_add(checker);
                buf[i + 2] = (((x + y) * 255 / (w + h)) as u8).saturating_add(noise);
                buf[i + 3] = 255;
            }
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: seed as u64 * 16_666_667,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// Dump Apple Metal golden fixtures: host-encoded AUs (dense and chunk-aligned)
    /// plus upstream decode as YUV420P. Float wavelet math is not bit-exact; the Swift
    /// test PSNR-matches Metal against these. Set `PYROWAVE_GOLDEN_DIR` and copy into
    /// `clients/apple/Tests/PunktfunkKitTests/PyroWaveFixtures/`.
    #[test]
    #[ignore = "fixture generator — needs a real Vulkan 1.3 compute device"]
    fn pyrowave_dump_golden() {
        let dir = match std::env::var("PYROWAVE_GOLDEN_DIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => {
                eprintln!("PYROWAVE_GOLDEN_DIR not set — skipping dump");
                return;
            }
        };
        std::fs::create_dir_all(&dir).expect("create golden dir");

        // Odd-block geometry: 256 aligns clean, 144 → aligned 160 exercises overhang. ~1.6 bpp.
        let (w, h) = (256u32, 144u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 4_000_000, crate::ChromaFormat::Yuv420).expect("open");

        let dump = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(name), bytes).expect("write fixture");
            eprintln!("wrote {name}: {} bytes", bytes.len());
        };

        enc.submit(&test_card(w, h, 7)).expect("submit");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(!au.chunk_aligned);
        dump("au-dense.bin", &au.data);
        // SAFETY: test-only FFI with locally-owned buffers.
        let (y, cb, cr) = unsafe { decode_planes(w, h, &au.data) };
        dump("ref-dense-y.bin", &y);
        dump("ref-dense-cb.bin", &cb);
        dump("ref-dense-cr.bin", &cr);

        // Different frame: Swift window walk + FRAG reassembly must reproduce the stream.
        enc.set_wire_chunking(1408);
        enc.submit(&test_card(w, h, 11)).expect("chunked submit");
        let au = enc.poll().expect("poll").expect("chunked AU");
        assert!(au.chunk_aligned);
        assert_eq!(au.data.len() % 1408, 0);
        dump("au-chunked.bin", &au.data);
        // SAFETY: test-only FFI with locally-owned buffers.
        let (y, cb, cr) = unsafe {
            // Same framed walk the clients use.
            let mut stream = Vec::new();
            let mut frag: Vec<u8> = Vec::new();
            for win in au.data.chunks(1408) {
                let used = u16::from_le_bytes([win[0], win[1]]) as usize;
                let kind = u16::from_le_bytes([win[2], win[3]]);
                let body = &win[4..4 + used];
                match kind {
                    0 => stream.extend_from_slice(body),
                    1 => frag = body.to_vec(),
                    2 => frag.extend_from_slice(body),
                    3 => {
                        frag.extend_from_slice(body);
                        stream.extend_from_slice(&frag);
                        frag.clear();
                    }
                    k => panic!("unknown window kind {k}"),
                }
            }
            decode_planes(w, h, &stream)
        };
        dump("ref-chunked-y.bin", &y);
        dump("ref-chunked-cb.bin", &cb);
        dump("ref-chunked-cr.bin", &cr);

        // 4:4:4 dense AU + full-res chroma reference. Same odd-block geometry.
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 6_500_000, crate::ChromaFormat::Yuv444).expect("open");
        enc.submit(&test_card(w, h, 13)).expect("444 submit");
        let au = enc.poll().expect("poll").expect("444 AU");
        assert!(!au.chunk_aligned);
        dump("au-dense444.bin", &au.data);
        // SAFETY: test-only FFI with locally-owned buffers.
        let (y, cb, cr) = unsafe { decode_planes_chroma(w, h, &au.data, true) };
        dump("ref-dense444-y.bin", &y);
        dump("ref-dense444-cb.bin", &cb);
        dump("ref-dense444-cr.bin", &cr);
    }

    // Device-create ladder needs a real GPU. Grammar is what drifts: same env var drives
    // Windows (patch live) and Linux. Device-free: `queue_priority_candidates` takes the
    // raw string so env-var tests do not race.

    /// Unset means the realtime ladder (REALTIME then HIGH), not a single class.
    #[test]
    fn unset_requests_the_realtime_ladder() {
        assert_eq!(
            queue_priority_candidates(None),
            vec![
                vk::QueueGlobalPriorityKHR::REALTIME,
                vk::QueueGlobalPriorityKHR::HIGH
            ]
        );
    }

    /// `off` is the only disable spelling (case-insensitive). `0` is not: the C side
    /// does not accept it either.
    #[test]
    fn only_off_disables_and_it_is_case_insensitive() {
        assert!(queue_priority_candidates(Some("off")).is_empty());
        assert!(queue_priority_candidates(Some("OFF")).is_empty());
        assert!(queue_priority_candidates(Some("Off")).is_empty());
        assert!(!queue_priority_candidates(Some("0")).is_empty());
    }

    /// `high` is HIGH only — silently trying REALTIME first would hide "elevated, not realtime".
    #[test]
    fn high_asks_for_high_alone() {
        assert_eq!(
            queue_priority_candidates(Some("high")),
            vec![vk::QueueGlobalPriorityKHR::HIGH]
        );
        assert_eq!(
            queue_priority_candidates(Some("HIGH")),
            vec![vk::QueueGlobalPriorityKHR::HIGH]
        );
    }

    /// Junk falls back to the default ladder, not to off: unparseable must not disable the lever.
    #[test]
    fn junk_falls_back_to_the_default_ladder() {
        for raw in ["", "realtime", "REALTIME", "yes", "1", "medium", "  high"] {
            assert!(
                !queue_priority_candidates(Some(raw)).is_empty(),
                "{raw:?} must not disable the priority request"
            );
        }
        // Full ladder, not HIGH alone. The C patch does not trim, so neither do we.
        assert_eq!(queue_priority_candidates(Some("  high")).len(), 2);
    }

    /// A refused class walks the ladder down, never fails the open. `NOT_PERMITTED` is
    /// specified; `INITIALIZATION_FAILED` matches VkBridge. Anything else must propagate.
    #[test]
    fn only_refusals_walk_the_ladder_down() {
        assert!(priority_refused(vk::Result::ERROR_NOT_PERMITTED_KHR));
        assert!(priority_refused(vk::Result::ERROR_INITIALIZATION_FAILED));
        assert!(!priority_refused(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY));
        assert!(!priority_refused(vk::Result::ERROR_EXTENSION_NOT_PRESENT));
        assert!(!priority_refused(vk::Result::SUCCESS));
    }

    /// Walk a windowed AU back into the flat codec-packet stream (the clients' parse).
    fn walk_windows(au: &[u8], window: usize) -> Vec<u8> {
        let mut stream = Vec::new();
        let mut frag: Vec<u8> = Vec::new();
        for win in au.chunks(window) {
            let used = u16::from_le_bytes([win[0], win[1]]) as usize;
            let kind = u16::from_le_bytes([win[2], win[3]]);
            let body = &win[4..4 + used];
            match kind {
                0 => stream.extend_from_slice(body),
                1 => frag = body.to_vec(),
                2 => frag.extend_from_slice(body),
                3 => {
                    frag.extend_from_slice(body);
                    stream.extend_from_slice(&frag);
                    frag.clear();
                }
                k => panic!("unknown window kind {k}"),
            }
        }
        stream
    }

    /// Luma PSNR (dB) of decoded Y against BT.709 limited luma of source BGRA. Luma
    /// only: chroma is subsampled on 4:2:0, and luma is where wavelet quantisation shows.
    fn luma_psnr(src_bgra: &[u8], decoded_y: &[u8]) -> f64 {
        assert_eq!(src_bgra.len(), decoded_y.len() * 4);
        let mut sse = 0.0f64;
        for (px, &got) in src_bgra.chunks_exact(4).zip(decoded_y) {
            let (b, g, r) = (px[0] as f64, px[1] as f64, px[2] as f64);
            let want = 16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b;
            let d = want - got as f64;
            sse += d * d;
        }
        let mse = sse / decoded_y.len() as f64;
        if mse <= f64::EPSILON {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    /// With `PUNKTFUNK_PYROWAVE_STREAMED_AU=1`, a busy-card GPU encode must come out of
    /// `poll_chunk` in several window-aligned pieces that concatenate to a decodable AU.
    /// Flat fills reassemble even with missing subbands; the busy card puts energy in
    /// every subband so a lost window collapses PSNR.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_streamed_chunks_reassemble_and_keep_the_picture() {
        const WINDOW: usize = 1408;
        // 1280×720 at 60 Mb/s ≈ 125 KB/AU — several chunks, many windows.
        let (w, h) = (1280u32, 720u32);
        let mut enc = PyroWaveEncoder::open(w, h, 60, 200_000_000, crate::ChromaFormat::Yuv420)
            .expect("open pyrowave encoder");
        enc.set_wire_chunking(WINDOW);

        assert!(
            enc.supports_chunked_poll(),
            "PUNKTFUNK_PYROWAVE_STREAMED_AU=1 must be set in the ENVIRONMENT of this test binary \
             — without it PW6 is off by design and there is nothing to verify"
        );

        for seed in [7u32, 11, 13] {
            let frame = test_card(w, h, seed);
            let FramePayload::Cpu(ref src) = frame.payload else {
                panic!("test card is a CPU frame")
            };
            let src = src.clone();
            enc.submit(&frame).expect("submit");

            let mut au = Vec::new();
            let (mut chunks, mut firsts, mut lasts) = (0u32, 0u32, 0u32);
            loop {
                let c = enc
                    .poll_chunk()
                    .expect("poll_chunk")
                    .expect("an AU is in flight");
                assert!(c.chunk_aligned, "wire chunking is on");
                assert!(c.keyframe, "every pyrowave AU is a keyframe");
                assert_eq!(
                    c.data.len() % WINDOW,
                    0,
                    "every chunk is a whole number of windows — a cut inside a window would \
                     split the 4-byte framing prefix from its body"
                );
                chunks += 1;
                firsts += u32::from(c.first);
                lasts += u32::from(c.last);
                au.extend_from_slice(&c.data);
                if c.last {
                    break;
                }
            }
            assert_eq!(firsts, 1, "exactly one opening chunk");
            assert_eq!(lasts, 1, "exactly one closing chunk");
            assert!(
                chunks > 1,
                "seed {seed}: the AU came out in ONE piece ({} B) — the cut never engaged, so \
                 this run proves nothing about PW6",
                au.len()
            );
            assert_eq!(au.len() % WINDOW, 0, "the AU is a whole number of windows");

            assert!(
                enc.poll_chunk().expect("poll_chunk after last").is_none(),
                "no AU is in flight once `last` was handed out"
            );

            let stream = walk_windows(&au, WINDOW);
            // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
            let (y, _cb, _cr) = unsafe { decode_planes(w, h, &stream) };
            let psnr = luma_psnr(&src, &y);
            eprintln!(
                "seed {seed}: {chunks} chunks, {} B AU ({} windows), luma PSNR {psnr:.2} dB",
                au.len(),
                au.len() / WINDOW
            );
            assert!(
                psnr > 30.0,
                "seed {seed}: luma PSNR {psnr:.2} dB — the streamed reassembly lost or reordered \
                 picture data (a flat-fill test would NOT have caught this)"
            );
        }
    }

    /// Two `pyrowave_encoder` objects each keep a 3-bit `sequence_count`, so alternating
    /// them emits 1,1,2,2… The decoder restarts only when the value changes, so a repeat
    /// is swallowed. Patch 0007 stamps one counter. Over 20 frames (past the wrap at 8):
    /// wire +1 mod 8 per AU; one persistent decoder reports ready every AU; consecutive
    /// pictures differ (`test_card` reseeded; flat fills hide a swallowed frame).
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn wire_sequence_increments_across_alternating_handles() {
        const FRAMES: u32 = 20;
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open");
        const { assert!(SLOTS >= 2) };

        let mut aus: Vec<Vec<u8>> = Vec::new();
        for i in 0..FRAMES {
            // Content moves every frame. Odd seeds only: `test_card` starts its LCG at
            // `seed | 1`, so 2 and 3 produce a byte-identical card.
            enc.submit(&test_card(w, h, 2 * i + 1)).expect("submit");
            let au = enc.poll().expect("poll").expect("one AU per frame");
            aus.push(au.data);
        }

        let seqs: Vec<u8> = aus
            .iter()
            .map(|au| {
                crate::pyrowave_wire::wire_sequence(au, 0).expect("AU carries a block header")
            })
            .collect();
        for (i, pair) in seqs.windows(2).enumerate() {
            assert_eq!(
                pair[1],
                (pair[0] + 1) & 7,
                "frame {} -> {}: wire sequence went {} -> {} (all: {seqs:?}). Two encoder handles \
                 each counting alone produce repeats, which the decoder reads as more blocks of \
                 the same frame — check that patch 0007 is applied and set_next_sequence is called",
                i,
                i + 1,
                pair[0],
                pair[1]
            );
        }

        // One decoder for the whole run: a fresh decoder per AU resets `last_seq`.
        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        unsafe {
            let mut dev: pw::pyrowave_device = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_create_default_device(&mut dev),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let dinfo = pw::pyrowave_decoder_create_info {
                device: dev,
                width: w as i32,
                height: h as i32,
                chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
                fragment_path: false,
            };
            let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
            assert_eq!(
                pw::pyrowave_decoder_create(&dinfo, &mut dec),
                pw::pyrowave_result_PYROWAVE_SUCCESS
            );
            let mut last_y: Option<Vec<u8>> = None;
            for (i, au) in aus.iter().enumerate() {
                assert_eq!(
                    pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
                    pw::pyrowave_result_PYROWAVE_SUCCESS,
                    "frame {i} was rejected by the decoder"
                );
                assert!(
                    pw::pyrowave_decoder_decode_is_ready(dec, false),
                    "frame {i} never became decodable — the decoder is still accumulating it into \
                     the PREVIOUS frame, which is exactly the repeated-sequence failure"
                );
                let mut y = vec![0u8; (w * h) as usize];
                let mut cb = vec![0u8; (w * h / 4) as usize];
                let mut cr = vec![0u8; (w * h / 4) as usize];
                let mut buf: pw::pyrowave_cpu_buffer = std::mem::zeroed();
                buf.format = pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV420P;
                buf.width = w as i32;
                buf.height = h as i32;
                buf.data = [
                    y.as_mut_ptr() as *mut _,
                    cb.as_mut_ptr() as *mut _,
                    cr.as_mut_ptr() as *mut _,
                ];
                buf.row_stride_in_bytes = [w as usize, (w / 2) as usize, (w / 2) as usize];
                buf.plane_size_in_bytes = [y.len(), cb.len(), cr.len()];
                assert_eq!(
                    pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &buf),
                    pw::pyrowave_result_PYROWAVE_SUCCESS,
                    "frame {i} failed to decode"
                );
                if let Some(prev) = &last_y {
                    assert_ne!(
                        prev,
                        &y,
                        "frame {i} decoded to the SAME picture as frame {} — a swallowed frame",
                        i - 1
                    );
                }
                last_y = Some(y);
            }
            pw::pyrowave_decoder_destroy(dec);
            pw::pyrowave_device_destroy(dev);
        }
    }

    /// Decode a whole AU stream through one decoder (`last_seq` is not reset per frame)
    /// and return each frame's luma plane.
    ///
    /// # Safety
    /// Test-only FFI into the vendored decoder with locally-owned buffers.
    unsafe fn decode_stream_luma(w: u32, h: u32, aus: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut dev: pw::pyrowave_device = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_create_default_device(&mut dev),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        let dinfo = pw::pyrowave_decoder_create_info {
            device: dev,
            width: w as i32,
            height: h as i32,
            chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            fragment_path: false,
        };
        let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_decoder_create(&dinfo, &mut dec),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        let mut out = Vec::with_capacity(aus.len());
        for (i, au) in aus.iter().enumerate() {
            assert_eq!(
                pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
                pw::pyrowave_result_PYROWAVE_SUCCESS,
                "frame {i} rejected"
            );
            assert!(
                pw::pyrowave_decoder_decode_is_ready(dec, false),
                "frame {i} never became decodable"
            );
            let mut y = vec![0u8; (w * h) as usize];
            let mut cb = vec![0u8; (w * h / 4) as usize];
            let mut cr = vec![0u8; (w * h / 4) as usize];
            let mut buf: pw::pyrowave_cpu_buffer = std::mem::zeroed();
            buf.format = pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV420P;
            buf.width = w as i32;
            buf.height = h as i32;
            buf.data = [
                y.as_mut_ptr() as *mut _,
                cb.as_mut_ptr() as *mut _,
                cr.as_mut_ptr() as *mut _,
            ];
            buf.row_stride_in_bytes = [w as usize, (w / 2) as usize, (w / 2) as usize];
            buf.plane_size_in_bytes = [y.len(), cb.len(), cr.len()];
            assert_eq!(
                pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &buf),
                pw::pyrowave_result_PYROWAVE_SUCCESS,
                "frame {i} failed to decode"
            );
            out.push(y);
        }
        pw::pyrowave_decoder_destroy(dec);
        pw::pyrowave_device_destroy(dev);
        out
    }

    /// PSNR (dB) between two equal-sized 8-bit planes; `f64::INFINITY` when identical.
    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        let mse = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| {
                let d = x as f64 - y as f64;
                d * d
            })
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    /// Two frames in flight at once must match the synchronous depth-1 pictures, in
    /// order. Ground truth is this encoder's own depth-1 decode: raw AU bytes are not
    /// run-to-run reproducible. Flat fills hide a torn frame. Does not cover capture:
    /// `.process` requeues the SPA buffer while encode still holds a dup'd fd. Sets
    /// `max_inflight` directly; the shipped value stays 1.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn overlapping_two_frames_reproduces_the_synchronous_picture() {
        const FRAMES: u32 = 16;
        let (w, h) = (256u32, 256u32);
        // Odd seeds: `test_card` starts its LCG at `seed | 1`, so 2 and 3 build the same card.
        let cards: Vec<CapturedFrame> = (0..FRAMES).map(|i| test_card(w, h, 2 * i + 1)).collect();
        let open = || {
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open")
        };

        let mut enc = open();
        let sync: Vec<Vec<u8>> = cards
            .iter()
            .map(|c| {
                enc.submit(c).expect("sync submit");
                assert_eq!(
                    enc.inflight.len(),
                    1,
                    "submit must leave exactly one in flight"
                );
                enc.poll()
                    .expect("sync poll")
                    .expect("one AU per frame")
                    .data
            })
            .collect();
        drop(enc);

        let mut enc = open();
        enc.max_inflight = SLOTS;
        let mut overlapped: Vec<Vec<u8>> = Vec::new();
        let mut saw_two_in_flight = false;
        for c in &cards {
            enc.submit(c).expect("overlapped submit");
            saw_two_in_flight |= enc.inflight.len() == 2;
            if enc.inflight.len() >= SLOTS {
                overlapped.push(
                    enc.poll()
                        .expect("overlapped poll")
                        .expect("an AU once the pipeline is full")
                        .data,
                );
            }
        }
        enc.flush().expect("flush drains the tail");
        while let Some(au) = enc.poll().expect("tail poll") {
            overlapped.push(au.data);
        }
        drop(enc);
        assert!(
            saw_two_in_flight,
            "two frames were never actually in flight — this test proved nothing"
        );
        assert_eq!(
            overlapped.len(),
            sync.len(),
            "the overlapped run emitted a different number of AUs — a frame was lost"
        );

        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        let (sy, oy) = unsafe {
            (
                decode_stream_luma(w, h, &sync),
                decode_stream_luma(w, h, &overlapped),
            )
        };
        let mut worst = f64::INFINITY;
        for i in 0..sy.len() {
            let p = psnr(&sy[i], &oy[i]);
            worst = worst.min(p);
            // 45 dB is far above "looks the same"; a torn frame from two moving cards
            // lands in the teens. Wavelet RDO is not bit-reproducible.
            assert!(
                p > 45.0,
                "frame {i}: overlapped decode is {p:.1} dB from the synchronous one — the pipelined \
                 path changed the picture"
            );
            // A frame one position off still scores well against a neighbour. Must match its own.
            if i > 0 {
                let prev = psnr(&sy[i - 1], &oy[i]);
                assert!(
                    p > prev,
                    "frame {i} matches the PREVIOUS reference better ({prev:.1} dB) than its own \
                     ({p:.1} dB) — the pipeline is off by one"
                );
            }
        }
        eprintln!(
            "depth-2 vs depth-1 over {} frames: worst-case PSNR {}",
            sy.len(),
            if worst.is_infinite() {
                "identical (inf)".to_string()
            } else {
                format!("{worst:.1} dB")
            }
        );
    }
}
