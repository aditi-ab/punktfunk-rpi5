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

use crate::config::Config;
use crate::error::{PunktfunkError, Result};
use crate::fec::ErasureCoder;
use crate::session::Frame;
use crate::stats::StatsCounters;
use std::collections::HashMap;
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

/// How far behind the newest frame's capture pts an INCOMPLETE frame may sit before it is
/// declared lost (counted in `frames_dropped`, which triggers the client's recovery-keyframe
/// request). TIME-based, not frame-count-based, so the fuse is the same at every refresh rate: a
/// fixed index window is refresh-relative (4 frames = 66 ms at 60 fps but only 33 ms at 120 fps —
/// inside normal Wi-Fi retry/block-ack reorder timescales, where a delayed-not-lost shard can
/// trail newer frames). Observed live at 120 fps: the too-tight fuse declared merely-late frames
/// dead every few seconds, and each false loss cost a recovery-IDR burst + an inflated loss report
/// (FEC churn) — a self-sustaining latency/bitrate oscillation. 120 ms rides safely above radio
/// retry jitter while still detecting a real loss ~2× faster than the original 16-frame window did
/// at 60 fps.
const LOSS_WINDOW_NS: u64 = 120_000_000;

/// Hard cap on how many frame INDICES behind the newest an incomplete frame may sit, whatever its
/// pts claims — bounds the reassembler's memory against a corrupt/hostile pts (which
/// [`LOSS_WINDOW_NS`] alone would trust) and against pathologically high frame rates. At 120 fps,
/// 120 ms ≈ 14 indices, so 64 leaves ample slack up to ~500 fps.
const HARD_LOSS_WINDOW: u32 = 64;

/// How many frames behind the newest the reassembler remembers emitted/abandoned frame indices
/// (`completed`), so a straggler shard can neither resurrect an abandoned frame nor re-open an
/// emitted one. Must cover at least [`HARD_LOSS_WINDOW`]: stragglers can trickle in later than the
/// loss verdict.
const REORDER_WINDOW: u32 = 64;

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
///
/// Frame numbering: a caller can pass an **explicit** `frame_index` to
/// [`packetize_each`](Self::packetize_each) (the punktfunk/1 encode loop owns the video numbering
/// so the encoder's reference-frame-invalidation bookkeeping stays 1:1 with the wire across
/// encoder rebuilds/resets), or pass `None` to draw from the internal counter (the legacy path —
/// synthetic/spike/ABI sessions where no encoder cares). Speed-test probe filler draws from a
/// **separate** index space ([`alloc_probe_index`](Self::alloc_probe_index)) so a burst never
/// consumes video indexes — see [`crate::quic::VIDEO_CAP_PROBE_SEQ`].
pub struct Packetizer {
    next_frame_index: u32,
    /// Probe-space frame counter (see [`alloc_probe_index`](Self::alloc_probe_index)).
    next_probe_index: u32,
    next_seq: u32,
    shard_payload: usize,
    fec: crate::config::FecConfig,
    version: u8,
    /// Reusable zero-padded scratch for the frame's final data shard when the frame isn't an
    /// exact `shard_payload` multiple (and for the single all-zero shard of an empty frame).
    /// Every other data shard is a `shard_payload`-sized slice straight into the frame buffer —
    /// blocks are consecutive shard ranges, so only the frame's last shard can be partial.
    tail: Vec<u8>,
}

impl Packetizer {
    pub fn new(config: &Config) -> Self {
        Packetizer {
            next_frame_index: 0,
            next_probe_index: 0,
            next_seq: 0,
            shard_payload: config.shard_payload,
            fec: config.fec,
            version: config.phase as u8,
            tail: Vec::new(),
        }
    }

    /// Allocate the next **probe-space** frame index (speed-test filler). A separate counter from
    /// the video `frame_index`es so a multi-thousand-AU probe burst never advances the video
    /// numbering — the client routes [`FLAG_PROBE`]-flagged shards into its own reassembly window
    /// (see [`Reassembler`]), so the two spaces never collide. Only used against clients that
    /// advertise [`crate::quic::VIDEO_CAP_PROBE_SEQ`].
    pub fn alloc_probe_index(&mut self) -> u32 {
        let i = self.next_probe_index;
        self.next_probe_index = i.wrapping_add(1);
        i
    }

    /// Live-adjust the FEC recovery percentage (adaptive FEC). Takes effect on the next
    /// [`packetize`](Self::packetize); the wire is self-describing (each packet carries its block's
    /// data/recovery counts), so the receiver needs no notification. Clamped to ≤ 90.
    pub fn set_fec_percent(&mut self, pct: u8) {
        self.fec.fec_percent = pct.min(90);
    }

    /// The current FEC recovery percentage.
    pub fn fec_percent(&self) -> u8 {
        self.fec.fec_percent
    }

    /// Packetize one access unit into owned wire packets (header ++ shard payload each).
    /// Thin wrapper over [`packetize_each`](Self::packetize_each) — the allocation-free
    /// streaming path's reference implementation (tests and the loss harness use this).
    pub fn packetize(
        &mut self,
        frame: &[u8],
        pts_ns: u64,
        user_flags: u32,
        coder: &dyn ErasureCoder,
    ) -> Result<Vec<Vec<u8>>> {
        let mut packets = Vec::new();
        self.packetize_each(frame, pts_ns, user_flags, None, coder, |hdr, body| {
            let mut pkt = Vec::with_capacity(HEADER_LEN + body.len());
            pkt.extend_from_slice(hdr.as_bytes());
            pkt.extend_from_slice(body);
            packets.push(pkt);
            Ok(())
        })?;
        Ok(packets)
    }

