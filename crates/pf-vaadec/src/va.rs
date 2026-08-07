//! The libva decode buffer layouts for H.264, **hand-declared**.
//!
//! There is no libva binding in this workspace and this crate deliberately does not
//! introduce one: it must compile and be tested on macOS and in the Linux container,
//! where `libva` headers need not exist at all. So the structures VAAPI reads are
//! declared here as plain `#[repr(C)]` PODs, exactly as `pf-dxvadec`'s `dxva` module declares
//! DXVA's — same reasoning, same discipline.
//!
//! # These are not eyeballed
//!
//! Every size, every field offset and the bit-field allocation order below were
//! measured against the real headers (libva **2.23.0**, `x86_64-linux-gnu`) by
//! compiling a probe that printed `sizeof`, `_Alignof` and `offsetof` for each field
//! and set individual bit-fields to read the resulting word back. The numbers that
//! probe produced are pinned as `const` assertions at the bottom of this module, so
//! a mistake here is a compile error rather than a driver reading the wrong byte.
//!
//! The measured facts worth stating in prose, because they are the ones a reader
//! would otherwise assume wrongly:
//!
//! * `VAPictureH264` is **36** bytes — five 4-byte fields plus `VA_PADDING_LOW`
//!   (4 × `uint32_t`) of reserved tail. It is embedded 1 + 16 times in the picture
//!   parameter buffer and 64 times in the slice parameter buffer, so its size being
//!   right is load-bearing for every offset after it.
//! * `VAPictureParameterBufferH264` is **672** bytes, `VAIQMatrixBufferH264` **240**,
//!   and `VASliceParameterBufferH264` **3128** — the last one because it carries two
//!   32-entry reference lists *and* the full prediction weight tables inline.
//! * The three deprecated FMO fields (`num_slice_groups_minus1`,
//!   `slice_group_map_type`, `slice_group_change_rate_minus1`) still occupy bytes
//!   624..628. Deprecated does not mean absent: dropping them would shift every
//!   later field. They are declared, and always zero.
//! * C bit-fields on this ABI allocate from the **least significant bit**, proven
//!   rather than assumed: setting `log2_max_frame_num_minus4` (the 4 bits declared
//!   after eight single-bit flags and a 2-bit field) to `0xf` yields `0x0000_0f00`,
//!   and `weighted_bipred_idc = 3` yields `0x0000_000c`.
//!
//! # Surface identity
//!
//! `VAPictureH264::picture_id` is a `VASurfaceID`, not a slot index — unlike DXVA,
//! where the surface index and the DPB slot are the same number by construction.
//! This crate never invents one: the conversion (`plan_to_va`) takes the caller's
//! slot → `VASurfaceID` table and indexes it, so the Linux layer owns surface
//! allocation and this half stays pure.

/// `VA_INVALID_SURFACE` — what an unused `ReferenceFrames` / `RefPicList` entry
/// carries. Paired with [`VA_PICTURE_H264_INVALID`]; drivers key on the flag, but a
/// stale surface id in an "invalid" entry is the kind of thing that decodes fine on
/// one vendor and not another, so both are always written together.
pub const VA_INVALID_SURFACE: u32 = 0xffff_ffff;

/// `VABufferType` for the four buffers a decode submits, measured off real headers
/// by `layout-probe.c` rather than counted off the enum in the header.
///
/// ⚠ The last two are the trap: `VASliceParameterBufferType` is **4** and
/// `VASliceDataBufferType` is **5**, not the 3 and 4 that counting from the top
/// gives — `VABitPlaneBufferType` and `VASliceGroupMapBufferType` sit in between
/// for the codecs that need them. Getting these wrong hands the driver a slice as
/// if it were something else, which is not a decode error but a decode of garbage.
pub const VA_PICTURE_PARAMETER_BUFFER_TYPE: u32 = 0;
pub const VA_IQ_MATRIX_BUFFER_TYPE: u32 = 1;
pub const VA_SLICE_PARAMETER_BUFFER_TYPE: u32 = 4;
pub const VA_SLICE_DATA_BUFFER_TYPE: u32 = 5;

