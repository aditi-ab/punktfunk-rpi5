//! The libva decode buffer layouts for H.264, **hand-declared** — plus the
//! codec-independent `VAImage` pair the test-only surface readback needs.
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
//!
//! # The image half, and why it is here at all
//!
//! [`VaImage`] and [`VaImageFormat`] are not decode buffers: they are what
//! `vaDeriveImage` (or `vaCreateImage` + `vaGetImage`) writes back when something
//! wants to READ a decoded surface on the CPU. Nothing on the production video path
//! does — the rung exports a DRM-PRIME dmabuf and the presenter samples it, which is
//! the zero-copy contract this project refuses to spend — so the only caller is the
//! frame-hash parity harness in `pf-client-core`'s `video_vaapi_native::parity`, which
//! exists solely under `#[cfg(test)]`.
//!
//! They live here for the same reason every other structure in this file does: the
//! harness must not force a `libva-dev` build dependency on a crate that compiles on
//! macOS and in the container. Declaring them costs nothing at runtime (nothing
//! constructs one outside a test) and lets the readback's geometry — the part that has
//! already cost this program a release, in the shape of a chroma plane read at the
//! DISPLAY height instead of the driver's reported offset — be unit-tested with no
//! device at all. That walk is [`pack_two_plane`].

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

// ---------------------------------------------------------------------------
// The image pair — a CPU-readable view of a decoded surface (module docs).
//
// ⚠ TEST-ONLY BY CONSTRUCTION. Nothing on the production video path maps a surface;
// these types exist so a parity harness can, without this crate growing a libva
// build dependency. `pack_two_plane` below is pure and is the only logic here.
// ---------------------------------------------------------------------------

/// `VA_LSB_FIRST` — the byte order every YUV format libva describes uses. Named
/// because [`VaImageFormat`] carries the field and a zero there is not a "left unset",
/// it is an invalid enumerator.
pub const VA_LSB_FIRST: u32 = 1;
/// `VA_MSB_FIRST` — measured beside it so the pair reads as an enumeration rather
/// than as one magic number.
pub const VA_MSB_FIRST: u32 = 2;

/// `VAImageFormat` — what a `VAImage` is in, and what `vaCreateImage` is asked for.
///
/// The RGB fields are dead weight for this crate's two formats (NV12 and P010) and
/// are declared anyway: they occupy bytes 12..32 and dropping them would shift
/// `va_reserved`, which is exactly the class of mistake the assertions below exist
/// to make a compile error.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VaImageFormat {
    pub fourcc: u32,
    /// [`VA_LSB_FIRST`] or [`VA_MSB_FIRST`].
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    /// RGB only.
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    /// `va_reserved[VA_PADDING_LOW]` — "must be zero".
    pub va_reserved: [u32; 4],
}

/// `VAImage` — the descriptor `vaDeriveImage` / `vaCreateImage` fills in.
///
/// ⚠ `width` and `height` are **`unsigned short`**, not `unsigned int`. That is the
/// one thing about this structure a reader would get wrong by counting 32-bit words:
/// every field after them sits two bytes earlier than the obvious arithmetic puts it,
/// which is why `data_size` is at 60 and not 64. Measured, not reasoned about.
///
/// `pitches` and `offsets` are per PLANE and are the driver's own — the chroma plane
/// begins at `offsets[1]`, which on a decode surface is nowhere near
/// `pitches[0] * display_height` because the surface is padded to the codec's granule.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaImage {
    /// `VAImageID`, and what `vaGetImage` / `vaDestroyImage` are handed.
    pub image_id: u32,
    pub format: VaImageFormat,
    /// `VABufferID` — the buffer `vaMapBuffer` returns the pixels of.
    pub buf: u32,
    pub width: u16,
    pub height: u16,
    /// The whole mapped extent, in bytes. Everything [`pack_two_plane`] reads is
    /// bounds-checked against it.
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    /// Palette fields, meaningless for YUV and declared for their bytes.
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    /// `va_reserved[VA_PADDING_LOW]`.
    pub va_reserved: [u32; 4],
}

