//! The AV1 bitstream buffer's contents, and the tile-control records that address
//! it — the counterpart of [`mod@crate::pack`], which cannot be reused because AV1 has
//! no Annex-B start codes to normalise and no slices to prefix.
//!
//! # What goes in the buffer
//!
//! Every tile-group (or frame) OBU's **`tile_data` region**, concatenated in plan
//! order: from the first tile's `tile_size_minus_1` field through the end of the
//! OBU payload. Not the OBU header, not the `obu_size` field, not — for an
//! `OBU_FRAME` — the frame header, all of which the driver reads out of
//! `DXVA_PicParams_AV1` instead. The `tile_size_minus_1` fields BETWEEN tiles do
//! ride along, unread.
//!
//! That is byte for byte what libavcodec's `dxva2_av1.c` uploads. Its
//! `decode_slice` is handed `raw_tile_group->tile_data.data` — CBS AV1's name for
//! exactly this region — and either points `ctx_pic->bitstream` straight at it
//! (the single-tile-group shortcut) or `memcpy`s each one onto the end of an
//! accumulating buffer; `commit_bitstream_and_slice_buffer` then `memcpy`s the
//! result into the driver's mapping. [`pf_vkdecode::Av1Bitstream::groups`] is that
//! same region, produced by the same walk that finds the tiles.
//!
//! ⚠ The native Vulkan rung uploads something DIFFERENT — the tile payloads alone,
//! size fields stripped — and both are correct, because both APIs address tiles by
//! an explicit (offset, size) pair and neither ever reads the bytes between them.
//! The layouts differ because the METHOD differs: on Vulkan the reference
//! implementation is libavcodec's Vulkan hwaccel, and here it is libavcodec's DXVA
//! hwaccel. This backend reproduces libavcodec on the evidence that a hand-built
//! variant of a D3D11VA submission was once rejected by an Intel driver outright,
//! so where a choice exists it is not made on first principles.
//!
//! # Two rules that differ from the H.264/HEVC packer
//!
//! 1. **The padding is charged to NOBODY.** `commit_bitstream_and_slice_buffer`
//!    pads the bitstream buffer to the 128-byte granule with the same expression
//!    `dxva2_h264.c` uses — `FFMIN(128 - (size & 127), dxva_size - size)`, so a
//!    buffer already on the granule still gets a full block — and adds it to the
//!    BUFFER's `DataSize`. It does not touch a single `DXVA_Tile_AV1`. The H.264
//!    and HEVC paths do the opposite (`SliceBytesInBuffer += padding` on the last
//!    record), and copying that habit here would tell the driver the last tile is
//!    up to 128 bytes longer than it is — trailing zeros are legal filler after a
//!    slice's `rbsp_trailing_bits`, but an AV1 tile's size is exact and its
//!    entropy decoder is not looking for a stop bit.
//! 2. **One record per TILE, not per tile group.** See [`TileAv1`].

use pf_vkdecode::Av1Bitstream;

use crate::dxva::BITSTREAM_ALIGN;
use crate::dxva_av1::TileAv1;
use crate::pack::PackError;

/// What came out of an AV1 pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedAv1 {
    /// The tile-control records as the driver reads them: the caller's rows,
    /// columns and `anchor_frame`, with `DataOffset`/`DataSize` rewritten to
    /// address the packed buffer.
    pub tiles: Vec<TileAv1>,
    /// Bytes written, padding included — the `DataSize` of the bitstream buffer's
    /// `D3D11_VIDEO_DECODER_BUFFER_DESC`.
    pub data_size: u32,
}

/// The exact byte count [`pack_av1`] needs before padding: every tile-group
/// region, end to end.
///
/// Separate from [`pack_av1`] for the same reason [`crate::pack::packed_size`] is:
/// so "how big is this access unit's tile data" has one answer rather than two
/// that can drift.
pub fn packed_size_av1(bitstream: &Av1Bitstream) -> usize {
    bitstream.groups.iter().fold(0usize, |total, group| {
        total.saturating_add(group.end.saturating_sub(group.start))
    })
}

