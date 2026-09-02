//! Vulkan LINEAR-dmabuf bridge. NVIDIA EGL cannot sample LINEAR; CUDA will not import a raw
//! dmabuf fd. Vulkan imports via `VK_EXT_external_memory_dma_buf` and exports `OPAQUE_FD`
//! memory CUDA can import:
//!
//! ```text
//!   dmabuf fd ──VkImportMemoryFdInfoKHR(DMA_BUF)──▶ VkBuffer (cached per fd)
//!        │ vkCmdCopyBuffer (GPU, device-local)
//!        ▼
//!   exportable VkBuffer ──vkGetMemoryFdKHR(OPAQUE_FD)──▶ cuImportExternalMemory ──▶ CUdeviceptr
//! ```
//!
//! One exportable buffer + CUDA mapping per resolution. Per frame: GPU copy, fence wait,
//! pitched CUDA copy into the encoder pool. Cache imports by fd (PipeWire's pool is stable
//! for a stream). Init/import failure disables the importer; CPU mmap takes over.

use super::cuda::{self, DeviceBuffer};
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use std::collections::HashMap;

struct SrcBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
}

/// Exportable destination, CUDA-mapped; rebuilt when the resolution grows.
struct DstBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    /// CUDA mapping; owns the exported OPAQUE_FD.
    cuda: cuda::ExternalDmabuf,
}

#[derive(Debug, PartialEq, Eq)]
struct Nv12Layout {
    uv_offset: u64,
    size: u64,
    pitch_words: u32,
    uv_offset_words: u32,
    pitch_usize: usize,
}

fn nv12_layout(width: u32, height: u32) -> Option<Nv12Layout> {
    if width == 0 || height == 0 {
        return None;
    }
    let pitch = u64::from(width).checked_add(3)? & !3;
    let uv_offset = pitch.checked_mul(u64::from(height))?;
    let uv_size = pitch.checked_mul(u64::from(height.div_ceil(2)))?;
    Some(Nv12Layout {
        uv_offset,
        size: uv_offset.checked_add(uv_size)?,
        pitch_words: u32::try_from(pitch / 4).ok()?,
        uv_offset_words: u32::try_from(uv_offset / 4).ok()?,
        pitch_usize: usize::try_from(pitch).ok()?,
    })
}

/// RGB→NV12 compute pipeline (`rgb2nv12_buf.comp`).
struct Csc {
    module: vk::ShaderModule,
    dset_layout: vk::DescriptorSetLayout,
    playout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    dpool: vk::DescriptorPool,
    dset: vk::DescriptorSet,
}

/// RGB→NV12 SPIR-V. Rebuild: `glslangValidator -V rgb2nv12_buf.comp -o rgb2nv12_buf.spv`. CI gates drift.
const CSC_SPV: &[u8] = include_bytes!("rgb2nv12_buf.spv");

pub struct VkBridge {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    src_cache: HashMap<i32, SrcBuf>,
    dst: Option<DstBuf>,
    /// Built on first [`import_linear_nv12`](Self::import_linear_nv12).
    csc: Option<Csc>,
}

// SAFETY: owns unsynchronized Vulkan handles, a CUDA mapping, and an fd→buffer cache. A single
// queue + command buffer need external sync. Created and used only on the capture thread; never
// shared via `&`. `Send` is ownership transfer into `Send` `EglImporter`; not `Sync`.
unsafe impl Send for VkBridge {}

