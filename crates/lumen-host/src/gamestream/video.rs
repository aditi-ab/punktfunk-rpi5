//! GameStream video wire packetization: an encoded access unit → UDP datagrams a stock
//! Moonlight client decodes. Each datagram is
//!   `RTP_PACKET(12, big-endian) + reserved[4] + NV_VIDEO_PACKET(16, little-endian) + payload`
//! and the frame's bitstream is prefixed with an 8-byte `video_short_frame_header_t`, then
//! striped into ≤4 FEC blocks of ≤255 data shards. Byte-exact spec:
//! `docs/research/gamestream-protocol-research.json` (video plane).
//!
//! P1.3 sends **data shards only** (`fecPercentage = 0`): on a clean LAN the client has
//! every data shard and never runs Reed–Solomon recovery, so we get a decodable frame
//! without matching Moonlight's `nanors` parity matrix (that interop work is P1.5). Plaintext
//! only (encryption negotiated off for now). This lives in lumen-host for fast iteration;
//! the wire codec moves into lumen-core (the P1 wire mode) once proven.

/// RTP `header` byte: version 2 (0x80) | extension (0x10) — Moonlight keys on the extension.
const RTP_HEADER_BYTE: u8 = 0x80 | 0x10;
const FLAG_PIC: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const MULTI_FEC_FLAGS: u8 = 0x10;
const MAX_DATA_SHARDS_PER_BLOCK: usize = 255;
const MAX_FEC_BLOCKS: usize = 4;
/// Per-shard header: RTP(12) + reserved(4) + NV_VIDEO_PACKET(16).
const SHARD_HEADER: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Idr,
    P,
}

/// Splits encoded access units into GameStream video datagrams.
pub struct VideoPacketizer {
    /// Negotiated `packetSize` (ANNOUNCE `x-nv-video[0].packetSize`).
    packet_size: usize,
    /// Per-shard payload bytes = `blocksize - SHARD_HEADER`, `blocksize = packetSize + 16`.
    payload_per_shard: usize,
    frame_index: u32,
    /// Monotonic per-stream packet counter (the RTP sequence / streamPacketIndex source).
    seq: u32,
}

impl VideoPacketizer {
    pub fn new(packet_size: usize) -> Self {
        VideoPacketizer {
            packet_size,
            payload_per_shard: packet_size + 16 - SHARD_HEADER,
            frame_index: 0,
            seq: 0,
        }
    }

    /// Packetize one encoded AU into wire datagrams (ready for UDP send).
    pub fn packetize(
        &mut self,
        au: &[u8],
        frame_type: FrameType,
        timestamp_90k: u32,
    ) -> Vec<Vec<u8>> {
        let frame_index = self.frame_index;
        self.frame_index = self.frame_index.wrapping_add(1);
        let pps = self.payload_per_shard;

        // frame payload = 8-byte short frame header + the AU bitstream.
        let total_len = 8 + au.len();
        let last_payload_len = match total_len % pps {
            0 => pps,
            r => r,
        };
        let mut fp = Vec::with_capacity(total_len);
        fp.extend_from_slice(&short_frame_header(frame_type, last_payload_len as u16));
        fp.extend_from_slice(au);

        let total_data = total_len.div_ceil(pps).max(1);
        let n_blocks = total_data
            .div_ceil(MAX_DATA_SHARDS_PER_BLOCK)
            .clamp(1, MAX_FEC_BLOCKS);
        let per_block = total_data.div_ceil(n_blocks);

        let mut packets = Vec::with_capacity(total_data);
        for b in 0..n_blocks {
            let first = b * per_block;
            let last = ((b + 1) * per_block).min(total_data);
            if first >= last {
                break;
            }
            let block_data_count = last - first;
            for (fec_index, shard) in (first..last).enumerate() {
                let start = shard * pps;
                let end = (start + pps).min(fp.len());
                let mut payload = vec![0u8; pps]; // last shard zero-padded
                payload[..end - start].copy_from_slice(&fp[start..end]);

                let mut flags = FLAG_PIC;
                if shard == 0 {
                    flags |= FLAG_SOF;
                }
                if shard == total_data - 1 {
                    flags |= FLAG_EOF;
                }
                let multi_fec_blocks = ((b as u8) << 4) | (((n_blocks - 1) as u8) << 6);
                // fecInfo: dataShards<<22 | fecIndex<<12 | fecPercentage<<4 (pct = 0).
                let fec_info: u32 = ((block_data_count as u32) << 22) | ((fec_index as u32) << 12);
                let seq = self.seq;
                self.seq = self.seq.wrapping_add(1);

                packets.push(build_packet(
                    seq,
                    timestamp_90k,
                    frame_index,
                    flags,
                    multi_fec_blocks,
                    fec_info,
                    &payload,
                ));
            }
        }
        packets
    }
}

