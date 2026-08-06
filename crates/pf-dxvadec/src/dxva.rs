//! The DXVA buffer layouts, hand-declared.
//!
//! # Why these are written out by hand
//!
//! `ID3D11VideoDecoder` is fed C structures from `dxva.h` — `DXVA_PicParams_H264`,
//! `DXVA_PicParams_HEVC`, their quantization matrices and their slice-control
//! records. **windows-rs does not generate any of them**, verified against the
//! pinned rev (`acb5a1a`): the whole `d3d11` header module is present — the
//! interfaces (`ID3D11VideoDevice::CreateVideoDecoder`,
//! `ID3D11VideoContext::{GetDecoderBuffer, ReleaseDecoderBuffer,
//! SubmitDecoderBuffers, DecoderBeginFrame, DecoderEndFrame}`), the descriptors
//! (`D3D11_VIDEO_DECODER_{DESC,CONFIG,BUFFER_DESC}`) and the buffer-type constants
//! — but `dxva.h` itself is not part of the Win32 metadata, so a grep for
//! `DXVA_PicParams_*`/`DXVA_Slice_*`/`DXVA_Qmatrix_*` across the entire generated
//! tree returns nothing. FFmpeg is in the same position on MinGW and does the same
//! thing (its `compat/` mirrors and `libavcodec/dxva2_*.c`), so this module is the
//! standard answer, not a shortcut.
//!
//! # Why this is the most safety-critical file in the backend
//!
//! Nothing here is type-checked against Windows. A field at the wrong offset, a
//! bitfield packed from the wrong end, a `CHAR` declared `u8` — none of that is a
//! compile error; it is a driver reading garbage where a QP or a reference index
//! should be, which surfaces as silent picture corruption on someone else's screen.
//! Three defences, all of them in this file:
//!
//! 1. **Every struct's size AND every field's offset** is asserted at compile time
//!    against the value derived from the C declaration reproduced in each struct's
//!    doc comment (`const _: () = { … }` blocks, the same technique
//!    `video_d3d11.rs` uses to pin libav's `AVD3D11VA*Context` ABI). A field
//!    inserted, reordered or mis-typed cannot build.
//! 2. **Bitfield words are never expressed as Rust "bitfields"** (Rust has none):
//!    each packed word is a plain integer with named `set_*` builders whose bit
//!    positions are written as literals next to the C declaration they come from.
//!    MSVC allocates C bitfields from the least significant bit of the storage
//!    unit upward in declaration order, which is what the literals encode, and
//!    which the tests below check against independently-derived expected words.
//! 3. **No `unsafe` reaches the layout.** Every struct is constructed field by
//!    field from a `const fn zeroed()` (real zeros, not `mem::zeroed`), so this
//!    module compiles and its tests run on macOS and Linux exactly as on Windows.
//!    The one unsafe in the crate is [`as_bytes`], and it is fenced behind a
//!    sealed trait that only these `#[repr(C)]` PODs implement.
//!
//! # Alignment — and the one place natural alignment is WRONG
//!
//! `dxva.h` declares these as wire-format structures under **1-byte packing**, not
//! under MSVC's default. For five of the six that is indistinguishable from natural
//! alignment, because every member happens to sit at a naturally-aligned offset and
//! every total is already a multiple of 4: `DXVA_PicParams_H264` is 1040,
//! `DXVA_PicParams_HEVC` 232, the two quantization matrices 224 and 1000 — all
//! confirmed against libavcodec's runtime `sizeof` in the n8.1 capture described in
//! `tests/libav_picparams_parity.rs`.
//!
//! The slice-control records are the exception and the reason this section exists.
//! `{UINT, UINT, USHORT}` is **10 bytes packed and 12 under natural alignment**, and
//! an earlier revision of this file declared them plain `#[repr(C)]` — asserting 12
//! with "2 bytes tail padding" in the comment, which was a guess dressed as a proof.
//! Measured on hardware (RTX 4090, patched FFmpeg n8.1, both vendored vectors, 250
//! AUs each): the H.264 slice-control buffer's `DataSize` is 20 on a stream with two
//! slices per picture, and the HEVC one's is 10 on a stream with one slice segment
//! per picture. Two codecs, two slice counts, one answer — 10.
//!
//! What the mistake costs, so it is never re-introduced: record 0's fields land at
//! 0/4/8 either way, so a SINGLE-slice stream decodes correctly and the two extra
//! bytes are trailing slop nobody reads. From the second record on, every field is
//! displaced by two bytes per preceding record, so the driver reads a slice offset
//! built from half of one field and half of the next. punktfunk hosts do emit
//! multi-slice streams.
//!
//! Hence: **the packed structs carry `#[repr(C, packed)]`** and the proofs below
//! pin `align_of` as well as `size_of`, plus — for every struct — that its size is
//! exactly its last member's offset plus that member's size, which is the assertion
//! that would have caught this one. Interior padding was already impossible (the
//! per-field offset asserts see it); it was TAIL padding that got in.
//!
//! Sources: the DXVA specifications "DirectX Video Acceleration Specification for
//! H.264/AVC Decoding" (§4.2 `DXVA_PicParams_H264`, §4.4 `DXVA_Qmatrix_H264`, §4.6
//! `DXVA_Slice_H264_Short`) and "DirectX Video Acceleration Specification for
//! HEVC/H.265 Decoding" (§4.1 `DXVA_PicParams_HEVC`, §4.2 `DXVA_Qmatrix_HEVC`,
//! §4.3 `DXVA_Slice_HEVC_Short`), cross-read against mingw-w64's `dxva.h`.

// The field names are the DXVA specs' names, character for character. Renaming
// them to snake_case would make every line of the conversion modules — and every
// review of it against the spec text or against libavcodec's `dxva2_*.c` — a
// translation exercise, which is exactly the kind of friction that lets a
// mis-assigned field survive a reading. `windows-rs` makes the same choice for
// every generated Win32 struct.
#![allow(non_snake_case)]

use std::mem::align_of;
use std::mem::offset_of;
use std::mem::size_of;

/// The `bPicEntry` sentinel for an unused `RefFrameList`/`RefPicList` slot, and
/// for an unused `RefPicSet*` index-array entry: `Index7Bits = 0x7F`,
/// `AssociatedFlag = 1`. Both DXVA specs name `0xFF` explicitly, and it is what
/// every shipping decoder pads with.
pub const UNUSED_ENTRY: u8 = 0xFF;

/// The bitstream buffer's size granule. The DXVA specs require the submitted
/// bitstream data size to be a multiple of 128 bytes, zero-padded at the end,
/// with the padding charged to the last slice's `SliceBytesInBuffer`.
pub const BITSTREAM_ALIGN: usize = 128;

/// `DXVA_PicEntry_H264` / `DXVA_PicEntry_HEVC` — one byte, identical in both specs:
///
/// ```c
/// typedef struct _DXVA_PicEntry_H264 {
///     union {
///         struct {
///             UCHAR Index7Bits     : 7;
///             UCHAR AssociatedFlag : 1;
///         };
///         UCHAR bPicEntry;
///     };
/// } DXVA_PicEntry_H264;   /* 1 byte */
/// ```
///
/// `Index7Bits` is the **uncompressed surface index** — for D3D11VA, the
/// `ArraySlice` of the decode texture array the picture lives in, which is
/// exactly the DPB slot index this backend's [`crate::SlotMap`] hands out.
/// `AssociatedFlag` means "bottom field" on `CurrPic` and "long-term reference"
/// on a `RefFrameList`/`RefPicList` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PicEntry(pub u8);

impl PicEntry {
    /// An unused entry (`0xFF`).
    pub const UNUSED: PicEntry = PicEntry(UNUSED_ENTRY);

    /// A surface index plus the entry's associated flag.
    ///
    /// `index` is masked to seven bits rather than checked: the callers pass DPB
    /// slot indices bounded by an envelope-gated 17-slot map, so the mask is
    /// unreachable — and a `debug_assert` says so without giving a corrupt
    /// stream a way to panic a release client.
    pub const fn new(index: u8, associated: bool) -> PicEntry {
        debug_assert!(index < 0x80, "a surface index must fit seven bits");
        PicEntry((index & 0x7F) | ((associated as u8) << 7))
    }

