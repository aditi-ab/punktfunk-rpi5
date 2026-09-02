//! VAAPI dmabuf → Vulkan import of per-plane `VkImage`s with the surface's
//! explicit DRM format modifier.
//!
//! Formats: R8/R8G8 for NV12 and full-chroma NV24; R16/R16G16 for P010.
//! Same-Mesa export/import is the contract. A driver rejection is a clean
//! error; the caller demotes to software decode. EGL sibling: `video_gl.rs`.

use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{DmabufFrame, DrmFrameGuard};
use std::os::fd::{AsRawFd as _, BorrowedFd, IntoRawFd as _};

/// fourcc('N','V','1','2').
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
/// fourcc('P','0','1','0'). 10 bits MSB-aligned in 16.
const DRM_FORMAT_P010: u32 = 0x3031_3050;
/// fourcc('N','V','2','4'). CSC keys chroma siting off plane widths, so the
/// full-size chroma plane needs no extra shader. Packed AYUV/Y410 is
/// single-plane and still demotes.
const DRM_FORMAT_NV24: u32 = 0x3432_564e;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// Fallback when the export carried no explicit modifier.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 4] = [
    ash::ext::external_memory_dma_buf::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// Imported frame. GPU reads outlive submit: park until the fence signals,
/// then [`HwFrame::destroy`] (drops the decoder surface guard).
pub struct HwFrame {
    pub luma_view: vk::ImageView,
    pub chroma_view: vk::ImageView,
    pub color: pf_client_core::video::ColorDesc,
    pub width: u32,
    pub height: u32,
    /// Fourcc. CSC picks its P010 vs 8-bit rows off this.
    fourcc: u32,
    images: [vk::Image; 2],
    memories: [vk::DeviceMemory; 2],
    views: [vk::ImageView; 2],
    _guard: DrmFrameGuard,
}

impl HwFrame {
    pub fn is_p010(&self) -> bool {
        self.fourcc == DRM_FORMAT_P010
    }

    /// Plane images for the presenter's foreign-acquire barriers.
    pub fn luma_image(&self) -> vk::Image {
        self.images[0]
    }

    pub fn chroma_image(&self) -> vk::Image {
        self.images[1]
    }

    pub fn destroy(self, device: &ash::Device) {
        // SAFETY: views, images, and memories are owned by `self`. Called only
        // after the frame's fence has signaled, so the GPU is idle on them.
        unsafe {
            for v in self.views {
                device.destroy_image_view(v, None);
            }
            for i in self.images {
                device.destroy_image(i, None);
            }
            for m in self.memories {
                device.free_memory(m, None);
            }
        }
        // `_guard` drops after the GPU reads: the VAAPI surface stays mapped until here.
    }
}

