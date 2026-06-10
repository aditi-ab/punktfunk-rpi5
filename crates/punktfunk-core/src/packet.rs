//! Zero-copy wire framing: split an access unit into FEC blocks of MTU-sized shards,
//! and reassemble + FEC-recover them on the far side.
//!
//! ## Wire layout
//!
//! Each packet is a fixed [`PacketHeader`] followed by one FEC shard's payload. Fields
//! are host-endian for now (every target platform is little-endian); the `punktfunk/1` (P2)
//! spec will pin byte order explicitly when we talk to non-LE peers.
//!
//! ## GameStream mapping (P1)
//!
//! `frame_index`↔`frameIndex`, `stream_seq`↔`streamPacketIndex`,
//! (`block_index`,`block_count`)↔the `multiFecBlocks` nibbles, and
//! (`data_shards`,`recovery_shards`,`shard_index`)↔the `fecInfo` bitfield. We carry them
//! as explicit fields rather than bit-packing; full GameStream wire-exactness is an M2
//! concern (it also needs RTP framing + RTSP), this is the coherent internal format.

use crate::config::Config;
use crate::error::{PunktfunkError, Result};
use crate::fec::ErasureCoder;
use crate::session::Frame;
use crate::stats::StatsCounters;
use std::collections::{BTreeMap, HashMap, HashSet};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Identifies a punktfunk video packet (vs. an input datagram, see [`crate::input`]).
pub const PUNKTFUNK_MAGIC: u8 = 0xC9;

// Frame flags (mirroring GameStream's FLAG_*).
pub const FLAG_PIC: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;

/// Crypto framing overhead [`Session`](crate::session::Session) adds when encrypting:
/// an 8-byte sequence prefix plus the GCM tag.
pub const CRYPTO_OVERHEAD: usize = 8 + crate::crypto::TAG_LEN;

/// Largest UDP datagram the core will send or accept. `Config::validate` bounds
/// `shard_payload` so `HEADER_LEN + shard_payload + CRYPTO_OVERHEAD ≤ MAX_DATAGRAM_BYTES`.
pub const MAX_DATAGRAM_BYTES: usize = 2048;

/// How many frames behind the newest the reassembler keeps before pruning stragglers.
const REORDER_WINDOW: u32 = 16;

/// Fixed per-packet header. `#[repr(C)]`, no padding, zero-copy (de)serializable.
#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct PacketHeader {
    pub pts_ns: u64,
    pub frame_index: u32,
    pub stream_seq: u32,
    pub frame_bytes: u32,
    pub user_flags: u32,
    pub block_index: u16,
    pub block_count: u16,
    pub data_shards: u16,
    pub recovery_shards: u16,
    pub shard_index: u16,
    pub shard_bytes: u16,
    pub magic: u8,
    pub version: u8,
    pub fec_scheme: u8,
    pub flags: u8,
}

/// Size of [`PacketHeader`] on the wire (40 bytes).
pub const HEADER_LEN: usize = std::mem::size_of::<PacketHeader>();

const _: () = assert!(HEADER_LEN == 40, "PacketHeader must be 40 bytes / unpadded");

// ---------------------------------------------------------------------------
// Host side: packetization
// ---------------------------------------------------------------------------

/// Splits encoded access units into FEC-protected shard packets. Host-side only.
pub struct Packetizer {
    next_frame_index: u32,
    next_seq: u32,
    shard_payload: usize,
    fec: crate::config::FecConfig,
    version: u8,
}

impl Packetizer {
    pub fn new(config: &Config) -> Self {
        Packetizer {
            next_frame_index: 0,
            next_seq: 0,
            shard_payload: config.shard_payload,
            fec: config.fec,
            version: config.phase as u8,
        }
    }