/// Flags for [`VaPictureH264::flags`].
pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_TOP_FIELD: u32 = 0x0000_0002;
pub const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 0x0000_0004;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x0000_0010;

/// `VA_SLICE_DATA_FLAG_ALL` — this buffer holds the whole slice, which is the only
/// shape we submit (the wire delivers complete access units; nothing here streams a
/// slice in fragments).
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0x00;

/// `VAPictureH264` — one DPB entry, or the current picture.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureH264 {
    /// `VASurfaceID` of the decode surface holding this picture.
    pub picture_id: u32,
    /// `frame_num` for a short-term reference, `LongTermFrameIdx` for a long-term
    /// one — the same pair DXVA and Vulkan key references by, which is why
    /// [`pf_bitstream::h264::RefPic`] already carries exactly this.
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// `va_reserved[VA_PADDING_LOW]` — "must be zero".
    pub va_reserved: [u32; 4],
}

impl VaPictureH264 {
    /// The entry an unused reference slot carries: invalid flag AND invalid surface.
    pub const fn invalid() -> Self {
        VaPictureH264 {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        }
    }
}

/// `VAPictureParameterBufferH264::seq_fields`, unpacked.
///
/// Declared as its own type rather than as a bare `u32` so the bit layout lives
/// beside the structure it belongs to and can be unit-tested on its own; [`Self::pack`]
/// is the only place the shifts appear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqFieldsH264 {
    pub chroma_format_idc: u8,
    /// `residual_colour_transform_flag` in the header's older spelling; the standard
    /// renamed it `separate_colour_plane_flag`.
    pub separate_colour_plane_flag: bool,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// A.3.3.2 — level-derived, not an SPS syntax element.
    pub min_luma_bi_pred_size8x8: bool,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: bool,
}

impl SeqFieldsH264 {
    /// Pack to the `uint32_t` the union aliases. Bit positions are the measured
    /// allocation order (module docs), LSB first, in declaration order.
    pub const fn pack(self) -> u32 {
        (self.chroma_format_idc as u32 & 0x3)
            | ((self.separate_colour_plane_flag as u32) << 2)
            | ((self.gaps_in_frame_num_value_allowed_flag as u32) << 3)
            | ((self.frame_mbs_only_flag as u32) << 4)
            | ((self.mb_adaptive_frame_field_flag as u32) << 5)
            | ((self.direct_8x8_inference_flag as u32) << 6)
            | ((self.min_luma_bi_pred_size8x8 as u32) << 7)
            | ((self.log2_max_frame_num_minus4 as u32 & 0xf) << 8)
            | ((self.pic_order_cnt_type as u32 & 0x3) << 12)
            | ((self.log2_max_pic_order_cnt_lsb_minus4 as u32 & 0xf) << 14)
            | ((self.delta_pic_order_always_zero_flag as u32) << 18)
    }
}

/// `VAPictureParameterBufferH264::pic_fields`, unpacked. See [`SeqFieldsH264`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PicFieldsH264 {
    pub entropy_coding_mode_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub transform_8x8_mode_flag: bool,
    pub field_pic_flag: bool,
    pub constrained_intra_pred_flag: bool,
    /// `bottom_field_pic_order_in_frame_present_flag` in current spec spelling.
    pub pic_order_present_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    /// `nal_ref_idc != 0` — a statement about THIS picture, not the PPS.
    pub reference_pic_flag: bool,
}

impl PicFieldsH264 {
    pub const fn pack(self) -> u32 {
        (self.entropy_coding_mode_flag as u32)
            | ((self.weighted_pred_flag as u32) << 1)
            | ((self.weighted_bipred_idc as u32 & 0x3) << 2)
            | ((self.transform_8x8_mode_flag as u32) << 4)
            | ((self.field_pic_flag as u32) << 5)
            | ((self.constrained_intra_pred_flag as u32) << 6)
            | ((self.pic_order_present_flag as u32) << 7)
            | ((self.deblocking_filter_control_present_flag as u32) << 8)
            | ((self.redundant_pic_cnt_present_flag as u32) << 9)
            | ((self.reference_pic_flag as u32) << 10)
    }
}

