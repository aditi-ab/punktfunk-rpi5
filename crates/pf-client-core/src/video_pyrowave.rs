//! PyroWave client decode (design/pyrowave-codec-plan.md §4.5) — the wired-LAN wavelet
//! codec's decoder, running as plain Vulkan compute on the PRESENTER's own VkDevice (the
//! whole point: decode + CSC + present on one device, zero interop): the AU is one
//! self-delimiting pyrowave packet; `push_packet` → ready →
//! `decode_gpu_buffer` recorded into OUR command buffer, submitted on the shared graphics
//! queue under the device's [`QueueLock`], fence-waited (sub-ms — Phase-0 measured
//! 0.067 ms GPU at 1080p on the RTX 5070 Ti).
//!
//! Output: three separate single-component planes (Y full-res; Cb/Cr half-res, or
//! full-res on a 4:4:4 session) — R8, or R16 UNORM on a 10-bit session — the decode
//! path requires STORAGE usage and IDENTITY/R swizzles, so the encoder's two-component
//! RG trick is not allowed here (pyrowave.h validation). The presenter samples them
//! with its planar CSC variant (colour per the negotiated `ColorInfo` — the wavelet
//! bitstream carries no VUI). A small ring of plane-sets keeps a decode from overwriting the set
//! the presenter is still sampling; the synchronous fence bounds decode-side reuse and
//! the ring depth covers present-side latency (≤ 1–2 frames in this pipeline).
//!
//! pyrowave 0.4.0 requires the instance/device create-infos to stay alive on the shared
//! device — the presenter doesn't pin its originals, so [`Hold`] reconstructs
//! content-equivalent ones from [`VulkanDecodeDevice`]'s exported extension lists,
//! feature facts and queue-family shape (pyrowave reads them for extension/feature
//! detection; pointer identity is not required).
//!
//! **Mid-stream resize:** the pyrowave decoder object is fixed-size, but every frame's
//! bitstream opens with a sequence header carrying its dimensions — [`au_dims`] sniffs
//! it and [`PyroWaveDecoder::reconfigure`] rebuilds the decoder + plane ring in place
//! when the host's `Reconfigure` pipeline rebuild lands (the pyrowave *device*, command
//! pool and pinned create-infos are dimension-independent and survive). Superseded plane
//! rings are retired, not destroyed — the presenter may still hold their views (see
//! [`RETIRE_HANDOVERS`]).

// UNSAFE-LINT EXEMPTION (rationale + exit criteria: `unsafe_op_in_unsafe_fn` in the workspace
// Cargo.toml). This body is `pyrowave-sys` C-API and ash/Vulkan compute calls almost line for line;
// narrowing it would add one `unsafe {}` plus one SAFETY comment per call that could only restate
// the signature. Clearing this file means DELETING the markers that carry no caller contract, not
// wrapping the calls — until then the lint is off HERE and enforced everywhere else.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::video::{ColorDesc, VulkanDecodeDevice};
use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pyrowave_sys as pw;
use std::ffi::{c_char, c_void, CString};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Plane-set ring depth: decode writes slot N while the presenter may still sample
/// N-1/N-2 (its own submission raced ahead under the shared queue's FIFO order, so
/// same-queue execution ordering already serializes writes vs. reads per slot; the ring
/// keeps LOGICAL reuse far enough behind).
const RING: usize = 4;

/// A mid-stream resize retires the old plane ring, but its images can't be destroyed
/// immediately: the pump→presenter frame channel (depth 2, newest-wins) may still hold a
/// frame referencing them, and the presenter binds a frame's views into its descriptor
/// set only inside the `present` call that carries it. Once this many NEW-ring frames
/// have been handed over, every old-ring frame has been displaced from the channel and
/// any present that picked one up has long finished recording; combined with the
/// queue-idle taken before destruction (covers submitted GPU work) the retired images
/// are provably unreachable. The wall-clock floor is a belt for a presenter stalled
/// mid-`present` (swapchain acquire on an occluded window) while frames keep flowing.
const RETIRE_HANDOVERS: u32 = 8;
const RETIRE_MIN_AGE: Duration = Duration::from_millis(250);

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

/// Parse an upstream `BitstreamSequenceHeader` (pyrowave_common.hpp) at the start of
/// `bytes`: 8 bytes, two LE u32s — word 0 = `width_minus_1:14 | height_minus_1:14 |
/// sequence:3 | extended:1`, word 1 = `total_blocks:24 | code:2 | …`. Returns the frame
/// dimensions when this really is a START-OF-FRAME sequence header (the `extended` bit
/// distinguishes it from a regular `BitstreamHeader`, which carries a wavelet block).
fn seq_header_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 8 {
        return None;
    }
    let w0 = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let w1 = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if w0 >> 31 == 0 {
        return None; // regular block header, not a sequence header
    }
    if (w1 >> 24) & 0x3 != 0 {
        return None; // extended, but not BITSTREAM_EXTENDED_CODE_START_OF_FRAME
    }
    Some(((w0 & 0x3FFF) + 1, ((w0 >> 14) & 0x3FFF) + 1))
}