impl VkBridge {
    pub fn new() -> Result<VkBridge> {
        // SAFETY: ash cannot check Vulkan CreateInfo/handle validity. Every `*CreateInfo` is a
        // local that outlives the synchronous `create_*`/`enumerate_*` that reads it; the priority
        // ladder rebuilds `prio`/`gp_info`/`qci`/`exts` per attempt. Handles are created and
        // `?`-checked in this function. Constructor shares nothing across threads.
        unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .context("vkCreateInstance")?;

            // 0x10DE = NVIDIA, matching CUDA device 0. Destroy the instance on failure: `Drop`
            // is not wired yet, and a refused bridge retries every frame.
            let phys = match instance
                .enumerate_physical_devices()
                .context("enumerate GPUs")
                .map(|devs| {
                    devs.into_iter()
                        .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
                }) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    instance.destroy_instance(None);
                    return Err(anyhow!("no NVIDIA Vulkan device"));
                }
                Err(e) => {
                    instance.destroy_instance(None);
                    return Err(e);
                }
            };
            let mem_props = instance.get_physical_device_memory_properties(phys);

            // Compute implies transfer. Copy only needs transfer; the NV12 CSC dispatch needs
            // compute.
            let qf = match instance
                .get_physical_device_queue_family_properties(phys)
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            {
                Some(i) => i as u32,
                None => {
                    instance.destroy_instance(None);
                    return Err(anyhow!("no compute-capable queue family"));
                }
            };

            // CSC shares SM with the game: request elevated global priority so it schedules
            // first. `PUNKTFUNK_VK_QUEUE_PRIORITY` = off | high | realtime (default realtime).
            // The create loop walks REALTIME→HIGH→none on NOT_PERMITTED / INITIALIZATION_FAILED
            // so a refused class never fails the bridge.
            let gp_ext = std::env::var("PUNKTFUNK_VK_QUEUE_PRIORITY")
                .ok()
                .as_deref()
                .map_or(Some(vk::QueueGlobalPriorityKHR::REALTIME), |v| match v {
                    "off" | "0" => None,
                    "high" => Some(vk::QueueGlobalPriorityKHR::HIGH),
                    _ => Some(vk::QueueGlobalPriorityKHR::REALTIME),
                })
                .and_then(|want| {
                    // KHR is the promoted name; fall back to EXT.
                    let props = instance.enumerate_device_extension_properties(phys).ok()?;
                    let has = |name: &std::ffi::CStr| {
                        props
                            .iter()
                            .any(|p| p.extension_name_as_c_str() == Ok(name))
                    };
                    if has(vk::KHR_GLOBAL_PRIORITY_NAME) {
                        Some((vk::KHR_GLOBAL_PRIORITY_NAME, want))
                    } else if has(vk::EXT_GLOBAL_PRIORITY_NAME) {
                        Some((vk::EXT_GLOBAL_PRIORITY_NAME, want))
                    } else {
                        None
                    }
                });
            let base_exts = [
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ];
            let mut try_priority = gp_ext.map(|(_, want)| want);
            let device = loop {
                let prio = [1.0f32];
                let mut gp_info = vk::DeviceQueueGlobalPriorityCreateInfoKHR::default()
                    .global_priority(try_priority.unwrap_or(vk::QueueGlobalPriorityKHR::MEDIUM));
                let mut qci0 = vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(qf)
                    .queue_priorities(&prio);
                let mut exts: Vec<*const std::ffi::c_char> = base_exts.to_vec();
                if try_priority.is_some() {
                    qci0 = qci0.push_next(&mut gp_info);
                    exts.push(gp_ext.expect("try_priority implies gp_ext").0.as_ptr());
                }
                let qci = [qci0];
                match instance.create_device(
                    phys,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&qci)
                        .enabled_extension_names(&exts),
                    None,
                ) {
                    Ok(d) => {
                        if let Some(p) = try_priority {
                            tracing::info!(
                                priority = ?p,
                                "VkBridge queue at elevated global priority (CSC schedules \
                                 ahead of a GPU-bound game where the driver honors it)"
                            );
                        }
                        break d;
                    }
                    Err(
                        vk::Result::ERROR_NOT_PERMITTED_KHR
                        | vk::Result::ERROR_INITIALIZATION_FAILED,
                    ) if try_priority == Some(vk::QueueGlobalPriorityKHR::REALTIME) => {
                        try_priority = Some(vk::QueueGlobalPriorityKHR::HIGH);
                    }
                    Err(
                        vk::Result::ERROR_NOT_PERMITTED_KHR
                        | vk::Result::ERROR_INITIALIZATION_FAILED,
                    ) if try_priority.is_some() => {
                        tracing::debug!(
                            "global-priority queue not permitted — VkBridge at default priority"
                        );
                        try_priority = None;
                    }
                    Err(e) => {
                        instance.destroy_instance(None);
                        return Err(e)
                            .context("vkCreateDevice (external-memory extensions supported?)");
                    }
                }
            };
            // `Drop` is now live and no-ops on VK_NULL_HANDLE. Fill remaining objects in
            // place so a later `?` unwinds instead of leaking instance + device.
            let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
            let queue = device.get_device_queue(qf, 0);
            let mut me = VkBridge {
                _entry: entry,
                instance,
                device,
                ext_fd,
                queue,
                cmd_pool: vk::CommandPool::null(),
                cmd: vk::CommandBuffer::null(),
                fence: vk::Fence::null(),
                mem_props,
                src_cache: HashMap::new(),
                dst: None,
                csc: None,
            };
            me.cmd_pool = me
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(qf)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .context("create command pool")?;
            me.cmd = me
                .device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(me.cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .context("allocate command buffer")?[0];
            me.fence = me
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .context("create fence")?;

            tracing::info!("Vulkan bridge ready (dmabuf import → OPAQUE_FD export → CUDA)");
            Ok(me)
        }
    }

    fn memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| anyhow!("no compatible Vulkan memory type"))
    }

    /// Import `fd` (dup'd internally; Vulkan owns the dup) as a transfer-src buffer of `size`.
    unsafe fn import_src(&mut self, fd: i32, size: u64) -> Result<()> {
        // SAFETY: caller contract: this thread owns the handles. Every builder info is a local
        // that outlives the call that reads it. Each fallible step destroys what it created.
        unsafe {
            use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
            let dup = libc::dup(fd);
            if dup < 0 {
                bail!("dup(dmabuf fd)");
            }
            // Own the dup until `allocate_memory` succeeds (Vulkan then consumes it). `SrcBuf`
            // has no Drop and is filled only on success, so each fallible step must destroy the
            // buffer it created: a failed import retries every frame.
            let dup = OwnedFd::from_raw_fd(dup);
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        // STORAGE: NV12 CSC reads it as an SSBO. Harmless on the copy path.
                        .usage(
                            vk::BufferUsageFlags::TRANSFER_SRC
                                | vk::BufferUsageFlags::STORAGE_BUFFER,
                        )
                        .push_next(&mut ext_info),
                    None,
                )
                .context("create import buffer")?;
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            if let Err(e) = self.ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                dup.as_raw_fd(),
                &mut fd_props,
            ) {
                self.device.destroy_buffer(buffer, None);
                return Err(e).context("vkGetMemoryFdPropertiesKHR");
            }
            let reqs = self.device.get_buffer_memory_requirements(buffer);
            let mem_type = match self.memory_type(
                reqs.memory_type_bits & fd_props.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            ) {
                Ok(t) => t,
                Err(e) => {
                    self.device.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            // Successful import consumes the fd. On failure close it and destroy the buffer.
            let raw = dup.into_raw_fd();
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(raw);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size.max(size))
                    .memory_type_index(mem_type)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    libc::close(raw); // failed import does not consume the fd
                    self.device.destroy_buffer(buffer, None);
                    return Err(anyhow!("import dmabuf memory: {e}"));
                }
            };
            if let Err(e) = self.device.bind_buffer_memory(buffer, memory, 0) {
                // `memory` owns the imported fd — freeing it releases the fd too.
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
                return Err(e).context("bind import memory");
            }
            self.src_cache.insert(
                fd,
                SrcBuf {
                    buffer,
                    memory,
                    size,
                },
            );
            Ok(())
        }
    }

    /// Recreate the exportable destination if it is smaller than `size`, plus its CUDA mapping.
    unsafe fn ensure_dst(&mut self, size: u64) -> Result<()> {
        // SAFETY: caller contract: this thread owns the handles. Builder infos are locals that
        // outlive the call. Created handles are destroyed on error or owned by `DstBuf`.
        unsafe {
            if self.dst.as_ref().is_some_and(|d| d.size >= size) {
                return Ok(());
            }
            // Build the replacement fully before retiring the old one. Raw ash handles have no
            // Drop; `VkBridge::drop` only frees live `self.dst`. Swap only on full success.
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        // STORAGE: NV12 CSC writes it as an SSBO.
                        .usage(
                            vk::BufferUsageFlags::TRANSFER_DST
                                | vk::BufferUsageFlags::STORAGE_BUFFER,
                        )
                        .push_next(&mut ext_info),
                    None,
                )
                .context("create export buffer")?;
            let reqs = self.device.get_buffer_memory_requirements(buffer);
            let mem_type = match self
                .memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            {
                Ok(t) => t,
                Err(e) => {
                    self.device.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    self.device.destroy_buffer(buffer, None);
                    return Err(e).context("allocate exportable memory");
                }
            };
            if let Err(e) = self.device.bind_buffer_memory(buffer, memory, 0) {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
                return Err(e).context("bind export memory");
            }
            let opaque_fd = match self.ext_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            ) {
                Ok(f) => f,
                Err(e) => {
                    self.device.free_memory(memory, None);
                    self.device.destroy_buffer(buffer, None);
                    return Err(e).context("vkGetMemoryFdKHR");
                }
            };
            // CUDA owns the fd on success. Size must match the allocation. `import_owned_fd`
            // closes `opaque_fd` on failure, so only Vulkan objects unwind here.
            let cuda = match cuda::ExternalDmabuf::import_owned_fd(opaque_fd, reqs.size) {
                Ok(c) => c,
                Err(e) => {
                    self.device.free_memory(memory, None);
                    self.device.destroy_buffer(buffer, None);
                    return Err(e).context("cuImportExternalMemory(OPAQUE_FD from Vulkan)");
                }
            };
            if let Some(old) = self.dst.take() {
                self.device.destroy_buffer(old.buffer, None);
                self.device.free_memory(old.memory, None);
            }
            tracing::info!(size, "Vulkan→CUDA exportable staging buffer ready");
            self.dst = Some(DstBuf {
                buffer,
                memory,
                size: reqs.size,
                cuda,
            });
            Ok(())
        }
    }

    /// Build the RGB→NV12 compute pipeline once: two-SSBO set + a 28-byte push-constant
    /// block matching `rgb2nv12_buf.comp`'s `Push`. Mid-build failure destroys what exists
    /// (retry is per-frame) and leaves `self.csc` `None`.
    unsafe fn ensure_csc(&mut self) -> Result<()> {
        // SAFETY: caller contract: this thread owns the handles. `build_csc` fills `csc` front
        // to back. Failure destroys in reverse; Vulkan no-ops on the null handles a partial
        // build left.
        unsafe {
            if self.csc.is_some() {
                return Ok(());
            }
            let mut csc = Csc {
                module: vk::ShaderModule::null(),
                dset_layout: vk::DescriptorSetLayout::null(),
                playout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                dpool: vk::DescriptorPool::null(),
                dset: vk::DescriptorSet::null(),
            };
            if let Err(e) = self.build_csc(&mut csc) {
                let d = &self.device;
                d.destroy_descriptor_pool(csc.dpool, None); // frees `csc.dset` with it
                d.destroy_pipeline(csc.pipeline, None);
                d.destroy_pipeline_layout(csc.playout, None);
                d.destroy_descriptor_set_layout(csc.dset_layout, None);
                d.destroy_shader_module(csc.module, None);
                return Err(e);
            }
            self.csc = Some(csc);
            tracing::info!(
                "Vulkan-bridge NV12 compute CSC ready (LINEAR path feeds NVENC native YUV)"
            );
            Ok(())
        }
    }

    /// Fallible half of [`ensure_csc`](Self::ensure_csc): fills `csc` front to back so the
    /// caller can destroy a partial build.
    unsafe fn build_csc(&mut self, csc: &mut Csc) -> Result<()> {
        // SAFETY: caller contract (via `ensure_csc`): this thread owns the handles. Builder
        // infos are locals that outlive the call. Partial results land in `csc` for the caller
        // to destroy.
        unsafe {
            let words: Vec<u32> = CSC_SPV
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            csc.module = self
                .device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .context("create CSC shader module")?;
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            csc.dset_layout = self
                .device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .context("create CSC dset layout")?;
            let pc = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(28)];
            let layouts = [csc.dset_layout];
            csc.playout = self
                .device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&pc),
                    None,
                )
                .context("create CSC pipeline layout")?;
            let entry = c"main";
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(csc.module)
                .name(entry);
            csc.pipeline = self
                .device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(csc.playout)],
                    None,
                )
                .map_err(|(_, e)| anyhow!("create CSC pipeline: {e}"))?[0];
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(2)];
            csc.dpool = self
                .device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .context("create CSC descriptor pool")?;
            csc.dset = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(csc.dpool)
                        .set_layouts(&layouts),
                )
                .context("allocate CSC descriptor set")?[0];
            Ok(())
        }
    }

    /// Convert one LINEAR RGB dmabuf into a pooled NV12 CUDA buffer through the Vulkan CSC.
    /// Source and destination spans are validated before any import, allocation, or dispatch.
    /// The checked layout must fit Vulkan's byte sizes, the shader's u32 word offsets, and CUDA's
    /// host `usize`; `pool` must come from [`cuda::BufferPool::new_nv12`].
    pub fn import_linear_nv12(
        &mut self,
        fd: i32,
        offset: u32,
        stride: u32,
        width: u32,
        height: u32,
        pool: &cuda::BufferPool,
    ) -> Result<DeviceBuffer> {
        anyhow::ensure!(
            offset % 4 == 0 && stride % 4 == 0,
            "LINEAR dmabuf offset/stride not word-aligned ({offset}/{stride})"
        );
        let layout = nv12_layout(width, height)
            .context("NV12 destination layout exceeds addressable buffer or shader offsets")?;
        // SAFETY: `fd` is the caller's live dmabuf (`import_src` dups it). This frame's source
        // span is checked below. `nv12_layout` proved dest sizes and shader offsets;
        // `ensure_dst(layout.size)` covers the write range. Descriptor binds live src/dst
        // WHOLE_SIZE; `*Info` arrays are locals; `cmd`/`queue`/`fence` are this thread's.
        // Dispatch is ⌈w/32⌉×⌈h/16⌉ groups of 8×8, writing whole words inside that range.
        // `wait_for_fences` retires the compute pass (shader-write barrier recorded) before
        // CUDA reads the shared memory.
        unsafe {
            let span = offset as u64 + stride as u64 * height as u64;
            if !self.src_cache.contains_key(&fd) {
                let size = libc::lseek(fd, 0, libc::SEEK_END);
                anyhow::ensure!(size > 0, "lseek(dmabuf)");
                self.import_src(fd, size as u64)?;
            }
            let (src_buffer, src_size) = {
                let s = &self.src_cache[&fd];
                (s.buffer, s.size)
            };
            // This frame's chunk metadata, not the cached import, decides how far the shader reads.
            anyhow::ensure!(src_size >= span, "dmabuf smaller than frame span");
            self.ensure_dst(layout.size)?;
            self.ensure_csc()?;
            let (dst_buffer, dst_cuda_ptr) = {
                let d = self.dst.as_ref().unwrap();
                (d.buffer, d.cuda.ptr)
            };
            let csc = self.csc.as_ref().unwrap();

            let src_info = [vk::DescriptorBufferInfo::default()
                .buffer(src_buffer)
                .range(vk::WHOLE_SIZE)];
            let dst_info = [vk::DescriptorBufferInfo::default()
                .buffer(dst_buffer)
                .range(vk::WHOLE_SIZE)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(csc.dset)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&src_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(csc.dset)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&dst_info),
            ];
            self.device.update_descriptor_sets(&writes, &[]);

            self.device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .context("begin cmd")?;
            self.device
                .cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, csc.pipeline);
            self.device.cmd_bind_descriptor_sets(
                self.cmd,
                vk::PipelineBindPoint::COMPUTE,
                csc.playout,
                0,
                &[csc.dset],
                &[],
            );
            let push: [u32; 7] = [
                width,
                height,
                offset / 4,
                stride / 4,
                layout.pitch_words,
                layout.uv_offset_words,
                layout.pitch_words,
            ];
            let push_words = push.map(u32::to_ne_bytes);
            let push_bytes: &[u8] = push_words.as_flattened();
            self.device.cmd_push_constants(
                self.cmd,
                csc.playout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                push_bytes,
            );
            self.device
                .cmd_dispatch(self.cmd, width.div_ceil(32), height.div_ceil(16), 1);
            // Shader write → CUDA read of the same memory.
            let barrier = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ);
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[barrier],
                &[],
                &[],
            );
            self.device
                .end_command_buffer(self.cmd)
                .context("end cmd")?;
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds);
            self.device
                .queue_submit(self.queue, &[submit], self.fence)
                .context("queue submit")?;
            // TIMEOUT/DEVICE_LOST must not `?` out with work still running: `cmd`/`fence` are
            // reused every frame, and `ensure_dst` later destroys `dst.buffer` assuming none.
            // Drain and reset before propagating.
            if let Err(e) = self
                .device
                .wait_for_fences(&[self.fence], true, 1_000_000_000)
            {
                let _ = self.device.device_wait_idle();
                let _ = self.device.reset_fences(&[self.fence]);
                return Err(e).context("fence wait");
            }
            self.device
                .reset_fences(&[self.fence])
                .context("reset fence")?;

            cuda::make_current()?;
            let out = pool.get()?;
            cuda::copy_pitched_nv12_to_buffer(
                dst_cuda_ptr,
                dst_cuda_ptr + layout.uv_offset,
                layout.pitch_usize,
                &out,
            )?;
            Ok(out)
        }
    }

    /// Drop the cached import for `fd`. PipeWire recycles fds; without this the cache could
    /// serve a stale buffer for a reused number, or leak one entry per recycled pool buffer.
    pub fn forget_fd(&mut self, fd: i32) {
        if let Some(s) = self.src_cache.remove(&fd) {
            // SAFETY: `s.buffer`/`s.memory` were created by `import_src` and are owned by the
            // removed cache entry, so each is destroyed once. No GPU work still references
            // them: every import fence-waits before return, and this is the owning thread.
            unsafe {
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
        }
    }

    /// Bridge one LINEAR dmabuf frame into a pooled CUDA buffer: GPU copy dmabuf→exportable,
    /// then pitched CUDA copy exportable→`pool` buffer.
    pub fn import_linear(
        &mut self,
        fd: i32,
        offset: u32,
        stride: u32,
        height: u32,
        pool: &cuda::BufferPool,
    ) -> Result<DeviceBuffer> {
        // SAFETY: `fd` is the caller's live dmabuf (`import_src` dups it; Vulkan owns the dup).
        // `lseek` only queries size. `src_size >= span` is re-checked against this frame's
        // `offset`/`stride` (a cached import is not proof). `ensure_dst(span)` makes `dst` at
        // least `span`, so `cmd_copy_buffer` of `span` and the CUDA read of `[offset, span)`
        // stay in range. `*Info`/`region`/`cmds`/`submit` are locals. `cmd`/`queue`/`fence`
        // are this thread's. `wait_for_fences` retires the copy before CUDA reads. `dst` is
        // `&self.dst` and does not alias `&self.device`.
        unsafe {
            let span = offset as u64 + stride as u64 * height as u64;
            if !self.src_cache.contains_key(&fd) {
                let size = libc::lseek(fd, 0, libc::SEEK_END);
                anyhow::ensure!(size > 0, "lseek(dmabuf)");
                self.import_src(fd, size as u64)?;
            }
            let (src_buffer, src_size) = {
                let s = &self.src_cache[&fd];
                (s.buffer, s.size)
            };
            // Per frame, not per import: cached size can be smaller than this chunk's span.
            // Clamping the Vulkan copy would let the CUDA de-stride read past `dst`.
            anyhow::ensure!(src_size >= span, "dmabuf smaller than frame span");
            self.ensure_dst(span)?;
            let dst = self.dst.as_ref().unwrap();

            self.device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .context("begin cmd")?;
            let region = vk::BufferCopy::default().size(span);
            self.device
                .cmd_copy_buffer(self.cmd, src_buffer, dst.buffer, &[region]);
            self.device
                .end_command_buffer(self.cmd)
                .context("end cmd")?;
            let cmds = [self.cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds);
            self.device
                .queue_submit(self.queue, &[submit], self.fence)
                .context("queue submit")?;
            // TIMEOUT/DEVICE_LOST must not `?` out with work still running: `cmd`/`fence` are
            // reused every frame, and `ensure_dst` later destroys `dst.buffer` assuming none.
            // Drain and reset before propagating.
            if let Err(e) = self
                .device
                .wait_for_fences(&[self.fence], true, 1_000_000_000)
            {
                let _ = self.device.device_wait_idle();
                let _ = self.device.reset_fences(&[self.fence]);
                return Err(e).context("fence wait");
            }
            self.device
                .reset_fences(&[self.fence])
                .context("reset fence")?;

            cuda::make_current()?;
            let out = pool.get()?;
            cuda::copy_pitched_to_buffer(dst.cuda.ptr + offset as u64, stride as usize, &out)?;
            Ok(out)
        }
    }
}

