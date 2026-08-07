//! The libva decode buffer layouts for H.265, hand-declared — the HEVC twin of
//! [`crate::va`], measured the same way and pinned the same way.
//!
//! Sizes and offsets come from the committed `layout-probe.c` run against libva
//! **2.23.0** headers: `VAPictureHEVC` **28**, `VAPictureParameterBufferHEVC` **604**,
//! `VASliceParameterBufferHEVC` **264**, `VAIQMatrixBufferHEVC` **1016**. Every bit
//! position below was read back off a real header too, not counted by eye.
//!
//! # HEVC's reference plumbing is a THIRD convention
//!
//! This program has now met three different ways of saying which pictures a short-term
//! reference set contains, and they are not interchangeable:
//!
//! * **Vulkan** takes DPB *slot* indices in `RefPicSetStCurrBefore/After/LtCurr` —
//!   writing reference-list positions there is what made HEVC unplayable on every
//!   driver until it was root-caused.
//! * **DXVA** takes positions into `RefPicList[]` in identically named arrays.
//! * **VAAPI** takes neither: it marks membership as **flags on the DPB entries
//!   themselves** (`VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE` / `_AFTER` / `_LT_CURR`), and
//!   the per-slice `RefPicList[2][15]` holds **indices into `ReferenceFrames`** rather
//!   than pictures.
//!
//! Three spellings of one idea, one of which has already cost this project a
//! shipped defect — so the conversion states which it is writing, every time.
//!
//! # And the offset is a BYTE offset
//!
//! H.264's `slice_data_bit_offset` counts bits; HEVC's `slice_data_byte_offset` counts
//! bytes, over the same definition (from and including the NAL header byte, with
//! emulation-prevention bytes removed). `slice_data()` is byte-aligned by
//! `byte_alignment()`, so the parser's `header_bit_size / 8` is exact rather than
//! rounded — and the conversion asserts that rather than assuming it.

/// Flags for [`VaPictureHEVC::flags`].
pub const VA_PICTURE_HEVC_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_HEVC_FIELD_PIC: u32 = 0x0000_0002;
pub const VA_PICTURE_HEVC_BOTTOM_FIELD: u32 = 0x0000_0004;
pub const VA_PICTURE_HEVC_LONG_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x0000_0010;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_AFTER: u32 = 0x0000_0020;
pub const VA_PICTURE_HEVC_RPS_LT_CURR: u32 = 0x0000_0040;

/// `ReferenceFrames` length — 15, not 16 as in H.264.
pub const REFERENCE_FRAMES_LEN_H265: usize = 15;

/// `RefPicList[2][15]`'s inner length, and the value an unused entry carries
/// (`0xff`, libva's "no entry" for an index into `ReferenceFrames`).
pub const REF_PIC_LIST_LEN_H265: usize = 15;
pub const REF_PIC_LIST_UNUSED: u8 = 0xff;

/// `VAPictureHEVC` — one DPB entry, or the current picture.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureHEVC {
    pub picture_id: u32,
    pub pic_order_cnt: i32,
    /// Long-term marking AND the picture's RPS membership, ORed together.
    pub flags: u32,
    pub va_reserved: [u32; 4],
}

impl VaPictureHEVC {
    pub const fn invalid() -> Self {
        VaPictureHEVC {
            picture_id: crate::va::VA_INVALID_SURFACE,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
            va_reserved: [0; 4],
        }
    }
}

/// `VAPictureParameterBufferHEVC::pic_fields`, unpacked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PicFieldsH265 {
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub pcm_enabled_flag: bool,
    pub scaling_list_enabled_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub amp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub sign_data_hiding_enabled_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub pcm_loop_filter_disabled_flag: bool,
    /// Derived, not a syntax element: the stream never reorders.
    pub no_pic_reordering_flag: bool,
    /// Derived: no picture uses bi-prediction.
    pub no_bi_pred_flag: bool,
}

