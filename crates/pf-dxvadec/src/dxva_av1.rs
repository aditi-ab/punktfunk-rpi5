//! The DXVA **AV1** buffer layouts, hand-declared — M7's Windows half.
//!
//! Same contract and the same hazards as [`crate::dxva`]: windows-rs generates
//! nothing from `dxva.h`, nothing here is type-checked against Windows, and a field
//! at the wrong offset is a driver reading a reference index where a quantiser
//! should be.
//!
//! # This one was MEASURED against Microsoft's own header
//!
//! The H.264 and HEVC layouts were checked against libavcodec's DXVA byte capture —
//! a mirror, and the best available at the time. AV1 does better: `DXVA_PicParams_AV1`
//! ships in the **Windows SDK's own `dxva.h`** (`10.0.26100.0` and `10.0.28000.0` on
//! the .173 box), which is the declaration the DRIVER was compiled against. So every
//! size, every offset and every bit position below came out of
//! `layout-probe-av1.c` compiled with MSVC against that header, and each is pinned as
//! a compile-time assertion. Re-run the probe (its own header carries the one-liner)
//! if the SDK ever moves.
//!
//! Measured: `DXVA_PicParams_AV1` is **912 bytes, alignment 1** — `dxva.h` packs
//! every one of these to a byte boundary, which is why the structs below carry
//! `#[repr(C, packed)]` and not a plain `#[repr(C)]`.
//!
//! # Two things AV1 puts somewhere unexpected
//!
//! **Global motion is per REFERENCE, inside the picture entry.** `DXVA_PicEntry_AV1`
//! is 36 bytes and carries `wmmat[6]` plus the warp type for that reference — where
//! Vulkan hangs one `StdVideoAV1GlobalMotion` block off the picture info. Same data,
//! a different owner, and a conversion that assumed the Vulkan shape would leave
//! every warped reference at identity.
//!
//! **CDEF strengths are packed two-to-a-byte.** `y_strengths[i]` and
//! `uv_strengths[i]` are single bytes holding `primary` in the low six bits and
//! `secondary` in the top two — not the separate arrays the AV1 syntax (and Vulkan's
//! Std block) use.

/// `DXVA_PicEntry_AV1` — 36 bytes, and it carries global motion (module docs).
///
/// ```c
/// typedef struct _DXVA_PicEntry_AV1 {
///     UINT width;
///     UINT height;
///     INT wmmat[6];              // global motion parameters
///     union { struct { UCHAR wminvalid:1; UCHAR wmtype:2; UCHAR Reserved:5; };
///             UCHAR GlobalMotionFlags; };
///     UCHAR Index;
///     UINT16 Reserved16Bits;
/// } DXVA_PicEntry_AV1;
/// ```
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PicEntryAv1 {
    /// The REFERENCE's own `UpscaledWidth` — not the current frame's. AV1 lets
    /// every frame pick its own size, and this pair is what lets the driver scale
    /// motion out of a differently-sized reference (libavcodec:
    /// `pp->frame_refs[i].width = ref_frame->width`).
    pub width: u32,
    /// The reference's own `FrameHeight`, on the same terms as [`Self::width`].
    pub height: u32,
    pub wmmat: [i32; 6],
    pub global_motion_flags: u8,
    /// ⚠⚠ The AV1 reference **SLOT** — `ref_frame_idx[i]`, 0..8 — or
    /// [`UNUSED_INDEX`] where this reference is not present. **Not a surface
    /// index.**
    ///
    /// This is a subscript INTO [`PicParamsAv1::ref_frame_map_texture_index`],
    /// which is the array that names surfaces; the driver dereferences one through
    /// the other. libavcodec writes `pp->frame_refs[i].Index = ref_frame ? ref_idx
    /// : 0xFF` with `ref_idx = frame_header->ref_frame_idx[i]`, and Chromium's
    /// `d3d11_av1_accelerator.cc` writes the same thing.
    ///
    /// Putting a surface index here is not a refusal: on a stream where reference
    /// `i` happens to live in the slot whose number equals its surface it decodes
    /// correctly, and everywhere else it predicts from whichever picture the
    /// reference store holds at the surface's number.
    pub index: u8,
    pub reserved16: u16,
}

/// What `DXVA_PicEntry_AV1::Index` (and `RefFrameMapTextureIndex`) carry for a
/// reference that is not present. `0xFF` is DXVA's universal "no surface".
pub const UNUSED_INDEX: u8 = 0xFF;