impl Drop for VkBridge {
    fn drop(&mut self) {
        // SAFETY: Drop on the owning thread. `device_wait_idle` drains in-flight work first.
        // Every handle was created and exclusively owned by this `VkBridge`; destroy in
        // dependency order (children, then device, then instance), each exactly once.
        // `dst.cuda` drops after `free_memory`; CUDA holds its own dup'd OPAQUE_FD.
        unsafe {
            let _ = self.device.device_wait_idle();
            for (_, s) in self.src_cache.drain() {
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            if let Some(d) = self.dst.take() {
                self.device.destroy_buffer(d.buffer, None);
                self.device.free_memory(d.memory, None);
            }
            if let Some(c) = self.csc.take() {
                self.device.destroy_pipeline(c.pipeline, None);
                self.device.destroy_pipeline_layout(c.playout, None);
                self.device.destroy_descriptor_pool(c.dpool, None); // frees `c.dset` with it
                self.device
                    .destroy_descriptor_set_layout(c.dset_layout, None);
                self.device.destroy_shader_module(c.module, None);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_layout_checks_sizes_and_shader_offsets() {
        assert_eq!(
            nv12_layout(1919, 1080),
            Some(Nv12Layout {
                uv_offset: 2_073_600,
                size: 3_110_400,
                pitch_words: 480,
                uv_offset_words: 518_400,
                pitch_usize: 1920,
            })
        );
        assert_eq!(nv12_layout(0, 1080), None);
        assert_eq!(nv12_layout(1920, 0), None);
        assert_eq!(nv12_layout(u32::MAX, u32::MAX), None);
    }
}