/// `VAPictureParameterBufferH264` — one per picture, before any slice data.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureParameterBufferH264 {
    pub curr_pic: VaPictureH264,
    /// The DPB, not this AU's reference lists. VAAPI documents this as "in DPB",
    /// which is the same statement DXVA's `RefFrameList` makes and the opposite of
    /// Vulkan's `pReferenceSlots` — so it is filled from pf-bitstream's per-AU
    /// `dpb_refs` snapshot, the accessor M5 added for exactly this distinction.
    pub reference_frames: [VaPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    /// Packed [`SeqFieldsH264`]. (One byte of padding precedes it — `num_ref_frames`
    /// ends at 619 and the union is 4-aligned at 620.)
    pub seq_fields: u32,
    /// Deprecated FMO fields. Still occupy bytes 624..628; always zero here, and
    /// the conversion (`plan_to_va`) refuses a stream that uses slice groups rather
    /// than silently ignoring them.
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// Packed [`PicFieldsH264`].
    pub pic_fields: u32,
    pub frame_num: u16,
    /// `va_reserved[VA_PADDING_MEDIUM]`. Two bytes of padding precede it (`frame_num`
    /// ends at 638, the array is 4-aligned at 640).
    pub va_reserved: [u32; 8],
}

/// `VAIQMatrixBufferH264` — both scaling list sets, raster scan order.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaIqMatrixBufferH264 {
    pub scaling_list4x4: [[u8; 16]; 6],
    pub scaling_list8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; 4],
}

/// `VASliceParameterBufferH264` — one per slice NALU.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    /// Bit offset from the start of the NAL unit byte to the first bit of
    /// `slice_data()`, counted **after emulation-prevention bytes are removed** even
    /// though the buffer handed to the driver still contains them.
    ///
    /// Nothing else in this program needs this number: DXVA takes a byte offset to
    /// the slice and Vulkan takes none at all. The vendored parser records it as
    /// `SliceHeader::header_bit_size` because its own production backend is VAAPI,
    /// so it costs no new parsing — see the crate docs.
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    /// 8.2.4.2 reference lists — the AU's own, unlike
    /// [`VaPictureParameterBufferH264::reference_frames`].
    pub ref_pic_list0: [VaPictureH264; 32],
    pub ref_pic_list1: [VaPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; 4],
}

impl VaSliceParameterBufferH264 {
    /// An all-zero record with the reference lists invalidated — the base every
    /// slice is built from, so an unwritten entry is never a stale surface id.
    pub const fn zeroed() -> Self {
        VaSliceParameterBufferH264 {
            slice_data_size: 0,
            slice_data_offset: 0,
            slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
            slice_data_bit_offset: 0,
            first_mb_in_slice: 0,
            slice_type: 0,
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            ref_pic_list0: [VaPictureH264::invalid(); 32],
            ref_pic_list1: [VaPictureH264::invalid(); 32],
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: 0,
            luma_weight_l0_flag: 0,
            luma_weight_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_weight_l0_flag: 0,
            chroma_weight_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_weight_l1_flag: 0,
            luma_weight_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_weight_l1_flag: 0,
            chroma_weight_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
            va_reserved: [0; 4],
        }
    }
}

// ---------------------------------------------------------------------------
// Layout proofs — the probe's output, pinned.
//
// libva 2.23.0, x86_64-linux-gnu. A `#[repr(C)]` Rust struct and a C struct agree
// by definition of repr(C), so these assertions are not testing the compiler: they
// are testing that the FIELDS AND THEIR ORDER above match the header, which is the
// part a human transcribed and can get wrong.
// ---------------------------------------------------------------------------