/// 8-byte `video_short_frame_header_t` (little-endian), prefixed to the AU bitstream.
fn short_frame_header(frame_type: FrameType, last_payload_len: u16) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0] = 0x01; // headerType
    h[1..3].copy_from_slice(&0u16.to_le_bytes()); // frame_processing_latency
    h[3] = match frame_type {
        FrameType::Idr => 2,
        FrameType::P => 1,
    };
    h[4..6].copy_from_slice(&last_payload_len.to_le_bytes());
    // h[6..8] unknown = 0
    h
}

/// Build one wire datagram: RTP(BE) + reserved + NV_VIDEO_PACKET(LE) + payload.
fn build_packet(
    seq: u32,
    timestamp_90k: u32,
    frame_index: u32,
    flags: u8,
    multi_fec_blocks: u8,
    fec_info: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(SHARD_HEADER + payload.len());
    // --- RTP_PACKET (12 bytes, big-endian) ---
    p.push(RTP_HEADER_BYTE); // header
    p.push(0); // packetType (unused for video)
    p.extend_from_slice(&(seq as u16).to_be_bytes()); // sequenceNumber
    p.extend_from_slice(&timestamp_90k.to_be_bytes()); // timestamp (90 kHz)
    p.extend_from_slice(&0u32.to_be_bytes()); // ssrc
                                              // --- reserved[4] ---
    p.extend_from_slice(&[0u8; 4]);
    // --- NV_VIDEO_PACKET (16 bytes, little-endian) ---
    p.extend_from_slice(&(seq << 8).to_le_bytes()); // streamPacketIndex (low byte 0)
    p.extend_from_slice(&frame_index.to_le_bytes()); // frameIndex
    p.push(flags);
    p.push(0); // extraFlags
    p.push(MULTI_FEC_FLAGS);
    p.push(multi_fec_blocks);
    p.extend_from_slice(&fec_info.to_le_bytes()); // fecInfo
                                                  // --- payload ---
    p.extend_from_slice(payload);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_block_layout() {
        let mut pk = VideoPacketizer::new(1392); // payload_per_shard = 1392+16-32 = 1376
        assert_eq!(pk.payload_per_shard, 1376);
        let au = vec![0xABu8; 4000]; // 8+4000 = 4008 → ceil(4008/1376) = 3 data shards
        let pkts = pk.packetize(&au, FrameType::Idr, 90_000);
        assert_eq!(pkts.len(), 3);
        // Every datagram is SHARD_HEADER + payload_per_shard.
        for p in &pkts {
            assert_eq!(p.len(), SHARD_HEADER + 1376);
            assert_eq!(p[0], 0x90); // RTP header byte
        }
        // First packet: SOF set, fecIndex 0, frameIndex 0.
        let first = &pkts[0];
        assert_eq!(first[24] & FLAG_SOF, FLAG_SOF);
        assert_eq!(first[24] & FLAG_PIC, FLAG_PIC);
        let frame_index = u32::from_le_bytes(first[20..24].try_into().unwrap());
        assert_eq!(frame_index, 0);
        let fec_info = u32::from_le_bytes(first[28..32].try_into().unwrap());
        assert_eq!(fec_info >> 22, 3); // dataShards = 3
        assert_eq!((fec_info >> 12) & 0x3ff, 0); // fecIndex 0
                                                 // Last packet: EOF set, fecIndex 2.
        let last = &pkts[2];
        assert_eq!(last[24] & FLAG_EOF, FLAG_EOF);
        let fec_info_last = u32::from_le_bytes(last[28..32].try_into().unwrap());
        assert_eq!((fec_info_last >> 12) & 0x3ff, 2);
        // RTP sequence numbers are 0,1,2.
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(u16::from_be_bytes(p[2..4].try_into().unwrap()), i as u16);
        }
    }

    #[test]
    fn multi_block_split() {
        let mut pk = VideoPacketizer::new(1392);
        // Need > 255 data shards → multi-block. 255*1376 ≈ 351 KB; use 600 KB.
        let au = vec![0u8; 600_000];
        let pkts = pk.packetize(&au, FrameType::P, 0);
        let total = (8 + au.len()).div_ceil(1376);
        assert_eq!(pkts.len(), total);
        // n_blocks = ceil(total/255), clamped to 4; check multiFecBlocks lastBlock nibble.
        let n_blocks = total.div_ceil(255).clamp(1, 4);
        let last_block = ((pkts.last().unwrap()[27]) >> 6) & 0x3;
        assert_eq!(last_block as usize, n_blocks - 1);
    }
}