/// The frame dimensions an AU announces, or `None` when they can't be known from this AU
/// (the sequence header rode a lost shard of a partial). The encoder writes exactly one
/// sequence header per frame, at byte 0 of the frame's bitstream — so it sits at the
/// start of an unaligned AU, and at the start of the FIRST window's body in a
/// chunk-aligned AU (§4.4 framing: 4-byte prefix `used:u16 | kind:u16`; kind PACKED or
/// FRAG_FIRST both begin with the frame's first packet, and that packet begins with the
/// sequence header).
fn au_dims(au: &[u8], aligned: bool, wire_window: usize) -> Option<(u32, u32)> {
    if !aligned {
        return seq_header_dims(au);
    }
    let win = &au[..au.len().min(wire_window)];
    if win.len() < 4 {
        return None;
    }
    let used = u16::from_le_bytes([win[0], win[1]]) as usize;
    let kind = u16::from_le_bytes([win[2], win[3]]);
    if used == 0 || 4 + used > win.len() {
        return None; // first window lost/garbage — the sequence header went with it
    }
    // WIN_PACKED (0) and WIN_FRAG_FIRST (1) both start at the frame's first packet;
    // a CONT/LAST fragment here would mean the first window was lost.
    if kind > 1 {
        return None;
    }
    seq_header_dims(&win[4..4 + used])
}

/// Content-equivalent reconstruction of the presenter device's create-infos, pinned for
/// the lifetime of the `pyrowave_device` (heap boxes; moving `Hold` moves only pointers).
struct Hold {
    _inst_ext_names: Vec<CString>,
    _inst_ext_ptrs: Vec<*const c_char>,
    _dev_ext_names: Vec<CString>,
    _dev_ext_ptrs: Vec<*const c_char>,
    _app_info: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _queue_prio: Box<[f32; 1]>,
    _queue_cis: Vec<vk::DeviceQueueCreateInfo<'static>>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v11: Box<vk::PhysicalDeviceVulkan11Features<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

impl Hold {
    fn build(vkd: &VulkanDecodeDevice) -> Hold {
        let inst_ext_names = vkd.instance_extensions.clone();
        let inst_ext_ptrs: Vec<*const c_char> = inst_ext_names.iter().map(|c| c.as_ptr()).collect();
        let dev_ext_names = vkd.device_extensions.clone();
        let dev_ext_ptrs: Vec<*const c_char> = dev_ext_names.iter().map(|c| c.as_ptr()).collect();

        let mut app_info =
            Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3));
        let mut instance_ci = Box::new(vk::InstanceCreateInfo::default());
        instance_ci.p_application_info = &mut *app_info;
        instance_ci.enabled_extension_count = inst_ext_ptrs.len() as u32;
        instance_ci.pp_enabled_extension_names = if inst_ext_ptrs.is_empty() {
            std::ptr::null()
        } else {
            inst_ext_ptrs.as_ptr()
        };

        let queue_prio = Box::new([1.0f32]);
        let mut queue_cis: Vec<vk::DeviceQueueCreateInfo<'static>> = vkd
            .queue_families
            .iter()
            .map(|&fam| {
                let mut ci = vk::DeviceQueueCreateInfo::default().queue_family_index(fam);
                ci.queue_count = 1;
                ci
            })
            .collect();
        for ci in &mut queue_cis {
            ci.p_queue_priorities = queue_prio.as_ptr();
        }

        // The feature facts the presenter enabled (VulkanDecodeDevice reports exactly
        // what device creation turned on — pyrowave keys its paths off these).
        let mut feat2 = Box::new(vk::PhysicalDeviceFeatures2::default());
        feat2.features.shader_int16 = vkd.f_shader_int16 as u32;
        let mut v11 = Box::new(
            vk::PhysicalDeviceVulkan11Features::default()
                .sampler_ycbcr_conversion(vkd.f_sampler_ycbcr),
        );
        let mut v12 = Box::new(
            vk::PhysicalDeviceVulkan12Features::default()
                .timeline_semaphore(vkd.f_timeline_semaphore)
                .storage_buffer8_bit_access(vkd.f_storage_buffer8)
                .shader_float16(vkd.f_shader_float16),
        );
        let mut v13 = Box::new(
            vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(vkd.f_synchronization2)
                .subgroup_size_control(vkd.f_subgroup_size_control)
                .compute_full_subgroups(vkd.f_compute_full_subgroups),
        );
        feat2.p_next = &mut *v11 as *mut _ as *mut c_void;
        v11.p_next = &mut *v12 as *mut _ as *mut c_void;
        v12.p_next = &mut *v13 as *mut _ as *mut c_void;

        let mut device_ci = Box::new(vk::DeviceCreateInfo::default());
        device_ci.p_next = &*feat2 as *const _ as *const c_void;
        device_ci.queue_create_info_count = queue_cis.len() as u32;
        device_ci.p_queue_create_infos = queue_cis.as_ptr();
        device_ci.enabled_extension_count = dev_ext_ptrs.len() as u32;
        device_ci.pp_enabled_extension_names = dev_ext_ptrs.as_ptr();