    /// The surface index (`Index7Bits`).
    pub const fn index(self) -> u8 {
        self.0 & 0x7F
    }

    /// The associated flag (bottom-field / long-term, per field).
    pub const fn associated(self) -> bool {
        self.0 & 0x80 != 0
    }
}

// ---------------------------------------------------------------------------
// H.264
// ---------------------------------------------------------------------------

/// `DXVA_PicParams_H264` — the H.264 picture-parameters buffer
/// (`D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS`).
///
/// ```c
/// typedef struct _DXVA_PicParams_H264 {
///     USHORT wFrameWidthInMbsMinus1;              /*   0 */
///     USHORT wFrameHeightInMbsMinus1;             /*   2 */
///     DXVA_PicEntry_H264 CurrPic;                 /*   4 */
///     UCHAR  num_ref_frames;                      /*   5 */
///     union { struct { ...15 bitfields... }; USHORT wBitFields; };  /*   6 */
///     UCHAR  bit_depth_luma_minus8;               /*   8 */
///     UCHAR  bit_depth_chroma_minus8;             /*   9 */
///     USHORT Reserved16Bits;                      /*  10 */
///     UINT   StatusReportFeedbackNumber;          /*  12 */
///     DXVA_PicEntry_H264 RefFrameList[16];        /*  16 */
///     INT    CurrFieldOrderCnt[2];                /*  32 */
///     INT    FieldOrderCntList[16][2];            /*  40 */
///     CHAR   pic_init_qs_minus26;                 /* 168 */
///     CHAR   chroma_qp_index_offset;              /* 169 */
///     CHAR   second_chroma_qp_index_offset;       /* 170 */
///     UCHAR  ContinuationFlag;                    /* 171 */
///     CHAR   pic_init_qp_minus26;                 /* 172 */
///     UCHAR  num_ref_idx_l0_active_minus1;        /* 173 */
///     UCHAR  num_ref_idx_l1_active_minus1;        /* 174 */
///     UCHAR  Reserved8BitsA;                      /* 175 */
///     USHORT FrameNumList[16];                    /* 176 */
///     UINT   UsedForReferenceFlags;               /* 208 */
///     USHORT NonExistingFrameFlags;               /* 212 */
///     USHORT frame_num;                           /* 214 */
///     UCHAR  log2_max_frame_num_minus4;           /* 216 */
///     UCHAR  pic_order_cnt_type;                  /* 217 */
///     UCHAR  log2_max_pic_order_cnt_lsb_minus4;   /* 218 */
///     UCHAR  delta_pic_order_always_zero_flag;    /* 219 */
///     UCHAR  direct_8x8_inference_flag;           /* 220 */
///     UCHAR  entropy_coding_mode_flag;            /* 221 */
///     UCHAR  pic_order_present_flag;              /* 222 */
///     UCHAR  num_slice_groups_minus1;             /* 223 */
///     UCHAR  slice_group_map_type;                /* 224 */
///     UCHAR  deblocking_filter_control_present_flag; /* 225 */
///     UCHAR  redundant_pic_cnt_present_flag;      /* 226 */
///     UCHAR  Reserved8BitsB;                      /* 227 */
///     USHORT slice_group_change_rate_minus1;      /* 228 */
///     UCHAR  SliceGroupMap[810];                  /* 230 */
/// } DXVA_PicParams_H264;                          /* 1040 bytes */
/// ```
///
/// `CHAR` is signed on MSVC for these members (the spec calls them signed
/// quantities: QP deltas and chroma offsets are negative in real streams), hence
/// `i8` where the C says `CHAR` and `u8` where it says `UCHAR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PicParamsH264 {
    pub wFrameWidthInMbsMinus1: u16,
    pub wFrameHeightInMbsMinus1: u16,
    pub CurrPic: PicEntry,
    pub num_ref_frames: u8,
    /// The packed bitfield word — build it with [`H264BitFields`].
    pub wBitFields: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub Reserved16Bits: u16,
    pub StatusReportFeedbackNumber: u32,
    pub RefFrameList: [PicEntry; 16],
    pub CurrFieldOrderCnt: [i32; 2],
    pub FieldOrderCntList: [[i32; 2]; 16],
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub ContinuationFlag: u8,
    pub pic_init_qp_minus26: i8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub Reserved8BitsA: u8,
    pub FrameNumList: [u16; 16],
    pub UsedForReferenceFlags: u32,
    pub NonExistingFrameFlags: u16,
    pub frame_num: u16,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: u8,
    pub direct_8x8_inference_flag: u8,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
    pub Reserved8BitsB: u8,
    pub slice_group_change_rate_minus1: u16,
    /// Flexible-macroblock-ordering map. Always all-zero here: this backend
    /// refuses a stream with slice groups outright (see
    /// [`crate::pic::PlanToDxvaError::SliceGroups`]) rather than submit a map it
    /// did not derive.
    pub SliceGroupMap: [u8; 810],
}

impl PicParamsH264 {
    /// An all-zero picture-parameters buffer. Written out as real zeros rather
    /// than `mem::zeroed` so the whole crate stays free of unsafe construction —
    /// and so a field added to the struct without a value here is a compile
    /// error, not a silently-zero field.
    pub const fn zeroed() -> PicParamsH264 {
        PicParamsH264 {
            wFrameWidthInMbsMinus1: 0,
            wFrameHeightInMbsMinus1: 0,
            CurrPic: PicEntry(0),
            num_ref_frames: 0,
            wBitFields: 0,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            Reserved16Bits: 0,
            StatusReportFeedbackNumber: 0,
            RefFrameList: [PicEntry(0); 16],
            CurrFieldOrderCnt: [0; 2],
            FieldOrderCntList: [[0; 2]; 16],
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            ContinuationFlag: 0,
            pic_init_qp_minus26: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            Reserved8BitsA: 0,
            FrameNumList: [0; 16],
            UsedForReferenceFlags: 0,
            NonExistingFrameFlags: 0,
            frame_num: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: 0,
            direct_8x8_inference_flag: 0,
            entropy_coding_mode_flag: 0,
            pic_order_present_flag: 0,
            num_slice_groups_minus1: 0,
            slice_group_map_type: 0,
            deblocking_filter_control_present_flag: 0,
            redundant_pic_cnt_present_flag: 0,
            Reserved8BitsB: 0,
            slice_group_change_rate_minus1: 0,
            SliceGroupMap: [0; 810],
        }
    }
}

/// The fifteen bitfields packed into [`PicParamsH264::wBitFields`], as a builder.
///
/// ```c
/// USHORT field_pic_flag                 : 1;   /* bit  0 */
/// USHORT MbaffFrameFlag                 : 1;   /* bit  1 */
/// USHORT residual_colour_transform_flag : 1;   /* bit  2 */
/// USHORT sp_for_switch_flag             : 1;   /* bit  3 */
/// USHORT chroma_format_idc              : 2;   /* bits 4-5 */
/// USHORT RefPicFlag                     : 1;   /* bit  6 */
/// USHORT constrained_intra_pred_flag    : 1;   /* bit  7 */
/// USHORT weighted_pred_flag             : 1;   /* bit  8 */
/// USHORT weighted_bipred_idc            : 2;   /* bits 9-10 */
/// USHORT MbsConsecutiveFlag             : 1;   /* bit 11 */
/// USHORT frame_mbs_only_flag            : 1;   /* bit 12 */
/// USHORT transform_8x8_mode_flag        : 1;   /* bit 13 */
/// USHORT MinLumaBipredSize8x8Flag       : 1;   /* bit 14 */
/// USHORT IntraPicFlag                   : 1;   /* bit 15 */
/// ```
///
/// `field_pic_flag`, `MbaffFrameFlag` and `residual_colour_transform_flag` are
/// always zero for this backend: pf-bitstream's envelope gate rejects interlaced
/// and separate-colour-plane streams before a plan exists, so writing them from a
/// parsed value would only be a way to smuggle a stream past that gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct H264BitFields {
    pub chroma_format_idc: u8,
    pub ref_pic_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub frame_mbs_only_flag: bool,
    pub transform_8x8_mode_flag: bool,
    /// `MinLumaBipredSize8x8Flag` — level 3.1 and above (the DXVA spec defines it
    /// as `level_idc >= 31`, and libavcodec's DXVA path writes exactly that).
    pub min_luma_bipred_size_8x8: bool,
    /// Every slice of the AU is I or SI.
    pub intra_pic_flag: bool,
}

