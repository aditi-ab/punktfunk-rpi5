//! Shared ash/Vulkan leaf helpers for the Linux encode backends
//! (`vulkan_video.rs`, `pyrowave.rs`).
// `unsafe_op_in_unsafe_fn` off HERE. Wrapping each ash call would add a
// SAFETY comment that only restates the signature. Exit: delete unmarked
// calls; do not wrap them.
#![allow(unsafe_op_in_unsafe_fn)]

use anyhow::Result;
use ash::vk;
use pf_frame::PixelFormat;

pub(super) fn ext_advertised(exts: &[vk::ExtensionProperties], name: &std::ffi::CStr) -> bool {
    // Bounded: a missing NUL is `Err` (non-match), not a walk past the array.
    exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

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

/// DRM fourcc → VkFormat whose *color* components match; Vulkan does the byte swizzle.
pub(crate) fn fourcc_to_vk(fourcc: u32) -> Option<vk::Format> {
    // fourcc_code(a,b,c,d) = a | b<<8 | c<<16 | d<<24
    const XR24: u32 = 0x3432_5258; // XRGB8888
    const AR24: u32 = 0x3432_5241; // ARGB8888
    const XB24: u32 = 0x3432_4258; // XBGR8888
    const AB24: u32 = 0x3432_4241; // ABGR8888
    const NV12: u32 = 0x3231_564e; // DRM_FORMAT_NV12
                                   // DRM word layout == Vulkan PACK32 (not a byte swizzle). A2R10G10B10 is
                                   // optional; a reject means drop XR30 from the capture offer, not convert here.
    const XR30: u32 = 0x3033_5258; // DRM_FORMAT_XRGB2101010
    const XB30: u32 = 0x3033_4258; // DRM_FORMAT_XBGR2101010
    match fourcc {
        XR24 | AR24 => Some(vk::Format::B8G8R8A8_UNORM),
        XB24 | AB24 => Some(vk::Format::R8G8B8A8_UNORM),
        XR30 => Some(vk::Format::A2R10G10B10_UNORM_PACK32),
        XB30 => Some(vk::Format::A2B10G10R10_UNORM_PACK32),
        NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
        _ => None,
    }
}

pub(crate) fn pixel_to_vk(fmt: PixelFormat) -> Option<vk::Format> {
    match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => Some(vk::Format::B8G8R8A8_UNORM),
        PixelFormat::Rgbx | PixelFormat::Rgba => Some(vk::Format::R8G8B8A8_UNORM),
        // Sampling yields PQ in [0,1], which `rgb2yuv10.comp` wants.
        PixelFormat::X2Rgb10 => Some(vk::Format::A2R10G10B10_UNORM_PACK32),
        PixelFormat::X2Bgr10 => Some(vk::Format::A2B10G10R10_UNORM_PACK32),
        _ => None,
    }
}

