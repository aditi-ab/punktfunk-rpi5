//! The libva decode buffer layouts for AV1, hand-declared — the third sibling of
//! [`crate::va`] and [`crate::va_h265`], measured the same way and pinned the same
//! way.
//!
//! Sizes and offsets come from the committed `layout-probe.c` run against libva
//! **2.23.0** headers (`va_dec_av1.h`, x86_64-linux-gnu):
//! `VASegmentationStructAV1` **156**, `VAFilmGrainStructAV1` **176**,
//! `VAWarpedMotionParamsAV1` **56**, `VADecPictureParameterBufferAV1` **1160**
//! (align **8**), `VASliceParameterBufferAV1` **40**. Every bit position below was
//! read back off a real header one field at a time, not counted by eye — which
//! matters more here than for the other two codecs, because **three of AV1's six
//! bit-field unions are NARROWER than a word**: `loop_filter_info_fields` is a
//! `uint8_t`, `qmatrix_fields` and `loop_restoration_fields` are `uint16_t`. A
//! `u32` packer over any of them would write straight through the neighbouring
//! field, and on the two `uint16_t` ones that neighbour is padding on one side and
//! `mode_control_fields` / `wm[0]` on the other.
//!
//! # AV1's reference plumbing is a FIFTH convention
//!
//! This program has now written down five spellings of "which pictures does this
//! frame use", and they are not interchangeable:
//!
//! * **Vulkan H.265** — DPB *slot* indices in `RefPicSetStCurr*`;
//! * **DXVA H.265** — positions into `RefPicList[]` in identically named arrays;
//! * **VAAPI H.265** — membership *flags* ORed onto each DPB entry, with per-slice
//!   lists indexing `ReferenceFrames`;
//! * **DXVA AV1** — `frame_refs[7]` by reference NAME, each entry carrying a
//!   reference SLOT that indexes `RefFrameMapTextureIndex[8]`, plus that
//!   reference's own size and own global motion;
//! * **VAAPI AV1** — [`VaDecPictureParameterBufferAV1::ref_frame_map`] is indexed
//!   by AV1 reference **SLOT** (0..8) and holds a **`VASurfaceID`** — an actual
//!   surface handle, not an index into anything — while
//!   [`VaDecPictureParameterBufferAV1::ref_frame_idx`] is indexed by reference
//!   **NAME** and holds *"a list of indices into `ref_frame_map[8]`"*, i.e. the
//!   slot. Global motion is a **picture-level** array
//!   ([`VaDecPictureParameterBufferAV1::wm`], seven entries, `wm[0]` = `LAST_FRAME`)
//!   and NOT part of a reference entry, and **there is no per-reference size
//!   anywhere in this structure at all**.
//!
//! Both halves of that last sentence are measured rather than assumed:
//! `grep -c ref_frame_width /usr/include/va/va_dec_av1.h` is **0** on libva 2.23.0,
//! so a VAAPI driver takes each reference's dimensions from the SURFACE it was
//! decoded into. (`pf_bitstream::av1::RefState::upscaled_width`'s doc comment says
//! VA-API has `ref_frame_width`/`ref_frame_height`; it does not, and the field is
//! still load-bearing for DXVA, which does.) The two statements that DO reach a
//! VAAPI driver — the slot table and the name table — come from `va_dec_av1.h`'s
//! own comments and from libavcodec's `vaapi_av1.c`:
//!
//! ```text
//! pic_param.ref_frame_map[i] = <VASurfaceID of ref_tab[i]>        for i in 0..8
//! pic_param.ref_frame_idx[i] = frame_header->ref_frame_idx[i]     for i in 0..7
//! pic_param.wm[i - 1]        = <global motion of reference name i>
//!                                                for i in LAST_FRAME..=ALTREF_FRAME
//! ```
//!
//! # Where libva's AV1 buffers differ from every other codec here
//!
//! * **The "slice" parameter buffer is a TILE parameter buffer.** The header says so
//!   in as many words: *"It uses the name VASliceParameterBufferAV1 to be consistent
//!   with other codec, but actually means VATileParameterBufferAV1."* One record per
//!   TILE, not per tile group.
//! * **Several records share one data buffer.** libavcodec's `vaapi_av1.c` calls
//!   `ff_vaapi_decode_make_slice_buffer` once per tile-group OBU with `nb_params =
//!   tg_end - tg_start + 1`, so one `VASliceParameterBufferType` buffer carries
//!   `nb_params` ELEMENTS beside one `VASliceDataBufferType` buffer holding the whole
//!   group's `tile_data` region, and each record's `slice_data_offset` is relative to
//!   THAT buffer. H.264 and H.265 send one record per buffer, so this is the only
//!   place `vaCreateBuffer`'s `num_elements` is not 1.
//! * **There is no IQ matrix buffer.** AV1's quantiser matrices are SELECTED by
//!   index out of tables the decoder already holds
//!   ([`QmatrixFieldsAV1`]), so a submission is picture parameters plus tile
//!   pairs and nothing else.

/// `VASliceParameterBufferAV1::anchor_frame_idx` on an ordinary frame.
///
/// `anchor_frame_idx` selects a reference for LARGE-SCALE TILE decoding, which no
/// punktfunk stream and no conformance vector here uses; libavcodec leaves the whole
/// record zero-initialised and never writes the field.
pub const ANCHOR_FRAME_UNUSED: u8 = 0;

/// `PRIMARY_REF_NONE` (AV1 spec 6.8.2): `primary_ref_frame` meaning "this frame
/// loads no propagated state".
pub const PRIMARY_REF_NONE: u8 = 7;

/// `SUPERRES_NUM` (AV1 spec): the `superres_scale_denominator` that means "no
/// upscaling". libva documents the field as 8 when `use_superres` is 0 and 9..=16
/// when it is 1 — so a frame without superres does NOT send 0 here.
pub const SUPERRES_NUM: u8 = 8;

/// `VAAV1TransformationType` (measured: 0, 1, 2, 3). The same numbering AV1 5.9.24
/// gives `GmType`, and the same the vendored parser's `WarpModelType` uses — so the
/// conversion casts rather than remaps, and this table is here to make that
/// checkable.
pub const VA_AV1_TRANSFORMATION_IDENTITY: u32 = 0;
pub const VA_AV1_TRANSFORMATION_TRANSLATION: u32 = 1;
pub const VA_AV1_TRANSFORMATION_ROTZOOM: u32 = 2;
pub const VA_AV1_TRANSFORMATION_AFFINE: u32 = 3;

/// `ref_frame_map[8]` — AV1's `NUM_REF_FRAMES`.
pub const REF_FRAME_MAP_LEN: usize = 8;

/// `ref_frame_idx[7]` / `wm[7]` — AV1's `REFS_PER_FRAME`.
pub const REFS_PER_FRAME: usize = 7;

/// `ref_deltas[8]` — AV1's `TOTAL_REFS_PER_FRAME`.
pub const TOTAL_REFS_PER_FRAME: usize = 8;