    /// Packetize one access unit into wire packets (header + shard payload each).
    pub fn packetize(
        &mut self,
        frame: &[u8],
        pts_ns: u64,
        user_flags: u32,
        coder: &dyn ErasureCoder,
    ) -> Result<Vec<Vec<u8>>> {
        let payload = self.shard_payload;
        let frame_index = self.next_frame_index;
        self.next_frame_index = self.next_frame_index.wrapping_add(1);

        // At least one (zero-padded) data shard even for an empty frame.
        let total_data = frame.len().div_ceil(payload).max(1);
        let max_block = self.fec.max_data_per_block as usize;
        let block_count = total_data.div_ceil(max_block).max(1);
        let frame_bytes = frame.len() as u32;

        // Defend the u16 wire fields against silent truncation. `Config::validate`
        // already rejects configs that could reach these for valid frame sizes; this is
        // the belt-and-suspenders for a frame larger than the negotiated maximum.
        if payload > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg("shard_payload exceeds u16"));
        }
        if block_count > u16::MAX as usize {
            return Err(PunktfunkError::Unsupported(
                "frame too large: block count exceeds u16",
            ));
        }

        let mut packets = Vec::new();
        for b in 0..block_count {
            let first = b * max_block;
            let last = ((b + 1) * max_block).min(total_data);
            let block_data_count = last - first;

            // Build this block's data shards (each `payload` bytes, last zero-padded).
            let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(block_data_count);
            for s in first..last {
                let start = s * payload;
                let end = (start + payload).min(frame.len());
                let mut shard = vec![0u8; payload];
                if start < frame.len() {
                    shard[..end - start].copy_from_slice(&frame[start..end]);
                }
                data_shards.push(shard);
            }

            let recovery_count = self.fec.recovery_for(block_data_count);
            let recovery = coder.encode(&data_shards, recovery_count)?;
            let total_shards = block_data_count + recovery_count;
            if total_shards > u16::MAX as usize {
                return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
            }

            for shard_index in 0..total_shards {
                let body: &[u8] = if shard_index < block_data_count {
                    &data_shards[shard_index]
                } else {
                    &recovery[shard_index - block_data_count]
                };

                let seq = self.next_seq;
                self.next_seq = self.next_seq.wrapping_add(1);

                let mut flags = FLAG_PIC;
                if b == 0 && shard_index == 0 {
                    flags |= FLAG_SOF;
                }
                if b + 1 == block_count && shard_index + 1 == total_shards {
                    flags |= FLAG_EOF;
                }

                let hdr = PacketHeader {
                    pts_ns,
                    frame_index,
                    stream_seq: seq,
                    frame_bytes,
                    user_flags,
                    block_index: b as u16,
                    block_count: block_count as u16,
                    data_shards: block_data_count as u16,
                    recovery_shards: recovery_count as u16,
                    shard_index: shard_index as u16,
                    shard_bytes: payload as u16,
                    magic: PUNKTFUNK_MAGIC,
                    version: self.version,
                    fec_scheme: coder.scheme() as u8,
                    flags,
                };

                let mut pkt = Vec::with_capacity(HEADER_LEN + body.len());
                pkt.extend_from_slice(hdr.as_bytes());
                pkt.extend_from_slice(body);
                packets.push(pkt);
            }
        }
        Ok(packets)
    }
}

// ---------------------------------------------------------------------------
// Client side: reassembly + FEC recovery
// ---------------------------------------------------------------------------

struct BlockBuf {
    data_shards: usize,
    recovery_shards: usize,
    shard_bytes: usize,
    /// Length `data_shards + recovery_shards`; `Some` = received.
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    done: bool,
}

struct FrameBuf {
    frame_bytes: usize,
    block_count: usize,
    pts_ns: u64,
    user_flags: u32,
    blocks: HashMap<u16, BlockBuf>,
    /// Reconstructed payload per completed block, ordered by block index.
    block_data: BTreeMap<u16, Vec<u8>>,
}

