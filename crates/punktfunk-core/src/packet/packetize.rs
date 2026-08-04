//! Host side: split an access unit into FEC-protected shard packets.

use super::*;
use crate::config::Config;
use crate::error::{PunktfunkError, Result};
use crate::fec::ErasureCoder;
use zerocopy::IntoBytes;

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
    /// The negotiated frame-size cap — kept so a live shard-payload swap
    /// ([`set_shard_payload`](Self::set_shard_payload)) can re-derive the per-frame block
    /// ceilings from the same formulas construction used.
    max_frame_bytes: usize,
    fec: crate::config::FecConfig,
    version: u8,
    /// Reusable zero-padded scratch for the frame's final data shard when the frame isn't an
    /// exact `shard_payload` multiple (and for the single all-zero shard of an empty frame).
    /// Every other data shard is a `shard_payload`-sized slice straight into the frame buffer —
    /// blocks are consecutive shard ranges, so only the frame's last shard can be partial.
    tail: Vec<u8>,
    /// Reusable PER-BLOCK parity buffers for [`ErasureCoder::encode_into`] (plan Phase 1.4):
    /// one pool per block index, each growing once to its high-water recovery count, then
    /// every frame's parity is generated into them with zero allocations. Per-block (not one
    /// shared pool) because the data-first wire order (latency plan T1.3) emits every block's
    /// DATA shards before any block's parity — all blocks' parity must stay alive until the
    /// frame's second emission pass.
    recovery: Vec<Vec<Vec<u8>>>,
    /// The peer's per-block `data + recovery` acceptance ceiling, frozen from the **negotiated**
    /// config exactly as the far side derives it in [`ReassemblerLimits::from_config`]. Adaptive
    /// FEC moves `fec.fec_percent` live ([`set_fec_percent`](Self::set_fec_percent)) but the
    /// receiver's ceiling is computed once at session construction and never re-derived, so parity
    /// must be clamped against this or a raised percentage puts blocks over the far side's bound —
    /// where every packet of the block is dropped wholesale, the frame never completes, and the
    /// resulting loss pushes adaptive FEC *higher*. See the `recovery_for` clamp in `packetize_each`.
    max_total_shards: usize,
    /// The peer's per-frame block ceiling — the streamed path's bound on how many sentinel
    /// blocks it may emit (a streamed AU's size isn't known up front, so this is the only
    /// pre-emission guard against producing a frame the receiver must reject). The receiver
    /// derives the same ceiling per packet from the packet's own `shard_bytes`
    /// (`Reassembler::push` — geometry is per-frame), so this stays in step as long as it is
    /// computed from the shard size this packetizer actually stamps.
    max_blocks: usize,
    /// The streamed path's block-count ceiling in SLICE mode ([`USER_FLAG_SLICE_STREAM`]) —
    /// variable-K blocks, floored at `min(MIN_STREAM_BLOCK_SHARDS, max_data_per_block)` shards.
    slice_block_cap: usize,
}

/// One in-progress **streamed** access unit (design/nvenc-subframe-slice-output.md Phase 2 +
/// the P2 slice pipeline): the caller feeds encoder chunks in as they exist
/// ([`Packetizer::push_streamed`]) and slice-granularity blocks leave under SENTINEL headers
/// (`block_count = 0`, `frame_bytes` = the block's shard-aligned base byte offset — the first
/// block's base 0 is byte-identical to the legacy sentinel) before the AU's total size is
/// known; [`Packetizer::finish_streamed`] seals the tail block with the real totals and
/// `FLAG_EOF`, which retro-validate the whole layout. Only ever sent to a peer that advertised
/// [`crate::quic::VIDEO_CAP_STREAMED_AU`] — and slice-granularity (non-zero-base) sentinels
/// additionally require [`crate::quic::VIDEO_CAP_MULTI_SLICE`], whose receivers know the
/// base-offset contract (stable pre-multi-slice clients reject a non-zero sentinel
/// `frame_bytes`, and their hosts never fired this path at real bitrates anyway).
pub struct StreamedAu {
    frame_index: u32,
    pts_ns: u64,
    user_flags: u32,
    /// Bytes not yet sealed into a block: the sub-shard remainder plus anything below the
    /// slice-flush threshold. The final block always has ≥ 1 byte — flushes emit only whole
    /// shards, and a flush that WOULD empty this keeps one shard back (see `push_streamed`),
    /// so `finish_streamed` always has something real to seal.
    pending: Vec<u8>,
    /// Sentinel blocks already emitted.
    blocks_out: u16,
    /// Total AU bytes accumulated so far (`pending` included).
    total_bytes: u64,
    /// WHOLE SHARDS already emitted in sentinel blocks — the next block's base offset in shard
    /// units (bases are always shard-aligned so the frame layout tiles in shard units; the
    /// receiver derives the final block's base from the totals the same way).
    emitted_shards: u64,
    /// The frame's first packet (block 0, shard 0 — carries `FLAG_SOF`) has been emitted.
    opened: bool,
}