impl VaImage {
    /// An all-zero descriptor — what a caller hands `vaDeriveImage` to fill.
    ///
    /// Zero rather than uninitialised on purpose: a failed derive leaves a descriptor
    /// the caller still reads — to decide whether there is an image to destroy, and to
    /// report what the driver DID hand back — and reading uninitialised bytes to do
    /// that is undefined behaviour rather than a diagnostic.
    pub const fn zeroed() -> VaImage {
        VaImage {
            image_id: 0,
            format: VaImageFormat {
                fourcc: 0,
                byte_order: 0,
                bits_per_pixel: 0,
                depth: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                alpha_mask: 0,
                va_reserved: [0; 4],
            },
            buf: 0,
            width: 0,
            height: 0,
            data_size: 0,
            num_planes: 0,
            pitches: [0; 3],
            offsets: [0; 3],
            num_palette_entries: 0,
            entry_bytes: 0,
            component_order: [0; 4],
            va_reserved: [0; 4],
        }
    }
}

/// Why a mapped image could not be read as the picture it was supposed to hold.
///
/// Every arm carries what the DRIVER said rather than a verdict, because the whole
/// point of this walk refusing instead of guessing is that the refusal names the
/// thing that has to be looked at next. A harness that quietly produced a short or
/// mis-strided buffer would compare hashes of garbage against libavcodec's and report
/// a decode defect that is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageReadError {
    /// The caller asked for a format this walk does not describe. Only the two
    /// two-plane YUV formats the surface pool is ever created with are supported.
    UnsupportedFourcc { fourcc: u32 },
    /// The image came back in a different format from the surface pool's — a driver
    /// that substituted, which is precisely the "derive handed you something you
    /// cannot interpret" case.
    Fourcc { got: u32, want: u32 },
    /// Fewer than two planes: a packed or opaque layout, not NV12/P010.
    NotTwoPlane { planes: u32 },
    /// The image is smaller than the region asked for.
    TooSmall {
        image: (u32, u32),
        display: (u32, u32),
    },
    /// A row of the picture does not fit the plane's own pitch.
    Pitch {
        plane: usize,
        pitch: u32,
        need: usize,
    },
    /// A row would be read past the end of the mapped buffer.
    OutOfBounds {
        plane: usize,
        row: u32,
        at: usize,
        end: usize,
        mapped: usize,
    },
}

impl std::fmt::Display for ImageReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageReadError::UnsupportedFourcc { fourcc } => {
                write!(f, "no two-plane layout for fourcc {}", fourcc_name(*fourcc))
            }
            ImageReadError::Fourcc { got, want } => write!(
                f,
                "the image is {} but the surface pool is {}",
                fourcc_name(*got),
                fourcc_name(*want)
            ),
            ImageReadError::NotTwoPlane { planes } => {
                write!(f, "the image has {planes} plane(s), not two")
            }
            ImageReadError::TooSmall { image, display } => write!(
                f,
                "the image is {}x{} but the picture is {}x{}",
                image.0, image.1, display.0, display.1
            ),
            ImageReadError::Pitch { plane, pitch, need } => write!(
                f,
                "plane {plane}'s pitch is {pitch} bytes, a row needs {need}"
            ),
            ImageReadError::OutOfBounds {
                plane,
                row,
                at,
                end,
                mapped,
            } => write!(
                f,
                "plane {plane} row {row} spans {at}..{end} of a {mapped}-byte mapping"
            ),
        }
    }
}

impl std::error::Error for ImageReadError {}

/// A fourcc as its four characters, for a message a human can act on.
fn fourcc_name(fourcc: u32) -> String {
    let bytes = fourcc.to_le_bytes();
    match std::str::from_utf8(&bytes) {
        Ok(s) if bytes.iter().all(|b| b.is_ascii_graphic()) => s.to_string(),
        _ => format!("{fourcc:#010x}"),
    }
}

/// How many bytes one tightly packed `display`-sized picture of `fourcc` occupies —
/// the layout every golden set in this program hashes.
///
/// `None` for a fourcc with no two-plane 4:2:0 layout here.
pub fn packed_len(display: (u32, u32), fourcc: u32) -> Option<usize> {
    let bytes_per_sample = bytes_per_sample(fourcc)?;
    let (w, h) = (display.0 as usize, display.1 as usize);
    Some(w * bytes_per_sample * (h + h.div_ceil(2)))
}

/// One luma sample's size in bytes for the two formats the pool is ever built with.
fn bytes_per_sample(fourcc: u32) -> Option<usize> {
    match fourcc {
        crate::drm::VA_FOURCC_NV12 => Some(1),
        // ⚠ P010 is 16 bits per sample with the ten meaningful bits in the HIGH end
        // of each little-endian word. This walk moves bytes and never touches the
        // alignment; a driver that handed back LSB-aligned samples would produce a
        // buffer of exactly the right SIZE and the wrong content, which is a
        // divergence the goldens catch and this function cannot.
        crate::drm::VA_FOURCC_P010 => Some(2),
        _ => None,
    }
}

