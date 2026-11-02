//! AC-3 / E-AC-3 passthrough on the `0xD4` plane: the host encodes, the client hands each
//! access unit to a receiver unchanged. Core decodes nothing here.
//!
//! An access unit is whole [`BLOCK_SAMPLES`] blocks per channel: AC-3 always 6, E-AC-3 1, 2, 3
//! or 6. `Welcome::audio_frame_us` labels that length and rounds, so size from
//! [`frame_samples`]. The bitrate is fixed per layout ([`bitrate_kbps`]) and sits outside ABR.
//! [`AuAssembler`] rebuilds an access unit from its `0xD4` parts
//! ([`AUDIO_BITSTREAM_MAGIC`](crate::quic::AUDIO_BITSTREAM_MAGIC)), which share one seq: one
//! unit is one seq, as one Opus frame is.

use crate::quic::{AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3};

/// Samples per channel in one block; every access unit is whole blocks.
/// cbindgen:ignore
pub const BLOCK_SAMPLES: usize = 256;

/// [`Hello::audio_bitstreams`](crate::quic::Hello::audio_bitstreams): the sink plays AC-3
/// through unchanged.
pub const AUDIO_BITSTREAM_AC3: u8 = 0x01;
/// [`Hello::audio_bitstreams`](crate::quic::Hello::audio_bitstreams): the sink plays E-AC-3
/// through unchanged.
pub const AUDIO_BITSTREAM_EAC3: u8 = 0x02;

/// True for a codec this plane carries.
pub const fn is_bitstream(codec: u8) -> bool {
    matches!(codec, AUDIO_CODEC_AC3 | AUDIO_CODEC_EAC3)
}

/// Blocks in one access unit. AC-3 (and any codec but E-AC-3) is 6. E-AC-3 takes the nearest of
/// 1, 2, 3 and 6 to `frame_us` at 5 333⅓ µs a block; `0` reads as 3, the host default.
/// `audio_frame_us` is a label that rounds: a receiver sizes from samples, never from it.
pub const fn blocks_for(codec: u8, frame_us: u32) -> u32 {
    if codec != AUDIO_CODEC_EAC3 {
        return 6;
    }
    // Thirds of a microsecond: a block is exactly 16 000. Each bound is the midpoint.
    match frame_us as u64 * 3 {
        0 => 3,
        1..=24_000 => 1,
        24_001..=40_000 => 2,
        40_001..=72_000 => 3,
        _ => 6,
    }
}

/// The `audio_frame_us` label for `blocks` blocks, to the nearest microsecond: 6 → 32 000,
/// 3 → 16 000, 2 → 10 667, 1 → 5 333. AC-3 is always 32 000.
pub const fn frame_us(codec: u8, blocks: u32) -> u16 {
    let blocks = if codec == AUDIO_CODEC_EAC3 { blocks } else { 6 };
    let us = (blocks as u64 * 16_000 + 1) / 3;
    if us > u16::MAX as u64 {
        u16::MAX
    } else {
        us as u16
    }
}

/// Samples per channel in one access unit labelled `frame_us`.
pub const fn frame_samples(codec: u8, frame_us: u32) -> usize {
    blocks_for(codec, frame_us) as usize * BLOCK_SAMPLES
}

/// The rate the host encodes at and the client reserves against, kbit/s. `0` is no plane:
/// AC-3 has no 7.1 and an odd count has no layout. Fixed because a passthrough sink is a
/// receiver: the rate is a quality floor, not an ABR lever. The host asserts each entry is on
/// omur's ladder.
pub const fn bitrate_kbps(codec: u8, channels: u8) -> u32 {
    match (codec, channels) {
        (AUDIO_CODEC_AC3, 2) => 192,
        (AUDIO_CODEC_AC3, 6) => 448,
        (AUDIO_CODEC_EAC3, 2) => 256,
        (AUDIO_CODEC_EAC3, 6) => 640,
        (AUDIO_CODEC_EAC3, 8) => 1_024,
        _ => 0,
    }
}