/// The slice-flush floor: a sentinel block below this many data shards costs disproportionate
/// per-block FEC parity (`ceil(k × pct/100)` ≥ 1 whatever `k`), so slice boundaries only flush
/// once this much has accumulated (~22 KB at the standard shard payload). Small slices simply
/// ride with the next one; the wire is never worse than one flush per slice.
pub const MIN_STREAM_BLOCK_SHARDS: usize = 16;

impl StreamedAu {
    /// The wire frame index this AU is sealed with (the caller's RFI bookkeeping domain).
    pub fn frame_index(&self) -> u32 {
        self.frame_index
    }
}

impl Packetizer {
    pub fn new(config: &Config) -> Self {
        let max_data = config.fec.max_data_per_block as usize;
        let mut p = Packetizer {
            next_frame_index: 0,
            next_probe_index: 0,
            next_seq: 0,
            shard_payload: config.shard_payload,
            max_frame_bytes: config.max_frame_bytes,
            fec: config.fec,
            version: config.phase as u8,
            tail: Vec::new(),
            recovery: Vec::new(),
            // Mirrors `ReassemblerLimits::from_config` — keep the two in step.
            max_total_shards: (max_data + config.fec.recovery_for(max_data))
                .min(config.fec.scheme.max_total_shards()),
            // Derived from the shard size below (single source of truth for the formulas).
            max_blocks: 0,
            slice_block_cap: 0,
        };
        p.set_shard_payload(config.shard_payload);
        p
    }

    /// Live-swap the wire shard payload (mid-session shard renegotiation,
    /// design/shard-payload-reneg.md Phase 1). Takes effect on the next packetized AU — call
    /// ONLY between AUs, never with a [`StreamedAu`] in flight: an open streamed AU's
    /// shard-aligned tiling derives from the size it began with, and re-keying under it would
    /// corrupt the frame's layout. The per-frame block ceilings follow the new size here; the
    /// receiver re-derives its side per packet from the header's own `shard_bytes` (geometry
    /// is per-frame there), so the two stay in step by construction. Bounds are the caller's
    /// contract — go through [`Session::set_shard_payload`](crate::session::Session::set_shard_payload),
    /// which enforces the `Config::validate` rules.
    pub fn set_shard_payload(&mut self, shard_payload: usize) {
        let max_data = self.fec.max_data_per_block as usize;
        let total_data_max = self.max_frame_bytes.div_ceil(shard_payload.max(1)).max(1);
        self.shard_payload = shard_payload;
        self.max_blocks = total_data_max.div_ceil(max_data).max(1);
        // Every non-final SLICE block carries at least `min(MIN_STREAM_BLOCK_SHARDS, K)`
        // data shards (the flush floor, clamped by the block size), so a max-size frame
        // bounds the block count. Mirrors the receiver's slice firewall — keep in step.
        self.slice_block_cap = total_data_max / MIN_STREAM_BLOCK_SHARDS.min(max_data.max(1)) + 2;
    }