/// Per-session bounds the reassembler enforces on every packet header *before*
/// allocating, so a hostile or corrupt header cannot drive unbounded memory use. All
/// derived from the negotiated [`Config`].
#[derive(Clone, Copy, Debug)]
pub struct ReassemblerLimits {
    /// Expected shard payload length; every shard in the stream must match exactly.
    pub shard_bytes: usize,
    /// Max data shards per block (the negotiated `max_data_per_block`).
    pub max_data_shards: usize,
    /// Max total shards per block (data + recovery), capped by the FEC scheme ceiling.
    pub max_total_shards: usize,
    /// Max FEC blocks per frame.
    pub max_blocks: usize,
    /// Max accepted access-unit size.
    pub max_frame_bytes: usize,
}

impl ReassemblerLimits {
    pub fn from_config(c: &Config) -> Self {
        let max_data = c.fec.max_data_per_block as usize;
        let max_total =
            (max_data + c.fec.recovery_for(max_data)).min(c.fec.scheme.max_total_shards());
        let total_data = c.max_frame_bytes.div_ceil(c.shard_payload.max(1)).max(1);
        ReassemblerLimits {
            shard_bytes: c.shard_payload,
            max_data_shards: max_data,
            max_total_shards: max_total,
            max_blocks: total_data.div_ceil(max_data).max(1),
            max_frame_bytes: c.max_frame_bytes,
        }
    }
}

/// Buffers incoming shards, recovers lost ones via FEC, and emits whole access units.
/// Client-side only.
pub struct Reassembler {
    limits: ReassemblerLimits,
    frames: HashMap<u32, FrameBuf>,
    /// Recently-emitted frames, so stray/late shards can't resurrect them. Pruned to
    /// the reorder window alongside `frames`.
    completed: HashSet<u32>,
    newest_frame: Option<u32>,
}

impl Reassembler {
    pub fn new(limits: ReassemblerLimits) -> Self {
        Reassembler {
            limits,
            frames: HashMap::new(),
            completed: HashSet::new(),
            newest_frame: None,
        }
    }