        Hold {
            _inst_ext_names: inst_ext_names,
            _inst_ext_ptrs: inst_ext_ptrs,
            _dev_ext_names: dev_ext_names,
            _dev_ext_ptrs: dev_ext_ptrs,
            _app_info: app_info,
            instance_ci,
            _queue_prio: queue_prio,
            _queue_cis: queue_cis,
            _feat2: feat2,
            _v11: v11,
            _v12: v12,
            _v13: v13,
            device_ci,
        }
    }
}

/// The queue-lock trampolines pyrowave calls around any internal queue use. `userdata`
/// is a raw pointer to the [`crate::video::QueueLock`] kept alive by the decoder's Arc.
unsafe extern "C" fn queue_lock_cb(ud: *mut c_void) {
    // SAFETY: `ud` is the QueueLock the decoder's Arc pins; pyrowave only calls this
    // while the decoder (and thus the Arc) lives.
    unsafe { (*(ud as *const crate::video::QueueLock)).lock() }
}
unsafe extern "C" fn queue_unlock_cb(ud: *mut c_void) {
    // SAFETY: as above.
    unsafe { (*(ud as *const crate::video::QueueLock)).unlock() }
}

/// One decoded PyroWave frame: three single-component plane images (R8, or R16 on a
/// 10-bit session) on the presenter's device, GENERAL layout, decode-complete (the
/// decoder fence-waits before handing it over). `slot` identifies the ring entry; the
/// images/views live as long as the decoder.
pub struct PyroWavePlanarFrame {
    /// Raw `VkImageView`s (Y, Cb, Cr) for the presenter's planar CSC sampling.
    pub views: [u64; 3],
    pub width: u32,
    pub height: u32,
    pub color: ColorDesc,
    /// Every PyroWave frame is independently decodable — always a clean re-anchor.
    pub keyframe: bool,
}

struct PlaneSet {
    imgs: [vk::Image; 3],
    mems: [vk::DeviceMemory; 3],
    views: [vk::ImageView; 3],
    /// First use transitions from UNDEFINED; afterwards GENERAL→GENERAL.
    initialized: bool,
}

/// A plane ring superseded by a mid-stream resize, awaiting safe destruction (see
/// [`RETIRE_HANDOVERS`] for the lifetime argument).
struct RetiredRing {
    sets: Vec<PlaneSet>,
    /// Frames handed to the presenter since this ring was retired.
    handed_over: u32,
    retired_at: Instant,
}

/// One decode-output plane (`fmt` = R8, or R16 on a 10-bit session): storage (decode
/// writes) + sampled (presenter CSC).
unsafe fn make_plane(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fmt: vk::Format,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let img = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(fmt)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
    )?;
    let req = device.get_image_memory_requirements(img);
    let ti = (0..mem_props.memory_type_count)
        .find(|&i| {
            (req.memory_type_bits & (1 << i)) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .unwrap_or(0);
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ti),
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
    let view = match device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            }),
        None,
    ) {
        Ok(v) => v,
        Err(e) => {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
            return Err(e.into());
        }
    };
    Ok((img, mem, view))
}

unsafe fn destroy_sets(device: &ash::Device, sets: &[PlaneSet]) {
    for set in sets {
        for v in set.views {
            device.destroy_image_view(v, None);
        }
        for i in set.imgs {
            device.destroy_image(i, None);
        }
        for m in set.mems {
            device.free_memory(m, None);
        }
    }
}

/// Build a fresh [`RING`]-deep plane ring at the given dimensions; cleans up the partial
/// ring on failure (the caller keeps whatever it was using before).
unsafe fn build_ring(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
    chroma444: bool,
    fmt: vk::Format,
) -> Result<Vec<PlaneSet>> {
    // 4:2:0 = half-res chroma; 4:4:4 = full-res. The presenter's planar CSC samples with
    // normalized UVs, so the chroma plane resolution is transparent to it.
    let (cw, ch) = if chroma444 {
        (width, height)
    } else {
        (width / 2, height / 2)
    };
    let mut ring: Vec<PlaneSet> = Vec::with_capacity(RING);
    for _ in 0..RING {
        let built = (|| -> Result<PlaneSet> {
            let (y, ym, yv) = make_plane(device, mem_props, width, height, fmt)?;
            let (cb, cbm, cbv) = match make_plane(device, mem_props, cw, ch, fmt) {
                Ok(p) => p,
                Err(e) => {
                    device.destroy_image_view(yv, None);
                    device.destroy_image(y, None);
                    device.free_memory(ym, None);
                    return Err(e);
                }
            };
            let (cr, crm, crv) = match make_plane(device, mem_props, cw, ch, fmt) {
                Ok(p) => p,
                Err(e) => {
                    for (v, i, m) in [(yv, y, ym), (cbv, cb, cbm)] {
                        device.destroy_image_view(v, None);
                        device.destroy_image(i, None);
                        device.free_memory(m, None);
                    }
                    return Err(e);
                }
            };
            Ok(PlaneSet {
                imgs: [y, cb, cr],
                mems: [ym, cbm, crm],
                views: [yv, cbv, crv],
                initialized: false,
            })
        })();
        match built {
            Ok(set) => ring.push(set),
            Err(e) => {
                destroy_sets(device, &ring);
                return Err(e);
            }
        }
    }
    Ok(ring)
}