    /// The wire shard payload AUs are currently packetized at.
    pub fn shard_payload(&self) -> usize {
        self.shard_payload
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
    /// Wire order is DATA-FIRST (latency plan T1.3): every block's data shards in block order,
    /// then every block's parity shards in block order. In the lossless case a frame completes
    /// the moment its last DATA shard arrives, so the completion-gating packet no longer sits
    /// behind the parity tail of the paced spread (~fec% of the spread saved). The receiver is
    /// order-agnostic (headers are self-describing; the reassembler completes each block on
    /// `data + recovery ≥ k`), so this is not a wire-format change. `FLAG_SOF` stays on the
    /// first emitted packet (block 0, shard 0); `FLAG_EOF` marks the last emitted packet —
    /// the final parity shard, or the final data shard of a FEC-free frame.
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
        // Per-block shard geometry (deterministic — recomputed in both passes).
        let block_data_count = |b: usize| ((b + 1) * max_block).min(total_data) - b * max_block;
        // Parity for a `k`-shard block: the configured percentage, clamped so the block's wire
        // total never exceeds what the peer will accept (see `max_total_shards`). The clamp only
        // binds on blocks near `max_data_per_block`; smaller blocks keep the full adaptive range,
        // so raising FEC still buys real protection wherever there is headroom. Bound as locals,
        // not as a `&self` method: `emit_one` below would otherwise capture all of `self` and
        // collide with the `&mut self.recovery[b]` parity borrow.
        let (fec, max_total_shards) = (self.fec, self.max_total_shards);
        let recovery_for =
            move |k: usize| fec.recovery_for(k).min(max_total_shards.saturating_sub(k));

        // One parity pool per block, reused across frames (steady-state zero-alloc).
        if self.recovery.len() < block_count {
            self.recovery.resize_with(block_count, Vec::new);
        }

        // Total parity across the frame decides where FLAG_EOF lands (the last emitted packet).
        let mut total_recovery = 0usize;
        for b in 0..block_count {
            let k = block_data_count(b);
            let m = recovery_for(k);
            if k + m > u16::MAX as usize {
                return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
            }
            total_recovery += m;
        }

        let mut emit_one =
            |next_seq: &mut u32, b: usize, shard_index: usize, body: &[u8], flags: u8| {
                let seq = *next_seq;
                *next_seq = next_seq.wrapping_add(1);
                let k = block_data_count(b);
                let hdr = PacketHeader {
                    pts_ns,
                    frame_index,
                    stream_seq: seq,
                    frame_bytes,
                    user_flags,
                    block_index: b as u16,
                    block_count: block_count as u16,
                    data_shards: k as u16,
                    recovery_shards: recovery_for(k) as u16,
                    shard_index: shard_index as u16,
                    shard_bytes: payload as u16,
                    magic: PUNKTFUNK_MAGIC,
                    version: self.version,
                    fec_scheme: coder.scheme() as u8,
                    flags,
                };
                emit(&hdr, body)
            };
        let mut next_seq = self.next_seq;

        // Pass 1 — per block: generate parity into the block's pool, emit the DATA shards.
        for b in 0..block_count {
            let first = b * max_block;
            let k = block_data_count(b);

            // This block's data shards: references into `frame` (plus the staged tail).
            let data_shards: Vec<&[u8]> = (first..first + k).map(shard_at).collect();
            let recovery_count = recovery_for(k);
            coder.encode_into(&data_shards, recovery_count, &mut self.recovery[b])?;

            for (shard_index, body) in data_shards.iter().enumerate() {
                let mut flags = FLAG_PIC;
                if b == 0 && shard_index == 0 {
                    flags |= FLAG_SOF;
                }
                if total_recovery == 0 && b + 1 == block_count && shard_index + 1 == k {
                    flags |= FLAG_EOF;
                }
                emit_one(&mut next_seq, b, shard_index, body, flags)?;
            }
        }

        // Pass 2 — per block: emit the parity shards (the frame's tail on the wire).
        let mut parity_left = total_recovery;
        for b in 0..block_count {
            let k = block_data_count(b);
            let recovery_count = recovery_for(k);
            for r in 0..recovery_count {
                parity_left -= 1;
                let mut flags = FLAG_PIC;
                if parity_left == 0 {
                    flags |= FLAG_EOF;
                }
                let body: &[u8] = &self.recovery[b][r];
                emit_one(&mut next_seq, b, k + r, body, flags)?;
            }
        }
        self.next_seq = next_seq;
        Ok(())
    }

    /// Open a **streamed** access unit (see [`StreamedAu`]). `frame_index` follows the same
    /// contract as [`packetize_each`](Self::packetize_each): `Some(i)` = the caller owns the
    /// video numbering; `None` draws from the internal counter.
    pub fn begin_streamed(
        &mut self,
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
    ) -> StreamedAu {
        let frame_index = frame_index.unwrap_or_else(|| {
            let i = self.next_frame_index;
            self.next_frame_index = i.wrapping_add(1);
            i
        });
        StreamedAu {
            frame_index,
            pts_ns,
            user_flags,
            pending: Vec::new(),
            blocks_out: 0,
            total_bytes: 0,
            emitted_shards: 0,
            opened: false,
        }
    }