    /// Ingest one (already-decrypted) packet. Returns the access unit when its last
    /// block completes, otherwise `None`.
    pub fn push(
        &mut self,
        pkt: &[u8],
        coder: &dyn ErasureCoder,
        stats: &StatsCounters,
    ) -> Result<Option<Frame>> {
        // On a lossy datagram link a malformed or non-video packet is dropped, never
        // fatal: it must not abort `poll_frame`. Only a genuine FEC reconstruction
        // failure propagates as an error.
        if pkt.len() < HEADER_LEN {
            StatsCounters::add(&stats.packets_dropped, 1);
            return Ok(None);
        }
        let hdr = match PacketHeader::read_from_bytes(&pkt[..HEADER_LEN]) {
            Ok(h) => h,
            Err(_) => {
                StatsCounters::add(&stats.packets_dropped, 1);
                return Ok(None);
            }
        };

        let lim = self.limits;
        let shard_bytes = hdr.shard_bytes as usize;
        let data_shards = hdr.data_shards as usize;
        let recovery_shards = hdr.recovery_shards as usize;
        let total = data_shards + recovery_shards;
        let shard_index = hdr.shard_index as usize;
        let block_count = hdr.block_count as usize;
        let frame_bytes = hdr.frame_bytes as usize;

        // Bound every attacker-controllable header field against the negotiated limits
        // BEFORE allocating anything keyed on it — this is the firewall against a tiny
        // datagram triggering a huge `vec![None; total]` / `Vec::with_capacity`.
        let drop = |stats: &StatsCounters| {
            StatsCounters::add(&stats.packets_dropped, 1);
        };
        if hdr.magic != PUNKTFUNK_MAGIC
            || shard_bytes != lim.shard_bytes
            || pkt.len() < HEADER_LEN + shard_bytes
            || data_shards == 0
            || data_shards > lim.max_data_shards
            || total == 0
            || total > lim.max_total_shards
            || shard_index >= total
            || block_count == 0
            || block_count > lim.max_blocks
            || hdr.block_index as usize >= block_count
            || frame_bytes > lim.max_frame_bytes
        {
            drop(stats);
            return Ok(None);
        }
        let payload = pkt[HEADER_LEN..HEADER_LEN + shard_bytes].to_vec();

        self.advance_window(hdr.frame_index, stats);

        // Drop shards for frames we've already emitted (e.g. the recovery shards of a
        // frame that completed early via the all-originals-present fast path) or that
        // have fallen out of the reorder window.
        if self.completed.contains(&hdr.frame_index) || self.is_stale(hdr.frame_index) {
            drop(stats);
            return Ok(None);
        }

        // First packet of a frame establishes its geometry; later packets must agree.
        let frame = self
            .frames
            .entry(hdr.frame_index)
            .or_insert_with(|| FrameBuf {
                frame_bytes,
                block_count,
                pts_ns: hdr.pts_ns,
                user_flags: hdr.user_flags,
                blocks: HashMap::new(),
                block_data: BTreeMap::new(),
            });
        if frame.block_count != block_count || frame.frame_bytes != frame_bytes {
            drop(stats);
            return Ok(None);
        }

        if frame.block_data.contains_key(&hdr.block_index) {
            return Ok(None); // block already reconstructed; late/duplicate shard
        }

        // First packet of a block sizes its shard vector; later packets must match its
        // (data, recovery, shard_bytes) geometry, so `shard_index` is always in bounds.
        frame
            .blocks
            .entry(hdr.block_index)
            .or_insert_with(|| BlockBuf {
                data_shards,
                recovery_shards,
                shard_bytes,
                shards: vec![None; total],
                received: 0,
                done: false,
            });
        let block = frame.blocks.get_mut(&hdr.block_index).unwrap();
        if block.data_shards != data_shards
            || block.recovery_shards != recovery_shards
            || block.shard_bytes != shard_bytes
        {
            drop(stats);
            return Ok(None);
        }

        if block.shards[shard_index].is_none() {
            block.shards[shard_index] = Some(payload);
            block.received += 1;
        }

        // Reconstruct as soon as we hold enough shards.
        if !block.done && block.received >= block.data_shards {
            let present_data = block.shards[..block.data_shards]
                .iter()
                .filter(|s| s.is_some())
                .count();
            let recovered =
                coder.reconstruct(block.data_shards, block.recovery_shards, &mut block.shards)?;
            block.done = true;
            StatsCounters::add(
                &stats.fec_recovered_shards,
                (block.data_shards - present_data) as u64,
            );

            // Concatenate the block's data shards into its contiguous payload.
            let mut block_payload = Vec::with_capacity(block.data_shards * block.shard_bytes);
            for shard in &recovered {
                block_payload.extend_from_slice(shard);
            }
            frame.block_data.insert(hdr.block_index, block_payload);
            frame.blocks.remove(&hdr.block_index);
        }

        // Whole frame ready?
        if frame.block_data.len() == frame.block_count {
            let frame = self.frames.remove(&hdr.frame_index).unwrap();
            self.completed.insert(hdr.frame_index);
            // Reserve based on the bytes we actually hold, not the (already-bounded but
            // still caller-supplied) frame_bytes, so a small frame can't over-reserve.
            let actual: usize = frame.block_data.values().map(|b| b.len()).sum();
            let mut data = Vec::with_capacity(actual);
            for (_, block_payload) in frame.block_data.into_iter() {
                data.extend_from_slice(&block_payload);
            }
            data.truncate(frame.frame_bytes); // trim trailing-shard zero padding
            return Ok(Some(Frame {
                data,
                frame_index: hdr.frame_index,
                pts_ns: frame.pts_ns,
                flags: frame.user_flags,
            }));
        }
        Ok(None)
    }

