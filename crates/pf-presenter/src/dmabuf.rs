//! VAAPI dmabuf → Vulkan import: per-plane `VkImage`s (R8 luma + GR88 chroma) with the
//! surface's explicit DRM format modifier — the same layer-wise import the EGL presenter
//! (`video_gl.rs`) proved on this hardware, minus the toolkit. Same-Mesa export/import
//! is the contract; anything a driver rejects surfaces as a clean error and the caller
//! demotes the decoder to software (never a black screen).

use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{DmabufFrame, DrmFrameGuard};
use std::os::fd::{BorrowedFd, IntoRawFd as _};

/// `fourcc('N','V','1','2')` — the only VAAPI decoder output today (8-bit 4:2:0).
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// `DRM_FORMAT_MOD_LINEAR` — the fallback when the export carried no explicit modifier.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The four device extensions the import path needs; queried at device creation. All
/// Mesa drivers (RADV/ANV/radeonsi boxes) expose the set — NVIDIA proprietary has no
/// usable VAAPI anyway, so the software path owns that vendor by design.
pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 4] = [
    ash::ext::external_memory_dma_buf::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// One imported frame: both plane images + their memory, and the decoder surface guard.
/// GPU reads outlive the submit — the presenter parks this until the frame's fence has
/// signaled, then calls [`HwFrame::destroy`] (which finally drops the guard).
pub struct HwFrame {
    pub luma_view: vk::ImageView,
    pub chroma_view: vk::ImageView,
    pub color: pf_client_core::video::ColorDesc,
    pub width: u32,
    pub height: u32,
    images: [vk::Image; 2],
    memories: [vk::DeviceMemory; 2],
    views: [vk::ImageView; 2],
    _guard: DrmFrameGuard,
}

impl HwFrame {
    /// The raw plane images — the presenter's foreign-acquire barriers need them.
    pub fn luma_image(&self) -> vk::Image {
        self.images[0]
    }

    pub fn chroma_image(&self) -> vk::Image {
        self.images[1]
    }

    pub fn destroy(self, device: &ash::Device) {
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
        // _guard (the mapped AVFrame / VAAPI surface) drops here — after every GPU read.
    }
}

/// Import one frame's two planes. Fails cleanly (caller demotes) on anything the driver
/// rejects: unknown fourcc, unsupported modifier, import refusal.
pub fn import(
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    frame: DmabufFrame,
) -> Result<HwFrame> {
    // The demotion test hook (plan §8, Phase 2 acceptance): fault every import so the
    // failure-streak → force_software → software-decode recovery is exercisable on any
    // box, no broken driver required. Read per hw frame — demotion silences it within
    // three frames, so the env lookup never runs hot.
    if std::env::var_os("PUNKTFUNK_HW_FAULT").is_some_and(|v| v == "import") {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    if frame.fourcc != DRM_FORMAT_NV12 {
        bail!("hw presenter handles NV12 only (got {:#x})", frame.fourcc);
    }
    if frame.planes.len() < 2 {
        bail!("NV12 needs 2 planes (got {})", frame.planes.len());
    }
    // EGL could leave an INVALID modifier to the driver's implied choice; explicit-
    // modifier images can't — LINEAR is the only honest guess (debug-visible if wrong).
    let modifier = if frame.modifier == DRM_FORMAT_MOD_INVALID {
        tracing::debug!("dmabuf carried no explicit modifier — importing as LINEAR");
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
        vk::Format::R8_UNORM,
        y.fd,
        y.offset,
        y.stride,
        modifier,
    )
    .context("luma plane")?;
    let (chroma_img, chroma_mem) = match plane_image(
        device,
        ext_mem_fd,
        frame.width.div_ceil(2),
        frame.height.div_ceil(2),
        vk::Format::R8G8_UNORM,
        c.fd,
        c.offset,
        c.stride,
        modifier,
    )
    .context("chroma plane")
    {
        Ok(r) => r,
        Err(e) => {
            unsafe {
                device.destroy_image(luma_img, None);
                device.free_memory(luma_mem, None);
            }
            return Err(e);
        }
    };

    let view = |image, format| {
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
    let destroy_images = |views: &[vk::ImageView]| unsafe {
        for v in views {
            device.destroy_image_view(*v, None);
        }
        device.destroy_image(luma_img, None);
        device.destroy_image(chroma_img, None);
        device.free_memory(luma_mem, None);
        device.free_memory(chroma_mem, None);
    };
    let luma_view = match view(luma_img, vk::Format::R8_UNORM) {
        Ok(v) => v,
        Err(e) => {
            destroy_images(&[]);
            return Err(e);
        }
    };
    let chroma_view = match view(chroma_img, vk::Format::R8G8_UNORM) {
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
        images: [luma_img, chroma_img],
        memories: [luma_mem, chroma_mem],
        views: [luma_view, chroma_view],
        _guard: frame.guard,
    })
}

/// One single-plane image over a dmabuf plane: explicit-modifier tiling with the plane's
/// (offset, pitch), external-memory dmabuf handle type, dedicated import of a dup'd fd
/// (Vulkan takes ownership of the fd it's given; the frame guard keeps owning the
/// original).
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
        size: 0, // must be 0 for imports (the driver derives it)
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&plane_layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
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
        // The fd's importable memory types, intersected with the image's requirement.
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            ext_mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_props,
            )
        }
        .context("vkGetMemoryFdPropertiesKHR")?;
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & fd_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for dmabuf")?;

        // Vulkan owns the fd it imports — dup so the decoder guard keeps the original.
        let owned = unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .context("dup dmabuf fd")?;
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(owned.into_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
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
        // (On allocate_memory failure Vulkan still closed the dup'd fd — nothing leaks.)
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();

    match result {
        Ok(memory) => Ok((image, memory)),
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            Err(e)
        }
    }
}