    /// Feed one encoder chunk into a streamed AU, emitting every SLICE-GRANULARITY block that
    /// completes under sentinel headers (`block_count = 0`; `frame_bytes` = the block's BASE
    /// SHARD OFFSET × shard size — see [`PacketHeader`]'s sentinel contract). `slice_end` marks
    /// an encoder-chunk boundary (an Annex-B slice cut): only there may a partial-frame block
    /// flush, and only WHOLE shards flush (the sub-shard remainder stays pending so every
    /// sentinel base is shard-aligned and the layout tiles in shard units — the receiver's
    /// placement + retro-validation contract). Small tails ride along until
    /// [`MIN_STREAM_BLOCK_SHARDS`] accumulate: per-block parity makes tiny blocks
    /// FEC-expensive. The frame's last block — whose header must carry the real totals — is
    /// never emitted here. Packets reach `emit` in wire order, data then parity per block.
    pub fn push_streamed(
        &mut self,
        au: &mut StreamedAu,
        chunk: &[u8],
        slice_end: bool,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        au.total_bytes += chunk.len() as u64;
        au.pending.extend_from_slice(chunk);
        let payload = self.shard_payload;
        let block_bytes = self.fec.max_data_per_block as usize * payload;
        // The AU's own [`USER_FLAG_SLICE_STREAM`] bit is the single gate for the slice wire
        // shape: without it, `slice_end` is inert and every sentinel goes out byte-identical
        // to the legacy contract (full-K, `frame_bytes = 0`) — which shipped receivers REQUIRE
        // (their firewall drops any other sentinel). Callers therefore pass `slice_end`
        // unconditionally and gate by setting the flag on the AU.
        let slice_wire = au.user_flags & USER_FLAG_SLICE_STREAM != 0;
        // A chunk can complete SEVERAL blocks (a slice bigger than one FEC block, or a huge
        // legacy chunk) — keep cutting until neither flush condition holds, so the final block
        // can never end up oversized.
        loop {
            let whole = au.pending.len() / payload;
            // Legacy full-block flush stays as the hard ceiling (a frame bigger than one FEC
            // block must flush regardless of slice geometry); slice boundaries flush earlier.
            let must_flush = au.pending.len() > block_bytes;
            let slice_flush = slice_wire && slice_end && whole >= MIN_STREAM_BLOCK_SHARDS;
            if !(must_flush || slice_flush) {
                return Ok(());
            }
            // Room for this sentinel block AND the final block after it, within the u16 wire
            // field and the mode's block-count ceiling (the receiver enforces its mirror).
            let cap = if slice_wire {
                self.slice_block_cap
            } else {
                self.max_blocks
            };
            if au.blocks_out as usize + 2 > cap.min(u16::MAX as usize) {
                return Err(PunktfunkError::Unsupported(
                    "streamed AU exceeds the negotiated max_frame_bytes",
                ));
            }
            // Never drain `pending` to EMPTY. [`finish_streamed`] must have bytes left to seal,
            // or the final block degenerates to a single zero-padded filler shard whose derived
            // base (`total_data − 1`) overlaps the block flushed just now — which the receiver's
            // retro-validation correctly reads as a lying header and kills the whole AU. It bites
            // exactly when the AU's length is a multiple of `shard_payload` (~1 in 1408 frames on
            // a 1500-MTU link), and only on the slice arm: the legacy `must_flush` is a strict
            // `>`, so its remainder is never empty. Keeping one whole shard back costs nothing —
            // it rides out in the final block, which has to exist regardless.
            let mut k = whole.min(self.fec.max_data_per_block as usize);
            if k > 1 && k == whole && au.pending.len() == whole * payload {
                k -= 1;
            }
            let sof = !au.opened;
            let (bi, pts, uf) = (au.blocks_out, au.pts_ns, au.user_flags);
            let fi = au.frame_index;
            let base_bytes = if slice_wire {
                au.emitted_shards
                    .checked_mul(payload as u64)
                    .and_then(|b| u32::try_from(b).ok())
                    .ok_or(PunktfunkError::Unsupported("streamed AU exceeds u32 bytes"))?
            } else {
                0 // the legacy sentinel contract: uniform full-K blocks, no base on the wire
            };
            let flush_len = k * payload;
            self.emit_streamed_block(
                fi,
                pts,
                uf,
                bi,
                &au.pending[..flush_len],
                base_bytes,
                0,
                sof,
                false,
                coder,
                &mut emit,
            )?;
            au.pending.drain(..flush_len);
            au.emitted_shards += k as u64;
            au.blocks_out += 1;
            au.opened = true;
        }
    }

