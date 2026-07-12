//! Vendored `VK_KHR_video_encode_av1` bindings — the AV1-encode structs, `StdVideoEncodeAV1*`
//! types and struct-type constants that our pinned `ash 0.38.0+1.3.281` does not ship (the
//! extension was finalized in Vulkan 1.3.290). Bumping `ash` to git-master (`+1.4.352`, which has
//! them) breaks the *client*: it drops the lifetime on `vk::AllocationCallbacks`, and `sdl3-sys`'s
//! `ash` feature still generates `AllocationCallbacks<'static>`, so the presenter's SDL/Vulkan
//! surface path won't compile. Rather than churn the client for a host-only need, we vendor just
//! the encode-side definitions here, **copied verbatim from ash-master's generated code** (so the
//! layouts are correct-by-construction) and chain them into ash's generic video-encode-queue calls
//! via raw `p_next`, exactly as the HEVC path already chains its rate-control struct.
//!
//! Everything *common* to AV1 (sequence header, tile/quant/loop-filter/CDEF/… sub-structs, the
//! `StdVideoAV1*` enums) is already present in 1.3.281's `ash::vk::native` — AV1 **decode** brought
//! it in — so we reuse those and vendor only the encode-specific pieces. Delete this module and
//! switch to `ash::vk::*` once `ash` publishes a 1.4.x release and `sdl3-sys` regenerates.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use ash::vk;
use ash::vk::native::{
    StdVideoAV1FrameType, StdVideoAV1InterpolationFilter, StdVideoAV1Level, StdVideoAV1Profile,
    StdVideoAV1SequenceHeader, StdVideoAV1TxMode,
};
use std::ffi::{c_void, CStr};

/// `VK_KHR_video_encode_av1` extension name — ash 0.38's `ash::khr::video_encode_av1` doesn't exist,
/// so we pass this raw to `enabled_extension_names`.
pub const EXTENSION_NAME: &CStr = c"VK_KHR_video_encode_av1";

// ---------- struct-type (VkStructureType) values — construct via `vk::StructureType::from_raw` ----------
pub const ST_CAPABILITIES: i32 = 1_000_513_000;
pub const ST_SESSION_PARAMETERS_CREATE_INFO: i32 = 1_000_513_001;
pub const ST_PICTURE_INFO: i32 = 1_000_513_002;
pub const ST_DPB_SLOT_INFO: i32 = 1_000_513_003;
pub const ST_PROFILE_INFO: i32 = 1_000_513_005;
pub const ST_RATE_CONTROL_INFO: i32 = 1_000_513_006;
pub const ST_RATE_CONTROL_LAYER_INFO: i32 = 1_000_513_007;
pub const ST_SESSION_CREATE_INFO: i32 = 1_000_513_009;
pub const ST_GOP_REMAINING_FRAME_INFO: i32 = 1_000_513_010;

/// `VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR` (bit 18).
pub const VIDEO_CODEC_OPERATION_ENCODE_AV1: u32 = 0x0004_0000;
/// `VK_MAX_VIDEO_AV1_REFERENCES_PER_FRAME_KHR` — LAST..ALTREF (the 7 inter reference names).
pub const MAX_VIDEO_AV1_REFERENCES_PER_FRAME: usize = 7;
/// `STD_VIDEO_AV1_PRIMARY_REF_NONE` — a frame that inherits no CDF/context from any reference
/// (the recovery-anchor lever: a clean P-frame independent of prior probability state).
pub const PRIMARY_REF_NONE: u8 = 7;
/// `VK_VIDEO_ENCODE_AV1_SUPERBLOCK_SIZE_128_BIT_KHR` (bit 1 of the superblock-size flags).
pub const SUPERBLOCK_SIZE_128: u32 = 0x2;

// `VkVideoEncodeAV1PredictionModeKHR`
pub const PREDICTION_MODE_INTRA_ONLY: i32 = 0;
pub const PREDICTION_MODE_SINGLE_REFERENCE: i32 = 1;
// `VkVideoEncodeAV1RateControlGroupKHR`
pub const RC_GROUP_INTRA: i32 = 0;
pub const RC_GROUP_PREDICTIVE: i32 = 1;
pub const RC_GROUP_BIPREDICTIVE: i32 = 2;