impl H264BitFields {
    /// Pack to the `wBitFields` word. Two-bit members are masked, not checked:
    /// `chroma_format_idc` and `weighted_bipred_idc` are both spec-bounded to
    /// 0..=3 and the planner's envelope gate has already refused anything else,
    /// so a mask can only ever be a no-op — but it is a no-op that cannot panic
    /// on a hostile stream.
    pub const fn pack(self) -> u16 {
        // `MbsConsecutiveFlag` (bit 11) is hard-1: it means "macroblocks are in
        // raster order within a slice", which is true for everything that is not
        // flexible macroblock ordering — and FMO is refused before this runs.
        ((self.chroma_format_idc as u16 & 0x3) << 4)
            | ((self.ref_pic_flag as u16) << 6)
            | ((self.constrained_intra_pred_flag as u16) << 7)
            | ((self.weighted_pred_flag as u16) << 8)
            | ((self.weighted_bipred_idc as u16 & 0x3) << 9)
            | (1 << 11)
            | ((self.frame_mbs_only_flag as u16) << 12)
            | ((self.transform_8x8_mode_flag as u16) << 13)
            | ((self.min_luma_bipred_size_8x8 as u16) << 14)
            | ((self.intra_pic_flag as u16) << 15)
    }
}

/// `DXVA_Qmatrix_H264` — the H.264 inverse-quantization matrix buffer
/// (`D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX`).
///
/// ```c
/// typedef struct _DXVA_Qmatrix_H264 {
///     UCHAR bScalingLists4x4[6][16];   /*   0 */
///     UCHAR bScalingLists8x8[2][64];   /*  96 */
/// } DXVA_Qmatrix_H264;                 /* 224 bytes */
/// ```
///
/// Entry `[i][j]` is `ScalingList4x4[i][j]` / `ScalingList8x8[i][j]` **in the
/// order the bitstream codes them** (7.3.2.1.1.1), which is the zig-zag order —
/// not the raster order the inverse-scan produces. The vendored parser stores
/// them in exactly that coded order (`parse_scaling_list` writes `scaling_list[j]`
/// for the j-th coded coefficient), so the conversion is a straight copy.
///
/// Only the first two 8x8 lists travel: DXVA carries `Intra Y` and `Inter Y`
/// (H.264 list indices 0 and 3), because 8x8 chroma lists exist only in 4:4:4
/// profiles, which this backend does not decode.
///
/// ⚠ Old ATI/AMD UVD parts wanted these lists in RASTER order instead
/// (libavcodec's `FF_DXVA2_WORKAROUND_SCALING_LIST_ZIGZAG`). No such workaround is
/// implemented here and none is expected to be needed: punktfunk hosts encode with
/// flat scaling lists, so the buffer is all-defaults on every stream this client
/// will ever see. If a field report ever shows blockiness that tracks a non-flat
/// SPS/PPS matrix on old AMD, this comment is the place to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct QmatrixH264 {
    pub bScalingLists4x4: [[u8; 16]; 6],
    pub bScalingLists8x8: [[u8; 64]; 2],
}

impl QmatrixH264 {
    /// An all-zero quantization-matrix buffer.
    pub const fn zeroed() -> QmatrixH264 {
        QmatrixH264 {
            bScalingLists4x4: [[0; 16]; 6],
            bScalingLists8x8: [[0; 64]; 2],
        }
    }
}

/// `DXVA_Slice_H264_Short` — one entry of the H.264 slice-control buffer
/// (`D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL`), short format.
///
/// ```c
/// typedef struct _DXVA_Slice_H264_Short {
///     UINT   BSNALunitDataLocation;   /* 0 */
///     UINT   SliceBytesInBuffer;      /* 4 */
///     USHORT wBadSliceChopping;       /* 8 */
/// } DXVA_Slice_H264_Short;            /* 10 bytes — PACKED, no tail padding */
/// ```
///
/// **Ten bytes, not twelve** — `#[repr(C, packed)]`, and the single most important
/// number in this file after the picture-parameters offsets. See the module docs'
/// alignment section for the hardware measurement it comes from and for what
/// getting it wrong does to every record after the first.
///
/// Short format only. The long format (`DXVA_Slice_H264_Long`) additionally
/// carries the derived reference lists and the prediction weight tables, which
/// this backend does not build — a device offering only long-format configs is
/// refused at decoder creation and the ladder answers with the FFmpeg rung, which
/// does implement both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, packed)]
pub struct SliceH264Short {
    /// Byte offset of the slice's **start code** within the bitstream buffer.
    pub BSNALunitDataLocation: u32,
    /// Start code + NALU bytes; the last slice of an AU also carries the
    /// buffer's trailing 128-byte alignment padding.
    pub SliceBytesInBuffer: u32,
    /// 0 — a whole slice, in one buffer. The nonzero values describe a slice
    /// split across bitstream buffers, which this backend never does.
    pub wBadSliceChopping: u16,
}

// ---------------------------------------------------------------------------
// HEVC
// ---------------------------------------------------------------------------

/// `DXVA_PicParams_HEVC` — the HEVC picture-parameters buffer.
///
/// ```c
/// typedef struct _DXVA_PicParams_HEVC {
///     USHORT PicWidthInMinCbsY;                      /*   0 */
///     USHORT PicHeightInMinCbsY;                     /*   2 */
///     union { ...8 bitfields...; USHORT wFormatAndSequenceInfoFlags; }; /* 4 */
///     DXVA_PicEntry_HEVC CurrPic;                    /*   6 */
///     UCHAR  sps_max_dec_pic_buffering_minus1;       /*   7 */
///     UCHAR  log2_min_luma_coding_block_size_minus3; /*   8 */
///     UCHAR  log2_diff_max_min_luma_coding_block_size; /* 9 */
///     UCHAR  log2_min_transform_block_size_minus2;   /*  10 */
///     UCHAR  log2_diff_max_min_transform_block_size; /*  11 */
///     UCHAR  max_transform_hierarchy_depth_inter;    /*  12 */
///     UCHAR  max_transform_hierarchy_depth_intra;    /*  13 */
///     UCHAR  num_short_term_ref_pic_sets;            /*  14 */
///     UCHAR  num_long_term_ref_pics_sps;             /*  15 */
///     UCHAR  num_ref_idx_l0_default_active_minus1;   /*  16 */
///     UCHAR  num_ref_idx_l1_default_active_minus1;   /*  17 */
///     CHAR   init_qp_minus26;                        /*  18 */
///     UCHAR  ucNumDeltaPocsOfRefRpsIdx;              /*  19 */
///     USHORT wNumBitsForShortTermRPSInSlice;         /*  20 */
///     USHORT ReservedBits2;                          /*  22 */
///     union { ...; UINT32 dwCodingParamToolFlags; }; /*  24 */
///     union { ...; UINT32 dwCodingSettingPicturePropertyFlags; }; /* 28 */
///     CHAR   pps_cb_qp_offset;                       /*  32 */
///     CHAR   pps_cr_qp_offset;                       /*  33 */
///     UCHAR  num_tile_columns_minus1;                /*  34 */
///     UCHAR  num_tile_rows_minus1;                   /*  35 */
///     USHORT column_width_minus1[19];                /*  36 */
///     USHORT row_height_minus1[21];                  /*  74 */
///     UCHAR  diff_cu_qp_delta_depth;                 /* 116 */
///     CHAR   pps_beta_offset_div2;                   /* 117 */
///     CHAR   pps_tc_offset_div2;                     /* 118 */
///     UCHAR  log2_parallel_merge_level_minus2;       /* 119 */
///     INT    CurrPicOrderCntVal;                     /* 120 */
///     DXVA_PicEntry_HEVC RefPicList[15];             /* 124 */
///     UCHAR  ReservedBits5;                          /* 139 */
///     INT    PicOrderCntValList[15];                 /* 140 */
///     UCHAR  RefPicSetStCurrBefore[8];               /* 200 */
///     UCHAR  RefPicSetStCurrAfter[8];                /* 208 */
///     UCHAR  RefPicSetLtCurr[8];                     /* 216 */
///     USHORT ReservedBits6;                          /* 224 */
///     USHORT ReservedBits7;                          /* 226 */
///     UINT   StatusReportFeedbackNumber;             /* 228 */
/// } DXVA_PicParams_HEVC;                             /* 232 bytes */
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PicParamsHevc {
    pub PicWidthInMinCbsY: u16,
    pub PicHeightInMinCbsY: u16,
    /// Packed word — build it with [`HevcFormatFlags`].
    pub wFormatAndSequenceInfoFlags: u16,
    pub CurrPic: PicEntry,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub ucNumDeltaPocsOfRefRpsIdx: u8,
    pub wNumBitsForShortTermRPSInSlice: u16,
    pub ReservedBits2: u16,
    /// Packed word — build it with [`HevcToolFlags`].
    pub dwCodingParamToolFlags: u32,
    /// Packed word — build it with [`HevcPictureFlags`].
    pub dwCodingSettingPicturePropertyFlags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub CurrPicOrderCntVal: i32,
    pub RefPicList: [PicEntry; 15],
    pub ReservedBits5: u8,
    pub PicOrderCntValList: [i32; 15],
    pub RefPicSetStCurrBefore: [u8; 8],
    pub RefPicSetStCurrAfter: [u8; 8],
    pub RefPicSetLtCurr: [u8; 8],
    pub ReservedBits6: u16,
    pub ReservedBits7: u16,
    pub StatusReportFeedbackNumber: u32,
}

