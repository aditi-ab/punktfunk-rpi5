//! Vulkan bridge for LINEAR dmabufs (gamescope's only offer), completing zero-copy where the
//! other interops can't: NVIDIA's EGL won't sample LINEAR, and the CUDA driver rejects raw
//! dmabuf fds as external memory. Vulkan *does* import dmabufs (`VK_EXT_external_memory_dma_buf`)
//! and *does* export `OPAQUE_FD` memory that CUDA officially imports. So:
//!
//! ```text
//!   dmabuf fd ──VkImportMemoryFdInfoKHR(DMA_BUF)──▶ VkBuffer (cached per fd)
//!        │ vkCmdCopyBuffer (GPU, device-local)
//!        ▼
//!   exportable VkBuffer ──vkGetMemoryFdKHR(OPAQUE_FD)──▶ cuImportExternalMemory ──▶ CUdeviceptr
//! ```
//!
//! The exportable buffer + its CUDA mapping are created once per resolution; per frame it's one
//! GPU buffer copy (fence-waited) and one pitched CUDA copy into the encoder's pooled buffer.
//! No CPU ever touches pixels. Imports are cached per fd (PipeWire's buffer pool is stable for
//! a stream's life). Falls back cleanly: any init/import error disables the importer and the
//! CPU mmap path takes over.

use super::cuda::{self, DeviceBuffer};
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use std::collections::HashMap;

/// Vulkan objects for one imported source dmabuf (cached per fd).
struct SrcBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
}

/// The per-resolution destination: exportable Vulkan memory mapped into CUDA.
struct DstBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    /// CUDA's view of the same memory (owns the exported OPAQUE_FD).
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

/// The lazy compute-CSC pipeline (`rgb2nv12_buf.comp`) for [`VkBridge::import_linear_nv12`].
struct Csc {
    module: vk::ShaderModule,
    dset_layout: vk::DescriptorSetLayout,
    playout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    dpool: vk::DescriptorPool,
    dset: vk::DescriptorSet,
}

/// The buffer-to-buffer RGB→NV12 compute shader (see `rgb2nv12_buf.comp` beside this file;
/// rebuild with `glslangValidator -V rgb2nv12_buf.comp -o rgb2nv12_buf.spv`; CI gates drift).
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
    /// Built on the first [`import_linear_nv12`](Self::import_linear_nv12); RGB-only bridges
    /// never pay for it.
    csc: Option<Csc>,
}

// SAFETY: `VkBridge` owns ash Vulkan handles (instance/device/queue/command pool+buffer/fence), a
// CUDA external-memory mapping, and an fd→buffer cache — none `Sync`, and a single queue +
// command buffer must be externally synchronized. It is created inside `EglImporter::import_linear`
// on the dedicated `punktfunk-pipewire` capture thread and every method (`import_linear`, `Drop`)
// runs on that thread; it is never shared via `&` across threads. `Send` asserts only that
// transferring ownership is sound (so the bridge can live inside the `Send` `EglImporter`); the live
// handles are never touched off-thread, and `Sync` is deliberately NOT implied.
unsafe impl Send for VkBridge {}

