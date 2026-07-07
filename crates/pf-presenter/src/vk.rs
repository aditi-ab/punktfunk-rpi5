//! The Vulkan presenter: swapchain + two frame paths into one device-local RGBA video
//! image, then a letterboxed `vkCmdBlitImage` composite.
//!
//! * **Software** (`FrameInput::Cpu`): staging upload + `copy_buffer_to_image` (row
//!   stride via `buffer_row_length`) — transfer-only, runs on every GPU.
//! * **Hardware** (`FrameInput::Dmabuf`): the decoder's NV12 dmabuf imported per-plane
//!   (`dmabuf.rs`) and converted by the CSC render pass (`csc.rs`) — zero-copy, gated on
//!   the four import extensions at device creation; boxes without them (NVIDIA
//!   proprietary by design) report `supports_dmabuf() == false` and the caller keeps the
//!   decoder on software.
//!
//! Pacing: one frame in flight (the submit fence is waited before each record), FIFO by
//! default (`PUNKTFUNK_PRESENT_MODE=mailbox|immediate` if available). Present is
//! arrival-paced by the caller: a frame input on each decoded frame,
//! `FrameInput::Redraw` re-blits the retained video image (expose/resize redraws).

use crate::csc::{build_fullscreen_pipeline, csc_rows, CscPass};
use crate::dmabuf::{self, HwFrame};
use crate::overlay::{OverlayFrame, SharedDevice};
use anyhow::{anyhow, bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{CpuFrame, DmabufFrame};
use std::ffi::CString;

/// One presenter iteration's video input.
pub enum FrameInput<'a> {
    /// No new frame — re-composite the retained video image (expose/resize).
    Redraw,
    Cpu(&'a CpuFrame),
    Dmabuf(DmabufFrame),
}

/// The dmabuf/CSC machinery, present only when the device carries the import extensions.
struct HwCtx {
    ext_mem_fd: ash::khr::external_memory_fd::Device,
    csc: CscPass,
}

/// The overlay composite: one premultiplied-alpha quad blended over the swapchain image
/// after the video blit (the §6.1 contract's presenter half). Always built — it has no
/// Skia dependency and costs nothing while no overlay frame arrives (the render pass
/// isn't even recorded).
struct OverlayPipe {
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    /// Per-swapchain-image render targets, rebuilt with the swapchain.
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
}

impl OverlayPipe {
    fn new(device: &ash::Device, format: vk::Format) -> Result<OverlayPipe> {
        // LOAD the blitted video, blend the overlay, end PRESENT-ready — this pass owns
        // the swapchain image's final transition on overlay frames.
        let attachment = [vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_ref)];
        let deps = [vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            )];
        let render_pass = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpass)
                    .dependencies(&deps),
                None,
            )
        }
        .context("overlay render pass")?;

        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }?;
        let samplers = [sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let set_layouts = [set_layout];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )
        }?;
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let desc_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&set_layouts),
            )
        }?[0];
        let pipeline = build_fullscreen_pipeline(
            device,
            render_pass,
            pipeline_layout,
            include_bytes!("../shaders/overlay.frag.spv"),
            true, // premultiplied blend over the video
        )?;
        Ok(OverlayPipe {
            render_pass,
            set_layout,
            pipeline_layout,
            pipeline,
            desc_pool,
            desc_set,
            sampler,
            views: Vec::new(),
            framebuffers: Vec::new(),
        })
    }

    /// Rebuild the per-swapchain-image views + framebuffers (swapchain recreation).
    fn rebuild_targets(
        &mut self,
        device: &ash::Device,
        images: &[vk::Image],
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Result<()> {
        self.destroy_targets(device);
        for &image in images {
            let view = unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(subresource_range()),
                    None,
                )
            }?;
            self.views.push(view);
            let attachments = [view];
            let fb = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }?;
            self.framebuffers.push(fb);
        }
        Ok(())
    }

    fn destroy_targets(&mut self, device: &ash::Device) {
        unsafe {
            for fb in self.framebuffers.drain(..) {
                device.destroy_framebuffer(fb, None);
            }
            for v in self.views.drain(..) {
                device.destroy_image_view(v, None);
            }
        }
    }

    fn destroy(&mut self, device: &ash::Device) {
        self.destroy_targets(device);
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.desc_pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// The one video image (device-local RGBA the size of the decoded stream) + its staging.
/// `view`/`framebuffer` exist only on hw-capable devices (the CSC pass renders into it).
struct VideoImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
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
    qfi: u32,
    /// Dmabuf import + CSC — `None` when the device lacks the import extensions.
    hw: Option<HwCtx>,
    /// The console-UI composite quad (§6.1's presenter half).
    overlay_pipe: OverlayPipe,
    /// The submitted hw frame (plane images + decoder-surface guard): its GPU reads end
    /// with the in-flight fence, so it's destroyed right after the next fence wait.
    retired_hw: Option<HwFrame>,
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
        // The dmabuf import set is optional: enabled when the device offers all four,
        // else the presenter is software-only (`supports_dmabuf() == false`).
        let available = unsafe { instance.enumerate_device_extension_properties(pdev) }?;
        let has = |name: &std::ffi::CStr| {
            available
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };
        let hw_capable = dmabuf::DEVICE_EXTENSIONS.iter().all(|n| has(n));
        let mut dev_exts = vec![ash::khr::swapchain::NAME.as_ptr()];
        if hw_capable {
            dev_exts.extend(dmabuf::DEVICE_EXTENSIONS.iter().map(|n| n.as_ptr()));
        } else {
            tracing::info!(
                "device lacks the dmabuf import extensions — hardware frames unavailable \
                 (software decode only)"
            );
        }
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
        let hw = if hw_capable {
            Some(HwCtx {
                ext_mem_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                csc: CscPass::new(&device)?,
            })
        } else {
            None
        };

        let format = pick_format(&surface_i, pdev, surface)?;
        let present_mode = pick_present_mode(&surface_i, pdev, surface)?;
        tracing::info!(?format, ?present_mode, "swapchain config");
        let overlay_pipe = OverlayPipe::new(&device, format.format)?;

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
            qfi,
            hw,
            overlay_pipe,
            retired_hw: None,
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
        self.overlay_pipe
            .rebuild_targets(&self.device, &self.images, self.format.format, extent)?;

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

    /// Whether the hardware (dmabuf) path exists on this device — callers keep the
    /// decoder on software when it doesn't.
    pub fn supports_dmabuf(&self) -> bool {
        self.hw.is_some()
    }

    /// Quiesce the queue — the run loop calls this before dropping the overlay so
    /// nothing in flight still references its images.
    pub fn wait_idle(&self) {
        unsafe { self.device.device_wait_idle() }.ok();
    }

    /// The device handles the console-UI overlay renders on (§6.1). Valid for the
    /// presenter's lifetime; the run loop drops the overlay first.
    pub fn shared_device(&self) -> SharedDevice {
        SharedDevice {
            entry: self.entry.clone(),
            instance: self.instance.clone(),
            physical_device: self.pdev,
            device: self.device.clone(),
            queue: self.queue,
            queue_family_index: self.qfi,
        }
    }

    /// Present one frame: route `input` into the video image (staging upload or dmabuf
    /// import + CSC pass; `Redraw` re-blits what's retained), clear, letterbox-blit,
    /// blend the console-UI `overlay` quad if one arrived, present. Returns false when
    /// the swapchain was out of date — the caller recreates (with current window state)
    /// and may retry.
    pub fn present(
        &mut self,
        window: &sdl3::video::Window,
        input: FrameInput,
        overlay: Option<&OverlayFrame>,
    ) -> Result<bool> {
        if self.extent.width == 0 || self.extent.height == 0 {
            return Ok(true); // minimized — nothing to do
        }
        // A dmabuf frame imports before anything touches the queue: an import the driver
        // rejects must fail out here, before this present consumed the acquire semaphore.
        let mut hw_frame: Option<HwFrame> = None;
        let cpu_frame = match input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some(f),
            FrameInput::Dmabuf(d) => {
                let hw = self
                    .hw
                    .as_ref()
                    .context("hardware frame without dmabuf support")?;
                hw_frame = Some(dmabuf::import(&self.device, &hw.ext_mem_fd, d)?);
                None
            }
        };

        // One frame in flight: the fence covers the command buffer, the staging buffer
        // AND the previously submitted hw frame — waiting makes all three reusable.
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
            self.device.reset_fences(&[self.fence])?;
        }
        if let Some(old) = self.retired_hw.take() {
            old.destroy(&self.device);
        }

        if let Some(f) = cpu_frame {
            self.stage_frame(f)?;
        }
        if let Some(f) = &hw_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            // Safe while nothing in flight references the set — the fence wait above.
            let hw = self.hw.as_ref().unwrap();
            hw.csc.bind_planes(&self.device, f.luma_view, f.chroma_view);
        }
        if let Some(o) = overlay {
            // Point the composite at this overlay image (same fence-wait safety).
            let infos = [vk::DescriptorImageInfo::default()
                .image_view(o.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let writes = [vk::WriteDescriptorSet::default()
                .dst_set(self.overlay_pipe.desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos)];
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
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
                // Never submitted — the import (if any) dies here, GPU never saw it.
                if let Some(f) = hw_frame {
                    f.destroy(&self.device);
                }
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

            // Hardware frame: acquire the foreign planes, then the CSC pass renders
            // NV12→RGBA into the video image (render pass ends it in TRANSFER_SRC for
            // the blit below).
            if let (Some(f), Some(hw), Some(v)) = (&hw_frame, &self.hw, &self.video) {
                for view_image in [f.luma_image(), f.chroma_image()] {
                    foreign_acquire_barrier(&self.device, self.cmd_buf, view_image, self.qfi);
                }
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                self.device.cmd_begin_render_pass(
                    self.cmd_buf,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(hw.csc.render_pass)
                        .framebuffer(v.framebuffer)
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent,
                        }),
                    vk::SubpassContents::INLINE,
                );
                self.device.cmd_bind_pipeline(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    hw.csc.pipeline,
                );
                self.device.cmd_set_viewport(
                    self.cmd_buf,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: extent.width as f32,
                        height: extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                self.device.cmd_set_scissor(
                    self.cmd_buf,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    }],
                );
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    hw.csc.pipeline_layout,
                    0,
                    &[hw.csc.desc_set],
                    &[],
                );
                let rows = csc_rows(f.color);
                let bytes = std::slice::from_raw_parts(rows.as_ptr().cast::<u8>(), 48);
                self.device.cmd_push_constants(
                    self.cmd_buf,
                    hw.csc.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytes,
                );
                self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(self.cmd_buf);
            }

            // New frame: staging → video image (stride carried by buffer_row_length).
            if let (Some(f), Some(v), Some(s)) = (cpu_frame, &self.video, &self.staging) {
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
            if let Some(o) = overlay {
                // Cross-submit visibility for the overlay image (Skia flushed it on this
                // queue): same-layout barrier = execution + memory dependency only.
                barrier(
                    &self.device,
                    self.cmd_buf,
                    o.image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
                // The composite pass blends the quad and ends the image PRESENT-ready.
                self.device.cmd_begin_render_pass(
                    self.cmd_buf,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(self.overlay_pipe.render_pass)
                        .framebuffer(self.overlay_pipe.framebuffers[index as usize])
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: self.extent,
                        }),
                    vk::SubpassContents::INLINE,
                );
                self.device.cmd_bind_pipeline(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline,
                );
                self.device.cmd_set_viewport(
                    self.cmd_buf,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: self.extent.width as f32,
                        height: self.extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                self.device.cmd_set_scissor(
                    self.cmd_buf,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: self.extent,
                    }],
                );
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline_layout,
                    0,
                    &[self.overlay_pipe.desc_set],
                    &[],
                );
                self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(self.cmd_buf);
            } else {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                );
            }
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
            // The hw frame is on the GPU now — park it until the fence proves the reads
            // done (destroyed at the next present's fence wait, or in Drop).
            self.retired_hw = hw_frame.take();

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
                if v.framebuffer != vk::Framebuffer::null() {
                    self.device.destroy_framebuffer(v.framebuffer, None);
                }
                if v.view != vk::ImageView::null() {
                    self.device.destroy_image_view(v.view, None);
                }
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
        }
        // COLOR_ATTACHMENT is the CSC pass's render target; harmless where hw is absent.
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
                    .usage(
                        vk::ImageUsageFlags::TRANSFER_DST
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::COLOR_ATTACHMENT,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let memory = self.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        unsafe { self.device.bind_image_memory(image, memory, 0) }?;
        // The CSC pass renders into it — view + framebuffer, hw-capable devices only.
        let (view, framebuffer) = if let Some(hw) = &self.hw {
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(subresource_range()),
                    None,
                )
            }?;
            let attachments = [view];
            let framebuffer = unsafe {
                self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(hw.csc.render_pass)
                        .attachments(&attachments)
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )
            }?;
            (view, framebuffer)
        } else {
            (vk::ImageView::null(), vk::Framebuffer::null())
        };
        self.video = Some(VideoImage {
            image,
            memory,
            view,
            framebuffer,
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
            if let Some(f) = self.retired_hw.take() {
                f.destroy(&self.device); // idle above — the GPU reads are done
            }
            if let Some(s) = self.staging.take() {
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            if let Some(v) = self.video.take() {
                if v.framebuffer != vk::Framebuffer::null() {
                    self.device.destroy_framebuffer(v.framebuffer, None);
                }
                if v.view != vk::ImageView::null() {
                    self.device.destroy_image_view(v.view, None);
                }
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
            if let Some(hw) = self.hw.take() {
                hw.csc.destroy(&self.device);
            }
            self.overlay_pipe.destroy(&self.device);
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

/// Acquire a dmabuf plane image from its foreign owner (the VAAPI decoder): queue-family
/// transfer FOREIGN → ours, UNDEFINED → SHADER_READ_ONLY (content is preserved across
/// the transfer regardless of the UNDEFINED old-layout, per the external-memory rules).
fn foreign_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        .dst_queue_family_index(qfi)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
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