pub struct PyroWaveDecoder {
    // ash wrappers reconstructed over the presenter's raw handles (not owned — the
    // presenter outlives the decoder; Drop destroys only what this struct created).
    device: ash::Device,
    queue: vk::Queue,
    _hold: Box<Hold>,
    queue_lock: Arc<crate::video::QueueLock>,
    pw_dev: pw::pyrowave_device,
    pw_dec: pw::pyrowave_decoder,
    ring: Vec<PlaneSet>,
    /// Plane rings superseded by mid-stream resizes, pending safe destruction.
    retired: Vec<RetiredRing>,
    next: usize,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
    /// Session-fixed negotiated chroma ([`Welcome::chroma_format`]): 4:4:4 = full-res
    /// chroma planes + `Chroma444` pyrowave decoders (the seq-header bit is
    /// decoder-enforced upstream, so a mismatch fails loudly, never silently).
    chroma444: bool,
    /// Session colour signalling ([`Welcome::color`]): the wavelet bitstream has no VUI,
    /// so the negotiated `ColorInfo` is the contract the presenter CSC configures from.
    color: ColorDesc,
    /// Session-fixed negotiated depth ≥10: the planes are `R16_UNORM` carrying the host's
    /// P010-style studio codes (the presenter samples them with depth-10 MSB-packed rows).
    hdr16: bool,
    /// The wire shard payload — the parse-window size for chunk-aligned AUs (§4.4): each
    /// window holds whole self-delimiting codec packets, zero-padded to the window.
    wire_window: usize,
}

// SAFETY: used only from the single decode thread; the shared-queue accesses go through
// QueueLock, matching every other Vulkan backend's threading contract.
unsafe impl Send for PyroWaveDecoder {}

