//! Vendored `VK_VALVE_video_encode_rgb_conversion` bindings — the RGB→YCbCr encode-source
//! extension (Vulkan 1.4.327; RADV since Mesa 26.0, hardware-gated on the VCN EFC front-end
//! conversion block). Our pinned `ash 0.38.0+1.3.281` predates it entirely; same vendoring
//! rationale as [`vk_av1_encode`](super::vk_av1_encode) — definitions copied from the registry
//! so the layouts are correct-by-construction, chained via raw `p_next`. Consumed by
//! `vulkan_video.rs`: B0 probes + logs availability (design/vulkan-rgb-direct-encode.md);
//! B1 makes the captured BGRx dmabuf the direct encode source with EFC doing the 709-narrow CSC.
#![allow(dead_code)]

use ash::vk;
use std::ffi::{c_void, CStr};

pub const EXTENSION_NAME: &CStr = c"VK_VALVE_video_encode_rgb_conversion";

// ---------- struct-type (VkStructureType) values — construct via `stype` ----------
pub const ST_PHYSICAL_DEVICE_FEATURES: i32 = 1_000_390_000;
pub const ST_CAPABILITIES: i32 = 1_000_390_001;
pub const ST_PROFILE_INFO: i32 = 1_000_390_002;
pub const ST_SESSION_CREATE_INFO: i32 = 1_000_390_003;

// `VkVideoEncodeRgbModelConversionFlagBitsVALVE`
pub const MODEL_RGB_IDENTITY: u32 = 0x01;
pub const MODEL_YCBCR_IDENTITY: u32 = 0x02;
pub const MODEL_YCBCR_709: u32 = 0x04;
pub const MODEL_YCBCR_601: u32 = 0x08;
pub const MODEL_YCBCR_2020: u32 = 0x10;
// `VkVideoEncodeRgbRangeCompressionFlagBitsVALVE`
pub const RANGE_FULL: u32 = 0x01;
pub const RANGE_NARROW: u32 = 0x02;
// `VkVideoEncodeRgbChromaOffsetFlagBitsVALVE`
pub const CHROMA_OFFSET_COSITED_EVEN: u32 = 0x01;
pub const CHROMA_OFFSET_MIDPOINT: u32 = 0x02;

/// `VkPhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE` — chain into
/// `VkPhysicalDeviceFeatures2` (query) / `VkDeviceCreateInfo` (enable).
#[repr(C)]
pub struct PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub video_encode_rgb_conversion: vk::Bool32,
}

/// `VkVideoEncodeRgbConversionCapabilitiesVALVE` — chain into the
/// `vkGetPhysicalDeviceVideoCapabilitiesKHR` output when the queried profile carries
/// [`VideoEncodeProfileRgbConversionInfoVALVE`]; reports which conversions the HW does.
#[repr(C)]
pub struct VideoEncodeRgbConversionCapabilitiesVALVE {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub rgb_models: u32,
    pub rgb_ranges: u32,
    pub x_chroma_offsets: u32,
    pub y_chroma_offsets: u32,
}

/// `VkVideoEncodeProfileRgbConversionInfoVALVE` — part of the video-profile *identity*: every
/// consumer of the profile (caps query, format query, session, image profile lists) must carry
/// the same chain.
#[repr(C)]
pub struct VideoEncodeProfileRgbConversionInfoVALVE {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub perform_encode_rgb_conversion: vk::Bool32,
}

/// `VkVideoEncodeSessionRgbConversionCreateInfoVALVE` — chain into
/// `VkVideoSessionCreateInfoKHR`; single-bit selections of the conversion actually performed.
#[repr(C)]
pub struct VideoEncodeSessionRgbConversionCreateInfoVALVE {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub rgb_model: u32,
    pub rgb_range: u32,
    pub x_chroma_offset: u32,
    pub y_chroma_offset: u32,
}

/// `vk::StructureType` for a raw `ST_*` constant above.
#[inline]
pub fn stype(raw: i32) -> vk::StructureType {
    vk::StructureType::from_raw(raw)
}

// ---------- ABI layout guard ----------
//
// These structs are hand-copied from the registry and handed to the driver through raw `p_next`
// chains, so nothing in the type system relates them to the C definitions any more: an edit that
// inserts, drops, widens or re-pads a field is not a compile error, it is the driver reading our
// bytes at the wrong offsets. The assertions below are the missing compile error. They are `const`
// rather than `#[cfg(test)]` (the shape `amf.rs` uses) so they hold in every build, including the
// shipped one, and on any target this module compiles for.
//
// What they catch: a changed field width, an inserted or removed field, a changed array length, a
// padding assumption that only holds on one target. What they CANNOT catch: swapping two fields of
// the same type — offsets are unchanged. That case is only caught by reading the registry, so the
// field order here was diffed against `vulkan_core.h` (Vulkan-Headers `main`, 2026-07-25) when
// these assertions were written, along with every `ST_*` and flag-bit value above.
//
// Deliberately duplicated in `vk_av1_encode.rs` rather than shared: both modules exist to be
// deleted wholesale once `ash` ships these bindings, and a shared helper would make deleting one
// break the other.
macro_rules! assert_abi_layout {
    ($t:ty { size: $size:expr, align: $align:expr $(, $field:ident @ $off:expr)* $(,)? }) => {
        const _: () = {
            assert!(
                ::core::mem::size_of::<$t>() == $size,
                concat!(stringify!($t), ": size does not match the C ABI")
            );
            assert!(
                ::core::mem::align_of::<$t>() == $align,
                concat!(stringify!($t), ": alignment does not match the C ABI")
            );
            $(assert!(
                ::core::mem::offset_of!($t, $field) == $off,
                concat!(stringify!($t), ".", stringify!($field), ": offset does not match the C ABI")
            );)*
        };
    };
}

assert_abi_layout!(PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    video_encode_rgb_conversion @ 16,
});

assert_abi_layout!(VideoEncodeRgbConversionCapabilitiesVALVE {
    size: 32, align: 8,
    s_type @ 0,
    p_next @ 8,
    rgb_models @ 16,
    rgb_ranges @ 20,
    x_chroma_offsets @ 24,
    y_chroma_offsets @ 28,
});

assert_abi_layout!(VideoEncodeProfileRgbConversionInfoVALVE {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    perform_encode_rgb_conversion @ 16,
});

assert_abi_layout!(VideoEncodeSessionRgbConversionCreateInfoVALVE {
    size: 32, align: 8,
    s_type @ 0,
    p_next @ 8,
    rgb_model @ 16,
    rgb_range @ 20,
    x_chroma_offset @ 24,
    y_chroma_offset @ 28,
});