/// `cdef_y_strengths[8]` / `cdef_uv_strengths[8]` — as many as `cdef_bits` can
/// select (`1 << 3`).
pub const CDEF_MAX: usize = 8;

/// `width_in_sbs_minus_1[63]` / `height_in_sbs_minus_1[63]`.
///
/// ⚠ **63, not 64**, and the header explains why: *"Though the maximum number of
/// tiles is 64, since ones of the last tile are computed from ones of the other
/// tiles and frame_width/height, they are not necessarily specified."* libavcodec's
/// `vaapi_av1.c` nonetheless loops `for (i = 0; i < frame_header->tile_cols; i++)`,
/// which writes index 63 — one past the end — on a 64-column frame. The conversion
/// clamps instead; see [`crate::pic_av1`].
pub const TILE_SBS_LEN: usize = 63;

/// `wm[7]`'s index for a reference NAME.
///
/// libavcodec writes `pic_param.wm[i - 1]` for `i = LAST_FRAME..=ALTREF_FRAME`, so
/// `wm[0]` is `LAST_FRAME` and `wm[6]` is `ALTREF_FRAME` — the same indexing
/// `pf_bitstream::av1::AuPlan::refs` uses, and one step off the parser's own
/// `gm_params[]`, which is indexed by the spec's reference index (`INTRA_FRAME` = 0).
pub const LAST_FRAME: usize = 1;

// ---------------------------------------------------------------------------
// The bit-field unions, unpacked
// ---------------------------------------------------------------------------

/// `VADecPictureParameterBufferAV1::seq_info_fields` (32 bits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqInfoFieldsAV1 {
    pub still_picture: bool,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_interintra_compound: bool,
    pub enable_masked_compound: bool,
    pub enable_dual_filter: bool,
    pub enable_order_hint: bool,
    pub enable_jnt_comp: bool,
    pub enable_cdef: bool,
    pub mono_chrome: bool,
    pub color_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    /// `va_deprecated` in the header, and still part of the layout — a field that is
    /// deprecated is not a field that moved.
    ///
    /// ⚠ **ONE bit**, where AV1's `chroma_sample_position` is a two-bit enumerator
    /// (UNKNOWN 0, VERTICAL 1, COLOCATED 2). [`Self::pack`] masks, which is exactly
    /// what libavcodec's assignment into the C bit-field does — so COLOCATED reaches a
    /// driver as UNKNOWN through both paths. Not a defect this rung can fix: there is
    /// no second bit to put it in, and the field is deprecated precisely because
    /// drivers do not read it.
    pub chroma_sample_position: u8,
    pub film_grain_params_present: bool,
}

impl SeqInfoFieldsAV1 {
    pub const fn pack(self) -> u32 {
        (self.still_picture as u32)
            | ((self.use_128x128_superblock as u32) << 1)
            | ((self.enable_filter_intra as u32) << 2)
            | ((self.enable_intra_edge_filter as u32) << 3)
            | ((self.enable_interintra_compound as u32) << 4)
            | ((self.enable_masked_compound as u32) << 5)
            | ((self.enable_dual_filter as u32) << 6)
            | ((self.enable_order_hint as u32) << 7)
            | ((self.enable_jnt_comp as u32) << 8)
            | ((self.enable_cdef as u32) << 9)
            | ((self.mono_chrome as u32) << 10)
            | ((self.color_range as u32) << 11)
            | ((self.subsampling_x as u32) << 12)
            | ((self.subsampling_y as u32) << 13)
            | ((self.chroma_sample_position as u32 & 0x1) << 14)
            | ((self.film_grain_params_present as u32) << 15)
    }
}

/// `VADecPictureParameterBufferAV1::pic_info_fields` (32 bits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PicInfoFieldsAV1 {
    /// 0 KEY, 1 INTER, 2 INTRA_ONLY, 3 SWITCH — the AV1 spec's own numbering, which
    /// is the vendored parser's `FrameType` discriminant too.
    pub frame_type: u8,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub error_resilient_mode: bool,
    pub disable_cdf_update: bool,
    pub allow_screen_content_tools: bool,
    pub force_integer_mv: bool,
    pub allow_intrabc: bool,
    pub use_superres: bool,
    pub allow_high_precision_mv: bool,
    pub is_motion_mode_switchable: bool,
    pub use_ref_frame_mvs: bool,
    pub disable_frame_end_update_cdf: bool,
    pub uniform_tile_spacing_flag: bool,
    pub allow_warped_motion: bool,
    /// Large-scale tile decoding — outside this rung's envelope, always false.
    pub large_scale_tile: bool,
}

impl PicInfoFieldsAV1 {
    pub const fn pack(self) -> u32 {
        (self.frame_type as u32 & 0x3)
            | ((self.show_frame as u32) << 2)
            | ((self.showable_frame as u32) << 3)
            | ((self.error_resilient_mode as u32) << 4)
            | ((self.disable_cdf_update as u32) << 5)
            | ((self.allow_screen_content_tools as u32) << 6)
            | ((self.force_integer_mv as u32) << 7)
            | ((self.allow_intrabc as u32) << 8)
            | ((self.use_superres as u32) << 9)
            | ((self.allow_high_precision_mv as u32) << 10)
            | ((self.is_motion_mode_switchable as u32) << 11)
            | ((self.use_ref_frame_mvs as u32) << 12)
            | ((self.disable_frame_end_update_cdf as u32) << 13)
            | ((self.uniform_tile_spacing_flag as u32) << 14)
            | ((self.allow_warped_motion as u32) << 15)
            | ((self.large_scale_tile as u32) << 16)
    }
}

/// `VADecPictureParameterBufferAV1::loop_filter_info_fields` — **8 bits**, not 32.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoopFilterInfoFieldsAV1 {
    pub sharpness_level: u8,
    pub mode_ref_delta_enabled: bool,
    pub mode_ref_delta_update: bool,
}

impl LoopFilterInfoFieldsAV1 {
    pub const fn pack(self) -> u8 {
        (self.sharpness_level & 0x7)
            | ((self.mode_ref_delta_enabled as u8) << 3)
            | ((self.mode_ref_delta_update as u8) << 4)
    }
}

/// `VADecPictureParameterBufferAV1::qmatrix_fields` — **16 bits**, not 32.
///
/// Unlike DXVA, libva carries `using_qmatrix` itself, so the three indices need no
/// `0xFF` sentinel: they are simply ignored when the flag is clear. (`DXVA_PicParams_AV1`
/// has no such flag, which is why `pf_dxvadec::pic_av1` has to send 0xFF and why
/// leaving the parser's 0 there dequantised against matrix 0 on every frame.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QmatrixFieldsAV1 {
    pub using_qmatrix: bool,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
}

impl QmatrixFieldsAV1 {
    pub const fn pack(self) -> u16 {
        (self.using_qmatrix as u16)
            | ((self.qm_y as u16 & 0xf) << 1)
            | ((self.qm_u as u16 & 0xf) << 5)
            | ((self.qm_v as u16 & 0xf) << 9)
    }
}

