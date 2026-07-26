//! Vulkan-allocated NVENC input slots + the Vulkan compute cursor blend — the driver-portable
//! replacement for the retired `cursor_blend.cu` PTX kernels (design: remote-desktop-sweep §8,
//! Phase A). A vendored PTX blob is JIT'd against the driver's ISA ceiling, so it silently dies
//! on drivers older than the generating toolkit (CUDA errors 222/218 on-glass — the KWin leg's
//! invisible composite cursor); SPIR-V has no such coupling.
//!
//! ```text
//!   exportable VkBuffer ──vkGetMemoryFdKHR(OPAQUE_FD)──▶ cuImportExternalMemory ──▶ CUdeviceptr
//!        ▲                                                     │ NVENC registers + encodes
//!        └── cursor_blend.comp dispatch (cursor rect only) ◀───┘ CUDA copies frames in
//! ```
//!
//! The direct-SDK NVENC encoder allocates its input ring through [`VkSlotBlend::alloc_slot`]
//! instead of `cuMemAllocPitch`: same contiguous layouts (`InputSurface` docs), but the memory is
//! Vulkan external memory both APIs address.
//!
//! Cursor-bearing frames blend one of two ways:
//!
//! * **Stream-ordered** ([`blend_ref_ordered`](VkSlotBlend::blend_ref_ordered), the fast path
//!   when the driver exports a timeline semaphore to CUDA): the encoder enqueues its CUDA copy
//!   with no CPU sync, CUDA signals the shared timeline on the copy stream, the blend submission
//!   waits for and then advances it on the Vulkan queue, and CUDA waits the advanced value
//!   before the encode — the whole copy→blend→encode chain orders on-device, so a visible
//!   cursor costs the submit path nothing. (Before this, EVERY cursor frame took the CPU-synced
//!   path below; under gamescope — which composites the pointer into every frame — that
//!   serialized submit behind the game's GPU load and capped a 120 fps session near 80.)
//! * **CPU-synced** ([`blend_ref`](VkSlotBlend::blend_ref), the fallback when timeline bring-up
//!   failed): the encoder blocks on its CUDA copy, the blend fence-waits — the same coherence
//!   ceremony [`super::vulkan::VkBridge`] ships for its CSC (fence-ordered cross-API access on
//!   NVIDIA, no queue-family transfer needed).
//!
//! Frames without a cursor never touch Vulkan at all.
//!
//! Falls back cleanly: if bring-up fails the encoder allocates plain CUDA surfaces and composite
//! mode degrades to no cursor (warned once) — never a failed session.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::cuda::{self, CUdeviceptr};
use anyhow::{anyhow, Context as _, Result};
use ash::vk;

/// Max cursor-overlay bitmap edge (px) — matches [`cuda::CURSOR_MAX`] and the capture-side clamp.
pub const CURSOR_MAX: u32 = cuda::CURSOR_MAX;

/// The vendored SPIR-V for `cursor_blend.comp` (beside this file; rebuild with
/// `glslc cursor_blend.comp -o cursor_blend.spv`).
const CURSOR_SPV: &[u8] = include_bytes!("cursor_blend.spv");

/// NVENC input-surface layout — selects the spec-constant `MODE` pipeline and the allocation
/// arithmetic (mirroring `InputSurface`'s contiguous layouts).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotFormat {
    /// Packed 4-byte ARGB (NVENC byte order B,G,R,A): `pitch × height`.
    Argb,
    /// NV12: Y rows `[0, H)` + interleaved UV rows `[H, 3H/2)` under one pitch.
    Nv12,
    /// Planar YUV444: three full-res planes stacked at `pitch × height` intervals.
    Yuv444,
}

impl SlotFormat {
    fn mode(self) -> u32 {
        match self {
            SlotFormat::Argb => 0,
            SlotFormat::Nv12 => 1,
            SlotFormat::Yuv444 => 2,
        }
    }
    fn row_bytes(self, width: u32) -> u64 {
        match self {
            SlotFormat::Argb => width as u64 * 4,
            SlotFormat::Nv12 | SlotFormat::Yuv444 => width as u64,
        }
    }
    fn rows(self, height: u32) -> u64 {
        match self {
            SlotFormat::Argb => height as u64,
            SlotFormat::Nv12 => height as u64 + (height as u64 / 2).max(1),
            SlotFormat::Yuv444 => height as u64 * 3,
        }
    }
}

