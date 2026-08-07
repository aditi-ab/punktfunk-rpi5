//! The buffer DESCRIPTORS one `ID3D11VideoContext::SubmitDecoderBuffers` call
//! carries: which buffers are in the set at all, and the four
//! `D3D11_VIDEO_DECODER_BUFFER_DESC` fields whose values are a DECISION rather
//! than a pointer the driver handed back.
//!
//! # Why this is a module of its own
//!
//! Review 13 found four defects in this backend. **Two of the three structural
//! ones lived here rather than in the picture parameters**: an HEVC
//! quantization-matrix buffer submitted unconditionally (so a driver was handed a
//! matrix of zeros on every stream that disables scaling lists), and a
//! `NumMBsInBuffer` left at 0 where libavcodec's H.264 path writes
//! `mb_width * mb_height` — on the exact call (`SubmitDecoderBuffers`) that this
//! codebase has already seen an Intel driver reject a hand-built variant on.
//!
//! Neither is visible in the picture parameters, neither is visible in a smoke
//! test, and — before this module — neither was visible to any gate this program
//! runs, because the descriptors were built inside `cfg(windows)` code that no CI
//! leg compiles. That is the whole reason the values live here: a descriptor set
//! is a pure function of the conversion's output plus the packer's output, so it
//! can be asserted on any host, on every leg, over every AU of the vendored
//! vectors.
//!
//! # ⚠ The Windows layer still builds its own for H.264 and HEVC — rewire them
//!
//! `pf-client-core`'s `video_d3d11_native.rs` was rewired for **AV1**
//! (`fill_and_submit_av1` builds its submission from [`descriptors_av1`] and
//! cross-checks every `DataSize` against what its writers actually wrote), and
//! that is what this module was written for. Its H.264 and HEVC arm
//! (`fill_and_submit_slices` + the private `buffer_desc`) still constructs the
//! same four descriptors itself and should be rewired the same way. Until it is,
//! the two must be read together: this module is the SPEC and the tests are its
//! proof, and a divergence between them is a defect in the Windows file. The
//! ordering, the values and the presence rule below are exactly what that file
//! does today, transcribed — not a new invention.
//!
//! # The values, and where each comes from
//!
//! `CompressedBufferType` (D3D11's `BufferType`) code points, from windows-rs at
//! the workspace's pinned rev (`acb5a1a`,
//! `crates/libs/windows/src/Windows/Win32/d3d11/mod.rs`) — the same numbers
//! DXVA2's `DXVA2_*BufferType` enumeration uses:
//!
//! | buffer | code point |
//! |---|---|
//! | picture parameters | 0 |
//! | inverse quantization matrix | 4 |
//! | slice control | 5 |
//! | bitstream | 6 |
//!
//! **Order**: picture parameters, quantization matrices, bitstream, slice
//! control. libavcodec's `ff_dxva2_common_end_frame` fills its four-entry
//! descriptor array in exactly that order and submits the array as filled; a
//! driver is entitled to care, and matching the path every Windows player
//! exercises costs nothing.
//!
//! **`DataOffset`** is 0 on every buffer, for both sides: each buffer is written
//! from its own mapping's byte 0. (libavcodec `memset`s the descriptor and never
//! writes the field.)
//!
//! **`DataSize`** is the number of bytes actually written: the whole
//! hand-declared struct for the picture parameters and the quantization matrices,
//! the packer's PADDED size for the bitstream ([`crate::pack::Packed::data_size`],
//! a multiple of [`crate::dxva::BITSTREAM_ALIGN`]), and `slices *
//! size_of::<DXVA_Slice_*_Short>()` for the slice control — **ten** bytes per
//! record, not twelve. That number is a measured fact rather than a derivation
//! (`dxva.rs`'s alignment section carries the measurement), and the slice-control
//! `DataSize` is where it is observable from outside: 20 bytes for a two-slice
//! H.264 picture, 10 for a one-segment HEVC one.
//!
//! **`NumMBsInBuffer` is codec-ASYMMETRIC, and that is not an accident to be
//! tidied up:**
//!
//! * H.264 — `mb_width * mb_height` on the BITSTREAM and SLICE_CONTROL
//!   descriptors ([`crate::pic::DecodePlanDxva::mb_count`]);
//! * HEVC — 0 on the same two. HEVC has no macroblocks and the field has no CTB
//!   spelling;
//! * **AV1 — 0 on all three**, and neither a tile count nor a superblock count.
//!   `dxva2_av1.c`'s `commit_bitstream_and_slice_buffer` writes a literal
//!   `dsc11->NumMBsInBuffer = 0` on the bitstream descriptor and passes a literal
//!   `0` as `ff_dxva2_commit_buffer`'s `mb_count` for the tile buffer;
//! * picture parameters and quantization matrices — 0 in every codec.
//!
//! That asymmetry is libavcodec's, read out of an **FFmpeg n8.1** tree:
//! `dxva2_h264.c:307` computes `const unsigned mb_count = h->mb_width *
//! h->mb_height` and writes it on the bitstream descriptor (`:412` D3D11, `:425`
//! DXVA2) and passes it for the slice-control commit (`:440-442`);
//! `dxva2_hevc.c` writes a literal 0 in the same three places (`:338`, `:349`,
//! `:359-361`); and `dxva2.c` passes a literal 0 for the two parameter buffers.
//! Setting a CTB count on the HEVC path would be a fresh divergence in the other
//! direction, which is why it is spelled out here rather than left to symmetry.
//!
//! # Presence: the quantization matrix is codec-asymmetric too
//!
//! * **H.264: always submitted.** `dxva2_h264.c:513-516` passes `&ctx_pic->qm`
//!   with `sizeof(qm)` unconditionally, and the PPS's lists are always meaningful
//!   (the vendored parser has already applied Table 7-2's fallback rules, so a PPS
//!   that codes no matrix carries the SPS's or the flat default).
//! * **HEVC: submitted only when the sequence enables scaling lists.**
//!   `dxva2_hevc.c:417` takes `int scale = ctx_pic->pp.dwCodingParamToolFlags & 1`
//!   — bit 0 is `scaling_list_enabled_flag` — and `:423-426` passes `NULL`/0 when
//!   it is clear; the generic layer then submits an IQ-matrix buffer only `if
//!   (qm_size > 0)` (`dxva2.c` ~962), with `NumMBsInBuffer` 0.
//!   [`crate::pic_h265::DecodePlanDxvaH265::qmatrix`] is `None` in exactly that
//!   case, so presence here is `qmatrix.is_some()` and nothing else. Handing a
//!   driver a matrix the picture parameters just told it to ignore is a bet on the
//!   driver ignoring it too — and with the vendored parser leaving an uncoded list
//!   all-zero, the losing side of that bet is every residual dequantizing to
//!   nothing.
//!
//! * **AV1: never.** `dxva2_av1_end_frame` calls `ff_dxva2_common_end_frame` with
//!   `NULL, 0` for the matrix pair, and the generic layer's `if (qm_size > 0)`
//!   then skips the buffer entirely — so an AV1 submission is THREE buffers,
//!   always. AV1's quantiser matrices are selected by index
//!   (`qm_y`/`qm_u`/`qm_v` in `DXVA_PicParams_AV1::quantization`) out of tables
//!   the decoder already has, not transmitted; there is no matrix to send.
//!
//! ⚠ The flag test is NECESSARY but not SUFFICIENT. HEVC 7.4.5 says that with
//! `scaling_list_enabled_flag` set and NO scaling-list data in either parameter
//! set, the Table 7-5/7-6 DEFAULT lists apply. FFmpeg's parser seeds those
//! defaults; the vendored cros-codecs parser leaves an uncoded SPS's lists ALL
//! ZERO. So "submit iff the flag" is only half the rule, and the other half lives
//! in [`crate::pic_h265`]'s `quantization_matrices`, which reads the PPS's copy
//! (which that parser DOES default-fill) unless the SPS is the only side that
//! coded any. All three cases are named CPU tests — two in `pic_h265.rs` for the
//! contents, three in `tests/libav_picparams_parity.rs` for the submission fact.
//!
//! # Provenance
//!
//! The libavcodec file:line references above were read out of an FFmpeg n8.1 tree
//! by this work package's coordinator, not out of this repository — there is no
//! FFmpeg source in the worktree, so nothing here can verify them, and a capture is
//! the authority. The buffer ORDER is the one claim with no line reference: it is
//! what `video_d3d11_native.rs` already submits and what
//! `ff_dxva2_common_end_frame` fills its array in, and the harness's descriptor
//! comparison is what will confirm it.