/// `VADecPictureParameterBufferAV1::mode_control_fields` (32 bits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeControlFieldsAV1 {
    pub delta_q_present_flag: bool,
    pub log2_delta_q_res: u8,
    pub delta_lf_present_flag: bool,
    pub log2_delta_lf_res: u8,
    pub delta_lf_multi: bool,
    /// 0 ONLY_4X4, 1 LARGEST, 2 SELECT.
    pub tx_mode: u8,
    pub reference_select: bool,
    pub reduced_tx_set_used: bool,
    pub skip_mode_present: bool,
}

impl ModeControlFieldsAV1 {
    pub const fn pack(self) -> u32 {
        (self.delta_q_present_flag as u32)
            | ((self.log2_delta_q_res as u32 & 0x3) << 1)
            | ((self.delta_lf_present_flag as u32) << 3)
            | ((self.log2_delta_lf_res as u32 & 0x3) << 4)
            | ((self.delta_lf_multi as u32) << 6)
            | ((self.tx_mode as u32 & 0x3) << 7)
            | ((self.reference_select as u32) << 9)
            | ((self.reduced_tx_set_used as u32) << 10)
            | ((self.skip_mode_present as u32) << 11)
    }
}

/// `VADecPictureParameterBufferAV1::loop_restoration_fields` — **16 bits**, not 32.
///
/// The three `*frame_restoration_type` fields take the SPEC's `FrameRestorationType`
/// (`RESTORE_NONE` 0, `RESTORE_WIENER` 1, `RESTORE_SGRPROJ` 2, `RESTORE_SWITCHABLE`
/// 3), not the coded two-bit `lr_type`. libavcodec sends
/// `remap_lr_type[frame_header->lr_type[i]]` with
/// `remap_lr_type = {NONE, SWITCHABLE, WIENER, SGRPROJ}` — i.e. it applies AV1
/// 5.9.20's `Remap_Lr_Type` mapping — and the vendored parser has already applied it
/// (`LoopRestorationParams::frame_restoration_type` is documented "Same as
/// FrameRestorationType in the specification"), so the conversion casts the parser's
/// enum and remaps nothing. Sending the coded value instead swaps WIENER and
/// SWITCHABLE on every frame that restores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoopRestorationFieldsAV1 {
    pub yframe_restoration_type: u8,
    pub cbframe_restoration_type: u8,
    pub crframe_restoration_type: u8,
    pub lr_unit_shift: u8,
    pub lr_uv_shift: u8,
}

impl LoopRestorationFieldsAV1 {
    pub const fn pack(self) -> u16 {
        (self.yframe_restoration_type as u16 & 0x3)
            | ((self.cbframe_restoration_type as u16 & 0x3) << 2)
            | ((self.crframe_restoration_type as u16 & 0x3) << 4)
            | ((self.lr_unit_shift as u16 & 0x3) << 6)
            | ((self.lr_uv_shift as u16 & 0x1) << 8)
    }
}

/// `VASegmentationStructAV1::segment_info_fields` (32 bits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmentInfoFieldsAV1 {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    pub update_data: bool,
}

impl SegmentInfoFieldsAV1 {
    pub const fn pack(self) -> u32 {
        (self.enabled as u32)
            | ((self.update_map as u32) << 1)
            | ((self.temporal_update as u32) << 2)
            | ((self.update_data as u32) << 3)
    }
}

/// `VAFilmGrainStructAV1::film_grain_info_fields` (32 bits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilmGrainInfoFieldsAV1 {
    pub apply_grain: bool,
    pub chroma_scaling_from_luma: bool,
    pub grain_scaling_minus_8: u8,
    pub ar_coeff_lag: u8,
    pub ar_coeff_shift_minus_6: u8,
    pub grain_scale_shift: u8,
    pub overlap_flag: bool,
    pub clip_to_restricted_range: bool,
}

impl FilmGrainInfoFieldsAV1 {
    pub const fn pack(self) -> u32 {
        (self.apply_grain as u32)
            | ((self.chroma_scaling_from_luma as u32) << 1)
            | ((self.grain_scaling_minus_8 as u32 & 0x3) << 2)
            | ((self.ar_coeff_lag as u32 & 0x3) << 4)
            | ((self.ar_coeff_shift_minus_6 as u32 & 0x3) << 6)
            | ((self.grain_scale_shift as u32 & 0x3) << 8)
            | ((self.overlap_flag as u32) << 10)
            | ((self.clip_to_restricted_range as u32) << 11)
    }
}

// ---------------------------------------------------------------------------
// The structures
// ---------------------------------------------------------------------------

/// `VASegmentationStructAV1`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSegmentationStructAV1 {
    pub segment_info_fields: u32,
    /// `FeatureData[segment][feature]` **after** AV1 5.9.14's `Clip3` — libva says
    /// so ("equivalent to variable FeatureData\[\]\[\] in spec, which is after
    /// clip3() operation"), and the vendored parser clips as it reads
    /// (`parse_segmentation_params` calls `helpers::clip3` with the spec's
    /// `FEATURE_MAX`), so no clipping happens in the conversion.
    pub feature_data: [[i16; 8]; 8],
    /// Bit `feature` set where `feature_enabled[segment][feature]` is. Indexed by
    /// SEGMENT; the bit position is the feature id.
    pub feature_mask: [u8; 8],
    pub va_reserved: [u32; 4],
}

impl VaSegmentationStructAV1 {
    pub const fn zeroed() -> Self {
        VaSegmentationStructAV1 {
            segment_info_fields: 0,
            feature_data: [[0; 8]; 8],
            feature_mask: [0; 8],
            va_reserved: [0; 4],
        }
    }
}

/// `VAFilmGrainStructAV1`.
///
/// ⚠ The `ar_coeffs_*` are **signed** here (`int8_t`), where the bitstream — and the
/// vendored parser, and `DXVA_FilmGrain_AV1` — carry the `+128` biased form.
/// libavcodec writes `film_grain->ar_coeffs_y_plus_128[i] - 128`. Copying the biased
/// bytes across unchanged is a silent 128-offset on every autoregressive coefficient.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaFilmGrainStructAV1 {
    pub film_grain_info_fields: u32,
    pub grain_seed: u16,
    pub num_y_points: u8,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub num_cb_points: u8,
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub num_cr_points: u8,
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub ar_coeffs_y: [i8; 24],
    pub ar_coeffs_cb: [i8; 25],
    pub ar_coeffs_cr: [i8; 25],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub va_reserved: [u32; 4],
}

impl VaFilmGrainStructAV1 {
    pub const fn zeroed() -> Self {
        VaFilmGrainStructAV1 {
            film_grain_info_fields: 0,
            grain_seed: 0,
            num_y_points: 0,
            point_y_value: [0; 14],
            point_y_scaling: [0; 14],
            num_cb_points: 0,
            point_cb_value: [0; 10],
            point_cb_scaling: [0; 10],
            num_cr_points: 0,
            point_cr_value: [0; 10],
            point_cr_scaling: [0; 10],
            ar_coeffs_y: [0; 24],
            ar_coeffs_cb: [0; 25],
            ar_coeffs_cr: [0; 25],
            cb_mult: 0,
            cb_luma_mult: 0,
            cb_offset: 0,
            cr_mult: 0,
            cr_luma_mult: 0,
            cr_offset: 0,
            va_reserved: [0; 4],
        }
    }
}

