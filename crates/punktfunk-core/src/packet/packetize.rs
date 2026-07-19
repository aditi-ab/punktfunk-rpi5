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
}

impl Packetizer {
    pub fn new(config: &Config) -> Self {
        let max_data = config.fec.max_data_per_block as usize;
        Packetizer {
            next_frame_index: 0,
            next_probe_index: 0,
            next_seq: 0,
            shard_payload: config.shard_payload,
            fec: config.fec,
            version: config.phase as u8,
            tail: Vec::new(),
            recovery: Vec::new(),
            // Mirrors `ReassemblerLimits::from_config` — keep the two in step.
            max_total_shards: (max_data + config.fec.recovery_for(max_data))
                .min(config.fec.scheme.max_total_shards()),
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
}