/// One-slot reassembly of `0xD4` parts into an access unit.
///
/// Every part of a unit carries its seq. A part continues the held unit only as its next index,
/// at the same seq and count; the pts is part 0's, and later parts' are not compared. An index-0
/// part opens a new unit in place of the held one; any other part is ignored and the held unit
/// stays. The buffer stays allocated across units.
#[derive(Debug, Default)]
pub struct AuAssembler {
    buf: Vec<u8>,
    /// `(seq, pts, count, parts held)` of the unit in `buf`.
    held: Option<(u32, u64, u8, u8)>,
}

impl AuAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Some((seq, pts, access unit))` when the unit's last part lands.
    pub fn push(
        &mut self,
        seq: u32,
        pts_ns: u64,
        index: u8,
        count: u8,
        bytes: &[u8],
    ) -> Option<(u32, u64, Vec<u8>)> {
        let next =
            matches!(self.held, Some((s, _, n, held)) if s == seq && n == count && held == index);
        if !next {
            if index != 0 || count == 0 {
                return None;
            }
            self.held = None;
            self.buf.clear();
            if count == 1 {
                return Some((seq, pts_ns, bytes.to_vec()));
            }
            self.held = Some((seq, pts_ns, count, 0));
        }
        self.buf.extend_from_slice(bytes);
        let (seq, pts, n, held) = self.held.as_mut()?;
        *held += 1;
        if *held < *n {
            return None;
        }
        let unit = (*seq, *pts, self.buf.clone());
        self.held = None;
        self.buf.clear();
        Some(unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM};

    #[test]
    fn only_ac3_and_eac3_ride_this_plane() {
        assert!(is_bitstream(AUDIO_CODEC_AC3) && is_bitstream(AUDIO_CODEC_EAC3));
        assert!(!is_bitstream(AUDIO_CODEC_OPUS) && !is_bitstream(AUDIO_CODEC_PCM));
        assert_eq!(AUDIO_BITSTREAM_AC3 & AUDIO_BITSTREAM_EAC3, 0);
    }

    /// Every E-AC-3 length survives label and back; a label off the table snaps to the nearest.
    #[test]
    fn a_label_round_trips_to_its_blocks() {
        for (blocks, label) in [(1, 5_333), (2, 10_667), (3, 16_000), (6, 32_000)] {
            assert_eq!(frame_us(AUDIO_CODEC_EAC3, blocks), label);
            assert_eq!(blocks_for(AUDIO_CODEC_EAC3, u32::from(label)), blocks);
            assert_eq!(
                frame_samples(AUDIO_CODEC_EAC3, u32::from(label)),
                blocks as usize * 256
            );
        }
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, 0), 3, "the host default");
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, 5_000), 1);
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, 8_001), 2);
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, 20_000), 3);
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, 24_001), 6);
        assert_eq!(blocks_for(AUDIO_CODEC_EAC3, u32::MAX), 6);
    }

    /// An AC-3 syncframe is always 6 blocks, whatever the label says.
    #[test]
    fn ac3_ignores_the_label() {
        for us in [0, 5_333, 16_000, 32_000, u32::MAX] {
            assert_eq!(blocks_for(AUDIO_CODEC_AC3, us), 6);
            assert_eq!(frame_samples(AUDIO_CODEC_AC3, us), 1_536);
        }
        assert_eq!(frame_us(AUDIO_CODEC_AC3, 1), 32_000);
    }

    #[test]
    fn each_layout_has_one_fixed_rate() {
        assert_eq!(bitrate_kbps(AUDIO_CODEC_AC3, 2), 192);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_AC3, 6), 448);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_EAC3, 2), 256);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_EAC3, 6), 640);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_EAC3, 8), 1_024);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_AC3, 8), 0, "AC-3 has no 7.1");
        assert_eq!(bitrate_kbps(AUDIO_CODEC_EAC3, 3), 0);
        assert_eq!(bitrate_kbps(AUDIO_CODEC_PCM, 2), 0);
    }

    #[test]
    fn three_parts_in_order_make_one_unit() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(10, 99, 0, 3, b"ab"), None);
        assert_eq!(a.push(10, 99, 1, 3, b"cd"), None);
        assert_eq!(
            a.push(10, 99, 2, 3, b"e"),
            Some((10, 99, b"abcde".to_vec()))
        );
        // The last seq of the space is a seq like any other.
        assert_eq!(a.push(u32::MAX, 7, 0, 2, b"x"), None);
        assert_eq!(
            a.push(u32::MAX, 7, 1, 2, b"y"),
            Some((u32::MAX, 7, b"xy".to_vec()))
        );
    }

    /// The pts is part 0's: a later part's, retimed a microsecond apart, changes nothing.
    #[test]
    fn a_later_parts_pts_is_not_compared() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(4, 1_000, 0, 2, b"a"), None);
        assert_eq!(
            a.push(4, 2_000, 1, 2, b"b"),
            Some((4, 1_000, b"ab".to_vec()))
        );
    }

    #[test]
    fn a_gap_drops_the_partial_and_index_0_starts_clean() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(10, 1, 0, 3, b"ab"), None);
        assert_eq!(a.push(10, 1, 1, 3, b"cd"), None);
        // Part 2 of unit 10 was lost; the seq moves on to unit 11.
        assert_eq!(a.push(11, 2, 0, 2, b"ef"), None);
        assert_eq!(a.push(11, 2, 1, 2, b"g"), Some((11, 2, b"efg".to_vec())));
        // A lost part 0 leaves its other parts nothing to continue.
        assert_eq!(a.push(12, 3, 1, 2, b"h"), None, "nothing held");
        assert_eq!(a.push(13, 4, 0, 2, b"i"), None);
        assert_eq!(a.push(14, 4, 1, 2, b"j"), None, "another unit's part");
        // A late part of an older unit mid-unit is ignored; the held unit still completes.
        assert_eq!(a.push(12, 3, 1, 2, b"h"), None);
        assert_eq!(a.push(13, 4, 1, 2, b"k"), Some((13, 4, b"ik".to_vec())));
    }

    #[test]
    fn a_lone_part_is_a_whole_unit() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(5, 6, 0, 1, b"ac3"), Some((5, 6, b"ac3".to_vec())));
        // It also ends a partial it cuts into.
        assert_eq!(a.push(6, 7, 0, 2, b"a"), None);
        assert_eq!(a.push(7, 8, 0, 1, b"b"), Some((7, 8, b"b".to_vec())));
        assert_eq!(a.push(6, 7, 1, 2, b"c"), None);
    }

    #[test]
    fn an_out_of_order_part_is_refused() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(10, 1, 1, 2, b"b"), None, "nothing held");
        assert_eq!(a.push(10, 1, 0, 3, b"a"), None);
        assert_eq!(a.push(10, 1, 2, 3, b"c"), None, "part 1 skipped");
        assert_eq!(a.push(10, 1, 1, 2, b"b"), None, "another count");
        assert_eq!(a.push(10, 1, 1, 3, b"b"), None, "part 2 already went by");
        // The unit that can no longer complete gives way to the next one.
        assert_eq!(a.push(11, 2, 0, 1, b"d"), Some((11, 2, b"d".to_vec())));
    }

    #[test]
    fn a_repeated_part_is_refused() {
        let mut a = AuAssembler::new();
        assert_eq!(a.push(10, 1, 0, 3, b"a"), None);
        assert_eq!(a.push(10, 1, 1, 3, b"b"), None);
        assert_eq!(a.push(10, 1, 1, 3, b"b"), None, "the repeat is ignored");
        assert_eq!(a.push(10, 1, 2, 3, b"c"), Some((10, 1, b"abc".to_vec())));
        // A repeated first part restarts the unit, so its bytes land once.
        assert_eq!(a.push(20, 2, 0, 2, b"x"), None);
        assert_eq!(a.push(20, 2, 0, 2, b"x"), None);
        assert_eq!(a.push(20, 2, 1, 2, b"y"), Some((20, 2, b"xy".to_vec())));
    }
}