/// `VAWarpedMotionParamsAV1` — one reference NAME's global motion.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaWarpedMotionParamsAV1 {
    /// `VAAV1TransformationType`. Declared `u32` because a C enum whose enumerators
    /// are all non-negative is `unsigned int` on this ABI; either signedness is four
    /// bytes and every value written here is 0..=3, so the choice is a naming one.
    pub wmtype: u32,
    /// `gm_params[ref][0..6]`. ⚠ Only the first SIX are meaningful: AV1 5.9.24 codes
    /// six warp parameters and libavcodec copies `for (j = 0; j < 6; j++)`, leaving
    /// `wmmat[6]`/`wmmat[7]` zero.
    pub wmmat: [i32; 8],
    /// The INVERSE of the parser's `warp_valid` (`setup_shear`'s verdict): libva's
    /// field says the affine set is unusable.
    pub invalid: u8,
    pub va_reserved: [u32; 4],
}

impl VaWarpedMotionParamsAV1 {
    pub const fn zeroed() -> Self {
        VaWarpedMotionParamsAV1 {
            wmtype: VA_AV1_TRANSFORMATION_IDENTITY,
            wmmat: [0; 8],
            invalid: 0,
            va_reserved: [0; 4],
        }
    }
}

/// `VADecPictureParameterBufferAV1`.
///
/// ⚠ **Eight-byte aligned, 1160 bytes**, and the reason is
/// [`Self::anchor_frames_list`]: a pointer member drags the whole structure's
/// alignment up and inserts seven bytes of padding after `anchor_frames_num` that
/// nothing in the field list suggests. Measured, not counted.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaDecPictureParameterBufferAV1 {
    /// `seq_profile`: 0, 1 or 2.
    pub profile: u8,
    /// ⚠ The parser types this `i32` and leaves it **-1** when `enable_order_hint`
    /// is 0. Narrowed to a `u8` that would be 255 — a 256-bit order hint — so the
    /// conversion sends 0 there instead.
    pub order_hint_bits_minus_1: u8,
    /// 0 = 8-bit, 1 = 10-bit, 2 = 12-bit. An INDEX, not a depth.
    pub bit_depth_idx: u8,
    pub matrix_coefficients: u8,
    pub seq_info_fields: u32,
    /// The decode target's `VASurfaceID`.
    pub current_frame: u32,
    /// The surface the film-grained picture is written to. libva: *"Valid only when
    /// apply_grain equals 1."* This rung refuses `apply_grain` (see
    /// [`crate::pic_av1`]), so it always equals [`Self::current_frame`].
    pub current_display_picture: u32,
    /// Large-scale tile only; always 0 here.
    pub anchor_frames_num: u8,
    /// Large-scale tile only; always null here. Declared as a real pointer so the
    /// layout follows the target ABI rather than a hard-coded width — which also
    /// means the offsets pinned below are the LP64 ones, as everywhere else in this
    /// crate.
    pub anchor_frames_list: *mut u32,
    /// ⚠ The **upscaled** (post-superres) width minus one — libva: *"Picture
    /// original resolution. If SuperRes is enabled, this is the upscaled
    /// resolution."* libavcodec sends the coded `frame_width_minus_1` syntax
    /// element, which is that same quantity: AV1 5.9.8 reads it into
    /// `UpscaledWidth` and only then divides down into `FrameWidth`.
    pub frame_width_minus1: u16,
    pub frame_height_minus1: u16,
    /// Large-scale tile only.
    pub output_frame_width_in_tiles_minus_1: u16,
    pub output_frame_height_in_tiles_minus_1: u16,
    /// Indexed by AV1 reference **SLOT**, holding a **`VASurfaceID`** (module docs).
    /// `VA_INVALID_ID` for a slot holding nothing — which is the DEFAULT this
    /// structure zeroes to, not what a submission carries: `pic_av1` substitutes a live
    /// surface for every empty entry before the buffer reaches a driver, because
    /// `va_dec_av1.h:352` says the driver will not check the ids and prescribes exactly
    /// that recovery.
    pub ref_frame_map: [u32; REF_FRAME_MAP_LEN],
    /// Indexed by reference **NAME**, holding an index into [`Self::ref_frame_map`]
    /// — i.e. an AV1 slot (module docs).
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    /// Index into [`Self::ref_frame_idx`], or [`PRIMARY_REF_NONE`].
    pub primary_ref_frame: u8,
    /// ⚠ A `u8`, where AV1 allows up to 8 order-hint bits — so the full range fits,
    /// but only just.
    pub order_hint: u8,
    pub seg_info: VaSegmentationStructAV1,
    pub film_grain_info: VaFilmGrainStructAV1,
    pub tile_cols: u8,
    pub tile_rows: u8,
    /// Each tile's width in superblocks MINUS ONE — the coded syntax element, not a
    /// count. (DXVA's `tiles.widths[]` is the count, `+1`; the two APIs disagree and
    /// both are documented.)
    pub width_in_sbs_minus_1: [u16; TILE_SBS_LEN],
    pub height_in_sbs_minus_1: [u16; TILE_SBS_LEN],
    /// Large-scale tile only.
    pub tile_count_minus_1: u16,
    pub context_update_tile_id: u16,
    pub pic_info_fields: u32,
    pub superres_scale_denominator: u8,
    pub interp_filter: u8,
    /// `loop_filter_level[0..2]` — the two LUMA levels.
    pub filter_level: [u8; 2],
    /// `loop_filter_level[2]`.
    pub filter_level_u: u8,
    /// `loop_filter_level[3]`.
    pub filter_level_v: u8,
    /// An **8-bit** union ([`LoopFilterInfoFieldsAV1`]).
    pub loop_filter_info_fields: u8,
    pub ref_deltas: [i8; TOTAL_REFS_PER_FRAME],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub v_ac_delta_q: i8,
    /// A **16-bit** union ([`QmatrixFieldsAV1`]).
    pub qmatrix_fields: u16,
    pub mode_control_fields: u32,
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    /// `(primary << 2) | (secondary & 3)`, per the header's own formula. The
    /// secondary strength must be the CODED two-bit value — see
    /// [`pf_bitstream::av1::coded_cdef_sec_strength`].
    pub cdef_y_strengths: [u8; CDEF_MAX],
    pub cdef_uv_strengths: [u8; CDEF_MAX],
    /// A **16-bit** union ([`LoopRestorationFieldsAV1`]).
    pub loop_restoration_fields: u16,
    /// Global motion by reference NAME: `wm[0]` is `LAST_FRAME` (module docs).
    pub wm: [VaWarpedMotionParamsAV1; REFS_PER_FRAME],
    /// `va_reserved[VA_PADDING_MEDIUM]` — eight, where the other two codecs' picture
    /// buffers use `VA_PADDING_MEDIUM` and `VA_PADDING_LOW` respectively.
    pub va_reserved: [u32; 8],
}