impl PicEntryAv1 {
    pub const fn zeroed() -> PicEntryAv1 {
        PicEntryAv1 {
            width: 0,
            height: 0,
            wmmat: [0; 6],
            global_motion_flags: 0,
            index: UNUSED_INDEX,
            reserved16: 0,
        }
    }
}

/// `GlobalMotionFlags`'s members, packed by [`GlobalMotionFlags::pack`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalMotionFlags {
    pub wminvalid: bool,
    /// `wmtype`: 0 identity, 1 translation, 2 rotzoom, 3 affine. Two bits.
    pub wmtype: u8,
}

impl GlobalMotionFlags {
    pub const fn pack(self) -> u8 {
        (self.wminvalid as u8) | ((self.wmtype & 0x3) << 1)
    }
}

/// The tile block inside [`PicParamsAv1`]. Declared as its own type so the
/// 64-entry arrays are named once.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TilesAv1 {
    pub cols: u8,
    pub rows: u8,
    pub context_update_id: u16,
    pub widths: [u16; 64],
    pub heights: [u16; 64],
}

impl TilesAv1 {
    pub const fn zeroed() -> TilesAv1 {
        TilesAv1 {
            cols: 0,
            rows: 0,
            context_update_id: 0,
            widths: [0; 64],
            heights: [0; 64],
        }
    }
}

/// The loop-filter block.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopFilterAv1 {
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub sharpness_level: u8,
    pub control_flags: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub delta_lf_res: u8,
    pub frame_restoration_type: [u8; 3],
    pub log2_restoration_unit_size: [u16; 3],
    pub reserved16: u16,
}

impl LoopFilterAv1 {
    pub const fn zeroed() -> LoopFilterAv1 {
        LoopFilterAv1 {
            filter_level: [0; 2],
            filter_level_u: 0,
            filter_level_v: 0,
            sharpness_level: 0,
            control_flags: 0,
            ref_deltas: [0; 8],
            mode_deltas: [0; 2],
            delta_lf_res: 0,
            frame_restoration_type: [0; 3],
            log2_restoration_unit_size: [0; 3],
            reserved16: 0,
        }
    }
}

/// `loop_filter.ControlFlags`' members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopFilterFlagsAv1 {
    pub mode_ref_delta_enabled: bool,
    pub mode_ref_delta_update: bool,
    pub delta_lf_multi: bool,
    pub delta_lf_present: bool,
}

impl LoopFilterFlagsAv1 {
    pub const fn pack(self) -> u8 {
        (self.mode_ref_delta_enabled as u8)
            | ((self.mode_ref_delta_update as u8) << 1)
            | ((self.delta_lf_multi as u8) << 2)
            | ((self.delta_lf_present as u8) << 3)
    }
}

/// The quantisation block.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizationAv1 {
    pub control_flags: u8,
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
    pub reserved16: u16,
}

impl QuantizationAv1 {
    pub const fn zeroed() -> QuantizationAv1 {
        QuantizationAv1 {
            control_flags: 0,
            base_qindex: 0,
            y_dc_delta_q: 0,
            u_dc_delta_q: 0,
            v_dc_delta_q: 0,
            u_ac_delta_q: 0,
            v_ac_delta_q: 0,
            qm_y: 0,
            qm_u: 0,
            qm_v: 0,
            reserved16: 0,
        }
    }
}

/// `quantization.ControlFlags`' members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuantizationFlagsAv1 {
    pub delta_q_present: bool,
    /// Two bits.
    pub delta_q_res: u8,
}

impl QuantizationFlagsAv1 {
    pub const fn pack(self) -> u8 {
        (self.delta_q_present as u8) | ((self.delta_q_res & 0x3) << 1)
    }
}

/// The CDEF block. ⚠ Its strengths are packed two fields to a byte — see
/// [`CdefStrength`] and the module docs.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CdefAv1 {
    pub control_flags: u8,
    pub y_strengths: [u8; 8],
    pub uv_strengths: [u8; 8],
}

impl CdefAv1 {
    pub const fn zeroed() -> CdefAv1 {
        CdefAv1 {
            control_flags: 0,
            y_strengths: [0; 8],
            uv_strengths: [0; 8],
        }
    }
}