// AV1 reference names (index into `reference_name_slot_indices`, which is 0-based over LAST..ALTREF).
pub const REFERENCE_NAME_LAST_FRAME_IDX: usize = 0; // STD_VIDEO_AV1_REFERENCE_NAME_LAST_FRAME - 1

// ---------- bindgen bitfield helper (copied verbatim from ash-master native.rs) ----------
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct __BindgenBitfieldUnit<Storage> {
    storage: Storage,
}
impl<Storage> __BindgenBitfieldUnit<Storage> {
    #[inline]
    pub const fn new(storage: Storage) -> Self {
        Self { storage }
    }
}
impl<Storage> __BindgenBitfieldUnit<Storage>
where
    Storage: AsRef<[u8]> + AsMut<[u8]>,
{
    #[inline]
    pub fn get_bit(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let byte = self.storage.as_ref()[byte_index];
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        byte & (1 << bit_index) == (1 << bit_index)
    }
    #[inline]
    pub fn set_bit(&mut self, index: usize, val: bool) {
        let byte_index = index / 8;
        let byte = &mut self.storage.as_mut()[byte_index];
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        let mask = 1 << bit_index;
        if val {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
    #[inline]
    pub fn get(&self, bit_offset: usize, bit_width: u8) -> u64 {
        let mut val = 0;
        for i in 0..(bit_width as usize) {
            if self.get_bit(i + bit_offset) {
                let index = if cfg!(target_endian = "big") {
                    bit_width as usize - 1 - i
                } else {
                    i
                };
                val |= 1 << index;
            }
        }
        val
    }
    #[inline]
    pub fn set(&mut self, bit_offset: usize, bit_width: u8, val: u64) {
        for i in 0..(bit_width as usize) {
            let mask = 1 << i;
            let val_bit_is_set = val & mask == mask;
            let index = if cfg!(target_endian = "big") {
                bit_width as usize - 1 - i
            } else {
                i
            };
            self.set_bit(index + bit_offset, val_bit_is_set);
        }
    }
}

// ---------- Std encode structs (copied from ash-master native.rs; common Std types reused from ash) ----------
#[repr(C, align(4))]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1PictureInfoFlags {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
impl StdVideoEncodeAV1PictureInfoFlags {
    #[inline]
    pub fn set_error_resilient_mode(&mut self, val: u32) {
        self._bitfield_1.set(0, 1, val as u64)
    }
    #[inline]
    pub fn set_disable_cdf_update(&mut self, val: u32) {
        self._bitfield_1.set(1, 1, val as u64)
    }
    #[inline]
    pub fn set_use_superres(&mut self, val: u32) {
        self._bitfield_1.set(2, 1, val as u64)
    }
    #[inline]
    pub fn set_render_and_frame_size_different(&mut self, val: u32) {
        self._bitfield_1.set(3, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_screen_content_tools(&mut self, val: u32) {
        self._bitfield_1.set(4, 1, val as u64)
    }
    #[inline]
    pub fn set_is_filter_switchable(&mut self, val: u32) {
        self._bitfield_1.set(5, 1, val as u64)
    }
    #[inline]
    pub fn set_force_integer_mv(&mut self, val: u32) {
        self._bitfield_1.set(6, 1, val as u64)
    }
    #[inline]
    pub fn set_frame_size_override_flag(&mut self, val: u32) {
        self._bitfield_1.set(7, 1, val as u64)
    }
    #[inline]
    pub fn set_buffer_removal_time_present_flag(&mut self, val: u32) {
        self._bitfield_1.set(8, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_intrabc(&mut self, val: u32) {
        self._bitfield_1.set(9, 1, val as u64)
    }
    #[inline]
    pub fn set_frame_refs_short_signaling(&mut self, val: u32) {
        self._bitfield_1.set(10, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_high_precision_mv(&mut self, val: u32) {
        self._bitfield_1.set(11, 1, val as u64)
    }
    #[inline]
    pub fn set_is_motion_mode_switchable(&mut self, val: u32) {
        self._bitfield_1.set(12, 1, val as u64)
    }
    #[inline]
    pub fn set_use_ref_frame_mvs(&mut self, val: u32) {
        self._bitfield_1.set(13, 1, val as u64)
    }
    #[inline]
    pub fn set_disable_frame_end_update_cdf(&mut self, val: u32) {
        self._bitfield_1.set(14, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_warped_motion(&mut self, val: u32) {
        self._bitfield_1.set(15, 1, val as u64)
    }
    #[inline]
    pub fn set_reduced_tx_set(&mut self, val: u32) {
        self._bitfield_1.set(16, 1, val as u64)
    }
    #[inline]
    pub fn set_skip_mode_present(&mut self, val: u32) {
        self._bitfield_1.set(17, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_q_present(&mut self, val: u32) {
        self._bitfield_1.set(18, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_lf_present(&mut self, val: u32) {
        self._bitfield_1.set(19, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_lf_multi(&mut self, val: u32) {
        self._bitfield_1.set(20, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_enabled(&mut self, val: u32) {
        self._bitfield_1.set(21, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_update_map(&mut self, val: u32) {
        self._bitfield_1.set(22, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_temporal_update(&mut self, val: u32) {
        self._bitfield_1.set(23, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_update_data(&mut self, val: u32) {
        self._bitfield_1.set(24, 1, val as u64)
    }
    #[inline]
    pub fn set_UsesLr(&mut self, val: u32) {
        self._bitfield_1.set(25, 1, val as u64)
    }
    #[inline]
    pub fn set_usesChromaLr(&mut self, val: u32) {
        self._bitfield_1.set(26, 1, val as u64)
    }
    #[inline]
    pub fn set_show_frame(&mut self, val: u32) {
        self._bitfield_1.set(27, 1, val as u64)
    }
    #[inline]
    pub fn set_showable_frame(&mut self, val: u32) {
        self._bitfield_1.set(28, 1, val as u64)
    }
    #[inline]
    pub fn set_reserved(&mut self, val: u32) {
        self._bitfield_1.set(29, 3, val as u64)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1PictureInfo {
    pub flags: StdVideoEncodeAV1PictureInfoFlags,
    pub frame_type: StdVideoAV1FrameType,
    pub frame_presentation_time: u32,
    pub current_frame_id: u32,
    pub order_hint: u8,
    pub primary_ref_frame: u8,
    pub refresh_frame_flags: u8,
    pub coded_denom: u8,
    pub render_width_minus_1: u16,
    pub render_height_minus_1: u16,
    pub interpolation_filter: StdVideoAV1InterpolationFilter,
    pub TxMode: StdVideoAV1TxMode,
    pub delta_q_res: u8,
    pub delta_lf_res: u8,
    pub ref_order_hint: [u8; 8usize],
    pub ref_frame_idx: [i8; 7usize],
    pub reserved1: [u8; 3usize],
    pub delta_frame_id_minus_1: [u32; 7usize],
    pub pTileInfo: *const ash::vk::native::StdVideoAV1TileInfo,
    pub pQuantization: *const ash::vk::native::StdVideoAV1Quantization,
    pub pSegmentation: *const ash::vk::native::StdVideoAV1Segmentation,
    pub pLoopFilter: *const ash::vk::native::StdVideoAV1LoopFilter,
    pub pCDEF: *const ash::vk::native::StdVideoAV1CDEF,
    pub pLoopRestoration: *const ash::vk::native::StdVideoAV1LoopRestoration,
    pub pGlobalMotion: *const ash::vk::native::StdVideoAV1GlobalMotion,
    pub pExtensionHeader: *const StdVideoEncodeAV1ExtensionHeader,
    pub pBufferRemovalTimes: *const u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ReferenceInfoFlags {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
impl StdVideoEncodeAV1ReferenceInfoFlags {
    #[inline]
    pub fn set_disable_frame_end_update_cdf(&mut self, val: u32) {
        self._bitfield_1.set(0, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_enabled(&mut self, val: u32) {
        self._bitfield_1.set(1, 1, val as u64)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ReferenceInfo {
    pub flags: StdVideoEncodeAV1ReferenceInfoFlags,
    pub RefFrameId: u32,
    pub frame_type: StdVideoAV1FrameType,
    pub OrderHint: u8,
    pub reserved1: [u8; 3usize],
    pub pExtensionHeader: *const StdVideoEncodeAV1ExtensionHeader,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ExtensionHeader {
    pub temporal_id: u8,
    pub spatial_id: u8,
}

// ---------- KHR extension structs (repr(C); lifetimes/PhantomData dropped — layout-identical,
//            chained by raw p_next). Flag/enum newtypes flattened to their u32/i32 repr. ----------
#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1ProfileInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub std_profile: StdVideoAV1Profile,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1CapabilitiesKHR {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub flags: u32,
    pub max_level: StdVideoAV1Level,
    pub coded_picture_alignment: vk::Extent2D,
    pub max_tiles: vk::Extent2D,
    pub min_tile_size: vk::Extent2D,
    pub max_tile_size: vk::Extent2D,
    pub superblock_sizes: u32,
    pub max_single_reference_count: u32,
    pub single_reference_name_mask: u32,
    pub max_unidirectional_compound_reference_count: u32,
    pub max_unidirectional_compound_group1_reference_count: u32,
    pub unidirectional_compound_reference_name_mask: u32,
    pub max_bidirectional_compound_reference_count: u32,
    pub max_bidirectional_compound_group1_reference_count: u32,
    pub max_bidirectional_compound_group2_reference_count: u32,
    pub bidirectional_compound_reference_name_mask: u32,
    pub max_temporal_layer_count: u32,
    pub max_spatial_layer_count: u32,
    pub max_operating_points: u32,
    pub min_q_index: u32,
    pub max_q_index: u32,
    pub prefers_gop_remaining_frames: vk::Bool32,
    pub requires_gop_remaining_frames: vk::Bool32,
    pub std_syntax_flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1SessionParametersCreateInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub p_std_sequence_header: *const StdVideoAV1SequenceHeader,
    pub p_std_decoder_model_info: *const c_void,
    pub std_operating_point_count: u32,
    pub p_std_operating_points: *const c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1PictureInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub prediction_mode: i32,
    pub rate_control_group: i32,
    pub constant_q_index: u32,
    pub p_std_picture_info: *const StdVideoEncodeAV1PictureInfo,
    pub reference_name_slot_indices: [i32; MAX_VIDEO_AV1_REFERENCES_PER_FRAME],
    pub primary_reference_cdf_only: vk::Bool32,
    pub generate_obu_extension_header: vk::Bool32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1DpbSlotInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub p_std_reference_info: *const StdVideoEncodeAV1ReferenceInfo,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1RateControlInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub flags: u32,
    pub gop_frame_count: u32,
    pub key_frame_period: u32,
    pub consecutive_bipredictive_frame_count: u32,
    pub temporal_layer_count: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1QIndexKHR {
    pub intra_q_index: u32,
    pub predictive_q_index: u32,
    pub bipredictive_q_index: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1FrameSizeKHR {
    pub intra_frame_size: u32,
    pub predictive_frame_size: u32,
    pub bipredictive_frame_size: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1RateControlLayerInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_min_q_index: vk::Bool32,
    pub min_q_index: VideoEncodeAV1QIndexKHR,
    pub use_max_q_index: vk::Bool32,
    pub max_q_index: VideoEncodeAV1QIndexKHR,
    pub use_max_frame_size: vk::Bool32,
    pub max_frame_size: VideoEncodeAV1FrameSizeKHR,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1GopRemainingFrameInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_gop_remaining_frames: vk::Bool32,
    pub gop_remaining_intra: u32,
    pub gop_remaining_predictive: u32,
    pub gop_remaining_bipredictive: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1SessionCreateInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_max_level: vk::Bool32,
    pub max_level: StdVideoAV1Level,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1OperatingPointInfoFlags {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct StdVideoEncodeAV1OperatingPointInfo {
    pub flags: StdVideoEncodeAV1OperatingPointInfoFlags,
    pub operating_point_idc: u16,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub decoder_buffer_delay: u32,
    pub encoder_buffer_delay: u32,
    pub initial_display_delay_minus_1: u8,
}

/// `vk::StructureType` for a raw `ST_*` constant above.
#[inline]
pub fn stype(raw: i32) -> vk::StructureType {
    vk::StructureType::from_raw(raw)
}