impl PicParamsHevc {
    /// An all-zero picture-parameters buffer.
    pub const fn zeroed() -> PicParamsHevc {
        PicParamsHevc {
            PicWidthInMinCbsY: 0,
            PicHeightInMinCbsY: 0,
            wFormatAndSequenceInfoFlags: 0,
            CurrPic: PicEntry(0),
            sps_max_dec_pic_buffering_minus1: 0,
            log2_min_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_luma_coding_block_size: 0,
            log2_min_transform_block_size_minus2: 0,
            log2_diff_max_min_transform_block_size: 0,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            num_short_term_ref_pic_sets: 0,
            num_long_term_ref_pics_sps: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            init_qp_minus26: 0,
            ucNumDeltaPocsOfRefRpsIdx: 0,
            wNumBitsForShortTermRPSInSlice: 0,
            ReservedBits2: 0,
            dwCodingParamToolFlags: 0,
            dwCodingSettingPicturePropertyFlags: 0,
            pps_cb_qp_offset: 0,
            pps_cr_qp_offset: 0,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            column_width_minus1: [0; 19],
            row_height_minus1: [0; 21],
            diff_cu_qp_delta_depth: 0,
            pps_beta_offset_div2: 0,
            pps_tc_offset_div2: 0,
            log2_parallel_merge_level_minus2: 0,
            CurrPicOrderCntVal: 0,
            RefPicList: [PicEntry(0); 15],
            ReservedBits5: 0,
            PicOrderCntValList: [0; 15],
            RefPicSetStCurrBefore: [0; 8],
            RefPicSetStCurrAfter: [0; 8],
            RefPicSetLtCurr: [0; 8],
            ReservedBits6: 0,
            ReservedBits7: 0,
            StatusReportFeedbackNumber: 0,
        }
    }
}

/// [`PicParamsHevc::wFormatAndSequenceInfoFlags`], as a builder.
///
/// ```c
/// USHORT chroma_format_idc                 : 2;  /* bits 0-1  */
/// USHORT separate_colour_plane_flag        : 1;  /* bit  2    */
/// USHORT bit_depth_luma_minus8             : 3;  /* bits 3-5  */
/// USHORT bit_depth_chroma_minus8           : 3;  /* bits 6-8  */
/// USHORT log2_max_pic_order_cnt_lsb_minus4 : 4;  /* bits 9-12 */
/// USHORT NoPicReorderingFlag               : 1;  /* bit 13    */
/// USHORT NoBiPredFlag                      : 1;  /* bit 14    */
/// USHORT ReservedBits1                     : 1;  /* bit 15    */
/// ```
///
/// `NoPicReorderingFlag`/`NoBiPredFlag` are hardware hints, not stream facts, and
/// stay 0 — the same choice libavcodec's DXVA HEVC path makes. Claiming them would
/// let a driver take a shortcut this decoder cannot guarantee is safe for a stream
/// whose SPS it re-reads per AU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcFormatFlags {
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
}

impl HevcFormatFlags {
    /// Pack to the `wFormatAndSequenceInfoFlags` word. Widths are masked for the
    /// same reason as [`H264BitFields::pack`]: spec-bounded inputs, no panic path.
    pub const fn pack(self) -> u16 {
        (self.chroma_format_idc as u16 & 0x3)
            | ((self.separate_colour_plane_flag as u16) << 2)
            | ((self.bit_depth_luma_minus8 as u16 & 0x7) << 3)
            | ((self.bit_depth_chroma_minus8 as u16 & 0x7) << 6)
            | ((self.log2_max_pic_order_cnt_lsb_minus4 as u16 & 0xF) << 9)
    }
}

/// [`PicParamsHevc::dwCodingParamToolFlags`], as a builder.
///
/// ```c
/// UINT32 scaling_list_enabled_flag                    : 1;  /* bit  0     */
/// UINT32 amp_enabled_flag                             : 1;  /* bit  1     */
/// UINT32 sample_adaptive_offset_enabled_flag          : 1;  /* bit  2     */
/// UINT32 pcm_enabled_flag                             : 1;  /* bit  3     */
/// UINT32 pcm_sample_bit_depth_luma_minus1             : 4;  /* bits 4-7   */
/// UINT32 pcm_sample_bit_depth_chroma_minus1           : 4;  /* bits 8-11  */
/// UINT32 log2_min_pcm_luma_coding_block_size_minus3   : 2;  /* bits 12-13 */
/// UINT32 log2_diff_max_min_pcm_luma_coding_block_size : 2;  /* bits 14-15 */
/// UINT32 pcm_loop_filter_disabled_flag                : 1;  /* bit 16     */
/// UINT32 long_term_ref_pics_present_flag              : 1;  /* bit 17     */
/// UINT32 sps_temporal_mvp_enabled_flag                : 1;  /* bit 18     */
/// UINT32 strong_intra_smoothing_enabled_flag          : 1;  /* bit 19     */
/// UINT32 dependent_slice_segments_enabled_flag        : 1;  /* bit 20     */
/// UINT32 output_flag_present_flag                     : 1;  /* bit 21     */
/// UINT32 num_extra_slice_header_bits                  : 3;  /* bits 22-24 */
/// UINT32 sign_data_hiding_enabled_flag                : 1;  /* bit 25     */
/// UINT32 cabac_init_present_flag                      : 1;  /* bit 26     */
/// UINT32 ReservedBits3                                : 5;  /* bits 27-31 */
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcToolFlags {
    pub scaling_list_enabled_flag: bool,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm_enabled_flag: bool,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub pcm_loop_filter_disabled_flag: bool,
    pub long_term_ref_pics_present_flag: bool,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
}