impl VaDecPictureParameterBufferAV1 {
    /// An all-zero buffer with the sentinels a driver must not read as real values:
    /// every reference slot empty, and no anchor-frame list.
    pub const fn zeroed() -> Self {
        VaDecPictureParameterBufferAV1 {
            profile: 0,
            order_hint_bits_minus_1: 0,
            bit_depth_idx: 0,
            matrix_coefficients: 0,
            seq_info_fields: 0,
            current_frame: crate::va::VA_INVALID_SURFACE,
            current_display_picture: crate::va::VA_INVALID_SURFACE,
            anchor_frames_num: 0,
            anchor_frames_list: std::ptr::null_mut(),
            frame_width_minus1: 0,
            frame_height_minus1: 0,
            output_frame_width_in_tiles_minus_1: 0,
            output_frame_height_in_tiles_minus_1: 0,
            ref_frame_map: [crate::va::VA_INVALID_SURFACE; REF_FRAME_MAP_LEN],
            ref_frame_idx: [0; REFS_PER_FRAME],
            primary_ref_frame: PRIMARY_REF_NONE,
            order_hint: 0,
            seg_info: VaSegmentationStructAV1::zeroed(),
            film_grain_info: VaFilmGrainStructAV1::zeroed(),
            tile_cols: 0,
            tile_rows: 0,
            width_in_sbs_minus_1: [0; TILE_SBS_LEN],
            height_in_sbs_minus_1: [0; TILE_SBS_LEN],
            tile_count_minus_1: 0,
            context_update_tile_id: 0,
            pic_info_fields: 0,
            superres_scale_denominator: SUPERRES_NUM,
            interp_filter: 0,
            filter_level: [0; 2],
            filter_level_u: 0,
            filter_level_v: 0,
            loop_filter_info_fields: 0,
            ref_deltas: [0; TOTAL_REFS_PER_FRAME],
            mode_deltas: [0; 2],
            base_qindex: 0,
            y_dc_delta_q: 0,
            u_dc_delta_q: 0,
            u_ac_delta_q: 0,
            v_dc_delta_q: 0,
            v_ac_delta_q: 0,
            qmatrix_fields: 0,
            mode_control_fields: 0,
            cdef_damping_minus_3: 0,
            cdef_bits: 0,
            cdef_y_strengths: [0; CDEF_MAX],
            cdef_uv_strengths: [0; CDEF_MAX],
            loop_restoration_fields: 0,
            wm: [VaWarpedMotionParamsAV1::zeroed(); REFS_PER_FRAME],
            va_reserved: [0; 8],
        }
    }
}

/// `VASliceParameterBufferAV1` — **one per TILE**, not per tile group (module docs).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSliceParameterBufferAV1 {
    /// This tile's byte count.
    pub slice_data_size: u32,
    /// This tile's offset **inside the accompanying `VASliceDataBufferType` buffer**
    /// — which holds the whole tile group's `tile_data` region, not just this tile.
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub tile_row: u16,
    pub tile_column: u16,
    /// `va_deprecated` in the header — and libavcodec fills both anyway, so this
    /// rung does too. A deprecated field a driver may still read is not a field to
    /// leave at whatever `zeroed()` chose.
    pub tg_start: u16,
    pub tg_end: u16,
    pub anchor_frame_idx: u8,
    pub tile_idx_in_tile_list: u16,
    pub va_reserved: [u32; 4],
}

impl VaSliceParameterBufferAV1 {
    pub const fn zeroed() -> Self {
        VaSliceParameterBufferAV1 {
            slice_data_size: 0,
            slice_data_offset: 0,
            slice_data_flag: crate::va::VA_SLICE_DATA_FLAG_ALL,
            tile_row: 0,
            tile_column: 0,
            tg_start: 0,
            tg_end: 0,
            anchor_frame_idx: ANCHOR_FRAME_UNUSED,
            tile_idx_in_tile_list: 0,
            va_reserved: [0; 4],
        }
    }
}

// ---------------------------------------------------------------------------
// Layout proofs — the probe's output, pinned (libva 2.23.0, x86_64-linux-gnu).
// ---------------------------------------------------------------------------