/// Import both planes. Driver rejection is a clean error; the caller demotes.
pub fn import(
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    frame: DmabufFrame,
) -> Result<HwFrame> {
    // Test hook: fault every import so demotion is exercisable without a broken
    // driver. Per-frame lookup is fine — demotion silences it within three frames.
    if std::env::var_os("PUNKTFUNK_HW_FAULT").is_some_and(|v| v == "import") {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    let (luma_fmt, chroma_fmt, chroma_full_res) = match frame.fourcc {
        DRM_FORMAT_NV12 => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM, false),
        DRM_FORMAT_P010 => (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM, false),
        DRM_FORMAT_NV24 => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM, true),
        other => bail!("hw presenter handles NV12/P010/NV24 only (got {other:#x})"),
    };
    if frame.planes.len() < 2 {
        bail!("2-plane YCbCr needs 2 planes (got {})", frame.planes.len());
    }
    // Explicit-modifier images cannot take INVALID; LINEAR is the only honest guess.
    let modifier = if frame.modifier == DRM_FORMAT_MOD_INVALID {
        tracing::trace!("dmabuf carried no explicit modifier — importing as LINEAR");
        DRM_FORMAT_MOD_LINEAR
    } else {
        frame.modifier
    };

    let y = &frame.planes[0];
    let c = &frame.planes[1];
    let (luma_img, luma_mem) = plane_image(
        device,
        ext_mem_fd,
        frame.width,
        frame.height,
        luma_fmt,
        y.fd,
        y.offset,
        y.stride,
        modifier,
    )
    .context("luma plane")?;
    let (cw, ch) = if chroma_full_res {
        (frame.width, frame.height)
    } else {
        (frame.width.div_ceil(2), frame.height.div_ceil(2))
    };
    let (chroma_img, chroma_mem) = match plane_image(
        device, ext_mem_fd, cw, ch, chroma_fmt, c.fd, c.offset, c.stride, modifier,
    )
    .context("chroma plane")
    {
        Ok(r) => r,
        Err(e) => {
            // SAFETY: `luma_img` / `luma_mem` were created in this call and never
            // submitted, so the GPU is idle on them.
            unsafe {
                device.destroy_image(luma_img, None);
                device.free_memory(luma_mem, None);
            }
            return Err(e);
        }
    };

    let view = |image, format| {
        // SAFETY: `image` is owned by this function; the create-info locals
        // outlive the call.
        unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .context("plane image view")
    };
    // SAFETY: luma/chroma image and memory were created in this call and never
    // submitted, so the GPU is idle on them.
    let destroy_images = |views: &[vk::ImageView]| unsafe {
        for v in views {
            device.destroy_image_view(*v, None);
        }
        device.destroy_image(luma_img, None);
        device.destroy_image(chroma_img, None);
        device.free_memory(luma_mem, None);
        device.free_memory(chroma_mem, None);
    };
    let luma_view = match view(luma_img, luma_fmt) {
        Ok(v) => v,
        Err(e) => {
            destroy_images(&[]);
            return Err(e);
        }
    };
    let chroma_view = match view(chroma_img, chroma_fmt) {
        Ok(v) => v,
        Err(e) => {
            destroy_images(&[luma_view]);
            return Err(e);
        }
    };

    Ok(HwFrame {
        luma_view,
        chroma_view,
        color: frame.color,
        width: frame.width,
        height: frame.height,
        fourcc: frame.fourcc,
        images: [luma_img, chroma_img],
        memories: [luma_mem, chroma_mem],
        views: [luma_view, chroma_view],
        _guard: frame.guard,
    })
}

/// One plane as an explicit-modifier image. Vulkan takes the fd it is given,
/// so this dups; the frame guard keeps the original.
#[allow(clippy::too_many_arguments)]
fn plane_image(
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    width: u32,
    height: u32,
    format: vk::Format,
    fd: std::os::fd::RawFd,
    offset: u32,
    stride: u32,
    modifier: u64,
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let plane_layouts = [vk::SubresourceLayout {
        offset: u64::from(offset),
        size: 0, // 0 on import: the driver derives size.
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&plane_layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // SAFETY: create-info and pNext locals (`modifier_info`, `external_info`)
    // outlive the call.
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .push_next(&mut modifier_info)
                .push_next(&mut external_info)
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .with_context(|| {
        format!("create {width}x{height} {format:?} image (modifier {modifier:#018x})")
    })?;

    let result = (|| {
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: `fd` is a live plane fd of the caller's `DmabufFrame`;
        // `fd_props` is a local outliving the call.
        unsafe {
            ext_mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_props,
            )
        }
        .context("vkGetMemoryFdPropertiesKHR")?;
        // SAFETY: `image` was created above and has not been destroyed.
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & fd_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for dmabuf")?;

        // Vulkan owns the fd it imports — dup so the decoder guard keeps the original.
        // SAFETY: `fd` is a plane fd of the caller's `DmabufFrame`. `DrmFrameGuard`
        // keeps those fds open until `import` moves it onto `HwFrame`. The borrow
        // ends at `try_clone_to_owned`.
        let owned = unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .context("dup dmabuf fd")?;
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(owned.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        // SAFETY: `import_info` and `dedicated` are locals that outlive the call.
        // `import_info.fd` is `owned`'s dup, still open.
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .push_next(&mut import_info)
                    .push_next(&mut dedicated)
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("import dmabuf memory")?;
        // Vulkan takes the fd only on a successful import. `into_raw_fd` here;
        // `?` above still closes the dup.
        let _ = owned.into_raw_fd();
        // SAFETY: `image` and `memory` were created above and are still owned here.
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: `memory` was allocated in this call and never bound, so
            // the GPU is idle on it.
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();

    match result {
        Ok(memory) => Ok((image, memory)),
        Err(e) => {
            // SAFETY: `image` was created in this call and never bound, so the
            // GPU is idle on it.
            unsafe { device.destroy_image(image, None) };
            Err(e)
        }
    }
}