/// Expand packed 24-bpp CPU RGB into `scratch` (caller-owned, reused) as 4-bpp
/// with pad 0xFF: no 24-bpp VkFormat is reliably sampleable.
///
/// `bgra_target = false` keeps channel order (CSC views match). `true` forces
/// B,G,R,X because VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207 requires the
/// encode source to match the session `pictureFormat` (`B8G8R8A8_UNORM`).
///
/// Payloads are tightly packed (`FramePayload::Cpu`); a truncated source
/// yields a truncated output — upload paths bound-check the bytes.
pub(crate) fn normalize_cpu_rgb<'a>(
    fmt: PixelFormat,
    bytes: &'a [u8],
    scratch: &'a mut Vec<u8>,
    bgra_target: bool,
) -> (PixelFormat, &'a [u8]) {
    let (bpp, r, g, b) = match fmt {
        PixelFormat::Rgb => (3usize, 0usize, 1usize, 2usize),
        PixelFormat::Bgr => (3, 2, 1, 0),
        PixelFormat::Rgbx | PixelFormat::Rgba => (4, 0, 1, 2),
        PixelFormat::Bgrx | PixelFormat::Bgra => (4, 2, 1, 0),
        _ => return (fmt, bytes),
    };
    if bpp == 4 && (!bgra_target || b == 0) {
        return (fmt, bytes); // 4-bpp already in session order: borrow
    }
    let px = bytes.len() / bpp;
    scratch.clear();
    scratch.resize(px * 4, 0xFF);
    let (dr, dg, db) = if bgra_target { (2, 1, 0) } else { (r, g, b) };
    for (dst, src) in scratch.chunks_exact_mut(4).zip(bytes.chunks_exact(bpp)) {
        dst[dr] = src[r];
        dst[dg] = src[g];
        dst[db] = src[b];
    }
    let out_fmt = if bgra_target || b == 0 {
        PixelFormat::Bgrx
    } else {
        PixelFormat::Rgbx
    };
    (out_fmt, scratch.as_slice())
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

/// False for `ERROR_OUT_OF_{DEVICE,HOST}_MEMORY`: three tight OOMs must not
/// permanently latch a working host onto CPU capture. Deterministic refusals
/// (unsupported fourcc, driver reject) do count — they repeat forever.
pub(crate) fn import_failure_feeds_latch(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<vk::Result>() {
        Some(&r) => {
            r != vk::Result::ERROR_OUT_OF_DEVICE_MEMORY && r != vk::Result::ERROR_OUT_OF_HOST_MEMORY
        }
        None => true,
    }
}

/// Caller destroys all three returned handles.
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

/// Also imports one-fd LINEAR NV12: UV layout from plane-1, else shared-stride
/// contiguous planes.
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
    use std::os::fd::{AsRawFd, IntoRawFd};
    let fmt = fourcc_to_vk(d.fourcc)
        .with_context(|| format!("unsupported dmabuf fourcc {:#x}", d.fourcc))?;
    // Dup first, keep owned: Vulkan takes the fd only on successful
    // `allocate_memory`; `vkFreeMemory` then closes it. Close-after-success
    // is a double-close of a recycled number.
    let dup = d.fd.try_clone().context("dup dmabuf fd")?;
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
    // Destroy only what this call created; the caller's `DmabufFrame` fd stays theirs.
    let fd_props = {
        let mut p = vk::MemoryFdPropertiesKHR::default();
        // Borrow-only; error leaves `memory_type_bits = 0` and the fallback uses image reqs.
        let _ = (ext_fd.fp().get_memory_fd_properties_khr)(
            device.handle(),
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            dup.as_raw_fd(),
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
        .fd(dup.as_raw_fd());
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ti)
            .push_next(&mut ded)
            .push_next(&mut import),
        None,
    ) {
        Ok(mem) => {
            // Success transferred fd ownership to the memory object — release, don't close.
            let _ = dup.into_raw_fd();
            mem
        }
        Err(e) => {
            device.destroy_image(img, None);
            return Err(e.into()); // `dup` drops: the one close of the failed import
        }
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        device.destroy_image(img, None);
        device.free_memory(mem, None); // closes the imported fd
        return Err(e.into());
    }
    let view = match device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(color_range(0)),
        None,
    ) {
        Ok(v) => v,
        Err(e) => {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
            return Err(e.into());
        }
    };
    Ok((img, mem, view))
}

/// On failure every handle this call created is destroyed, so callers can `?`.
pub(crate) unsafe fn make_host_buffer(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let buf = device.create_buffer(
        &vk::BufferCreateInfo::default().size(size).usage(usage),
        None,
    )?;
    let req = device.get_buffer_memory_requirements(buf);
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_buffer(buf, None);
            return Err(e.into());
        }
    };
    if let Err(e) = device.bind_buffer_memory(buf, mem, 0) {
        device.destroy_buffer(buf, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    Ok((buf, mem))
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
    // Unwind: callers only ever see the completed triple.
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

#[cfg(test)]
mod tests {
    #[test]
    fn ext_advertised_matches_exact_name() {
        let mut e = ash::vk::ExtensionProperties::default();
        let name = b"VK_EXT_queue_family_foreign\0";
        for (i, b) in name.iter().enumerate() {
            e.extension_name[i] = *b as std::ffi::c_char;
        }
        let exts = [ash::vk::ExtensionProperties::default(), e];
        assert!(super::ext_advertised(
            &exts,
            ash::ext::queue_family_foreign::NAME
        ));
        assert!(!super::ext_advertised(
            &exts[..1],
            ash::ext::queue_family_foreign::NAME
        ));
    }

    #[test]
    fn ext_advertised_rejects_unterminated_name_without_overrunning() {
        let mut bad = ash::vk::ExtensionProperties::default();
        bad.extension_name.fill(b'A' as std::ffi::c_char);
        // Last so an unbounded walk would leave the array.
        let exts = [ash::vk::ExtensionProperties::default(), bad];
        assert!(!super::ext_advertised(
            &exts,
            ash::ext::queue_family_foreign::NAME
        ));
        assert!(!super::ext_advertised(&exts, c"AAAA"));
    }

    use super::*;

    #[test]
    fn normalize_cpu_rgb_expands_24bpp_and_borrows_4bpp() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5, 6], &mut scratch, false);
        assert_eq!(f, PixelFormat::Rgbx);
        assert_eq!(b, &[1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // 5 bytes = one pixel + a 2-byte remainder that must be dropped.
        let mut scratch = Vec::new();
        let (_, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5], &mut scratch, false);
        assert_eq!(b, &[1, 2, 3, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgrx, &src, &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());

        assert_eq!(
            pixel_to_vk(PixelFormat::Rgbx),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert_eq!(
            pixel_to_vk(PixelFormat::Bgrx),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
    }

    #[test]
    fn normalize_cpu_rgb_forces_bgra_for_the_encode_source() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // R-first 4-bpp: swap; source alpha replaced by the 0xFF pad.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgbx, &[1, 2, 3, 4], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgra, &src, &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgra);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());
    }
}
