//! Video-image / staging-buffer (re)build + retired-frame destruction.

use super::gpu::subresource_range;
use super::{Presenter, Retired, Staging, VideoImage};
use anyhow::Result;
use ash::vk;
use pf_client_core::video::CpuFrame;

impl Retired {
    pub(super) fn destroy(self, device: &ash::Device) {
        match self {
            #[cfg(target_os = "linux")]
            Retired::Dmabuf(f) => f.destroy(device),
            #[cfg(windows)]
            Retired::D3d11(f) => f.destroy(device),
            Retired::Vk { frame, views } => {
                // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned
                // by this type and live for the call, and every builder struct is a local that
                // outlives it.
                unsafe {
                    for v in views {
                        device.destroy_image_view(v, None);
                    }
                }
                drop(frame); // guard drops here — AVFrame (and the VkImage) released
            }
        }
    }
}

impl Presenter {
    /// Copy the frame's RGBA into the staging buffer and (re)build the video image on a
    /// stream-size change. Rows keep their stride — `buffer_row_length` unpacks it.
    pub(super) fn stage_frame(&mut self, f: &CpuFrame) -> Result<()> {
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
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe { std::ptr::copy_nonoverlapping(f.rgba.as_ptr(), s.ptr, n) };
        Ok(())
    }

    pub(super) fn rebuild_video_image(&mut self, width: u32, height: u32) -> Result<()> {
        // Fence-quiesce: the old image is only ever referenced by OUR command buffers.
        self.quiesce_own()?;
        if let Some(v) = self.video.take() {
            // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by
            // this type and live for the call, and every builder struct is a local that outlives
            // it.
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
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let image = unsafe {
            self.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(self.video_format)
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
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let memory = self.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe { self.device.bind_image_memory(image, memory, 0) }?;
        // The CSC pass renders into it — view + framebuffer, unconditional (Vulkan-Video
        // frames need the pass on every device, dmabuf-capable or not).
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let view = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(self.video_format)
                    .subresource_range(subresource_range()),
                None,
            )
        }?;
        let attachments = [view];
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let framebuffer = unsafe {
            self.device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(self.csc.render_pass)
                    .attachments(&attachments)
                    .width(width)
                    .height(height)
                    .layers(1),
                None,
            )
        }?;
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
        self.quiesce_own()?;
        if let Some(s) = self.staging.take() {
            // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and
            // the GPU is known idle for them (the fence/queue-wait on the path here, or the
            // swapchain being retired), which is the obligation that makes a destroy sound rather
            // than the handle merely being non-null.
            unsafe {
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
        }
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let buffer = unsafe {
            self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(capacity as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let memory = self.allocate(
            reqs,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
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
}