/// `cdef.ControlFlags`' members: `damping` in bits 0-1, `bits` in 2-3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CdefFlagsAv1 {
    /// `cdef_damping_minus_3`, two bits.
    pub damping: u8,
    /// `cdef_bits`, two bits.
    pub bits: u8,
}

impl CdefFlagsAv1 {
    pub const fn pack(self) -> u8 {
        (self.damping & 0x3) | ((self.bits & 0x3) << 2)
    }
}

/// One packed CDEF strength byte: `primary` in the low SIX bits, `secondary` in
/// the top two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CdefStrength {
    pub primary: u8,
    pub secondary: u8,
}

impl CdefStrength {
    pub const fn pack(self) -> u8 {
        (self.primary & 0x3F) | ((self.secondary & 0x3) << 6)
    }
}

/// The segmentation block.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentationAv1 {
    pub control_flags: u8,
    pub reserved24: [u8; 3],
    /// One packed mask per segment — see [`SegmentFeatureMask`].
    pub feature_mask: [u8; 8],
    pub feature_data: [[i16; 8]; 8],
}

impl SegmentationAv1 {
    pub const fn zeroed() -> SegmentationAv1 {
        SegmentationAv1 {
            control_flags: 0,
            reserved24: [0; 3],
            feature_mask: [0; 8],
            feature_data: [[0; 8]; 8],
        }
    }
}

/// `segmentation.ControlFlags`' members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentationFlagsAv1 {
    pub enabled: bool,
    pub update_map: bool,
    pub update_data: bool,
    pub temporal_update: bool,
}

impl SegmentationFlagsAv1 {
    pub const fn pack(self) -> u8 {
        (self.enabled as u8)
            | ((self.update_map as u8) << 1)
            | ((self.update_data as u8) << 2)
            | ((self.temporal_update as u8) << 3)
    }
}

/// One segment's feature mask. The bit ORDER is the AV1 `SEG_LVL_*` order, which
/// is also the order the parser's `feature_enabled[segment]` is indexed by — so a
/// conversion can shift by feature index directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentFeatureMask {
    pub alt_q: bool,
    pub alt_lf_y_v: bool,
    pub alt_lf_y_h: bool,
    pub alt_lf_u: bool,
    pub alt_lf_v: bool,
    pub ref_frame: bool,
    pub skip: bool,
    pub globalmv: bool,
}

impl SegmentFeatureMask {
    pub const fn pack(self) -> u8 {
        (self.alt_q as u8)
            | ((self.alt_lf_y_v as u8) << 1)
            | ((self.alt_lf_y_h as u8) << 2)
            | ((self.alt_lf_u as u8) << 3)
            | ((self.alt_lf_v as u8) << 4)
            | ((self.ref_frame as u8) << 5)
            | ((self.skip as u8) << 6)
            | ((self.globalmv as u8) << 7)
    }
}

/// The film-grain block. ⚠ Its scaling points are `[value, scaling]` PAIRS, where
/// AV1's syntax and Vulkan's Std block keep two parallel arrays.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilmGrainAv1 {
    pub control_flags: u16,
    pub grain_seed: u16,
    pub scaling_points_y: [[u8; 2]; 14],
    pub num_y_points: u8,
    pub scaling_points_cb: [[u8; 2]; 10],
    pub num_cb_points: u8,
    pub scaling_points_cr: [[u8; 2]; 10],
    pub num_cr_points: u8,
    pub ar_coeffs_y: [u8; 24],
    pub ar_coeffs_cb: [u8; 25],
    pub ar_coeffs_cr: [u8; 25],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub reserved8: u8,
    pub cb_offset: i16,
    pub cr_offset: i16,
}

impl FilmGrainAv1 {
    pub const fn zeroed() -> FilmGrainAv1 {
        FilmGrainAv1 {
            control_flags: 0,
            grain_seed: 0,
            scaling_points_y: [[0; 2]; 14],
            num_y_points: 0,
            scaling_points_cb: [[0; 2]; 10],
            num_cb_points: 0,
            scaling_points_cr: [[0; 2]; 10],
            num_cr_points: 0,
            ar_coeffs_y: [0; 24],
            ar_coeffs_cb: [0; 25],
            ar_coeffs_cr: [0; 25],
            cb_mult: 0,
            cb_luma_mult: 0,
            cr_mult: 0,
            cr_luma_mult: 0,
            reserved8: 0,
            cb_offset: 0,
            cr_offset: 0,
        }
    }
}