impl PicFieldsH265 {
    pub const fn pack(self) -> u32 {
        (self.chroma_format_idc as u32 & 0x3)
            | ((self.separate_colour_plane_flag as u32) << 2)
            | ((self.pcm_enabled_flag as u32) << 3)
            | ((self.scaling_list_enabled_flag as u32) << 4)
            | ((self.transform_skip_enabled_flag as u32) << 5)
            | ((self.amp_enabled_flag as u32) << 6)
            | ((self.strong_intra_smoothing_enabled_flag as u32) << 7)
            | ((self.sign_data_hiding_enabled_flag as u32) << 8)
            | ((self.constrained_intra_pred_flag as u32) << 9)
            | ((self.cu_qp_delta_enabled_flag as u32) << 10)
            | ((self.weighted_pred_flag as u32) << 11)
            | ((self.weighted_bipred_flag as u32) << 12)
            | ((self.transquant_bypass_enabled_flag as u32) << 13)
            | ((self.tiles_enabled_flag as u32) << 14)
            | ((self.entropy_coding_sync_enabled_flag as u32) << 15)
            | ((self.pps_loop_filter_across_slices_enabled_flag as u32) << 16)
            | ((self.loop_filter_across_tiles_enabled_flag as u32) << 17)
            | ((self.pcm_loop_filter_disabled_flag as u32) << 18)
            | ((self.no_pic_reordering_flag as u32) << 19)
            | ((self.no_bi_pred_flag as u32) << 20)
    }
}

/// `VAPictureParameterBufferHEVC::slice_parsing_fields`, unpacked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SliceParsingFieldsH265 {
    pub lists_modification_present_flag: bool,
    pub long_term_ref_pics_present_flag: bool,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
    pub output_flag_present_flag: bool,
    pub dependent_slice_segments_enabled_flag: bool,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_disable_deblocking_filter_flag: bool,
    pub slice_segment_header_extension_present_flag: bool,
    pub rap_pic_flag: bool,
    pub idr_pic_flag: bool,
    pub intra_pic_flag: bool,
}

impl SliceParsingFieldsH265 {
    pub const fn pack(self) -> u32 {
        (self.lists_modification_present_flag as u32)
            | ((self.long_term_ref_pics_present_flag as u32) << 1)
            | ((self.sps_temporal_mvp_enabled_flag as u32) << 2)
            | ((self.cabac_init_present_flag as u32) << 3)
            | ((self.output_flag_present_flag as u32) << 4)
            | ((self.dependent_slice_segments_enabled_flag as u32) << 5)
            | ((self.pps_slice_chroma_qp_offsets_present_flag as u32) << 6)
            | ((self.sample_adaptive_offset_enabled_flag as u32) << 7)
            | ((self.deblocking_filter_override_enabled_flag as u32) << 8)
            | ((self.pps_disable_deblocking_filter_flag as u32) << 9)
            | ((self.slice_segment_header_extension_present_flag as u32) << 10)
            | ((self.rap_pic_flag as u32) << 11)
            | ((self.idr_pic_flag as u32) << 12)
            | ((self.intra_pic_flag as u32) << 13)
    }
}

/// `VASliceParameterBufferHEVC::LongSliceFlags`, unpacked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LongSliceFlagsH265 {
    pub last_slice_of_pic: bool,
    pub dependent_slice_segment_flag: bool,
    /// 0 = B, 1 = P, 2 = I (H.265's own numbering, not H.264's).
    pub slice_type: u8,
    pub color_plane_id: u8,
    pub slice_sao_luma_flag: bool,
    pub slice_sao_chroma_flag: bool,
    pub mvd_l1_zero_flag: bool,
    pub cabac_init_flag: bool,
    pub slice_temporal_mvp_enabled_flag: bool,
    pub slice_deblocking_filter_disabled_flag: bool,
    pub collocated_from_l0_flag: bool,
    pub slice_loop_filter_across_slices_enabled_flag: bool,
}

impl LongSliceFlagsH265 {
    pub const fn pack(self) -> u32 {
        (self.last_slice_of_pic as u32)
            | ((self.dependent_slice_segment_flag as u32) << 1)
            | ((self.slice_type as u32 & 0x3) << 2)
            | ((self.color_plane_id as u32 & 0x3) << 4)
            | ((self.slice_sao_luma_flag as u32) << 6)
            | ((self.slice_sao_chroma_flag as u32) << 7)
            | ((self.mvd_l1_zero_flag as u32) << 8)
            | ((self.cabac_init_flag as u32) << 9)
            | ((self.slice_temporal_mvp_enabled_flag as u32) << 10)
            | ((self.slice_deblocking_filter_disabled_flag as u32) << 11)
            | ((self.collocated_from_l0_flag as u32) << 12)
            | ((self.slice_loop_filter_across_slices_enabled_flag as u32) << 13)
    }
}