const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaPictureH264>() == 36);
    assert!(offset_of!(VaPictureH264, picture_id) == 0);
    assert!(offset_of!(VaPictureH264, frame_idx) == 4);
    assert!(offset_of!(VaPictureH264, flags) == 8);
    assert!(offset_of!(VaPictureH264, top_field_order_cnt) == 12);
    assert!(offset_of!(VaPictureH264, bottom_field_order_cnt) == 16);
    assert!(offset_of!(VaPictureH264, va_reserved) == 20);

    assert!(size_of::<VaPictureParameterBufferH264>() == 672);
    assert!(offset_of!(VaPictureParameterBufferH264, curr_pic) == 0);
    assert!(offset_of!(VaPictureParameterBufferH264, reference_frames) == 36);
    assert!(offset_of!(VaPictureParameterBufferH264, picture_width_in_mbs_minus1) == 612);
    assert!(offset_of!(VaPictureParameterBufferH264, picture_height_in_mbs_minus1) == 614);
    assert!(offset_of!(VaPictureParameterBufferH264, bit_depth_luma_minus8) == 616);
    assert!(offset_of!(VaPictureParameterBufferH264, bit_depth_chroma_minus8) == 617);
    assert!(offset_of!(VaPictureParameterBufferH264, num_ref_frames) == 618);
    assert!(offset_of!(VaPictureParameterBufferH264, seq_fields) == 620);
    assert!(offset_of!(VaPictureParameterBufferH264, num_slice_groups_minus1) == 624);
    assert!(offset_of!(VaPictureParameterBufferH264, slice_group_map_type) == 625);
    assert!(offset_of!(VaPictureParameterBufferH264, slice_group_change_rate_minus1) == 626);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_init_qp_minus26) == 628);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_init_qs_minus26) == 629);
    assert!(offset_of!(VaPictureParameterBufferH264, chroma_qp_index_offset) == 630);
    assert!(offset_of!(VaPictureParameterBufferH264, second_chroma_qp_index_offset) == 631);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_fields) == 632);
    assert!(offset_of!(VaPictureParameterBufferH264, frame_num) == 636);
    assert!(offset_of!(VaPictureParameterBufferH264, va_reserved) == 640);

    assert!(size_of::<VaIqMatrixBufferH264>() == 240);
    assert!(offset_of!(VaIqMatrixBufferH264, scaling_list4x4) == 0);
    assert!(offset_of!(VaIqMatrixBufferH264, scaling_list8x8) == 96);
    assert!(offset_of!(VaIqMatrixBufferH264, va_reserved) == 224);

    assert!(size_of::<VaSliceParameterBufferH264>() == 3128);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_size) == 0);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_offset) == 4);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_flag) == 8);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_bit_offset) == 12);
    assert!(offset_of!(VaSliceParameterBufferH264, first_mb_in_slice) == 14);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_type) == 16);
    assert!(offset_of!(VaSliceParameterBufferH264, direct_spatial_mv_pred_flag) == 17);
    assert!(offset_of!(VaSliceParameterBufferH264, num_ref_idx_l0_active_minus1) == 18);
    assert!(offset_of!(VaSliceParameterBufferH264, num_ref_idx_l1_active_minus1) == 19);
    assert!(offset_of!(VaSliceParameterBufferH264, cabac_init_idc) == 20);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_qp_delta) == 21);
    assert!(offset_of!(VaSliceParameterBufferH264, disable_deblocking_filter_idc) == 22);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_alpha_c0_offset_div2) == 23);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_beta_offset_div2) == 24);
    assert!(offset_of!(VaSliceParameterBufferH264, ref_pic_list0) == 28);
    assert!(offset_of!(VaSliceParameterBufferH264, ref_pic_list1) == 1180);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_log2_weight_denom) == 2332);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_log2_weight_denom) == 2333);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l0_flag) == 2334);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l0) == 2336);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_offset_l0) == 2400);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l0_flag) == 2464);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l0) == 2466);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_offset_l0) == 2594);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l1_flag) == 2722);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l1) == 2724);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_offset_l1) == 2788);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l1_flag) == 2852);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l1) == 2854);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_offset_l1) == 2982);
    assert!(offset_of!(VaSliceParameterBufferH264, va_reserved) == 3112);
};

#[cfg(test)]
mod tests {
    use super::*;

