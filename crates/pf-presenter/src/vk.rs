//! The transfer-only Vulkan presenter: swapchain + staging upload + letterboxed blit.
//!
//! Phase 1 deliberately has NO graphics pipeline: software-decoded RGBA uploads to a
//! device-local image (`copy_buffer_to_image`, the row stride handled by
//! `buffer_row_length`), and each present clears the swapchain image and
//! `vkCmdBlitImage`s the video into the Contain-fit letterbox rect (linear filter,
//! format conversion by the blit). No render pass, no framebuffers, no image views, no
//! SPIR-V toolchain — all of that arrives with the Phase 2 dmabuf/CSC pass, which is the
//! first thing that actually needs a shader.
//!
//! Pacing: one frame in flight (the submit fence is waited before each record), FIFO by
//! default (`PUNKTFUNK_PRESENT_MODE=mailbox|immediate` if available). Present is
//! arrival-paced by the caller: `present(Some(frame))` on each decoded frame,
//! `present(None)` re-blits the retained video image (expose/resize redraws).

use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::CpuFrame;
use std::ffi::CString;

/// The one video image (device-local RGBA the size of the decoded stream) + its staging.
struct VideoImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    capacity: usize,
}

pub struct Presenter {
    // Field order = drop order documentation only; teardown is explicit in `Drop`.
    entry: ash::Entry,
    instance: ash::Instance,
    surface_i: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    device: ash::Device,
    swap_d: ash::khr::swapchain::Device,
    queue: vk::Queue,
    format: vk::SurfaceFormatKHR,
    present_mode: vk::PresentModeKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    /// Per-swapchain-image render-finished semaphores (present consumes them on the
    /// image's schedule — one shared semaphore could be re-submitted while a previous
    /// present still holds it).
    render_sems: Vec<vk::Semaphore>,
    acquire_sem: vk::Semaphore,
    fence: vk::Fence,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    staging: Option<Staging>,
    video: Option<VideoImage>,
    /// The submit fence has a submission pending (wait before recording again — also
    /// what makes the single staging buffer safe to overwrite).
    submitted: bool,
}