impl HevcToolFlags {
    /// Pack to the `dwCodingParamToolFlags` word.
    pub const fn pack(self) -> u32 {
        (self.scaling_list_enabled_flag as u32)
            | ((self.amp_enabled_flag as u32) << 1)
            | ((self.sample_adaptive_offset_enabled_flag as u32) << 2)
            | ((self.pcm_enabled_flag as u32) << 3)
            | ((self.pcm_sample_bit_depth_luma_minus1 as u32 & 0xF) << 4)
            | ((self.pcm_sample_bit_depth_chroma_minus1 as u32 & 0xF) << 8)
            | ((self.log2_min_pcm_luma_coding_block_size_minus3 as u32 & 0x3) << 12)
            | ((self.log2_diff_max_min_pcm_luma_coding_block_size as u32 & 0x3) << 14)
            | ((self.pcm_loop_filter_disabled_flag as u32) << 16)
            | ((self.long_term_ref_pics_present_flag as u32) << 17)
            | ((self.sps_temporal_mvp_enabled_flag as u32) << 18)
            | ((self.strong_intra_smoothing_enabled_flag as u32) << 19)
            | ((self.dependent_slice_segments_enabled_flag as u32) << 20)
            | ((self.output_flag_present_flag as u32) << 21)
            | ((self.num_extra_slice_header_bits as u32 & 0x7) << 22)
            | ((self.sign_data_hiding_enabled_flag as u32) << 25)
            | ((self.cabac_init_present_flag as u32) << 26)
    }
}

/// [`PicParamsHevc::dwCodingSettingPicturePropertyFlags`], as a builder.
///
/// ```c
/// UINT32 constrained_intra_pred_flag                 : 1;  /* bit  0     */
/// UINT32 transform_skip_enabled_flag                 : 1;  /* bit  1     */
/// UINT32 cu_qp_delta_enabled_flag                    : 1;  /* bit  2     */
/// UINT32 pps_slice_chroma_qp_offsets_present_flag    : 1;  /* bit  3     */
/// UINT32 weighted_pred_flag                          : 1;  /* bit  4     */
/// UINT32 weighted_bipred_flag                        : 1;  /* bit  5     */
/// UINT32 transquant_bypass_enabled_flag              : 1;  /* bit  6     */
/// UINT32 tiles_enabled_flag                          : 1;  /* bit  7     */
/// UINT32 entropy_coding_sync_enabled_flag            : 1;  /* bit  8     */
/// UINT32 uniform_spacing_flag                        : 1;  /* bit  9     */
/// UINT32 loop_filter_across_tiles_enabled_flag       : 1;  /* bit 10     */
/// UINT32 pps_loop_filter_across_slices_enabled_flag  : 1;  /* bit 11     */
/// UINT32 deblocking_filter_override_enabled_flag     : 1;  /* bit 12     */
/// UINT32 pps_deblocking_filter_disabled_flag         : 1;  /* bit 13     */
/// UINT32 lists_modification_present_flag             : 1;  /* bit 14     */
/// UINT32 slice_segment_header_extension_present_flag : 1;  /* bit 15     */
/// UINT32 IrapPicFlag                                 : 1;  /* bit 16     */
/// UINT32 IdrPicFlag                                  : 1;  /* bit 17     */
/// UINT32 IntraPicFlag                                : 1;  /* bit 18     */
/// UINT32 ReservedBits4                               : 13; /* bits 19-31 */
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcPictureFlags {
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub uniform_spacing_flag: bool,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_deblocking_filter_disabled_flag: bool,
    pub lists_modification_present_flag: bool,
    pub slice_segment_header_extension_present_flag: bool,
    pub irap_pic_flag: bool,
    pub idr_pic_flag: bool,
    pub intra_pic_flag: bool,
}

impl HevcPictureFlags {
    /// Pack to the `dwCodingSettingPicturePropertyFlags` word.
    pub const fn pack(self) -> u32 {
        (self.constrained_intra_pred_flag as u32)
            | ((self.transform_skip_enabled_flag as u32) << 1)
            | ((self.cu_qp_delta_enabled_flag as u32) << 2)
            | ((self.pps_slice_chroma_qp_offsets_present_flag as u32) << 3)
            | ((self.weighted_pred_flag as u32) << 4)
            | ((self.weighted_bipred_flag as u32) << 5)
            | ((self.transquant_bypass_enabled_flag as u32) << 6)
            | ((self.tiles_enabled_flag as u32) << 7)
            | ((self.entropy_coding_sync_enabled_flag as u32) << 8)
            | ((self.uniform_spacing_flag as u32) << 9)
            | ((self.loop_filter_across_tiles_enabled_flag as u32) << 10)
            | ((self.pps_loop_filter_across_slices_enabled_flag as u32) << 11)
            | ((self.deblocking_filter_override_enabled_flag as u32) << 12)
            | ((self.pps_deblocking_filter_disabled_flag as u32) << 13)
            | ((self.lists_modification_present_flag as u32) << 14)
            | ((self.slice_segment_header_extension_present_flag as u32) << 15)
            | ((self.irap_pic_flag as u32) << 16)
            | ((self.idr_pic_flag as u32) << 17)
            | ((self.intra_pic_flag as u32) << 18)
    }
}

/// `DXVA_Qmatrix_HEVC` — the HEVC inverse-quantization matrix buffer.
///
/// ```c
/// typedef struct _DXVA_Qmatrix_HEVC {
///     UCHAR ucScalingLists0[6][16];          /*   0 */
///     UCHAR ucScalingLists1[6][64];          /*  96 */
///     UCHAR ucScalingLists2[6][64];          /* 480 */
///     UCHAR ucScalingLists3[2][64];          /* 864 */
///     UCHAR ucScalingListDCCoefSizeID2[6];   /* 992 */
///     UCHAR ucScalingListDCCoefSizeID3[2];   /* 998 */
/// } DXVA_Qmatrix_HEVC;                       /* 1000 bytes */
/// ```
///
/// `ucScalingLists{0,1,2,3}` are sizeIds 0..3 (4x4, 8x8, 16x16, 32x32), each
/// `[matrixId][coefficient]` in coded (diagonal-scan) order. sizeId 3 has only two
/// matrices — HEVC codes matrixId 0 and 3 for 32x32 — so `ucScalingLists3[k]` is
/// the parser's `scaling_list_32x32[k * 3]`, and likewise for its DC coefficients.
/// The DC entries are `scaling_list_dc_coef_minus8 + 8`, i.e. the ScalingFactor DC
/// value itself, not the coded delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct QmatrixHevc {
    pub ucScalingLists0: [[u8; 16]; 6],
    pub ucScalingLists1: [[u8; 64]; 6],
    pub ucScalingLists2: [[u8; 64]; 6],
    pub ucScalingLists3: [[u8; 64]; 2],
    pub ucScalingListDCCoefSizeID2: [u8; 6],
    pub ucScalingListDCCoefSizeID3: [u8; 2],
}

impl QmatrixHevc {
    /// An all-zero quantization-matrix buffer.
    pub const fn zeroed() -> QmatrixHevc {
        QmatrixHevc {
            ucScalingLists0: [[0; 16]; 6],
            ucScalingLists1: [[0; 64]; 6],
            ucScalingLists2: [[0; 64]; 6],
            ucScalingLists3: [[0; 64]; 2],
            ucScalingListDCCoefSizeID2: [0; 6],
            ucScalingListDCCoefSizeID3: [0; 2],
        }
    }
}

/// `DXVA_Slice_HEVC_Short` — one entry of the HEVC slice-control buffer. Byte-for
/// byte the H.264 short record; declared separately because the two specs define
/// them separately and a future spec revision is free to diverge.
///
/// ```c
/// typedef struct _DXVA_Slice_HEVC_Short {
///     UINT   BSNALunitDataLocation;   /* 0 */
///     UINT   SliceBytesInBuffer;      /* 4 */
///     USHORT wBadSliceChopping;       /* 8 */
/// } DXVA_Slice_HEVC_Short;            /* 10 bytes — PACKED, no tail padding */
/// ```
///
/// Ten bytes for the same reason as [`SliceH264Short`], and measured independently:
/// the HEVC capture's slice-control `DataSize` is 10 on a vector with exactly one
/// slice segment per picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, packed)]
pub struct SliceHevcShort {
    pub BSNALunitDataLocation: u32,
    pub SliceBytesInBuffer: u32,
    pub wBadSliceChopping: u16,
}

// ---------------------------------------------------------------------------
// Layout proofs
// ---------------------------------------------------------------------------