/// Read the `display`-sized picture out of a mapped `VAImage`, packed tightly — byte
/// for byte the layout `pf-vkdecode`'s golden files hash.
///
/// This is the whole of the readback that can be wrong without a device, so it is the
/// whole of what is worth testing without one. Three things it does deliberately:
///
/// * **The chroma plane starts at `offsets[1]`**, the driver's own number, never at
///   `pitches[0] * height`. A decode surface is padded to the codec's granule — 240
///   lines of HEVC live in a 256-line surface — so computing the offset from the
///   display height reads the tail of the luma padding as chroma and smears every
///   row. This project has already paid for that once on another rung.
/// * **Padding columns are dropped per row.** `pitches[0]` is the surface's stride,
///   which is wider than the picture; only `width * bytes_per_sample` bytes of each
///   row belong to the golden.
/// * **Every read is bounds-checked against the mapping the driver declared**, and a
///   failure is returned rather than clamped. A short mapping means the descriptor
///   and the buffer disagree, and no hash taken from it means anything.
///
/// `mapped` must be the buffer `vaMapBuffer` returned, of length
/// [`VaImage::data_size`]; the caller passes it as a slice so this function needs no
/// `unsafe` and can be driven from a plain array in a test.
pub fn pack_two_plane(
    image: &VaImage,
    mapped: &[u8],
    display: (u32, u32),
    fourcc: u32,
) -> Result<Vec<u8>, ImageReadError> {
    let bytes_per_sample =
        bytes_per_sample(fourcc).ok_or(ImageReadError::UnsupportedFourcc { fourcc })?;
    if image.format.fourcc != fourcc {
        return Err(ImageReadError::Fourcc {
            got: image.format.fourcc,
            want: fourcc,
        });
    }
    if image.num_planes < 2 {
        return Err(ImageReadError::NotTwoPlane {
            planes: image.num_planes,
        });
    }
    let (width, height) = display;
    if u32::from(image.width) < width || u32::from(image.height) < height {
        return Err(ImageReadError::TooSmall {
            image: (u32::from(image.width), u32::from(image.height)),
            display,
        });
    }
    // One row of the picture, in both planes: 4:2:0 chroma is half the rows but
    // interleaved (U,V) pairs, so a chroma row carries exactly as many BYTES as a
    // luma row.
    let row_bytes = width as usize * bytes_per_sample;
    let rows = [height, height.div_ceil(2)];
    let mut out = Vec::with_capacity(row_bytes * (rows[0] + rows[1]) as usize);
    for (plane, plane_rows) in rows.iter().enumerate() {
        let pitch = image.pitches[plane] as usize;
        if pitch < row_bytes {
            return Err(ImageReadError::Pitch {
                plane,
                pitch: image.pitches[plane],
                need: row_bytes,
            });
        }
        let base = image.offsets[plane] as usize;
        for row in 0..*plane_rows {
            let at = base + row as usize * pitch;
            let end = at + row_bytes;
            if end > mapped.len() {
                return Err(ImageReadError::OutOfBounds {
                    plane,
                    row,
                    at,
                    end,
                    mapped: mapped.len(),
                });
            }
            out.extend_from_slice(&mapped[at..end]);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Image layout proofs — the probe's output, pinned (libva 2.23.0-1ubuntu1,
// x86_64-linux-gnu, measured 2026-08-07 by `layout-probe.c`).
// ---------------------------------------------------------------------------

const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaImageFormat>() == 48);
    assert!(offset_of!(VaImageFormat, fourcc) == 0);
    assert!(offset_of!(VaImageFormat, byte_order) == 4);
    assert!(offset_of!(VaImageFormat, bits_per_pixel) == 8);
    assert!(offset_of!(VaImageFormat, depth) == 12);
    assert!(offset_of!(VaImageFormat, red_mask) == 16);
    assert!(offset_of!(VaImageFormat, green_mask) == 20);
    assert!(offset_of!(VaImageFormat, blue_mask) == 24);
    assert!(offset_of!(VaImageFormat, alpha_mask) == 28);
    assert!(offset_of!(VaImageFormat, va_reserved) == 32);

    // ⚠ `width`/`height` are 16-bit, which is why `data_size` is at 60 rather than
    // at the 64 that counting 32-bit fields would give.
    assert!(size_of::<VaImage>() == 120);
    assert!(offset_of!(VaImage, image_id) == 0);
    assert!(offset_of!(VaImage, format) == 4);
    assert!(offset_of!(VaImage, buf) == 52);
    assert!(offset_of!(VaImage, width) == 56);
    assert!(offset_of!(VaImage, height) == 58);
    assert!(offset_of!(VaImage, data_size) == 60);
    assert!(offset_of!(VaImage, num_planes) == 64);
    assert!(offset_of!(VaImage, pitches) == 68);
    assert!(offset_of!(VaImage, offsets) == 80);
    assert!(offset_of!(VaImage, num_palette_entries) == 92);
    assert!(offset_of!(VaImage, entry_bytes) == 96);
    assert!(offset_of!(VaImage, component_order) == 100);
    assert!(offset_of!(VaImage, va_reserved) == 104);
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

    // -----------------------------------------------------------------------
    // The image walk. Every one of these runs on macOS and in the container: the
    // geometry is the half of a surface readback that can be wrong without a
    // device, and it is the half that has been wrong before.
    // -----------------------------------------------------------------------

    /// A driver-shaped `VAImage`: a surface PADDED past the picture in both axes,
    /// with the chroma plane where the driver puts it rather than where the display
    /// height would.
    // `_picture` is named at every call site so each test reads as the shape it is
    // about, and is deliberately not consulted: the walk takes the picture size from
    // its own argument, which is the whole point of the crop.
    fn padded_image(
        _picture: (u16, u16),
        surface: (u16, u16),
        pitch: u32,
        fourcc: u32,
    ) -> (VaImage, Vec<u8>) {
        let mut image = VaImage::zeroed();
        image.format.fourcc = fourcc;
        image.format.byte_order = VA_LSB_FIRST;
        image.width = surface.0;
        image.height = surface.1;
        image.num_planes = 2;
        image.pitches = [pitch, pitch, 0];
        // The trap, expressed: chroma starts after the WHOLE padded luma plane.
        image.offsets = [0, pitch * u32::from(surface.1), 0];
        let total = pitch as usize * (surface.1 as usize + surface.1.div_ceil(2) as usize);
        image.data_size = total as u32;
        // Fill the mapping so every byte says where it came from: luma rows count
        // 0.., chroma rows 128.., and the padding columns are 0xff so a walk that
        // read them would produce something unmistakable.
        let mut mapped = vec![0xffu8; total];
        for y in 0..surface.1 as usize {
            for x in 0..pitch as usize {
                mapped[y * pitch as usize + x] = if x < surface.0 as usize {
                    (y % 100) as u8
                } else {
                    0xff
                };
            }
        }
        let chroma = image.offsets[1] as usize;
        for y in 0..surface.1.div_ceil(2) as usize {
            for x in 0..pitch as usize {
                mapped[chroma + y * pitch as usize + x] = if x < surface.0 as usize {
                    128 + (y % 100) as u8
                } else {
                    0xff
                };
            }
        }
        (image, mapped)
    }

    #[test]
    fn the_walk_crops_to_the_picture_and_takes_chroma_from_the_drivers_offset() {
        // 320x240 picture in a 320x256 surface at a 384-byte pitch — HEVC's 128-line
        // granule and a stride that is not the width, which is the everyday shape.
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        let out = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the walk must read a padded NV12 surface");
        assert_eq!(out.len(), 320 * 240 + 320 * 120);
        assert_eq!(
            out.len(),
            packed_len((320, 240), crate::drm::VA_FOURCC_NV12).unwrap()
        );
        // No padding byte reached the output: 0xff is only ever a padding column.
        assert!(
            !out.contains(&0xff),
            "a padding column leaked into the packed picture"
        );
        // Luma row 3 is all 3s; chroma row 3 is all 131 — which is only true if the
        // chroma plane was taken from offsets[1] and not from pitch * 240.
        assert!(out[3 * 320..4 * 320].iter().all(|&b| b == 3));
        let chroma = 320 * 240;
        assert!(out[chroma + 3 * 320..chroma + 4 * 320]
            .iter()
            .all(|&b| b == 131));
    }

    #[test]
    fn reading_chroma_at_the_display_height_would_have_been_caught() {
        // The counterfactual for the assertion above: an image that claims chroma
        // starts at `pitch * display_height` — the 1088-row smear — hands back
        // LUMA padding rows where chroma belongs, and the walk cannot tell. So the
        // guarantee is that the walk uses the DRIVER's offset, and this proves the
        // two answers actually differ on the shape the drivers hand out (they would
        // coincide on an unpadded surface, which is why the test above uses one that
        // is padded in BOTH axes).
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        let right = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the driver's own offset reads");
        image.offsets[1] = 384 * 240;
        let wrong = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the wrong offset also reads — that is the point");
        assert_ne!(
            right, wrong,
            "chroma at the display height must differ from chroma at the driver's \
             offset, or this walk's central claim is untestable"
        );
    }

    #[test]
    fn ten_bit_rows_are_twice_as_wide() {
        // P010's samples are 16 bits, so a 320-sample row is 640 bytes and the packed
        // picture is exactly twice an NV12 one. A walk that assumed one byte per
        // sample would produce a half-width picture of the right total length for
        // some other resolution, which is the kind of thing a length check alone
        // misses.
        let (image, mapped) = padded_image((320, 240), (320, 256), 768, crate::drm::VA_FOURCC_P010);
        let out = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_P010)
            .expect("the walk must read a padded P010 surface");
        assert_eq!(out.len(), 320 * 2 * 240 + 320 * 2 * 120);
        assert_eq!(
            out.len(),
            packed_len((320, 240), crate::drm::VA_FOURCC_P010).unwrap()
        );
        assert_eq!(
            out.len(),
            2 * packed_len((320, 240), crate::drm::VA_FOURCC_NV12).unwrap()
        );
    }

    #[test]
    fn an_odd_height_keeps_its_half_chroma_row() {
        let (image, mapped) = padded_image((16, 9), (16, 16), 32, crate::drm::VA_FOURCC_NV12);
        let out = pack_two_plane(&image, &mapped, (16, 9), crate::drm::VA_FOURCC_NV12)
            .expect("an odd height still reads");
        assert_eq!(out.len(), 16 * 9 + 16 * 5, "9 luma rows, 5 chroma rows");
    }

    #[test]
    fn a_substituted_format_is_refused_rather_than_reinterpreted() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.format.fourcc = crate::drm::VA_FOURCC_P010;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::Fourcc {
                got: crate::drm::VA_FOURCC_P010,
                want: crate::drm::VA_FOURCC_NV12
            })
        );
    }

    #[test]
    fn a_packed_or_opaque_image_is_refused() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.num_planes = 1;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::NotTwoPlane { planes: 1 })
        );
    }

    #[test]
    fn an_image_smaller_than_the_picture_is_refused() {
        let (image, mapped) = padded_image((320, 240), (320, 240), 384, crate::drm::VA_FOURCC_NV12);
        assert_eq!(
            pack_two_plane(&image, &mapped, (321, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::TooSmall {
                image: (320, 240),
                display: (321, 240)
            })
        );
    }

    #[test]
    fn a_pitch_narrower_than_a_row_is_refused() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.pitches[1] = 16;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::Pitch {
                plane: 1,
                pitch: 16,
                need: 320
            })
        );
    }

    #[test]
    fn a_mapping_shorter_than_the_descriptor_claims_is_refused_not_truncated() {
        // The failure mode that matters most: a short read must NOT silently produce
        // a shorter picture, because its hash would then be a hash of something the
        // decoder never wrote.
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        // One byte short of the LAST chroma row the picture needs. Cutting the tail
        // of the allocation would not do it: the surface is padded past the picture,
        // so there is slack after the last row this walk reads — which is itself
        // worth pinning, since it is why a `data_size` check alone would not catch a
        // driver whose offsets point outside its buffer.
        let last_row_end = image.offsets[1] as usize + 119 * image.pitches[1] as usize + 320;
        assert!(
            last_row_end < mapped.len(),
            "the padded surface must have slack after the picture's last chroma row"
        );
        let err = pack_two_plane(
            &image,
            &mapped[..last_row_end - 1],
            (320, 240),
            crate::drm::VA_FOURCC_NV12,
        )
        .expect_err("a short mapping must be refused");
        assert!(
            matches!(
                err,
                ImageReadError::OutOfBounds {
                    plane: 1,
                    row: 119,
                    ..
                }
            ),
            "expected the last chroma row to be refused, got {err}"
        );
    }

    #[test]
    fn an_unknown_fourcc_has_no_packed_length_and_no_walk() {
        assert_eq!(
            packed_len((320, 240), 0x3132_3449),
            None,
            "I421 is not ours"
        );
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), 0x3132_3449),
            Err(ImageReadError::UnsupportedFourcc {
                fourcc: 0x3132_3449
            })
        );
    }
}