/// `film_grain.ControlFlags`' members — a SIXTEEN-bit word, unlike every other
/// control word here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilmGrainFlagsAv1 {
    pub apply_grain: bool,
    /// Two bits.
    pub scaling_shift_minus8: u8,
    pub chroma_scaling_from_luma: bool,
    /// Two bits.
    pub ar_coeff_lag: u8,
    /// Two bits.
    pub ar_coeff_shift_minus6: u8,
    /// Two bits.
    pub grain_scale_shift: u8,
    pub overlap_flag: bool,
    pub clip_to_restricted_range: bool,
    pub matrix_coeff_is_identity: bool,
}

impl FilmGrainFlagsAv1 {
    pub const fn pack(self) -> u16 {
        (self.apply_grain as u16)
            | (((self.scaling_shift_minus8 & 0x3) as u16) << 1)
            | ((self.chroma_scaling_from_luma as u16) << 3)
            | (((self.ar_coeff_lag & 0x3) as u16) << 4)
            | (((self.ar_coeff_shift_minus6 & 0x3) as u16) << 6)
            | (((self.grain_scale_shift & 0x3) as u16) << 8)
            | ((self.overlap_flag as u16) << 10)
            | ((self.clip_to_restricted_range as u16) << 11)
            | ((self.matrix_coeff_is_identity as u16) << 12)
    }
}

/// `DXVA_PicParams_AV1` — 912 bytes, packed (module docs).
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PicParamsAv1 {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub curr_pic_texture_index: u8,
    pub superres_denom: u8,
    pub bitdepth: u8,
    pub seq_profile: u8,
    pub tiles: TilesAv1,
    /// `CodingParamToolFlags` — see [`CodingFlagsAv1`].
    pub coding: u32,
    /// `FormatAndPictureInfoFlags` — see [`FormatFlagsAv1`].
    pub format: u8,
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub order_hint_bits: u8,
    /// The seven reference NAMES (`LAST`..`ALTREF`), each with its own global
    /// motion (module docs).
    pub frame_refs: [PicEntryAv1; 7],
    /// The eight reference SLOTS, as surface indices — DXVA's statement about the
    /// whole reference store, the counterpart of `RefFrameList` on the other
    /// codecs. [`UNUSED_INDEX`] for an empty slot.
    pub ref_frame_map_texture_index: [u8; 8],
    pub loop_filter: LoopFilterAv1,
    pub quantization: QuantizationAv1,
    pub cdef: CdefAv1,
    pub interp_filter: u8,
    pub segmentation: SegmentationAv1,
    pub film_grain: FilmGrainAv1,
    pub reserved32: u32,
    pub status_report_feedback_number: u32,
}

impl PicParamsAv1 {
    pub const fn zeroed() -> PicParamsAv1 {
        PicParamsAv1 {
            width: 0,
            height: 0,
            max_width: 0,
            max_height: 0,
            curr_pic_texture_index: 0,
            superres_denom: 0,
            bitdepth: 0,
            seq_profile: 0,
            tiles: TilesAv1::zeroed(),
            coding: 0,
            format: 0,
            primary_ref_frame: 0,
            order_hint: 0,
            order_hint_bits: 0,
            frame_refs: [PicEntryAv1::zeroed(); 7],
            ref_frame_map_texture_index: [UNUSED_INDEX; 8],
            loop_filter: LoopFilterAv1::zeroed(),
            quantization: QuantizationAv1::zeroed(),
            cdef: CdefAv1::zeroed(),
            interp_filter: 0,
            segmentation: SegmentationAv1::zeroed(),
            film_grain: FilmGrainAv1::zeroed(),
            reserved32: 0,
            status_report_feedback_number: 0,
        }
    }
}