    /// Track the newest frame and prune stragglers that fell out of the reorder window
    /// (counting them as dropped).
    fn advance_window(&mut self, frame_index: u32, stats: &StatsCounters) {
        let newest = match self.newest_frame {
            // `frame_index` is newer iff it's within the forward half of the index space.
            Some(n) if frame_index.wrapping_sub(n) > u32::MAX / 2 => n,
            _ => frame_index,
        };
        self.newest_frame = Some(newest);

        let before = self.frames.len();
        self.frames
            .retain(|&idx, _| newest.wrapping_sub(idx) <= REORDER_WINDOW);
        let pruned = before - self.frames.len();
        if pruned > 0 {
            StatsCounters::add(&stats.frames_dropped, pruned as u64);
        }
        self.completed
            .retain(|&idx| newest.wrapping_sub(idx) <= REORDER_WINDOW);
    }

    /// True if `frame_index` lies behind the newest frame by more than the reorder
    /// window (so its shards arrive too late to be useful).
    fn is_stale(&self, frame_index: u32) -> bool {
        match self.newest_frame {
            Some(n) => {
                let behind = n.wrapping_sub(frame_index);
                behind > REORDER_WINDOW && behind <= u32::MAX / 2
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FecScheme;
    use crate::fec::coder_for;

    fn limits() -> ReassemblerLimits {
        ReassemblerLimits {
            shard_bytes: 16,
            max_data_shards: 8,
            max_total_shards: 12,
            max_blocks: 4,
            max_frame_bytes: 4096,
        }
    }

    fn base_header() -> PacketHeader {
        PacketHeader {
            pts_ns: 0,
            frame_index: 0,
            stream_seq: 0,
            frame_bytes: 16,
            user_flags: 0,
            block_index: 0,
            block_count: 1,
            data_shards: 1,
            recovery_shards: 0,
            shard_index: 0,
            shard_bytes: 16,
            magic: PUNKTFUNK_MAGIC,
            version: 1,
            fec_scheme: 0,
            flags: FLAG_PIC,
        }
    }

    fn packet(h: PacketHeader) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(&vec![0xAB; h.shard_bytes as usize]);
        p
    }

    /// A header advertising 65535+65535 shards must be dropped, not allocate gigabytes.
    #[test]
    fn rejects_oversized_shard_counts() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();
        let mut h = base_header();
        h.data_shards = 65535;
        h.recovery_shards = 65535;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(stats.snapshot().packets_dropped, 1);
    }

    /// A second packet for a block whose geometry differs from the first must be dropped
    /// — never index past the block's allocated shard vector (the old OOB panic).
    #[test]
    fn rejects_inconsistent_block_geometry_without_panicking() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();

        let mut h1 = base_header();
        h1.data_shards = 4;
        h1.recovery_shards = 2; // block sized to 6 slots
        h1.frame_bytes = 64;
        assert!(r
            .push(&packet(h1), coder.as_ref(), &stats)
            .unwrap()
            .is_none());

        // Same block, different geometry, shard_index valid for ITS total (8) but past
        // the established block's 6 slots.
        let mut h2 = base_header();
        h2.data_shards = 6;
        h2.recovery_shards = 2;
        h2.shard_index = 7;
        h2.frame_bytes = 64;
        assert!(r
            .push(&packet(h2), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(stats.snapshot().packets_dropped, 1);
    }

    #[test]
    fn rejects_wrong_shard_bytes_and_oversized_frame() {
        let coder = coder_for(FecScheme::Gf8);

        let mut r = Reassembler::new(limits());
        let stats = StatsCounters::default();
        let mut h = base_header();
        h.shard_bytes = 8; // != negotiated 16
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(stats.snapshot().packets_dropped, 1);

        let mut r = Reassembler::new(limits());
        let stats = StatsCounters::default();
        let mut h = base_header();
        h.frame_bytes = 1_000_000; // > max_frame_bytes
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(stats.snapshot().packets_dropped, 1);
    }
}