    /// Packetize one access unit, yielding each packet to `emit` as a `(header, shard bytes)`
    /// pair — in exact wire order, which is also the order the session's nonce counter
    /// advances. No per-packet allocation happens here, so the caller can write header and
    /// shard straight into a pooled wire buffer and seal in place
    /// ([`Session::seal_frame`](crate::session::Session::seal_frame)). An `emit` error aborts
    /// the frame mid-way (packet numbering has already advanced — callers treat it as fatal).
    ///
    /// `frame_index`: `Some(i)` stamps the AU with the caller's index — the punktfunk/1 encode
    /// loop numbers video AUs itself so the encoder's RFI bookkeeping (LTR marks, DPB timestamps)
    /// is 1:1 with what the client sees, surviving encoder rebuilds/resets that restart internal
    /// counters. `None` draws from the internal counter (the legacy/self-numbering path). A
    /// session must not mix the two styles for the same index space.
    pub fn packetize_each(
        &mut self,
        frame: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload = self.shard_payload;
        let frame_index = frame_index.unwrap_or_else(|| {
            let i = self.next_frame_index;
            self.next_frame_index = i.wrapping_add(1);
            i
        });

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

        // Stage the frame's one possibly-partial shard (the last) in the reusable
        // zero-padded scratch; every full shard is referenced in place below.
        let full_shards = frame.len() / payload;
        self.tail.clear();
        self.tail.resize(payload, 0);
        let rem = frame.len() % payload;
        if rem > 0 {
            self.tail[..rem].copy_from_slice(&frame[full_shards * payload..]);
        }
        let tail = &self.tail;
        let shard_at = |s: usize| -> &[u8] {
            if s < full_shards {
                &frame[s * payload..(s + 1) * payload]
            } else {
                tail.as_slice()
            }
        };

        for b in 0..block_count {
            let first = b * max_block;
            let last = ((b + 1) * max_block).min(total_data);
            let block_data_count = last - first;

            // This block's data shards: references into `frame` (plus the staged tail).
            let data_shards: Vec<&[u8]> = (first..last).map(shard_at).collect();

            let recovery_count = self.fec.recovery_for(block_data_count);
            let recovery = coder.encode(&data_shards, recovery_count)?;
            let total_shards = block_data_count + recovery_count;
            if total_shards > u16::MAX as usize {
                return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
            }

            for shard_index in 0..total_shards {
                let body: &[u8] = if shard_index < block_data_count {
                    data_shards[shard_index]
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
                emit(&hdr, body)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Client side: reassembly + FEC recovery
// ---------------------------------------------------------------------------

/// Per-block reassembly state. The block's DATA bytes live in the owning [`FrameBuf::buf`]
/// (each shard copied once, straight to its final AU offset); this tracks presence and holds
/// the received recovery shards until the block resolves.
struct BlockState {
    /// The block's K/M — pinned by the frame geometry derived from `frame_bytes` and validated
    /// against every packet of the block.
    data_shards: usize,
    recovery_shards: usize,
    /// Per-data-shard presence: which ranges of the frame buffer hold received bytes (also the
    /// FEC input map — the codec reads only present slots).
    have_data: Vec<bool>,
    data_received: usize,
    /// Received recovery shards (pooled shard-sized buffers, reclaimed when the block resolves).
    recovery: Vec<Option<Vec<u8>>>,
    recovery_received: usize,
    /// Terminal — either reconstructed (its buffer range is fully written) or unrecoverable
    /// (corrupt shards; the frame can never complete). Later shards for it are ignored.
    done: bool,
    /// The block resolved by actually consuming parity (`missing > 0` at reconstruct) — the only
    /// case where a data shard arriving after `done` was counted into `fec_recovered_shards` and
    /// must be netted back out as [`fec_late_shards`](crate::stats::Stats::fec_late_shards).
    reconstructed: bool,
}

struct FrameBuf {
    frame_bytes: usize,
    block_count: usize,
    pts_ns: u64,
    user_flags: u32,
    /// The whole frame's data region — `total_data_shards × shard_bytes` zeroed bytes. Data
    /// shards are copied to their final offset on arrival; FEC reconstruction writes only the
    /// missing shards' ranges. On completion this Vec IS [`Frame::data`] (truncated to
    /// `frame_bytes`) — the old shard→block→AU copy chain and its ~per-packet allocations are
    /// gone (the 2026-07-14 sweeps pinned the client pump as the ~1.5 Gbps wall, ~85% userspace).
    buf: Vec<u8>,
    blocks: HashMap<u16, BlockState>,
    /// Blocks fully reconstructed into `buf`. The frame completes when this reaches
    /// `block_count` (a failed block never counts — the frame then ages out as dropped).
    blocks_ok: usize,
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

/// One frame-index space's reassembly state: the in-flight frames, the recently-emitted memory,
/// and the loss-window anchor. The [`Reassembler`] keeps two — video and speed-test probe filler —
/// because the two ride **separate index counters** on a [`VIDEO_CAP_PROBE_SEQ`]-aware host
/// (a probe burst must neither advance the video loss window nor be dropped as "stale" against
/// it). [`VIDEO_CAP_PROBE_SEQ`]: crate::quic::VIDEO_CAP_PROBE_SEQ
#[derive(Default)]
struct ReassemblyWindow {
    frames: HashMap<u32, FrameBuf>,
    /// Recently-terminated frames (emitted OR abandoned by the loss window), so stray/late shards
    /// can't resurrect them. The value is the frame's parity-restored data shards (frame-wide
    /// index `block × max_data_shards + shard`, usually empty): each was counted into
    /// `fec_recovered_shards` at reconstruct, so when one ARRIVES after all — late, not lost —
    /// it's removed here and counted into `fec_late_shards` for the loss windows to net out
    /// (reordering alone must not read as packet loss). The removal makes the accounting exact:
    /// a wire duplicate of a shard that did arrive matches nothing and counts nothing. Pruned to
    /// the reorder window alongside `frames`.
    completed: HashMap<u32, Vec<u32>>,
    /// The newest frame seen, as `(frame_index, capture pts)` — the loss-window anchor: an
    /// incomplete frame is declared lost once it sits [`LOSS_WINDOW_NS`] behind this pts (or
    /// [`HARD_LOSS_WINDOW`] indices, whichever trips first).
    newest_frame: Option<(u32, u64)>,
}

/// Frame buffers are allocated whole (zeroed) at a frame's first shard, so bound how much a
/// window of tiny first-shards can commit: the sum of in-flight `FrameBuf::buf` bytes (both index
/// spaces) may not exceed `IN_FLIGHT_BUF_FACTOR × max_frame_bytes`. Honest streams hold 1–3
/// partially-arrived frames of ACTUAL size (≪ max); without this cap, [`HARD_LOSS_WINDOW`]
/// max-sized declarations from one header-sized packet each could commit gigabytes — an
/// amplification the old sparse per-shard allocation didn't have.
const IN_FLIGHT_BUF_FACTOR: usize = 4;

/// Recovery-shard buffer pool ceiling (shard-sized buffers): enough for several max-recovery
/// blocks in flight, small enough (~720 KB at a 1408-byte shard) to keep after a loss burst.
const RECOVERY_POOL_MAX: usize = 512;

/// Buffers incoming shards, recovers lost ones via FEC, and emits whole access units.
/// Client-side only.
pub struct Reassembler {
    limits: ReassemblerLimits,
    /// The video stream's window — its aged-out incomplete frames count into `frames_dropped`
    /// (the client's loss-recovery trigger).
    video: ReassemblyWindow,
    /// Speed-test probe filler ([`FLAG_PROBE`] in `user_flags`). Routed by the flag, so it also
    /// captures an OLD host's probe frames (which still carry video-space indexes — they complete
    /// fine here, and keeping them out of the video window means a burst can no longer advance the
    /// video loss anchor). Aged-out probe frames are NOT `frames_dropped` — probe loss is measured
    /// bytes-wise by the probe accumulator and must not fire video recovery.
    probe: ReassemblyWindow,
    /// Reusable shard-sized buffers for received recovery shards — the only shard bytes that
    /// still need their own storage (data shards land straight in the frame buffer). Capped at
    /// [`RECOVERY_POOL_MAX`].
    recovery_pool: Vec<Vec<u8>>,
    /// Sum of in-flight `FrameBuf::buf` bytes across both windows (see [`IN_FLIGHT_BUF_FACTOR`]).
    in_flight_bytes: usize,
}

impl Reassembler {
    pub fn new(limits: ReassemblerLimits) -> Self {
        Reassembler {
            limits,
            video: ReassemblyWindow::default(),
            probe: ReassemblyWindow::default(),
            recovery_pool: Vec::new(),
            in_flight_bytes: 0,
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
        // fatal: it must not abort `poll_frame`. A FEC reconstruction failure (corrupt or
        // incompatible shards that passed the header checks) likewise drops the block rather
        // than killing the whole session — the stream recovers at the next keyframe/RFI.
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

        // Disjoint field borrows: the window (`video`/`probe`), the recovery pool, and the
        // in-flight budget are all touched while a frame entry is mutably borrowed.
        let Reassembler {
            limits,
            video,
            probe,
            recovery_pool,
            in_flight_bytes,
        } = self;
        let lim = *limits;
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
        // Derived-geometry firewall: every sender (our Packetizer, any version) slices a frame
        // into consecutive blocks of exactly `max_data_per_block` data shards with only the LAST
        // block smaller, and stamps the exact `frame_bytes` in every header. That makes every
        // data shard's final AU offset computable on arrival —
        //     offset = (block_index × max_data_per_block + shard_index) × shard_bytes
        // — which is what lets shards land straight in the frame buffer below. Enforce the
        // invariant so a header lying about its geometry is dropped instead of scribbling into
        // another shard's range.
        let total_data = frame_bytes.div_ceil(shard_bytes).max(1);
        let expect_blocks = total_data.div_ceil(lim.max_data_shards).max(1);
        let block_idx = hdr.block_index as usize;
        let expect_data_shards = if block_idx + 1 == expect_blocks {
            total_data - (expect_blocks - 1) * lim.max_data_shards
        } else {
            lim.max_data_shards
        };
        if block_count != expect_blocks || data_shards != expect_data_shards {
            drop(stats);
            return Ok(None);
        }
        let body = &pkt[HEADER_LEN..HEADER_LEN + shard_bytes];

        // Route by index space: speed-test probe filler (FLAG_PROBE in user_flags) reassembles in
        // its own window so its indexes never interact with the video loss window — a probe burst
        // can neither advance the video anchor nor be dropped as stale against it (and its aged-out
        // frames never count as `frames_dropped`, which would fire video loss recovery).
        let is_probe = hdr.user_flags & (FLAG_PROBE as u32) != 0;
        let win = if is_probe { probe } else { video };
        win.advance_window(
            hdr.frame_index,
            hdr.pts_ns,
            stats,
            !is_probe,
            recovery_pool,
            in_flight_bytes,
            lim.max_data_shards,
        );

        // Drop shards for frames already terminated (emitted — e.g. the recovery shards of a
        // frame that completed early via the all-originals-present fast path — or abandoned by
        // the loss window) and for frames that have fallen out of the loss window entirely.
        if let Some(reconstructed) = win.completed.get_mut(&hdr.frame_index) {
            // A data shard the parity reconstruct already restored (and counted into
            // `fec_recovered_shards`) was late, not lost: count the arrival so the loss windows
            // net it out (`recovered - late`), or plain reordering reads as packet loss and
            // spooks adaptive FEC + the bitrate controller. Removing the match keeps it exact —
            // wire duplicates of delivered shards match nothing, recovery shards are never in
            // the list. No probe/video split: `fec_recovered_shards` counts both windows.
            if shard_index < data_shards {
                let fw = block_idx as u32 * lim.max_data_shards as u32 + shard_index as u32;
                if let Some(pos) = reconstructed.iter().position(|&s| s == fw) {
                    reconstructed.swap_remove(pos);
                    StatsCounters::add(&stats.fec_late_shards, 1);
                }
            }
            drop(stats);
            return Ok(None);
        }
        if win.is_stale(hdr.frame_index, hdr.pts_ns) {
            drop(stats);
            return Ok(None);
        }

        // First packet of a frame allocates its whole (zeroed) buffer, budget-gated; later
        // packets must agree with its geometry.
        let buf_len = total_data * shard_bytes;
        let frame = match win.frames.entry(hdr.frame_index) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                if *in_flight_bytes + buf_len > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                    // Budget exhausted (several max-size frames all partially in flight) — a
                    // stream this bites is already deep in loss; dropping the packet is strictly
                    // milder than what the loss window would do to the frame moments later.
                    drop(stats);
                    return Ok(None);
                }
                *in_flight_bytes += buf_len;
                e.insert(FrameBuf {
                    frame_bytes,
                    block_count,
                    pts_ns: hdr.pts_ns,
                    user_flags: hdr.user_flags,
                    buf: vec![0; buf_len],
                    blocks: HashMap::new(),
                    blocks_ok: 0,
                })
            }
        };
        if frame.block_count != block_count || frame.frame_bytes != frame_bytes {
            drop(stats);
            return Ok(None);
        }
        let FrameBuf {
            buf,
            blocks,
            blocks_ok,
            ..
        } = frame;

        // First packet of a block sizes its state; `data_shards` is already pinned by the
        // derived geometry above, but `recovery_shards` is per-block wire input (adaptive FEC
        // varies it per frame) — later packets must match the block's first.
        let block = blocks.entry(hdr.block_index).or_insert_with(|| BlockState {
            data_shards,
            recovery_shards,
            have_data: vec![false; data_shards],
            data_received: 0,
            recovery: vec![None; recovery_shards],
            recovery_received: 0,
            done: false,
            reconstructed: false,
        });
        if block.recovery_shards != recovery_shards {
            drop(stats);
            return Ok(None);
        }
        if block.done {
            // A data shard the parity reconstruct already restored (`!have_data`) was late, not
            // lost — net it out of the `fec_recovered_shards` it was counted into (see the
            // completed-frame twin above; this arm covers multi-block frames whose other blocks
            // are still in flight). `have_data == true` = wire duplicate; a failed reconstruct
            // (`!reconstructed`) never counted its missing shards, so neither do we.
            if block.reconstructed
                && shard_index < block.data_shards
                && !block.have_data[shard_index]
            {
                block.have_data[shard_index] = true; // it HAS arrived now — dedups a re-dup
                StatsCounters::add(&stats.fec_late_shards, 1);
            }
            return Ok(None);
        }

        if shard_index < data_shards {
            // A data shard lands at its final AU offset — the only copy its bytes ever make
            // past decrypt.
            if !block.have_data[shard_index] {
                let off = (block_idx * lim.max_data_shards + shard_index) * shard_bytes;
                buf[off..off + shard_bytes].copy_from_slice(body);
                block.have_data[shard_index] = true;
                block.data_received += 1;
            }
        } else {
            let slot = shard_index - data_shards;
            if block.recovery[slot].is_none() {
                let mut rb = recovery_pool.pop().unwrap_or_default();
                rb.clear();
                rb.extend_from_slice(body);
                block.recovery[slot] = Some(rb);
                block.recovery_received += 1;
            }
        }

        // Reconstruct as soon as we hold enough shards.
        if block.data_received + block.recovery_received >= block.data_shards {
            let missing = block.data_shards - block.data_received;
            let outcome = if missing == 0 {
                Ok(()) // every original arrived — its bytes are already in place
            } else {
                let base = block_idx * lim.max_data_shards * shard_bytes;
                let region = &mut buf[base..base + block.data_shards * shard_bytes];
                let mut slots: Vec<&mut [u8]> = region.chunks_mut(shard_bytes).collect();
                let parity: Vec<(usize, &[u8])> = block
                    .recovery
                    .iter()
                    .enumerate()
                    .filter_map(|(j, s)| s.as_deref().map(|b| (j, b)))
                    .collect();
                coder.reconstruct_into(block.recovery_shards, &mut slots, &block.have_data, &parity)
            };
            // The parity buffers are spent either way — reclaim them for the next block.
            for slot in block.recovery.iter_mut() {
                if let Some(rb) = slot.take() {
                    if recovery_pool.len() < RECOVERY_POOL_MAX {
                        recovery_pool.push(rb);
                    }
                }
            }
            block.done = true;
            match outcome {
                Ok(()) => {
                    // With in-order delivery `missing` is exactly the block's lost shards; under
                    // reordering the early trigger also "recovers" shards that are merely still
                    // in flight — their later arrival counts `fec_late_shards` (both arms above)
                    // so loss estimators can net the two (`window_loss_ppm`).
                    block.reconstructed = missing > 0;
                    StatsCounters::add(&stats.fec_recovered_shards, missing as u64);
                    *blocks_ok += 1;
                }
                Err(_) => {
                    // Corrupt/incompatible shards that slipped past the header checks: discard
                    // this block (done, but never counted ok — the frame can't complete and ages
                    // out) and keep the session alive; the client recovers at the next
                    // keyframe/RFI.
                    StatsCounters::add(&stats.packets_dropped, 1);
                    return Ok(None);
                }
            }
        }

        // Whole frame ready?
        if *blocks_ok == block_count {
            let mut done = win.frames.remove(&hdr.frame_index).unwrap();
            win.completed.insert(
                hdr.frame_index,
                reconstructed_shards(&done.blocks, lim.max_data_shards),
            );
            *in_flight_bytes -= done.buf.len();
            done.buf.truncate(done.frame_bytes); // trim trailing-shard zero padding
            return Ok(Some(Frame {
                data: done.buf,
                frame_index: hdr.frame_index,
                pts_ns: done.pts_ns,
                flags: done.user_flags,
            }));
        }
        Ok(None)
    }

    /// Drop all in-flight state — every partially-assembled frame and the completed/abandoned
    /// index memory, in both index spaces — as if the session just started. Used by the client's
    /// backlog flush ([`Session::flush_backlog`](crate::session::Session::flush_backlog)): after
    /// the socket backlog is discarded wholesale, the partial frames here can never complete
    /// (their remaining shards were just thrown away) and the window anchors (`newest_frame`)
    /// point into the discarded past.
    pub fn reset(&mut self) {
        self.video = ReassemblyWindow::default();
        self.probe = ReassemblyWindow::default();
        // The dropped frames' buffers (and their parity bufs) go back to the allocator, not the
        // pool — a flush is the rare path. The budget resets with them.
        self.in_flight_bytes = 0;
    }
}

/// The data shards of a terminating frame that only exist because parity restored them
/// (`reconstructed` blocks' still-absent originals), as frame-wide indexes
/// (`block × max_data_shards + shard`) for the [`ReassemblyWindow::completed`] late-shard
/// memory. Empty (no allocation) for the overwhelmingly common clean frame.
fn reconstructed_shards(blocks: &HashMap<u16, BlockState>, max_data_shards: usize) -> Vec<u32> {
    let mut v = Vec::new();
    for (&bi, b) in blocks {
        if b.reconstructed {
            for (i, have) in b.have_data.iter().enumerate() {
                if !have {
                    v.push(bi as u32 * max_data_shards as u32 + i as u32);
                }
            }
        }
    }
    v
}

impl ReassemblyWindow {
    /// Track the newest frame, declare incomplete frames that fell out of the loss window
    /// ([`LOSS_WINDOW_NS`] behind the newest pts, or [`HARD_LOSS_WINDOW`] indices) lost — for the
    /// video window (`count_drops`) counting them dropped, which is what drives the client's
    /// recovery-keyframe request — and prune the completed-index memory to [`REORDER_WINDOW`].
    #[allow(clippy::too_many_arguments)]
    fn advance_window(
        &mut self,
        frame_index: u32,
        pts_ns: u64,
        stats: &StatsCounters,
        count_drops: bool,
        recovery_pool: &mut Vec<Vec<u8>>,
        in_flight_bytes: &mut usize,
        max_data_shards: usize,
    ) {
        let (newest, newest_pts) = match self.newest_frame {
            // `frame_index` is newer iff it's within the forward half of the index space.
            Some((n, p)) if frame_index.wrapping_sub(n) > u32::MAX / 2 => (n, p),
            _ => (frame_index, pts_ns),
        };
        self.newest_frame = Some((newest, newest_pts));

        let before = self.frames.len();
        let completed = &mut self.completed;
        self.frames.retain(|&idx, f| {
            let keep = newest.wrapping_sub(idx) <= HARD_LOSS_WINDOW
                && newest_pts.saturating_sub(f.pts_ns) <= LOSS_WINDOW_NS;
            if !keep {
                // Remember the abandoned index so a straggler shard is dropped (below, and in
                // `push`) instead of resurrecting the frame — which would re-allocate its buffers
                // and double-count the drop when it aged out again. Blocks that reconstructed
                // before the frame died still counted `fec_recovered_shards`, so their restored
                // shards join the late-shard memory exactly like an emitted frame's.
                completed.insert(idx, reconstructed_shards(&f.blocks, max_data_shards));
                // Release its buffer budget and reclaim its parity bufs for the pool.
                *in_flight_bytes -= f.buf.len();
                for block in f.blocks.values_mut() {
                    for slot in block.recovery.iter_mut() {
                        if let Some(rb) = slot.take() {
                            if recovery_pool.len() < RECOVERY_POOL_MAX {
                                recovery_pool.push(rb);
                            }
                        }
                    }
                }
            }
            keep
        });
        let pruned = before - self.frames.len();
        if pruned > 0 && count_drops {
            StatsCounters::add(&stats.frames_dropped, pruned as u64);
        }
        self.completed
            .retain(|&idx, _| newest.wrapping_sub(idx) <= REORDER_WINDOW);
    }

    /// True if this packet's frame lies outside the loss window (behind the newest frame by more
    /// than [`LOSS_WINDOW_NS`] of capture time or [`HARD_LOSS_WINDOW`] indices) — its shards
    /// arrive too late to be useful, and accepting one would only create a frame buffer the next
    /// [`advance_window`](Self::advance_window) immediately declares lost.
    fn is_stale(&self, frame_index: u32, pts_ns: u64) -> bool {
        match self.newest_frame {
            Some((n, newest_pts)) => {
                let behind = n.wrapping_sub(frame_index);
                behind <= u32::MAX / 2
                    && (behind > HARD_LOSS_WINDOW
                        || newest_pts.saturating_sub(pts_ns) > LOSS_WINDOW_NS)
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

    /// The loss window is TIME-based: an incomplete frame survives newer frames arriving within
    /// [`LOSS_WINDOW_NS`] of its capture pts (a 33 ms-late shard at 120 fps is late, not lost —
    /// the old 4-INDEX window wrongly killed it), is declared lost once the newest pts moves past
    /// the window (`frames_dropped`), and a straggler shard can't resurrect it afterwards.
    #[test]
    fn incomplete_frames_age_out_by_capture_time_not_frame_count() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();
        const FRAME_NS: u64 = 8_333_333; // 120 fps

        // Frame 0: one of its two shards arrives — incomplete.
        let mut h = base_header();
        h.data_shards = 2;
        h.frame_bytes = 32;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());

        // Frames 1..=8 complete around it (well past the old 4-index window, inside 120 ms):
        // frame 0 must still be alive — no drop counted.
        for i in 1..=8u32 {
            let mut h = base_header();
            h.frame_index = i;
            h.pts_ns = i as u64 * FRAME_NS;
            assert!(r
                .push(&packet(h), coder.as_ref(), &stats)
                .unwrap()
                .is_some());
        }
        assert_eq!(stats.snapshot().frames_dropped, 0);

        // Frame 0's second shard arrives 8 frames late (~66 ms at 120 fps) — completes fine.
        let mut h = base_header();
        h.data_shards = 2;
        h.frame_bytes = 32;
        h.shard_index = 1;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_some());

        // Frame 20: incomplete again; then a frame lands past the 120 ms window → declared lost.
        let mut h = base_header();
        h.frame_index = 20;
        h.pts_ns = 20 * FRAME_NS;
        h.data_shards = 2;
        h.frame_bytes = 32;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        let mut h = base_header();
        h.frame_index = 21;
        h.pts_ns = 20 * FRAME_NS + LOSS_WINDOW_NS + 1;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_some());
        assert_eq!(stats.snapshot().frames_dropped, 1);

        // A straggler shard for the abandoned frame 20 is dropped, never resurrected.
        let mut h = base_header();
        h.frame_index = 20;
        h.pts_ns = 20 * FRAME_NS;
        h.data_shards = 2;
        h.frame_bytes = 32;
        h.shard_index = 1;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(stats.snapshot().frames_dropped, 1, "no double-count");
    }

    /// The explicit-index path stamps the caller's `frame_index` and leaves the internal video
    /// counter untouched — the punktfunk/1 encode loop owns the numbering, and mixing must not
    /// perturb the legacy self-numbering path (tests/ABI/synthetic).
    #[test]
    fn explicit_frame_index_is_stamped_and_internal_counter_untouched() {
        use crate::config::{FecConfig, FecScheme, ProtocolPhase, Role};
        let cfg = Config {
            role: Role::Host,
            phase: ProtocolPhase::P2Punktfunk,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0,
                max_data_per_block: 8,
            },
            shard_payload: 16,
            max_frame_bytes: 4096,
            encrypt: false,
            key: [0u8; 16],
            salt: [0u8; 4],
            loopback_drop_period: 0,
        };
        let coder = coder_for(FecScheme::Gf16);
        let mut pk = Packetizer::new(&cfg);
        let mut seen = Vec::new();
        pk.packetize_each(&[1u8; 16], 0, 0, Some(4242), coder.as_ref(), |hdr, _| {
            seen.push(hdr.frame_index);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![4242]);
        // The legacy wrapper still numbers from the untouched internal counter.
        let pkts = pk.packetize(&[1u8; 16], 0, 0, coder.as_ref()).unwrap();
        let hdr = PacketHeader::read_from_bytes(&pkts[0][..HEADER_LEN]).unwrap();
        assert_eq!(hdr.frame_index, 0);
        // The probe space is a third, independent counter.
        assert_eq!(pk.alloc_probe_index(), 0);
        assert_eq!(pk.alloc_probe_index(), 1);
    }

    /// Probe filler (FLAG_PROBE in user_flags) reassembles in its OWN window: a probe frame whose
    /// index is far behind the video stream's completes anyway (an old client's single window
    /// would drop it as stale), and video frames complete undisturbed around it.
    #[test]
    fn probe_frames_reassemble_in_their_own_window() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();

        // Establish a video stream far into its index space.
        let mut v = base_header();
        v.frame_index = 100_000;
        v.pts_ns = 1_000_000_000;
        assert!(r
            .push(&packet(v), coder.as_ref(), &stats)
            .unwrap()
            .is_some());

        // A probe frame at index 0 — 100k "behind" the video window — must still complete.
        let mut p = base_header();
        p.frame_index = 0;
        p.pts_ns = 1_000_000_100;
        p.user_flags = FLAG_PROBE as u32;
        let got = r.push(&packet(p), coder.as_ref(), &stats).unwrap();
        assert!(got.is_some(), "probe frame must complete in its own window");
        assert_eq!(got.unwrap().flags & FLAG_PROBE as u32, FLAG_PROBE as u32);

        // The probe burst must not have advanced the VIDEO window: the next video frame is
        // contiguous and completes, with nothing counted dropped.
        let mut v2 = base_header();
        v2.frame_index = 100_001;
        v2.pts_ns = 1_000_000_200;
        assert!(r
            .push(&packet(v2), coder.as_ref(), &stats)
            .unwrap()
            .is_some());
        assert_eq!(stats.snapshot().frames_dropped, 0);
    }

    /// An incomplete probe frame aging out of the probe window is NOT a video `frames_dropped`
    /// (which would fire the client's loss recovery) — probe loss is measured bytes-wise by the
    /// probe accumulator.
    #[test]
    fn aged_out_probe_frames_do_not_count_as_dropped() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();

        // Probe frame 0: one of two shards — incomplete.
        let mut p = base_header();
        p.user_flags = FLAG_PROBE as u32;
        p.data_shards = 2;
        p.frame_bytes = 32;
        assert!(r
            .push(&packet(p), coder.as_ref(), &stats)
            .unwrap()
            .is_none());

        // A much newer probe frame ages it out of the probe window.
        let mut p2 = base_header();
        p2.user_flags = FLAG_PROBE as u32;
        p2.frame_index = 1;
        p2.pts_ns = LOSS_WINDOW_NS + 1;
        assert!(r
            .push(&packet(p2), coder.as_ref(), &stats)
            .unwrap()
            .is_some());
        assert_eq!(
            stats.snapshot().frames_dropped,
            0,
            "probe-window drops must not fire video loss recovery"
        );
    }

    /// Build a host config for the end-to-end roundtrips: 16-byte shards, 4-data-shard blocks.
    fn e2e_config(scheme: FecScheme, fec_percent: u8) -> Config {
        use crate::config::{FecConfig, ProtocolPhase, Role};
        Config {
            role: Role::Host,
            phase: ProtocolPhase::P2Punktfunk,
            fec: FecConfig {
                scheme,
                fec_percent,
                max_data_per_block: 4,
            },
            shard_payload: 16,
            max_frame_bytes: 4096,
            encrypt: false,
            key: [0u8; 16],
            salt: [0u8; 4],
            loopback_drop_period: 0,
        }
    }

    /// Packetize a synthetic AU, deliver a mangled subset (losses within the FEC budget,
    /// optionally reversed, with a duplicate), and assert the reassembled AU is byte-identical
    /// to the source — the shards landed straight in the frame buffer at the right offsets and
    /// FEC filled the holes.
    ///
    /// `fec_recovered_shards` accounting: with in-order delivery it equals the kill count
    /// exactly (and nothing is late). With reversed delivery parity arrives first, so the
    /// `data + recovery ≥ k` trigger reconstructs EARLY and restores late-not-lost shards too —
    /// deliberate (latency), but each such shard's later arrival must count `fec_late_shards`
    /// so the NET (`recovered - late`) still equals the true kill count: reordering alone must
    /// not read as loss (it pollutes LossReports → adaptive FEC + the ABR controller).
    fn e2e_roundtrip(
        scheme: FecScheme,
        frame_len: usize,
        fec_percent: u8,
        kill: &[usize],
        reverse: bool,
    ) {
        let cfg = e2e_config(scheme, fec_percent);
        let coder = coder_for(scheme);
        let mut pk = Packetizer::new(&cfg);
        let src: Vec<u8> = (0..frame_len).map(|i| (i * 131 + 7) as u8).collect();
        let pkts = pk.packetize(&src, 12345, 0, coder.as_ref()).unwrap();

        let mut delivery: Vec<Vec<u8>> = pkts
            .iter()
            .enumerate()
            .filter(|(i, _)| !kill.contains(i))
            .map(|(_, p)| p.clone())
            .collect();
        if reverse {
            delivery.reverse(); // recovery shards (and the tail) arrive first
        }
        if let Some(dup) = delivery.first().cloned() {
            delivery.push(dup); // a duplicate must be ignored, not double-counted
        }

        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let mut got = None;
        for p in &delivery {
            if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
                assert!(got.is_none(), "frame must complete exactly once");
                got = Some(f);
            }
        }
        let f = got.expect("frame must complete within the FEC budget");
        assert_eq!(f.data, src, "reassembled AU must be byte-identical");
        assert_eq!(f.pts_ns, 12345);
        let snap = stats.snapshot();
        let (recovered, late) = (snap.fec_recovered_shards, snap.fec_late_shards);
        if reverse {
            assert!(
                recovered >= kill.len() as u64,
                "early reconstruct counts more"
            );
        } else {
            assert_eq!(recovered, kill.len() as u64);
        }
        assert_eq!(
            recovered - late,
            kill.len() as u64,
            "net recovered (recovered - late) must equal the true loss regardless of order \
             (recovered={recovered} late={late} killed={})",
            kill.len()
        );
    }

