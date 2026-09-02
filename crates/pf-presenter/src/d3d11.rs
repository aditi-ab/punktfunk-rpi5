//! D3D11 shared-texture → Vulkan import (Windows): presenter half of
//! D3D11VA (`pf_client_core::video_d3d11`). Each frame is the NT handle of
//! a shareable single-plane RGB texture — BGRA8 sRGB, or RGB10A2 PQ
//! (the video processor already did YUV→RGB). Imported as one `VkImage`
//! (`VK_KHR_external_memory_win32`, dedicated allocation) and blitted into
//! the video image; no CSC.
//!
//! Both sides acquire/release the DXGI keyed mutex (`VK_KHR_win32_keyed_mutex`)
//! on key 0. Import is per-frame (parked in `Retired` until the fence);
//! the decoder ring still owns the NT handle. A driver reject is a clean
//! error; the caller demotes.

use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{ColorDesc, D3d11Frame};

/// Required at device creation. Missing either, `supports_d3d11()` is false.
pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 2] = [
    ash::khr::external_memory_win32::NAME,
    ash::khr::win32_keyed_mutex::NAME,
];

/// Spec-required probe for the image `import` creates. An unsupported
/// external image is undefined.
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
    // SAFETY: `instance` is live; `fmt_info` and `props` are locals that outlive the call.
    unsafe { instance.get_physical_device_image_format_properties2(pdev, &fmt_info, &mut props) }
        .is_ok()
        && ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}

/// `(bgra8, rgb10)`: BGRA8 gates D3D11VA; RGB10A2 gates only PQ pass-through.
/// Without RGB10, a PQ stream tone-maps to BGRA8 on the decoder.
pub fn import_supported(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> (bool, bool) {
    let bgra8 = format_importable(instance, pdev, vk::Format::B8G8R8A8_UNORM);
    let rgb10 = format_importable(instance, pdev, vk::Format::A2B10G10R10_UNORM_PACK32);
    tracing::info!(bgra8, rgb10, "D3D11 texture → Vulkan import support");
    (bgra8, rgb10)
}

/// Imported blit source. Park until the in-flight fence signals, then
/// [`HwFrame::destroy`]. `memory` is what the submit's keyed-mutex info names.
pub struct HwFrame {
    pub color: ColorDesc,
    pub width: u32,
    pub height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl HwFrame {
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// The submit's keyed-mutex acquire/release info names this allocation.
    pub fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }

    pub fn destroy(self, device: &ash::Device) {
        // SAFETY: `self` owns `image` and `memory`. Called only after the frame's
        // fence has signaled, so the GPU is idle on them.
        unsafe {
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Import one hand-off frame. A driver reject is a clean error; the caller demotes.
pub fn import(
    device: &ash::Device,
    ext_mem_win32: &ash::khr::external_memory_win32::Device,
    frame: &D3d11Frame,
) -> Result<HwFrame> {
    // Test hook: fault every import so demotion is exercisable without a broken driver.
    if std::env::var_os("PUNKTFUNK_HW_FAULT").is_some_and(|v| v == "import") {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    // DXGI R10G10B10A2 matches Vulkan A2B10G10R10_PACK32 (R in the low bits).
    let mp_format = if frame.rgb10 {
        vk::Format::A2B10G10R10_UNORM_PACK32
    } else {
        vk::Format::B8G8R8A8_UNORM
    };
    let handle_type = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

    // Single-plane, TRANSFER_SRC only: match the D3D11 resource (no view-format aliasing).
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle_type);
    // SAFETY: `external_info` is a local that outlives the call; the returned handle is owned here.
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
        let handle = frame.handle as vk::HANDLE;
        let mut handle_props = vk::MemoryWin32HandlePropertiesKHR::default();
        // SAFETY: `handle` is the decoder ring's live NT handle; `handle_props` is a
        // local that outlives the call.
        unsafe {
            ext_mem_win32.get_memory_win32_handle_properties(handle_type, handle, &mut handle_props)
        }
        .context("vkGetMemoryWin32HandlePropertiesKHR")?;
        // SAFETY: `image` was created above and has not been destroyed.
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & handle_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for the D3D11 texture")?;

        // Import does not take NT-handle ownership; the decoder ring still closes it.
        let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(handle_type)
            .handle(handle);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        // SAFETY: `import_info` and `dedicated` are locals that outlive the call.
        // `import_info.handle` is the decoder ring's live NT handle, not owned here.
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
        // SAFETY: `image` and `memory` were created above and are still owned here.
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: `memory` was allocated in this call and never bound, so the GPU is idle on it.
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();
    let memory = match result {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: `image` was created in this call and never bound, so the GPU is idle on it.
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