/// `CodingParamToolFlags`' members, in declaration order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodingFlagsAv1 {
    pub use_128x128_superblock: bool,
    pub intra_edge_filter: bool,
    pub interintra_compound: bool,
    pub masked_compound: bool,
    pub warped_motion: bool,
    pub dual_filter: bool,
    pub jnt_comp: bool,
    pub screen_content_tools: bool,
    pub integer_mv: bool,
    pub cdef: bool,
    pub restoration: bool,
    pub film_grain: bool,
    pub intrabc: bool,
    pub high_precision_mv: bool,
    pub switchable_motion_mode: bool,
    pub filter_intra: bool,
    pub disable_frame_end_update_cdf: bool,
    pub disable_cdf_update: bool,
    pub reference_mode: bool,
    pub skip_mode: bool,
    pub reduced_tx_set: bool,
    pub superres: bool,
    /// Two bits.
    pub tx_mode: u8,
    pub use_ref_frame_mvs: bool,
    pub enable_ref_frame_mvs: bool,
    pub reference_frame_update: bool,
}

impl CodingFlagsAv1 {
    pub const fn pack(self) -> u32 {
        (self.use_128x128_superblock as u32)
            | ((self.intra_edge_filter as u32) << 1)
            | ((self.interintra_compound as u32) << 2)
            | ((self.masked_compound as u32) << 3)
            | ((self.warped_motion as u32) << 4)
            | ((self.dual_filter as u32) << 5)
            | ((self.jnt_comp as u32) << 6)
            | ((self.screen_content_tools as u32) << 7)
            | ((self.integer_mv as u32) << 8)
            | ((self.cdef as u32) << 9)
            | ((self.restoration as u32) << 10)
            | ((self.film_grain as u32) << 11)
            | ((self.intrabc as u32) << 12)
            | ((self.high_precision_mv as u32) << 13)
            | ((self.switchable_motion_mode as u32) << 14)
            | ((self.filter_intra as u32) << 15)
            | ((self.disable_frame_end_update_cdf as u32) << 16)
            | ((self.disable_cdf_update as u32) << 17)
            | ((self.reference_mode as u32) << 18)
            | ((self.skip_mode as u32) << 19)
            | ((self.reduced_tx_set as u32) << 20)
            | ((self.superres as u32) << 21)
            | (((self.tx_mode & 0x3) as u32) << 22)
            | ((self.use_ref_frame_mvs as u32) << 24)
            | ((self.enable_ref_frame_mvs as u32) << 25)
            | ((self.reference_frame_update as u32) << 26)
    }
}

/// `FormatAndPictureInfoFlags`' members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatFlagsAv1 {
    /// Two bits: 0 KEY, 1 INTER, 2 INTRA_ONLY, 3 SWITCH.
    pub frame_type: u8,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub mono_chrome: bool,
}

impl FormatFlagsAv1 {
    pub const fn pack(self) -> u8 {
        (self.frame_type & 0x3)
            | ((self.show_frame as u8) << 2)
            | ((self.showable_frame as u8) << 3)
            | ((self.subsampling_x as u8) << 4)
            | ((self.subsampling_y as u8) << 5)
            | ((self.mono_chrome as u8) << 6)
    }
}

/// `DXVA_Tile_AV1` — one tile's location in the bitstream buffer. 16 bytes.
///
/// ONE RECORD PER TILE, not per tile GROUP. `row` and `column` are the tile's
/// position in the frame's tile grid, which only a per-tile record can carry, and
/// libavcodec's `dxva2_av1.c` sizes its array `frame_header->tile_cols *
/// frame_header->tile_rows` and fills it `for (tile_num = h->tg_start; tile_num <=
/// h->tg_end; tile_num++)`. A frame whose four tiles arrive in one tile group is
/// four of these, not one.
///
/// [`Self::data_offset`] and [`Self::data_size`] address that tile's raw payload
/// inside the bitstream buffer: the bytes AFTER its `tile_size_minus_1` field, and
/// not one byte more. See [`mod@crate::pack_av1`] for what the buffer holds around
/// them.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileAv1 {
    pub data_offset: u32,
    pub data_size: u32,
    pub row: u16,
    pub column: u16,
    pub reserved16: u16,
    pub anchor_frame: u8,
    pub reserved8: u8,
}

// The byte-view permission ([`crate::dxva::as_bytes`] / [`crate::dxva::slice_bytes`]),
// for the two structures a submission actually copies into a driver mapping. The
// sealed trait's argument is even simpler here than for the H.264/HEVC buffers:
// `#[repr(C, packed)]` leaves NO padding at all, so "every byte is initialized" is
// a property of the layout rather than of how carefully `zeroed()` was written.
//
// The nested blocks (`TilesAv1`, `LoopFilterAv1`, …) deliberately do NOT implement
// it: they are never submitted on their own, only as members of
// [`PicParamsAv1`].
impl crate::dxva::DxvaBuffer for PicParamsAv1 {}
impl crate::dxva::DxvaBuffer for TileAv1 {}