// Sizes and offsets, asserted at COMPILE time against the C declarations
// reproduced above. This is the whole defence against a silently mis-declared
// buffer: nothing else in the pipeline can tell a wrong offset from a right one,
// because the driver accepts either and only the picture differs.
//
// Three kinds of assertion, and the third is new because the first two missed a
// real defect (the slice records' 12-vs-10; see the module docs):
//
// 1. `size_of` per struct, against the C total.
// 2. `offset_of` per FIELD, which is what makes interior padding or a reordering
//    impossible.
// 3. **size == last field's offset + last field's own size**, per struct — the
//    assertion that catches TAIL padding, which is exactly what (1) and (2) cannot
//    see: a struct whose declared total is itself wrong satisfies both. Every one of
//    these buffers is a wire format with no padding anywhere, and this is where that
//    is stated as a proof rather than a comment.
const _: () = {
    assert!(size_of::<PicEntry>() == 1);

    assert!(size_of::<PicParamsH264>() == 1040);
    assert!(offset_of!(PicParamsH264, wFrameWidthInMbsMinus1) == 0);
    assert!(offset_of!(PicParamsH264, wFrameHeightInMbsMinus1) == 2);
    assert!(offset_of!(PicParamsH264, CurrPic) == 4);
    assert!(offset_of!(PicParamsH264, num_ref_frames) == 5);
    assert!(offset_of!(PicParamsH264, wBitFields) == 6);
    assert!(offset_of!(PicParamsH264, bit_depth_luma_minus8) == 8);
    assert!(offset_of!(PicParamsH264, bit_depth_chroma_minus8) == 9);
    assert!(offset_of!(PicParamsH264, Reserved16Bits) == 10);
    assert!(offset_of!(PicParamsH264, StatusReportFeedbackNumber) == 12);
    assert!(offset_of!(PicParamsH264, RefFrameList) == 16);
    assert!(offset_of!(PicParamsH264, CurrFieldOrderCnt) == 32);
    assert!(offset_of!(PicParamsH264, FieldOrderCntList) == 40);
    assert!(offset_of!(PicParamsH264, pic_init_qs_minus26) == 168);
    assert!(offset_of!(PicParamsH264, chroma_qp_index_offset) == 169);
    assert!(offset_of!(PicParamsH264, second_chroma_qp_index_offset) == 170);
    assert!(offset_of!(PicParamsH264, ContinuationFlag) == 171);
    assert!(offset_of!(PicParamsH264, pic_init_qp_minus26) == 172);
    assert!(offset_of!(PicParamsH264, num_ref_idx_l0_active_minus1) == 173);
    assert!(offset_of!(PicParamsH264, num_ref_idx_l1_active_minus1) == 174);
    assert!(offset_of!(PicParamsH264, Reserved8BitsA) == 175);
    assert!(offset_of!(PicParamsH264, FrameNumList) == 176);
    assert!(offset_of!(PicParamsH264, UsedForReferenceFlags) == 208);
    assert!(offset_of!(PicParamsH264, NonExistingFrameFlags) == 212);
    assert!(offset_of!(PicParamsH264, frame_num) == 214);
    assert!(offset_of!(PicParamsH264, log2_max_frame_num_minus4) == 216);
    assert!(offset_of!(PicParamsH264, pic_order_cnt_type) == 217);
    assert!(offset_of!(PicParamsH264, log2_max_pic_order_cnt_lsb_minus4) == 218);
    assert!(offset_of!(PicParamsH264, delta_pic_order_always_zero_flag) == 219);
    assert!(offset_of!(PicParamsH264, direct_8x8_inference_flag) == 220);
    assert!(offset_of!(PicParamsH264, entropy_coding_mode_flag) == 221);
    assert!(offset_of!(PicParamsH264, pic_order_present_flag) == 222);
    assert!(offset_of!(PicParamsH264, num_slice_groups_minus1) == 223);
    assert!(offset_of!(PicParamsH264, slice_group_map_type) == 224);
    assert!(offset_of!(PicParamsH264, deblocking_filter_control_present_flag) == 225);
    assert!(offset_of!(PicParamsH264, redundant_pic_cnt_present_flag) == 226);
    assert!(offset_of!(PicParamsH264, Reserved8BitsB) == 227);
    assert!(offset_of!(PicParamsH264, slice_group_change_rate_minus1) == 228);
    assert!(offset_of!(PicParamsH264, SliceGroupMap) == 230);

    assert!(size_of::<QmatrixH264>() == 224);
    assert!(offset_of!(QmatrixH264, bScalingLists4x4) == 0);
    assert!(offset_of!(QmatrixH264, bScalingLists8x8) == 96);

    assert!(size_of::<SliceH264Short>() == 10);
    assert!(align_of::<SliceH264Short>() == 1);
    assert!(offset_of!(SliceH264Short, BSNALunitDataLocation) == 0);
    assert!(offset_of!(SliceH264Short, SliceBytesInBuffer) == 4);
    assert!(offset_of!(SliceH264Short, wBadSliceChopping) == 8);

    assert!(size_of::<PicParamsHevc>() == 232);
    assert!(offset_of!(PicParamsHevc, PicWidthInMinCbsY) == 0);
    assert!(offset_of!(PicParamsHevc, PicHeightInMinCbsY) == 2);
    assert!(offset_of!(PicParamsHevc, wFormatAndSequenceInfoFlags) == 4);
    assert!(offset_of!(PicParamsHevc, CurrPic) == 6);
    assert!(offset_of!(PicParamsHevc, sps_max_dec_pic_buffering_minus1) == 7);
    assert!(offset_of!(PicParamsHevc, log2_min_luma_coding_block_size_minus3) == 8);
    assert!(offset_of!(PicParamsHevc, log2_diff_max_min_luma_coding_block_size) == 9);
    assert!(offset_of!(PicParamsHevc, log2_min_transform_block_size_minus2) == 10);
    assert!(offset_of!(PicParamsHevc, log2_diff_max_min_transform_block_size) == 11);
    assert!(offset_of!(PicParamsHevc, max_transform_hierarchy_depth_inter) == 12);
    assert!(offset_of!(PicParamsHevc, max_transform_hierarchy_depth_intra) == 13);
    assert!(offset_of!(PicParamsHevc, num_short_term_ref_pic_sets) == 14);
    assert!(offset_of!(PicParamsHevc, num_long_term_ref_pics_sps) == 15);
    assert!(offset_of!(PicParamsHevc, num_ref_idx_l0_default_active_minus1) == 16);
    assert!(offset_of!(PicParamsHevc, num_ref_idx_l1_default_active_minus1) == 17);
    assert!(offset_of!(PicParamsHevc, init_qp_minus26) == 18);
    assert!(offset_of!(PicParamsHevc, ucNumDeltaPocsOfRefRpsIdx) == 19);
    assert!(offset_of!(PicParamsHevc, wNumBitsForShortTermRPSInSlice) == 20);
    assert!(offset_of!(PicParamsHevc, ReservedBits2) == 22);
    assert!(offset_of!(PicParamsHevc, dwCodingParamToolFlags) == 24);
    assert!(offset_of!(PicParamsHevc, dwCodingSettingPicturePropertyFlags) == 28);
    assert!(offset_of!(PicParamsHevc, pps_cb_qp_offset) == 32);
    assert!(offset_of!(PicParamsHevc, pps_cr_qp_offset) == 33);
    assert!(offset_of!(PicParamsHevc, num_tile_columns_minus1) == 34);
    assert!(offset_of!(PicParamsHevc, num_tile_rows_minus1) == 35);
    assert!(offset_of!(PicParamsHevc, column_width_minus1) == 36);
    assert!(offset_of!(PicParamsHevc, row_height_minus1) == 74);
    assert!(offset_of!(PicParamsHevc, diff_cu_qp_delta_depth) == 116);
    assert!(offset_of!(PicParamsHevc, pps_beta_offset_div2) == 117);
    assert!(offset_of!(PicParamsHevc, pps_tc_offset_div2) == 118);
    assert!(offset_of!(PicParamsHevc, log2_parallel_merge_level_minus2) == 119);
    assert!(offset_of!(PicParamsHevc, CurrPicOrderCntVal) == 120);
    assert!(offset_of!(PicParamsHevc, RefPicList) == 124);
    assert!(offset_of!(PicParamsHevc, ReservedBits5) == 139);
    assert!(offset_of!(PicParamsHevc, PicOrderCntValList) == 140);
    assert!(offset_of!(PicParamsHevc, RefPicSetStCurrBefore) == 200);
    assert!(offset_of!(PicParamsHevc, RefPicSetStCurrAfter) == 208);
    assert!(offset_of!(PicParamsHevc, RefPicSetLtCurr) == 216);
    assert!(offset_of!(PicParamsHevc, ReservedBits6) == 224);
    assert!(offset_of!(PicParamsHevc, ReservedBits7) == 226);
    assert!(offset_of!(PicParamsHevc, StatusReportFeedbackNumber) == 228);

    assert!(size_of::<QmatrixHevc>() == 1000);
    assert!(offset_of!(QmatrixHevc, ucScalingLists0) == 0);
    assert!(offset_of!(QmatrixHevc, ucScalingLists1) == 96);
    assert!(offset_of!(QmatrixHevc, ucScalingLists2) == 480);
    assert!(offset_of!(QmatrixHevc, ucScalingLists3) == 864);
    assert!(offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2) == 992);
    assert!(offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3) == 998);

    assert!(size_of::<SliceHevcShort>() == 10);
    assert!(align_of::<SliceHevcShort>() == 1);
    assert!(offset_of!(SliceHevcShort, BSNALunitDataLocation) == 0);
    assert!(offset_of!(SliceHevcShort, SliceBytesInBuffer) == 4);
    assert!(offset_of!(SliceHevcShort, wBadSliceChopping) == 8);

    // NO TAIL PADDING, per struct: the size is the last member's offset plus the
    // last member's own size, nothing more. The right-hand sizes are the C
    // declarations' (`UCHAR SliceGroupMap[810]`, `UINT StatusReportFeedbackNumber`,
    // …), so each line is an independent statement of the total rather than a
    // restatement of `size_of`.
    assert!(size_of::<PicParamsH264>() == offset_of!(PicParamsH264, SliceGroupMap) + 810);
    assert!(size_of::<QmatrixH264>() == offset_of!(QmatrixH264, bScalingLists8x8) + 2 * 64);
    assert!(size_of::<SliceH264Short>() == offset_of!(SliceH264Short, wBadSliceChopping) + 2);
    assert!(
        size_of::<PicParamsHevc>() == offset_of!(PicParamsHevc, StatusReportFeedbackNumber) + 4
    );
    assert!(size_of::<QmatrixHevc>() == offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3) + 2);
    assert!(size_of::<SliceHevcShort>() == offset_of!(SliceHevcShort, wBadSliceChopping) + 2);
};