impl PyroWaveDecoder {
    pub fn new(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<PyroWaveDecoder> {
        if !vkd.pyrowave_decode {
            bail!("presenter device lacks the PyroWave compute feature set");
        }
        if !chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // SAFETY: the handles in `vkd` are the presenter's live instance/device (it
        // outlives the decoder — same contract the FFmpeg Vulkan backend relies on);
        // `Hold` pins the reconstructed create-infos for the pyrowave device's lifetime.
        unsafe { Self::new_inner(vkd, width, height, shard_payload, chroma444, color, hdr16) }
    }

    unsafe fn new_inner(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<PyroWaveDecoder> {
        let static_fn = ash::StaticFn {
            get_instance_proc_addr: std::mem::transmute::<usize, vk::PFN_vkGetInstanceProcAddr>(
                vkd.get_instance_proc_addr,
            ),
        };
        let instance_h = vk::Instance::from_raw(vkd.instance as u64);
        let device_h = vk::Device::from_raw(vkd.device as u64);
        let entry = ash::Entry::from_static_fn(static_fn.clone());
        let instance = ash::Instance::load(&static_fn, instance_h);
        let device = ash::Device::load(instance.fp_v1_0(), device_h);
        let queue = device.get_device_queue(vkd.graphics_qf, 0);
        let _ = &entry;

        let hold = Box::new(Hold::build(vkd));
        let queue_lock = vkd.queue_lock.clone();
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: queue.as_raw() as usize as pw::VkQueue,
            familyIndex: vkd.graphics_qf,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            // SAFETY(cast): re-labels the loader entry point between ash's and bindgen's
            // identical C function-pointer types.
            GetInstanceProcAddr: Some(std::mem::transmute::<
                vk::PFN_vkGetInstanceProcAddr,
                unsafe extern "C" fn(pw::VkInstance, *const c_char) -> pw::PFN_vkVoidFunction,
            >(static_fn.get_instance_proc_addr)),
            instance: vkd.instance as pw::VkInstance,
            physical_device: vkd.physical_device as pw::VkPhysicalDevice,
            device: vkd.device as pw::VkDevice,
            instance_create_info: &*hold.instance_ci as *const vk::InstanceCreateInfo
                as *const pw::VkInstanceCreateInfo,
            device_create_info: &*hold.device_ci as *const vk::DeviceCreateInfo
                as *const pw::VkDeviceCreateInfo,
            queue_info: &mut queue_info,
            queue_info_count: 1,
            // The presenter, Skia and every decode lane serialize on this same lock.
            queue_lock_callback: Some(queue_lock_cb),
            queue_unlock_callback: Some(queue_unlock_cb),
            userdata: Arc::as_ptr(&queue_lock) as *mut c_void,
        };
        let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
        pw_check(
            pw::pyrowave_create_device(&create, &mut pw_dev),
            "create_device (shared presenter device)",
        )?;
        let _ =
            pw::pyrowave_device_set_queue_type(pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT);

        let dinfo = pw::pyrowave_decoder_create_info {
            device: pw_dev,
            width: width as i32,
            height: height as i32,
            chroma: if chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
            // The fragment-iDWT path is for Mali/Adreno-class mobile GPUs only.
            fragment_path: false,
        };
        let mut pw_dec: pw::pyrowave_decoder = std::ptr::null_mut();
        if let Err(e) = pw_check(
            pw::pyrowave_decoder_create(&dinfo, &mut pw_dec),
            "decoder_create",
        ) {
            pw::pyrowave_device_destroy(pw_dev);
            return Err(e);
        }

        // Plane-set ring: 3 single-component planes (R8; R16 on a 10-bit session),
        // storage (decode writes) + sampled (presenter CSC).
        let mem_props = instance.get_physical_device_memory_properties(
            vk::PhysicalDevice::from_raw(vkd.physical_device as u64),
        );
        // 16-bit sessions decode into R16_UNORM storage planes; STORAGE_IMAGE support for
        // R16_UNORM is optional in Vulkan (universal on desktop) — probe it so an exotic
        // device fails with a clear message instead of a validation error.
        let plane_fmt = if hdr16 {
            let props = instance.get_physical_device_format_properties(
                vk::PhysicalDevice::from_raw(vkd.physical_device as u64),
                vk::Format::R16_UNORM,
            );
            if !props
                .optimal_tiling_features
                .contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
            {
                pw::pyrowave_decoder_destroy(pw_dec);
                pw::pyrowave_device_destroy(pw_dev);
                bail!("this GPU lacks R16_UNORM STORAGE_IMAGE — cannot decode a 10-bit PyroWave session");
            }
            vk::Format::R16_UNORM
        } else {
            vk::Format::R8_UNORM
        };
        let ring = match build_ring(&device, &mem_props, width, height, chroma444, plane_fmt) {
            Ok(r) => r,
            Err(e) => {
                pw::pyrowave_decoder_destroy(pw_dec);
                pw::pyrowave_device_destroy(pw_dev);
                return Err(e);
            }
        };

        let cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(vkd.graphics_qf)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let cmd = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0];
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;

        tracing::info!(
            width,
            height,
            "PyroWave decoder open on the presenter's device (compute iDWT, BT.709 limited)"
        );
        Ok(PyroWaveDecoder {
            device,
            queue,
            _hold: hold,
            queue_lock,
            pw_dev,
            pw_dec,
            ring,
            retired: Vec::new(),
            next: 0,
            cmd_pool,
            cmd,
            fence,
            mem_props,
            width,
            height,
            chroma444,
            color,
            hdr16,
            wire_window: shard_payload.max(64),
        })
    }

