//! S1 spike: can this NVIDIA driver import a block-linear (tiled) dmabuf into Vulkan as a
//! sampled image, and can one allocation be both a dmabuf and CUDA-importable? Runs by hand
//! on the RTX box; the verdict decides P2's primary path (`design/producer-native-capture.md`
//! §5). Everything here is test-only.

use anyhow::{bail, Context, Result};
use ash::vk;
use std::ffi::CStr;
use std::os::fd::OwnedFd;

/// The findings, printed whole so the verdict reads without the log.
#[derive(Debug, Default)]
struct Verdict {
    device: String,
    /// `(modifier, sampled, storage)` for B8G8R8A8_UNORM.
    modifiers: Vec<(u64, bool, bool)>,
    tiled_modifier: Option<u64>,
    /// The exported image reports this modifier; the import must use it.
    exported_modifier: Option<u64>,
    imported_as_sampled: bool,
    readback_matches: bool,
    /// A `DMA_BUF` buffer allocation lists `OPAQUE_FD` as compatible (CUDA can map it too).
    buffer_dmabuf_opaque_compatible: bool,
    /// Same question for a tiled image allocation.
    image_dmabuf_opaque_compatible: bool,
}

const W: u32 = 256;
const H: u32 = 128;

fn pattern(x: u32, y: u32) -> [u8; 4] {
    [
        (x & 0xff) as u8,
        (y & 0xff) as u8,
        ((x ^ y) & 0xff) as u8,
        0xff,
    ]
}

fn find_type(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..mp.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0 && mp.memory_types[i as usize].property_flags.contains(want)
        })
        .context("no memory type")
}

#[test]
#[ignore = "requires an NVIDIA GPU + driver — run on the RTX box (.21)"]
fn nvidia_block_linear_dmabuf_imports_into_vulkan() {
    // SAFETY: a hand-run probe over raw Vulkan handles it creates and destroys itself.
    let v = unsafe { run() }.expect("spike");
    println!("S1 verdict: {v:#?}");
    assert!(
        v.imported_as_sampled && v.readback_matches,
        "the tiled dmabuf did not round-trip through a Vulkan import"
    );
}

