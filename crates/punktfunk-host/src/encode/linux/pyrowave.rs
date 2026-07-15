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

use super::vk_util::{color_range, find_mem, import_rgb_dmabuf, make_plain_image, pixel_to_vk};
use crate::capture::{CapturedFrame, FramePayload};
use crate::encode::{EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use ash::vk::Handle as _;
use pyrowave_sys as pw;
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::raw::c_char;

/// Same prebuilt RGB→(Y, interleaved-UV) BT.709-limited compute CSC the Vulkan Video backend
/// uses. PyroWave carries no VUI, so the colour contract is fixed by this shader: the Phase-2
/// client CSC must assume BT.709 limited range.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// Max resident dmabuf imports (mirrors `vulkan_video.rs` — PipeWire cycles a small fixed pool).
const IMPORT_CACHE_CAP: usize = 16;
/// Headroom over the per-frame rate budget for the packetized bitstream (block headers + meta;
/// the rate controller itself never exceeds the budget).
const BS_SLACK: usize = 256 * 1024;

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

    // Per-buffer dmabuf-import cache keyed by (st_dev, st_ino) — mirrors `vulkan_video.rs`.
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,
    // CPU-input staging (software capture / smoke tests), lazily (re)created on format change.
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,

    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,

    // --- state ---
    width: u32,
    height: u32,
    fps: u32,
    /// Per-frame bitstream budget (hard CBR): `bitrate / (8 * fps)`.
    frame_budget: usize,
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
    pub fn open(width: u32, height: u32, fps: u32, bitrate_bps: u64) -> Result<Self> {
        if width % 2 != 0 || height % 2 != 0 {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // SAFETY: `open_inner` only issues Vulkan/pyrowave calls whose preconditions it
        // establishes itself (valid instance/device, correctly-chained create-infos that
        // `DeviceHold` keeps alive); all handles are freshly created and owned by the result.
        unsafe { Self::open_inner(width, height, fps.max(1), bitrate_bps.max(1_000_000)) }
    }

    unsafe fn open_inner(w: u32, h: u32, fps: u32, bitrate: u64) -> Result<Self> {
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

        // Pick the first real GPU with a graphics+compute family (pyrowave requires a
        // graphics-capable queue in the device create info; the CSC + codec run on it).
        let (pd, family) = {
            let mut found = None;
            for pd in instance.enumerate_physical_devices()? {
                let props = instance.get_physical_device_properties(pd);
                if props.device_type == vk::PhysicalDeviceType::CPU {
                    continue; // skip llvmpipe
                }
                let fam = instance
                    .get_physical_device_queue_family_properties(pd)
                    .iter()
                    .position(|q| {
                        q.queue_flags
                            .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                    });
                if let Some(f) = fam {
                    found = Some((pd, f as u32));
                    break;
                }
            }
            found.context("no Vulkan GPU with a graphics+compute queue")?
        };
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // Feature gate — pyrowave's documented encoder requirements (pyrowave.h): shaderInt16,
        // storageBuffer8BitAccess, subgroup size control (1.3 core); shaderFloat16 is optional.
        let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut have13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut have2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut have12)
            .push_next(&mut have13);
        instance.get_physical_device_features2(pd, &mut have2);
        let missing: Vec<&str> = [
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
        .collect();
        if !missing.is_empty() {
            bail!("GPU lacks pyrowave-required Vulkan features: {missing:?}");
        }

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
        let queue = device.get_device_queue(family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);

        // ---- hand the device to pyrowave (create-infos stay pinned in `hold`) ----
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: queue.as_raw() as pw::VkQueue,
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
            >(entry.static_fn().get_instance_proc_addr)),
            instance: instance.handle().as_raw() as usize as pw::VkInstance,
            physical_device: pd.as_raw() as usize as pw::VkPhysicalDevice,
            device: device.handle().as_raw() as usize as pw::VkDevice,
            instance_create_info: &*hold.instance_ci as *const vk::InstanceCreateInfo
                as *const pw::VkInstanceCreateInfo,
            device_create_info: &*hold.device_ci as *const vk::DeviceCreateInfo
                as *const pw::VkDeviceCreateInfo,
            queue_info: &mut queue_info,
            queue_info_count: 1,
            // Single-threaded over this private device (encode thread only) and pyrowave only
            // submits inside our API calls — no locking needed.
            queue_lock_callback: None,
            queue_unlock_callback: None,
            userdata: std::ptr::null_mut(),
        };
        let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
        pw_check(
            pw::pyrowave_create_device(&create, &mut pw_dev),
            "create_device",
        )?;
        // Our explicit command buffers live on a compute-capable family.
        let _ =
            pw::pyrowave_device_set_queue_type(pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT);

        let einfo = pw::pyrowave_encoder_create_info {
            device: pw_dev,
            width: w as i32,
            height: h as i32,
            chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
        };
        let mut pw_enc: pw::pyrowave_encoder = std::ptr::null_mut();
        if let Err(e) = pw_check(
            pw::pyrowave_encoder_create(&einfo, &mut pw_enc),
            "encoder_create",
        ) {
            pw::pyrowave_device_destroy(pw_dev);
            return Err(e);
        }

        // ---- CSC planes: full-res R8 luma + half-res RG8 chroma, storage-written by the CSC
        //      and sampled directly by pyrowave (R/G view swizzles synthesize Cb/Cr) ----
        let (y_img, y_mem, y_view) = make_plain_image(
            &device,
            &mem_props,
            vk::Format::R8_UNORM,
            w,
            h,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        let (uv_img, uv_mem, uv_view) = make_plain_image(
            &device,
            &mem_props,
            vk::Format::R8G8_UNORM,
            w / 2,
            h / 2,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;

        // ---- CSC compute pipeline (same shader + layout as vulkan_video.rs) ----
        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(CSC_SPV))?;
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
        ];
        let csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [csc_dsl];
        let csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().set_layouts(&dsls),
            None,
        )?;
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
        device.destroy_shader_module(shader, None);

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2),
        ];
        let csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        let csc_set = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(csc_pool)
                .set_layouts(&dsls),
        )?[0];
        // Bindings 1/2 (Y, UV storage targets) are fixed for the encoder's lifetime.
        let yi = [vk::DescriptorImageInfo::default()
            .image_view(y_view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let uvi = [vk::DescriptorImageInfo::default()
            .image_view(uv_view)
            .image_layout(vk::ImageLayout::GENERAL)];
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
            ],
            &[],
        );

        let cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
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

        let frame_budget = budget_for(bitrate, fps);
        let props = instance.get_physical_device_properties(pd);
        tracing::info!(
            gpu = %props.device_name_as_c_str().unwrap_or(c"?").to_string_lossy(),
            mode = %format!("{w}x{h}@{fps}"),
            budget_kib = frame_budget / 1024,
            "PyroWave encoder open (intra-only wavelet, BT.709 limited 4:2:0)"
        );

        Ok(Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            queue,
            family,
            mem_props,
            _hold: hold,
            pw_dev,
            pw_enc,
            csc_pipe,
            csc_layout,
            csc_dsl,
            csc_pool,
            csc_set,
            sampler,
            y_img,
            y_mem,
            y_view,
            uv_img,
            uv_mem,
            uv_view,
            import_cache: Vec::new(),
            cpu_img: None,
            cpu_stage: None,
            cmd_pool,
            cmd,
            fence,
            width: w,
            height: h,
            fps,
            frame_budget,
            bitstream: Vec::new(),
            pending: VecDeque::new(),
            frame_count: 0,
        })
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

    /// Import a dmabuf with per-buffer caching — same policy as `vulkan_video.rs::import_cached`.
    unsafe fn import_cached(
        &mut self,
        d: &crate::capture::DmabufFrame,
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
        let (img, mem, view) =
            import_rgb_dmabuf(&self.device, &self.ext_fd, &self.mem_props, d, cw, ch)?;
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
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        dev.begin_command_buffer(
            self.cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

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
                let fmt = pixel_to_vk(frame.format).context("unsupported CPU pixel format")?;
                let view = self.ensure_cpu_rgb(fmt, bytes)?;
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
        dev.cmd_dispatch(self.cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);

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
                plane(
                    self.uv_img,
                    w / 2,
                    h / 2,
                    rg8,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                ),
                plane(
                    self.uv_img,
                    w / 2,
                    h / 2,
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
        dev.wait_for_fences(&[self.fence], true, 5_000_000_000)
            .context("pyrowave encode fence")?;

        // ---- packetize: boundary = whole buffer, so the AU is exactly one pyrowave packet ----
        let cap = self.frame_budget + BS_SLACK;
        self.bitstream.resize(cap, 0);
        let mut n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_compute_num_packets(self.pw_enc, cap, &mut n),
            "compute_num_packets",
        )?;
        if n != 1 {
            bail!("pyrowave: expected a single packet at boundary {cap}, got {n}");
        }
        let mut packet = pw::pyrowave_packet { offset: 0, size: 0 };
        let mut out_n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_packetize(
                self.pw_enc,
                &mut packet,
                cap,
                &mut out_n,
                self.bitstream.as_mut_ptr() as *mut std::ffi::c_void,
                cap,
            ),
            "packetize",
        )?;
        let au = self.bitstream[packet.offset..packet.offset + packet.size].to_vec();
        self.frame_count += 1;
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: frame.pts_ns,
            // Every frame is independently decodable — SOF/keyframe on each AU is the codec's
            // whole recovery story (plan §1.2).
            keyframe: true,
            recovery_anchor: false,
        });
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // SAFETY: single-threaded encoder; `encode_frame` records/submits on handles this
        // struct owns and waits its own fence before touching results.
        unsafe { self.encode_frame(frame) }
    }

    fn caps(&self) -> EncoderCaps {
        // All defaults: no RFI (meaningless — every frame is intra), no HDR (8-bit SDR codec),
        // no intra-refresh wave (ditto). 4:2:0 only until the 4:4:4 ride-along (plan §6).
        EncoderCaps::default()
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
            let einfo = pw::pyrowave_encoder_create_info {
                device: self.pw_dev,
                width: self.width as i32,
                height: self.height as i32,
                chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            };
            let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
            if pw::pyrowave_encoder_create(&einfo, &mut enc) != pw::pyrowave_result_PYROWAVE_SUCCESS
            {
                tracing::error!("pyrowave: encoder rebuild failed");
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
        tracing::info!(
            mbps = bps / 1_000_000,
            budget_kib = self.frame_budget / 1024,
            "pyrowave: per-frame rate budget retargeted in place"
        );
        true
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
        unsafe {
            self.device.device_wait_idle().ok();
            pw::pyrowave_encoder_destroy(self.pw_enc);
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
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixelFormat;

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

    /// Decode an AU with a standalone pyrowave CPU decoder and return plane means (Y, Cb, Cr).
    /// This is the Phase-1 "golden frames" oracle: the host-encoded bitstream must round-trip
    /// through upstream's own decoder to the CSC's expected values.
    unsafe fn decode_plane_means(w: u32, h: u32, au: &[u8]) -> (f64, f64, f64) {
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
        assert_eq!(
            pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        assert!(pw::pyrowave_decoder_decode_is_ready(dec, false));

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
            pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &mut buf),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        pw::pyrowave_decoder_destroy(dec);
        pw::pyrowave_device_destroy(dev);

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
        let mut enc = PyroWaveEncoder::open(w, h, 60, 40_000_000).expect("open");
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
            let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data) };
            let (ye, cbe, cre) = bt709(*c);
            assert!(
                (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
                "frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
                 expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
            );
        }

        // In-place rate retarget + encoder rebuild both keep encoding.
        assert!(enc.reconfigure_bitrate(100_000_000));
        assert!(enc.reset());
        enc.submit(&cpu_frame(w, h, 999, [10, 20, 30, 255]))
            .expect("submit after reset");
        assert!(enc.poll().expect("poll").is_some());
    }
}