    /// Multi-block frame with a partial tail shard, heavy loss, both delivery orders + dups.
    /// 100 bytes / 16 = 7 shards → blocks of (4 data + 2 rec) and (3 data + 2 rec).
    #[test]
    fn e2e_multiblock_loss_reorder_dup_gf16() {
        // Packet order: blk0 = idx 0..6 (4 data + 2 rec), blk1 = idx 6..11 (3 data + 2 rec).
        // Kill 2 data in block 0 and 1 data in block 1 — all within the 50% budget.
        e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 7], false);
        e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 7], true);
    }

    #[test]
    fn e2e_multiblock_loss_reorder_dup_gf8() {
        e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 8], false);
        e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 8], true);
    }

    /// Zero losses, in order: the pure fast path (no codec call, recovered == 0) must still
    /// emit an identical AU.
    #[test]
    fn e2e_clean_delivery_gf16() {
        e2e_roundtrip(FecScheme::Gf16, 100, 50, &[], false);
    }

    /// An empty AU rides one zero-padded shard and reassembles to zero bytes.
    #[test]
    fn e2e_empty_frame() {
        let cfg = e2e_config(FecScheme::Gf16, 0);
        let coder = coder_for(FecScheme::Gf16);
        let mut pk = Packetizer::new(&cfg);
        let pkts = pk.packetize(&[], 7, 0, coder.as_ref()).unwrap();
        assert_eq!(pkts.len(), 1);
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let f = r
            .push(&pkts[0], coder.as_ref(), &stats)
            .unwrap()
            .expect("empty frame completes");
        assert!(f.data.is_empty());
    }

    /// Loss beyond the FEC budget: the frame never emits, ages out as dropped, and the
    /// unrecoverable-block path must not fire (block never gathers k shards at all).
    #[test]
    fn e2e_unrecoverable_loss_ages_out() {
        let cfg = e2e_config(FecScheme::Gf16, 50);
        let coder = coder_for(FecScheme::Gf16);
        let mut pk = Packetizer::new(&cfg);
        let src = vec![0x5Au8; 64]; // one block: 4 data + 2 recovery
        let pkts = pk.packetize(&src, 1_000, 0, coder.as_ref()).unwrap();
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        // Deliver only 3 of 6 shards (k=4): can never reconstruct.
        for p in &pkts[..3] {
            assert!(r.push(p, coder.as_ref(), &stats).unwrap().is_none());
        }
        // A newer frame past the loss window ages it out as a video drop.
        let next = pk
            .packetize(&src, 1_000 + LOSS_WINDOW_NS + 1, 0, coder.as_ref())
            .unwrap();
        let mut done = false;
        for p in &next {
            done |= r.push(p, coder.as_ref(), &stats).unwrap().is_some();
        }
        assert!(done);
        assert_eq!(stats.snapshot().frames_dropped, 1);
    }

    /// The in-flight buffer budget: a window of tiny first-shards all declaring max-size frames
    /// stops allocating at [`IN_FLIGHT_BUF_FACTOR`] × max_frame_bytes instead of committing
    /// gigabytes (the eager whole-frame buffer's amplification defense).
    #[test]
    fn in_flight_buffer_budget_bounds_allocation() {
        let lim = limits(); // max_frame_bytes 4096, shards 16 B, ≤8 data shards × ≤4 blocks
        let mut r = Reassembler::new(lim);
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();
        // Largest geometry-consistent frame: 4 blocks × 8 shards × 16 B = 512 B per buffer.
        // Budget = 4 × 4096 = 16384 B → exactly 32 such frames fit; the 33rd must be refused.
        for i in 0..33u32 {
            let mut h = base_header();
            h.frame_index = i;
            h.frame_bytes = 512;
            h.block_count = 4;
            h.data_shards = 8;
            r.push(&packet(h), coder.as_ref(), &stats).unwrap();
        }
        assert_eq!(
            stats.snapshot().packets_dropped,
            1,
            "the frame past the budget is dropped, everything under it accepted"
        );
    }

    /// A header whose (data_shards, block_count) disagree with the geometry derived from its own
    /// frame_bytes is dropped — the derived-offset invariant that lets shards land directly in
    /// the frame buffer.
    #[test]
    fn rejects_geometry_inconsistent_with_frame_bytes() {
        let mut r = Reassembler::new(limits());
        let coder = coder_for(FecScheme::Gf8);
        let stats = StatsCounters::default();
        let mut h = base_header();
        h.frame_bytes = 16; // exactly one shard…
        h.data_shards = 2; // …but claims two
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
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