// ---------------------------------------------------------------------------
// Byte view
// ---------------------------------------------------------------------------

mod sealed {
    /// Implemented only by this module's `#[repr(C)]` plain-old-data buffers.
    /// Sealed so no downstream type can opt into [`super::as_bytes`]'s unsafe
    /// transmute-to-bytes on a type that owns a pointer or has padding it cares
    /// about.
    pub trait DxvaBuffer: Copy + 'static {}
}

pub use sealed::DxvaBuffer;

impl DxvaBuffer for PicParamsH264 {}
impl DxvaBuffer for QmatrixH264 {}
impl DxvaBuffer for SliceH264Short {}
impl DxvaBuffer for PicParamsHevc {}
impl DxvaBuffer for QmatrixHevc {}
impl DxvaBuffer for SliceHevcShort {}

/// The raw bytes of a DXVA buffer, ready for `memcpy` into the mapping
/// `GetDecoderBuffer` returns.
///
/// Reading a struct's padding bytes is what makes this unsafe in principle; here
/// it is sound and the values are meaningful, because every implementor is
/// `#[repr(C)]` POD built from a `zeroed()` base — so the tail padding a driver
/// reads is zero rather than uninitialized, which is exactly what the DXVA specs
/// call for in reserved bytes.
pub fn as_bytes<T: DxvaBuffer>(value: &T) -> &[u8] {
    // SAFETY: `T: DxvaBuffer` is a sealed trait implemented only for this
    // module's `#[repr(C)]` structs, none of which contains a pointer, a
    // reference, or any type with a niche or a `Drop`. Their entire
    // `size_of::<T>()` byte range — payload and padding alike — is therefore
    // initialized memory owned by `value`, and the returned slice borrows it for
    // exactly `value`'s lifetime, so nothing can mutate or free it while the
    // slice is alive. The alignment requirement is trivially met (the slice is
    // `u8`), and `size_of::<T>()` never exceeds `isize::MAX`.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

/// The raw bytes of a slice-control array, laid out exactly as the
/// `D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL` buffer wants them: `n` records
/// back to back.
pub fn slice_bytes<T: DxvaBuffer>(values: &[T]) -> &[u8] {
    // SAFETY: the same POD argument as `as_bytes`, extended over a slice: the
    // elements are contiguous with `size_of::<T>()` stride by the definition of
    // a Rust slice, every byte of every element is initialized (POD built from
    // `zeroed()`), and the borrow ties the byte view to `values`. The length
    // cannot overflow `isize::MAX`: it is the size of a live allocation.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One "set this member, expect this word" case of a packing table.
    type PackCase<T> = (fn(&mut T), u32);

    #[test]
    fn a_pic_entry_packs_the_index_into_seven_bits_and_the_flag_into_the_eighth() {
        assert_eq!(PicEntry::new(0, false).0, 0x00);
        assert_eq!(PicEntry::new(5, false).0, 0x05);
        assert_eq!(PicEntry::new(5, true).0, 0x85);
        assert_eq!(PicEntry::new(0x7F, true).0, 0xFF);
        // Round-trip: the accessors read back exactly what was packed.
        let entry = PicEntry::new(17, true);
        assert_eq!(entry.index(), 17);
        assert!(entry.associated());
        assert_eq!(PicEntry::UNUSED.0, UNUSED_ENTRY);
    }

    #[test]
    fn the_h264_bitfield_word_packs_each_member_at_its_declared_bit() {
        // One member at a time, checked against the bit position written in the
        // C declaration — an independent derivation from the packing expression.
        let base = H264BitFields::default();
        // MbsConsecutiveFlag is hard-1, so a default word is bit 11 alone.
        assert_eq!(base.pack(), 1 << 11);

        let mut f = base;
        f.chroma_format_idc = 1;
        assert_eq!(f.pack(), (1 << 11) | (1 << 4));
        f.chroma_format_idc = 3;
        assert_eq!(f.pack(), (1 << 11) | (3 << 4));

        let mut f = base;
        f.ref_pic_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 6));

        let mut f = base;
        f.constrained_intra_pred_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 7));

        let mut f = base;
        f.weighted_pred_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 8));

        let mut f = base;
        f.weighted_bipred_idc = 2;
        assert_eq!(f.pack(), (1 << 11) | (2 << 9));

        let mut f = base;
        f.frame_mbs_only_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 12));

        let mut f = base;
        f.transform_8x8_mode_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 13));

        let mut f = base;
        f.min_luma_bipred_size_8x8 = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 14));

        let mut f = base;
        f.intra_pic_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 15));
    }

    #[test]
    fn a_typical_h264_bitfield_word_matches_a_hand_computed_value() {
        // 4:2:0, reference picture, CABAC-irrelevant, weighted prediction off,
        // progressive, 8x8 transform on, level 4.0, an inter picture:
        // bits 4 (chroma=1), 6 (ref), 11 (consecutive), 12 (frame_mbs_only),
        // 13 (transform_8x8), 14 (level >= 3.1).
        let f = H264BitFields {
            chroma_format_idc: 1,
            ref_pic_flag: true,
            frame_mbs_only_flag: true,
            transform_8x8_mode_flag: true,
            min_luma_bipred_size_8x8: true,
            ..Default::default()
        };
        assert_eq!(f.pack(), 0b0111_1000_0101_0000);
    }

    #[test]
    fn the_hevc_format_word_packs_each_member_at_its_declared_bit() {
        let base = HevcFormatFlags::default();
        assert_eq!(base.pack(), 0);

        let mut f = base;
        f.chroma_format_idc = 1;
        assert_eq!(f.pack(), 1);

        let mut f = base;
        f.separate_colour_plane_flag = true;
        assert_eq!(f.pack(), 1 << 2);

        // Main 10: luma AND chroma at 2, at bits 3 and 6.
        let mut f = base;
        f.bit_depth_luma_minus8 = 2;
        f.bit_depth_chroma_minus8 = 2;
        assert_eq!(f.pack(), (2 << 3) | (2 << 6));

        let mut f = base;
        f.log2_max_pic_order_cnt_lsb_minus4 = 4;
        assert_eq!(f.pack(), 4 << 9);
    }

    #[test]
    fn the_hevc_tool_word_packs_each_member_at_its_declared_bit() {
        let base = HevcToolFlags::default();
        assert_eq!(base.pack(), 0);

        let checks: [PackCase<HevcToolFlags>; 13] = [
            (|f| f.scaling_list_enabled_flag = true, 1 << 0),
            (|f| f.amp_enabled_flag = true, 1 << 1),
            (|f| f.sample_adaptive_offset_enabled_flag = true, 1 << 2),
            (|f| f.pcm_enabled_flag = true, 1 << 3),
            (|f| f.pcm_loop_filter_disabled_flag = true, 1 << 16),
            (|f| f.long_term_ref_pics_present_flag = true, 1 << 17),
            (|f| f.sps_temporal_mvp_enabled_flag = true, 1 << 18),
            (|f| f.strong_intra_smoothing_enabled_flag = true, 1 << 19),
            (|f| f.dependent_slice_segments_enabled_flag = true, 1 << 20),
            (|f| f.output_flag_present_flag = true, 1 << 21),
            (|f| f.sign_data_hiding_enabled_flag = true, 1 << 25),
            (|f| f.cabac_init_present_flag = true, 1 << 26),
            (|f| f.num_extra_slice_header_bits = 5, 5 << 22),
        ];
        for (set, expected) in checks {
            let mut f = base;
            set(&mut f);
            assert_eq!(f.pack(), expected);
        }

        // The multi-bit PCM members, at their declared widths.
        let mut f = base;
        f.pcm_sample_bit_depth_luma_minus1 = 7;
        f.pcm_sample_bit_depth_chroma_minus1 = 7;
        f.log2_min_pcm_luma_coding_block_size_minus3 = 1;
        f.log2_diff_max_min_pcm_luma_coding_block_size = 2;
        assert_eq!(f.pack(), (7 << 4) | (7 << 8) | (1 << 12) | (2 << 14));
    }

    #[test]
    fn the_hevc_picture_word_packs_each_member_at_its_declared_bit() {
        let base = HevcPictureFlags::default();
        assert_eq!(base.pack(), 0);

        let checks: [PackCase<HevcPictureFlags>; 19] = [
            (|f| f.constrained_intra_pred_flag = true, 1 << 0),
            (|f| f.transform_skip_enabled_flag = true, 1 << 1),
            (|f| f.cu_qp_delta_enabled_flag = true, 1 << 2),
            (
                |f| f.pps_slice_chroma_qp_offsets_present_flag = true,
                1 << 3,
            ),
            (|f| f.weighted_pred_flag = true, 1 << 4),
            (|f| f.weighted_bipred_flag = true, 1 << 5),
            (|f| f.transquant_bypass_enabled_flag = true, 1 << 6),
            (|f| f.tiles_enabled_flag = true, 1 << 7),
            (|f| f.entropy_coding_sync_enabled_flag = true, 1 << 8),
            (|f| f.uniform_spacing_flag = true, 1 << 9),
            (|f| f.loop_filter_across_tiles_enabled_flag = true, 1 << 10),
            (
                |f| f.pps_loop_filter_across_slices_enabled_flag = true,
                1 << 11,
            ),
            (
                |f| f.deblocking_filter_override_enabled_flag = true,
                1 << 12,
            ),
            (|f| f.pps_deblocking_filter_disabled_flag = true, 1 << 13),
            (|f| f.lists_modification_present_flag = true, 1 << 14),
            (
                |f| f.slice_segment_header_extension_present_flag = true,
                1 << 15,
            ),
            (|f| f.irap_pic_flag = true, 1 << 16),
            (|f| f.idr_pic_flag = true, 1 << 17),
            (|f| f.intra_pic_flag = true, 1 << 18),
        ];
        for (set, expected) in checks {
            let mut f = base;
            set(&mut f);
            assert_eq!(f.pack(), expected);
        }
    }

    #[test]
    fn every_dxva_buffer_has_the_size_its_spec_declares() {
        // The const block above already fails the build on a mismatch; this test
        // is what makes the numbers show up in a test run — and it is where a
        // reader who distrusts a `const _` finds the same claim executable.
        assert_eq!(size_of::<PicParamsH264>(), 1040);
        assert_eq!(size_of::<QmatrixH264>(), 224);
        assert_eq!(size_of::<PicParamsHevc>(), 232);
        assert_eq!(size_of::<QmatrixHevc>(), 1000);
        assert_eq!(size_of::<PicEntry>(), 1);
        // TEN, not twelve. Measured against libavcodec on hardware: the H.264
        // slice-control buffer is 20 bytes for a two-slice picture and the HEVC one
        // 10 bytes for a one-slice-segment picture (module docs). A `#[repr(C)]`
        // `{u32, u32, u16}` is 12, and every record after the first would then be
        // displaced by two bytes per preceding record.
        assert_eq!(size_of::<SliceH264Short>(), 10);
        assert_eq!(size_of::<SliceHevcShort>(), 10);
        assert_eq!(align_of::<SliceH264Short>(), 1);
        assert_eq!(align_of::<SliceHevcShort>(), 1);
    }

    #[test]
    fn as_bytes_sees_the_struct_at_its_declared_offsets() {
        // A byte view is the actual submission format, so read a few fields back
        // out of it at their declared offsets — this catches an endianness or
        // packing surprise the size assertions alone would miss.
        let mut pp = PicParamsH264::zeroed();
        pp.wFrameWidthInMbsMinus1 = 0x0102;
        pp.CurrPic = PicEntry::new(3, false);
        pp.StatusReportFeedbackNumber = 0x0A0B_0C0D;
        pp.frame_num = 0x1234;
        let bytes = as_bytes(&pp);
        assert_eq!(bytes.len(), 1040);
        assert_eq!(&bytes[0..2], &0x0102u16.to_le_bytes());
        assert_eq!(bytes[4], 3);
        assert_eq!(&bytes[12..16], &0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(&bytes[214..216], &0x1234u16.to_le_bytes());
        // Everything the writer did not touch is a real zero, which is what the
        // reserved fields of both specs require.
        assert!(bytes[230..1040].iter().all(|&b| b == 0));
    }

    #[test]
    fn slice_bytes_lays_records_out_back_to_back_with_no_gap() {
        let records = [
            SliceH264Short {
                BSNALunitDataLocation: 0,
                SliceBytesInBuffer: 100,
                wBadSliceChopping: 0,
            },
            SliceH264Short {
                BSNALunitDataLocation: 100,
                SliceBytesInBuffer: 250,
                wBadSliceChopping: 0,
            },
        ];
        let bytes = slice_bytes(&records);
        // TEN bytes per record, so the second record starts at byte 10 — this is the
        // test that fails if the packing is ever relaxed back to natural alignment,
        // and it fails on the SECOND record, which is exactly where the driver
        // would have started misreading.
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[0..4], &0u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &100u32.to_le_bytes());
        assert_eq!(&bytes[8..10], &0u16.to_le_bytes());
        assert_eq!(&bytes[10..14], &100u32.to_le_bytes());
        assert_eq!(&bytes[14..18], &250u32.to_le_bytes());
        assert_eq!(&bytes[18..20], &0u16.to_le_bytes());
    }
}
