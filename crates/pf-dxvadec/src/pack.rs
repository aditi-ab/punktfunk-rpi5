//! Bitstream-buffer packing: the AU's slice NALUs laid into the mapping
//! `GetDecoderBuffer(D3D11_VIDEO_DECODER_BUFFER_BITSTREAM)` hands back, plus the
//! byte locations the slice-control records point at.
//!
//! Pure, so the rules that are easy to get subtly wrong — the start-code
//! normalisation and the 128-byte tail padding — are unit-tested on every CI leg
//! rather than inferred from a picture on someone's screen.
//!
//! Three rules, all from the DXVA specs and all matching what libavcodec's
//! `dxva2_h264.c`/`dxva2_hevc.c` submit:
//!
//! 1. **Every slice is preceded by a three-byte start code** (`00 00 01`) in the
//!    buffer, and `BSNALunitDataLocation` points at the start code, not at the
//!    NALU header. The AU as planned already carries start codes, but Annex-B
//!    allows a four-byte one (a `zero_byte` ahead of the three-byte prefix) and
//!    the plan's ranges include whichever the encoder emitted; normalising to
//!    three keeps every driver looking at the byte pattern its reference
//!    implementation was written against.
//! 2. **`SliceBytesInBuffer` counts the start code**, so it is `3 + NALU bytes`.
//! 3. **The buffer's data size is padded to a multiple of 128 bytes with zeros**,
//!    and the padding is charged to the LAST slice's `SliceBytesInBuffer`.
//!    Trailing zeros after a slice's `rbsp_trailing_bits` are legal bitstream
//!    filler, so a driver that reads to the stated length reads only padding —
//!    which is precisely why the padding must be counted rather than left
//!    dangling past the last record.
//!
//! Non-VCL NALUs never enter the buffer: only the plan's slice ranges are packed.
//! That is the same discipline pf-vkdecode's recording layer follows (feeding
//! whole AUs, parameter sets included, hangs VCN firmware), and here it also
//! keeps `BSNALunitDataLocation` meaningful — the specs define it as the offset
//! of a *slice* NALU.

use std::ops::Range;

use crate::dxva::BITSTREAM_ALIGN;

/// One packed slice, in the codec-neutral shape both slice-control records need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceRecord {
    /// `BSNALunitDataLocation` — byte offset of the slice's start code within
    /// the bitstream buffer.
    pub location: u32,
    /// `SliceBytesInBuffer` — start code plus NALU bytes, plus (on the last
    /// slice only) the buffer's tail padding.
    pub bytes: u32,
}

/// What came out of a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub records: Vec<SliceRecord>,
    /// Bytes written, padding included — the `DataSize` of the bitstream
    /// buffer's `D3D11_VIDEO_DECODER_BUFFER_DESC`.
    pub data_size: u32,
}

/// Why an AU could not be packed. All three are stream or sizing conditions the
/// caller turns into a refused AU, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// The plan held no slices.
    NoSlices,
    /// A slice range falls outside the AU buffer it was planned from — a caller
    /// bug (a plan paired with the wrong AU), checked rather than trusted
    /// because the alternative is an out-of-bounds slice index.
    RangeOutsideAu { start: usize, end: usize, au: usize },
    /// A slice range does not begin with an Annex-B start code. pf-bitstream
    /// plans from an Annex-B AU, so this means the AU was mutated between
    /// planning and packing.
    NoStartCode { start: usize },
    /// The driver's bitstream buffer cannot hold this AU. Real: a mapping is
    /// typically a few MiB and a 4K IDR can approach it; the honest answer is a
    /// refused AU (and the recovery request that follows) rather than a
    /// truncated picture.
    BufferTooSmall { needed: usize, capacity: usize },
    /// A byte offset or length exceeded `u32`, which is what the DXVA records
    /// carry.
    Overflow(usize),
    /// AV1 ([`mod@crate::pack_av1`]): the frame carried no tile data.
    NoTiles,
    /// AV1: a tile payload lies inside none of the tile-group regions the same
    /// walk produced. Unreachable through [`pf_vkdecode::plan_bitstream`], and
    /// checked because the alternative to refusing is a tile record addressing
    /// another tile's bytes.
    TileOutsideGroup { start: usize, end: usize },
    /// AV1: the caller's record template and the walk's tile list are different
    /// lengths, so no record can be matched to a tile with confidence.
    TileCountMismatch { records: usize, tiles: usize },
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::NoSlices => write!(f, "the plan holds no slices"),
            PackError::RangeOutsideAu { start, end, au } => {
                write!(
                    f,
                    "slice range {start}..{end} falls outside the {au}-byte AU"
                )
            }
            PackError::NoStartCode { start } => {
                write!(f, "the slice at byte {start} carries no Annex-B start code")
            }
            PackError::BufferTooSmall { needed, capacity } => write!(
                f,
                "the AU needs {needed} bitstream bytes; the driver's buffer holds {capacity}"
            ),
            PackError::Overflow(value) => write!(f, "byte value {value} exceeds u32"),
            PackError::NoTiles => write!(f, "the frame carried no tile data"),
            PackError::TileOutsideGroup { start, end } => write!(
                f,
                "tile payload {start}..{end} lies inside no tile-group region"
            ),
            PackError::TileCountMismatch { records, tiles } => write!(
                f,
                "{records} tile records against {tiles} tiles in the bitstream"
            ),
        }
    }
}