unsafe fn run() -> Result<Verdict> {
    // SAFETY: hand-run probe; every handle below is created here and destroyed before return.
    unsafe {
        let mut v = Verdict::default();
        let entry = ash::Entry::load().context("load libvulkan")?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        )?;
        let phys = instance
            .enumerate_physical_devices()?
            .into_iter()
            .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
            .context("no NVIDIA device")?;
        let props = instance.get_physical_device_properties(phys);
        v.device = CStr::from_ptr(props.device_name.as_ptr())
            .to_string_lossy()
            .into_owned();
        let mem_props = instance.get_physical_device_memory_properties(phys);
        let qf = instance
            .get_physical_device_queue_family_properties(phys)
            .iter()
            .position(|q| {
                q.queue_flags
                    .contains(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
            })
            .context("compute+transfer queue")? as u32;
        let exts = [
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ash::khr::image_format_list::NAME.as_ptr(),
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
            .context("vkCreateDevice with VK_EXT_image_drm_format_modifier")?;
        let queue = device.get_device_queue(qf, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let ext_mod = ash::ext::image_drm_format_modifier::Device::new(&instance, &device);
        let fmt = vk::Format::B8G8R8A8_UNORM;

        // 1. The driver's modifiers for the capture format.
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(phys, fmt, &mut fp2);
        let n = list.drm_format_modifier_count as usize;
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut mods);
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(phys, fmt, &mut fp2);
        for m in &mods {
            let f = m.drm_format_modifier_tiling_features;
            v.modifiers.push((
                m.drm_format_modifier,
                f.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE),
                f.contains(vk::FormatFeatureFlags::STORAGE_IMAGE),
            ));
        }
        let tiled = mods
            .iter()
            .find(|m| {
                m.drm_format_modifier != 0
                    && m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features.contains(
                        vk::FormatFeatureFlags::SAMPLED_IMAGE
                            | vk::FormatFeatureFlags::TRANSFER_SRC
                            | vk::FormatFeatureFlags::TRANSFER_DST,
                    )
            })
            .map(|m| m.drm_format_modifier);
        let Some(tiled) = tiled else {
            println!("S1 verdict: {v:#?}");
            bail!("no tiled modifier with SAMPLED|TRANSFER on this driver");
        };
        v.tiled_modifier = Some(tiled);

        // 2. Compatibility of one allocation as dmabuf + opaque fd (a host-owned buffer both a
        // compositor and CUDA could use).
        let ebi = vk::PhysicalDeviceExternalBufferInfo::default()
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut ebp = vk::ExternalBufferProperties::default();
        instance.get_physical_device_external_buffer_properties(phys, &ebi, &mut ebp);
        v.buffer_dmabuf_opaque_compatible = ebp
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut mod_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(tiled)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let ifi = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(fmt)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut eifp = vk::ExternalImageFormatProperties::default();
        let mut ifp = vk::ImageFormatProperties2::default().push_next(&mut eifp);
        instance
            .get_physical_device_image_format_properties2(phys, &ifi, &mut ifp)
            .context("tiled SAMPLED dmabuf image format query")?;
        v.image_dmabuf_opaque_compatible = eifp
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);

        // 3. The stand-in for a compositor buffer: a tiled image exported as a dmabuf, filled
        // with a known pattern.
        let mods_arr = [tiled];
        let mut mod_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods_arr);
        let mut ext_img = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let extent = vk::Extent3D {
            width: W,
            height: H,
            depth: 1,
        };
        let img_a = device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(fmt)
                .extent(extent)
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut mod_list)
                .push_next(&mut ext_img),
            None,
        )?;
        let req = device.get_image_memory_requirements(img_a);
        let mut ded_a = vk::MemoryDedicatedAllocateInfo::default().image(img_a);
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mem_a = device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(find_type(
                    &mem_props,
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )?)
                .push_next(&mut ded_a)
                .push_next(&mut export),
            None,
        )?;
        device.bind_image_memory(img_a, mem_a, 0)?;
        let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
        ext_mod.get_image_drm_format_modifier_properties(img_a, &mut mp)?;
        v.exported_modifier = Some(mp.drm_format_modifier);
        let layout = device.get_image_subresource_layout(
            img_a,
            vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        );

        // Staging upload of the pattern.
        let bytes: Vec<u8> = (0..H)
            .flat_map(|y| (0..W).flat_map(move |x| pattern(x, y)))
            .collect();
        let (stage, stage_mem) = host_buffer(&device, &mem_props, bytes.len() as u64)?;
        let p = device.map_memory(
            stage_mem,
            0,
            bytes.len() as u64,
            vk::MemoryMapFlags::empty(),
        )?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast(), bytes.len());
        device.unmap_memory(stage_mem);
        let pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default().queue_family_index(qf),
            None,
        )?;
        let cmd = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .command_buffer_count(1),
        )?[0];
        device.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(img_a)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(range)],
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(extent);
        device.cmd_copy_buffer_to_image(
            cmd,
            stage,
            img_a,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[region],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(img_a)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .subresource_range(range)],
        );
        device.end_command_buffer(cmd)?;
        submit_wait(&device, queue, cmd)?;

        // 4. Export the dmabuf and import it as a second, sampled image — the P2 path.
        let fd = ext_fd.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default()
                .memory(mem_a)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )?;
        let planes = [vk::SubresourceLayout {
            offset: layout.offset,
            size: 0,
            row_pitch: layout.row_pitch,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(mp.drm_format_modifier)
            .plane_layouts(&planes);
        let mut ext_img_b = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let img_b = device
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(extent)
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                    .initial_layout(vk::ImageLayout::PREINITIALIZED)
                    .push_next(&mut explicit)
                    .push_next(&mut ext_img_b),
                None,
            )
            .context("import image: tiled dmabuf as SAMPLED")?;
        v.imported_as_sampled = true;
        let mut fdprops = vk::MemoryFdPropertiesKHR::default();
        ext_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd,
            &mut fdprops,
        )?;
        let req_b = device.get_image_memory_requirements(img_b);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd);
        let mut ded_b = vk::MemoryDedicatedAllocateInfo::default().image(img_b);
        let mem_b = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req_b.size)
                    .memory_type_index(find_type(
                        &mem_props,
                        req_b.memory_type_bits & fdprops.memory_type_bits,
                        vk::MemoryPropertyFlags::empty(),
                    )?)
                    .push_next(&mut import)
                    .push_next(&mut ded_b),
                None,
            )
            .context("import memory: dmabuf fd")?;
        device.bind_image_memory(img_b, mem_b, 0)?;

        // 5. Read the import back through a transfer: the driver resolves the tiling.
        let (out, out_mem) = host_buffer(&device, &mem_props, bytes.len() as u64)?;
        device.reset_command_pool(pool, vk::CommandPoolResetFlags::empty())?;
        device.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .image(img_b)
                .old_layout(vk::ImageLayout::PREINITIALIZED)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::HOST_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .subresource_range(range)],
        );
        device.cmd_copy_image_to_buffer(
            cmd,
            img_b,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            out,
            &[region],
        );
        device.end_command_buffer(cmd)?;
        submit_wait(&device, queue, cmd)?;
        let p = device.map_memory(out_mem, 0, bytes.len() as u64, vk::MemoryMapFlags::empty())?;
        let got = std::slice::from_raw_parts(p.cast::<u8>(), bytes.len()).to_vec();
        device.unmap_memory(out_mem);
        let mismatches = got.iter().zip(&bytes).filter(|(a, b)| a != b).count();
        v.readback_matches = mismatches == 0;
        if !v.readback_matches {
            println!("readback: {mismatches} of {} bytes differ", bytes.len());
        }

        // Teardown: images before their memory; the second image owns the fd through its import.
        let _ = device.device_wait_idle();
        device.destroy_buffer(out, None);
        device.free_memory(out_mem, None);
        device.destroy_buffer(stage, None);
        device.free_memory(stage_mem, None);
        device.destroy_command_pool(pool, None);
        device.destroy_image(img_b, None);
        device.free_memory(mem_b, None);
        device.destroy_image(img_a, None);
        device.free_memory(mem_a, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
        Ok(v)
    }
}