use std::mem::size_of;

use crate::dxva::PicParamsH264;
use crate::dxva::PicParamsHevc;
use crate::dxva::QmatrixH264;
use crate::dxva::QmatrixHevc;
use crate::dxva::SliceH264Short;
use crate::dxva::SliceHevcShort;
use crate::dxva_av1::PicParamsAv1;
use crate::dxva_av1::TileAv1;
use crate::pack::Packed;
use crate::pack_av1::PackedAv1;
use crate::pic::DecodePlanDxva;
use crate::pic_h265::DecodePlanDxvaH265;

/// `D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS`.
pub const BUFFER_PICTURE_PARAMETERS: u32 = 0;
/// `D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX`.
pub const BUFFER_INVERSE_QUANTIZATION_MATRIX: u32 = 4;
/// `D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL`.
pub const BUFFER_SLICE_CONTROL: u32 = 5;
/// `D3D11_VIDEO_DECODER_BUFFER_BITSTREAM`.
pub const BUFFER_BITSTREAM: u32 = 6;

/// One buffer of a submission, reduced to the fields a caller DECIDES.
///
/// Deliberately not a `D3D11_VIDEO_DECODER_BUFFER_DESC`: that structure has
/// fourteen members, of which ten are either for a mode this backend does not use
/// (`BufferIndex`, `FirstMBaddress`, `Width`/`Height`/`Stride` — motion-compensation
/// buffers), or for protected content (`pIV`, `IVSize`, `PartialEncryption`,
/// `EncryptedBlockInfo`), or reserved. All ten are zero on every buffer this
/// backend submits, which the Windows layer expresses as `..Default::default()`;
/// the four here are the ones that carry a decision, and therefore the ones a
/// comparison against libavcodec is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferDescriptor {
    /// `BufferType` — one of this module's `BUFFER_*` code points. (The DXVA
    /// specs and libavcodec's DXVA2 path call the same field
    /// `CompressedBufferType`.)
    pub buffer_type: u32,
    /// `DataOffset` — 0 on every buffer of every submission (module docs).
    pub data_offset: u32,
    /// `DataSize` — bytes written into the driver's mapping.
    pub data_size: u32,
    /// `NumMBsInBuffer` — codec-asymmetric; see the module docs.
    pub num_mbs_in_buffer: u32,
}