impl std::error::Error for PackError {}

/// The payload of an Annex-B NALU: the bytes after its start code.
///
/// Accepts both prefixes the standard allows — `00 00 01` and `00 00 00 01` —
/// and nothing else. Deliberately not a general scan: the plan says this range
/// IS a NALU, so a start code anywhere but the front would mean the range is
/// wrong, and finding a later one would paper over that.
fn nalu_payload(nalu: &[u8]) -> Option<&[u8]> {
    match nalu {
        [0, 0, 1, rest @ ..] => Some(rest),
        [0, 0, 0, 1, rest @ ..] => Some(rest),
        _ => None,
    }
}

/// The exact byte count [`pack`] needs before padding: three start-code bytes
/// plus each slice's NALU payload.
///
/// Separate from [`pack`] so a caller can size or check a mapping before it maps
/// one — and so the "how big is this AU" question has one answer rather than two
/// that can drift.
pub fn packed_size(au: &[u8], slices: &[Range<usize>]) -> Result<usize, PackError> {
    if slices.is_empty() {
        return Err(PackError::NoSlices);
    }
    let mut total = 0usize;
    for range in slices {
        let nalu = au.get(range.clone()).ok_or(PackError::RangeOutsideAu {
            start: range.start,
            end: range.end,
            au: au.len(),
        })?;
        let payload = nalu_payload(nalu).ok_or(PackError::NoStartCode { start: range.start })?;
        total = total.saturating_add(3 + payload.len());
    }
    Ok(total)
}

