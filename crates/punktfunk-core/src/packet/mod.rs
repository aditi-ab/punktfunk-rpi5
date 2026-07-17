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
//! as explicit fields rather than bit-packing; full GameStream wire-exactness is a GameStream-host
//! concern (it also needs RTP framing + RTSP), this is the coherent internal format.

mod header;
mod packetize;
mod reassemble;

pub use header::*;
pub use packetize::*;
pub use reassemble::*;

#[cfg(test)]
mod tests;
