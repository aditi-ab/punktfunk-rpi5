//! FFmpeg's Vulkan hwcontext surface (`AVVulkanDeviceContext`, `AVVulkanFramesContext`,
//! `AVVkFrame`), bindgen-generated from the system headers at build time — see build.rs
//! for why this must not be hand-transcribed.
//!
//! The raw bindings use vulkan.h's own handle types (pointers on 64-bit). The [`ash`]
//! conversion helpers below cross between them and ash's u64-newtype handles; both sides
//! are the same underlying Vulkan object handles, so the casts are value-preserving.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
// bindgen's layout tests deref-null-pointer by design; silence the lints they trip.
#![allow(deref_nullptr)]
#![allow(unnecessary_transmutes)]

#[cfg(target_os = "linux")]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Conversions between the generated vulkan.h handle types and ash's.
#[cfg(target_os = "linux")]
pub mod ashx {
    use super::*;
    use ash::vk::Handle as _;

    /// vulkan.h non-dispatchable handles are `*mut T` on 64-bit; ash's are `u64`
    /// newtypes. Same bits either way.
    pub fn image(h: VkImage) -> ash::vk::Image {
        ash::vk::Image::from_raw(h as u64)
    }

    pub fn semaphore(h: VkSemaphore) -> ash::vk::Semaphore {
        ash::vk::Semaphore::from_raw(h as u64)
    }

    pub fn image_layout(l: VkImageLayout) -> ash::vk::ImageLayout {
        ash::vk::ImageLayout::from_raw(l as i32)
    }

    // --- ash → vulkan.h (filling AVVulkanDeviceContext) ---------------------------------

    pub fn to_instance(h: ash::vk::Instance) -> VkInstance {
        h.as_raw() as VkInstance
    }

    pub fn to_physical_device(h: ash::vk::PhysicalDevice) -> VkPhysicalDevice {
        h.as_raw() as VkPhysicalDevice
    }

    pub fn to_device(h: ash::vk::Device) -> VkDevice {
        h.as_raw() as VkDevice
    }

    /// ash's loader-level `vkGetInstanceProcAddr` as the header's PFN type. Both are the
    /// same C ABI function pointer (`extern "system"` == `extern "C"` on the platforms
    /// this crate builds for).
    pub fn to_get_proc_addr(
        f: unsafe extern "system" fn(
            ash::vk::Instance,
            *const std::ffi::c_char,
        ) -> ash::vk::PFN_vkVoidFunction,
    ) -> PFN_vkGetInstanceProcAddr {
        unsafe { std::mem::transmute(f) }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The allocator runs (links against the system libavutil) and the struct is
    /// readable at the offsets bindgen computed — sem_value zero-initialized.
    #[test]
    fn vk_frame_alloc_links_and_zeroes() {
        unsafe {
            let f = av_vk_frame_alloc();
            assert!(!f.is_null(), "av_vk_frame_alloc returned NULL");
            assert_eq!((*f).sem_value[0], 0);
            assert_eq!((*f).queue_family[0], 0);
            // Leak the one test frame rather than binding av_free here.
        }
    }

    /// AV_NUM_DATA_POINTERS-sized arrays came through with the right length.
    #[test]
    fn frame_arrays_are_av_num_data_pointers() {
        let f: AVVkFrame = unsafe { std::mem::zeroed() };
        assert_eq!(f.img.len(), 8);
        assert_eq!(f.sem_value.len(), 8);
    }
}
