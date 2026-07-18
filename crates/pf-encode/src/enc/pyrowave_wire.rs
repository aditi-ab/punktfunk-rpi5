//! Shared PyroWave AU wire-framing (design/pyrowave-codec-plan.md §4.4) — the single source of
//! truth for the on-wire access-unit shape, used by BOTH the Linux (dmabuf/CSC) and Windows (NV12
//! zero-copy) host encoders. It turns pyrowave's packetized bitstream into either the **dense**
//! single-packet AU or the **datagram-aligned** windowed AU. Pure (no GPU/FFI) so it is unit-tested
//! on any platform and both encoders emit byte-identical framing — the clients parse this exact
//! layout, so it must stay in ONE place.
//!
//! Datagram-aligned AU: each `chunk`-sized window opens with a 4-byte prefix (`u16` used-length +
//! `u16` kind) and carries either WHOLE self-delimiting codec packets (`WIN_PACKED` — several small
//! ones share a window) or one fragment of an oversized ATOMIC packet (a `FRAG` chain — pyrowave's
//! 32×32 blocks are atomic and can exceed a shard). A lost shard zeroes its window (`used = 0`) so
//! the receiver skips it and drops any fragment chain it interrupts. Padding after `used` is zeroed.

/// The 4-byte per-window framing prefix (`u16` used-length + `u16` kind).
pub(crate) const WINDOW_PREFIX: usize = 4;
/// Window kinds: whole packets / an oversized packet's fragments.
const WIN_PACKED: u16 = 0;
const WIN_FRAG_FIRST: u16 = 1;
const WIN_FRAG_CONT: u16 = 2;
const WIN_FRAG_LAST: u16 = 3;

/// The packetize boundary to request from pyrowave: for a `wire_chunk` shard it is the shard payload
/// minus the 4-byte window prefix (so a whole codec packet + its prefix fits one shard); for the
/// dense case it is the whole-bitstream cap (one packet per AU).
pub(crate) fn packet_boundary(wire_chunk: Option<usize>, dense_cap: usize) -> usize {
    wire_chunk.map(|c| c - WINDOW_PREFIX).unwrap_or(dense_cap)
}

/// Patch the frame's `BitstreamSequenceHeader` to signal `ycbcr_range = LIMITED`. pyrowave's C API
/// fills the header with `= {}` (all VUI fields zeroed) and offers NO way to set colour/range, so it
/// signals `ycbcr_range = 0 = YCBCR_RANGE_FULL` — but BOTH host CSCs (`rgb2yuv.comp` on Linux, the
/// D3D11 `BgraToYuvPlanes` on Windows) always emit BT.709 **LIMITED** Y′CbCr (black = Y′16). A client
/// that honours the VUI (the Apple wavelet decoder reads `(word1 >> 30) & 1`) then skips the
/// limited→full expansion and shows washed-out, raised blacks. Patching the bit makes the bitstream
/// HONEST for every client — clients that hardcode limited (the Vulkan `video_pyrowave` path) are
/// unaffected, and pyrowave's own decode ignores the flag (it reconstructs raw Y′CbCr). The other
/// zeroed VUI fields (BT.709 primaries / transform / transfer) are already correct.
///
/// `seq_offset` is the byte offset of the frame's 8-byte `BitstreamSequenceHeader` in `bitstream` —
/// the SOF packet's offset. `ycbcr_range` is bit 30 of the little-endian second word, i.e. bit 6 of
/// byte `seq_offset + 7` (`0x40`).
pub(crate) fn mark_limited_range(bitstream: &mut [u8], seq_offset: usize) {
    if let Some(b) = bitstream.get_mut(seq_offset + 7) {
        *b |= 0x40;
    }
}