const _: () = {
    use std::mem::align_of;
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaSegmentationStructAV1>() == 156);
    assert!(offset_of!(VaSegmentationStructAV1, segment_info_fields) == 0);
    assert!(offset_of!(VaSegmentationStructAV1, feature_data) == 4);
    assert!(offset_of!(VaSegmentationStructAV1, feature_mask) == 132);
    assert!(offset_of!(VaSegmentationStructAV1, va_reserved) == 140);

    assert!(size_of::<VaFilmGrainStructAV1>() == 176);
    assert!(offset_of!(VaFilmGrainStructAV1, film_grain_info_fields) == 0);
    assert!(offset_of!(VaFilmGrainStructAV1, grain_seed) == 4);
    assert!(offset_of!(VaFilmGrainStructAV1, num_y_points) == 6);
    assert!(offset_of!(VaFilmGrainStructAV1, point_y_value) == 7);
    assert!(offset_of!(VaFilmGrainStructAV1, point_y_scaling) == 21);
    assert!(offset_of!(VaFilmGrainStructAV1, num_cb_points) == 35);
    assert!(offset_of!(VaFilmGrainStructAV1, point_cb_value) == 36);
    assert!(offset_of!(VaFilmGrainStructAV1, point_cb_scaling) == 46);
    assert!(offset_of!(VaFilmGrainStructAV1, num_cr_points) == 56);
    assert!(offset_of!(VaFilmGrainStructAV1, point_cr_value) == 57);
    assert!(offset_of!(VaFilmGrainStructAV1, point_cr_scaling) == 67);
    assert!(offset_of!(VaFilmGrainStructAV1, ar_coeffs_y) == 77);
    assert!(offset_of!(VaFilmGrainStructAV1, ar_coeffs_cb) == 101);
    assert!(offset_of!(VaFilmGrainStructAV1, ar_coeffs_cr) == 126);
    assert!(offset_of!(VaFilmGrainStructAV1, cb_mult) == 151);
    assert!(offset_of!(VaFilmGrainStructAV1, cb_luma_mult) == 152);
    assert!(offset_of!(VaFilmGrainStructAV1, cb_offset) == 154);
    assert!(offset_of!(VaFilmGrainStructAV1, cr_mult) == 156);
    assert!(offset_of!(VaFilmGrainStructAV1, cr_luma_mult) == 157);
    assert!(offset_of!(VaFilmGrainStructAV1, cr_offset) == 158);
    assert!(offset_of!(VaFilmGrainStructAV1, va_reserved) == 160);

    assert!(size_of::<VaWarpedMotionParamsAV1>() == 56);
    assert!(offset_of!(VaWarpedMotionParamsAV1, wmtype) == 0);
    assert!(offset_of!(VaWarpedMotionParamsAV1, wmmat) == 4);
    assert!(offset_of!(VaWarpedMotionParamsAV1, invalid) == 36);
    assert!(offset_of!(VaWarpedMotionParamsAV1, va_reserved) == 40);

    // The pointer member is what makes this one align 8 rather than 4, and it is
    // asserted for its own sake: the padding it creates at offsets 17..24 is the
    // kind a hand-written declaration silently omits.
    assert!(size_of::<VaDecPictureParameterBufferAV1>() == 1160);
    assert!(align_of::<VaDecPictureParameterBufferAV1>() == 8);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, profile) == 0);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, order_hint_bits_minus_1) == 1);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, bit_depth_idx) == 2);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, matrix_coefficients) == 3);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, seq_info_fields) == 4);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, current_frame) == 8);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, current_display_picture) == 12);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, anchor_frames_num) == 16);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, anchor_frames_list) == 24);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, frame_width_minus1) == 32);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, frame_height_minus1) == 34);
    assert!(
        offset_of!(
            VaDecPictureParameterBufferAV1,
            output_frame_width_in_tiles_minus_1
        ) == 36
    );
    assert!(
        offset_of!(
            VaDecPictureParameterBufferAV1,
            output_frame_height_in_tiles_minus_1
        ) == 38
    );
    assert!(offset_of!(VaDecPictureParameterBufferAV1, ref_frame_map) == 40);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, ref_frame_idx) == 72);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, primary_ref_frame) == 79);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, order_hint) == 80);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, seg_info) == 84);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, film_grain_info) == 240);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, tile_cols) == 416);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, tile_rows) == 417);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, width_in_sbs_minus_1) == 418);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, height_in_sbs_minus_1) == 544);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, tile_count_minus_1) == 670);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, context_update_tile_id) == 672);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, pic_info_fields) == 676);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, superres_scale_denominator) == 680);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, interp_filter) == 681);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, filter_level) == 682);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, filter_level_u) == 684);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, filter_level_v) == 685);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, loop_filter_info_fields) == 686);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, ref_deltas) == 687);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, mode_deltas) == 695);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, base_qindex) == 697);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, y_dc_delta_q) == 698);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, u_dc_delta_q) == 699);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, u_ac_delta_q) == 700);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, v_dc_delta_q) == 701);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, v_ac_delta_q) == 702);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, qmatrix_fields) == 704);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, mode_control_fields) == 708);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, cdef_damping_minus_3) == 712);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, cdef_bits) == 713);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, cdef_y_strengths) == 714);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, cdef_uv_strengths) == 722);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, loop_restoration_fields) == 730);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, wm) == 732);
    assert!(offset_of!(VaDecPictureParameterBufferAV1, va_reserved) == 1124);

    assert!(size_of::<VaSliceParameterBufferAV1>() == 40);
    assert!(offset_of!(VaSliceParameterBufferAV1, slice_data_size) == 0);
    assert!(offset_of!(VaSliceParameterBufferAV1, slice_data_offset) == 4);
    assert!(offset_of!(VaSliceParameterBufferAV1, slice_data_flag) == 8);
    assert!(offset_of!(VaSliceParameterBufferAV1, tile_row) == 12);
    assert!(offset_of!(VaSliceParameterBufferAV1, tile_column) == 14);
    assert!(offset_of!(VaSliceParameterBufferAV1, tg_start) == 16);
    assert!(offset_of!(VaSliceParameterBufferAV1, tg_end) == 18);
    assert!(offset_of!(VaSliceParameterBufferAV1, anchor_frame_idx) == 20);
    assert!(offset_of!(VaSliceParameterBufferAV1, tile_idx_in_tile_list) == 22);
    assert!(offset_of!(VaSliceParameterBufferAV1, va_reserved) == 24);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe's own single-field vectors, restated. Each of these is a number a
    /// real `gcc` printed after setting exactly one bit-field.
    #[test]
    fn av1_bit_fields_pack_where_the_probe_measured() {
        assert_eq!(
            SeqInfoFieldsAV1 {
                still_picture: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0001
        );
        assert_eq!(
            SeqInfoFieldsAV1 {
                mono_chrome: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0400
        );
        assert_eq!(
            SeqInfoFieldsAV1 {
                film_grain_params_present: true,
                ..Default::default()
            }
            .pack(),
            0x0000_8000
        );
        assert_eq!(
            PicInfoFieldsAV1 {
                frame_type: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0003
        );
        assert_eq!(
            PicInfoFieldsAV1 {
                use_ref_frame_mvs: true,
                ..Default::default()
            }
            .pack(),
            0x0000_1000
        );
        assert_eq!(
            PicInfoFieldsAV1 {
                large_scale_tile: true,
                ..Default::default()
            }
            .pack(),
            0x0001_0000
        );
        assert_eq!(
            LoopFilterInfoFieldsAV1 {
                sharpness_level: 7,
                ..Default::default()
            }
            .pack(),
            0x07
        );
        assert_eq!(
            LoopFilterInfoFieldsAV1 {
                mode_ref_delta_update: true,
                ..Default::default()
            }
            .pack(),
            0x10
        );
        assert_eq!(
            QmatrixFieldsAV1 {
                using_qmatrix: true,
                ..Default::default()
            }
            .pack(),
            0x0001
        );
        assert_eq!(
            QmatrixFieldsAV1 {
                qm_v: 0xf,
                ..Default::default()
            }
            .pack(),
            0x1e00
        );
        assert_eq!(
            ModeControlFieldsAV1 {
                delta_q_present_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0001
        );
        assert_eq!(
            ModeControlFieldsAV1 {
                tx_mode: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0180
        );
        assert_eq!(
            ModeControlFieldsAV1 {
                skip_mode_present: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0800
        );
        assert_eq!(
            LoopRestorationFieldsAV1 {
                yframe_restoration_type: 3,
                ..Default::default()
            }
            .pack(),
            0x0003
        );
        assert_eq!(
            LoopRestorationFieldsAV1 {
                lr_uv_shift: 1,
                ..Default::default()
            }
            .pack(),
            0x0100
        );
        assert_eq!(
            SegmentInfoFieldsAV1 {
                enabled: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0001
        );
        assert_eq!(
            SegmentInfoFieldsAV1 {
                update_data: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0008
        );
        assert_eq!(
            FilmGrainInfoFieldsAV1 {
                apply_grain: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0001
        );
        assert_eq!(
            FilmGrainInfoFieldsAV1 {
                grain_scale_shift: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0300
        );
        assert_eq!(
            FilmGrainInfoFieldsAV1 {
                clip_to_restricted_range: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0800
        );
    }

    /// Every field alone must light only its own bits, and nothing may reach the
    /// reserved tail. Two probe vectors per word would not catch a shift typo that
    /// overlapped two neighbours — and on the three NARROW unions, a field that
    /// overflowed its declared width would be invisible in a `u32` comparison, which
    /// is why each of those is checked against its own type's mask.
    #[test]
    fn every_av1_field_owns_a_distinct_bit_range() {
        // A free function rather than a closure so `seen` can be reset between the
        // unions without the closure's borrow outliving it.
        fn check(seen: &mut u32, bits: u32, mask: u32) {
            assert_ne!(bits, 0, "a field packed to nothing");
            assert_eq!(*seen & bits, 0, "two fields share a bit: {bits:#010x}");
            assert_eq!(bits & !mask, 0, "a field reached the reserved tail");
            *seen |= bits;
        }

        let mut seen = 0u32;
        const SEQ_MASK: u32 = 0x0000_ffff;
        check(
            &mut seen,
            SeqInfoFieldsAV1 {
                chroma_sample_position: 1,
                ..Default::default()
            }
            .pack(),
            SEQ_MASK,
        );
        for set in [
            |f: &mut SeqInfoFieldsAV1| f.still_picture = true,
            |f: &mut SeqInfoFieldsAV1| f.use_128x128_superblock = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_filter_intra = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_intra_edge_filter = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_interintra_compound = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_masked_compound = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_dual_filter = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_order_hint = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_jnt_comp = true,
            |f: &mut SeqInfoFieldsAV1| f.enable_cdef = true,
            |f: &mut SeqInfoFieldsAV1| f.mono_chrome = true,
            |f: &mut SeqInfoFieldsAV1| f.color_range = true,
            |f: &mut SeqInfoFieldsAV1| f.subsampling_x = true,
            |f: &mut SeqInfoFieldsAV1| f.subsampling_y = true,
            |f: &mut SeqInfoFieldsAV1| f.film_grain_params_present = true,
        ] {
            let mut f = SeqInfoFieldsAV1::default();
            set(&mut f);
            check(&mut seen, f.pack(), SEQ_MASK);
        }
        assert_eq!(seen, SEQ_MASK, "all 16 seq_info bits accounted for");

        seen = 0;
        const PIC_MASK: u32 = 0x0001_ffff;
        check(
            &mut seen,
            PicInfoFieldsAV1 {
                frame_type: 3,
                ..Default::default()
            }
            .pack(),
            PIC_MASK,
        );
        for set in [
            |f: &mut PicInfoFieldsAV1| f.show_frame = true,
            |f: &mut PicInfoFieldsAV1| f.showable_frame = true,
            |f: &mut PicInfoFieldsAV1| f.error_resilient_mode = true,
            |f: &mut PicInfoFieldsAV1| f.disable_cdf_update = true,
            |f: &mut PicInfoFieldsAV1| f.allow_screen_content_tools = true,
            |f: &mut PicInfoFieldsAV1| f.force_integer_mv = true,
            |f: &mut PicInfoFieldsAV1| f.allow_intrabc = true,
            |f: &mut PicInfoFieldsAV1| f.use_superres = true,
            |f: &mut PicInfoFieldsAV1| f.allow_high_precision_mv = true,
            |f: &mut PicInfoFieldsAV1| f.is_motion_mode_switchable = true,
            |f: &mut PicInfoFieldsAV1| f.use_ref_frame_mvs = true,
            |f: &mut PicInfoFieldsAV1| f.disable_frame_end_update_cdf = true,
            |f: &mut PicInfoFieldsAV1| f.uniform_tile_spacing_flag = true,
            |f: &mut PicInfoFieldsAV1| f.allow_warped_motion = true,
            |f: &mut PicInfoFieldsAV1| f.large_scale_tile = true,
        ] {
            let mut f = PicInfoFieldsAV1::default();
            set(&mut f);
            check(&mut seen, f.pack(), PIC_MASK);
        }
        assert_eq!(seen, PIC_MASK, "all 17 pic_info bits accounted for");

        seen = 0;
        const MODE_MASK: u32 = 0x0000_0fff;
        for set in [
            |f: &mut ModeControlFieldsAV1| f.delta_q_present_flag = true,
            |f: &mut ModeControlFieldsAV1| f.log2_delta_q_res = 3,
            |f: &mut ModeControlFieldsAV1| f.delta_lf_present_flag = true,
            |f: &mut ModeControlFieldsAV1| f.log2_delta_lf_res = 3,
            |f: &mut ModeControlFieldsAV1| f.delta_lf_multi = true,
            |f: &mut ModeControlFieldsAV1| f.tx_mode = 3,
            |f: &mut ModeControlFieldsAV1| f.reference_select = true,
            |f: &mut ModeControlFieldsAV1| f.reduced_tx_set_used = true,
            |f: &mut ModeControlFieldsAV1| f.skip_mode_present = true,
        ] {
            let mut f = ModeControlFieldsAV1::default();
            set(&mut f);
            check(&mut seen, f.pack(), MODE_MASK);
        }
        assert_eq!(seen, MODE_MASK);

        seen = 0;
        const FG_MASK: u32 = 0x0000_0fff;
        for set in [
            |f: &mut FilmGrainInfoFieldsAV1| f.apply_grain = true,
            |f: &mut FilmGrainInfoFieldsAV1| f.chroma_scaling_from_luma = true,
            |f: &mut FilmGrainInfoFieldsAV1| f.grain_scaling_minus_8 = 3,
            |f: &mut FilmGrainInfoFieldsAV1| f.ar_coeff_lag = 3,
            |f: &mut FilmGrainInfoFieldsAV1| f.ar_coeff_shift_minus_6 = 3,
            |f: &mut FilmGrainInfoFieldsAV1| f.grain_scale_shift = 3,
            |f: &mut FilmGrainInfoFieldsAV1| f.overlap_flag = true,
            |f: &mut FilmGrainInfoFieldsAV1| f.clip_to_restricted_range = true,
        ] {
            let mut f = FilmGrainInfoFieldsAV1::default();
            set(&mut f);
            check(&mut seen, f.pack(), FG_MASK);
        }
        assert_eq!(seen, FG_MASK);

        seen = 0;
        for set in [
            |f: &mut SegmentInfoFieldsAV1| f.enabled = true,
            |f: &mut SegmentInfoFieldsAV1| f.update_map = true,
            |f: &mut SegmentInfoFieldsAV1| f.temporal_update = true,
            |f: &mut SegmentInfoFieldsAV1| f.update_data = true,
        ] {
            let mut f = SegmentInfoFieldsAV1::default();
            set(&mut f);
            check(&mut seen, f.pack(), 0x0000_000f);
        }
        assert_eq!(seen, 0x0000_000f);

        // The three NARROW unions, checked in their own widths.
        let mut seen8 = 0u8;
        for bits in [
            LoopFilterInfoFieldsAV1 {
                sharpness_level: 7,
                ..Default::default()
            }
            .pack(),
            LoopFilterInfoFieldsAV1 {
                mode_ref_delta_enabled: true,
                ..Default::default()
            }
            .pack(),
            LoopFilterInfoFieldsAV1 {
                mode_ref_delta_update: true,
                ..Default::default()
            }
            .pack(),
        ] {
            assert_ne!(bits, 0);
            assert_eq!(seen8 & bits, 0);
            seen8 |= bits;
        }
        assert_eq!(seen8, 0x1f, "five bits, and nothing in the reserved three");

        let mut seen16 = 0u16;
        for bits in [
            QmatrixFieldsAV1 {
                using_qmatrix: true,
                ..Default::default()
            }
            .pack(),
            QmatrixFieldsAV1 {
                qm_y: 0xf,
                ..Default::default()
            }
            .pack(),
            QmatrixFieldsAV1 {
                qm_u: 0xf,
                ..Default::default()
            }
            .pack(),
            QmatrixFieldsAV1 {
                qm_v: 0xf,
                ..Default::default()
            }
            .pack(),
        ] {
            assert_ne!(bits, 0);
            assert_eq!(seen16 & bits, 0);
            seen16 |= bits;
        }
        assert_eq!(seen16, 0x1fff, "13 bits, and nothing in the reserved three");

        seen16 = 0;
        for bits in [
            LoopRestorationFieldsAV1 {
                yframe_restoration_type: 3,
                ..Default::default()
            }
            .pack(),
            LoopRestorationFieldsAV1 {
                cbframe_restoration_type: 3,
                ..Default::default()
            }
            .pack(),
            LoopRestorationFieldsAV1 {
                crframe_restoration_type: 3,
                ..Default::default()
            }
            .pack(),
            LoopRestorationFieldsAV1 {
                lr_unit_shift: 3,
                ..Default::default()
            }
            .pack(),
            LoopRestorationFieldsAV1 {
                lr_uv_shift: 1,
                ..Default::default()
            }
            .pack(),
        ] {
            assert_ne!(bits, 0);
            assert_eq!(seen16 & bits, 0);
            seen16 |= bits;
        }
        assert_eq!(
            seen16, 0x01ff,
            "nine bits, and nothing in the reserved seven"
        );
    }

    /// The sentinels a zeroed picture buffer must NOT leave as plausible values.
    ///
    /// `ref_frame_map` is the one that matters: a zero there is a perfectly valid
    /// `VASurfaceID`, and `va_dec_av1.h` says outright that the *"Driver is not
    /// responsible to validate reference frames' id"* — so an unfilled slot has to
    /// carry `VA_INVALID_ID` or the driver predicts from surface 0.
    #[test]
    fn a_zeroed_picture_buffer_carries_the_sentinels_not_zeros() {
        let p = VaDecPictureParameterBufferAV1::zeroed();
        assert!(p
            .ref_frame_map
            .iter()
            .all(|&s| s == crate::va::VA_INVALID_SURFACE));
        assert_eq!(p.current_frame, crate::va::VA_INVALID_SURFACE);
        assert_eq!(p.current_display_picture, crate::va::VA_INVALID_SURFACE);
        assert_eq!(p.primary_ref_frame, PRIMARY_REF_NONE);
        assert_eq!(
            p.superres_scale_denominator, SUPERRES_NUM,
            "a frame without superres sends 8, never 0 — libva documents 8 or 9..=16"
        );
        assert!(p.anchor_frames_list.is_null());
        let t = VaSliceParameterBufferAV1::zeroed();
        assert_eq!(t.slice_data_flag, crate::va::VA_SLICE_DATA_FLAG_ALL);
    }

    /// Every enumerator [`crate::pic_av1`] CASTS rather than remaps, pinned against
    /// libva's own documented numbering.
    ///
    /// Four families reach a driver as a bare `as u8` / `as u32` of a vendored-parser
    /// enum. Each cast is only correct because the parser's discriminants happen to be
    /// the spec's, and "happen to be" is what a test is for: a vendored-parser bump
    /// that renumbered any of them would decode to something plausible rather than
    /// failing.
    ///
    /// The right-hand sides are transcribed from `va_dec_av1.h` and from libavcodec's
    /// `av1.h`, not from the parser — comparing the parser against itself would pass
    /// forever.
    #[test]
    fn every_enumerator_this_rung_casts_matches_the_numbering_libva_documents() {
        use cros_codecs::codec::av1::parser::FrameRestorationType;
        use cros_codecs::codec::av1::parser::FrameType;
        use cros_codecs::codec::av1::parser::InterpolationFilter;
        use cros_codecs::codec::av1::parser::TxMode;
        use cros_codecs::codec::av1::parser::WarpModelType;

        // `VAAV1TransformationType` (measured off the header).
        assert_eq!(
            WarpModelType::Identity as u32,
            VA_AV1_TRANSFORMATION_IDENTITY
        );
        assert_eq!(
            WarpModelType::Translation as u32,
            VA_AV1_TRANSFORMATION_TRANSLATION
        );
        assert_eq!(WarpModelType::RotZoom as u32, VA_AV1_TRANSFORMATION_ROTZOOM);
        assert_eq!(WarpModelType::Affine as u32, VA_AV1_TRANSFORMATION_AFFINE);

        // `pic_info_fields.frame_type`, which the header documents inline:
        // "0: KEY_FRAME; 1: INTER_FRAME; 2: INTRA_ONLY_FRAME; 3: SWITCH_FRAME".
        assert_eq!(FrameType::KeyFrame as u8, 0);
        assert_eq!(FrameType::InterFrame as u8, 1);
        assert_eq!(FrameType::IntraOnlyFrame as u8, 2);
        assert_eq!(FrameType::SwitchFrame as u8, 3);

        // `mode_control_fields.tx_mode` — "read_tx_mode, value range [0..2]", i.e.
        // ONLY_4X4 / TX_MODE_LARGEST / TX_MODE_SELECT.
        assert_eq!(TxMode::Only4x4 as u8, 0);
        assert_eq!(TxMode::Largest as u8, 1);
        assert_eq!(TxMode::Select as u8, 2);

        // `interp_filter` — "value range [0..4]", AV1 6.8.9's
        // EIGHTTAP / EIGHTTAP_SMOOTH / EIGHTTAP_SHARP / BILINEAR / SWITCHABLE.
        assert_eq!(InterpolationFilter::EightTap as u8, 0);
        assert_eq!(InterpolationFilter::EightTapSmooth as u8, 1);
        assert_eq!(InterpolationFilter::EightTapSharp as u8, 2);
        assert_eq!(InterpolationFilter::Bilinear as u8, 3);
        assert_eq!(InterpolationFilter::Switchable as u8, 4);

        // `loop_restoration_fields.*frame_restoration_type` — libavcodec's
        // `AV1_RESTORE_NONE/WIENER/SGRPROJ/SWITCHABLE` = 0/1/2/3, which is what its
        // `remap_lr_type[] = {NONE, SWITCHABLE, WIENER, SGRPROJ}` PRODUCES from the
        // coded two-bit `lr_type`. The vendored parser applies the same
        // `REMAP_LR_TYPE` as it reads, so [`crate::pic_av1`] casts and remaps
        // nothing; if these four numbers moved, it would have to.
        assert_eq!(FrameRestorationType::None as u8, 0);
        assert_eq!(FrameRestorationType::Wiener as u8, 1);
        assert_eq!(FrameRestorationType::Sgrproj as u8, 2);
        assert_eq!(FrameRestorationType::Switchable as u8, 3);
    }
}