/// `VAPictureParameterBufferHEVC`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureParameterBufferHEVC {
    pub curr_pic: VaPictureHEVC,
    /// The DPB — 15 entries, each carrying its own RPS membership flags.
    pub reference_frames: [VaPictureHEVC; REFERENCE_FRAMES_LEN_H265],
    /// LUMA SAMPLES, not macroblocks: HEVC states the picture size directly.
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub pic_fields: u32,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub slice_parsing_fields: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pic_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub num_extra_slice_header_bits: u8,
    /// Bit length of the short-term RPS coded in THIS slice header, or 0 when the
    /// slice referenced an SPS set instead.
    pub st_rps_bits: u32,
    pub va_reserved: [u32; 8],
}

/// `VAIQMatrixBufferHEVC` — four list sizes plus the two DC tables.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaIqMatrixBufferHEVC {
    pub scaling_list4x4: [[u8; 16]; 6],
    pub scaling_list8x8: [[u8; 64]; 6],
    pub scaling_list16x16: [[u8; 64]; 6],
    pub scaling_list32x32: [[u8; 64]; 2],
    pub scaling_list_dc16x16: [u8; 6],
    pub scaling_list_dc32x32: [u8; 2],
    pub va_reserved: [u32; 4],
}

/// `VASliceParameterBufferHEVC`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSliceParameterBufferHEVC {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    /// BYTES from and including the NAL header byte to `slice_data()`, with
    /// emulation-prevention bytes removed (module docs).
    pub slice_data_byte_offset: u32,
    pub slice_segment_address: u32,
    /// Indices into [`VaPictureParameterBufferHEVC::reference_frames`], NOT pictures
    /// and NOT surfaces. `0xff` marks an unused entry.
    pub ref_pic_list: [[u8; REF_PIC_LIST_LEN_H265]; 2],
    pub long_slice_flags: u32,
    pub collocated_ref_idx: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub five_minus_max_num_merge_cand: u8,
    pub num_entry_point_offsets: u16,
    pub entry_offset_to_subset_array: u16,
    pub slice_data_num_emu_prevn_bytes: u16,
    /// `va_reserved[VA_PADDING_LOW - 2]`.
    pub va_reserved: [u32; 2],
}

impl VaSliceParameterBufferHEVC {
    /// An all-zero record with both reference lists marked unused.
    pub const fn zeroed() -> Self {
        VaSliceParameterBufferHEVC {
            slice_data_size: 0,
            slice_data_offset: 0,
            slice_data_flag: crate::va::VA_SLICE_DATA_FLAG_ALL,
            slice_data_byte_offset: 0,
            slice_segment_address: 0,
            ref_pic_list: [[REF_PIC_LIST_UNUSED; REF_PIC_LIST_LEN_H265]; 2],
            long_slice_flags: 0,
            collocated_ref_idx: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            slice_beta_offset_div2: 0,
            slice_tc_offset_div2: 0,
            luma_log2_weight_denom: 0,
            delta_chroma_log2_weight_denom: 0,
            delta_luma_weight_l0: [0; 15],
            luma_offset_l0: [0; 15],
            delta_chroma_weight_l0: [[0; 2]; 15],
            chroma_offset_l0: [[0; 2]; 15],
            delta_luma_weight_l1: [0; 15],
            luma_offset_l1: [0; 15],
            delta_chroma_weight_l1: [[0; 2]; 15],
            chroma_offset_l1: [[0; 2]; 15],
            five_minus_max_num_merge_cand: 0,
            num_entry_point_offsets: 0,
            entry_offset_to_subset_array: 0,
            slice_data_num_emu_prevn_bytes: 0,
            va_reserved: [0; 2],
        }
    }
}

// ---------------------------------------------------------------------------
// Layout proofs — the probe's output, pinned (libva 2.23.0, x86_64-linux-gnu).
// ---------------------------------------------------------------------------

