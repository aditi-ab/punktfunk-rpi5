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
// UNSAFE-LINT EXEMPTION (rationale + exit criteria: `unsafe_op_in_unsafe_fn` in the workspace
// Cargo.toml). This body is `pyrowave-sys` C-API and ash/Vulkan compute calls almost line for line;
// narrowing it would add one `unsafe {}` plus one SAFETY comment per call that could only restate
// the signature. Clearing this file means DELETING the markers that carry no caller contract, not
// wrapping the calls — until then the lint is off HERE and enforced everywhere else.
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
    /// The elevated global-priority request chained into `_queue_ci[0].p_next`
    /// (`PYROWAVE_QUEUE_PRIORITY`, see `queue_priority_candidates`). A `Box` for the same
    /// move-stability reason as its siblings: `pyrowave_create_device` RETAINS `device_ci` for
    /// the device's lifetime and Granite reads the chain back through
    /// `get_existing_create_info()`, so this must stay put and — critically — must describe the
    /// device that actually got created. The create ladder below is therefore required to write
    /// its FINAL state back here (null the `p_next` if the no-priority attempt is the one that
    /// succeeded); a chain the device was not created with is a lie Granite would believe.
    _queue_gp: Box<[vk::DeviceQueueGlobalPriorityCreateInfoKHR<'static>; 1]>,
    // A plain Vec (not Box<[_; N]> like its siblings): Phase 8 pushes queue_family_foreign
    // conditionally. The heap buffer as_ptr() feeds device_ci is move-stable like the Boxes.
    _dev_exts: Vec<*const c_char>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

/// Percentile of a sorted sample slice, by nearest-rank. **Pure.** Used for the `PUNKTFUNK_PERF`
/// encode split: a p99 is the whole point here (the game-load spike this codec suffers is a TAIL
/// event — the mean barely moves while individual frames go 2 ms → 18 ms), so a mean-only readout
/// would report "fine" through exactly the failure the priority lever exists to fix.
fn pct(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The global-priority classes to try, in order, for `PYROWAVE_QUEUE_PRIORITY`.
///
/// **Pure, and character-identical to the vendored C patch's grammar**
/// (`patches/0005-global-priority-queue.patch`, `context.cpp` ~2136-2152): unset → `realtime`;
/// ASCII-lowercased; `off` → no candidates at all; `high` → `[HIGH]`; anything else, including
/// junk → `[REALTIME, HIGH]`. Keeping the two grammars identical is the whole point — the same
/// env var drives the Windows path (where the patch is live, because Granite builds its own
/// device there) and this Linux path (where the patch is inert, because we pass our own
/// create-infos and Granite takes its `inherit_info` branch). One knob that meant two different
/// things per platform is exactly the documentation trap this wiring exists to close.
///
/// Note `off` is the ONLY spelling that disables it; `0` is not, because the C side does not
/// accept `0` either. Do not "improve" that here without changing the patch in the same commit.
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

/// Whether a `create_device` error means "this priority class was refused" — i.e. walk the ladder
/// down rather than failing the encoder open.
///
/// `ERROR_NOT_PERMITTED_KHR` is the specified answer and the only one the C patch handles.
/// `ERROR_INITIALIZATION_FAILED` is here because the in-tree precedent that already ships this
/// ladder on Linux — pf-zerocopy's VkBridge — accepts it too, and because the failure mode this
/// guards against is severe and asymmetric: a PyroWave open is reached only by a NEGOTIATED
/// PyroWave session, so a hard error here is a dead stream, not a fallback to another encoder.
/// Treating one extra driver-specific refusal as a downgrade costs nothing; treating it as fatal
/// costs the session.
fn priority_refused(e: vk::Result) -> bool {
    matches!(
        e,
        vk::Result::ERROR_NOT_PERMITTED_KHR | vk::Result::ERROR_INITIALIZATION_FAILED
    )
}

/// One frame recorded and queue-submitted, whose fence has not been waited and whose bitstream has
/// therefore not been packetized yet. PW5 stage 3: this is what the `submit`/`poll` split created —
/// before it, no such state could exist because `submit` did the whole thing inline.
/// How many independent per-frame resource sets the encoder allocates.
///
/// PW5 stage 4. Two, because Granite caps the overlap at two anyway: the pyrowave device defaults
/// to `init_frame_contexts(2)` and `next_frame_context()` — called at the top of every
/// `encode_gpu_synchronous` — waits the context it rotates into, so "frame N may not begin
/// recording until N-2 completed" is enforced below us. A third slot would buy nothing without a
/// vendored `init_frame_contexts(3)`, which is not exposed.
///
/// **Allocated is not the same as used.** `max_inflight` decides how many are live at once and is
/// still 1 here; this stage is pure capacity so the depth change that follows is a one-line
/// behaviour change rather than a simultaneous re-plumbing.
const SLOTS: usize = 2;

/// Everything ONE in-flight frame needs exclusively.
///
/// PW5 stage 4 exists because the analysis found six single-slot resources beyond the y/uv images
/// the plan named, and every one of them is a correctness problem under overlap — not a
/// performance one:
///
/// * `csc_set` was ONE descriptor set rewritten every frame by `bind_rgb`. Updating a descriptor
///   set still bound by a PENDING command buffer is a spec violation
///   (VUID-vkUpdateDescriptorSets-None-03047), and the kind that produces a wrong picture rather
///   than a validation error on most drivers.
/// * `y_img`/`uv_img` — the CSC of N+1 storage-writes exactly the images pyrowave is still
///   sampling for N. The old barrier comment ("the previous frame's encode already completed under
///   our synchronous fence") was load-bearing and said so.
/// * `cursor_img`/`cursor_stage` — the struct comment said it plainly: *"Single (not ring) because
///   PyroWave encodes one frame synchronously — no in-flight overlap to race."* A new cursor
///   bitmap's host write + copy races N's sampled read.
/// * `cmd` + `fence` — you cannot record into a PENDING command buffer at all.
/// * `cpu_img`/`cpu_stage` (software capture / tests) — the host writes staging while N's copy is
///   still pending.
///
/// `bitstream` and `import_cache` are deliberately NOT here. `bitstream` is only touched during
/// packetize, i.e. only on the poll side, one frame at a time. `import_cache` retains the
/// `VkImage`/`VkDeviceMemory` per dmabuf inode, so dropping a `CapturedFrame` (and its dup'd fd)
/// while the GPU still reads it is not a use-after-free — do not "optimise" that retention away.
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
    /// Per-slot: each slot's cursor image is its own, so a bitmap change is uploaded once per slot
    /// (`SLOTS` small uploads instead of one) rather than once globally, which would leave the
    /// other slot showing the previous pointer.
    cursor_serial: u64,
    cursor_ready: bool,
    /// CPU-input staging (software capture / smoke tests), lazily (re)created on format change.
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
}

impl Slot {
    /// All-null, matching `open_inner`'s "construct then fill" unwind discipline: every
    /// `vkDestroy*`/`vkFree*` of `VK_NULL_HANDLE` is the spec-defined no-op, so `Drop` running on a
    /// partially-built slot is sound.
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

#[derive(Clone, Copy)]
struct InFlight {
    /// Which [`Slot`] this frame's command buffer, fence, descriptor set and images belong to.
    /// Carried per-frame rather than recomputed so `wait_and_packetize` cannot wait the wrong
    /// fence — the failure that would look like corruption, not like an error.
    slot: usize,
    /// The capture timestamp this frame's AU must carry. Held here rather than re-read from the
    /// `CapturedFrame` because the frame is the CALLER's and is gone by the time we packetize.
    pts_ns: u64,
    /// The bitstream buffer size this frame was ENCODED against (`frame_budget + BS_SLACK`), and
    /// the packetize boundary in dense mode.
    ///
    /// Snapshotted at submit rather than re-read at poll because the split opened a window that
    /// did not exist before: `reconfigure_bitrate` can land between the two, and in dense mode the
    /// boundary IS this number — a shrunk budget would make `compute_num_packets` return more than
    /// one packet and the encode would bail with "unexpected packet count" on a frame that was
    /// perfectly fine.
    cap: usize,
    /// The wire sequence value stamped into this frame's block headers, so the AU can be checked
    /// against what we asked for — see the self-check in `wait_and_packetize`.
    seq: u8,
    /// The datagram alignment this frame was encoded for; `set_wire_chunking` can likewise land
    /// mid-flight, and a frame packetized at a boundary it was not rate-controlled for would ship
    /// with the wrong `chunk_aligned` flag.
    wire_chunk: Option<usize>,
    /// `PUNKTFUNK_PERF`: when `submit` started. The summary keeps measuring submit→AU (what
    /// `92326312` measured, so stage 0's baseline stays comparable), not just the wait half.
    t0: std::time::Instant,
}

pub struct PyroWaveEncoder {
    // --- vulkan core (owned; private to this encoder) ---
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    family: u32,
    /// `src` family for the fresh-dmabuf acquire barrier: FOREIGN when the extension is
    /// enabled, else the core EXTERNAL substitute (Phase 8 — see `open_inner`).
    foreign_qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    _hold: DeviceHold,

    // --- pyrowave (borrows our device; destroyed before it) ---
    pw_dev: pw::pyrowave_device,
    /// ONE `pyrowave_encoder` per [`Slot`] (PW5 stage 5), alternated.
    ///
    /// The object CANNOT hold two frames in flight — not "probably not", structurally not.
    /// `Encoder::Impl` owns one each of `wavelet_img_high_res`, `bucket_buffer`, `meta_buffer`,
    /// `block_stat_buffer`, `payload_data` and `quant_buffer`, and `Impl::encode` OPENS by
    /// discarding them: an image barrier with `VK_IMAGE_LAYOUT_UNDEFINED` as the old layout (a
    /// written promise that nothing else is reading it) plus three `fill_buffer` clears. Two
    /// encodes recorded into two command buffers and submitted to the same queue have NO execution
    /// dependency in Vulkan — submission order orders the start, not the completion — so N+1's DWT
    /// would overwrite the bands and zero the RDO buckets while N's block packing still reads them.
    ///
    /// So overlap means two handles on one device, and within a handle the encodes stay strictly
    /// serialized (a slot's next frame is only recorded after that slot's previous one was
    /// retired), which keeps patch 0004's scratch-pool invariant intact without touching it.
    pw_encs: Vec<pw::pyrowave_encoder>,
    /// The wire sequence counter, kept HERE rather than in the encoder objects.
    ///
    /// ⚠ pyrowave's own `sequence_count` is PER-ENCODER, so two alternating handles each count
    /// 1,2,3… independently and the wire sees 1,1,2,2,3,3…. The decoder restarts a frame only when
    /// the value CHANGES (`diff = (hdr.sequence - last_seq) & 0x7; restart = diff != 0`), so a
    /// repeat reads as MORE BLOCKS OF THE SAME FRAME: `clear()` never runs and every second frame
    /// is silently swallowed, on every client. `patches/0007-encoder-sequence-override.patch`
    /// exposes a setter so this single counter is stamped regardless of which handle encodes.
    wire_seq: u32,

    // --- CSC pipeline + sampler: SHARED by every slot (immutable once built, read-only in
    //     recording, so no overlap hazard). The per-frame resources live in `slots`. ---
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,

    // Per-buffer dmabuf-import cache keyed by (st_dev, st_ino) — mirrors `vulkan_video.rs`.
    // NOT per-slot: it retains the VkImage/VkDeviceMemory per inode, which is exactly what makes
    // it safe for two slots to sample the same imported buffer.
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,
    /// Reused 3→4 expansion buffer for 24-bpp CPU payloads (`vk_util::normalize_cpu_rgb`).
    /// Not per-slot: it is consumed synchronously inside `submit_frame` (copied into staging
    /// before the call returns), so no GPU work ever reads it.
    cpu_expand: Vec<u8>,

    cmd_pool: vk::CommandPool,
    /// The `SLOTS` independent per-frame resource sets — see [`Slot`] for why each member is in
    /// there and why `bitstream`/`import_cache` are not.
    slots: Vec<Slot>,
    /// Which slot the NEXT submit records into; advances modulo `SLOTS` per submitted frame.
    next_slot: usize,
    /// Frames recorded and queue-submitted whose fence has not been waited yet — i.e. exactly the
    /// work that may still be executing on the GPU. `reset()` keys its bounded wait on this being
    /// non-empty: a never-submitted fence would otherwise read as "wedged" (fences start
    /// unsignaled). At today's depth of 1 this holds at most one entry.
    inflight: VecDeque<InFlight>,
    /// How many frames may be submitted-but-not-polled at once. **Still 1**, even though stage 4
    /// allocated `SLOTS` resource sets: raising it is stage 6's job and needs the second pyrowave
    /// encoder handle (stage 5) first — pyrowave's own `Encoder` object structurally cannot hold
    /// two frames (single wavelet/scratch buffers, and `Impl::encode` opens by discarding them
    /// with an UNDEFINED old layout). Never exceeds `SLOTS`.
    max_inflight: usize,

    // --- state ---
    width: u32,
    height: u32,
    fps: u32,
    /// Session-fixed negotiated chroma: 4:4:4 = full-res RG8 chroma plane + per-pixel CSC
    /// (`rgb2yuv444.comp`) + `Chroma444` pyrowave objects.
    chroma444: bool,
    /// What the global-priority ladder in `open_inner` actually produced, kept so it can be
    /// REPORTED rather than only logged. `punktfunk-encode-worker` sends it back to the host in
    /// its handshake, which is the process that owns the log pipeline and knows which binary to
    /// name — see [`super::worker::PriorityOutcome`].
    priority: super::worker::PriorityOutcome,
    /// `VkPhysicalDeviceProperties::deviceName` of the device this encoder opened. Sanity for the
    /// same handshake: on a multi-GPU host, "which GPU is the worker on" is otherwise invisible
    /// from the host process.
    device_name: String,
    /// Per-frame bitstream budget (hard CBR): `bitrate / (8 * fps)`.
    frame_budget: usize,
    /// `PUNKTFUNK_PERF`: the synchronous encode's own duration, which is the quantity the
    /// GPU-priority work is about — patch 0005's header records it going ~2 ms → 15-18 ms at
    /// 95 % game load. Every other backend (VAAPI, direct NVENC) already logs a submit split;
    /// this one did not, so the one encoder whose cost the priority lever exists to protect was
    /// the one you could not measure. Reservoir of recent samples, summarised on a slow cadence.
    perf_us: Vec<u32>,
    perf_logged_at: Option<std::time::Instant>,
    /// Datagram-aligned mode (plan §4.4): packetize at this boundary and pad every codec
    /// packet to it, so each wire shard carries whole self-delimiting packets. `None` =
    /// one packet per AU (the dense MVP shape).
    wire_chunk: Option<usize>,
    /// Measured windowing inflation → rate-budget deflation, so the bitrate pin holds on the
    /// WIRE, not just the raw bitstream (see [`crate::pyrowave_wire::WireBudget`]).
    wire_budget: crate::pyrowave_wire::WireBudget,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
    /// The AU currently being handed out in streamed chunks (PW6 — `Some` strictly between a
    /// `first` chunk and its `last`). See [`crate::pyrowave_wire::AuChunker`]: this backend's
    /// encode is synchronous, so the AU is COMPLETE before the first chunk leaves — the split is
    /// for the send side, never an encode/send overlap.
    chunker: Option<crate::pyrowave_wire::AuChunker>,
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
    /// `PUNKTFUNK_PERF`: record one encode duration and summarise on a slow cadence.
    ///
    /// **submit→AU on purpose**, and unchanged by PW5 stage 3's `submit`/`poll` split: the sample
    /// is stamped when `submit` starts and taken when the AU becomes readable, so it still covers
    /// CSC + encode + fence wait + packetize and stays directly comparable to the pre-split
    /// baseline (`92326312`). What the split changed is WHERE the wait sits — the host loop's own
    /// `submit_us` now excludes it, which is the shape every other backend already had.
    ///
    /// ⚠ At a depth greater than 1 this number legitimately grows by roughly one loop period,
    /// because frame N's AU is not retrieved until after N+1 has been submitted. That is real
    /// added latency, not an instrumentation artefact — see PW5's escalation gate.
    fn note_encode_us(&mut self, us: u32) {
        if !pf_host_config::config().perf {
            return;
        }
        self.perf_us.push(us);
        let now = std::time::Instant::now();
        let since = self.perf_logged_at.map(|t| now.duration_since(t));
        // Every 2 s, matching the other backends' submit-split cadence, and never before there
        // are enough samples for a p99 to mean anything.
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

    /// What the global-priority ladder produced for this encoder — the quantity
    /// `punktfunk-encode-worker` reports back so the host can log the grant (or the INERT refusal)
    /// once, naming the right binary.
    pub(crate) fn priority_outcome(&self) -> super::worker::PriorityOutcome {
        self.priority
    }

    /// The Vulkan device this encoder opened on.
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
        // The in-process path reads the intent from ITS OWN environment, exactly as it always
        // has, and owns the INERT warn. (`punktfunk-encode-worker` takes both from its parent —
        // see `open_in_worker`.)
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

    /// [`Self::open`] as `punktfunk-encode-worker` runs it.
    ///
    /// Two things differ, and only two — the encoder itself is opened by the identical code path,
    /// which is what keeps the worker/in-process A/B honest:
    ///
    /// * `intent` arrives **explicitly** from the host's handshake rather than from this process's
    ///   environment (which the worker strips of `PYROWAVE_QUEUE_PRIORITY` at startup), so one
    ///   operator knob cannot come to mean two different things across the process boundary;
    /// * the INERT warn is **left to the host**. It is the process with the log pipeline, and its
    ///   wording has to name the worker binary — the historical text says "CAP_SYS_NICE on the
    ///   host binary", which after 0.26.0-1 would send an operator to do the one thing that
    ///   breaks every KDE session.
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
        // Checked against the chroma actually being opened, NOT hardcoded 4:4:4. The 4:2:0 block
        // count is ~half of 4:4:4's but still unbounded (8192×6144 4:2:0 = 73728 > u16::MAX), and
        // the negotiator's 4:4:4 → 4:2:0 downgrade hands oversized modes to this open AS 4:2:0 —
        // so a `chroma.is_444()`-gated check is skipped exactly when it is needed. Wrapping the
        // index lets the resolve over-credit and `packetize` overshoot our bitstream buffer
        // (its own bounds `assert` is compiled out by the Release vendored build).
        // `validate_dimensions` rejects the impossible-at-any-chroma modes earlier; this is the
        // 4:4:4-specific half plus defence in depth for the lab override.
        if !crate::pyrowave_mode_fits_rdo(width, height, chroma444) {
            bail!(
                "pyrowave {} at {width}x{height} exceeds the rate controller's 16-bit block \
                 index (see pyrowave-sys patches/0002 note) — lower the resolution",
                if chroma444 { "4:4:4" } else { "4:2:0" }
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
                chroma444,
                intent,
                warn_inert,
            )
        }
    }

    /// `intent` is the raw `PYROWAVE_QUEUE_PRIORITY` value (`None` = unset ⇒ the default ladder),
    /// resolved by the CALLER: in-process from this process's environment, in the worker from the
    /// host's handshake. `warn_inert` decides whether THIS process emits the "every class refused"
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

        // Between `create_instance` and `create_device` the only live resource is the instance,
        // so this whole stretch runs as one fallible block with a single manual destroy on its
        // error arm. From the device on, a partially-constructed `Self` (below) makes the
        // existing `Drop` the sole unwind path — these used to be a dozen `?`s that each leaked
        // everything created before them.
        // SAFETY: plain physical-device queries on the live instance just created, and
        // `create_device` calls whose create-infos are pinned in `hold` for each call's duration.
        // The global-priority ladder (WP14 step 4) may call `create_device` several times; every
        // attempt reads the SAME pinned `hold`, and the only thing that varies between attempts is
        // `hold._queue_gp[0].global_priority` (a plain enum field) and, for the final attempt,
        // `hold._queue_ci[0].p_next` being nulled. Both live in `Box`es owned by `hold`, so the
        // pointers `device_ci` holds stay valid across the retries; a failed `create_device` does
        // not consume or invalidate its create-info, so re-passing it is sound.
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

            // VK_EXT_queue_family_foreign (Phase 8): the fresh-import acquire barrier names
            // FOREIGN as src — enable the extension when advertised (`pf-presenter/dmabuf.rs`
            // precedent), else fall back to the core QUEUE_FAMILY_EXTERNAL substitute. Must be
            // pushed BEFORE the count/as_ptr wiring below.
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
            // VK_KHR_global_priority (WP14 step 4): PyroWave encodes on the SAME shader cores a
            // game saturates, and `encode_gpu_synchronous` measurably collapses under that load
            // (patch 0005's header: ~2 ms → 15-18 ms at 95 % game load on an RTX 4090). An
            // elevated global-priority queue is the actual compute-PREEMPTION lever — unlike a
            // process-priority raise, which only orders submission. The vendored patch requests
            // it, but it is gated `if (!inherit_info)` and Linux passes its OWN create-infos, so
            // Granite takes the inherit branch and the patch has never done anything here. This
            // is the Linux half. Must be pushed BEFORE the count/as_ptr wiring below, exactly
            // like queue_family_foreign above.
            let gp_candidates = queue_priority_candidates(intent);
            // Enable whichever alias the driver advertises (KHR = the promoted name), mirroring
            // pf-zerocopy's VkBridge probe so the two can never disagree about the spelling.
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

            // The downgrade ladder, mirroring the C patch: try each class in turn, step down only
            // on a REFUSAL, and if every class is refused create with no global priority at all.
            // A refused class must NEVER fail the open — that graceful property is the entire
            // reason patch 0005 was kept despite its negative RTX/WDDM measurement, and it matters
            // more here: this path is reached only by a negotiated PyroWave session, so a hard
            // error is a dead stream rather than a fallback to another encoder.
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
                    // Either nothing was requested (`off`, or no extension), or every class was
                    // refused. EITHER WAY the retained create-info must now describe a device
                    // created WITHOUT a priority chain — `pyrowave_create_device` keeps
                    // `device_ci` for the device's lifetime and Granite reads it back through
                    // `get_existing_create_info()`. Leaving `p_next` pointing at the
                    // global-priority struct here would hand Granite a chain the device was not
                    // created with. (The extension itself stays enabled and that is correct: it
                    // IS enabled on the device, it just carries no request.)
                    hold._queue_ci[0].p_next = std::ptr::null();
                    if !gp_candidates.is_empty() && gp.is_some() && warn_inert {
                        // MEASURED on .21 (RTX 5070 Ti, NVIDIA 610.43.02, 2026-08-08), and it is
                        // not a vendor quirk: an unprivileged host is refused EVERY class, and the
                        // same binary with `cap_sys_nice+ep` is granted REALTIME on the first
                        // attempt. So this arm is the normal state of a packaged host today, the
                        // lever is inert until the capability ships, and the message has to say
                        // which capability rather than leave an operator guessing.
                        //
                        // `warn_inert` is false in `punktfunk-encode-worker`: it reports the
                        // outcome to its parent, which logs the same sentence naming the WORKER
                        // binary. Sending an operator to `setcap` the host — which this wording
                        // does — is precisely the 0.26.0-1 incident.
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
            // The ladder's outcome, made reportable. `queue_priority_candidates` only ever yields
            // REALTIME or HIGH, so the `Some(_)` arm is exact rather than a fallback (ash models
            // the class as a newtype, not a Rust enum, so this cannot be a `match` on constants).
            let priority = match chosen {
                Some(c) if c == vk::QueueGlobalPriorityKHR::REALTIME => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::Realtime)
                }
                Some(_) => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::High)
                }
                // Exactly the condition the INERT warn above fires on: something was asked for,
                // the extension was there, and every class came back refused.
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

        // Construct `Self` NOW, every not-yet-created resource at its null value, and assign
        // into it as resources come up. Any `?` from here drops `me`, and the existing `Drop`
        // tears down exactly the prefix that exists: it `device_wait_idle()`s first, null-guards
        // `pw_encs` (`pyrowave_encoder_destroy` dereferences before deleting),
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
            // PW5: depth 1 still. Stage 4 allocates the capacity; stage 6 spends it.
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
        for i in 0..SLOTS {
            pw_check(
                pw::pyrowave_encoder_create(&einfo, &mut me.pw_encs[i]),
                "encoder_create",
            )?;
        }

        // ---- CSC planes: full-res R8 luma + RG8 chroma (half-res for 4:2:0, full-res for
        //      4:4:4), storage-written by the CSC and sampled directly by pyrowave (R/G view
        //      swizzles synthesize Cb/Cr) ----
        let device = me.device.clone(); // cheap fn-table clone; lets `me.*` assignments interleave
        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };

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

        // Pool sized for ALL slots: 2 combined-image-samplers (binding 0 RGB + binding 3 cursor)
        // and 2 storage images (Y, UV) per set.
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

        // ---- the per-frame resource sets (PW5 stage 4) ----
        // Each iteration builds ONE complete `Slot` and assigns as it goes, so a failure part-way
        // leaves the earlier slots fully formed and the rest null — which `Drop` handles, since
        // every `vkDestroy*` of VK_NULL_HANDLE is the spec-defined no-op.
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
            // Cursor overlay: fixed CURSOR_MAX² RGBA8 sampled image + host staging (binding 3).
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
            // Bindings 1/2 (Y, UV storage targets) + 3 (cursor sampler) are fixed for the slot's
            // life; only binding 0 (the frame's RGB view) is rewritten per frame, by `bind_rgb`,
            // and THAT is why each slot needs its own set — see `Slot`.
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

        // What the extra slot actually COST, measured from the driver's own requirements rather
        // than estimated from the dimensions — the plan's estimate is not evidence, and on an iGPU
        // at 4K/4:4:4 this is the number that decides whether the capacity is affordable. The
        // per-frame CPU staging is excluded because it is allocated lazily and only on the
        // software-capture path.
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
    ///
    /// ⚠ This is the `vkUpdateDescriptorSets` the analysis flagged: writing a set that is still
    /// bound by a PENDING command buffer violates VUID-vkUpdateDescriptorSets-None-03047. It is
    /// safe because the set belongs to the slot we are about to record into, and that slot's
    /// previous frame was retired before `submit` chose it.
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

    /// Cursor-as-metadata: bring the cursor image up to date for this frame and return the shader
    /// push constant `[origin_x, origin_y, size_w, size_h]` (size 0 ⇒ the CSC skips the blend).
    /// Records the small upload (only when the bitmap `serial` changed) + layout transition into
    /// slot `slot`'s command buffer, ahead of the CSC dispatch that samples binding 3.
    ///
    /// PER SLOT since PW5 stage 4 — image, staging buffer and `cursor_serial` all. The old comment
    /// said it outright: a single shared image was only safe because there was no in-flight
    /// overlap to race. The cost of per-slot is that a changed bitmap uploads once per slot.
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
    ///
    /// PER SLOT since PW5 stage 4: the host writes this staging buffer, so writing it while a
    /// previous frame's buffer-to-image copy is still pending would race that copy.
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

    /// The per-frame budget handed to pyrowave rate control: `frame_budget`, deflated by the
    /// measured windowing inflation when the datagram-aligned wire is on — the bitrate pin is
    /// a promise about the wire, not the raw bitstream (see [`crate::pyrowave_wire::WireBudget`]).
    fn rate_budget(&self) -> usize {
        match self.wire_chunk {
            Some(_) => self.wire_budget.deflate(self.frame_budget).max(64 * 1024),
            None => self.frame_budget,
        }
    }

    /// The SUBMIT half of one frame (PW5 stage 3): ingest → CSC → pyrowave encode, recorded into
    /// our command buffer → queue-submit → **return**. The fence wait and packetize moved to
    /// [`wait_and_packetize`](Self::wait_and_packetize), which is where every other backend in
    /// this crate has always had them.
    ///
    /// On success exactly one [`InFlight`] is pushed. On failure nothing is pushed and the command
    /// buffer has been reset (see the error-arm note inside) — the caller may submit again.
    unsafe fn submit_frame(&mut self, frame: &CapturedFrame, t0: std::time::Instant) -> Result<()> {
        // A failed `reset()` leaves the encoder destroyed and null. Callers today turn that into
        // a session error and never resubmit, but a null here would be a use-after-free inside
        // pyrowave rather than a clean error — so fail loudly instead of relying on that.
        anyhow::ensure!(
            self.pw_encs.iter().all(|e| !e.is_null()),
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
        // Resolved before the closure (which borrows `self` mutably for the recording calls).
        let rate_budget = self.rate_budget();
        // THE slot this frame owns for its whole life — command buffer, fence, descriptor set,
        // y/uv images, cursor image and CPU staging (PW5 stage 4). `submit` guaranteed it is free
        // by draining to `max_inflight - 1` before calling here.
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

            // Cursor-as-metadata: refresh the cursor image (only when the bitmap changed) + get the
            // shader push constant. Recorded into `self.cmd` before the CSC dispatch samples binding 3.
            let cursor_pc = self.prep_cursor(slot, frame.cursor.as_ref())?;

            // ---- ingest RGB (same barrier discipline as vulkan_video.rs) ----
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
                    // 24-bpp Rgb/Bgr expands 3→4 first (see `normalize_cpu_rgb`) — refusing it here
                    // used to kill the session at its first frame with no fallback.
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

            // y/uv -> GENERAL for the CSC's storage writes (discard prior contents — this SLOT's
            // previous frame was retired before `submit` chose it, which is also the "execution
            // barrier before writing to images" pyrowave's contract asks for).
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
            // 4:2:0: one invocation per 2x2 luma block (per chroma sample); 4:4:4: per pixel.
            if self.chroma444 {
                dev.cmd_dispatch(cmd, w.div_ceil(8), h.div_ceil(8), 1);
            } else {
                dev.cmd_dispatch(cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);
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
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_sampled(y_img), to_sampled(uv_img)]),
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
                        y_img,
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
            // ⚠ THE LANDMINE (PW5 stage 5). Stamp OUR monotonic counter before the encode, or
            // the two alternating handles emit 1,1,2,2,3,3… and the decoder reads each repeat as
            // more blocks of the same frame — half the frames silently swallowed, on every client.
            // Needs `patches/0007-encoder-sequence-override.patch`; the round-trip test
            // `wire_sequence_increments_across_alternating_handles` is what keeps it honest.
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
            // SAFETY: on every closure error arm the buffer is RECORDING/INVALID/EXECUTABLE —
            // never PENDING (nothing was enqueued) — and the pool allows the reset.
            let _ = dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
            return Err(e);
        }
        // Submitted: from here the GPU may be executing, and NOTHING may touch `cmd`, the y/uv
        // images or `csc_set` until this entry is retired by `wait_and_packetize`.
        self.next_slot = (slot + 1) % SLOTS;
        // Advance only on SUCCESS: a frame that never reached the wire must not burn a sequence
        // value (a gap reads as a restart, which is right for a DROPPED frame and wrong for one
        // that was never emitted at all).
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

    /// The POLL half of one frame (PW5 stage 3): wait the oldest in-flight frame's fence, then
    /// packetize its bitstream into an `EncodedFrame` on `pending`.
    ///
    /// ⚠ The fence wait's failure path deliberately does NOT reset the command buffer: a timeout
    /// leaves it PENDING, where a reset violates VUID-vkResetCommandBuffer-commandBuffer-00045.
    /// The in-flight entry is likewise NOT popped on that failure — it is what tells `reset()`
    /// there is still live GPU work to re-wait before the encoder object may be destroyed.
    unsafe fn wait_and_packetize(&mut self) -> Result<()> {
        let Some(fr) = self.inflight.front().copied() else {
            return Ok(());
        };
        let dev = self.device.clone();
        dev.wait_for_fences(&[self.slots[fr.slot].fence], true, 5_000_000_000)
            .context("pyrowave encode fence")?;
        // Waited and signaled: the command buffer is INVALID (one-time submit), which the next
        // `begin` may implicitly reset, and the GPU is done with this frame's resources.
        self.inflight.pop_front();

        // ---- packetize ----
        // Dense (default): boundary = whole buffer → the AU is exactly one pyrowave packet.
        // Datagram-aligned (§4.4, `set_wire_chunking`): boundary = the wire shard payload;
        // each codec packet is zero-padded to the boundary so every shard carries whole
        // self-delimiting packets — the client windows its parse and a lost shard costs
        // only those blocks. Padding cost is small: the packetizer fills close to the
        // boundary by design.
        // `fr.cap`/`fr.wire_chunk`, NOT the live fields: `reconfigure_bitrate` and
        // `set_wire_chunking` may have landed since this frame was submitted, and packetizing at a
        // boundary the frame was not rate-controlled for is a spurious failure.
        let cap = fr.cap;
        self.bitstream.resize(cap, 0);
        // Chunked mode reserves the 4-byte window prefix from the packetize boundary (shared helper).
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
        // Correct pyrowave's zeroed sequence-header VUI: it signals ycbcr_range=FULL, but our CSC
        // emits BT.709 LIMITED — patch the bits HONEST so VUI-honoring clients don't wash out
        // blacks. (Linux capture has no HDR path, so this side never stamps BT.2020/PQ.)
        if let Some(p) = packets.first() {
            crate::pyrowave_wire::stamp_color_bits(&mut self.bitstream, p.offset, false);
            // Self-check on the ONE thing a dropped vendored patch would break silently. Without
            // `0007-encoder-sequence-override.patch` the two handles count independently, the wire
            // reads 1,1,2,2,3,3..., and every client's decoder folds each repeated value into the
            // previous frame — half the frames gone, no error anywhere. A re-vendor that loses the
            // patch would not fail to build; it would fail on glass, subtly. Two byte reads per
            // frame to make that loud instead. Once per process: if it is wrong it is wrong for
            // every frame.
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
        // Frame into the wire AU via the shared helper (byte-identical on Linux + Windows): the dense
        // single packet, or the datagram-aligned windowed AU (§4.4).
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
            // Every frame is independently decodable — SOF/keyframe on each AU is the codec's
            // whole recovery story (plan §1.2).
            keyframe: true,
            recovery_anchor: false,
            chunk_aligned: fr.wire_chunk.is_some(),
        });
        // submit→AU, the same quantity `92326312` measured before the split, so stage 0's
        // baseline stays comparable. Stamped here rather than in `submit` because that is where
        // the AU now becomes readable.
        self.note_encode_us(fr.t0.elapsed().as_micros() as u32);
        Ok(())
    }

    /// Retire in-flight frames until at most `keep` remain. Used by `submit` (make room before
    /// recording) and by `flush`/`poll` (drain).
    unsafe fn drain_to(&mut self, keep: usize) -> Result<()> {
        while self.inflight.len() > keep {
            self.wait_and_packetize()?;
        }
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // `PUNKTFUNK_PERF` encode split (kept above the SAFETY comment so that comment stays
        // attached to the block it proves — the crate denies undocumented unsafe blocks).
        let t0 = std::time::Instant::now();
        // SAFETY: single-threaded encoder; both halves work on handles this struct owns.
        // Command-buffer state on failure is `submit_frame`'s own business: its record-and-submit
        // closure resets the buffer on every pre-submit failure, and the fence wait's failure
        // (buffer possibly PENDING) deliberately does NOT reset, because that violates
        // VUID-vkResetCommandBuffer-commandBuffer-00045; the blanket reset that used to live
        // here fired on exactly that path. Recovery (`reset()`/`Drop`) waits the device idle
        // before anything touches `cmd` again.
        unsafe {
            // Make room before recording. This is THE invariant that makes the slot
            // `submit_frame` is about to pick provably free: at most `max_inflight - 1` frames may
            // still be in flight, and `max_inflight <= SLOTS`, so the slot `next_slot` points at
            // was retired. The host loop polls after every submit so this is normally a no-op; it
            // is here for callers that do not (the `spike` subcommand, the hardware smoke tests).
            self.drain_to(self.max_inflight.saturating_sub(1))?;
            self.submit_frame(frame, t0)
        }
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
        // Trait contract (PW6): each AU is drained through ONE method. Erroring beats
        // double-emitting the bytes the chunk cursor already handed out (which would reach the
        // wire twice, under the same frame index, and fail the receiver's retro-validation).
        // Checked BEFORE the fence wait below: if a chunk cursor is open this call is a caller
        // bug, and completing more work first would only widen the damage.
        if self.chunker.is_some() {
            bail!("pyrowave: poll() on an AU already being drained through poll_chunk");
        }
        // PW5 stage 3: THIS is where the fence wait now lives. `submit` returns as soon as the
        // work is queued; the AU only exists once the fence signals and the bitstream is
        // packetized, so poll completes the oldest in-flight frame before answering.
        if self.pending.is_empty() && !self.inflight.is_empty() {
            // SAFETY: single-threaded encoder, waiting its own fence and reading its own
            // bitstream; the failure path leaves the entry in flight for `reset()` to re-wait.
            unsafe { self.wait_and_packetize()? };
        }
        Ok(self.pending.pop_front())
    }

    // --- streamed AU (PW6) — see `pyrowave_wire::AuChunker` for what this does and does NOT buy.
    fn supports_chunked_poll(&self) -> bool {
        crate::pyrowave_wire::stream_chunk_step(self.wire_chunk).is_some()
    }

    fn poll_chunk(&mut self) -> Result<Option<crate::AuChunk>> {
        // Finish the AU already in flight before opening the next one — the host's `handle_chunk`
        // keys begin/finish off `first`/`last` and cannot interleave two AUs.
        if let Some(c) = self.chunker.as_mut() {
            if let Some(chunk) = c.next() {
                return Ok(Some(chunk));
            }
            self.chunker = None;
        }
        let Some(f) = self.pending.pop_front() else {
            return Ok(None);
        };
        // No blocking wait here (the trait allows one): `submit` already ran the whole encode
        // synchronously, so an AU in `pending` is complete by construction.
        match crate::pyrowave_wire::stream_chunk_step(self.wire_chunk) {
            Some(step) => Ok(self
                .chunker
                .insert(crate::pyrowave_wire::AuChunker::new(f, step))
                .next()),
            // Unarmed / dense: the trait's own default shape, so a host that polls chunks anyway
            // still gets whole AUs.
            None => Ok(Some(crate::AuChunk::whole(f))),
        }
    }

    fn reset(&mut self) -> bool {
        // A rebuild forfeits every in-flight frame — including an AU only half-handed-out through
        // `poll_chunk`. Dropping the cursor here (ahead of every `pending.clear()` arm below) is
        // what keeps the next `poll_chunk` from splicing the tail of a dead AU onto a fresh one;
        // the host sees a `first` without the previous `last`, logs "streamed AU abandoned
        // mid-flight" and lets the client age that frame out.
        self.chunker = None;
        // Cheap in-place rebuild: recreate only the pyrowave encoder object — there is no
        // rate-control history or reference state worth preserving (plan §4.3).
        //
        // Bounded wait first: the only work possibly still executing is a submitted frame whose
        // fence wait has not succeeded yet (`inflight` non-empty — either never polled, or polled
        // and timed out). Re-wait it under the same 5 s cap as `wait_and_packetize` — an untimed
        // `device_wait_idle` here would park the recovery thread on the exact device it suspects
        // is wedged, until the kernel's GPU reset, if ever. If the fence still won't signal,
        // destroying the pyrowave encoder under live GPU work would be a use-after-free, so
        // report "no in-place rebuild" and let the session surface a real error (`Drop`'s
        // unbounded idle covers teardown, where blocking on the kernel is acceptable).
        if !self.inflight.is_empty() {
            // Every in-flight frame's fence, not just the oldest — at depth > 1 there may be
            // several, and destroying the pyrowave encoder while ANY of them still executes is a
            // use-after-free.
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
            // The submitted frames are forfeit (their bitstream lives in the encoder object about
            // to be destroyed), but the GPU is provably done with them.
            self.inflight.clear();
        }
        // SAFETY: the device is idle for this encoder's work (the fence wait above, or no submit
        // outstanding) — this sweep-up is instant — and the pyrowave device outlives the encoder
        // object being swapped.
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
                // Publish the null IMMEDIATELY: the create below is fallible, and its failure path
                // must not leave a freed pointer in the field. `pyrowave_encoder_destroy` is a
                // plain `delete` (pyrowave_c.cpp) with no null check, so `Drop` running on a stale
                // handle is a double free — the exact shape this reset hits when the rebuild fails
                // because the device is already lost, which is the state that made the watchdog
                // fire.
                self.pw_encs[i] = std::ptr::null_mut();
                let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
                let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
                if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                    tracing::error!(result = ?r, slot = i, "pyrowave: encoder rebuild failed");
                    // This handle stays null — `Drop` and `submit_frame` both guard on it. The
                    // queued AUs are forfeit either way (the caller turns a false reset into a
                    // session error), so drop them rather than shipping output from a dead
                    // encoder.
                    self.pending.clear();
                    return false;
                }
                self.pw_encs[i] = enc;
            }
            // Fresh handles start their own counters at 0, but the CLIENT's `last_seq` does not
            // reset — so keep counting from where the stream was. A rebuild loses frames, and a
            // gap is exactly what tells the decoder to restart.
            self.next_slot = 0;
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
        // Since PW5 stage 3 there IS something buffered beyond `pending`: a submitted frame whose
        // fence has not been waited. Retire it so the caller's `poll`-until-`None` drain
        // (the trait's contract) actually returns every AU. Bounded by the same 5 s fence cap.
        // SAFETY: single-threaded encoder, waiting its own fence.
        unsafe { self.drain_to(0) }
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
        // `pw_encs` are the null-UNSAFE destroys and carry their own guard below.
        unsafe {
            self.device.device_wait_idle().ok();
            // Null when a failed `reset()` already destroyed it — `pyrowave_encoder_destroy`
            // is not null-safe.
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
            // Every slot, in the same all-null-tolerant way (a failed open leaves a partial
            // prefix built and the rest null; `vkDestroy*(VK_NULL_HANDLE)` is a spec no-op).
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

    /// PW5 stage 4: what the extra per-frame resource set actually COSTS in VRAM, at the modes
    /// that decide whether it is affordable. Reported from the driver's own memory requirements,
    /// not estimated from the dimensions — the plan's ~25-35 MB estimate is a guess, and on an
    /// iGPU at 4K/4:4:4 the real number is the one that matters.
    ///
    /// Prints rather than asserts a threshold: a hard limit here would be a guess about every
    /// future GPU. What it DOES assert is that a slot is not free and not absurd, so a refactor
    /// that accidentally allocated per-slot copies of something large fails visibly.
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

    // ---- WP14 step 4: the global-priority grammar --------------------------------------------
    //
    // The device-create ladder itself has NO unit test and cannot have one — it needs a real
    // Vulkan device. Its coverage is clippy plus the on-glass log line. What IS testable, and what
    // actually drifts, is the grammar: it must stay character-identical to the vendored C patch,
    // because the SAME env var drives the Windows path (where the patch is live) and this one.
    // These are device-free by construction — that is why `queue_priority_candidates` takes the
    // raw string instead of reading the environment itself (env-var tests race).

    /// Unset means `realtime`, which means "try REALTIME, then fall back to HIGH" — the ladder,
    /// not a single class. Windows parity: the C patch defaults the same way.
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

    /// `off` is the ONLY spelling that disables it, and it is case-insensitive because the C patch
    /// lowercases first. Note `0` is deliberately NOT off — the C side does not accept it either,
    /// and two grammars for one variable is the trap this wiring closes.
    #[test]
    fn only_off_disables_and_it_is_case_insensitive() {
        assert!(queue_priority_candidates(Some("off")).is_empty());
        assert!(queue_priority_candidates(Some("OFF")).is_empty());
        assert!(queue_priority_candidates(Some("Off")).is_empty());
        assert!(!queue_priority_candidates(Some("0")).is_empty());
    }

    /// `high` asks for HIGH ONLY — it must not silently try REALTIME first, or the knob would be
    /// unable to express "elevated, but not realtime" (the thing an operator reaches for after a
    /// compositor-jank report).
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

    /// Junk falls back to the default ladder rather than to "off": an unparseable value must never
    /// silently disable a performance lever the operator was trying to tune.
    #[test]
    fn junk_falls_back_to_the_default_ladder() {
        for raw in ["", "realtime", "REALTIME", "yes", "1", "medium", "  high"] {
            assert!(
                !queue_priority_candidates(Some(raw)).is_empty(),
                "{raw:?} must not disable the priority request"
            );
        }
        // ...and specifically the full ladder, not HIGH alone. `"  high"` is in the list above on
        // purpose: the C patch does NOT trim, so neither do we — a space-padded value is junk to
        // both, and the two must agree even in how they are wrong.
        assert_eq!(queue_priority_candidates(Some("  high")).len(), 2);
    }

    /// A refused class must walk the ladder down, never fail the open. `NOT_PERMITTED` is the
    /// specified refusal; `INITIALIZATION_FAILED` is accepted too, matching pf-zerocopy's shipped
    /// VkBridge ladder. Anything else is a real error and must propagate — a driver that is out of
    /// memory should not be silently retried at a lower priority and reported as a success.
    #[test]
    fn only_refusals_walk_the_ladder_down() {
        assert!(priority_refused(vk::Result::ERROR_NOT_PERMITTED_KHR));
        assert!(priority_refused(vk::Result::ERROR_INITIALIZATION_FAILED));
        assert!(!priority_refused(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY));
        assert!(!priority_refused(vk::Result::ERROR_EXTENSION_NOT_PRESENT));
        assert!(!priority_refused(vk::Result::SUCCESS));
    }

    // ---- PW6: the streamed-AU cut, on real GPU output ----------------------------------------
    // Appended at module END per the wave plan's ownership rule.

    /// Walk a windowed AU back into the flat codec-packet stream — the clients' parse
    /// (`video_pyrowave.rs::push_window`, Apple's `MetalWaveletDecoder`), so upstream's decoder
    /// sees exactly what a real client would feed it.
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

    /// Luma PSNR (dB) of a decoded Y plane against the BT.709 limited-range luma of the source
    /// BGRA — the same math `rgb2yuv.comp` runs on the GPU. Luma only: chroma is subsampled on
    /// the 4:2:0 path, and luma is where wavelet quantisation shows.
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

    /// PW6 on-glass: with `PUNKTFUNK_PYROWAVE_STREAMED_AU=1` armed, a real GPU encode of a BUSY
    /// test card must come out of `poll_chunk` in several window-aligned pieces that concatenate
    /// to a decodable AU — and the picture must survive, verified by PSNR against the CSC's own
    /// BT.709 math rather than by "it ran".
    ///
    /// Flat fills are useless here (they false-greened the Windows bring-up): a solid colour
    /// reassembles convincingly even when whole subbands are missing. The busy card puts energy
    /// in every subband, so a cut that lost or reordered a window shows up as a PSNR collapse.
    ///
    /// `#[ignore]`d: needs a real Vulkan 1.3 GPU.
    ///   cargo test -p pf-encode --features pyrowave --no-run
    ///   PUNKTFUNK_PYROWAVE_STREAMED_AU=1 <bin> --ignored --nocapture pyrowave_streamed_chunks
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn pyrowave_streamed_chunks_reassemble_and_keep_the_picture() {
        const WINDOW: usize = 1408;
        // 1280x720 at 60 Mb/s ≈ 125 KB/AU — comfortably several 256 KiB-target chunks' worth of
        // windows at the default step once the step is clamped to the AU, and big enough that the
        // AU spans many windows.
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

            // Drain the AU through the chunked poll, exactly as the native pump does.
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

            // A second `poll_chunk` must report the AU is done, not dribble more bytes.
            assert!(
                enc.poll_chunk().expect("poll_chunk after last").is_none(),
                "no AU is in flight once `last` was handed out"
            );

            // The picture: window-walk (the client's parse) → upstream's own decoder → PSNR.
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

    // ---- PW5 stage 5: the alternating-handle sequence gate ------------------------------------

    /// **THE gate for the second encoder handle.** Two `pyrowave_encoder` objects each keep their
    /// OWN 3-bit `sequence_count`, so alternating them emits `1,1,2,2,3,3…` on the wire. The
    /// decoder restarts a frame only when the value CHANGES
    /// (`diff = (hdr.sequence - last_seq) & 0x7; restart = diff != 0`), so every repeat reads as
    /// "more blocks of the same frame": `clear()` never runs and the second frame of each pair is
    /// silently swallowed. Half frame rate, occasional mixed-frame blocks, no error anywhere — a
    /// failure that passes a smoke test, on every client.
    ///
    /// `patches/0007-encoder-sequence-override.patch` exists solely to make that impossible, and
    /// this test is what proves it, three ways over 20 frames (well past the 3-bit wrap at 8):
    ///
    /// 1. the wire counter advances by exactly +1 mod 8 per AU, read straight out of the block
    ///    header the decoder reads;
    /// 2. ONE persistent decoder — `last_seq` carried across every push, exactly as a client's is —
    ///    reports ready for every single AU, so nothing is swallowed;
    /// 3. consecutive decoded pictures DIFFER. Content moves every frame (`test_card` reseeded per
    ///    frame; flat fills are the documented false-green trap here), so a swallowed frame would
    ///    show up as a repeat, and this catches it even if 1 and 2 somehow both passed.
    #[test]
    #[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
    fn wire_sequence_increments_across_alternating_handles() {
        const FRAMES: u32 = 20;
        let (w, h) = (256u32, 256u32);
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, crate::ChromaFormat::Yuv420).expect("open");
        // This gate is meaningless with a single encoder handle.
        const { assert!(SLOTS >= 2) };

        let mut aus: Vec<Vec<u8>> = Vec::new();
        for i in 0..FRAMES {
            // Content MOVES every frame — a repeated picture is the symptom being hunted, and a
            // static card would hide it. ODD seeds only: `test_card` starts its LCG at `seed | 1`,
            // so 2 and 3 produce a byte-identical card and consecutive even/odd seeds would fake
            // the very repeat this test looks for (it did, on the first run).
            enc.submit(&test_card(w, h, 2 * i + 1)).expect("submit");
            let au = enc.poll().expect("poll").expect("one AU per frame");
            aus.push(au.data);
        }

        // (1) the wire counter, read from the header the decoder parses.
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

        // (2) + (3) ONE decoder for the whole run — a fresh decoder per AU would reset `last_seq`
        // and hide the exact bug this exists to catch.
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

    /// Decode a whole AU stream through ONE decoder (a client's `last_seq` is not reset per frame)
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

    /// **PW5 stage 6's ENCODER-side gate.** Two frames genuinely in flight at once must produce the
    /// same pictures, in the same order, as the synchronous depth-1 path.
    ///
    /// This is the half of the depth-2 risk that lives in THIS crate: the slot resources
    /// (`cmd`/`fence`/`csc_set`/y/uv/cursor) and the alternating encoder handles. Ground truth is
    /// the encoder's OWN depth-1 output over the same frames, which is the honest reference —
    /// pyrowave's raw AU bytes are not reproducible run-to-run (see the stage-3 commit), but its
    /// DECODED planes are.
    ///
    /// Content moves every frame. Flat fills are the documented false-green trap here: a torn frame
    /// assembled from two halves of a static card is invisible, and a gray fill once green-lit a
    /// broken import.
    ///
    /// ⚠ WHAT THIS DOES **NOT** COVER, and no in-tree test can: the CAPTURE side. `.process`
    /// requeues the SPA buffer to the compositor at callback return while the encode thread still
    /// holds only a dup of its fd, so a second frame in flight widens the window in which the
    /// producer may overwrite a buffer we are still reading by a full frame period. That needs a
    /// live compositor, a real client and a long moving-content session — the on-glass tear-hunt
    /// PW5 stage 6 is gated on. This test passing is necessary, not sufficient.
    ///
    /// Drives the backend at depth 2 by setting `max_inflight` directly rather than through a
    /// shipped knob: the shipped value is 1 and this must not change that.
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

        // --- reference: strictly synchronous, one frame at a time ---
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

        // --- overlapped: submit N+1 before polling N ---
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

        // --- decode both streams and compare, frame by frame ---
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
            // 45 dB is far above "looks the same" — a torn frame stitched from two moving cards
            // lands in the teens. Not an equality assert only because the wavelet RDO is not
            // bit-reproducible; the printed worst-case is the number to read.
            assert!(
                p > 45.0,
                "frame {i}: overlapped decode is {p:.1} dB from the synchronous one — the pipelined \
                 path changed the picture"
            );
            // The discriminator that PSNR alone can miss: a frame delivered ONE POSITION OFF still
            // scores well against a similar neighbour. It must match its OWN reference best.
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