impl Presenter {
    /// Bring up instance → surface → device → swapchain over an SDL window.
    /// `instance_extensions` comes from `VideoSubsystem::vulkan_instance_extensions()`.
    pub fn new(window: &sdl3::video::Window, instance_extensions: &[String]) -> Result<Presenter> {
        let entry = unsafe { ash::Entry::load() }.context("libvulkan not loadable")?;

        let app_name = CString::new("punktfunk-session").unwrap();
        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .api_version(vk::API_VERSION_1_2);
        let ext_cstrings: Vec<CString> = instance_extensions
            .iter()
            .map(|e| CString::new(e.as_str()).unwrap())
            .collect();
        let ext_ptrs: Vec<*const i8> = ext_cstrings.iter().map(|e| e.as_ptr()).collect();
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&ext_ptrs),
                None,
            )
        }
        .context("vkCreateInstance")?;
        let surface_i = ash::khr::surface::Instance::new(&entry, &instance);

        let surface = unsafe { window.vulkan_create_surface(instance.handle()) }
            .map_err(|e| anyhow!("SDL_Vulkan_CreateSurface: {e}"))?;

        let (pdev, qfi) = pick_device(&instance, &surface_i, surface)?;
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        {
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let name = props
                .device_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            tracing::info!(device = %name, queue_family = qfi, "vulkan device");
        }

        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&[1.0])];
        // Phase 2 (dmabuf import) adds VK_EXT_external_memory_dma_buf +
        // VK_KHR_external_memory_fd + VK_EXT_image_drm_format_modifier here.
        let dev_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device = unsafe {
            instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_exts),
                None,
            )
        }
        .context("vkCreateDevice")?;
        let swap_d = ash::khr::swapchain::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(qfi, 0) };

        let format = pick_format(&surface_i, pdev, surface)?;
        let present_mode = pick_present_mode(&surface_i, pdev, surface)?;
        tracing::info!(?format, ?present_mode, "swapchain config");

        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(qfi),
                None,
            )
        }?;
        let cmd_buf = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        let acquire_sem =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        let fence = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
        }?;

        let mut p = Presenter {
            entry,
            instance,
            surface_i,
            surface,
            pdev,
            mem_props,
            device,
            swap_d,
            queue,
            format,
            present_mode,
            swapchain: vk::SwapchainKHR::null(),
            images: Vec::new(),
            extent: vk::Extent2D::default(),
            render_sems: Vec::new(),
            acquire_sem,
            fence,
            cmd_pool,
            cmd_buf,
            staging: None,
            video: None,
            submitted: false,
        };
        p.recreate_swapchain(window)?;
        Ok(p)
    }

    /// (Re)build the swapchain for the window's current pixel size. Also the resize path.
    pub fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        unsafe { self.device.device_wait_idle() }.ok();
        self.submitted = false;

        let caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        let (pw, ph) = window.size_in_pixels();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: pw.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: ph.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            // Minimized — keep the old swapchain; presents will report OUT_OF_DATE and
            // land back here once the window has a size again.
            return Ok(());
        }
        let mut min_images = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            min_images = min_images.min(caps.max_image_count);
        }

        let old = self.swapchain;
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(min_images)
            .image_format(self.format.format)
            .image_color_space(self.format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // TRANSFER_DST is the whole phase-1 pipeline (clear + blit); COLOR_ATTACHMENT
            // keeps the phase-2 render pass from forcing a swapchain rebuild contract change.
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            .clipped(true)
            .old_swapchain(old);
        let swapchain = unsafe { self.swap_d.create_swapchain(&info, None) }
            .context("vkCreateSwapchainKHR")?;
        if old != vk::SwapchainKHR::null() {
            unsafe { self.swap_d.destroy_swapchain(old, None) };
        }
        self.swapchain = swapchain;
        self.images = unsafe { self.swap_d.get_swapchain_images(swapchain) }?;
        self.extent = extent;

        for s in self.render_sems.drain(..) {
            unsafe { self.device.destroy_semaphore(s, None) };
        }
        for _ in 0..self.images.len() {
            self.render_sems.push(unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }
        tracing::debug!(
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            "swapchain (re)created"
        );
        Ok(())
    }

    /// Present one frame: upload `frame` if given (else re-blit the retained video
    /// image), clear, letterbox-blit, present. Returns false when the swapchain was out
    /// of date — the caller recreates (with current window state) and may retry.
    pub fn present(&mut self, window: &sdl3::video::Window, frame: Option<&CpuFrame>) -> Result<bool> {
        if self.extent.width == 0 || self.extent.height == 0 {
            return Ok(true); // minimized — nothing to do
        }
        // One frame in flight: the fence covers the command buffer AND the staging
        // buffer, so waiting here makes both safe to reuse.
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
            self.device.reset_fences(&[self.fence])?;
        }

        if let Some(f) = frame {
            self.stage_frame(f)?;
        }

        let (index, _suboptimal) = match unsafe {
            self.swap_d.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.acquire_sem,
                vk::Fence::null(),
            )
        } {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain(window)?;
                return Ok(false);
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        };
        let swap_image = self.images[index as usize];

        unsafe {
            self.device.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // New frame: staging → video image (stride carried by buffer_row_length).
            if let (Some(f), Some(v), Some(s)) = (frame, &self.video, &self.staging) {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                );
                let region = vk::BufferImageCopy::default()
                    .buffer_row_length((f.stride / 4) as u32)
                    .image_subresource(subresource_layers())
                    .image_extent(vk::Extent3D {
                        width: v.width,
                        height: v.height,
                        depth: 1,
                    });
                self.device.cmd_copy_buffer_to_image(
                    self.cmd_buf,
                    s.buffer,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                );
            }

            // Swapchain image: discard old content, clear to black (the letterbox bars),
            // blit the video in, hand to present.
            barrier(
                &self.device,
                self.cmd_buf,
                swap_image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            self.device.cmd_clear_color_image(
                self.cmd_buf,
                swap_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
                &[subresource_range()],
            );
            if let Some(v) = &self.video {
                let (dst0, dst1) = letterbox(self.extent, v.width, v.height);
                let blit = vk::ImageBlit::default()
                    .src_subresource(subresource_layers())
                    .src_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: v.width as i32,
                            y: v.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(subresource_layers())
                    .dst_offsets([dst0, dst1]);
                self.device.cmd_blit_image(
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            }
            barrier(
                &self.device,
                self.cmd_buf,
                swap_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR,
            );
            self.device.end_command_buffer(self.cmd_buf)?;

            let render_sem = self.render_sems[index as usize];
            let wait_sems = [self.acquire_sem];
            let wait_stages = [vk::PipelineStageFlags::TRANSFER];
            let cmd_bufs = [self.cmd_buf];
            let signal_sems = [render_sem];
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait_sems)
                    .wait_dst_stage_mask(&wait_stages)
                    .command_buffers(&cmd_bufs)
                    .signal_semaphores(&signal_sems)],
                self.fence,
            )?;
            self.submitted = true;

            let swapchains = [self.swapchain];
            let indices = [index];
            let present_sems = [render_sem];
            match self.swap_d.queue_present(
                self.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&present_sems)
                    .swapchains(&swapchains)
                    .image_indices(&indices),
            ) {
                Ok(_) => Ok(true),
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate_swapchain(window)?;
                    Ok(false)
                }
                Err(e) => Err(e).context("vkQueuePresentKHR"),
            }
        }
    }

    /// Copy the frame's RGBA into the staging buffer and (re)build the video image on a
    /// stream-size change. Rows keep their stride — `buffer_row_length` unpacks it.
    fn stage_frame(&mut self, f: &CpuFrame) -> Result<()> {
        anyhow::ensure!(
            f.stride % 4 == 0 && f.stride >= f.width as usize * 4,
            "unexpected RGBA stride {} for width {}",
            f.stride,
            f.width
        );
        if self
            .video
            .as_ref()
            .is_none_or(|v| v.width != f.width || v.height != f.height)
        {
            self.rebuild_video_image(f.width, f.height)?;
            tracing::info!(width = f.width, height = f.height, "video image (re)built");
        }
        let needed = f.stride * f.height as usize;
        if self.staging.as_ref().is_none_or(|s| s.capacity < needed) {
            self.rebuild_staging(needed)?;
        }
        let s = self.staging.as_ref().unwrap();
        let n = f.rgba.len().min(needed);
        unsafe { std::ptr::copy_nonoverlapping(f.rgba.as_ptr(), s.ptr, n) };
        Ok(())
    }

    fn rebuild_video_image(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe { self.device.device_wait_idle() }.ok();
        self.submitted = false;
        if let Some(v) = self.video.take() {
            unsafe {
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
        }
        let image = unsafe {
            self.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let memory = self.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        unsafe { self.device.bind_image_memory(image, memory, 0) }?;
        self.video = Some(VideoImage {
            image,
            memory,
            width,
            height,
        });
        Ok(())
    }

    fn rebuild_staging(&mut self, capacity: usize) -> Result<()> {
        unsafe { self.device.device_wait_idle() }.ok();
        self.submitted = false;
        if let Some(s) = self.staging.take() {
            unsafe {
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
        }
        let buffer = unsafe {
            self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(capacity as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }?;
        let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let memory = self.allocate(
            reqs,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }?;
        let ptr = unsafe {
            self.device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
        }? as *mut u8;
        self.staging = Some(Staging {
            buffer,
            memory,
            ptr,
            capacity,
        });
        Ok(())
    }

    fn allocate(
        &self,
        reqs: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory> {
        let type_index = (0..self.mem_props.memory_type_count)
            .find(|&i| {
                reqs.memory_type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .with_context(|| format!("no memory type for {flags:?}"))?;
        unsafe {
            self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("vkAllocateMemory")
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().ok();
            if let Some(s) = self.staging.take() {
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            if let Some(v) = self.video.take() {
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_semaphore(self.acquire_sem, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(self.swapchain, None);
            }
            self.device.destroy_device(None);
            self.surface_i.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
        // `entry` (the libvulkan handle) drops last, after every vk call is done.
        let _ = &self.entry;
    }
}

/// First physical device with a queue family that does graphics + present here;
/// `PUNKTFUNK_VK_DEVICE=<index>` overrides on multi-GPU boxes.
fn pick_device(
    instance: &ash::Instance,
    surface_i: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    let forced: Option<usize> = std::env::var("PUNKTFUNK_VK_DEVICE")
        .ok()
        .and_then(|v| v.parse().ok());
    let candidates: Vec<vk::PhysicalDevice> = match forced {
        Some(i) => devices.get(i).copied().into_iter().collect(),
        None => devices,
    };
    for pdev in candidates {
        let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
        for (i, f) in families.iter().enumerate() {
            let graphics = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present = unsafe {
                surface_i.get_physical_device_surface_support(pdev, i as u32, surface)
            }
            .unwrap_or(false);
            if graphics && present {
                return Ok((pdev, i as u32));
            }
        }
    }
    bail!("no Vulkan device with a graphics+present queue family")
}

/// Prefer BGRA8 UNORM (the near-universal presentable format); RGBA8 second; else
/// whatever the surface offers first. UNORM (not SRGB) — the decoded RGBA is already
/// display-referred, the blit must not re-encode it.
fn pick_format(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> Result<vk::SurfaceFormatKHR> {
    let formats = unsafe { surface_i.get_physical_device_surface_formats(pdev, surface) }?;
    for want in [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM] {
        if let Some(f) = formats.iter().find(|f| f.format == want) {
            return Ok(*f);
        }
    }
    formats
        .first()
        .copied()
        .ok_or_else(|| anyhow!("surface offers no formats"))
}

/// FIFO unless overridden (`PUNKTFUNK_PRESENT_MODE=mailbox|immediate`) and available —
/// a streaming client defaults to tear-free.
fn pick_present_mode(
    surface_i: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> Result<vk::PresentModeKHR> {
    let modes = unsafe { surface_i.get_physical_device_surface_present_modes(pdev, surface) }?;
    let want = match std::env::var("PUNKTFUNK_PRESENT_MODE").ok().as_deref() {
        Some("mailbox") => vk::PresentModeKHR::MAILBOX,
        Some("immediate") => vk::PresentModeKHR::IMMEDIATE,
        _ => vk::PresentModeKHR::FIFO,
    };
    Ok(if modes.contains(&want) {
        want
    } else {
        vk::PresentModeKHR::FIFO // always available per spec
    })
}

/// The Contain-fit letterbox: video (vw×vh) into the swapchain extent, centered.
fn letterbox(extent: vk::Extent2D, vw: u32, vh: u32) -> (vk::Offset3D, vk::Offset3D) {
    let (ew, eh) = (f64::from(extent.width), f64::from(extent.height));
    let scale = (ew / f64::from(vw.max(1))).min(eh / f64::from(vh.max(1)));
    let dw = (f64::from(vw) * scale).round();
    let dh = (f64::from(vh) * scale).round();
    let ox = ((ew - dw) / 2.0).floor() as i32;
    let oy = ((eh - dh) / 2.0).floor() as i32;
    (
        vk::Offset3D { x: ox, y: oy, z: 0 },
        vk::Offset3D {
            x: (ox + dw as i32).min(extent.width as i32),
            y: (oy + dh as i32).min(extent.height as i32),
            z: 1,
        },
    )
}

fn subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

fn subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// A full-subresource layout transition with the conservative ALL_COMMANDS/TRANSFER
/// scopes this transfer-only pipeline needs (per-frame granularity, not per-stage).
fn barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_pillarboxes_a_wide_window() {
        // 16:10 video in a 21:9-ish window: full height, centered horizontally.
        let (a, b) = letterbox(
            vk::Extent2D {
                width: 3440,
                height: 1440,
            },
            1280,
            800,
        );
        assert_eq!((a.y, b.y), (0, 1440));
        assert_eq!(b.x - a.x, 2304); // 1280 * (1440/800)
        assert_eq!(a.x, (3440 - 2304) / 2);
    }

    #[test]
    fn letterbox_matches_exact_fit() {
        let (a, b) = letterbox(
            vk::Extent2D {
                width: 1280,
                height: 800,
            },
            1280,
            800,
        );
        assert_eq!((a.x, a.y), (0, 0));
        assert_eq!((b.x, b.y), (1280, 800));
    }
}
