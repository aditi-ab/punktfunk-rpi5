//! D3D11 shared-texture → Vulkan import (Windows): the presenter half of the D3D11VA
//! decode path (`pf_client_core::video_d3d11`). Each decoded frame arrives as the NT
//! handle of a shareable single-plane RGB texture — **BGRA8** sRGB normally, **RGB10A2**
//! PQ for the HDR pass-through flavor (the decoder's VideoProcessor already did
//! YUV→RGB); we import it as a single-plane VkImage (`VK_KHR_external_memory_win32`,
//! dedicated allocation) and the presenter blits it straight into its video image — no
//! CSC pass. Single-plane RGBA is deliberate: importing the earlier multiplanar NV12
//! hand-off device-lost on NVIDIA however it was consumed (sampling or DMA copy, both
//! validation-clean — bisected 2026-07-09), while RGBA D3D11↔Vulkan interop is the path
//! Chromium/ANGLE exercise on every Windows driver.
//!
//! Synchronization is the texture's DXGI **keyed mutex** (`VK_KHR_win32_keyed_mutex`),
//! key 0 on both sides: the submit chains an acquire(0)/release(0) pair, so the GPU
//! waits for the decoder's conversion to complete before reading and the decoder's next
//! `AcquireSync(0)` on that ring slot blocks until our reads are done. Import is
//! per-frame (same discipline as the dmabuf path — parked in `Retired` until the fence
//! proves the GPU past it); NT-handle ownership stays with the decoder ring, the import
//! only references the payload.

use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{ColorDesc, D3d11Frame};

/// The two device extensions this path needs; queried at device creation. Broadly present
/// on every Windows driver (NVIDIA/AMD/Intel) — a device without them just reports
/// `supports_d3d11() == false` and the decoder chain skips D3D11VA.
pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 2] = [
    ash::khr::external_memory_win32::NAME,
    ash::khr::win32_keyed_mutex::NAME,
];

/// Can this device import a D3D11 texture of `format` as a blit source? The spec-required
/// capability probe for the exact image the import path creates — creating an external
/// image the driver doesn't support is undefined behavior (observed as
/// `VK_ERROR_DEVICE_LOST` at the first submits with the old NV12 hand-off).
fn format_importable(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
) -> bool {
    let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
    let fmt_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC)
        .push_next(&mut ext_info);
    let mut ext_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
    unsafe { instance.get_physical_device_image_format_properties2(pdev, &fmt_info, &mut props) }
        .is_ok()
        && ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}

/// The two hand-off flavors' import support: `.0` = BGRA8 (the SDR ring — gates the whole
/// D3D11VA path), `.1` = RGB10A2 (the HDR PQ ring — gates only the pass-through flavor;
/// without it a PQ stream keeps the decoder-side tonemap to BGRA8).
pub fn import_supported(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> (bool, bool) {
    let bgra8 = format_importable(instance, pdev, vk::Format::B8G8R8A8_UNORM);
    let rgb10 = format_importable(instance, pdev, vk::Format::A2B10G10R10_UNORM_PACK32);
    tracing::info!(bgra8, rgb10, "D3D11 texture → Vulkan import support");
    (bgra8, rgb10)
}

/// One imported frame: the BGRA8 image over the shared texture and its imported
/// (dedicated) memory — a blit source, nothing more. Parked until the in-flight fence
/// proves the GPU past the blit, then [`HwFrame::destroy`]ed — the memory is what the
/// keyed-mutex info on the submit references.
pub struct HwFrame {
    pub color: ColorDesc,
    pub width: u32,
    pub height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl HwFrame {
    /// The imported image — the acquire barrier + copy source.
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// The imported memory object — the submit's keyed-mutex acquire/release info needs it.
    pub fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }

    pub fn destroy(self, device: &ash::Device) {
        unsafe {
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Import one hand-off frame. Fails cleanly (the caller demotes to software after a
/// streak) on anything the driver rejects: unsupported multiplanar external format,
/// import refusal, no matching memory type.
pub fn import(
    device: &ash::Device,
    ext_mem_win32: &ash::khr::external_memory_win32::Device,
    frame: &D3d11Frame,
) -> Result<HwFrame> {
    // The demotion test hook — same contract as the dmabuf path's.
    if std::env::var_os("PUNKTFUNK_HW_FAULT").is_some_and(|v| v == "import") {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    // DXGI R10G10B10A2 and Vulkan A2B10G10R10_PACK32 are the same bit layout (R in the
    // low bits) — the standard interop pairing, same as BGRA8 ↔ B8G8R8A8.
    let mp_format = if frame.rgb10 {
        vk::Format::A2B10G10R10_UNORM_PACK32
    } else {
        vk::Format::B8G8R8A8_UNORM
    };
    let handle_type = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

    // One single-plane image over the whole texture, transfer-source only — the blit is
    // the whole job. Kept maximally "identical" to the D3D11 resource (no view-format
    // aliasing, no extra usages).
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .push_next(&mut external_info)
                .image_type(vk::ImageType::TYPE_2D)
                .format(mp_format)
                .extent(vk::Extent3D {
                    width: frame.width,
                    height: frame.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_SRC)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .with_context(|| {
        format!(
            "create {}x{} {mp_format:?} external image",
            frame.width, frame.height
        )
    })?;

    let result = (|| {
        // The handle's importable memory types, intersected with the image's requirement.
        let handle = frame.handle as vk::HANDLE;
        let mut handle_props = vk::MemoryWin32HandlePropertiesKHR::default();
        unsafe {
            ext_mem_win32.get_memory_win32_handle_properties(handle_type, handle, &mut handle_props)
        }
        .context("vkGetMemoryWin32HandlePropertiesKHR")?;
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & handle_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for the D3D11 texture")?;

        // Import does NOT take handle ownership (NT handle rule): the decoder ring keeps
        // closing its own handle; this allocation references the payload independently.
        let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(handle_type)
            .handle(handle);
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
        .context("import D3D11 texture memory")?;
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();
    let memory = match result {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            return Err(e);
        }
    };

    Ok(HwFrame {
        color: frame.color,
        width: frame.width,
        height: frame.height,
        image,
        memory,
    })
}
