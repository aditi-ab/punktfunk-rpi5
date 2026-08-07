//! Video-image / staging-buffer (re)build + retired-frame destruction.

use super::gpu::subresource_range;
use super::{CpuPlanes, Presenter, Retired, Staging, VideoImage};
use anyhow::Result;
use ash::vk;
use pf_client_core::video::CpuPlanarFrame;

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
            // The image and plane views belong to the DECODER's pools — nothing of ours
            // to destroy. The drop sends the release token (the caller reaches here only
            // after the sampling fence, so the token honestly means "GPU reads done").
            Retired::NativeVk(frame) => drop(frame),
        }
    }
}

/// Staging offset of plane `i` for a picture of this size, plus the total bytes needed.
///
/// Each plane starts on a 16-byte boundary so `bufferOffset` satisfies the copy's
/// "multiple of 4" rule whatever the picture dimensions are — with a 1-byte-per-texel
/// format an odd width would otherwise land a later plane on an odd offset.
fn plane_staging_offsets(f: &CpuPlanarFrame) -> ([usize; 3], usize) {
    let mut offsets = [0usize; 3];
    let mut at = 0usize;
    for (i, off) in offsets.iter_mut().enumerate() {
        let (w, h) = f.plane_dims(i);
        *off = at;
        at += (w as usize * h as usize).next_multiple_of(16);
    }
    (offsets, at)
}

impl CpuPlanes {
    /// Destroy every handle this value holds. Null handles are fine — Vulkan defines
    /// destroy/free on `VK_NULL_HANDLE` as a no-op — which is what lets
    /// [`Presenter::rebuild_cpu_planes`] unwind a build that failed part-way.
    pub(super) fn destroy(self, device: &ash::Device) {
        // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and the
        // GPU is known idle for them (the fence/queue-wait on the path here, or the swapchain
        // being retired), which is the obligation that makes a destroy sound rather than the
        // handle merely being non-null.
        unsafe {
            for i in 0..3 {
                device.destroy_image_view(self.views[i], None);
                device.destroy_image(self.images[i], None);
                device.free_memory(self.memory[i], None);
            }
        }
    }
}

impl Presenter {
    /// Copy the frame's three tightly-packed planes into the staging buffer and (re)build
    /// the plane images + video image on a stream-size change.
    ///
    /// Returns the per-plane staging offsets the record step copies from. Nothing here
    /// touches the queue: a rebuild that fails must fail BEFORE the acquire, the same
    /// rule the hardware imports follow.
    pub(super) fn stage_frame(&mut self, f: &CpuPlanarFrame) -> Result<[usize; 3]> {
        if self
            .video
            .as_ref()
            .is_none_or(|v| v.width != f.width || v.height != f.height)
        {
            self.rebuild_video_image(f.width, f.height)?;
            tracing::info!(width = f.width, height = f.height, "video image (re)built");
        }
        if self
            .cpu_planes
            .as_ref()
            .is_none_or(|p| p.width != f.width || p.height != f.height)
        {
            self.rebuild_cpu_planes(f.width, f.height)?;
        }
        let (offsets, needed) = plane_staging_offsets(f);
        if self.staging.as_ref().is_none_or(|s| s.capacity < needed) {
            self.rebuild_staging(needed)?;
        }
        let s = self.staging.as_ref().unwrap();
        for (i, off) in offsets.iter().enumerate() {
            let plane = f.plane(i);
            // SAFETY: per the Vulkan contract above - `s.ptr` maps a HOST_VISIBLE allocation of
            // `s.capacity >= needed` bytes and `plane_staging_offsets` placed `off + plane.len()`
            // inside `needed`; source and destination are distinct allocations.
            unsafe { std::ptr::copy_nonoverlapping(plane.as_ptr(), s.ptr.add(*off), plane.len()) };
        }
        Ok(offsets)
    }

    /// (Re)build the software rung's three R8 plane images for a luma size.
    fn rebuild_cpu_planes(&mut self, width: u32, height: u32) -> Result<()> {
        // Fence-quiesce: the old images are only ever referenced by OUR command buffers.
        self.quiesce_own()?;
        if let Some(p) = self.cpu_planes.take() {
            p.destroy(&self.device);
        }
        let (cw, ch) = CpuPlanarFrame::chroma_dims(width, height);
        let dims = [(width, height), (cw, ch), (cw, ch)];
        // Built INTO the owning value, not into loose arrays: nine fallible steps (three
        // images, three allocations, three views) used to `?` straight out and leak
        // everything created before the one that failed — up to ~12 MB per size change at
        // 4K, on the rung the client reaches because something already went wrong.
        // `destroy` tolerates the nulls a partial build leaves (Vulkan defines
        // destroy/free on `VK_NULL_HANDLE` as a no-op), so one call unwinds any prefix.
        let mut planes = CpuPlanes {
            images: [vk::Image::null(); 3],
            memory: [vk::DeviceMemory::null(); 3],
            views: [vk::ImageView::null(); 3],
            width,
            height,
            initialized: false,
        };
        for (i, dim) in dims.into_iter().enumerate() {
            if let Err(e) = self.build_cpu_plane(&mut planes, i, dim) {
                planes.destroy(&self.device);
                return Err(e);
            }
        }
        tracing::info!(width, height, "software plane images (re)built");
        self.cpu_planes = Some(planes);
        Ok(())
    }

    /// One R8 plane of [`CpuPlanes`], written into `planes` as each handle is created so
    /// a failure part-way leaves the caller something it can destroy.
    fn build_cpu_plane(&self, planes: &mut CpuPlanes, i: usize, (w, h): (u32, u32)) -> Result<()> {
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by
        // this type and live for the call, and every builder struct is a local that outlives
        // it.
        let image = unsafe {
            self.device.create_image(
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
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }?;
        planes.images[i] = image;
        // SAFETY: per the Vulkan contract above - a read-only query on the live device.
        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        planes.memory[i] = self.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by
        // this type and live for the call.
        unsafe { self.device.bind_image_memory(image, planes.memory[i], 0) }?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by
        // this type and live for the call, and every builder struct is a local that outlives
        // it.
        planes.views[i] = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
                    .subresource_range(subresource_range()),
                None,
            )
        }?;
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
