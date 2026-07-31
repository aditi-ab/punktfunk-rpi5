//! Wire packet header: the fixed [`PacketHeader`] + the flag/geometry consts every
//! packet carries. Zero-copy (de)serializable; 40 bytes, unpadded.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Identifies a punktfunk video packet (vs. an input datagram, see [`crate::input`]).
pub const PUNKTFUNK_MAGIC: u8 = 0xC9;

// Frame flags (mirroring GameStream's FLAG_*).
pub const FLAG_PIC: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;
/// Bandwidth-probe filler, not decodable video: a [`crate::quic::ProbeRequest`] speed test makes
/// the host burst access units carrying this flag so the client measures throughput/loss without
/// feeding them to the decoder. Punktfunk/1 only (GameStream never sets it).
pub const FLAG_PROBE: u8 = 0x8;

/// Application `user_flags` bit (the u32 [`PacketHeader::user_flags`] word, surfaced to the client
/// as [`crate::session::Frame::flags`]) — NOT a transport packet flag. Marks the access unit that
/// **completes an intra-refresh wave**: the picture is loss-free from here even though the frame is
/// a coded `P` (no IDR, so the decoder never sets `AV_FRAME_FLAG_KEY`). The client lifts its
/// post-loss display freeze on this bit as well as on a real keyframe — the only bitstream-invisible
/// clean point it can honor without forcing a full IDR. Lives above the low nibble because the host
/// reuses `FLAG_PIC`/`FLAG_SOF`/`FLAG_PROBE` bit values inside `user_flags`; `0x10` clears all four.
pub const USER_FLAG_RECOVERY_POINT: u32 = 0x10;

/// Application `user_flags` bit — a **definitive single-frame clean re-anchor**. Unlike
/// [`USER_FLAG_RECOVERY_POINT`] (an intra-refresh wave boundary, where the first boundary after a loss
/// is only half-healed so the client waits for the second), this marks an access unit the host coded
/// to reference a **known-good** picture on purpose — an AMD **LTR reference-frame-invalidation**
/// recovery frame (`ForceLTRReferenceBitfield`): a clean P-frame off a long-term reference the client
/// already has, not an IDR. The picture is loss-free the instant this AU decodes, so the client lifts
/// its post-loss freeze on the **first** such mark. Coded `P` (no IDR), so the decoder never sets
/// `AV_FRAME_FLAG_KEY` — this host flag is the only signal.
pub const USER_FLAG_RECOVERY_ANCHOR: u32 = 0x20;

/// `user_flags` bit: the AU's content is **shard-aligned self-delimiting chunks** — every
/// `shard_payload`-sized window of the frame buffer starts a fresh codec packet, padded to the
/// window with zeros (PyroWave datagram-aligned mode, design/pyrowave-codec-plan.md §4.4). Two
/// consequences: a receiver that opted into partial delivery can use an aged-out frame's buffer
/// AS-IS (missing shards stay zeroed; the codec's block walk skips zero windows), and even a
/// COMPLETE frame must be consumed window-by-window (the padding is not part of the stream).
pub const USER_FLAG_CHUNK_ALIGNED: u32 = 0x40;

/// `user_flags` bit: this AU was packetized as a **slice-streamed** frame (the P2 slice
/// pipeline): its sentinel blocks (`block_count == 0`) are SLICE-granularity and carry their
/// shard-aligned BASE byte offset in `frame_bytes` (the legacy fixed-geometry sentinel is the
/// degenerate base-0 case), and its FINAL block's base derives from the totals as
/// `(total_data_shards − final_data_shards) × shard_bytes` — variable-size blocks tile the
/// frame in shard units, so the uniform-geometry offset formula does not apply to ANY of its
/// blocks. On every packet of the AU (not just sentinels) because reorder can deliver the final
/// block first and its placement rule differs. Only emitted toward peers advertising
/// [`VIDEO_CAP_STREAMED_AU`](crate::quic::VIDEO_CAP_STREAMED_AU) ∧
/// [`VIDEO_CAP_MULTI_SLICE`](crate::quic::VIDEO_CAP_MULTI_SLICE) — the pair whose receivers
/// know this contract.
pub const USER_FLAG_SLICE_STREAM: u32 = 0x80;

/// Widest lost-frame range (frames, wrapping `last - first`) a reference-frame-invalidation
/// recovery may be asked to repair; anything wider goes straight to the keyframe path on BOTH
/// ends. RFI can only re-reference history the encoder still holds — NVENC keeps a 5-frame DPB,
/// AMD LTR ~1 s of marks — and a genuine loss this wide (>1 s even at 240 fps) has no valid
/// reference anywhere, so an RFI request for it is either hopeless or (worse) a phantom range
/// from a desynced counter. Shared by the host's RFI dispatch (range → keyframe fallback) and the
/// client-side gap detectors (huge gap → resync + keyframe request, no RFI).
pub const RFI_MAX_RANGE: u32 = 256;

/// Crypto framing overhead [`Session`](crate::session::Session) adds when encrypting:
/// an 8-byte sequence prefix plus the GCM tag.
pub const CRYPTO_OVERHEAD: usize = 8 + crate::crypto::TAG_LEN;

/// Largest UDP datagram the core will send or accept. `Config::validate` bounds
/// `shard_payload` so `HEADER_LEN + shard_payload + CRYPTO_OVERHEAD ≤ MAX_DATAGRAM_BYTES`.
pub const MAX_DATAGRAM_BYTES: usize = 2048;

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