impl BufferDescriptor {
    /// A descriptor with `DataOffset` 0, which is the only value this backend
    /// ever submits.
    const fn new(buffer_type: u32, data_size: u32, num_mbs_in_buffer: u32) -> BufferDescriptor {
        BufferDescriptor {
            buffer_type,
            data_offset: 0,
            data_size,
            num_mbs_in_buffer,
        }
    }
}

/// The slice-control buffer's `DataSize`: `n` short-format records back to back,
/// exactly as [`crate::dxva::slice_bytes`] lays them out.
///
/// Saturating rather than panicking on the (unreachable) overflow: a `u32` holds
/// 429 million ten-byte records, and an AU that produced more has already been
/// refused by the packer.
fn slice_control_size(record_size: usize, records: usize) -> u32 {
    u32::try_from(record_size.saturating_mul(records)).unwrap_or(u32::MAX)
}

/// The descriptor set of one H.264 submission, in libavcodec's order.
///
/// Four buffers, always: the quantization matrices travel on every H.264 picture
/// (module docs).
pub fn descriptors_h264(plan: &DecodePlanDxva, packed: &Packed) -> Vec<BufferDescriptor> {
    let mb_count = plan.mb_count;
    vec![
        BufferDescriptor::new(
            BUFFER_PICTURE_PARAMETERS,
            size_of::<PicParamsH264>() as u32,
            0,
        ),
        BufferDescriptor::new(
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            size_of::<QmatrixH264>() as u32,
            0,
        ),
        BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, mb_count),
        BufferDescriptor::new(
            BUFFER_SLICE_CONTROL,
            slice_control_size(size_of::<SliceH264Short>(), packed.records.len()),
            mb_count,
        ),
    ]
}

