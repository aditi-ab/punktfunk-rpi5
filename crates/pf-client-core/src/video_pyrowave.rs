//! PyroWave client decode (design/pyrowave-codec-plan.md §4.5) — the wired-LAN wavelet
//! codec's decoder, running as plain Vulkan compute on the PRESENTER's own VkDevice (the
//! whole point: decode + CSC + present on one device, zero interop). Bypasses FFmpeg
//! entirely: the AU is one self-delimiting pyrowave packet; `push_packet` → ready →
//! `decode_gpu_buffer` recorded into OUR command buffer, submitted on the shared graphics
//! queue under the device's [`QueueLock`], fence-waited (sub-ms — Phase-0 measured
//! 0.067 ms GPU at 1080p on the RTX 5070 Ti).
//!
//! Output: three separate R8 planes (Y full-res, Cb/Cr half-res) — the decode path
//! requires STORAGE usage and IDENTITY/R swizzles, so the encoder's two-component
//! RG8 trick is not allowed here (pyrowave.h validation). The presenter samples them
//! with its planar CSC variant (BT.709 limited — the codec's fixed colour contract,
//! there is no VUI). A small ring of plane-sets keeps a decode from overwriting the set
//! the presenter is still sampling; the synchronous fence bounds decode-side reuse and
//! the ring depth covers present-side latency (≤ 1–2 frames in this pipeline).
//!
//! pyrowave 0.4.0 requires the instance/device create-infos to stay alive on the shared
//! device — the presenter doesn't pin its originals, so [`Hold`] reconstructs
//! content-equivalent ones from [`VulkanDecodeDevice`]'s exported extension lists,
//! feature facts and queue-family shape (pyrowave reads them for extension/feature
//! detection; pointer identity is not required).

use crate::video::{ColorDesc, VulkanDecodeDevice};
use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pyrowave_sys as pw;
use std::ffi::{c_char, c_void, CString};
use std::sync::Arc;

/// Plane-set ring depth: decode writes slot N while the presenter may still sample
/// N-1/N-2 (its own submission raced ahead under the shared queue's FIFO order, so
/// same-queue execution ordering already serializes writes vs. reads per slot; the ring
/// keeps LOGICAL reuse far enough behind).
const RING: usize = 4;

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
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

/// One decoded PyroWave frame: three R8 plane images on the presenter's device, GENERAL
/// layout, decode-complete (the decoder fence-waits before handing it over). `slot`
/// identifies the ring entry; the images/views live as long as the decoder.
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
    next: usize,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    width: u32,
    height: u32,
    /// The wire shard payload — the parse-window size for chunk-aligned AUs (§4.4): each
    /// window holds whole self-delimiting codec packets, zero-padded to the window.
    wire_window: usize,
}

// SAFETY: used only from the single decode thread; the shared-queue accesses go through
// QueueLock, matching the FFmpeg-Vulkan backend's threading contract.
unsafe impl Send for PyroWaveDecoder {}

impl PyroWaveDecoder {
    pub fn new(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
    ) -> Result<PyroWaveDecoder> {
        if !vkd.pyrowave_decode {
            bail!("presenter device lacks the PyroWave compute feature set");
        }
        if width % 2 != 0 || height % 2 != 0 {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // SAFETY: the handles in `vkd` are the presenter's live instance/device (it
        // outlives the decoder — same contract the FFmpeg Vulkan backend relies on);
        // `Hold` pins the reconstructed create-infos for the pyrowave device's lifetime.
        unsafe { Self::new_inner(vkd, width, height, shard_payload) }
    }

    unsafe fn new_inner(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
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
            // The presenter/Skia/FFmpeg all serialize on this same lock.
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
            chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
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

        // Plane-set ring: 3 × R8, storage (decode writes) + sampled (presenter CSC).
        let mem_props = instance.get_physical_device_memory_properties(
            vk::PhysicalDevice::from_raw(vkd.physical_device as u64),
        );
        let make_plane = |w: u32, h: u32| -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
            let img = device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
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
            let mem = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(ti),
                None,
            )?;
            device.bind_image_memory(img, mem, 0)?;
            let view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(img)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )?;
            Ok((img, mem, view))
        };
        let mut ring = Vec::with_capacity(RING);
        for _ in 0..RING {
            let (y, ym, yv) = make_plane(width, height)?;
            let (cb, cbm, cbv) = make_plane(width / 2, height / 2)?;
            let (cr, crm, crv) = make_plane(width / 2, height / 2)?;
            ring.push(PlaneSet {
                imgs: [y, cb, cr],
                mems: [ym, cbm, crm],
                views: [yv, cbv, crv],
                initialized: false,
            });
        }

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
            mode = %format!("{width}x{height}"),
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
            next: 0,
            cmd_pool,
            cmd,
            fence,
            width,
            height,
            wire_window: shard_payload.max(64),
        })
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
        if aligned {
            let mut frag: Vec<u8> = Vec::new();
            for win in au.chunks(self.wire_window) {
                self.push_window(win, &mut frag)?;
            }
        } else {
            pw_check(
                pw::pyrowave_decoder_push_packet(
                    self.pw_dec,
                    au.as_ptr() as *const c_void,
                    au.len(),
                ),
                "push_packet",
            )?;
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

        let plane = |img: vk::Image, w: u32, h: u32| pw::pyrowave_image_view {
            image: img.as_raw() as usize as pw::VkImage,
            width: w,
            height: h,
            image_format: pw::VkFormat_VK_FORMAT_R8_UNORM,
            view_format: pw::VkFormat_VK_FORMAT_R8_UNORM,
            mip_level: 0,
            layer: 0,
            aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
            swizzle: pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
            layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
        };
        let (w, h) = (self.width, self.height);
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [
                plane(self.ring[slot].imgs[0], w, h),
                plane(self.ring[slot].imgs[1], w / 2, h / 2),
                plane(self.ring[slot].imgs[2], w / 2, h / 2),
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

        Ok(Some(PyroWavePlanarFrame {
            views: [
                self.ring[slot].views[0].as_raw(),
                self.ring[slot].views[1].as_raw(),
                self.ring[slot].views[2].as_raw(),
            ],
            width: w,
            height: h,
            // No VUI in the bitstream: BT.709 limited is the fixed contract with the
            // host's CSC (plan §4.7 CscRows note; sequence-header signaling is a
            // follow-up once the C API exposes it).
            color: ColorDesc {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false,
            },
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
            for set in &self.ring {
                for v in set.views {
                    self.device.destroy_image_view(v, None);
                }
                for i in set.imgs {
                    self.device.destroy_image(i, None);
                }
                for m in set.mems {
                    self.device.free_memory(m, None);
                }
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            // `self.device`/instance are the PRESENTER's — never destroyed here.
        }
    }
}