/// Pack one frame's tile data into `dst`, returning the tile-control records that
/// address it.
///
/// `tiles` is the per-tile record template [`crate::plan_to_dxva_av1`] produced:
/// its rows, columns and `anchor_frame` are carried through untouched and its
/// access-unit-relative `DataOffset`/`DataSize` are REPLACED — wholly, both
/// fields, so no record can come out of here half-rebased.
///
/// `dst` is the driver's mapped bitstream buffer at its whole reported size, not a
/// sub-slice: the padding rule needs the real capacity, because a buffer with no
/// room for the tail padding gets as much as fits rather than an error (libavcodec
/// clamps the same way, and the picture is complete either way).
pub fn pack_av1(
    au: &[u8],
    bitstream: &Av1Bitstream,
    tiles: &[TileAv1],
    dst: &mut [u8],
) -> Result<PackedAv1, PackError> {
    if bitstream.tiles.is_empty() || bitstream.groups.is_empty() {
        return Err(PackError::NoTiles);
    }
    if tiles.len() != bitstream.tiles.len() {
        return Err(PackError::TileCountMismatch {
            records: tiles.len(),
            tiles: bitstream.tiles.len(),
        });
    }
    let needed = packed_size_av1(bitstream);
    if needed > dst.len() {
        return Err(PackError::BufferTooSmall {
            needed,
            capacity: dst.len(),
        });
    }

    // The tile-group regions, copied end to end. `bases` remembers where each
    // landed so a tile's offset is its position INSIDE its own group plus that
    // group's base — the arithmetic `dxva2_av1.c` spells as
    // `ctx_pic->bitstream_size + tile_offset`.
    let mut cursor = 0usize;
    let mut bases = Vec::with_capacity(bitstream.groups.len());
    for group in &bitstream.groups {
        let bytes = au.get(group.clone()).ok_or(PackError::RangeOutsideAu {
            start: group.start,
            end: group.end,
            au: au.len(),
        })?;
        dst[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        bases.push((group.clone(), cursor));
        cursor += bytes.len();
    }

    let mut records = Vec::with_capacity(tiles.len());
    for (tile, template) in bitstream.tiles.iter().zip(tiles) {
        // Which group holds this tile. Resolved by CONTAINMENT rather than by
        // re-deriving the per-group tile counts: the counts are how the walk split
        // the tiles in the first place, and a second derivation that disagreed
        // would silently rebase a tile against the wrong group's base.
        let (group, base) = bases
            .iter()
            .find(|(group, _)| group.start <= tile.start && tile.end <= group.end)
            .ok_or(PackError::TileOutsideGroup {
                start: tile.start,
                end: tile.end,
            })?;
        let offset = base + (tile.start - group.start);
        let size = tile.end - tile.start;
        records.push(TileAv1 {
            data_offset: u32::try_from(offset).map_err(|_| PackError::Overflow(offset))?,
            data_size: u32::try_from(size).map_err(|_| PackError::Overflow(size))?,
            ..*template
        });
    }

    // Tail padding to the 128-byte granule — libavcodec's expression verbatim, so
    // data already on the granule still gets a FULL block. Charged to the buffer's
    // `DataSize` and to no tile record (module docs).
    let want = BITSTREAM_ALIGN - (cursor % BITSTREAM_ALIGN);
    let padding = want.min(dst.len() - cursor);
    dst[cursor..cursor + padding].fill(0);
    cursor += padding;

    Ok(PackedAv1 {
        tiles: records,
        data_size: u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxva_av1::UNUSED_INDEX;

    /// Byte ranges from `(start, end)` pairs. Spelled this way rather than as
    /// `vec![a..b]` because clippy reads a one-element `Vec` of `Range` as a
    /// mistyped `vec![a; b]`, which is a fair thing to suspect and not what these
    /// are.
    fn ranges<const N: usize>(pairs: [(usize, usize); N]) -> Vec<std::ops::Range<usize>> {
        pairs.into_iter().map(|(start, end)| start..end).collect()
    }

    /// A record template with a recognisable row/column and offsets that must not
    /// survive the pack.
    fn template(row: u16, column: u16) -> TileAv1 {
        TileAv1 {
            data_offset: 0xDEAD_BEEF,
            data_size: 0xDEAD_BEEF,
            row,
            column,
            reserved16: 0,
            anchor_frame: UNUSED_INDEX,
            reserved8: 0,
        }
    }

    /// Two tile groups of one tile each, at AU offsets 10..20 and 40..55, with a
    /// two-byte size field ahead of nothing (single-tile groups code none) — so
    /// each group's region IS its tile.
    fn two_groups() -> (Vec<u8>, Av1Bitstream) {
        let mut au = vec![0u8; 64];
        for (i, byte) in au.iter_mut().enumerate() {
            *byte = i as u8;
        }
        (
            au,
            Av1Bitstream {
                tiles: ranges([(10, 20), (40, 55)]),
                groups: ranges([(10, 20), (40, 55)]),
            },
        )
    }

    #[test]
    fn the_tile_data_regions_are_concatenated_and_the_offsets_follow_them() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0xCCu8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(&dst[0..10], &au[10..20]);
        assert_eq!(&dst[10..25], &au[40..55]);
        assert_eq!(
            (packed.tiles[0].data_offset, packed.tiles[0].data_size),
            (0, 10)
        );
        assert_eq!(
            (packed.tiles[1].data_offset, packed.tiles[1].data_size),
            (10, 15),
            "the second group's tile is rebased onto the first group's length, \
             which is `ctx_pic->bitstream_size + tile_offset`"
        );
        // The template's rows and columns ride across; its poison offsets do not.
        assert_eq!((packed.tiles[1].row, packed.tiles[1].column), (0, 1));
        let anchor = packed.tiles[1].anchor_frame;
        assert_eq!(anchor, UNUSED_INDEX);
    }

    #[test]
    fn a_tile_inside_a_group_keeps_its_distance_from_the_group_start() {
        // One group, 100..160, holding two tiles: the first at 102..120 (two bytes
        // of `tile_size_minus_1` ahead of it) and the second at 122..160. The size
        // fields are COPIED and never addressed — which is the layout libavcodec
        // uploads and the thing a payload-only packer would not reproduce.
        let au: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(102, 120), (122, 160)]),
            groups: ranges([(100, 160)]),
        };
        let mut dst = vec![0u8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(
            &dst[0..60],
            &au[100..160],
            "the WHOLE region, size fields and all"
        );
        assert_eq!(
            (packed.tiles[0].data_offset, packed.tiles[0].data_size),
            (2, 18)
        );
        assert_eq!(
            (packed.tiles[1].data_offset, packed.tiles[1].data_size),
            (22, 38)
        );
    }

    #[test]
    fn the_padding_is_charged_to_the_buffer_and_to_no_tile_record() {
        // THE asymmetry with `pack`. 25 bytes of tile data pad to 128, and both
        // tiles' `DataSize` must still be their own exact byte counts — an AV1
        // tile's size is exact, and 103 bytes of trailing zeros handed to its
        // entropy decoder is not filler, it is corruption.
        let (au, bitstream) = two_groups();
        let mut dst = vec![0xCCu8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 128);
        assert_eq!(packed.tiles.iter().map(|t| t.data_size).sum::<u32>(), 25);
        assert!(
            dst[25..128].iter().all(|&b| b == 0),
            "padding must be zeros"
        );
        assert_eq!(
            dst[128], 0xCC,
            "past the data size the mapping is untouched"
        );
    }

    #[test]
    fn data_already_on_the_granule_still_gets_a_full_padding_block() {
        // libavcodec's `128 - (size & 127)` never yields zero, so a 128-byte
        // buffer reports 256. Reproduced verbatim rather than "fixed".
        let au: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(0, 128)]),
            groups: ranges([(0, 128)]),
        };
        let mut dst = vec![0u8; 512];
        let packed = pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 256);
        // `TileAv1` is `#[repr(packed)]`: read the field out before comparing it.
        let size = packed.tiles[0].data_size;
        assert_eq!(size, 128);
    }

    #[test]
    fn padding_is_clamped_to_what_the_mapping_can_hold() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 30];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 30);
    }

    #[test]
    fn an_au_larger_than_the_mapping_is_refused_rather_than_truncated() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 16];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst),
            Err(PackError::BufferTooSmall {
                needed: 25,
                capacity: 16,
            })
        );
        assert_eq!(packed_size_av1(&bitstream), 25);
    }

    #[test]
    fn a_region_outside_the_au_is_caught_before_it_indexes() {
        let au = vec![0u8; 32];
        let bitstream = Av1Bitstream {
            tiles: ranges([(10, 40)]),
            groups: ranges([(10, 40)]),
        };
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst),
            Err(PackError::RangeOutsideAu {
                start: 10,
                end: 40,
                au: 32,
            })
        );
    }

    #[test]
    fn a_tile_that_belongs_to_no_group_is_refused_rather_than_rebased_against_group_zero() {
        // The two halves of an `Av1Bitstream` disagreeing. Nothing in the walk can
        // produce this, which is exactly why it is checked rather than assumed:
        // the alternative to a typed refusal is a tile record pointing at another
        // tile's bytes, and a picture that decodes.
        let au: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(10, 20), (150, 160)]),
            groups: ranges([(10, 20)]),
        };
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst),
            Err(PackError::TileOutsideGroup {
                start: 150,
                end: 160,
            })
        );
    }

    #[test]
    fn a_record_count_that_disagrees_with_the_tile_count_is_refused() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst),
            Err(PackError::TileCountMismatch {
                records: 1,
                tiles: 2,
            })
        );
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let mut dst = vec![0u8; 512];
        let empty = Av1Bitstream {
            tiles: Vec::new(),
            groups: Vec::new(),
        };
        assert_eq!(
            pack_av1(&[], &empty, &[], &mut dst),
            Err(PackError::NoTiles)
        );
        assert_eq!(packed_size_av1(&empty), 0);
    }
}