const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaPictureHEVC>() == 28);
    assert!(offset_of!(VaPictureHEVC, picture_id) == 0);
    assert!(offset_of!(VaPictureHEVC, pic_order_cnt) == 4);
    assert!(offset_of!(VaPictureHEVC, flags) == 8);
    assert!(offset_of!(VaPictureHEVC, va_reserved) == 12);

    assert!(size_of::<VaPictureParameterBufferHEVC>() == 604);
    assert!(offset_of!(VaPictureParameterBufferHEVC, curr_pic) == 0);
    assert!(offset_of!(VaPictureParameterBufferHEVC, reference_frames) == 28);
    assert!(offset_of!(VaPictureParameterBufferHEVC, pic_width_in_luma_samples) == 448);
    assert!(offset_of!(VaPictureParameterBufferHEVC, pic_height_in_luma_samples) == 450);
    assert!(offset_of!(VaPictureParameterBufferHEVC, pic_fields) == 452);
    assert!(
        offset_of!(
            VaPictureParameterBufferHEVC,
            sps_max_dec_pic_buffering_minus1
        ) == 456
    );
    assert!(offset_of!(VaPictureParameterBufferHEVC, init_qp_minus26) == 469);
    assert!(offset_of!(VaPictureParameterBufferHEVC, num_tile_columns_minus1) == 474);
    assert!(offset_of!(VaPictureParameterBufferHEVC, column_width_minus1) == 476);
    assert!(offset_of!(VaPictureParameterBufferHEVC, row_height_minus1) == 514);
    assert!(offset_of!(VaPictureParameterBufferHEVC, slice_parsing_fields) == 556);
    assert!(
        offset_of!(
            VaPictureParameterBufferHEVC,
            log2_max_pic_order_cnt_lsb_minus4
        ) == 560
    );
    assert!(offset_of!(VaPictureParameterBufferHEVC, num_extra_slice_header_bits) == 567);
    assert!(offset_of!(VaPictureParameterBufferHEVC, st_rps_bits) == 568);
    assert!(offset_of!(VaPictureParameterBufferHEVC, va_reserved) == 572);

    assert!(size_of::<VaIqMatrixBufferHEVC>() == 1016);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list4x4) == 0);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list8x8) == 96);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list16x16) == 480);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list32x32) == 864);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list_dc16x16) == 992);
    assert!(offset_of!(VaIqMatrixBufferHEVC, scaling_list_dc32x32) == 998);
    assert!(offset_of!(VaIqMatrixBufferHEVC, va_reserved) == 1000);

    assert!(size_of::<VaSliceParameterBufferHEVC>() == 264);
    assert!(offset_of!(VaSliceParameterBufferHEVC, slice_data_size) == 0);
    assert!(offset_of!(VaSliceParameterBufferHEVC, slice_data_byte_offset) == 12);
    assert!(offset_of!(VaSliceParameterBufferHEVC, slice_segment_address) == 16);
    assert!(offset_of!(VaSliceParameterBufferHEVC, ref_pic_list) == 20);
    assert!(offset_of!(VaSliceParameterBufferHEVC, long_slice_flags) == 52);
    assert!(offset_of!(VaSliceParameterBufferHEVC, collocated_ref_idx) == 56);
    assert!(offset_of!(VaSliceParameterBufferHEVC, luma_log2_weight_denom) == 64);
    assert!(offset_of!(VaSliceParameterBufferHEVC, delta_luma_weight_l0) == 66);
    assert!(offset_of!(VaSliceParameterBufferHEVC, luma_offset_l0) == 81);
    assert!(offset_of!(VaSliceParameterBufferHEVC, delta_chroma_weight_l0) == 96);
    assert!(offset_of!(VaSliceParameterBufferHEVC, chroma_offset_l0) == 126);
    assert!(offset_of!(VaSliceParameterBufferHEVC, delta_luma_weight_l1) == 156);
    assert!(offset_of!(VaSliceParameterBufferHEVC, chroma_offset_l1) == 216);
    assert!(offset_of!(VaSliceParameterBufferHEVC, five_minus_max_num_merge_cand) == 246);
    assert!(offset_of!(VaSliceParameterBufferHEVC, num_entry_point_offsets) == 248);
    assert!(offset_of!(VaSliceParameterBufferHEVC, slice_data_num_emu_prevn_bytes) == 252);
    assert!(offset_of!(VaSliceParameterBufferHEVC, va_reserved) == 256);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_bit_fields_pack_where_the_probe_measured() {
        assert_eq!(
            PicFieldsH265 {
                chroma_format_idc: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0003
        );
        assert_eq!(
            PicFieldsH265 {
                no_bi_pred_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0010_0000
        );
        assert_eq!(
            SliceParsingFieldsH265 {
                intra_pic_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0000_2000
        );
        assert_eq!(
            LongSliceFlagsH265 {
                slice_type: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_000c
        );
        assert_eq!(
            LongSliceFlagsH265 {
                slice_loop_filter_across_slices_enabled_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0000_2000
        );
    }

    /// Each field alone must light only its own bits — two probe vectors per word
    /// would not catch a shift typo that overlapped two neighbours.
    #[test]
    fn every_hevc_pic_field_owns_a_distinct_bit_range() {
        let mut seen = 0u32;
        let mut check = |bits: u32| {
            assert_ne!(bits, 0, "a field packed to nothing");
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        };
        check(
            PicFieldsH265 {
                chroma_format_idc: 3,
                ..Default::default()
            }
            .pack(),
        );
        for set in [
            |f: &mut PicFieldsH265| f.separate_colour_plane_flag = true,
            |f: &mut PicFieldsH265| f.pcm_enabled_flag = true,
            |f: &mut PicFieldsH265| f.scaling_list_enabled_flag = true,
            |f: &mut PicFieldsH265| f.transform_skip_enabled_flag = true,
            |f: &mut PicFieldsH265| f.amp_enabled_flag = true,
            |f: &mut PicFieldsH265| f.strong_intra_smoothing_enabled_flag = true,
            |f: &mut PicFieldsH265| f.sign_data_hiding_enabled_flag = true,
            |f: &mut PicFieldsH265| f.constrained_intra_pred_flag = true,
            |f: &mut PicFieldsH265| f.cu_qp_delta_enabled_flag = true,
            |f: &mut PicFieldsH265| f.weighted_pred_flag = true,
            |f: &mut PicFieldsH265| f.weighted_bipred_flag = true,
            |f: &mut PicFieldsH265| f.transquant_bypass_enabled_flag = true,
            |f: &mut PicFieldsH265| f.tiles_enabled_flag = true,
            |f: &mut PicFieldsH265| f.entropy_coding_sync_enabled_flag = true,
            |f: &mut PicFieldsH265| f.pps_loop_filter_across_slices_enabled_flag = true,
            |f: &mut PicFieldsH265| f.loop_filter_across_tiles_enabled_flag = true,
            |f: &mut PicFieldsH265| f.pcm_loop_filter_disabled_flag = true,
            |f: &mut PicFieldsH265| f.no_pic_reordering_flag = true,
            |f: &mut PicFieldsH265| f.no_bi_pred_flag = true,
        ] {
            let mut f = PicFieldsH265::default();
            set(&mut f);
            check(f.pack());
        }
        // Nothing may reach into the 11 reserved bits.
        assert_eq!(seen & !0x001f_ffff, 0);
    }

    #[test]
    fn every_hevc_slice_parsing_field_owns_a_distinct_bit_range() {
        let mut seen = 0u32;
        for set in [
            |f: &mut SliceParsingFieldsH265| f.lists_modification_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.long_term_ref_pics_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.sps_temporal_mvp_enabled_flag = true,
            |f: &mut SliceParsingFieldsH265| f.cabac_init_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.output_flag_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.dependent_slice_segments_enabled_flag = true,
            |f: &mut SliceParsingFieldsH265| f.pps_slice_chroma_qp_offsets_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.sample_adaptive_offset_enabled_flag = true,
            |f: &mut SliceParsingFieldsH265| f.deblocking_filter_override_enabled_flag = true,
            |f: &mut SliceParsingFieldsH265| f.pps_disable_deblocking_filter_flag = true,
            |f: &mut SliceParsingFieldsH265| f.slice_segment_header_extension_present_flag = true,
            |f: &mut SliceParsingFieldsH265| f.rap_pic_flag = true,
            |f: &mut SliceParsingFieldsH265| f.idr_pic_flag = true,
            |f: &mut SliceParsingFieldsH265| f.intra_pic_flag = true,
        ] {
            let mut f = SliceParsingFieldsH265::default();
            set(&mut f);
            let bits = f.pack();
            assert_ne!(bits, 0);
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        }
        assert_eq!(seen & !0x0000_3fff, 0);
    }

    #[test]
    fn an_unused_hevc_reference_is_invalid_and_lists_are_0xff() {
        let e = VaPictureHEVC::invalid();
        assert_eq!(e.flags, VA_PICTURE_HEVC_INVALID);
        assert_eq!(e.picture_id, crate::va::VA_INVALID_SURFACE);
        let s = VaSliceParameterBufferHEVC::zeroed();
        assert!(s
            .ref_pic_list
            .iter()
            .all(|l| l.iter().all(|&i| i == REF_PIC_LIST_UNUSED)));
    }
}