/// The descriptor set of one HEVC submission, in libavcodec's order.
///
/// THREE buffers when the sequence disables scaling lists (which is every
/// punktfunk HEVC stream and the vendored vector with it), four when it enables
/// them — and `NumMBsInBuffer` is 0 on all of them (module docs).
pub fn descriptors_h265(plan: &DecodePlanDxvaH265, packed: &Packed) -> Vec<BufferDescriptor> {
    let mut out = Vec::with_capacity(4);
    out.push(BufferDescriptor::new(
        BUFFER_PICTURE_PARAMETERS,
        size_of::<PicParamsHevc>() as u32,
        0,
    ));
    if plan.qmatrix.is_some() {
        out.push(BufferDescriptor::new(
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            size_of::<QmatrixHevc>() as u32,
            0,
        ));
    }
    out.push(BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, 0));
    out.push(BufferDescriptor::new(
        BUFFER_SLICE_CONTROL,
        slice_control_size(size_of::<SliceHevcShort>(), packed.records.len()),
        0,
    ));
    out
}

/// The descriptor set of one AV1 submission, in libavcodec's order.
///
/// **THREE buffers, always**, and `NumMBsInBuffer` 0 on every one of them (module
/// docs). The slice-control buffer carries `DXVA_Tile_AV1` records — sixteen bytes
/// each, one per TILE — where the other two codecs carry ten-byte slice records.
///
/// The bitstream `DataSize` is the packer's PADDED figure, which for AV1 is the
/// only place the padding is accounted at all: no tile record grows by it
/// ([`mod@crate::pack_av1`]).
pub fn descriptors_av1(packed: &PackedAv1) -> Vec<BufferDescriptor> {
    vec![
        BufferDescriptor::new(
            BUFFER_PICTURE_PARAMETERS,
            size_of::<PicParamsAv1>() as u32,
            0,
        ),
        BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, 0),
        BufferDescriptor::new(
            BUFFER_SLICE_CONTROL,
            slice_control_size(size_of::<TileAv1>(), packed.tiles.len()),
            0,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxva::PicEntry;
    use crate::pack::SliceRecord;
    use crate::pic::DxvaRef;

    /// A conversion result with nothing in it but the two fields the descriptors
    /// read. Built by hand rather than planned from a vector: this module's job is
    /// the descriptor SET, and the whole-stream evidence (250 H.264 + 250 HEVC AUs
    /// through the real planners) is in `tests/libav_picparams_parity.rs`.
    fn h264_plan(mb_count: u32) -> DecodePlanDxva {
        DecodePlanDxva {
            pic_params: PicParamsH264::zeroed(),
            qmatrix: QmatrixH264::zeroed(),
            slice_ranges: Vec::new(),
            setup_slot: 0,
            setup_id: 1,
            setup_is_reference: true,
            refs: Vec::<DxvaRef>::new(),
            mb_count,
        }
    }

    fn h265_plan(qmatrix: Option<QmatrixHevc>) -> DecodePlanDxvaH265 {
        DecodePlanDxvaH265 {
            pic_params: PicParamsHevc::zeroed(),
            qmatrix,
            slice_ranges: Vec::new(),
            setup_slot: 0,
            setup_id: 1,
            setup_is_reference: true,
            refs: Vec::new(),
        }
    }

    /// `n` slices packed into `data_size` bytes; the record contents do not matter
    /// here, only how many there are.
    fn packed(slices: usize, data_size: u32) -> Packed {
        Packed {
            records: (0..slices)
                .map(|i| SliceRecord {
                    location: i as u32 * 64,
                    bytes: 64,
                })
                .collect(),
            data_size,
        }
    }

    #[test]
    fn the_buffer_type_code_points_are_the_ones_windows_rs_declares() {
        // From the workspace's pinned windows-rs rev (`acb5a1a`),
        // `crates/libs/windows/src/Windows/Win32/d3d11/mod.rs`:
        // D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS = 0,
        // …_INVERSE_QUANTIZATION_MATRIX = 4, …_SLICE_CONTROL = 5, …_BITSTREAM = 6.
        // Nothing else in this crate can catch a transposed pair, and a
        // transposition would hand the driver a bitstream where it expects slice
        // control.
        assert_eq!(BUFFER_PICTURE_PARAMETERS, 0);
        assert_eq!(BUFFER_INVERSE_QUANTIZATION_MATRIX, 4);
        assert_eq!(BUFFER_SLICE_CONTROL, 5);
        assert_eq!(BUFFER_BITSTREAM, 6);
    }

    #[test]
    fn an_h264_submission_carries_four_buffers_in_libavcodecs_order() {
        let descs = descriptors_h264(&h264_plan(300), &packed(2, 512));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 1040);
        assert_eq!(descs[1].data_size, 224);
        assert_eq!(descs[2].data_size, 512);
        assert_eq!(descs[3].data_size, 2 * 10, "two ten-byte short records");
    }

    #[test]
    fn only_the_h264_bitstream_and_slice_control_buffers_carry_a_macroblock_count() {
        // Review 13's defect, in the smallest form that can express it: the field
        // is 0 on the two parameter buffers and mb_width*mb_height on the two the
        // hardware parses.
        let descs = descriptors_h264(&h264_plan(300), &packed(1, 256));
        assert_eq!(descs[0].num_mbs_in_buffer, 0, "picture parameters");
        assert_eq!(descs[1].num_mbs_in_buffer, 0, "quantization matrices");
        assert_eq!(descs[2].num_mbs_in_buffer, 300, "bitstream");
        assert_eq!(descs[3].num_mbs_in_buffer, 300, "slice control");
    }

    #[test]
    fn an_hevc_submission_omits_the_quantization_matrix_buffer_when_there_is_none() {
        let descs = descriptors_h265(&h265_plan(None), &packed(1, 384));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "a submission with no matrix must not carry an empty matrix buffer"
        );
        assert!(descs
            .iter()
            .all(|d| d.buffer_type != BUFFER_INVERSE_QUANTIZATION_MATRIX));
    }

    #[test]
    fn an_hevc_submission_carries_the_quantization_matrix_buffer_when_there_is_one() {
        let descs = descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(3, 640));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 232);
        assert_eq!(descs[1].data_size, 1000);
        assert_eq!(descs[2].data_size, 640);
        assert_eq!(descs[3].data_size, 3 * 10, "three ten-byte short records");
    }

    #[test]
    fn the_hevc_descriptors_carry_no_macroblock_count_at_all() {
        // The asymmetry, asserted rather than assumed: libavcodec's HEVC path
        // writes 0 where its H.264 path writes mb_width*mb_height, and a CTB count
        // here would be a divergence in the other direction.
        for descs in [
            descriptors_h265(&h265_plan(None), &packed(1, 256)),
            descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(4, 1024)),
        ] {
            for desc in descs {
                assert_eq!(
                    desc.num_mbs_in_buffer, 0,
                    "buffer type {} carries a macroblock count",
                    desc.buffer_type
                );
            }
        }
    }

    #[test]
    fn every_descriptor_starts_at_byte_zero_of_its_own_buffer() {
        let h264 = descriptors_h264(&h264_plan(1), &packed(2, 256));
        let h265 = descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(2, 256));
        for desc in h264.into_iter().chain(h265) {
            assert_eq!(desc.data_offset, 0);
        }
    }

    #[test]
    fn the_slice_control_size_is_one_short_format_record_per_slice() {
        // TEN bytes per record is the SHORT format, packed — measured against
        // libavcodec on hardware, not derived from the field types (a `#[repr(C)]`
        // `{u32, u32, u16}` would be twelve). The long format's record is an order of
        // magnitude larger, so this size is also the check that the records match the
        // `ConfigBitstreamRaw` this backend asks for.
        assert_eq!(size_of::<SliceH264Short>(), 10);
        assert_eq!(size_of::<SliceHevcShort>(), 10);
        for slices in [1usize, 2, 5, 68] {
            let h264 = descriptors_h264(&h264_plan(1), &packed(slices, 4096));
            assert_eq!(h264[3].data_size, 10 * slices as u32);
            let h265 = descriptors_h265(&h265_plan(None), &packed(slices, 4096));
            assert_eq!(h265[2].data_size, 10 * slices as u32);
        }
    }

    /// `n` tiles packed into `data_size` bytes.
    fn packed_av1(tiles: usize, data_size: u32) -> PackedAv1 {
        PackedAv1 {
            tiles: (0..tiles)
                .map(|i| TileAv1 {
                    data_offset: i as u32 * 64,
                    data_size: 64,
                    row: 0,
                    column: i as u16,
                    ..Default::default()
                })
                .collect(),
            data_size,
        }
    }

    #[test]
    fn an_av1_submission_carries_three_buffers_and_never_a_quantization_matrix() {
        // `dxva2_av1_end_frame` passes `NULL, 0` for the matrix pair, so the
        // generic layer's `if (qm_size > 0)` never fires. A fourth buffer here
        // would be a matrix AV1 does not transmit at all.
        let descs = descriptors_av1(&packed_av1(1, 384));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 912, "DXVA_PicParams_AV1, measured");
        assert_eq!(descs[1].data_size, 384);
        assert_eq!(descs[2].data_size, 16, "one sixteen-byte DXVA_Tile_AV1");
    }

    #[test]
    fn the_av1_descriptors_carry_no_macroblock_count_at_all() {
        // The third spelling of the asymmetry: H.264 writes mb_width*mb_height,
        // HEVC writes 0, AV1 writes 0 — and specifically NOT a tile count, which
        // is the symmetric-looking value there is now a plausible field for.
        for tiles in [1usize, 4, 64] {
            for desc in descriptors_av1(&packed_av1(tiles, 4096)) {
                assert_eq!(
                    desc.num_mbs_in_buffer, 0,
                    "buffer type {} carries a macroblock count",
                    desc.buffer_type
                );
                assert_eq!(desc.data_offset, 0);
            }
        }
    }

    #[test]
    fn the_av1_tile_buffer_is_sixteen_bytes_per_tile_not_ten() {
        // The slice-control buffer is the one place a codec's record SIZE is
        // observable from outside, and AV1's record is a different structure from
        // the other two: `DXVA_Tile_AV1` is 16 bytes (measured against the Windows
        // SDK's `dxva.h`), where `DXVA_Slice_*_Short` is 10.
        assert_eq!(size_of::<TileAv1>(), 16);
        for tiles in [1usize, 2, 8, 64] {
            let descs = descriptors_av1(&packed_av1(tiles, 4096));
            assert_eq!(descs[2].data_size, 16 * tiles as u32);
        }
    }

    #[test]
    fn a_reference_entry_in_the_plan_does_not_reach_the_descriptors() {
        // A guard on the shape of this module rather than on a value: descriptors
        // are a function of SIZES and the macroblock count, so nothing about the
        // reference list may leak into them. (Also keeps `DxvaRef` in the test's
        // vocabulary, so the plan built above stays a realistic one.)
        let mut plan = h264_plan(300);
        plan.refs.push(DxvaRef {
            slot: 2,
            id: 7,
            is_long_term: true,
            top_field_order_cnt: 4,
            bottom_field_order_cnt: 4,
            frame_num_or_lt_idx: 1,
        });
        plan.pic_params.RefFrameList[0] = PicEntry::new(2, true);
        assert_eq!(
            descriptors_h264(&plan, &packed(1, 256)),
            descriptors_h264(&h264_plan(300), &packed(1, 256))
        );
    }
}