/// What the encoder holds per ring slot: the CUDA view it registers with NVENC plus the id it
/// hands back to [`VkSlotBlend::blend`]. The backing Vulkan objects + CUDA mapping live in the
/// [`VkSlotBlend`] (freed by [`free_slots`](VkSlotBlend::free_slots) / drop), so this is Copy —
/// the encoder's ring keeps its existing shape.
#[derive(Clone, Copy)]
pub struct VkSlotRef {
    /// Device pointer NVENC registers (CUDA's mapping of the Vulkan memory).
    pub ptr: CUdeviceptr,
    /// Row stride in bytes (ours: row bytes rounded up to 256).
    pub pitch: usize,
    /// Luma rows (the plane-stride multiplier, as in `InputSurface`).
    pub height: u32,
    /// Index into the blend's slot table.
    pub id: usize,
}

/// One allocated slot's backing objects, freed together in reverse order (CUDA mapping first).
/// Each slot carries its own command buffer + descriptor set so ordered blends can be in flight
/// on several slots at once (the shared-single-set design raced the next recording against a
/// still-executing submission); the set's bindings are written once here — the slot buffer never
/// changes and the cursor staging buffer is shared.
struct SlotAlloc {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// CUDA's import of the exported OPAQUE_FD — must drop BEFORE the Vulkan memory is freed.
    cuda: cuda::ExternalDmabuf,
    cmd: vk::CommandBuffer,
    desc: vk::DescriptorSet,
}

/// The cross-API ordering state for stream-ordered blends: one Vulkan timeline semaphore,
/// exported as an OPAQUE_FD and imported into CUDA. `None` when the driver lacks timeline
/// semaphores or the export/import failed — blends then stay CPU-synced ([`VkSlotBlend::blend_ref`]).
struct Timeline {
    sem: vk::Semaphore,
    /// `vkWaitSemaphoresKHR` — the CPU-side quiesce for cursor-bitmap uploads and teardown
    /// (the 1.1 device gets the entry point from `VK_KHR_timeline_semaphore`).
    ts: ash::khr::timeline_semaphore::Device,
    /// CUDA's import; its `signal`/`wait` enqueue on the encode thread's copy stream.
    cuda: cuda::ExternalSemaphore,
    /// Last timeline value handed out (monotonic for the device's lifetime, never reused —
    /// each ordered blend consumes two: copy-done, then blend-done).
    ticket: u64,
    /// Last blend-done value an ACCEPTED `vkQueueSubmit` will signal — what upload/teardown
    /// quiesce on. Only advanced on submit success: a value from a failed submit would never
    /// be signaled and a quiesce on it would time out.
    last_blend: u64,
}

/// 28-byte push-constant block matching `cursor_blend.comp`'s `Push`.
#[repr(C)]
struct Push {
    pitch: u32,
    surf_w: u32,
    surf_h: u32,
    cur_w: u32,
    cur_h: u32,
    ox: i32,
    oy: i32,
}

pub struct VkSlotBlend {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    shader: vk::ShaderModule,
    desc_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    desc_pool: vk::DescriptorPool,
    /// One pipeline per [`SlotFormat`], indexed by `mode()` (spec constant).
    pipelines: [vk::Pipeline; 3],
    /// Host-visible cursor bitmap staging (CURSOR_MAX²·4, tight rows), persistently mapped.
    cur_buf: vk::Buffer,
    cur_mem: vk::DeviceMemory,
    cur_map: *mut u8,
    slots: Vec<SlotAlloc>,
    /// Stream-ordered blend support (`None` = CPU-synced blends only). See [`Timeline`].
    timeline: Option<Timeline>,
}

// SAFETY: raw Vulkan handles + a persistently-mapped pointer, all uniquely owned by this struct
// and destroyed exactly once in `Drop`; used from the encoder thread but moved with it. `Send`
// only (not `Sync`), matching the single-thread use — transferring opaque handles cannot dangle.
unsafe impl Send for VkSlotBlend {}