    // The three bit patterns the probe read back off real headers. If the shifts
    // above are ever "tidied", these fail with the measured value in hand.
    #[test]
    fn seq_fields_pack_where_the_probe_measured() {
        assert_eq!(
            SeqFieldsH264 {
                chroma_format_idc: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0003
        );
        assert_eq!(
            SeqFieldsH264 {
                log2_max_frame_num_minus4: 0xf,
                ..Default::default()
            }
            .pack(),
            0x0000_0f00
        );
    }

    #[test]
    fn pic_fields_pack_where_the_probe_measured() {
        assert_eq!(
            PicFieldsH264 {
                reference_pic_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0400
        );
        assert_eq!(
            PicFieldsH264 {
                weighted_bipred_idc: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_000c
        );
    }

    #[test]
    fn every_seq_field_owns_a_distinct_bit_range() {
        // Each field set alone must light only its own bits, and the OR of all of
        // them must equal the packing of all-at-once: a shift typo that overlapped
        // two fields would still pass the two probe vectors above.
        let each = [
            SeqFieldsH264 {
                chroma_format_idc: 3,
                ..Default::default()
            },
            SeqFieldsH264 {
                separate_colour_plane_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                gaps_in_frame_num_value_allowed_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                frame_mbs_only_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                mb_adaptive_frame_field_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                direct_8x8_inference_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                min_luma_bi_pred_size8x8: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                log2_max_frame_num_minus4: 0xf,
                ..Default::default()
            },
            SeqFieldsH264 {
                pic_order_cnt_type: 3,
                ..Default::default()
            },
            SeqFieldsH264 {
                log2_max_pic_order_cnt_lsb_minus4: 0xf,
                ..Default::default()
            },
            SeqFieldsH264 {
                delta_pic_order_always_zero_flag: true,
                ..Default::default()
            },
        ];
        let mut seen = 0u32;
        for f in each {
            let bits = f.pack();
            assert_ne!(bits, 0, "a field packed to nothing");
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        }
        let all = SeqFieldsH264 {
            chroma_format_idc: 3,
            separate_colour_plane_flag: true,
            gaps_in_frame_num_value_allowed_flag: true,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: true,
            direct_8x8_inference_flag: true,
            min_luma_bi_pred_size8x8: true,
            log2_max_frame_num_minus4: 0xf,
            pic_order_cnt_type: 3,
            log2_max_pic_order_cnt_lsb_minus4: 0xf,
            delta_pic_order_always_zero_flag: true,
        };
        assert_eq!(all.pack(), seen);
        // Nothing may reach past bit 18 — the last declared bit.
        assert_eq!(seen & !0x0007_ffff, 0);
    }

    #[test]
    fn every_pic_field_owns_a_distinct_bit_range() {
        let each = [
            PicFieldsH264 {
                entropy_coding_mode_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                weighted_pred_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                weighted_bipred_idc: 3,
                ..Default::default()
            },
            PicFieldsH264 {
                transform_8x8_mode_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                field_pic_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                constrained_intra_pred_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                pic_order_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                deblocking_filter_control_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                redundant_pic_cnt_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                reference_pic_flag: true,
                ..Default::default()
            },
        ];
        let mut seen = 0u32;
        for f in each {
            let bits = f.pack();
            assert_ne!(bits, 0);
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        }
        assert_eq!(seen & !0x0000_07ff, 0);
    }

    #[test]
    fn an_unused_reference_entry_is_invalid_in_both_ways() {
        let e = VaPictureH264::invalid();
        assert_eq!(e.flags, VA_PICTURE_H264_INVALID);
        assert_eq!(e.picture_id, VA_INVALID_SURFACE);
    }

    #[test]
    fn a_zeroed_slice_record_starts_with_invalidated_lists() {
        let s = VaSliceParameterBufferH264::zeroed();
        assert!(s
            .ref_pic_list0
            .iter()
            .all(|e| e.flags == VA_PICTURE_H264_INVALID));
        assert!(s
            .ref_pic_list1
            .iter()
            .all(|e| e.picture_id == VA_INVALID_SURFACE));
        assert_eq!(s.slice_data_flag, VA_SLICE_DATA_FLAG_ALL);
    }
}