unsafe fn host_buffer(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    // SAFETY: hand-run probe; every handle below is created here and destroyed before return.
    unsafe {
        let buf = device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST),
            None,
        )?;
        let req = device.get_buffer_memory_requirements(buf);
        let mem = device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(find_type(
                    mp,
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )?),
            None,
        )?;
        device.bind_buffer_memory(buf, mem, 0)?;
        Ok((buf, mem))
    }
}

unsafe fn submit_wait(
    device: &ash::Device,
    queue: vk::Queue,
    cmd: vk::CommandBuffer,
) -> Result<()> {
    // SAFETY: hand-run probe; every handle below is created here and destroyed before return.
    unsafe {
        let cmds = [cmd];
        device.queue_submit(
            queue,
            &[vk::SubmitInfo::default().command_buffers(&cmds)],
            vk::Fence::null(),
        )?;
        device.queue_wait_idle(queue)?;
        Ok(())
    }
}

/// A tiled dmabuf standing in for a compositor buffer: `width`×`height` BGRx holding
/// [`pattern`], exported from its own Vulkan device (the spike above proved the recipe). The
/// fd is a dup the caller may hand to an import; the image lives until this drops.
pub(crate) struct TiledPattern {
    pub fd: OwnedFd,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl TiledPattern {
    /// The reference colour at `(x, y)` as RGB, the way a sampler returns it.
    pub(crate) fn rgb(x: u32, y: u32) -> [u8; 3] {
        let p = pattern(x, y);
        [p[2], p[1], p[0]]
    }

    pub(crate) fn new(width: u32, height: u32) -> Result<TiledPattern> {
        // SAFETY: test fixture over raw Vulkan handles it creates and destroys itself.
        unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )?;
            let phys = instance
                .enumerate_physical_devices()?
                .into_iter()
                .find(|&p| instance.get_physical_device_properties(p).vendor_id == 0x10DE)
                .context("no NVIDIA device")?;
            let mem_props = instance.get_physical_device_memory_properties(phys);
            let qf = instance
                .get_physical_device_queue_family_properties(phys)
                .iter()
                .position(|q| {
                    q.queue_flags
                        .contains(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
                })
                .context("compute+transfer queue")? as u32;
            let exts = [
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                ash::ext::image_drm_format_modifier::NAME.as_ptr(),
                ash::khr::image_format_list::NAME.as_ptr(),
            ];
            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qf)
                .queue_priorities(&prio)];
            let device = instance.create_device(
                phys,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qci)
                    .enabled_extension_names(&exts),
                None,
            )?;
            let queue = device.get_device_queue(qf, 0);
            let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
            let ext_mod = ash::ext::image_drm_format_modifier::Device::new(&instance, &device);
            let fmt = vk::Format::B8G8R8A8_UNORM;
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(phys, fmt, &mut fp2);
            let n = list.drm_format_modifier_count as usize;
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut mods);
            let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(phys, fmt, &mut fp2);
            let tiled = mods
                .iter()
                .find(|m| {
                    m.drm_format_modifier != 0
                        && m.drm_format_modifier_plane_count == 1
                        && m.drm_format_modifier_tiling_features.contains(
                            vk::FormatFeatureFlags::SAMPLED_IMAGE
                                | vk::FormatFeatureFlags::TRANSFER_DST,
                        )
                })
                .map(|m| m.drm_format_modifier)
                .context("no tiled modifier")?;
            let mods_arr = [tiled];
            let mut mod_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&mods_arr);
            let mut ext_img = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let extent = vk::Extent3D {
                width,
                height,
                depth: 1,
            };
            let image = device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(extent)
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut mod_list)
                    .push_next(&mut ext_img),
                None,
            )?;
            let req = device.get_image_memory_requirements(image);
            let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(find_type(
                        &mem_props,
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )?)
                    .push_next(&mut ded)
                    .push_next(&mut export),
                None,
            )?;
            device.bind_image_memory(image, memory, 0)?;
            let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
            ext_mod.get_image_drm_format_modifier_properties(image, &mut mp)?;
            let layout = device.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            );
            let bytes: Vec<u8> = (0..height)
                .flat_map(|y| (0..width).flat_map(move |x| pattern(x, y)))
                .collect();
            let (stage, stage_mem) = host_buffer(&device, &mem_props, bytes.len() as u64)?;
            let ptr = device.map_memory(
                stage_mem,
                0,
                bytes.len() as u64,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
            device.unmap_memory(stage_mem);
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(qf),
                None,
            )?;
            let cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )?[0];
            device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(image)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .subresource_range(range)],
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                stage,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(extent)],
            );
            // Release to the external owner: what a compositor's export looks like to an importer.
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier::default()
                    .image(image)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .src_queue_family_index(qf)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .subresource_range(range)],
            );
            device.end_command_buffer(cmd)?;
            submit_wait(&device, queue, cmd)?;
            device.destroy_command_pool(pool, None);
            device.destroy_buffer(stage, None);
            device.free_memory(stage_mem, None);
            let raw = ext_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )?;
            Ok(TiledPattern {
                fd: <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw),
                modifier: mp.drm_format_modifier,
                offset: layout.offset as u32,
                stride: layout.row_pitch as u32,
                _entry: entry,
                instance,
                device,
                image,
                memory,
            })
        }
    }
}

impl Drop for TiledPattern {
    fn drop(&mut self) {
        // SAFETY: the handles are this fixture's own; the fd is closed by `OwnedFd`.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