impl VkSlotBlend {
    /// Bring up the device + blend pipelines. Requires the CUDA shared context (the encoder's) to
    /// be established; picks the NVIDIA physical device (the NVENC path is NVIDIA by definition).
    pub fn new() -> Result<VkSlotBlend> {
        // SAFETY: standard ash bring-up, same shape as `VkBridge::new` — every call is `unsafe`
        // only because ash cannot statically verify handle/CreateInfo validity. Every
        // `*CreateInfo`/`AllocateInfo` is built from locals that live for the duration of the
        // synchronous call reading them; every handle passed was created and `?`-checked in this
        // same function. Shares nothing across threads.
        unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .context("vkCreateInstance")?;
            let phys = match instance
                .enumerate_physical_devices()
                .context("enumerate GPUs")?
                .into_iter()
                .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
            {
                Some(p) => p,
                None => {
                    instance.destroy_instance(None);
                    return Err(anyhow!("no NVIDIA Vulkan device"));
                }
            };
            let mem_props = instance.get_physical_device_memory_properties(phys);
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
            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qf)
                .queue_priorities(&prio)];
            // Timeline-semaphore export to CUDA (the stream-ordered blend) is optional: probe the
            // device extensions + feature bit and enable them only where present, so a driver
            // without them still gets a working (CPU-synced) blend device. NVIDIA has shipped
            // both extensions since ~2019; the probe is for exotic/legacy stacks.
            let want_timeline = {
                let have_exts = instance
                    .enumerate_device_extension_properties(phys)
                    .map(|props| {
                        let has = |name: &std::ffi::CStr| {
                            props
                                .iter()
                                .any(|p| p.extension_name_as_c_str().is_ok_and(|n| n == name))
                        };
                        has(ash::khr::timeline_semaphore::NAME)
                            && has(ash::khr::external_semaphore_fd::NAME)
                    })
                    .unwrap_or(false);
                have_exts && {
                    let mut tl = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
                    let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut tl);
                    instance.get_physical_device_features2(phys, &mut f2);
                    tl.timeline_semaphore == vk::TRUE
                }
            };
            let mut exts = vec![ash::khr::external_memory_fd::NAME.as_ptr()];
            if want_timeline {
                exts.push(ash::khr::timeline_semaphore::NAME.as_ptr());
                exts.push(ash::khr::external_semaphore_fd::NAME.as_ptr());
            }
            let mut tl_enable =
                vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);
            let mut dci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&qci)
                .enabled_extension_names(&exts);
            if want_timeline {
                dci = dci.push_next(&mut tl_enable);
            }
            let device = match instance.create_device(phys, &dci, None) {
                Ok(d) => d,
                Err(e) => {
                    instance.destroy_instance(None);
                    return Err(e).context("vkCreateDevice (external_memory_fd supported?)");
                }
            };
            // From here teardown-on-error goes through `destroy_partial`, which tolerates null
            // handles — build everything into an incrementally-filled struct.
            let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
            let queue = device.get_device_queue(qf, 0);
            let mut me = VkSlotBlend {
                _entry: entry,
                instance,
                device,
                ext_fd,
                queue,
                cmd_pool: vk::CommandPool::null(),
                fence: vk::Fence::null(),
                mem_props,
                shader: vk::ShaderModule::null(),
                desc_layout: vk::DescriptorSetLayout::null(),
                pipe_layout: vk::PipelineLayout::null(),
                desc_pool: vk::DescriptorPool::null(),
                pipelines: [vk::Pipeline::null(); 3],
                cur_buf: vk::Buffer::null(),
                cur_mem: vk::DeviceMemory::null(),
                cur_map: std::ptr::null_mut(),
                slots: Vec::new(),
                timeline: None,
            };
            me.init_objects(qf).inspect_err(|_| {
                // `Drop` runs the same teardown and tolerates the nulls left by a partial init.
            })?;
            if want_timeline {
                // Non-fatal: any failure (export refused, CUDA import refused) leaves
                // `timeline` None and blends CPU-synced — never a failed bring-up.
                if let Err(e) = me.init_timeline() {
                    tracing::info!(
                        error = %format!("{e:#}"),
                        "cursor blend timeline export unavailable — blends stay CPU-synced"
                    );
                }
            }
            tracing::info!(
                stream_ordered = me.timeline.is_some(),
                "Vulkan slot blend ready (exportable NVENC inputs + SPIR-V cursor blend)"
            );
            Ok(me)
        }
    }

    /// Bring up the [`Timeline`]: create an exportable timeline semaphore, export its OPAQUE_FD,
    /// import it into CUDA. Only called when the device was created with the timeline extensions.
    fn init_timeline(&mut self) -> Result<()> {
        // SAFETY: ash calls on the live `self.device` with builder infos from locals outliving
        // each synchronous call. The semaphore is destroyed on every failure path after its
        // creation; the exported fd is owned by `import_owned_timeline_fd` (the driver takes it
        // on success, it closes it on failure). The shared CUDA context is current: the encoder
        // brings this device up from its encode thread with the context established.
        unsafe {
            let mut type_ci = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            let sem = self
                .device
                .create_semaphore(
                    &vk::SemaphoreCreateInfo::default()
                        .push_next(&mut type_ci)
                        .push_next(&mut export),
                    None,
                )
                .context("create timeline semaphore")?;
            let sem_fd = ash::khr::external_semaphore_fd::Device::new(&self.instance, &self.device);
            let fd = match sem_fd.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(sem)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            ) {
                Ok(f) => f,
                Err(e) => {
                    self.device.destroy_semaphore(sem, None);
                    return Err(e).context("vkGetSemaphoreFdKHR(timeline)");
                }
            };
            let cuda_sem = match cuda::ExternalSemaphore::import_owned_timeline_fd(fd) {
                Ok(c) => c,
                Err(e) => {
                    self.device.destroy_semaphore(sem, None);
                    return Err(e);
                }
            };
            self.timeline = Some(Timeline {
                sem,
                ts: ash::khr::timeline_semaphore::Device::new(&self.instance, &self.device),
                cuda: cuda_sem,
                ticket: 0,
                last_blend: 0,
            });
            Ok(())
        }
    }

    /// The non-device objects: command machinery, cursor staging, descriptor + pipelines.
    fn init_objects(&mut self, qf: u32) -> Result<()> {
        // SAFETY: same contract as `new` — ash calls on the live `self.device` with builder infos
        // from locals outliving each synchronous call; created handles are stored into `self`
        // immediately so the caller's `Drop` frees them on any later failure.
        unsafe {
            let d = &self.device;
            self.cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(qf)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .context("create command pool")?;
            self.fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .context("create fence")?;

            // Cursor staging: host-visible+coherent SSBO, persistently mapped.
            let cur_size = (CURSOR_MAX * CURSOR_MAX * 4) as u64;
            self.cur_buf = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(cur_size)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .context("create cursor buffer")?;
            let reqs = d.get_buffer_memory_requirements(self.cur_buf);
            let mem_type = self
                .memory_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .context("cursor buffer memory type")?;
            self.cur_mem = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type),
                    None,
                )
                .context("allocate cursor memory")?;
            d.bind_buffer_memory(self.cur_buf, self.cur_mem, 0)
                .context("bind cursor memory")?;
            self.cur_map = d
                .map_memory(self.cur_mem, 0, cur_size, vk::MemoryMapFlags::empty())
                .context("map cursor memory")? as *mut u8;

            // Descriptor set: binding 0 = surface SSBO (rebound per blend), 1 = cursor SSBO.
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
            self.desc_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .context("create descriptor layout")?;
            let pc = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(std::mem::size_of::<Push>() as u32)];
            let dl = [self.desc_layout];
            self.pipe_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&dl)
                        .push_constant_ranges(&pc),
                    None,
                )
                .context("create pipeline layout")?;
            // Per-slot sets (one per ring slot, written once at `alloc_slot`; freed by
            // `free_slots` on ring rebuilds, hence FREE_DESCRIPTOR_SET). 64 is comfortably
            // above any ring depth (the encoder's POOL is 8).
            let pool_sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(128)];
            self.desc_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(64)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .context("create descriptor pool")?;

            // The shader + one pipeline per MODE (spec constant 0).
            if CURSOR_SPV.len() % 4 != 0 {
                anyhow::bail!("cursor_blend.spv is not word-aligned");
            }
            let words: Vec<u32> = CURSOR_SPV
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            self.shader = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .context("create blend shader module")?;
            for mode in 0u32..3 {
                let entries = [vk::SpecializationMapEntry::default()
                    .constant_id(0)
                    .offset(0)
                    .size(4)];
                let data = mode.to_le_bytes();
                let spec = vk::SpecializationInfo::default()
                    .map_entries(&entries)
                    .data(&data);
                let stage = vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(self.shader)
                    .name(c"main")
                    .specialization_info(&spec);
                let info = [vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(self.pipe_layout)];
                let p = d
                    .create_compute_pipelines(vk::PipelineCache::null(), &info, None)
                    .map_err(|(_, e)| e)
                    .context("create blend pipeline")?[0];
                self.pipelines[mode as usize] = p;
            }
        }
        Ok(())
    }

    fn memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| anyhow!("no memory type for flags {flags:?}"))
    }

    /// Allocate one NVENC input slot as exportable Vulkan memory mapped into CUDA. Layout matches
    /// `InputSurface` (contiguous planes under one pitch); pitch = row bytes rounded to 256.
    pub fn alloc_slot(&mut self, fmt: SlotFormat, width: u32, height: u32) -> Result<VkSlotRef> {
        let pitch = (fmt.row_bytes(width) + 255) & !255;
        let size = pitch * fmt.rows(height);
        // SAFETY: exportable-buffer allocation, the exact `VkBridge::ensure_dst` incantation:
        // `ExternalMemoryBufferCreateInfo`/`ExportMemoryAllocateInfo` declare OPAQUE_FD,
        // `MemoryDedicatedAllocateInfo` ties the memory to the buffer; every info is a local
        // outliving its synchronous call and every failure path destroys the objects created so
        // far exactly once. `get_memory_fd` hands us an fd that `import_owned_fd` either adopts
        // (driver owns it) or closes on failure.
        unsafe {
            let d = &self.device;
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                        .push_next(&mut ext_info),
                    None,
                )
                .context("create slot buffer")?;
            let reqs = d.get_buffer_memory_requirements(buffer);
            let mem_type = match self
                .memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            {
                Ok(t) => t,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate exportable slot memory");
                }
            };
            if let Err(e) = d.bind_buffer_memory(buffer, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_buffer(buffer, None);
                return Err(e).context("bind slot memory");
            }
            let fd = match self.ext_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            ) {
                Ok(f) => f,
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("vkGetMemoryFdKHR(slot)");
                }
            };
            let ext = match cuda::ExternalDmabuf::import_owned_fd(fd, reqs.size) {
                Ok(c) => c,
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("cuImportExternalMemory(slot OPAQUE_FD)");
                }
            };
            // The slot's own descriptor set + command buffer (see `SlotAlloc`). Bindings are
            // written once here: binding 0 is this slot's buffer (immutable for its lifetime),
            // binding 1 the shared cursor staging buffer.
            let dls = [self.desc_layout];
            let desc = match d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.desc_pool)
                    .set_layouts(&dls),
            ) {
                Ok(s) => s[0],
                Err(e) => {
                    drop(ext); // CUDA's view of the memory goes first
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate slot descriptor set");
                }
            };
            let surf_info = [vk::DescriptorBufferInfo::default()
                .buffer(buffer)
                .offset(0)
                .range(size)];
            let cur_info = [vk::DescriptorBufferInfo::default()
                .buffer(self.cur_buf)
                .offset(0)
                .range((CURSOR_MAX * CURSOR_MAX * 4) as u64)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(desc)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&surf_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&cur_info),
            ];
            d.update_descriptor_sets(&writes, &[]);
            let cmd = match d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            ) {
                Ok(c) => c[0],
                Err(e) => {
                    let _ = d.free_descriptor_sets(self.desc_pool, &[desc]);
                    drop(ext); // CUDA's view of the memory goes first
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate slot command buffer");
                }
            };
            let r = VkSlotRef {
                ptr: ext.ptr,
                pitch: pitch as usize,
                height,
                id: self.slots.len(),
            };
            self.slots.push(SlotAlloc {
                buffer,
                memory,
                cuda: ext,
                cmd,
                desc,
            });
            Ok(r)
        }
    }

    /// Free every allocated slot (encoder teardown, alongside its ring clear). CUDA mappings drop
    /// first (field order in [`SlotAlloc`] frees `cuda` via its own `Drop` before we free the VK
    /// objects explicitly here).
    pub fn free_slots(&mut self) {
        // Ordered blends return with their work still on the queue — quiesce before freeing the
        // buffers/sets it references. `device_wait_idle` (not a timeline wait) so a submission
        // whose CUDA copy-done signal never fired is still covered; this tiny device idles in
        // microseconds outside that pathological case. CPU-synced blends need nothing (they
        // fence-wait before returning), so a `None` timeline skips it.
        if self.timeline.is_some() && !self.slots.is_empty() {
            // SAFETY: single-threaded owner; no other thread touches this device or its queue.
            unsafe {
                let _ = self.device.device_wait_idle();
            }
        }
        for s in self.slots.drain(..) {
            drop(s.cuda); // CUDA's view of the memory goes first
                          // SAFETY: `buffer`/`memory`/`cmd`/`desc` were created in `alloc_slot`, are uniquely
                          // owned by the drained `SlotAlloc`, and are destroyed exactly once here. No queue
                          // work is in flight: CPU-synced blends fence-wait before returning, and ordered
                          // blends were quiesced by the `device_wait_idle` above.
            unsafe {
                let _ = self.device.free_descriptor_sets(self.desc_pool, &[s.desc]);
                self.device.free_command_buffers(self.cmd_pool, &[s.cmd]);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
        }
    }

    /// Upload the cursor RGBA (`cw*ch*4`, tight rows) into the mapped staging buffer. Call only
    /// when the bitmap changes; position moves are push constants. Quiesces any in-flight
    /// ordered blend first (bitmap changes are rare — pointer-shape flips — so the occasional
    /// bounded CPU wait here is the trade that keeps the per-frame path sync-free).
    pub fn upload_cursor(&mut self, rgba: &[u8], cw: u32, ch: u32) {
        if let Some(t) = &self.timeline {
            if t.last_blend > 0 {
                let sems = [t.sem];
                let values = [t.last_blend];
                // SAFETY: `t.sem` is the live timeline semaphore; the wait info's arrays are
                // locals outliving the synchronous call. `last_blend` only holds values an
                // accepted submit will signal, so the wait terminates (the timeout is the
                // backstop for a wedged queue — then we proceed and risk one torn cursor
                // bitmap, never UB: the staging buffer stays alive regardless).
                let r = unsafe {
                    t.ts.wait_semaphores(
                        &vk::SemaphoreWaitInfo::default()
                            .semaphores(&sems)
                            .values(&values),
                        1_000_000_000,
                    )
                };
                if let Err(e) = r {
                    tracing::warn!(
                        error = ?e,
                        "cursor upload quiesce failed — proceeding (a torn cursor bitmap for \
                         one frame at worst)"
                    );
                }
            }
        }
        let cw = cw.min(CURSOR_MAX);
        let ch = ch.min(CURSOR_MAX);
        let len = (cw * ch * 4) as usize;
        let len = len.min(rgba.len());
        // SAFETY: `cur_map` is the live persistent mapping of the CURSOR_MAX²·4 host-coherent
        // allocation (created in `init_objects`, unmapped only in `Drop`); `len` is clamped to
        // both the source slice and the buffer capacity. No blend reads race this host write:
        // CPU-synced blends fence-wait before returning, and ordered blends were quiesced via
        // the timeline wait above.
        unsafe {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), self.cur_map, len);
        }
    }

    /// Stream-ordered blends available: the timeline semaphore was exported to CUDA at bring-up,
    /// so [`blend_ref_ordered`](Self::blend_ref_ordered) can order copy→blend→encode on-device.
    pub fn ordered_ready(&self) -> bool {
        self.timeline.is_some()
    }

    /// Last timeline value handed out (0 = no ordered blend submitted yet) — test observability
    /// for "did the ordered path actually run".
    pub fn ordered_ticket(&self) -> u64 {
        self.timeline.as_ref().map_or(0, |t| t.ticket)
    }

    /// Push constants + dispatch geometry for one blend (must match cursor_blend.comp): ARGB =
    /// per cursor px; NV12/YUV444 = per word-aligned 4-px span × (2-row blocks | rows). `None`
    /// when the clamped cursor rect is empty (nothing to dispatch).
    fn blend_geometry(
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Option<(Push, u32, u32)> {
        let cw = cw.min(CURSOR_MAX);
        let ch = ch.min(CURSOR_MAX);
        if cw == 0 || ch == 0 {
            return None;
        }
        let push = Push {
            pitch: slot.pitch as u32,
            surf_w,
            surf_h: slot.height,
            cur_w: cw,
            cur_h: ch,
            ox,
            oy,
        };
        let (gx, gy) = match fmt {
            SlotFormat::Argb => (cw.div_ceil(8), ch.div_ceil(8)),
            _ => {
                let x0 = (ox >> 2) << 2;
                let spans = ((ox + cw as i32) - x0 + 3).div_euclid(4).max(1) as u32;
                let rows = match fmt {
                    SlotFormat::Nv12 => ch.div_ceil(2),
                    _ => ch,
                };
                (spans.div_ceil(8), rows.div_ceil(8))
            }
        };
        Some((push, gx, gy))
    }

    /// Record the blend (barriers + bind + push constants + dispatch) into `id`'s own command
    /// buffer and return it, ready for submission.
    ///
    /// # Safety
    /// The slot's previous submission must have completed: sync blends fence-wait before
    /// returning; ordered blends rely on the encoder's ring-reuse discipline (a slot is reused
    /// only after its encode — GPU-ordered after its blend — was polled).
    unsafe fn record_blend(
        &self,
        id: usize,
        fmt: SlotFormat,
        push: &Push,
        gx: u32,
        gy: u32,
    ) -> Result<vk::CommandBuffer> {
        let alloc = self
            .slots
            .get(id)
            .ok_or_else(|| anyhow!("bad slot id {id}"))?;
        let d = &self.device;
        let cmd = alloc.cmd;
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .context("begin blend cmd")?;
        // CUDA wrote the frame into this memory outside Vulkan's view — make it visible to
        // the shader (external-memory coherence ceremony; NVIDIA honors this with the
        // fence/semaphore ordering alone, the barrier is the spec-shaped belt-and-braces).
        let acquire = [vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)];
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &acquire,
            &[],
            &[],
        );
        d.cmd_bind_pipeline(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pipelines[fmt.mode() as usize],
        );
        d.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pipe_layout,
            0,
            &[alloc.desc],
            &[],
        );
        let bytes = std::slice::from_raw_parts(
            (push as *const Push) as *const u8,
            std::mem::size_of::<Push>(),
        );
        d.cmd_push_constants(
            cmd,
            self.pipe_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            bytes,
        );
        d.cmd_dispatch(cmd, gx.max(1), gy.max(1), 1);
        // Release the shader's writes so the downstream CUDA/NVENC reads (fence- or
        // semaphore-ordered) see them.
        let release = [vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ)];
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &release,
            &[],
            &[],
        );
        d.end_command_buffer(cmd).context("end blend cmd")?;
        Ok(cmd)
    }

    /// Blend the uploaded cursor into `slot` at `(ox, oy)`: record, submit, fence-wait. The
    /// caller has CPU-synced its CUDA frame copy first; the fence wait makes the shader's writes
    /// visible to the subsequent NVENC encode (the `VkBridge` precedent: fence-ordered cross-API
    /// access, no queue-family transfer — NVIDIA-only path).
    #[allow(clippy::too_many_arguments)] // surface geometry + cursor rect — unpacked kernel args
    pub fn blend_ref(
        &mut self,
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        let Some((push, gx, gy)) = Self::blend_geometry(slot, fmt, surf_w, cw, ch, ox, oy) else {
            return Ok(());
        };
        // SAFETY: single-threaded record/submit/wait on handles this struct owns. The slot's
        // previous submission completed (`record_blend`'s contract: this path fence-waits every
        // blend, and an earlier ORDERED blend on this slot finished before the encoder reused it
        // — ring-reuse discipline). Submit-info arrays are locals outliving the synchronous
        // calls. The dispatch's shader accesses stay in-bounds by the shader's own guards
        // (surfW/surfH/curW/curH from `push`) against the slot allocation sized in `alloc_slot`
        // for exactly that geometry.
        unsafe {
            let d = &self.device;
            let cmd = self.record_blend(slot.id, fmt, &push, gx, gy)?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            d.queue_submit(self.queue, &submit, self.fence)
                .context("submit blend")?;
            let r = d.wait_for_fences(&[self.fence], true, 1_000_000_000);
            d.reset_fences(&[self.fence]).ok();
            r.context("blend fence wait")?;
        }
        Ok(())
    }

    /// Stream-ordered blend — no CPU sync anywhere (the fix for cursor frames serializing the
    /// NVENC submit path). The caller has ENQUEUED (not synced) its CUDA frame copy on this
    /// thread's copy stream; this method then
    /// 1. enqueues a CUDA signal of `copy_done` on that stream (fires once the copy completes),
    /// 2. submits the blend waiting `copy_done` and signaling `blend_done` on the Vulkan queue,
    /// 3. enqueues a CUDA wait of `blend_done` — so later stream work (the encode, via the
    ///    session's IO-stream binding) orders after the blend's writes.
    ///
    /// Timeline values are fresh per call and never reused. Failure leaves no dangling waiter:
    /// the CUDA wait is enqueued only after the submit was accepted, and an orphaned CUDA
    /// signal is legal (timeline signals may skip values — a later, larger signal satisfies
    /// any waiter).
    #[allow(clippy::too_many_arguments)] // same unpacked kernel args as `blend_ref`
    pub fn blend_ref_ordered(
        &mut self,
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        let Some((push, gx, gy)) = Self::blend_geometry(slot, fmt, surf_w, cw, ch, ox, oy) else {
            return Ok(());
        };
        let (copy_done, blend_done, sem) = {
            let t = self
                .timeline
                .as_mut()
                .ok_or_else(|| anyhow!("ordered blend without timeline support"))?;
            t.ticket += 2;
            (t.ticket - 1, t.ticket, t.sem)
        };
        // Signal FIRST: if this fails nothing was submitted and the fresh values are simply
        // skipped. (The reverse order could wedge the queue — a submitted blend waiting on a
        // copy-done signal that never got enqueued.)
        self.timeline
            .as_ref()
            .expect("checked above")
            .cuda
            .signal(copy_done)
            .context("cuSignalExternalSemaphoresAsync (copy done)")?;
        // SAFETY: single-threaded record/submit on handles this struct owns. The slot's previous
        // submission completed (`record_blend`'s contract — ring-reuse discipline, see above).
        // All submit-info arrays and the timeline-submit chain are locals outliving the
        // synchronous `queue_submit`. No fence: completion is observed through the timeline
        // (the encode polls it via the CUDA wait below; teardown quiesces via
        // `device_wait_idle`).
        unsafe {
            let cmd = self.record_blend(slot.id, fmt, &push, gx, gy)?;
            let cmds = [cmd];
            let wait_sems = [sem];
            let wait_vals = [copy_done];
            let wait_stages = [vk::PipelineStageFlags::COMPUTE_SHADER];
            let sig_sems = [sem];
            let sig_vals = [blend_done];
            let mut tsi = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_vals)
                .signal_semaphore_values(&sig_vals);
            let submit = [vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .signal_semaphores(&sig_sems)
                .push_next(&mut tsi)];
            self.device
                .queue_submit(self.queue, &submit, vk::Fence::null())
                .context("submit ordered blend")?;
        }
        let t = self.timeline.as_mut().expect("checked above");
        // Only now: `blend_done` WILL be signaled, so quiesces may rely on it.
        t.last_blend = blend_done;
        if let Err(e) = t.cuda.wait(blend_done) {
            // The blend is in flight but the encode would no longer order after it — restore
            // the ordering with a bounded CPU wait (the CPU-synced path's cost, once). The
            // frame still composites correctly.
            let sems = [t.sem];
            let values = [blend_done];
            // SAFETY: live timeline semaphore; wait-info arrays are locals outliving the
            // synchronous call; `blend_done` was accepted for signaling just above, so the
            // wait terminates (timeout backstops a wedged queue).
            let r = unsafe {
                t.ts.wait_semaphores(
                    &vk::SemaphoreWaitInfo::default()
                        .semaphores(&sems)
                        .values(&values),
                    1_000_000_000,
                )
            };
            tracing::warn!(
                error = %format!("{e:#}"),
                cpu_wait = ?r,
                "ordered cursor blend: CUDA-side wait enqueue failed — ordering restored with \
                 a CPU wait this frame"
            );
        }
        Ok(())
    }
}