    /// Mid-stream resize: rebuild the pyrowave decoder + plane ring at the new
    /// dimensions in place, keeping the (dimension-independent) pyrowave device, command
    /// pool, fence and pinned create-infos. Build-new-before-drop-old: a failure leaves
    /// the current decoder untouched (and propagates — with the stream now at a size we
    /// can't decode, the session ends with a real error instead of a frozen picture).
    /// The old ring is RETIRED, not destroyed: the presenter / frame channel may still
    /// reference its views (see [`RETIRE_HANDOVERS`]).
    unsafe fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        if !self.chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (resize to {width}x{height})");
        }
        let dinfo = pw::pyrowave_decoder_create_info {
            device: self.pw_dev,
            width: width as i32,
            height: height as i32,
            // Chroma is session-fixed (negotiated); a resize never changes it.
            chroma: if self.chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
            fragment_path: false,
        };
        let mut new_dec: pw::pyrowave_decoder = std::ptr::null_mut();
        pw_check(
            pw::pyrowave_decoder_create(&dinfo, &mut new_dec),
            "decoder_create (mid-stream resize)",
        )?;
        let new_ring = match build_ring(
            &self.device,
            &self.mem_props,
            width,
            height,
            self.chroma444,
            if self.hdr16 {
                vk::Format::R16_UNORM
            } else {
                vk::Format::R8_UNORM
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                pw::pyrowave_decoder_destroy(new_dec);
                return Err(e).context("plane ring (mid-stream resize)");
            }
        };
        // Our own decode work is fence-synchronous (never in flight here), so the old
        // pyrowave decoder can go immediately; only the plane images wait (retired).
        pw::pyrowave_decoder_destroy(self.pw_dec);
        self.pw_dec = new_dec;
        let old = std::mem::replace(&mut self.ring, new_ring);
        self.retired.push(RetiredRing {
            sets: old,
            handed_over: 0,
            retired_at: Instant::now(),
        });
        self.next = 0;
        tracing::info!(
            from = %format_args!("{}x{}", self.width, self.height),
            to = %format_args!("{width}x{height}"),
            "PyroWave decoder rebuilt for mid-stream resize"
        );
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Destroy retired rings that are provably unreachable (enough new-ring frames handed
    /// over + a wall-clock floor — see [`RETIRE_HANDOVERS`]); the queue idle bounds any
    /// still-submitted presenter sampling of the retiring views.
    unsafe fn reap_retired(&mut self) {
        let ripe = |r: &RetiredRing| {
            r.handed_over >= RETIRE_HANDOVERS && r.retired_at.elapsed() >= RETIRE_MIN_AGE
        };
        if !self.retired.iter().any(ripe) {
            return;
        }
        {
            let _guard = self.queue_lock.guard();
            let _ = self.device.queue_wait_idle(self.queue);
        }
        let mut kept = Vec::new();
        for r in self.retired.drain(..) {
            if ripe(&r) {
                destroy_sets(&self.device, &r.sets);
            } else {
                kept.push(r);
            }
        }
        self.retired = kept;
    }

    /// One AU in → one frame out. `aligned` = the AU is shard-window chunked (each
    /// `wire_window` holds whole self-delimiting packets, zero-padded — walk and strip);
    /// `complete` = every shard arrived (a partial decodes anyway: missing blocks are
    /// localized blur for exactly this frame, §4.4).
    pub fn decode_frame(
        &mut self,
        au: &[u8],
        aligned: bool,
        complete: bool,
    ) -> Result<Option<PyroWavePlanarFrame>> {
        // SAFETY: single decode thread; all handles owned/pinned by `self`; queue access
        // serialized under the device-wide QueueLock; the fence bounds GPU completion
        // before the frame is handed to the presenter.
        unsafe { self.decode_inner(au, aligned, complete) }
    }

    /// Consume one framed shard window (§4.4): a 4-byte prefix (u16 used-length + u16
    /// kind) then either WHOLE self-delimiting codec packets (PACKED) or one fragment of
    /// an oversized packet (FRAG chain). A lost shard arrives as a zeroed window
    /// (used = 0) — skipped, and it breaks any fragment chain it interrupts (that
    /// packet's blocks are unusable without their end; dropping them is the §4.4 blur).
    unsafe fn push_window(&mut self, win: &[u8], frag: &mut Vec<u8>) -> Result<()> {
        if win.len() < 4 {
            return Ok(());
        }
        let used = u16::from_le_bytes([win[0], win[1]]) as usize;
        let kind = u16::from_le_bytes([win[2], win[3]]);
        if used == 0 || 4 + used > win.len() {
            frag.clear(); // missing / garbage window — drop any chain in progress
            return Ok(());
        }
        let body = &win[4..4 + used];
        match kind {
            0 => {
                frag.clear();
                pw_check(
                    pw::pyrowave_decoder_push_packet(
                        self.pw_dec,
                        body.as_ptr() as *const c_void,
                        body.len(),
                    ),
                    "push_packet",
                )
            }
            1 => {
                frag.clear();
                frag.extend_from_slice(body);
                Ok(())
            }
            2 => {
                if !frag.is_empty() {
                    frag.extend_from_slice(body);
                }
                Ok(())
            }
            3 => {
                if !frag.is_empty() {
                    frag.extend_from_slice(body);
                    let r = pw_check(
                        pw::pyrowave_decoder_push_packet(
                            self.pw_dec,
                            frag.as_ptr() as *const c_void,
                            frag.len(),
                        ),
                        "push_packet (fragmented)",
                    );
                    frag.clear();
                    return r;
                }
                Ok(())
            }
            _ => {
                frag.clear();
                Ok(())
            }
        }
    }

    unsafe fn decode_inner(
        &mut self,
        au: &[u8],
        aligned: bool,
        complete: bool,
    ) -> Result<Option<PyroWavePlanarFrame>> {
        // Mid-stream resize: every frame's bitstream opens with a sequence header
        // carrying its dimensions, so the AU itself announces the host's mode switch —
        // no control-plane ordering to race (the Reconfigured ack travels on another
        // stream). Upstream hard-errors on a dimension mismatch, so rebuild FIRST. A
        // partial that lost its first shard sniffs `None` and decodes at the current
        // size (correct when the size didn't change; harmlessly dropped below when it
        // did — the next complete frame carries the header again).
        if let Some(dims) = au_dims(au, aligned, self.wire_window) {
            if dims != (self.width, self.height) {
                self.reconfigure(dims.0, dims.1)?;
            }
        }
        let mut push_err: Option<anyhow::Error> = None;
        if aligned {
            let mut frag: Vec<u8> = Vec::new();
            for win in au.chunks(self.wire_window) {
                if let Err(e) = self.push_window(win, &mut frag) {
                    push_err = Some(e);
                    break;
                }
            }
        } else if let Err(e) = pw_check(
            pw::pyrowave_decoder_push_packet(self.pw_dec, au.as_ptr() as *const c_void, au.len()),
            "push_packet",
        ) {
            push_err = Some(e);
        }
        if let Some(e) = push_err {
            // A partial straddling a resize can carry blocks the (possibly wrong-size)
            // decoder rejects — that's one lost frame, not a broken session; the next
            // complete frame re-anchors (all-intra). A COMPLETE frame that fails to
            // parse is real corruption: propagate.
            if complete {
                return Err(e);
            }
            tracing::debug!(error = %format!("{e:#}"), "partial AU rejected — frame dropped");
            return Ok(None);
        }
        // A complete AU that isn't ready is a stale/duplicate (sequence rewind) — skip.
        // A PARTIAL is decoded regardless: missing wavelet blocks reconstruct as zeros,
        // i.e. localized blur for exactly this one frame (the next is complete again).
        if complete && !pw::pyrowave_decoder_decode_is_ready(self.pw_dec, false) {
            return Ok(None);
        }

        let slot = self.next;
        self.next = (self.next + 1) % RING;
        let dev = self.device.clone();
        dev.begin_command_buffer(
            self.cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        let old_layout = if self.ring[slot].initialized {
            vk::ImageLayout::GENERAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_write = |img| {
            vk::ImageMemoryBarrier2::default()
                // Order against the presenter's prior sampling of this slot (same queue).
                .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(img)
                .subresource_range(range)
        };
        let pre: Vec<_> = self.ring[slot].imgs.iter().map(|&i| to_write(i)).collect();
        dev.cmd_pipeline_barrier2(
            self.cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre),
        );

        // The declared format/extent MUST equal the ring image's real ones: pyrowave wraps
        // our VkImage under `image_format` and vkCreateImageView's its storage view with
        // `view_format` (pyrowave_c.cpp `WrappedViewBuffers::wrap` — its own
        // `pyrowave_image_get_image_view` helper fills both from the image itself).
        // Declaring R8 over a 10-bit session's R16_UNORM planes is an invalid view the
        // driver executes anyway: its addressing covers half the surface, so decoded 8-bit
        // codes fuse pairwise into 16-bit texels (structured garbage) and the never-written
        // remainder samples as all-plane zeros (saturated green) — the 2026-07 AMD-client
        // field report. Same discipline for chroma extents: a 4:4:4 ring is full-res.
        let fmt = if self.hdr16 {
            pw::VkFormat_VK_FORMAT_R16_UNORM
        } else {
            pw::VkFormat_VK_FORMAT_R8_UNORM
        };
        let plane = |img: vk::Image, w: u32, h: u32| pw::pyrowave_image_view {
            image: img.as_raw() as usize as pw::VkImage,
            width: w,
            height: h,
            image_format: fmt,
            view_format: fmt,
            mip_level: 0,
            layer: 0,
            aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
            swizzle: pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
            layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
        };
        let (w, h) = (self.width, self.height);
        let (cw, ch) = if self.chroma444 {
            (w, h)
        } else {
            (w / 2, h / 2)
        };
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [
                plane(self.ring[slot].imgs[0], w, h),
                plane(self.ring[slot].imgs[1], cw, ch),
                plane(self.ring[slot].imgs[2], cw, ch),
            ],
        };
        pw::pyrowave_device_set_command_buffer(
            self.pw_dev,
            self.cmd.as_raw() as usize as pw::VkCommandBuffer,
        );
        let dec_res = pw::pyrowave_decoder_decode_gpu_buffer(
            self.pw_dec,
            std::ptr::null(),
            std::ptr::null(),
            &buffers,
        );
        pw::pyrowave_device_set_command_buffer(self.pw_dev, std::ptr::null_mut());
        pw_check(dec_res, "decode_gpu_buffer")?;

        // Decode's storage writes → the presenter's fragment sampling (layout stays
        // GENERAL: that is what the planar CSC descriptors use for this path).
        let to_read = |img| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(img)
                .subresource_range(range)
        };
        let post: Vec<_> = self.ring[slot].imgs.iter().map(|&i| to_read(i)).collect();
        dev.cmd_pipeline_barrier2(
            self.cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&post),
        );
        dev.end_command_buffer(self.cmd)?;

        dev.reset_fences(&[self.fence])?;
        {
            let _guard = self.queue_lock.guard();
            let cmds = [self.cmd];
            dev.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                self.fence,
            )?;
        }
        dev.wait_for_fences(&[self.fence], true, 5_000_000_000)
            .context("pyrowave decode fence")?;
        self.ring[slot].initialized = true;

        // This frame is about to reach the presenter — it advances every retired ring's
        // displacement count, and ripe rings can now be destroyed.
        for r in &mut self.retired {
            r.handed_over += 1;
        }
        self.reap_retired();

        Ok(Some(PyroWavePlanarFrame {
            views: [
                self.ring[slot].views[0].as_raw(),
                self.ring[slot].views[1].as_raw(),
                self.ring[slot].views[2].as_raw(),
            ],
            width: w,
            height: h,
            // No VUI in the bitstream: the negotiated Welcome `ColorInfo` is the contract
            // with the host's CSC (BT.709 limited for SDR sessions; BT.2020 PQ once the
            // HDR leg lands — design/pyrowave-444-hdr.md).
            color: self.color,
            keyframe: true,
        }))
    }
}

