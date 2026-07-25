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

/// Normalize a CPU RGB payload for Vulkan upload. The packed 24-bpp `Rgb`/`Bgr` the PipeWire
/// capturer can negotiate are expanded 3→4 into `scratch` (kept by the caller across frames — no
/// per-frame allocation) with the pad byte = 0xFF; refusing them instead used to kill a session
/// at its first frame (WP5.4). No packed 24-bpp VkFormat is reliably uploadable/sampleable on
/// target GPUs, and this path is CPU-sourced by definition, so one cheap expand pass serves it
/// (the same call NVENC answers with its swscale 3→4 expand, WP1.4).
///
/// `bgra_target = false` (the CSC paths): channel order is preserved — the sampler reads through
/// the matching view format, so any 4-bpp order works and 4-bpp inputs pass through borrowed.
/// `bgra_target = true` (the RGB-direct encode source): the output byte order is forced to
/// B,G,R,X, because the video session's `pictureFormat` is `B8G8R8A8_UNORM` and
/// VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207 requires the source image to match it — an
/// R-first source (`Rgbx`/`Rgba`/`Rgb`) is channel-swapped during the same pass. (Caught live on
/// RADV by `vulkan_smoke_rgb_cpu24`; the mismatch predates the 24-bpp support for `Rgbx` CPU
/// sources.)
///
/// Payloads are tightly packed with no row padding (`FramePayload::Cpu`'s contract), so the
/// conversion is row-agnostic; a truncated source yields a truncated output, which the upload
/// paths already bound-check exactly as they did the raw bytes.
pub(crate) fn normalize_cpu_rgb<'a>(
    fmt: PixelFormat,
    bytes: &'a [u8],
    scratch: &'a mut Vec<u8>,
    bgra_target: bool,
) -> (PixelFormat, &'a [u8]) {
    // Per-pixel source layout: bytes-per-pixel + where R, G, B sit in each pixel.
    let (bpp, r, g, b) = match fmt {
        PixelFormat::Rgb => (3usize, 0usize, 1usize, 2usize),
        PixelFormat::Bgr => (3, 2, 1, 0),
        PixelFormat::Rgbx | PixelFormat::Rgba => (4, 0, 1, 2),
        PixelFormat::Bgrx | PixelFormat::Bgra => (4, 2, 1, 0),
        _ => return (fmt, bytes),
    };
    if bpp == 4 && (!bgra_target || b == 0) {
        return (fmt, bytes); // 4-bpp in an acceptable order: borrow untouched
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

/// Whether a failed dmabuf import should count toward pf-zerocopy's raw-dmabuf degrade latch
/// (`note_raw_dmabuf_import_failure` — 3 consecutive failures flip capture to CPU delivery for
/// the process). Deterministic refusals (unsupported fourcc, the driver rejecting the buffer)
/// must count — they repeat identically forever and the latch is their only recovery. Transient
/// VRAM pressure must NOT: three tight allocation OOMs would otherwise permanently downgrade a
/// working host to CPU capture.
pub(crate) fn import_failure_feeds_latch(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<vk::Result>() {
        Some(&r) => {
            r != vk::Result::ERROR_OUT_OF_DEVICE_MEMORY && r != vk::Result::ERROR_OUT_OF_HOST_MEMORY
        }
        None => true,
    }
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
    use std::os::fd::{AsRawFd, IntoRawFd};
    let fmt = fourcc_to_vk(d.fourcc)
        .with_context(|| format!("unsupported dmabuf fourcc {:#x}", d.fourcc))?;
    // Dup the fd FIRST, and keep it OWNED: ownership transfers to Vulkan only on a SUCCESSFUL
    // `allocate_memory` (VK_KHR_external_memory_fd — from then on `vkFreeMemory` closes it), so
    // the release below sits in exactly that arm. Every earlier failure drops the `OwnedFd` for
    // a single clean close. An explicit `close` after a successful import would be a double
    // close — and a recycled fd number then clobbers an unrelated descriptor in this process.
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
    // Unwind discipline below mirrors `make_plain_image`: every failure destroys what this call
    // created (and ONLY that — the caller's `DmabufFrame` fd stays theirs).
    let fd_props = {
        let mut p = vk::MemoryFdPropertiesKHR::default();
        // Borrow-only query (no ownership transfer); an error leaves memory_type_bits = 0 and
        // the fallback below uses the image requirements alone.
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
            return Err(e.into()); // `dup` drops here: the one close of the failed import's fd
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

/// Create + allocate + bind a host-visible/coherent buffer with `make_plain_image`'s unwind
/// discipline: on any failure everything this call created is destroyed before returning, so
/// callers can `?` freely. Both `ensure_cpu_rgb` staging twins open-coded this sequence and
/// leaked the buffer (and then buffer+memory) on the allocate/bind failure arms.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// CSC mode (`bgra_target = false`): the 3→4 expand is a pure byte shuffle — no channel
    /// reorder, pad byte 0xFF, truncated tail pixels dropped (never overrun) — and 4-bpp inputs
    /// pass through borrowed untouched.
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

        // Truncated tail: 5 bytes = one whole pixel + a 2-byte remainder that must be dropped.
        let mut scratch = Vec::new();
        let (_, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5], &mut scratch, false);
        assert_eq!(b, &[1, 2, 3, 0xFF]);

        // 4-bpp passthrough: borrowed, scratch untouched.
        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgrx, &src, &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());

        // The 4-bpp mapping the expand lands on matches pixel_to_vk's existing table.
        assert_eq!(
            pixel_to_vk(PixelFormat::Rgbx),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert_eq!(
            pixel_to_vk(PixelFormat::Bgrx),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
    }

    /// RGB-direct mode (`bgra_target = true`): everything lands in B,G,R,X order because the
    /// video session's `pictureFormat` is `B8G8R8A8_UNORM` and the encode source must match it
    /// (VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207 — caught live on RADV). B-first inputs pass
    /// through borrowed; R-first inputs are channel-swapped, 3-bpp and 4-bpp alike.
    #[test]
    fn normalize_cpu_rgb_forces_bgra_for_the_encode_source() {
        // Rgb (R,G,B) → B,G,R,X with the swap folded into the expand.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        // Bgr (B,G,R) → same order, expanded.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // Rgbx: 4-bpp but R-first — swapped, alpha replaced by the 0xFF pad.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgbx, &[1, 2, 3, 4], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        // Bgrx/Bgra already match the session order: borrowed untouched.
        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgra, &src, &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgra);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());
    }
}
