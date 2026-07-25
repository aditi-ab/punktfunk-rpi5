//! PyroWave host encoder (design/pyrowave-codec-plan.md §4.3) — the opt-in wired-LAN
//! ultra-low-latency codec. Intra-only CDF 9/7 wavelet, pure Vulkan compute via the vendored
//! `pyrowave-sys` C API; measured 0.15–0.5 ms GPU encode at 1080p–4K on the RTX 5070 Ti
//! (Phase-0 microbench), vs 1–2 ms NVENC retrieve — and every frame is a keyframe, so the
//! whole IDR/RFI recovery apparatus is structurally unnecessary.
//!
//! Shape: the encoder owns a private ash instance/device (any Vulkan-1.3 GPU — this backend is
//! deliberately vendor-agnostic) shared with pyrowave via `pyrowave_create_device`, which
//! requires the original `VkInstanceCreateInfo`/`VkDeviceCreateInfo` to stay alive for the
//! device's lifetime — [`DeviceHold`] pins them. Frames enter as capture dmabufs (imported with
//! explicit DRM modifiers, cached per buffer) or CPU RGB (staging upload); the shared
//! `rgb2yuv.comp` BT.709-limited CSC writes an R8 luma image + an RG8 chroma image, which
//! pyrowave samples directly (two-component images synthesize the Cb/Cr planes via R/G view
//! swizzles — the documented NV12-style hand-off). Encode records into OUR command buffer
//! (`pyrowave_device_set_command_buffer`), so ingest + CSC + encode ride one submission; the
//! synchronous fence wait per frame is sub-millisecond by design (that is the codec's whole
//! point — overlapping frames buys nothing at this speed).
//!
//! MVP wire mapping (§4.4): the frame packetizes as ONE pyrowave packet (boundary = buffer
//! size) and ships as an opaque AU through the normal FEC/packetizer path, `keyframe = true`
//! on every AU. NOTE: until Phase 2 lands `CODEC_PYROWAVE` negotiation + a client decoder,
//! no shipping client can decode this — the backend is reachable only via an explicit
//! `PUNKTFUNK_ENCODER=pyrowave` and logs that loudly.
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

/// Same prebuilt RGB→(Y, interleaved-UV) BT.709-limited compute CSC the Vulkan Video backend
/// uses. PyroWave carries no VUI, so the colour contract is fixed by this shader: the Phase-2
/// client CSC must assume BT.709 limited range.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// The 4:4:4 twin (`rgb2yuv444.comp`): one invocation per pixel, full-res interleaved CbCr,
/// same BT.709-limited coefficients byte-for-byte.
const CSC444_SPV: &[u8] = include_bytes!("rgb2yuv444.spv");
/// Fixed cursor-overlay texture size (px) — mirrors `vulkan_video.rs`; the shared CSC shader bounds
/// sampling by its push constant, so one allocation fits every pointer bitmap.
const CURSOR_MAX: u32 = 256;
/// Max resident dmabuf imports (mirrors `vulkan_video.rs` — PipeWire cycles a small fixed pool).
const IMPORT_CACHE_CAP: usize = 16;
/// Headroom over the per-frame rate budget for the packetized bitstream (block headers + meta;
/// the rate controller itself never exceeds the budget).
const BS_SLACK: usize = 256 * 1024;

/// The DRM modifiers the PyroWave device can import as a SAMPLED image of the capture's
/// packed-RGB format. The capture advertises these for the pyrowave passthrough instead of
/// VAAPI's LINEAR-only policy — Mutter+NVIDIA never allocates LINEAR, but its tiled
/// dmabufs import fine through `VK_EXT_image_drm_format_modifier` (validated by upstream's
/// interop test). Instance + physical device only; probed per session setup (cheap).
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
        // The SAME device selection `open_inner` uses — these modifiers are what the capturer
        // will allocate against, so the two must never diverge (see `select_physical_device`).
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
                            // Single-memory-plane only: the capture hands one fd/offset/stride.
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

/// The render node whose owner the WP4.5 observability line names alongside the picked device:
/// `PUNKTFUNK_RENDER_NODE` (the house DRM-node override) else `/dev/dri/renderD128`.
/// **Log-only** — see [`select_physical_device`] for why no oracle, this one included, is
/// allowed to CHANGE the selection.
fn capture_anchor_node() -> std::path::PathBuf {
    std::env::var("PUNKTFUNK_RENDER_NODE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/dri/renderD128"))
}

/// `(major, minor)` of a device node, split the way `VkPhysicalDeviceDrmPropertiesEXT` reports
/// its render node (glibc `gnu_dev_major`/`gnu_dev_minor` encoding).
fn node_rdev(path: &std::path::Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfffu64);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xffu64);
    Some((major as i64, minor as i64))
}

/// The node's PCI address `(domain, bus, device, function)` from sysfs — the
/// `VK_EXT_pci_bus_info` fallback for drivers without `VK_EXT_physical_device_drm`.
fn node_pci_address(path: &std::path::Path) -> Option<(u32, u32, u32, u32)> {
    let node = path.file_name()?.to_str()?;
    let dev = std::fs::canonicalize(format!("/sys/class/drm/{node}/device")).ok()?;
    let addr = dev.file_name()?.to_str()?; // e.g. "0000:01:00.0"
    let (rest, func) = addr.rsplit_once('.')?;
    let mut parts = rest.split(':');
    let domain = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((domain, bus, device, u32::from_str_radix(func, 16).ok()?))
}

/// The Vulkan features pyrowave's encoder documents as required (pyrowave.h): shaderInt16,
/// storageBuffer8BitAccess, timeline semaphores, subgroup size control (1.3 core);
/// shaderFloat16 is optional. Checked AFTER selection, as it always was — folding it into the
/// selection predicate was considered for WP4.5 and rejected: a fall-through to a non-display
/// GPU can strand the session on a device that cannot import the capturer's buffers, which
/// feeds the process-wide raw-dmabuf latch, while the hard `bail!` is an immediate,
/// diagnosable, latch-free failure the session layer renegotiates around.
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

/// Does `pd` own the anchor render node? `VK_EXT_physical_device_drm`'s render major/minor is
/// the primary identity (unique per GPU — it disambiguates twin-model GPUs that
/// `(vendor, device)` cannot); `VK_EXT_pci_bus_info` against the node's sysfs PCI address is the
/// fallback. A device advertising neither extension never matches.
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

/// The physical device this backend will run on.
struct PickedDevice {
    pd: vk::PhysicalDevice,
    /// Index of a graphics+compute queue family on `pd` (pyrowave requires a graphics-capable
    /// queue in the device create info; the CSC + codec run on it).
    family: u32,
    vendor_id: u32,
    device_id: u32,
}

