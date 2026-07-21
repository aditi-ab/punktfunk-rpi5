//! Small ash/Vulkan leaf helpers shared by the Linux Vulkan encode backends
//! (`vulkan_video.rs`, `pyrowave.rs`) — extracted verbatim from `vulkan_video.rs`
//! when the PyroWave backend arrived so the two don't fork copies.
// Every unsafe block carries a `// SAFETY:` proof (parent module enforces it).

use anyhow::Result;
use ash::vk;
use pf_frame::PixelFormat;

pub(crate) fn color_range(layer: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: layer,
        layer_count: 1,
    }
}

pub(crate) unsafe fn find_mem(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    for i in 0..mp.memory_type_count {
        if (bits & (1 << i)) != 0 && mp.memory_types[i as usize].property_flags.contains(want) {
            return i;
        }
    }
    0
}

/// DRM fourcc -> the VkFormat whose *color* components match (Vulkan handles the byte swizzle).
pub(crate) fn fourcc_to_vk(fourcc: u32) -> Option<vk::Format> {
    // fourcc_code(a,b,c,d) = a | b<<8 | c<<16 | d<<24
    const XR24: u32 = 0x3432_5258; // XRGB8888
    const AR24: u32 = 0x3432_5241; // ARGB8888
    const XB24: u32 = 0x3432_4258; // XBGR8888
    const AB24: u32 = 0x3432_4241; // ABGR8888
    const NV12: u32 = 0x3231_564e; // DRM_FORMAT_NV12
    match fourcc {
        XR24 | AR24 => Some(vk::Format::B8G8R8A8_UNORM),
        XB24 | AB24 => Some(vk::Format::R8G8B8A8_UNORM),
        NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
        _ => None,
    }
}

pub(crate) fn pixel_to_vk(fmt: PixelFormat) -> Option<vk::Format> {
    match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => Some(vk::Format::B8G8R8A8_UNORM),
        PixelFormat::Rgbx | PixelFormat::Rgba => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

pub(crate) unsafe fn make_view(
    device: &ash::Device,
    image: vk::Image,
    fmt: vk::Format,
    layer: u32,
) -> Result<vk::ImageView> {
    Ok(device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(color_range(layer)),
        None,
    )?)
}

/// Import a packed-RGB dmabuf as a SAMPLED VkImage (explicit DRM modifier). Caller destroys all
/// three returned handles. Extracted verbatim from `vulkan_video.rs`'s import path.
pub(crate) unsafe fn import_rgb_dmabuf(
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    import_rgb_dmabuf_as(
        device,
        ext_fd,
        mem_props,
        d,
        cw,
        ch,
        vk::ImageUsageFlags::SAMPLED,
        None,
    )
}

/// [`import_rgb_dmabuf`] with the image usage explicit and an optional video-profile list.
/// Despite the historical name, this also imports gamescope's one-fd LINEAR NV12: the UV
/// subresource layout comes from the producer's plane-1 chunk when it reported one, falling
/// back to the shared-stride contiguous-plane contract.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn import_rgb_dmabuf_as(
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
    usage: vk::ImageUsageFlags,
    profile_list: Option<&mut vk::VideoProfileListInfoKHR>,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    use anyhow::Context;
    use std::os::fd::IntoRawFd;
    let fmt = fourcc_to_vk(d.fourcc)
        .with_context(|| format!("unsupported dmabuf fourcc {:#x}", d.fourcc))?;
    let planes: Vec<vk::SubresourceLayout> = if fmt == vk::Format::G8_B8R8_2PLANE_420_UNORM {
        let (uv_offset, uv_stride) = d.plane1.map(|(o, s)| (o as u64, s as u64)).unwrap_or((
            d.offset as u64 + d.stride as u64 * ch as u64,
            d.stride as u64,
        ));
        vec![
            vk::SubresourceLayout::default()
                .offset(d.offset as u64)
                .row_pitch(d.stride as u64),
            vk::SubresourceLayout::default()
                .offset(uv_offset)
                .row_pitch(uv_stride),
        ]
    } else {
        vec![vk::SubresourceLayout::default()
            .offset(d.offset as u64)
            .row_pitch(d.stride as u64)]
    };
    let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(d.modifier)
        .plane_layouts(&planes);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(fmt)
        .extent(vk::Extent3D {
            width: cw,
            height: ch,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext)
        .push_next(&mut drm);
    if let Some(pl) = profile_list {
        ci = ci.push_next(pl);
    }
    let img = device.create_image(&ci, None)?;
    // dup the fd; Vulkan takes ownership of the dup on a successful import.
    let dup = d.fd.try_clone().context("dup dmabuf fd")?.into_raw_fd();
    let fd_props = {
        let mut p = vk::MemoryFdPropertiesKHR::default();
        let _ = (ext_fd.fp().get_memory_fd_properties_khr)(
            device.handle(),
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            dup,
            &mut p,
        );
        p.memory_type_bits
    };
    let req = device.get_image_memory_requirements(img);
    let bits = req.memory_type_bits & fd_props;
    let ti = find_mem(
        mem_props,
        if bits != 0 {
            bits
        } else {
            req.memory_type_bits
        },
        vk::MemoryPropertyFlags::empty(),
    );
    let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(img);
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(dup);
    let mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ti)
            .push_next(&mut ded)
            .push_next(&mut import),
        None,
    )?;
    device.bind_image_memory(img, mem, 0)?;
    let view = device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(color_range(0)),
        None,
    )?;
    Ok((img, mem, view))
}

pub(crate) unsafe fn make_plain_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let img = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(fmt)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
    )?;
    let req = device.get_image_memory_requirements(img);
    // Unwind on failure: callers (the encoders' open paths) only ever see the completed triple.
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )),
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_image(img, None);
            return Err(e.into());
        }
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        device.destroy_image(img, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    match make_view(device, img, fmt, 0) {
        Ok(view) => Ok((img, mem, view)),
        Err(e) => {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
            Err(e)
        }
    }
}