impl Drop for PyroWaveDecoder {
    fn drop(&mut self) {
        // SAFETY: owned handles created by this struct on the presenter's device; the
        // fence-synchronous decode means no work of OURS is in flight, and the presenter
        // may still be sampling the last handed-over slot — idle the device's queue
        // under the shared lock before destroying the plane images.
        unsafe {
            {
                let _guard = self.queue_lock.guard();
                let _ = self.device.queue_wait_idle(self.queue);
            }
            pw::pyrowave_decoder_destroy(self.pw_dec);
            pw::pyrowave_device_destroy(self.pw_dev);
            destroy_sets(&self.device, &self.ring);
            for r in &self.retired {
                destroy_sets(&self.device, &r.sets);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            // `self.device`/instance are the PRESENTER's — never destroyed here.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{au_dims, seq_header_dims};

    /// Little-endian encoding of upstream's `BitstreamSequenceHeader` bitfields (see
    /// pyrowave_common.hpp): word 0 = width_minus_1:14 | height_minus_1:14 | sequence:3
    /// | extended:1; word 1 = total_blocks:24 | code:2 | chroma:1 | …
    fn seq_header(w: u32, h: u32, code: u32) -> [u8; 8] {
        let w0 = (w - 1) & 0x3FFF | ((h - 1) & 0x3FFF) << 14 | 1 << 31;
        let w1 = 0x1234 | code << 24; // arbitrary total_blocks
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&w0.to_le_bytes());
        out[4..8].copy_from_slice(&w1.to_le_bytes());
        out
    }

    /// A regular `BitstreamHeader` (block packet): extended bit clear.
    fn block_header() -> [u8; 8] {
        let w0 = 0xBEEFu32 | 8 << 16; // ballot | payload_words=8, extended=0
        let w1 = 42u32 << 8; // block_index
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&w0.to_le_bytes());
        out[4..8].copy_from_slice(&w1.to_le_bytes());
        out
    }

    /// Wrap `body` in one §4.4 framed window of `win` bytes (4-byte prefix + zero pad).
    fn window(body: &[u8], kind: u16, win: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(win);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(body);
        out.resize(win, 0);
        out
    }

    #[test]
    fn sniffs_dims_from_a_sequence_header() {
        assert_eq!(
            seq_header_dims(&seq_header(1920, 1080, 0)),
            Some((1920, 1080))
        );
        assert_eq!(
            seq_header_dims(&seq_header(1280, 720, 0)),
            Some((1280, 720))
        );
        // 14-bit fields carry up to 16384.
        assert_eq!(
            seq_header_dims(&seq_header(16384, 16384, 0)),
            Some((16384, 16384))
        );
    }

    #[test]
    fn rejects_non_sequence_headers() {
        assert_eq!(seq_header_dims(&block_header()), None); // extended bit clear
        assert_eq!(seq_header_dims(&seq_header(1920, 1080, 1)), None); // not START_OF_FRAME
        assert_eq!(seq_header_dims(&seq_header(1920, 1080, 0)[..7]), None); // short
        assert_eq!(seq_header_dims(&[]), None);
    }

    #[test]
    fn unaligned_au_sniffs_at_byte_zero() {
        let mut au = seq_header(2560, 1440, 0).to_vec();
        au.extend_from_slice(&block_header());
        assert_eq!(au_dims(&au, false, 1404), Some((2560, 1440)));
    }

    #[test]
    fn aligned_au_sniffs_the_first_window_body() {
        const WIN: usize = 64;
        let mut body = seq_header(1280, 800, 0).to_vec();
        body.extend_from_slice(&block_header());
        // WIN_PACKED first window, then another window of blocks.
        let mut au = window(&body, 0, WIN);
        au.extend_from_slice(&window(&block_header(), 0, WIN));
        assert_eq!(au_dims(&au, true, WIN), Some((1280, 800)));
        // An oversized first packet rides a FRAG chain — FRAG_FIRST also starts at the
        // frame's first byte, so the header is still there.
        let frag = window(&body, 1, WIN);
        assert_eq!(au_dims(&frag, true, WIN), Some((1280, 800)));
    }

    #[test]
    fn lost_first_window_means_unknown_dims() {
        const WIN: usize = 64;
        // A lost shard arrives as a zeroed window (used = 0) — the sequence header is gone.
        let mut au = vec![0u8; WIN];
        au.extend_from_slice(&window(&seq_header(1280, 800, 0), 0, WIN));
        assert_eq!(au_dims(&au, true, WIN), None);
        // A FRAG_CONT/LAST first window means the same (its FIRST was in a lost prior AU).
        let cont = window(&block_header(), 2, WIN);
        assert_eq!(au_dims(&cont, true, WIN), None);
        // Garbage used-length never reads out of bounds.
        let mut garbage = vec![0u8; WIN];
        garbage[0] = 0xFF;
        garbage[1] = 0xFF;
        assert_eq!(au_dims(&garbage, true, WIN), None);
    }
}