// Every number below was printed by `layout-probe-av1.c`, compiled with MSVC
// against the Windows SDK's own `dxva.h` (10.0.26100.0) on .173. Not transcribed
// from a specification, and not copied from libavcodec.
const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<PicEntryAv1>() == 36);
    assert!(size_of::<TileAv1>() == 16);
    assert!(size_of::<PicParamsAv1>() == 912);

    assert!(offset_of!(PicParamsAv1, width) == 0);
    assert!(offset_of!(PicParamsAv1, height) == 4);
    assert!(offset_of!(PicParamsAv1, max_width) == 8);
    assert!(offset_of!(PicParamsAv1, max_height) == 12);
    assert!(offset_of!(PicParamsAv1, curr_pic_texture_index) == 16);
    assert!(offset_of!(PicParamsAv1, superres_denom) == 17);
    assert!(offset_of!(PicParamsAv1, bitdepth) == 18);
    assert!(offset_of!(PicParamsAv1, seq_profile) == 19);
    assert!(offset_of!(PicParamsAv1, tiles) == 20);
    assert!(offset_of!(PicParamsAv1, coding) == 280);
    assert!(offset_of!(PicParamsAv1, format) == 284);
    assert!(offset_of!(PicParamsAv1, primary_ref_frame) == 285);
    assert!(offset_of!(PicParamsAv1, order_hint) == 286);
    assert!(offset_of!(PicParamsAv1, order_hint_bits) == 287);
    assert!(offset_of!(PicParamsAv1, frame_refs) == 288);
    assert!(offset_of!(PicParamsAv1, ref_frame_map_texture_index) == 540);
    assert!(offset_of!(PicParamsAv1, loop_filter) == 548);
    assert!(offset_of!(PicParamsAv1, quantization) == 576);
    assert!(offset_of!(PicParamsAv1, cdef) == 588);
    assert!(offset_of!(PicParamsAv1, interp_filter) == 605);
    assert!(offset_of!(PicParamsAv1, segmentation) == 606);
    assert!(offset_of!(PicParamsAv1, film_grain) == 746);
    assert!(offset_of!(PicParamsAv1, reserved32) == 904);
    assert!(offset_of!(PicParamsAv1, status_report_feedback_number) == 908);

    // Nested offsets, measured through the outer struct so a wrong INTERNAL
    // layout cannot hide behind a right outer one.
    assert!(offset_of!(TilesAv1, widths) == 4);
    assert!(offset_of!(TilesAv1, heights) == 132);
    assert!(offset_of!(LoopFilterAv1, filter_level_u) == 2);
    assert!(offset_of!(LoopFilterAv1, sharpness_level) == 4);
    assert!(offset_of!(LoopFilterAv1, control_flags) == 5);
    assert!(offset_of!(LoopFilterAv1, ref_deltas) == 6);
    assert!(offset_of!(LoopFilterAv1, mode_deltas) == 14);
    assert!(offset_of!(LoopFilterAv1, delta_lf_res) == 16);
    assert!(offset_of!(LoopFilterAv1, frame_restoration_type) == 17);
    assert!(offset_of!(LoopFilterAv1, log2_restoration_unit_size) == 20);
    assert!(size_of::<LoopFilterAv1>() == 28);
    assert!(offset_of!(QuantizationAv1, base_qindex) == 1);
    assert!(offset_of!(QuantizationAv1, qm_y) == 7);
    assert!(size_of::<QuantizationAv1>() == 12);
    assert!(offset_of!(CdefAv1, y_strengths) == 1);
    assert!(offset_of!(CdefAv1, uv_strengths) == 9);
    assert!(size_of::<CdefAv1>() == 17);
    assert!(offset_of!(SegmentationAv1, feature_mask) == 4);
    assert!(offset_of!(SegmentationAv1, feature_data) == 12);
    assert!(size_of::<SegmentationAv1>() == 140);
    assert!(offset_of!(FilmGrainAv1, grain_seed) == 2);
    assert!(offset_of!(FilmGrainAv1, scaling_points_y) == 4);
    assert!(offset_of!(FilmGrainAv1, num_y_points) == 32);
    assert!(offset_of!(FilmGrainAv1, scaling_points_cb) == 33);
    assert!(offset_of!(FilmGrainAv1, num_cb_points) == 53);
    assert!(offset_of!(FilmGrainAv1, scaling_points_cr) == 54);
    assert!(offset_of!(FilmGrainAv1, num_cr_points) == 74);
    assert!(offset_of!(FilmGrainAv1, ar_coeffs_y) == 75);
    assert!(offset_of!(FilmGrainAv1, ar_coeffs_cb) == 99);
    assert!(offset_of!(FilmGrainAv1, ar_coeffs_cr) == 124);
    assert!(offset_of!(FilmGrainAv1, cb_mult) == 149);
    assert!(offset_of!(FilmGrainAv1, cb_offset) == 154);
    assert!(offset_of!(FilmGrainAv1, cr_offset) == 156);
    assert!(size_of::<FilmGrainAv1>() == 158);
    assert!(offset_of!(PicEntryAv1, wmmat) == 8);
    assert!(offset_of!(PicEntryAv1, global_motion_flags) == 32);
    assert!(offset_of!(PicEntryAv1, index) == 33);
    assert!(offset_of!(TileAv1, data_size) == 4);
    assert!(offset_of!(TileAv1, row) == 8);
    assert!(offset_of!(TileAv1, column) == 10);
    assert!(offset_of!(TileAv1, anchor_frame) == 14);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every packed word, checked against the value MSVC produced for the same
    /// single-field assignment. These are the probe's own printed numbers — the
    /// point being that C bit-field order is ABI-defined, so the only honest way
    /// to know where `tx_mode` lands is to have asked the compiler that the
    /// driver agrees with.
    #[test]
    fn packed_words_match_what_msvc_measured() {
        assert_eq!(
            CodingFlagsAv1 {
                use_128x128_superblock: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0001
        );
        assert_eq!(
            CodingFlagsAv1 {
                tx_mode: 3,
                ..Default::default()
            }
            .pack(),
            0x00c0_0000
        );
        assert_eq!(
            CodingFlagsAv1 {
                reference_frame_update: true,
                ..Default::default()
            }
            .pack(),
            0x0400_0000
        );
        assert_eq!(
            FormatFlagsAv1 {
                frame_type: 3,
                ..Default::default()
            }
            .pack(),
            0x03
        );
        assert_eq!(
            FormatFlagsAv1 {
                mono_chrome: true,
                ..Default::default()
            }
            .pack(),
            0x40
        );
        assert_eq!(
            LoopFilterFlagsAv1 {
                delta_lf_present: true,
                ..Default::default()
            }
            .pack(),
            0x08
        );
        assert_eq!(
            QuantizationFlagsAv1 {
                delta_q_res: 3,
                ..Default::default()
            }
            .pack(),
            0x06
        );
        assert_eq!(
            CdefFlagsAv1 {
                bits: 3,
                ..Default::default()
            }
            .pack(),
            0x0c
        );
        assert_eq!(
            CdefStrength {
                secondary: 3,
                ..Default::default()
            }
            .pack(),
            0xc0
        );
        assert_eq!(
            SegmentationFlagsAv1 {
                temporal_update: true,
                ..Default::default()
            }
            .pack(),
            0x08
        );
        assert_eq!(
            SegmentFeatureMask {
                globalmv: true,
                ..Default::default()
            }
            .pack(),
            0x80
        );
        assert_eq!(
            FilmGrainFlagsAv1 {
                ar_coeff_shift_minus6: 3,
                ..Default::default()
            }
            .pack(),
            0x00c0
        );
        assert_eq!(
            FilmGrainFlagsAv1 {
                matrix_coeff_is_identity: true,
                ..Default::default()
            }
            .pack(),
            0x1000
        );
    }

    /// A zeroed picture-parameters block must name NO references. `0` is a valid
    /// surface index, so a memset-style default would quietly point every unused
    /// reference at surface 0 — which decodes, and decodes wrong.
    #[test]
    fn a_zeroed_block_names_no_reference() {
        let p = PicParamsAv1::zeroed();
        assert!(p
            .ref_frame_map_texture_index
            .iter()
            .all(|i| *i == UNUSED_INDEX));
        assert!(p.frame_refs.iter().all(|r| r.index == UNUSED_INDEX));
    }
}