impl Drop for VkSlotBlend {
    fn drop(&mut self) {
        self.free_slots();
        if let Some(t) = self.timeline.take() {
            drop(t.cuda); // CUDA's import of the semaphore goes first (mirrors the slot order)
                          // SAFETY: `sem` was created in `init_timeline`, is uniquely owned, and is destroyed
                          // exactly once here. No submission still references it: ordered blends only exist
                          // alongside allocated slots, and `free_slots` just quiesced (device_wait_idle) any
                          // in-flight work before this point.
            unsafe {
                self.device.destroy_semaphore(t.sem, None);
            }
        }
        // SAFETY: every handle below was created in `new`/`init_objects` (or is null from a
        // partial init — Vulkan destroy/free calls are defined no-ops on null handles) and is
        // uniquely owned; each is destroyed exactly once here, pipelines/layouts/pools before the
        // device, the device before the instance. No work is in flight (`blend` fence-waits).
        unsafe {
            let d = &self.device;
            for p in self.pipelines {
                if p != vk::Pipeline::null() {
                    d.destroy_pipeline(p, None);
                }
            }
            if self.shader != vk::ShaderModule::null() {
                d.destroy_shader_module(self.shader, None);
            }
            if self.desc_pool != vk::DescriptorPool::null() {
                d.destroy_descriptor_pool(self.desc_pool, None);
            }
            if self.pipe_layout != vk::PipelineLayout::null() {
                d.destroy_pipeline_layout(self.pipe_layout, None);
            }
            if self.desc_layout != vk::DescriptorSetLayout::null() {
                d.destroy_descriptor_set_layout(self.desc_layout, None);
            }
            if !self.cur_map.is_null() {
                d.unmap_memory(self.cur_mem);
            }
            if self.cur_buf != vk::Buffer::null() {
                d.destroy_buffer(self.cur_buf, None);
            }
            if self.cur_mem != vk::DeviceMemory::null() {
                d.free_memory(self.cur_mem, None);
            }
            if self.fence != vk::Fence::null() {
                d.destroy_fence(self.fence, None);
            }
            if self.cmd_pool != vk::CommandPool::null() {
                d.destroy_command_pool(self.cmd_pool, None);
            }
            d.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
