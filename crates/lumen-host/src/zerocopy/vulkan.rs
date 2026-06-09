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
}

// Confined to the capture thread; moved there once.
unsafe impl Send for VkBridge {}

impl VkBridge {
    /// Bring up Vulkan on the NVIDIA GPU with the external-memory extensions.
    pub fn new() -> Result<VkBridge> {
        unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .context("vkCreateInstance")?;

            // Pick the NVIDIA GPU (matches CUDA device 0 on this single-dGPU host).
            let phys = instance
                .enumerate_physical_devices()
                .context("enumerate GPUs")?
                .into_iter()
                .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
                .ok_or_else(|| anyhow!("no NVIDIA Vulkan device"))?;
            let mem_props = instance.get_physical_device_memory_properties(phys);

            // Any queue family supporting transfer (graphics/compute imply it).
            let qf = instance
                .get_physical_device_queue_family_properties(phys)
                .iter()
                .position(|q| {
                    q.queue_flags.intersects(
                        vk::QueueFlags::TRANSFER
                            | vk::QueueFlags::GRAPHICS
                            | vk::QueueFlags::COMPUTE,
                    )
                })
                .ok_or_else(|| anyhow!("no transfer-capable queue family"))?
                as u32;

            let exts = [
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ];
            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qf)
                .queue_priorities(&prio)];
            let device = instance
                .create_device(
                    phys,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&qci)
                        .enabled_extension_names(&exts),
                    None,
                )
                .context("vkCreateDevice (external-memory extensions supported?)")?;
            let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
            let queue = device.get_device_queue(qf, 0);

            let cmd_pool = device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(qf)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .context("create command pool")?;
            let cmd = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .context("allocate command buffer")?[0];
            let fence = device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .context("create fence")?;

            tracing::info!("Vulkan bridge ready (dmabuf import → OPAQUE_FD export → CUDA)");
            Ok(VkBridge {
                _entry: entry,
                instance,
                device,
                ext_fd,
                queue,
                cmd_pool,
                cmd,
                fence,
                mem_props,
                src_cache: HashMap::new(),
                dst: None,
            })
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
        let dup = libc::dup(fd);
        if dup < 0 {
            bail!("dup(dmabuf fd)");
        }
        let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let buffer = self
            .device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .push_next(&mut ext_info),
                None,
            )
            .context("create import buffer")?;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        self.ext_fd
            .get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                dup,
                &mut fd_props,
            )
            .context("vkGetMemoryFdPropertiesKHR")?;
        let reqs = self.device.get_buffer_memory_requirements(buffer);
        let mem_type = self.memory_type(
            reqs.memory_type_bits & fd_props.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )?;
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup); // Vulkan takes ownership of `dup` on success
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let memory = self
            .device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size.max(size))
                    .memory_type_index(mem_type)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            )
            .map_err(|e| {
                libc::close(dup); // failed import does not consume the fd
                anyhow!("import dmabuf memory: {e}")
            })?;
        self.device
            .bind_buffer_memory(buffer, memory, 0)
            .context("bind import memory")?;
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

    /// (Re)create the exportable destination of at least `size` bytes + its CUDA mapping.
    unsafe fn ensure_dst(&mut self, size: u64) -> Result<()> {
        if self.dst.as_ref().is_some_and(|d| d.size >= size) {
            return Ok(());
        }
        if let Some(old) = self.dst.take() {
            self.device.destroy_buffer(old.buffer, None);
            self.device.free_memory(old.memory, None);
            // old.cuda drops its mapping with it
        }
        let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let buffer = self
            .device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .push_next(&mut ext_info),
                None,
            )
            .context("create export buffer")?;
        let reqs = self.device.get_buffer_memory_requirements(buffer);
        let mem_type =
            self.memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let memory = self
            .device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            )
            .context("allocate exportable memory")?;
        self.device
            .bind_buffer_memory(buffer, memory, 0)
            .context("bind export memory")?;
        let opaque_fd = self
            .ext_fd
            .get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            )
            .context("vkGetMemoryFdKHR")?;
        // CUDA imports (and on success owns) the exported fd. Size must match the allocation.
        let cuda = cuda::ExternalDmabuf::import_owned_fd(opaque_fd, reqs.size)
            .context("cuImportExternalMemory(OPAQUE_FD from Vulkan)")?;
        tracing::info!(size, "Vulkan→CUDA exportable staging buffer ready");
        self.dst = Some(DstBuf {
            buffer,
            memory,
            size: reqs.size,
            cuda,
        });
        Ok(())
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
        unsafe {
            let span = offset as u64 + stride as u64 * height as u64;
            if !self.src_cache.contains_key(&fd) {
                let size = libc::lseek(fd, 0, libc::SEEK_END);
                anyhow::ensure!(size > 0, "lseek(dmabuf)");
                anyhow::ensure!(size as u64 >= span, "dmabuf smaller than frame span");
                self.import_src(fd, size as u64)?;
            }
            let (src_buffer, src_size) = {
                let s = &self.src_cache[&fd];
                (s.buffer, s.size)
            };
            let copy_size = src_size.min(span);
            self.ensure_dst(copy_size)?;
            let dst = self.dst.as_ref().unwrap();

            // Record + submit the GPU copy, wait on the fence (GPU-GPU, sub-millisecond).
            self.device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .context("begin cmd")?;
            let region = vk::BufferCopy::default().size(copy_size);
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
            self.device
                .wait_for_fences(&[self.fence], true, 1_000_000_000)
                .context("fence wait")?;
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
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