impl VkBridge {
    /// Bring up Vulkan on the NVIDIA GPU with the external-memory extensions.
    pub fn new() -> Result<VkBridge> {
        // SAFETY: standard ash bring-up — every call is `unsafe` only because ash cannot statically
        // verify Vulkan handle/CreateInfo validity. `ash::Entry::load` dlopens a real system
        // libvulkan. Each `*CreateInfo`/`AllocateInfo` is built by ash's builders from locals (`app`,
        // `exts`, `prio`, `qci`, `gp_info`, and the inline infos) that all live for the duration of
        // the synchronous `create_*`/`enumerate_*` call that reads them — the ladder loop rebuilds
        // `prio`/`gp_info`/`qci`/`exts` fresh per attempt, so every `enabled_extension_names(&exts)`
        // / `queue_priorities(&prio)` / `push_next(&mut gp_info)` borrow outlives its own
        // `create_device` call.
        // Every handle passed (`instance`, `phys`, `device`, `qf`, `cmd_pool`) was just created and
        // checked via `?`/`ok_or_else` in this same function, so no invalid handle is ever used. This
        // constructor shares nothing across threads.
        unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .context("vkCreateInstance")?;

            // Pick the NVIDIA GPU (matches CUDA device 0 on this single-dGPU host). Failure paths
            // between instance and device creation destroy the instance explicitly (the shape
            // `VkSlotBlend::new` uses) — a box that refuses the bridge retries per frame, so a
            // leak here is unbounded, not one-shot.
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

            // A COMPUTE-capable family (compute implies transfer): the copy path only needs
            // transfer, but the NV12 CSC dispatch (T2.5b) needs compute — on every NVIDIA
            // device family 0 is graphics+compute+transfer, so this picks the same family the
            // old transfer-only predicate did.
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

            // Global-priority queue (latency plan §7 LN4, PyroWave's `ac0e7332` lever for the
            // VkBridge): the LINEAR/gamescope CSC dispatch shares the SM/compute cores with the
            // game, so ask for an elevated global priority to get scheduled ahead of it.
            // `PUNKTFUNK_VK_QUEUE_PRIORITY` = off | high | realtime (default realtime); the
            // create loop downgrades REALTIME→HIGH→none on NOT_PERMITTED (and retries a plain
            // create on INITIALIZATION_FAILED) so a refused class never fails the bridge.
            let gp_ext = std::env::var("PUNKTFUNK_VK_QUEUE_PRIORITY")
                .ok()
                .as_deref()
                .map_or(Some(vk::QueueGlobalPriorityKHR::REALTIME), |v| match v {
                    "off" | "0" => None,
                    "high" => Some(vk::QueueGlobalPriorityKHR::HIGH),
                    _ => Some(vk::QueueGlobalPriorityKHR::REALTIME),
                })
                .and_then(|want| {
                    // Enable whichever alias the driver advertises (KHR = the promoted name).
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
                    // A refused class must never fail the bridge — walk the ladder down.
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
            // From here teardown-on-error goes through `Drop`, which tolerates null handles
            // (Vulkan destroy calls are defined no-ops on VK_NULL_HANDLE) — build the remaining
            // objects into an incrementally-filled struct so any `?` below unwinds cleanly
            // instead of leaking the instance + device.
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
        // SAFETY: caller contract: single-threaded use of handles this bridge owns; every builder
        // info is built from locals that outlive the synchronous call reading them, and every
        // fallible step destroys what it created before returning (comments below).
        unsafe {
            use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
            let dup = libc::dup(fd);
            if dup < 0 {
                bail!("dup(dmabuf fd)");
            }
            // Own the dup so every early return BEFORE Vulkan consumes it (at `allocate_memory` success)
            // closes it. `SrcBuf` holds raw handles with no Drop and is only populated on the success
            // path, so each fallible step below must also destroy the buffer it created — otherwise a
            // failed import (which the worker survives and the caller retries every frame) leaks a
            // VkBuffer + VkDeviceMemory + fd per frame for the worker's whole lifetime.
            let dup = OwnedFd::from_raw_fd(dup);
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        // STORAGE so the NV12 compute CSC can read it as an SSBO (T2.5b); harmless
                        // for the plain copy path.
                        .usage(
                            vk::BufferUsageFlags::TRANSFER_SRC
                                | vk::BufferUsageFlags::STORAGE_BUFFER,
                        )
                        .push_next(&mut ext_info),
                    None,
                )
                .context("create import buffer")?; // `dup` drops → closes on failure
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
            // Vulkan takes ownership of the fd on a SUCCESSFUL import: hand over the raw fd now, and on
            // failure close it ourselves (matching the original contract) plus destroy the buffer.
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

    /// (Re)create the exportable destination of at least `size` bytes + its CUDA mapping.
    unsafe fn ensure_dst(&mut self, size: u64) -> Result<()> {
        // SAFETY: caller contract: single-threaded use of handles this bridge owns; every builder
        // info is built from locals that outlive the synchronous call reading them; created handles
        // are destroyed on the error paths below or owned by `DstBuf`.
        unsafe {
            if self.dst.as_ref().is_some_and(|d| d.size >= size) {
                return Ok(());
            }
            // Build the replacement FULLY before retiring the old one. Previously the old dst was
            // destroyed and `self.dst` nulled up front, so a failed rebuild both dropped the working
            // buffer AND leaked every object the partial rebuild created (`buffer`/`memory` are raw ash
            // handles with no Drop, and `VkBridge::drop` only frees the live `self.dst`). Now every
            // fallible step unwinds locally, and the swap happens only on full success.
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        // STORAGE so the NV12 compute CSC can write it as an SSBO (T2.5b).
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
            // CUDA imports (and on success owns) the exported fd. Size must match the allocation.
            // `import_owned_fd` closes `opaque_fd` on its own failure, so only the Vulkan objects unwind.
            let cuda = match cuda::ExternalDmabuf::import_owned_fd(opaque_fd, reqs.size) {
                Ok(c) => c,
                Err(e) => {
                    self.device.free_memory(memory, None);
                    self.device.destroy_buffer(buffer, None);
                    return Err(e).context("cuImportExternalMemory(OPAQUE_FD from Vulkan)");
                }
            };
            // Full success: retire the previous buffer now, then publish the new one.
            if let Some(old) = self.dst.take() {
                self.device.destroy_buffer(old.buffer, None);
                self.device.free_memory(old.memory, None);
                // old.cuda drops its mapping with it
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

    /// Build the RGB→NV12 compute pipeline once (T2.5b): two-SSBO descriptor set + a 28-byte
    /// push-constant block matching `rgb2nv12_buf.comp`'s `Push`. A mid-build failure destroys
    /// what was already created (the caller retries per frame, so a leak would be unbounded) and
    /// leaves `self.csc` `None`.
    unsafe fn ensure_csc(&mut self) -> Result<()> {
        // SAFETY: caller contract: single-threaded use of handles this bridge owns. `build_csc`
        // fills `csc` front to back; on failure the destroy calls below run in reverse creation
        // order, and Vulkan destroys are defined no-ops on the null handles a partial build left.
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
                // Reverse creation order; Vulkan destroy calls are defined no-ops on the null
                // handles a partial build left behind.
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

    /// The fallible half of [`ensure_csc`](Self::ensure_csc): fills `csc` front to back so the
    /// caller knows exactly what to destroy when a step fails.
    unsafe fn build_csc(&mut self, csc: &mut Csc) -> Result<()> {
        // SAFETY: caller contract (via `ensure_csc`): single-threaded use of handles this bridge
        // owns; every builder info is built from locals that outlive the synchronous call reading
        // them; partial results land in `csc` for the caller to destroy on failure.
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
        // SAFETY: same structure and proofs as `import_linear` — `fd` is the caller's live dmabuf
        // (dup'd by `import_src`), and this frame's source span is checked below. `nv12_layout`
        // proved every destination size and shader offset; `ensure_dst(layout.size)` allocates the
        // shader's full write range. The descriptor
        // update binds the live cached src buffer and the live dst buffer WHOLE_SIZE; every
        // `*Info`/array is a local outliving its synchronous call; `cmd`/`queue`/`fence` are this
        // bridge's own single-thread handles. The dispatch covers ⌈w/32⌉×⌈h/16⌉ groups of 8×8
        // invocations, each writing only whole words inside the proven dst range (shader
        // contract). The host `wait_for_fences` retires the compute pass (with a shader-write →
        // memory barrier recorded before end) BEFORE CUDA reads the shared memory.
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
            // As in `import_linear`: this frame's chunk metadata, not the cached import's, decides
            // how far the shader reads.
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
            // Make the shader writes available before the external (CUDA) read.
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
            // Exception-safe wait: a TIMEOUT/DEVICE_LOST must not `?` out with the submission still
            // executing — `self.cmd` and `self.fence` are reused every frame, and the caller retries
            // on the SAME bridge (and `ensure_dst` later destroys `dst.buffer` assuming no in-flight
            // work references it). Drain the GPU and reset the fence before propagating so the shared
            // cmd/fence return clean.
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

            // De-stride both NV12 planes from the CUDA view into a pooled two-plane buffer.
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

    /// Drop the cached import for `fd` (the PipeWire buffer it wrapped is gone — pool recycle /
    /// renegotiation — or the caller is about to store a different dmabuf under the same slot).
    /// Without this the cache could serve a stale imported buffer for a reused fd number, or
    /// leak an entry per recycled pool buffer.
    pub fn forget_fd(&mut self, fd: i32) {
        if let Some(s) = self.src_cache.remove(&fd) {
            // SAFETY: `s.buffer`/`s.memory` were created by this bridge's `import_src` and are
            // exclusively owned by the removed cache entry, so each is destroyed exactly once.
            // No GPU work can still reference them: every `import_linear` fence-waits its copy to
            // completion before returning, and this runs on the same single owning thread.
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
        // SAFETY: `fd` is the live dmabuf fd handed in by the caller (borrowed; `import_src` dup's it
        // internally and Vulkan owns the dup). `libc::lseek` only queries the fd's size. The unsafe
        // `import_src`/`ensure_dst` are called with a valid fd and a checked size. The bounds are
        // proven: `src_size >= span` is re-checked below against THIS frame's `offset`/`stride`
        // (an import cached on an earlier frame is not proof for this one), and `ensure_dst(span)`
        // makes `dst` at least `span` — so the GPU `cmd_copy_buffer` of `span` bytes reads/writes
        // within both buffers, and the later CUDA pitched copy reading `[offset, span)` from
        // `dst.cuda.ptr` (= `offset + stride*height = span`) stays inside the copied region. The
        // `*Info`/`region`/`cmds`/`submit` are locals that outlive the synchronous calls reading them.
        // `cmd`/`queue`/`fence` are this bridge's own handles, used on this single thread only. The
        // host-side `wait_for_fences` fully retires the Vulkan copy BEFORE CUDA reads the shared
        // memory, so there is no GPU write/read data race. `dst` is an `&self.dst` shared borrow that
        // does not alias the `&self.device` calls.
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
            // Per frame, not per import: `offset`/`stride` come from this frame's PipeWire chunk
            // metadata, so a cached import can be smaller than the span they describe. Clamping
            // the Vulkan copy instead would leave the CUDA de-stride below reading past `dst`.
            anyhow::ensure!(src_size >= span, "dmabuf smaller than frame span");
            self.ensure_dst(span)?;
            let dst = self.dst.as_ref().unwrap();

            // Record + submit the GPU copy, wait on the fence (GPU-GPU, sub-millisecond).
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
            // Exception-safe wait: a TIMEOUT/DEVICE_LOST must not `?` out with the submission still
            // executing — `self.cmd` and `self.fence` are reused every frame, and the caller retries
            // on the SAME bridge (and `ensure_dst` later destroys `dst.buffer` assuming no in-flight
            // work references it). Drain the GPU and reset the fence before propagating so the shared
            // cmd/fence return clean.
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

            // De-stride from the CUDA view of the exportable memory into a pooled buffer.
            cuda::make_current()?;
            let out = pool.get()?;
            cuda::copy_pitched_to_buffer(dst.cuda.ptr + offset as u64, stride as usize, &out)?;
            Ok(out)
        }
    }
}

impl Drop for VkBridge {
    fn drop(&mut self) {
        // SAFETY: runs once when the bridge is dropped on its owning capture thread.
        // `device_wait_idle` first drains all in-flight GPU work, so no queued command still
        // references these objects. Every handle freed (the `src_cache` buffers+memories, the `dst`
        // buffer+memory, `fence`, `cmd_pool`, `device`, `instance`) was created by this `VkBridge`
        // and owned exclusively by it, so each `destroy_*`/`free_*` runs exactly once with no
        // double-free, in dependency order (child objects before `device`, `device` before
        // `instance`). `dst.cuda` is dropped after `free_memory`, which is safe because CUDA holds
        // its own dup'd OPAQUE_FD reference to the underlying allocation. No other thread touches
        // these handles.
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