    /// Close a streamed AU: seal the final block — its headers carry the REAL
    /// `frame_bytes`/`block_count`, which retro-validate the whole frame at the receiver — with
    /// `FLAG_EOF` on the last emitted packet. An empty AU degenerates to today's single
    /// zero-padded-shard frame (`block_count = 1`, never a sentinel).
    pub fn finish_streamed(
        &mut self,
        au: StreamedAu,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let frame_bytes = u32::try_from(au.total_bytes)
            .map_err(|_| PunktfunkError::Unsupported("streamed AU exceeds u32 bytes"))?;
        let block_count = au.blocks_out + 1;
        self.emit_streamed_block(
            au.frame_index,
            au.pts_ns,
            au.user_flags,
            au.blocks_out,
            &au.pending,
            frame_bytes,
            block_count,
            !au.opened,
            true,
            coder,
            &mut emit,
        )
    }

    /// Seal ONE streamed block (data shards + its parity, in wire order). Sentinel blocks pass
    /// `block_count = 0` and REUSE `frame_bytes` as the block's base byte offset (shard-aligned
    /// by the caller; the first block's base is 0 — byte-identical to the legacy sentinel);
    /// the final block passes the real totals. `sof` marks the frame's very first packet,
    /// `eof` its very last.
    #[allow(clippy::too_many_arguments)]
    fn emit_streamed_block(
        &mut self,
        frame_index: u32,
        pts_ns: u64,
        user_flags: u32,
        block_index: u16,
        bytes: &[u8],
        frame_bytes: u32,
        block_count: u16,
        sof: bool,
        eof: bool,
        coder: &dyn ErasureCoder,
        emit: &mut impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload = self.shard_payload;
        if payload > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg("shard_payload exceeds u16"));
        }
        // At least one (zero-padded) data shard even for an empty final block (empty AU).
        let k = bytes.len().div_ceil(payload).max(1);
        let m = self
            .fec
            .recovery_for(k)
            .min(self.max_total_shards.saturating_sub(k));
        if k + m > u16::MAX as usize {
            return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
        }
        // Stage the one possibly-partial (or empty-frame) shard in the zero-padded scratch.
        let full_shards = bytes.len() / payload;
        self.tail.clear();
        self.tail.resize(payload, 0);
        let rem = bytes.len() % payload;
        if rem > 0 {
            self.tail[..rem].copy_from_slice(&bytes[full_shards * payload..]);
        }
        let tail = &self.tail;
        let shard_at = |s: usize| -> &[u8] {
            if s < full_shards {
                &bytes[s * payload..(s + 1) * payload]
            } else {
                tail.as_slice()
            }
        };
        let data_shards: Vec<&[u8]> = (0..k).map(shard_at).collect();
        if self.recovery.is_empty() {
            self.recovery.push(Vec::new());
        }
        coder.encode_into(&data_shards, m, &mut self.recovery[0])?;

        let mut next_seq = self.next_seq;
        let mut emit_one = |next_seq: &mut u32, shard_index: usize, body: &[u8], flags: u8| {
            let seq = *next_seq;
            *next_seq = next_seq.wrapping_add(1);
            let hdr = PacketHeader {
                pts_ns,
                frame_index,
                stream_seq: seq,
                frame_bytes,
                user_flags,
                block_index,
                block_count,
                data_shards: k as u16,
                recovery_shards: m as u16,
                shard_index: shard_index as u16,
                shard_bytes: payload as u16,
                magic: PUNKTFUNK_MAGIC,
                version: self.version,
                fec_scheme: coder.scheme() as u8,
                flags,
            };
            emit(&hdr, body)
        };
        for (shard_index, body) in data_shards.iter().enumerate() {
            let mut flags = FLAG_PIC;
            if sof && shard_index == 0 {
                flags |= FLAG_SOF;
            }
            if eof && m == 0 && shard_index + 1 == k {
                flags |= FLAG_EOF;
            }
            emit_one(&mut next_seq, shard_index, body, flags)?;
        }
        for r in 0..m {
            let mut flags = FLAG_PIC;
            if eof && r + 1 == m {
                flags |= FLAG_EOF;
            }
            let body: &[u8] = &self.recovery[0][r];
            emit_one(&mut next_seq, k + r, body, flags)?;
        }
        self.next_seq = next_seq;
        Ok(())
    }
}