/// Frame pyrowave's `packets` (each an `(offset, size)` into `bitstream`) into the wire AU.
/// `wire_chunk = None` copies the single dense packet; `Some(chunk)` produces the windowed
/// datagram-aligned AU (a whole number of `chunk`-sized windows).
pub(crate) fn build_au(
    packets: &[(usize, usize)],
    bitstream: &[u8],
    wire_chunk: Option<usize>,
) -> Vec<u8> {
    let Some(chunk) = wire_chunk else {
        // Dense (default): boundary == whole buffer → the AU is exactly one pyrowave packet.
        let (off, size) = packets[0];
        return bitstream[off..off + size].to_vec();
    };
    let payload_max = chunk - WINDOW_PREFIX;
    let mut au: Vec<u8> = Vec::with_capacity((packets.len() + 1) * chunk);
    // The currently-open PACKED window: (start offset of its prefix, bytes used).
    let mut open: Option<(usize, usize)> = None;
    let close = |au: &mut Vec<u8>, open: &mut Option<(usize, usize)>, chunk: usize| {
        if let Some((start, used)) = open.take() {
            au[start..start + 2].copy_from_slice(&(used as u16).to_le_bytes());
            au[start + 2..start + 4].copy_from_slice(&WIN_PACKED.to_le_bytes());
            au.resize(start + chunk, 0);
        }
    };
    for &(off, size) in packets {
        let bytes = &bitstream[off..off + size];
        if size <= payload_max {
            let fits = open.is_some_and(|(_, used)| used + size <= payload_max);
            if !fits {
                close(&mut au, &mut open, chunk);
                let start = au.len();
                au.resize(start + WINDOW_PREFIX, 0);
                open = Some((start, 0));
            }
            au.extend_from_slice(bytes);
            if let Some((_, used)) = open.as_mut() {
                *used += size;
            }
        } else {
            // Oversized packet: its own FRAG chain of full windows.
            close(&mut au, &mut open, chunk);
            let mut o = 0usize;
            while o < size {
                let take = (size - o).min(payload_max);
                let kind = if o == 0 {
                    WIN_FRAG_FIRST
                } else if o + take == size {
                    WIN_FRAG_LAST
                } else {
                    WIN_FRAG_CONT
                };
                let start = au.len();
                au.resize(start + WINDOW_PREFIX, 0);
                au[start..start + 2].copy_from_slice(&(take as u16).to_le_bytes());
                au[start + 2..start + 4].copy_from_slice(&kind.to_le_bytes());
                au.extend_from_slice(&bytes[o..o + take]);
                au.resize(start + chunk, 0);
                o += take;
            }
        }
    }
    close(&mut au, &mut open, chunk);
    au
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a windowed AU back into the flat codec-packet stream (the client's parse), asserting the
    /// framing invariants the encoder promises: whole windows, in-bounds `used`, zeroed padding.
    fn walk(au: &[u8], chunk: usize) -> Vec<u8> {
        assert_eq!(au.len() % chunk, 0, "AU is a whole number of windows");
        let mut out = Vec::new();
        let mut frag: Vec<u8> = Vec::new();
        for win in au.chunks(chunk) {
            let used = u16::from_le_bytes([win[0], win[1]]) as usize;
            let kind = u16::from_le_bytes([win[2], win[3]]);
            assert!(WINDOW_PREFIX + used <= win.len(), "window overrun");
            assert!(
                win[WINDOW_PREFIX + used..].iter().all(|&b| b == 0),
                "non-zero padding after used"
            );
            let body = &win[WINDOW_PREFIX..WINDOW_PREFIX + used];
            match kind {
                0 => out.extend_from_slice(body),
                1 => frag = body.to_vec(),
                2 => frag.extend_from_slice(body),
                3 => {
                    frag.extend_from_slice(body);
                    out.extend_from_slice(&frag);
                    frag.clear();
                }
                k => panic!("unknown window kind {k}"),
            }
        }
        out
    }

    #[test]
    fn dense_is_the_single_packet() {
        let bs = (0u8..=200).collect::<Vec<u8>>();
        let au = build_au(&[(10, 50)], &bs, None);
        assert_eq!(au, bs[10..60]);
    }

    #[test]
    fn packed_windows_pack_small_packets_and_reconstruct() {
        // Three small packets that share windows; walking must reproduce them concatenated in order.
        let bs: Vec<u8> = (0..255u32).map(|i| i as u8).collect();
        let packets = [(0, 20), (20, 20), (40, 100)];
        let chunk = 64; // payload_max = 60
        let au = build_au(&packets, &bs, Some(chunk));
        let flat = walk(&au, chunk);
        let mut expect = Vec::new();
        for &(o, s) in &packets {
            expect.extend_from_slice(&bs[o..o + s]);
        }
        assert_eq!(flat, expect);
    }

    #[test]
    fn oversized_packet_fragments_and_reassembles() {
        // One atomic packet larger than a window → a FRAG chain the walk reassembles exactly.
        let bs: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let chunk = 64; // payload_max = 60
        let au = build_au(&[(0, 500)], &bs, Some(chunk));
        assert_eq!(walk(&au, chunk), bs[0..500]);
    }

    #[test]
    fn boundary_reserves_the_window_prefix() {
        assert_eq!(packet_boundary(Some(1408), 999_999), 1404);
        assert_eq!(packet_boundary(None, 777), 777);
    }

    #[test]
    fn mark_limited_range_sets_only_the_range_bit() {
        let mut bs = vec![0u8; 16];
        mark_limited_range(&mut bs, 0);
        // ycbcr_range = bit 30 of the LE second word = bit 6 of byte 7 (0x40); nothing else touched.
        assert_eq!(bs[7], 0x40);
        assert!(bs[..7].iter().all(|&b| b == 0));
        assert!(bs[8..].iter().all(|&b| b == 0));
        // Idempotent; an out-of-range offset is a silent no-op (never panics).
        mark_limited_range(&mut bs, 0);
        assert_eq!(bs[7], 0x40);
        mark_limited_range(&mut bs, 100);
    }
}