/// Pick pyrowave's physical device: the first non-CPU Vulkan device with a graphics+compute
/// family — **the pre-WP4.5 behaviour, kept by decision after two withdrawn attempts to
/// "fix" it**. What this function must never become:
///
/// - **Not `pf_gpu::selected_gpu()`** (withdrawn attempt #1): its Linux auto arm answers "the
///   NVIDIA GPU" whenever `/dev/nvidiactl` exists, regardless of which GPU the capture pipeline
///   runs on. On an Intel-compositor + NVIDIA-present laptop that moves the encoder onto the
///   one GPU that CANNOT import the compositor's dmabufs → five same-device rebuilds → the
///   process-wide raw-dmabuf latch degrades every later session to CPU capture, permanently.
/// - **Not the `/dev/dri/renderD128` render node** (withdrawn attempt #2, this session): render
///   minors are driver-BIND-ORDER artifacts, not display topology. On the common
///   AMD-iGPU + NVIDIA-display desktop (in-tree amdgpu binds before out-of-tree nvidia),
///   renderD128 is the idle iGPU while the compositor allocates on NVIDIA — anchoring there
///   deterministically picks a device that cannot import the capturer's buffers and trips the
///   same latch, with a success-looking log. The loader's first-device order is an
///   ICD-manifest lottery, but on that desktop class it usually lands on the NVIDIA device —
///   i.e. the status quo is accidentally right exactly where the anchor is deterministically
///   wrong.
///
/// The only correct oracle is *evidence of which device allocated the capture buffers* —
/// producer identity from the actual capture negotiation, threaded per session into this open.
/// That is real plumbing (capture and encode negotiate in different crates today) and belongs
/// to a change that can be measured on a hybrid rig; until then the selection stays put and
/// `open_inner` logs the picked device beside the house's two guesses so a field report can
/// finally SHOW a wrong-device topology instead of leaving it invisible.
///
/// ⚠ Shared by [`capture_modifiers`] and `open_inner` on purpose, and it must stay that way:
/// `capture_modifiers` advertises the DRM modifiers the CAPTURER then allocates for. If the two
/// disagreed about the device, capture would hand the encoder buffers it cannot import. Both
/// call sites being pure functions of the (process-stable) Vulkan device list is what makes the
/// agreement hold across an in-place resize's encoder re-open, which does NOT renegotiate
/// capture.
///
/// # Safety
/// `instance` must be live; only physical-device property/queue queries are issued against it.
unsafe fn select_physical_device(instance: &ash::Instance) -> Result<PickedDevice> {
    for pd in instance.enumerate_physical_devices()? {
        let props = instance.get_physical_device_properties(pd);
        if props.device_type == vk::PhysicalDeviceType::CPU {
            continue; // skip llvmpipe
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

/// Everything `pyrowave_create_device` requires to outlive the `pyrowave_device`: the create-info
/// structs (and every array/chain node they point into) used to build our instance + device. The
/// boxes pin the heap locations; moving the `DeviceHold` moves only the box pointers.
struct DeviceHold {
    _app_info: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _queue_prio: Box<[f32; 1]>,
    _queue_ci: Box<[vk::DeviceQueueCreateInfo<'static>; 1]>,
    _dev_exts: Box<[*const c_char; 3]>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

pub struct PyroWaveEncoder {
    // --- vulkan core (owned; private to this encoder) ---
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    family: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    _hold: DeviceHold,

    // --- pyrowave (borrows our device; destroyed before it) ---
    pw_dev: pw::pyrowave_device,
    pw_enc: pw::pyrowave_encoder,

    // --- CSC + planes (single slot: encode is synchronous per frame) ---
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    csc_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,

    // Cursor overlay (cursor-as-metadata): a fixed CURSOR_MAX² RGBA8 sampled image (bound at binding
    // 3) + host staging, re-uploaded only when the bitmap changes (`cursor_serial`). Single (not
    // ring) because PyroWave encodes one frame synchronously — no in-flight overlap to race.
    cursor_img: vk::Image,
    cursor_mem: vk::DeviceMemory,
    cursor_view: vk::ImageView,
    cursor_stage: vk::Buffer,
    cursor_stage_mem: vk::DeviceMemory,
    cursor_serial: u64,
    cursor_ready: bool,

    // Per-buffer dmabuf-import cache keyed by (st_dev, st_ino) — mirrors `vulkan_video.rs`.
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,
    // CPU-input staging (software capture / smoke tests), lazily (re)created on format change.
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
    /// Reused 3→4 expansion buffer for 24-bpp CPU payloads (`vk_util::normalize_cpu_rgb`).
    cpu_expand: Vec<u8>,

    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,

    // --- state ---
    width: u32,
    height: u32,
    fps: u32,
    /// Session-fixed negotiated chroma: 4:4:4 = full-res RG8 chroma plane + per-pixel CSC
    /// (`rgb2yuv444.comp`) + `Chroma444` pyrowave objects.
    chroma444: bool,
    /// Per-frame bitstream budget (hard CBR): `bitrate / (8 * fps)`.
    frame_budget: usize,
    /// Datagram-aligned mode (plan §4.4): packetize at this boundary and pad every codec
    /// packet to it, so each wire shard carries whole self-delimiting packets. `None` =
    /// one packet per AU (the dense MVP shape).
    wire_chunk: Option<usize>,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
    frame_count: u64,
}

// SAFETY: used only from the single encode thread; all Vulkan handles are owned and never shared
// (matches `VulkanVideoEncoder`'s `unsafe impl Send`). The pyrowave handles are only touched from
// that same thread, and pyrowave itself only submits GPU work inside API calls we make.
unsafe impl Send for PyroWaveEncoder {}

fn budget_for(bitrate_bps: u64, fps: u32) -> usize {
    ((bitrate_bps / (8 * fps.max(1) as u64)) as usize).max(64 * 1024)
}

impl PyroWaveEncoder {
    pub fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma: crate::ChromaFormat,
    ) -> Result<Self> {
        if !chroma.is_444() && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // Checked against the chroma actually being opened, NOT hardcoded 4:4:4. The 4:2:0 block
        // count is ~half of 4:4:4's but still unbounded (8192×6144 4:2:0 = 73728 > u16::MAX), and
        // the negotiator's 4:4:4 → 4:2:0 downgrade hands oversized modes to this open AS 4:2:0 —
        // so a `chroma.is_444()`-gated check is skipped exactly when it is needed. Wrapping the
        // index lets the resolve over-credit and `packetize` overshoot our bitstream buffer
        // (its own bounds `assert` is compiled out by the Release vendored build).
        // `validate_dimensions` rejects the impossible-at-any-chroma modes earlier; this is the
        // 4:4:4-specific half plus defence in depth for the lab override.
        if !crate::pyrowave_mode_fits_rdo(width, height, chroma.is_444()) {
            bail!(
                "pyrowave {} at {width}x{height} exceeds the rate controller's 16-bit block \
                 index (see pyrowave-sys patches/0002 note) — lower the resolution",
                if chroma.is_444() { "4:4:4" } else { "4:2:0" }
            );
        }
        // SAFETY: `open_inner` only issues Vulkan/pyrowave calls whose preconditions it
        // establishes itself (valid instance/device, correctly-chained create-infos that
        // `DeviceHold` keeps alive); all handles are freshly created and owned by the result.
        unsafe {
            Self::open_inner(
                width,
                height,
                fps.max(1),
                bitrate_bps.max(1_000_000),
                chroma.is_444(),
            )
        }
    }

    unsafe fn open_inner(w: u32, h: u32, fps: u32, bitrate: u64, chroma444: bool) -> Result<Self> {
        let entry = ash::Entry::load().context("load vulkan loader")?;

        let mut hold = DeviceHold {
            _app_info: Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3)),
            instance_ci: Box::new(vk::InstanceCreateInfo::default()),
            _queue_prio: Box::new([1.0f32]),
            _queue_ci: Box::new([vk::DeviceQueueCreateInfo::default()]),
            _dev_exts: Box::new([
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ]),
            _feat2: Box::new(vk::PhysicalDeviceFeatures2::default()),
            _v12: Box::new(vk::PhysicalDeviceVulkan12Features::default()),
            _v13: Box::new(vk::PhysicalDeviceVulkan13Features::default()),
            device_ci: Box::new(vk::DeviceCreateInfo::default()),
        };
        hold.instance_ci.p_application_info = &*hold._app_info;
        let instance = entry
            .create_instance(&hold.instance_ci, None)
            .context("create instance")?;

        // Between `create_instance` and `create_device` the only live resource is the instance,
        // so this whole stretch runs as one fallible block with a single manual destroy on its
        // error arm. From the device on, a partially-constructed `Self` (below) makes the
        // existing `Drop` the sole unwind path — these used to be a dozen `?`s that each leaked
        // everything created before them.
        // SAFETY: plain physical-device queries on the live instance just created, and a
        // `create_device` whose create-infos are pinned in `hold` for the call's duration.
        let selected = (|| unsafe {
            // The SAME selector `capture_modifiers` uses, so the two can never disagree about
            // the device (see `select_physical_device` — including why the selection itself is
            // deliberately unchanged by WP4.5).
            let picked = select_physical_device(&instance)?;
            let (pd, family) = (picked.pd, picked.family);
            // WP4.5, observability half (log-only BY DECISION — two selection "fixes" were
            // withdrawn after review; the rationale lives on `select_physical_device`): one
            // greppable line naming the picked device beside the house's two guesses at the
            // right one. On a multi-GPU host a wrong-device session used to be completely
            // invisible — a field report showed only downstream import failures. NO arm of this
            // is a WARN on purpose: a mismatch between these fields is not evidence of a wrong
            // pick (on an AMD-iGPU + NVIDIA-display desktop the loader's first device is right
            // and renderD128 is wrong; on the hybrid laptop it is the reverse), and a warning
            // that fires forever on healthy hosts only teaches people to ignore warnings.
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

            // Feature gate — pyrowave's documented encoder requirements; the re-query below
            // reads back the optionals mirrored into the device create-info (shaderFloat16,
            // vulkanMemoryModel, maintenance4).
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
            hold._v12.shader_float16 = have12.shader_float16; // optional, enable when present
            hold._v12.vulkan_memory_model = have12.vulkan_memory_model;
            hold._v12.vulkan_memory_model_device_scope = have12.vulkan_memory_model_device_scope;
            hold._v13.subgroup_size_control = vk::TRUE;
            hold._v13.compute_full_subgroups = vk::TRUE;
            hold._v13.synchronization2 = vk::TRUE;
            hold._v13.maintenance4 = have13.maintenance4;
            hold._feat2.p_next = &mut *hold._v12 as *mut _ as *mut std::ffi::c_void;
            hold._v12.p_next = &mut *hold._v13 as *mut _ as *mut std::ffi::c_void;

            hold._queue_ci[0] = vk::DeviceQueueCreateInfo::default().queue_family_index(family);
            hold._queue_ci[0].queue_count = 1;
            hold._queue_ci[0].p_queue_priorities = hold._queue_prio.as_ptr();
            hold.device_ci.p_next = &*hold._feat2 as *const _ as *const std::ffi::c_void;
            hold.device_ci.queue_create_info_count = 1;
            hold.device_ci.p_queue_create_infos = hold._queue_ci.as_ptr();
            hold.device_ci.enabled_extension_count = hold._dev_exts.len() as u32;
            hold.device_ci.pp_enabled_extension_names = hold._dev_exts.as_ptr();

            let device = instance
                .create_device(pd, &hold.device_ci, None)
                .context("create device")?;
            Ok((pd, family, device))
        })();
        let (pd, family, device) = match selected {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        let queue = device.get_device_queue(family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // Construct `Self` NOW, every not-yet-created resource at its null value, and assign
        // into it as resources come up. Any `?` from here drops `me`, and the existing `Drop`
        // tears down exactly the prefix that exists: it `device_wait_idle()`s first, null-guards
        // `pw_enc` (`pyrowave_encoder_destroy` dereferences before deleting),
        // `pyrowave_device_destroy(null)` is a plain `delete nullptr` (pyrowave_c.cpp) and
        // every `vkDestroy*`/`vkFree*` of a VK_NULL_HANDLE is the spec-defined no-op. One
        // teardown path serves both the error unwind and the normal drop, so an open-path leak
        // is unrepresentable rather than guarded (the c4c78129 shape, applied to ~20 resources).
        let mut me = Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            queue,
            family,
            mem_props,
            _hold: hold,
            pw_dev: std::ptr::null_mut(),
            pw_enc: std::ptr::null_mut(),
            csc_pipe: vk::Pipeline::null(),
            csc_layout: vk::PipelineLayout::null(),
            csc_dsl: vk::DescriptorSetLayout::null(),
            csc_pool: vk::DescriptorPool::null(),
            csc_set: vk::DescriptorSet::null(),
            sampler: vk::Sampler::null(),
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
            import_cache: Vec::new(),
            cpu_img: None,
            cpu_stage: None,
            cpu_expand: Vec::new(),
            cmd_pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            width: w,
            height: h,
            fps,
            chroma444,
            frame_budget: budget_for(bitrate, fps),
            wire_chunk: None,
            bitstream: Vec::new(),
            pending: VecDeque::new(),
            frame_count: 0,
        };

        // ---- hand the device to pyrowave (create-infos stay pinned in `me._hold` — pyrowave
        //      retains the pointers for the device's lifetime, and the Boxes' heap data does
        //      not move when `Self` does) ----
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: me.queue.as_raw() as pw::VkQueue,
            familyIndex: family,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            // SAFETY(cast): ash's loader entry point and bindgen's PFN type describe the same
            // C function pointer; the transmute only re-labels it.
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
            // Single-threaded over this private device (encode thread only) and pyrowave only
            // submits inside our API calls — no locking needed.
            queue_lock_callback: None,
            queue_unlock_callback: None,
            userdata: std::ptr::null_mut(),
        };
        pw_check(
            pw::pyrowave_create_device(&create, &mut me.pw_dev),
            "create_device",
        )?;
        // Our explicit command buffers live on a compute-capable family.
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
        pw_check(
            pw::pyrowave_encoder_create(&einfo, &mut me.pw_enc),
            "encoder_create",
        )?;

        // ---- CSC planes: full-res R8 luma + RG8 chroma (half-res for 4:2:0, full-res for
        //      4:4:4), storage-written by the CSC and sampled directly by pyrowave (R/G view
        //      swizzles synthesize Cb/Cr) ----
        let device = me.device.clone(); // cheap fn-table clone; lets `me.*` assignments interleave
        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };
        let (y_img, y_mem, y_view) = make_plain_image(
            &device,
            &me.mem_props,
            vk::Format::R8_UNORM,
            w,
            h,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        me.y_img = y_img;
        me.y_mem = y_mem;
        me.y_view = y_view;
        let (uv_img, uv_mem, uv_view) = make_plain_image(
            &device,
            &me.mem_props,
            vk::Format::R8G8_UNORM,
            cw,
            ch,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        me.uv_img = uv_img;
        me.uv_mem = uv_mem;
        me.uv_view = uv_view;

        // ---- CSC compute pipeline (same shader + layout as vulkan_video.rs) ----
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
            sb(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER), // cursor overlay
        ];
        me.csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [me.csc_dsl];
        // Push constant: cursor {ivec2 origin, ivec2 size} = 16 bytes (matches the shared CSC shader).
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
        // The module is consumed by pipeline creation either way — destroy it BEFORE `?`ing the
        // result, or the failure arm leaks it (it lives in no field). On failure the batch-of-1
        // out array is all VK_NULL_HANDLE per spec, so the discarded Err-arm vec holds nothing —
        // a future multi-entry batch could not assume that.
        device.destroy_shader_module(shader, None);
        me.csc_pipe = pipe_res.map_err(|(_, e)| e)?[0];

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                // binding 0 (RGB) + binding 3 (cursor).
                .descriptor_count(2),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2),
        ];
        me.csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        me.csc_set = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(me.csc_pool)
                .set_layouts(&dsls),
        )?[0];
        // Cursor overlay: fixed CURSOR_MAX² RGBA8 sampled image + host staging (bound at binding 3).
        let (cursor_img, cursor_mem, cursor_view) = make_plain_image(
            &device,
            &me.mem_props,
            vk::Format::R8G8B8A8_UNORM,
            CURSOR_MAX,
            CURSOR_MAX,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        me.cursor_img = cursor_img;
        me.cursor_mem = cursor_mem;
        me.cursor_view = cursor_view;
        let (cursor_stage, cursor_stage_mem) = make_host_buffer(
            &device,
            &me.mem_props,
            (CURSOR_MAX * CURSOR_MAX * 4) as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
        )?;
        me.cursor_stage = cursor_stage;
        me.cursor_stage_mem = cursor_stage_mem;
        // Bindings 1/2 (Y, UV storage targets) + 3 (cursor sampler) are fixed for the encoder's life.
        let yi = [vk::DescriptorImageInfo::default()
            .image_view(me.y_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let uvi = [vk::DescriptorImageInfo::default()
            .image_view(me.uv_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let curi = [vk::DescriptorImageInfo::default()
            .sampler(me.sampler)
            .image_view(me.cursor_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        device.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(me.csc_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(&yi),
                vk::WriteDescriptorSet::default()
                    .dst_set(me.csc_set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(&uvi),
                vk::WriteDescriptorSet::default()
                    .dst_set(me.csc_set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&curi),
            ],
            &[],
        );

        me.cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        me.cmd = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(me.cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0];
        me.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;

        let props = me.instance.get_physical_device_properties(pd);
        tracing::info!(
            gpu = %props.device_name_as_c_str().unwrap_or(c"?").to_string_lossy(),
            mode = %format!("{w}x{h}@{fps}"),
            budget_kib = me.frame_budget / 1024,
            chroma = if chroma444 { "4:4:4" } else { "4:2:0" },
            "PyroWave encoder open (intra-only wavelet, BT.709 limited)"
        );

        Ok(me)
    }

    /// Point CSC binding 0 at this frame's RGB view.
    unsafe fn bind_rgb(&self, rgb_view: vk::ImageView) {
        let ii = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(self.csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii)],
            &[],
        );
    }

    /// Cursor-as-metadata: bring the cursor image up to date for this frame and return the shader
    /// push constant `[origin_x, origin_y, size_w, size_h]` (size 0 ⇒ the CSC skips the blend).
    /// Records the small upload (only when the bitmap `serial` changed) + layout transition into
    /// `cmd`, ahead of the CSC dispatch that samples binding 3. Encode is synchronous, so the single
    /// shared image never races a prior frame; the first use transitions it to SHADER_READ_ONLY.
    unsafe fn prep_cursor(&mut self, cursor: Option<&pf_frame::CursorOverlay>) -> Result<[i32; 4]> {
        let dev = self.device.clone();
        let cmd = self.cmd;
        let img = self.cursor_img;
        let ready = self.cursor_ready;
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
                if self.cursor_serial != c.serial {
                    let bytes = (cw as usize) * (ch as usize) * 4;
                    let ptr = dev.map_memory(
                        self.cursor_stage_mem,
                        0,
                        bytes as u64,
                        vk::MemoryMapFlags::empty(),
                    )?;
                    std::ptr::copy_nonoverlapping(
                        c.rgba.as_ptr(),
                        ptr as *mut u8,
                        bytes.min(c.rgba.len()),
                    );
                    dev.unmap_memory(self.cursor_stage_mem);
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
                        self.cursor_stage,
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
                    self.cursor_serial = c.serial;
                    self.cursor_ready = true;
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
                    self.cursor_ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Import a dmabuf with per-buffer caching — same policy as `vulkan_video.rs::import_cached`.
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
        // Feed pf-zerocopy's raw-dmabuf degrade latch (the one 3efbe416 wired for the libav
        // path): a driver that deterministically refuses what the compositor allocates refuses
        // it identically forever, and only the latch — which flips capture to CPU delivery from
        // the next session — recovers the host. The CPU path serves every format capture
        // negotiates (24-bpp included, see `normalize_cpu_rgb`), so the degrade lands somewhere
        // that works. Transient allocation OOM is excluded (`import_failure_feeds_latch`).
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

    /// CPU RGB staging (software capture / smoke tests) — mirrors `vulkan_video.rs::ensure_cpu_rgb`.
    unsafe fn ensure_cpu_rgb(&mut self, fmt: vk::Format, bytes: &[u8]) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        let need = (w * h * 4) as u64;
        if self.cpu_img.map(|(_, _, _, f)| f) != Some(fmt) {
            if let Some((i, m, v, _)) = self.cpu_img.take() {
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
            self.cpu_img = Some((i, m, v, fmt));
        }
        if self.cpu_stage.map(|(_, _, s)| s < need).unwrap_or(true) {
            if let Some((b, m, _)) = self.cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let (buf, mem) = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            self.cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.cpu_stage.unwrap();
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        let n = bytes.len().min(need as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        dev.unmap_memory(m);
        Ok(self.cpu_img.unwrap().2)
    }

    /// One frame, synchronously: ingest → CSC → pyrowave encode (recorded into our command
    /// buffer) → submit + fence wait (sub-ms) → packetize into an `EncodedFrame`.
    unsafe fn encode_frame(&mut self, frame: &CapturedFrame) -> Result<()> {
        // A failed `reset()` leaves the encoder destroyed and null. Callers today turn that into
        // a session error and never resubmit, but a null here would be a use-after-free inside
        // pyrowave rather than a clean error — so fail loudly instead of relying on that.
        anyhow::ensure!(
            !self.pw_enc.is_null(),
            "pyrowave: encode after a failed reset (encoder was destroyed and not rebuilt)"
        );
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // The frame must be exactly the session's mode (WP4.5). PyroWave applies NO alignment —
        // `width`/`height` are the negotiated mode verbatim — so any mismatch is a bug somewhere,
        // and until now it was a SILENT one: the frame was encoded edge-smeared or cropped.
        //
        // Every other Linux backend already refuses exactly this, with this shape, in `submit`:
        // libav-NVENC (`linux/mod.rs`), VAAPI (`linux/vaapi.rs`) and openh264 (`sw.rs`) all carry
        // the same `ensure!`. PyroWave was the only one that didn't. (`vulkan_video.rs` bails in
        // its Dmabuf arms only — its CSC path and its CPU arm do not — so it is the weaker
        // precedent, not the model.)
        //
        // Mostly this is a wrong-picture bug rather than a memory-safety one: `rgb2yuv.comp`
        // clamps every fetch with `min(p, textureSize - 1)` and the CPU arm uploads
        // `min(len, need)` into a session-sized image. But it also closes a narrow real hazard —
        // `import_cached` keys on `(st_dev, st_ino)` and returns the cached `VkImage` on a hit
        // WITHOUT rechecking the extent, and unlike the capture side it is never cleared on a
        // renegotiation. A dmabuf inode recycled across a SHRINKING renegotiation would hand the
        // encoder an image sized for the old, larger allocation. After this check every frame
        // reaching that cache has the session's dimensions, so the size-change route is closed.
        //
        // ⚠ A mismatch is NOT always transient. A compositor-initiated PipeWire renegotiation
        // updates the capturer's size in place and signals nothing the encode loop reads, so this
        // can be a permanent new steady state — and `reset()` reopens at the SAME dimensions by
        // construction, so the host's five-reset budget cannot fix it (≈3.1 s of frozen stream,
        // then the session ends). That is still better than shipping a smeared picture forever,
        // but the real fix is for the host to treat this error as a PIPELINE rebuild rather than
        // an encoder reset. Filed; not this commit.
        if frame.width != w || frame.height != h {
            bail!(
                "pyrowave: frame {}x{} != session mode {w}x{h} — refusing a mismatched encode \
                 source",
                frame.width,
                frame.height
            );
        }
        // Everything from `begin` through `queue_submit` runs in one closure whose error arm
        // resets `self.cmd`. On every arm inside it the reset is LEGAL: a mid-recording failure
        // leaves RECORDING, a failed `end` leaves INVALID, a failed `queue_submit` enqueued
        // nothing — never PENDING (the pool carries RESET_COMMAND_BUFFER). Failures AFTER the
        // closure (the fence wait, packetize) must NOT reset: a fence timeout leaves the buffer
        // PENDING, where a reset violates VUID-vkResetCommandBuffer-commandBuffer-00045 — those
        // paths propagate untouched and the recovery (`reset()`/`Drop`) `device_wait_idle()`s
        // before anything touches `cmd`; a buffer that completed its one-time submit is INVALID,
        // which the next `begin` may implicitly reset.
        let record_and_submit = (|| -> Result<()> {
            dev.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // Cursor-as-metadata: refresh the cursor image (only when the bitmap changed) + get the
            // shader push constant. Recorded into `self.cmd` before the CSC dispatch samples binding 3.
            let cursor_pc = self.prep_cursor(frame.cursor.as_ref())?;

            // ---- ingest RGB (same barrier discipline as vulkan_video.rs) ----
            let rgb_view = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    let (old, src_qf, dst_qf) = if fresh {
                        (
                            vk::ImageLayout::UNDEFINED,
                            vk::QUEUE_FAMILY_FOREIGN_EXT,
                            self.family,
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
                        self.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    view
                }
                FramePayload::Cpu(bytes) => {
                    // 24-bpp Rgb/Bgr expands 3→4 first (see `normalize_cpu_rgb`) — refusing it here
                    // used to kill the session at its first frame with no fallback.
                    let mut scratch = std::mem::take(&mut self.cpu_expand);
                    let (norm_fmt, norm_bytes) =
                        normalize_cpu_rgb(frame.format, bytes, &mut scratch, false);
                    let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                    let view = match fmt {
                        Ok(f) => self.ensure_cpu_rgb(f, norm_bytes),
                        Err(e) => Err(e),
                    };
                    self.cpu_expand = scratch;
                    let view = view?;
                    let (img, ..) = self.cpu_img.unwrap();
                    let (stage, ..) = self.cpu_stage.unwrap();
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
                        self.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        self.cmd,
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
                        self.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    view
                }
                _ => bail!("pyrowave: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            self.bind_rgb(rgb_view);

            // y/uv -> GENERAL for the CSC's storage writes (discard prior contents — the previous
            // frame's encode already completed under our synchronous fence, which is also the
            // "execution barrier before writing to images" pyrowave's contract asks for).
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
                self.cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_general(self.y_img), to_general(self.uv_img)]),
            );
            dev.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
            dev.cmd_bind_descriptor_sets(
                self.cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.csc_layout,
                0,
                &[self.csc_set],
                &[],
            );
            let mut pc_bytes = [0u8; 16];
            for (i, v) in cursor_pc.iter().enumerate() {
                pc_bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            dev.cmd_push_constants(
                self.cmd,
                self.csc_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &pc_bytes,
            );
            // 4:2:0: one invocation per 2x2 luma block (per chroma sample); 4:4:4: per pixel.
            if self.chroma444 {
                dev.cmd_dispatch(self.cmd, w.div_ceil(8), h.div_ceil(8), 1);
            } else {
                dev.cmd_dispatch(self.cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);
            }

            // CSC storage writes -> pyrowave's sampled reads (images stay GENERAL — the layout
            // pyrowave's GPU-buffer contract accepts without transitions).
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
                self.cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_sampled(self.y_img), to_sampled(self.uv_img)]),
            );

            // ---- pyrowave encode, recorded into OUR command buffer ----
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
                        self.y_img,
                        w,
                        h,
                        r8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                    ),
                    // Two-component chroma image: view swizzles R/G synthesize the Cb/Cr planes
                    // (the documented NV12-style hand-off, pyrowave.h `pyrowave_gpu_buffers`).
                    // The view extent is the chroma IMAGE's own mip0 extent (it's a separate
                    // image, not a planar aspect): half-res for 4:2:0, full-res for 4:4:4.
                    plane(
                        self.uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        rg8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                    ),
                    plane(
                        self.uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        rg8,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_G,
                    ),
                ],
            };
            let rc = pw::pyrowave_rate_control {
                maximum_bitstream_size: self.frame_budget,
            };
            pw::pyrowave_device_set_command_buffer(
                self.pw_dev,
                self.cmd.as_raw() as usize as pw::VkCommandBuffer,
            );
            let enc_res = pw::pyrowave_encoder_encode_gpu_synchronous(
                self.pw_enc,
                std::ptr::null(),
                std::ptr::null(),
                &buffers,
                &rc,
            );
            pw::pyrowave_device_set_command_buffer(self.pw_dev, std::ptr::null_mut());
            pw_check(enc_res, "encode_gpu_synchronous")?;

            dev.end_command_buffer(self.cmd)?;
            dev.reset_fences(&[self.fence])?;
            let cmds = [self.cmd];
            dev.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                self.fence,
            )?;
            Ok(())
        })();
        if let Err(e) = record_and_submit {
            // SAFETY: on every closure error arm the buffer is RECORDING/INVALID/EXECUTABLE —
            // never PENDING (nothing was enqueued) — and the pool allows the reset.
            let _ = dev.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty());
            return Err(e);
        }
        dev.wait_for_fences(&[self.fence], true, 5_000_000_000)
            .context("pyrowave encode fence")?;

        // ---- packetize ----
        // Dense (default): boundary = whole buffer → the AU is exactly one pyrowave packet.
        // Datagram-aligned (§4.4, `set_wire_chunking`): boundary = the wire shard payload;
        // each codec packet is zero-padded to the boundary so every shard carries whole
        // self-delimiting packets — the client windows its parse and a lost shard costs
        // only those blocks. Padding cost is small: the packetizer fills close to the
        // boundary by design.
        let cap = self.frame_budget + BS_SLACK;
        self.bitstream.resize(cap, 0);
        // Chunked mode reserves the 4-byte window prefix from the packetize boundary (shared helper).
        let boundary = crate::pyrowave_wire::packet_boundary(self.wire_chunk, cap);
        let mut n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_compute_num_packets(self.pw_enc, boundary, &mut n),
            "compute_num_packets",
        )?;
        if n == 0 || (self.wire_chunk.is_none() && n != 1) {
            bail!("pyrowave: unexpected packet count {n} at boundary {boundary}");
        }
        let mut packets = vec![pw::pyrowave_packet { offset: 0, size: 0 }; n];
        let mut out_n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_packetize(
                self.pw_enc,
                packets.as_mut_ptr(),
                boundary,
                &mut out_n,
                self.bitstream.as_mut_ptr() as *mut std::ffi::c_void,
                cap,
            ),
            "packetize",
        )?;
        packets.truncate(out_n.max(1));
        // Correct pyrowave's zeroed sequence-header VUI: it signals ycbcr_range=FULL, but our CSC
        // emits BT.709 LIMITED — patch the bits HONEST so VUI-honoring clients don't wash out
        // blacks. (Linux capture has no HDR path, so this side never stamps BT.2020/PQ.)
        if let Some(p) = packets.first() {
            crate::pyrowave_wire::stamp_color_bits(&mut self.bitstream, p.offset, false);
        }
        // Frame into the wire AU via the shared helper (byte-identical on Linux + Windows): the dense
        // single packet, or the datagram-aligned windowed AU (§4.4).
        let pkts: Vec<(usize, usize)> = packets.iter().map(|p| (p.offset, p.size)).collect();
        let au = crate::pyrowave_wire::build_au(&pkts, &self.bitstream, self.wire_chunk);
        self.frame_count += 1;
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: frame.pts_ns,
            // Every frame is independently decodable — SOF/keyframe on each AU is the codec's
            // whole recovery story (plan §1.2).
            keyframe: true,
            recovery_anchor: false,
            chunk_aligned: self.wire_chunk.is_some(),
        });
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // SAFETY: single-threaded encoder; `encode_frame` records/submits on handles this
        // struct owns and waits its own fence before touching results. Command-buffer state on
        // failure is `encode_frame`'s own business now: its record-and-submit closure resets the
        // buffer on every pre-submit failure, and its post-submit failures (fence timeout —
        // buffer possibly PENDING) deliberately do NOT reset, because that violates
        // VUID-vkResetCommandBuffer-commandBuffer-00045; the blanket reset that used to live
        // here fired on exactly that path. Recovery (`reset()`/`Drop`) waits the device idle
        // before anything touches `cmd` again.
        unsafe { self.encode_frame(frame) }
    }

    fn caps(&self) -> EncoderCaps {
        // No RFI / no intra-refresh wave (every frame is intra). Report the real opened chroma so
        // the session glue's post-open cross-check stays quiet on a genuine 4:4:4 session — a
        // hardcoded `default()` here mis-reports a 4:4:4 open as 4:2:0 and fires a spurious
        // "chroma disagrees with the negotiated Welcome" warn.
        EncoderCaps {
            // The wavelet CSC composites the metadata cursor.
            blends_cursor: true,
            chroma_444: self.chroma444,
            ..EncoderCaps::default()
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.pop_front())
    }

    fn reset(&mut self) -> bool {
        // Cheap in-place rebuild: recreate only the pyrowave encoder object — there is no
        // rate-control history or reference state worth preserving (plan §4.3).
        // SAFETY: the device is idle for this encoder's work (submit waits its fence) and the
        // pyrowave device outlives the encoder object being swapped.
        unsafe {
            self.device.device_wait_idle().ok();
            pw::pyrowave_encoder_destroy(self.pw_enc);
            // Publish the null IMMEDIATELY: the create below is fallible, and its failure path
            // must not leave a freed pointer in the field. `pyrowave_encoder_destroy` is a plain
            // `delete` (pyrowave_c.cpp) with no null check, so `Drop` running on a stale handle
            // is a double free — the exact shape this reset hits when the rebuild fails because
            // the device is already lost, which is the state that made the watchdog fire.
            self.pw_enc = std::ptr::null_mut();
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
            let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
            let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
            if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                tracing::error!(result = ?r, "pyrowave: encoder rebuild failed");
                // `pw_enc` stays null — `Drop` and `encode_frame` both guard on it. The queued
                // AUs are forfeit either way (the caller turns a false reset into a session
                // error), so drop them rather than shipping output from a dead encoder.
                self.pending.clear();
                return false;
            }
            self.pw_enc = enc;
        }
        self.pending.clear();
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Rate control is a plain per-frame byte budget — an in-place retarget is free (no
        // IDR, nothing in flight). NOTE: Phase 3 pins the session rate and bypasses ABR
        // (plan §4.6 — wavelet quality collapses well above the AIMD floor); until then this
        // faithfully applies whatever the caller asks.
        self.frame_budget = budget_for(bps, self.fps);
        tracing::debug!(
            mbps = bps / 1_000_000,
            budget_kib = self.frame_budget / 1024,
            "pyrowave: per-frame rate budget retargeted in place"
        );
        true
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        // Sanity floor: a boundary below one block header + payload word is meaningless.
        if shard_payload >= 64 {
            self.wire_chunk = Some(shard_payload);
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization on (partial-frame loss mode)"
            );
        }
    }

    fn flush(&mut self) -> Result<()> {
        // Synchronous per-frame encode: nothing buffered beyond `pending`.
        Ok(())
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // SAFETY: owned handles, destroyed exactly once, GPU idled first; pyrowave objects go
        // before the VkDevice they borrow (encoder before device, per pyrowave.h).
        // This is also `open_inner`'s ONLY unwind path: it constructs `Self` right after
        // `create_device` with every later resource at its null value and assigns as they come
        // up, so on a failed open this runs against a partial prefix. That is sound because
        // `pyrowave_device_destroy(null)` is a bare `delete nullptr` (pyrowave_c.cpp — safe
        // no-op) and every `vkDestroy*`/`vkFree*` of VK_NULL_HANDLE is the spec-defined no-op;
        // `pw_enc` is the one null-UNSAFE destroy and carries its own guard below.
        unsafe {
            self.device.device_wait_idle().ok();
            // Null when a failed `reset()` already destroyed it — `pyrowave_encoder_destroy`
            // is not null-safe.
            if !self.pw_enc.is_null() {
                pw::pyrowave_encoder_destroy(self.pw_enc);
            }
            pw::pyrowave_device_destroy(self.pw_dev);
            for (_, _, i, m, v) in self.import_cache.drain(..) {
                self.device.destroy_image_view(v, None);
                self.device.destroy_image(i, None);
                self.device.free_memory(m, None);
            }
            if let Some((i, m, v, _)) = self.cpu_img.take() {
                self.device.destroy_image_view(v, None);
                self.device.destroy_image(i, None);
                self.device.free_memory(m, None);
            }
            if let Some((b, m, _)) = self.cpu_stage.take() {
                self.device.destroy_buffer(b, None);
                self.device.free_memory(m, None);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_descriptor_pool(self.csc_pool, None);
            self.device.destroy_pipeline(self.csc_pipe, None);
            self.device.destroy_pipeline_layout(self.csc_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.csc_dsl, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_image_view(self.y_view, None);
            self.device.destroy_image(self.y_img, None);
            self.device.free_memory(self.y_mem, None);
            self.device.destroy_image_view(self.uv_view, None);
            self.device.destroy_image(self.uv_img, None);
            self.device.free_memory(self.uv_mem, None);
            self.device.destroy_image_view(self.cursor_view, None);
            self.device.destroy_image(self.cursor_img, None);
            self.device.free_memory(self.cursor_mem, None);
            self.device.destroy_buffer(self.cursor_stage, None);
            self.device.free_memory(self.cursor_stage_mem, None);
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
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// BT.709 limited-range YCbCr of an 8-bit RGB fill — the same math as `rgb2yuv.comp`.
    fn bt709(fill: [u8; 4]) -> (f64, f64, f64) {
        let (b, g, r) = (fill[0] as f64, fill[1] as f64, fill[2] as f64); // BGRA order
        (
            16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b,
            128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b,
            128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b,
        )
    }

    /// Decode an AU with a standalone pyrowave decoder and return the full planar YUV
    /// (half-res chroma for 4:2:0, full-res for 4:4:4). This is the golden oracle for the
    /// smoke checks (plane means) and the Apple Metal port's committed PSNR fixtures
    /// (`pyrowave_dump_golden`).
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
        // SAFETY: forwarded — same contract as the caller.
        unsafe { decode_planes_chroma(w, h, au, false) }
    }

    /// Plane means of an upstream-decoded AU — the smoke assertion.
    unsafe fn decode_plane_means(w: u32, h: u32, au: &[u8], chroma444: bool) -> (f64, f64, f64) {
        // SAFETY: forwarded — same contract as the caller.
        let (y, cb, cr) = unsafe { decode_planes_chroma(w, h, au, chroma444) };
        let mean = |v: &[u8]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
        (mean(&y), mean(&cb), mean(&cr))
    }

    /// Full open → CSC → GPU encode → packetize path through the real encoder, then each AU
    /// CPU-decoded by upstream's own decoder and PSNR-checked against the CSC's BT.709 math.
    /// `#[ignore]`d: needs a real Vulkan 1.3 GPU — build anywhere, run on a GPU host:
    ///   cargo test -p punktfunk-host --features pyrowave --no-run
    ///   <host> target/debug/deps/punktfunk_host-<hash> --ignored --nocapture pyrowave_smoke
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

        // Datagram-aligned mode (§4.4): every emitted AU is a whole number of framed
        // windows — 4-byte prefix (used-length + kind), whole packets or FRAG chains for
        // oversized atomic blocks, zero padding after `used`. Walking + reassembling the
        // fragments must reproduce a decodable packet stream.
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

        // In-place rate retarget + encoder rebuild both keep encoding.
        assert!(enc.reconfigure_bitrate(100_000_000));
        assert!(enc.reset());
        enc.submit(&cpu_frame(w, h, 999, [10, 20, 30, 255]))
            .expect("submit after reset");
        assert!(enc.poll().expect("poll").is_some());
    }

    /// The 4:4:4 twin of `pyrowave_smoke`: per-pixel CSC into full-res RG8 chroma +
    /// `Chroma444` pyrowave objects, verified by upstream's own 4:4:4 CPU decode. The
    /// busy-card leg then drives the rate controller at the ~2.6 bpp operating point —
    /// exactly the regime that overran upstream's 4:2:0-sized payload staging before
    /// 24-bpp packed CPU frame (`PixelFormat::Rgb`/`Bgr`) — what the PipeWire portal negotiates
    /// when dmabuf delivery is off. `rgb` is given as (r, g, b) regardless of `fmt`'s byte order.
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

    /// WP5.4: 24-bpp CPU payloads are SERVED (3→4 expand, `vk_util::normalize_cpu_rgb`), not
    /// refused — the refusal used to kill the session at its first frame with no fallback.
    /// Channel order is the load-bearing assertion: an expand that swaps R/B or misplaces the
    /// pad byte moves the decoded chroma means by tens of codes.
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

    /// WP4.5: a frame that is not the session's mode must be REFUSED, not encoded. PyroWave
    /// applies no alignment, so a mismatch can only be a stale frame from a renegotiated mode —
    /// and the failure is silent without this check (`rgb2yuv.comp` clamps its fetches and the CPU
    /// arm uploads `min(len, need)`), so it ships an edge-smeared or cropped picture rather than
    /// erroring. Both directions, since undersized and oversized smear in opposite ways. The
    /// session must still be usable afterwards: the refusal happens before anything is recorded,
    /// so a correctly-sized frame right after must encode normally.
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
        // A refusal must enqueue NOTHING. Checked before the good frame, because `pending` is a
        // queue: asserting `is_some()` after a successful submit would pass even if the three
        // refusals had each pushed an AU, and would then be measuring the wrong one.
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

    /// WP5.1: a failed dmabuf import must leak neither the dup'd fd nor the VkImage. Drives
    /// `vk_util::import_rgb_dmabuf` DIRECTLY — not `import_cached` — so the deliberate failures
    /// cannot feed pf-zerocopy's raw-dmabuf degrade latch (which never un-latches by design).
    /// Two failure shapes alternate: a garbage DRM modifier fails at `create_image` (the OwnedFd
    /// drops), and a LINEAR memfd — not a real dmabuf — fails at `allocate_memory` (the error arm
    /// drops it). Before the OwnedFd rework each failure leaked one fd, observable right here.
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
            // SAFETY: sizing the owned memfd so an mmap-happy driver sees real pages.
            unsafe { libc::ftruncate(fd.as_raw_fd(), 64 * 64 * 4) };
            pf_frame::DmabufFrame {
                fd,
                fourcc: 0x3432_5258, // XR24 — maps, so the failure lands PAST the fourcc gate
                modifier,
                plane1: None,
                offset: 0,
                stride: 64 * 4,
            }
        };
        let fd_count = || std::fs::read_dir("/proc/self/fd").expect("procfs").count();
        // Warm any lazily-opened driver/loader descriptors before taking the baseline.
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
            // SAFETY: as above.
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

    /// `patches/0001-payload-data-444-sizing.patch` (the Phase-0 finding): it must stay
    /// within budget, decode, and be run-to-run deterministic (the overrun was not).
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

        // Busy content at the 4:4:4 operating point (~2.6 bpp).
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
            // Upstream's own decoder accepts it (a corrupt stream errors or garbles).
            // SAFETY: test-only FFI with locally-owned buffers.
            let _ = unsafe { decode_planes_chroma(w, h, &au.data, true) };
            sizes.push(au.data.len());
        }
        assert!(
            sizes.windows(2).all(|s| s[0] == s[1]),
            "identical input produced varying AU sizes (the Phase-0 overrun signature): {sizes:?}"
        );
    }

    /// A deterministic busy BGRA test card (gradients + checker + LCG noise) — flat fills
    /// exercise almost none of the entropy decoder, this hits every subband.
    fn test_card(w: u32, h: u32, seed: u32) -> CapturedFrame {
        let mut rng = seed | 1;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                let i = ((y * w + x) * 4) as usize;
                let checker = if (x / 16 + y / 16) % 2 == 0 { 48 } else { 0 };
                let noise = (rng >> 24) as u8 / 8;
                buf[i] = ((x * 255 / w) as u8).saturating_add(noise); // B
                buf[i + 1] = ((y * 255 / h) as u8).saturating_add(checker); // G
                buf[i + 2] = (((x + y) * 255 / (w + h)) as u8).saturating_add(noise); // R
                buf[i + 3] = 255;
            }
        }
        CapturedFrame {
            width: w,
            height: h,
            pts_ns: seed as u64 * 16_666_667,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// Dump the Apple Metal port's golden fixtures (plan §4.7): host-encoded AUs (dense AND
    /// chunk-aligned) plus upstream's own decode of each as raw YUV420P planes. The Swift test
    /// (PyroWaveGoldenTests.swift) PSNR-matches the Metal decode against these — float wavelet
    /// math is not bit-exact across implementations, upstream itself ships precision variants.
    /// `#[ignore]`d GPU test; regenerate on a Vulkan 1.3 host:
    ///   cargo test -p punktfunk-host --features pyrowave --no-run
    ///   PYROWAVE_GOLDEN_DIR=/tmp/golden <bin> --ignored --nocapture pyrowave_dump_golden
    /// then copy the files into clients/apple/Tests/PunktfunkKitTests/PyroWaveFixtures/.
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

        // Odd-block geometry on purpose: 256 aligns clean, 144 → aligned 160 exercises the
        // block-grid overhang. ~1.6 bpp at 60 fps.
        let (w, h) = (256u32, 144u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 4_000_000, crate::ChromaFormat::Yuv420).expect("open");

        let dump = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(name), bytes).expect("write fixture");
            eprintln!("wrote {name}: {} bytes", bytes.len());
        };

        // Dense AU + upstream-decoded reference planes.
        enc.submit(&test_card(w, h, 7)).expect("submit");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(!au.chunk_aligned);
        dump("au-dense.bin", &au.data);
        // SAFETY: test-only FFI with locally-owned buffers.
        let (y, cb, cr) = unsafe { decode_planes(w, h, &au.data) };
        dump("ref-dense-y.bin", &y);
        dump("ref-dense-cb.bin", &cb);
        dump("ref-dense-cr.bin", &cr);

        // Chunk-aligned AU of a DIFFERENT frame (its own reference): the Swift window walk +
        // FRAG reassembly must reproduce the packet stream.
        enc.set_wire_chunking(1408);
        enc.submit(&test_card(w, h, 11)).expect("chunked submit");
        let au = enc.poll().expect("poll").expect("chunked AU");
        assert!(au.chunk_aligned);
        assert_eq!(au.data.len() % 1408, 0);
        dump("au-chunked.bin", &au.data);
        // SAFETY: test-only FFI with locally-owned buffers.
        let (y, cb, cr) = unsafe {
            // Feed upstream through the same framed walk the clients use.
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

        // 4:4:4 dense AU + its reference (full-res chroma planes) — the Apple 4:4:4 layout's
        // golden (design/pyrowave-444-hdr.md Phase 4). Same odd-block geometry.
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
}