/// Pack `slices` of `au` into `dst`, returning the slice-control locations.
///
/// `dst` is the driver's mapped bitstream buffer (its whole reported size, not a
/// sub-slice): the padding rule needs to know the real capacity, because a
/// buffer with no room for the tail padding gets as much as fits rather than an
/// error — the picture is complete either way, and libavcodec clamps the same
/// way.
pub fn pack(au: &[u8], slices: &[Range<usize>], dst: &mut [u8]) -> Result<Packed, PackError> {
    let needed = packed_size(au, slices)?;
    if needed > dst.len() {
        return Err(PackError::BufferTooSmall {
            needed,
            capacity: dst.len(),
        });
    }

    let mut records = Vec::with_capacity(slices.len());
    let mut cursor = 0usize;
    for range in slices {
        // `packed_size` already proved both of these; re-deriving rather than
        // caching keeps this loop's indexing self-evidently in bounds.
        let nalu = au.get(range.clone()).ok_or(PackError::RangeOutsideAu {
            start: range.start,
            end: range.end,
            au: au.len(),
        })?;
        let payload = nalu_payload(nalu).ok_or(PackError::NoStartCode { start: range.start })?;
        let location = u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?;
        dst[cursor..cursor + 3].copy_from_slice(&[0, 0, 1]);
        dst[cursor + 3..cursor + 3 + payload.len()].copy_from_slice(payload);
        cursor += 3 + payload.len();
        let bytes =
            u32::try_from(3 + payload.len()).map_err(|_| PackError::Overflow(payload.len()))?;
        records.push(SliceRecord { location, bytes });
    }

    // Tail padding to the 128-byte granule. libavcodec's expression is
    // `128 - (current & 127)`, which yields a FULL 128-byte block when the data
    // already lands on the granule rather than yielding zero — reproduced
    // verbatim, because "the buffer always ends in at least one padding byte" is
    // a property some drivers have been observed to want and none can object to.
    let want = BITSTREAM_ALIGN - (cursor % BITSTREAM_ALIGN);
    let padding = want.min(dst.len() - cursor);
    dst[cursor..cursor + padding].fill(0);
    cursor += padding;
    if let Some(last) = records.last_mut() {
        // Charged to the last record so a driver reading `SliceBytesInBuffer`
        // never walks past the data size — see the module docs.
        last.bytes = last
            .bytes
            .saturating_add(u32::try_from(padding).unwrap_or(u32::MAX));
    }

    Ok(Packed {
        records,
        data_size: u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An AU of `count` slices, each `payload` bytes, alternating between the
    /// three- and four-byte start codes so both prefixes are exercised.
    fn synth_au(count: usize, payload: usize) -> (Vec<u8>, Vec<Range<usize>>) {
        let mut au = Vec::new();
        let mut ranges = Vec::new();
        for i in 0..count {
            let start = au.len();
            if i % 2 == 0 {
                au.extend_from_slice(&[0, 0, 1]);
            } else {
                au.extend_from_slice(&[0, 0, 0, 1]);
            }
            // A recognisable, non-zero payload per slice.
            au.extend(std::iter::repeat_n(0xA0 + i as u8, payload));
            ranges.push(start..au.len());
        }
        (au, ranges)
    }

    #[test]
    fn each_slice_is_written_with_a_three_byte_start_code_whatever_the_au_carried() {
        let (au, ranges) = synth_au(2, 5);
        let mut dst = vec![0xCCu8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.records.len(), 2);
        // Slice 0: three-byte prefix in the AU, three-byte prefix in the buffer.
        assert_eq!(packed.records[0].location, 0);
        assert_eq!(&dst[0..3], &[0, 0, 1]);
        assert_eq!(&dst[3..8], &[0xA0; 5]);
        assert_eq!(packed.records[0].bytes, 8);
        // Slice 1 carried a FOUR-byte prefix in the AU and still lands as three.
        assert_eq!(packed.records[1].location, 8);
        assert_eq!(&dst[8..11], &[0, 0, 1]);
        assert_eq!(&dst[11..16], &[0xA1; 5]);
    }

    #[test]
    fn the_buffer_is_padded_to_a_128_byte_granule_and_the_padding_is_charged_to_the_last_slice() {
        let (au, ranges) = synth_au(1, 5);
        let mut dst = vec![0xCCu8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        // 3 + 5 = 8 bytes of slice, padded to 128.
        assert_eq!(packed.data_size, 128);
        assert_eq!(packed.records[0].bytes, 128);
        assert!(dst[8..128].iter().all(|&b| b == 0), "padding must be zeros");
        // Beyond the reported data size the mapping is untouched — the driver
        // never looks there, and neither do we.
        assert_eq!(dst[128], 0xCC);
    }

    #[test]
    fn data_already_on_the_granule_still_gets_a_full_padding_block() {
        // 125-byte payload + 3-byte start code = 128 exactly.
        let (au, ranges) = synth_au(1, 125);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.data_size, 256);
        assert_eq!(packed.records[0].bytes, 256);
    }

    #[test]
    fn padding_is_clamped_to_what_the_mapping_can_hold() {
        let (au, ranges) = synth_au(1, 5);
        // Exactly enough for the slice and ten spare bytes.
        let mut dst = vec![0u8; 18];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.data_size, 18);
        assert_eq!(packed.records[0].bytes, 18);
    }

    #[test]
    fn an_au_larger_than_the_mapping_is_refused_rather_than_truncated() {
        let (au, ranges) = synth_au(2, 100);
        let mut dst = vec![0u8; 64];
        assert_eq!(
            pack(&au, &ranges, &mut dst),
            Err(PackError::BufferTooSmall {
                needed: 206,
                capacity: 64,
            })
        );
    }

    #[test]
    fn locations_are_contiguous_across_a_multi_slice_au() {
        let (au, ranges) = synth_au(4, 30);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        let mut expected = 0u32;
        for (i, record) in packed.records.iter().enumerate() {
            assert_eq!(record.location, expected);
            // Only the last record carries padding.
            if i + 1 < packed.records.len() {
                assert_eq!(record.bytes, 33);
            }
            expected += 33;
        }
        assert_eq!(packed.data_size, 4 * 33 + (128 - (4 * 33) % 128));
    }

    #[test]
    fn a_slice_without_a_start_code_is_a_typed_error_not_a_mis_packed_buffer() {
        let au = vec![0x41u8; 32];
        let mut dst = vec![0u8; 512];
        let range = 0usize..32;
        assert_eq!(
            pack(&au, std::slice::from_ref(&range), &mut dst),
            Err(PackError::NoStartCode { start: 0 })
        );
    }

    #[test]
    fn a_range_outside_the_au_is_caught_before_it_indexes() {
        let (au, _) = synth_au(1, 5);
        let mut dst = vec![0u8; 512];
        let au_len = au.len();
        let range = 0usize..au_len + 1;
        assert_eq!(
            pack(&au, std::slice::from_ref(&range), &mut dst),
            Err(PackError::RangeOutsideAu {
                start: 0,
                end: au_len + 1,
                au: au_len,
            })
        );
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let mut dst = vec![0u8; 512];
        assert_eq!(pack(&[], &[], &mut dst), Err(PackError::NoSlices));
        assert_eq!(packed_size(&[], &[]), Err(PackError::NoSlices));
    }

    #[test]
    fn packed_size_agrees_with_what_pack_writes_before_padding() {
        let (au, ranges) = synth_au(3, 17);
        assert_eq!(packed_size(&au, &ranges).unwrap(), 3 * 20);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        let unpadded: u32 =
            packed.records.iter().map(|r| r.bytes).sum::<u32>() - (packed.data_size - 3 * 20u32);
        assert_eq!(unpadded, 60);
    }
}
