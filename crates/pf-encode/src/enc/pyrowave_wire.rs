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
/// the SOF packet's offset. The colour bits live in the little-endian second word's top byte
/// (`seq_offset + 7`): `color_primaries` bit 27 (`0x08`), `transfer_function` bit 28 (`0x10`),
/// `ycbcr_transform` bit 29 (`0x20`), `ycbcr_range` bit 30 (`0x40`); `chroma_siting` bit 31 stays 0
/// (CENTER — the pyrowave CSCs use the centre-sited 2×2 box, unlike the left-cosited P010 path).
/// Range is ALWAYS stamped LIMITED (both CSCs emit studio range); `bt2020_pq` additionally stamps
/// BT.2020 primaries + PQ transfer + BT.2020 matrix — upstream's own enum semantics
/// (`pyrowave_common.hpp`), matching the session's negotiated `ColorInfo`.
pub(crate) fn stamp_color_bits(bitstream: &mut [u8], seq_offset: usize, bt2020_pq: bool) {
    if let Some(b) = bitstream.get_mut(seq_offset + 7) {
        *b |= 0x40;
        if bt2020_pq {
            *b |= 0x08 | 0x10 | 0x20;
        }
    }
}

/// The wavelet block space's total 32x32-block count for a mode — the exact counting walk of
/// upstream `WaveletBuffers::init_block_meta` (also ported to the Apple `WaveletLayout`, whose
/// golden tests pin it against real host AUs). Needed because the vendored RDO pass packs the
/// block index into 16 bits (`RDOperation.block_offset_saving` — see
/// `patches/0002-rdo-saving-clamp.patch`): a mode whose count exceeds `u16::MAX` would wrap
/// inside the rate controller, so the host guards such modes out (≈8K 4:4:4 territory).
pub(crate) fn block_count_32x32(width: u32, height: u32, chroma444: bool) -> u32 {
    const LEVELS: u32 = 5;
    let align = |v: u32| ((v + 31) & !31).max(128);
    let (aw, ah) = (align(width), align(height));
    let mut count = 0u32;
    for level in (0..LEVELS).rev() {
        let lw = (aw / 2) >> level;
        let lh = (ah / 2) >> level;
        let blocks_x8 = lw.div_ceil(8);
        let blocks_y8 = lh.div_ceil(8);
        let per_band = blocks_x8.div_ceil(4) * blocks_y8.div_ceil(4);
        let bands = if level == LEVELS - 1 { 4 } else { 3 };
        for component in 0..3u32 {
            if level == 0 && component != 0 && !chroma444 {
                continue;
            }
            count += per_band * bands;
        }
    }
    count
}

/// Wire-aware deflation of the per-frame rate budget for the datagram-aligned mode.
///
/// [`build_au`]'s windowing inflates the codec bitstream on its way to the wire: greedy packing
/// of pyrowave's few-hundred-byte atomic block packets into `chunk`-sized windows leaves the
/// tail of most windows zero-padded, plus the 4-byte prefixes and FRAG-chain tails. At
/// 1440p/~850 KiB frames that is ×1.2–1.3 — the 2026-07 field report's "Automatic" 407 Mb/s
/// pin put 550 Mb/s on a 1 GbE link. The pin is a promise about the LINK, so the codec budget
/// must absorb the framing: this tracker measures the real AU/bitstream ratio per frame and
/// deflates the budget handed to pyrowave's rate control by its EMA. Sealed-datagram framing
/// (packet header + AEAD tag) and FEC parity are deliberately NOT compensated — H.26x sessions
/// carry those on top of the configured bitrate too, and the pin must mean the same thing for
/// every codec.
pub(crate) struct WireBudget {
    /// EMA of `built AU bytes / packetized bitstream bytes`, ×1024 fixed point.
    scale_x1024: u32,
}

impl WireBudget {
    /// Startup prior (×1024 ≈ 1.25 — the 1440p field measurement's midpoint); the EMA
    /// converges onto the session's real ratio within ~a second of frames.
    const PRIOR_X1024: u32 = 1280;
    /// EMA weight 1/8: content-driven per-frame wobble smooths out; a mode/bitrate change
    /// re-converges in ~16 frames.
    const EMA_SHIFT: u32 = 3;
    /// Sanity clamp on the applied scale: never inflate the budget (×1.0 floor), never
    /// deflate below half (×2.0 cap — tiny explicit bitrates window very coarsely).
    const MIN_X1024: u32 = 1024;
    const MAX_X1024: u32 = 2048;

    pub(crate) fn new() -> WireBudget {
        WireBudget {
            scale_x1024: Self::PRIOR_X1024,
        }
    }

    /// Record one frame's measured inflation (`bitstream_len` = the packetized codec bytes the
    /// rate controller budgeted; `au_len` = the windowed AU that actually reaches the wire).
    pub(crate) fn observe(&mut self, bitstream_len: usize, au_len: usize) {
        if bitstream_len == 0 {
            return;
        }
        let sample = ((au_len as u64 * 1024) / bitstream_len as u64)
            .clamp(Self::MIN_X1024 as u64, Self::MAX_X1024 as u64) as u32;
        let ema = self.scale_x1024 as i64;
        self.scale_x1024 = (ema + ((sample as i64 - ema) >> Self::EMA_SHIFT)) as u32;
    }

    /// The rate-control budget that makes the WIRE hit `budget` bytes/frame under the
    /// currently-measured inflation.
    pub(crate) fn deflate(&self, budget: usize) -> usize {
        let scale = self.scale_x1024.clamp(Self::MIN_X1024, Self::MAX_X1024) as u64;
        ((budget as u64 * 1024) / scale) as usize
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

// ---------------------------------------------------------------------------
// Streamed-AU chunk cutting (PW6 — latency plan §T3.4, wave-2 plan PW6)
// ---------------------------------------------------------------------------

/// Default per-chunk target — ~3–4 chunks for a 400 Mb/s 60 fps AU (~833 KB). Deliberately coarse,
/// because the SEALER, not this size, sets how early bytes actually leave:
///
/// * Toward a plain `VIDEO_CAP_STREAMED_AU` client, `Packetizer::push_streamed` flushes only when
///   its pending buffer exceeds one FEC block — `fec.max_data_per_block × shard_payload`, which is
///   200 × 1408 = 281 600 B on the shipped 1500-MTU IPv4 geometry. Anything smaller than that is
///   simply buffered. (256 KiB sits just under one block, so the first flush lands on the SECOND
///   chunk; the win is intact either way — the whole-AU path seals all ~3 blocks before its first
///   datagram may leave.) Only a client that ALSO negotiated `VIDEO_CAP_MULTI_SLICE` gets the
///   finer `MIN_STREAM_BLOCK_SHARDS` floor (16 shards ≈ 22 KB), where the chunk size does set the
///   flush granularity directly. pf-encode is not told the session's FEC geometry, so this is a
///   fixed byte target rather than a block-derived one.
/// * Chunks are not free: the send thread paces each sealed batch on its own
///   (`stream.rs::pace_sealed`), and every call grants a fresh `max(bytes/4, 128 KiB)` microburst
///   allowance. Cutting an AU into dozens of chunks therefore erodes the pacing this host does to
///   stop line-rate bursts from overrunning the NIC — the failure mode the pacer exists for.
const STREAM_CHUNK_TARGET_BYTES: usize = 256 * 1024;
/// Clamp on the `PUNKTFUNK_PYROWAVE_CHUNK_KIB` override (see [`stream_chunk_step`]).
const STREAM_CHUNK_MIN_KIB: usize = 4;
const STREAM_CHUNK_MAX_KIB: usize = 8192;

/// Whether streamed-AU output is armed for this host process.
///
/// **Default OFF, and deliberately so.** The streamed wire shape costs one PyroWave-specific
/// regression that has not been measured: an UNPINNED streamed frame (its final block never
/// arrived, so `frame_bytes` is still the 0 sentinel) is excluded from partial delivery
/// (`reassemble.rs`, 2026-07 security-review finding 10) — where today's whole-AU path hands the
/// consumer a usable blurred partial, a streamed frame that loses its final block delivers
/// NOTHING. PyroWave clients opt into partial delivery unconditionally
/// (`client/pump/handshake.rs`), so this is a live behaviour change for every one of them. The
/// netem loss-harness leg (2 % on `lo`, FEC pinned off — the Phase-4 recipe) comparing
/// partial-delivery rates streamed vs whole-AU is the prerequisite for flipping the default;
/// until it has run, `PUNKTFUNK_PYROWAVE_STREAMED_AU=1` is how you get it.
///
/// The client's `VIDEO_CAP_STREAMED_AU` and the host's `PUNKTFUNK_STREAMED_AU` remain the outer
/// gates (`stream.rs`) — this only decides whether the ENCODER offers chunks at all.
fn stream_armed() -> bool {
    static ARMED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Latched once: `supports_chunked_poll` is re-queried per AU, and a knob that could change
    // mid-session would flip the wire shape under an open `StreamedAu`.
    *ARMED.get_or_init(|| {
        matches!(
            std::env::var("PUNKTFUNK_PYROWAVE_STREAMED_AU").as_deref(),
            Ok("1")
        )
    })
}

/// Bytes per streamed chunk, rounded DOWN to a whole number of `window`-sized windows (never
/// below one). The rounding is the whole point — see [`AuChunker`].
fn chunk_step(window: usize, target: usize) -> usize {
    (target / window.max(1)).max(1) * window.max(1)
}

/// The streamed-AU chunk size for a backend whose wire chunking is `wire_chunk`, or `None` when
/// this session must stay on the whole-AU path — which is the answer whenever the feature is not
/// armed ([`stream_armed`]) or the encoder is in DENSE mode.
///
/// Dense mode is excluded on purpose: there the AU is ONE atomic pyrowave packet with no window
/// framing, so a cut is neither shard-aligned nor a framing boundary. Every real PyroWave session
/// runs datagram-aligned (`stream.rs` sets `plan.wire_chunk = Some(session.shard_payload())`), so
/// nothing is lost — but the invariant this file promises stays true instead of nearly true.
///
/// `PUNKTFUNK_PYROWAVE_CHUNK_KIB` overrides the target (clamped to
/// [`STREAM_CHUNK_MIN_KIB`]..=[`STREAM_CHUNK_MAX_KIB`]); garbage falls back to the default.
pub(crate) fn stream_chunk_step(wire_chunk: Option<usize>) -> Option<usize> {
    let window = wire_chunk.filter(|&w| w > 0)?;
    if !stream_armed() {
        return None;
    }
    static TARGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let target = *TARGET.get_or_init(|| {
        std::env::var("PUNKTFUNK_PYROWAVE_CHUNK_KIB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|k| (STREAM_CHUNK_MIN_KIB..=STREAM_CHUNK_MAX_KIB).contains(k))
            .map(|k| k * 1024)
            .unwrap_or(STREAM_CHUNK_TARGET_BYTES)
    });
    Some(chunk_step(window, target))
}

/// Hands a **finished** datagram-aligned AU out in window-aligned pieces for the streamed-AU wire
/// ([`crate::Encoder::poll_chunk`], `punktfunk_core::quic::VIDEO_CAP_STREAMED_AU`). Shared by both
/// pyrowave backends so the cut rule cannot drift between Linux and Windows — the Windows backend
/// cannot even be compiled from a Linux/macOS dev box, so logic written into it directly ships
/// unverified.
///
/// ## What this does NOT buy (read before quoting PW6 as a latency win)
///
/// pyrowave's `encode_frame` is **synchronous**: `submit` returns only once the whole AU sits in
/// `pending`, so by the time the host can poll a chunk the encode is over. `poll_chunk` is
/// therefore NOT "emit slices as the encoder produces them" — it is "hand the finished AU out in
/// pieces so the wire work pipelines with itself". Concretely, what moves:
///
/// * whole-AU path: `Session::seal_frame_at` FEC-protects, packetizes and AEAD-seals the ENTIRE
///   ~830 KB AU before its first datagram may leave the socket;
/// * streamed path: each FEC block seals and paces as it completes, so the first byte reaches the
///   wire after one block's seal, and the remaining seal work overlaps its own transmission.
///
/// There is NO encode/send overlap here — unlike the H.26x sub-frame slice path, where chunks
/// genuinely appear while the encoder is still working. PW6 and PW5 (encode overlap) are
/// independent packages, not sequential ones.
///
/// It also does **not** give the client decode-while-arriving: the reassembler completes a
/// streamed AU exactly like a whole one (`reassemble.rs` — `block_count != 0 && blocks_ok ==
/// block_count`) and hands up ONE `Frame`. Client-side prefix decode is the separate
/// `Session::set_deliver_frame_parts` opt-in, which PyroWave's newest-wins frame channel cannot
/// take — see the PW6 section of `design/linux-host-performance-wave2-pyrowave.md`.
///
/// ## The cut rule
///
/// A chunk is a whole number of `chunk`-sized WINDOWS. [`build_au`] gives every window exactly ONE
/// `kind` in its 4-byte prefix (`WIN_PACKED` or one link of a `WIN_FRAG_*` chain), so a cut inside
/// a window would split a unit the clients parse atomically. Whole windows are `shard_payload`
/// multiples by construction, which is what makes the sealer's sentinel block bases shard-aligned
/// for free (plan §4.4) — the streamed path's placement contract.
pub(crate) struct AuChunker {
    au: Vec<u8>,
    /// Bytes already handed out.
    cursor: usize,
    /// Bytes per chunk — a whole number of windows ([`chunk_step`]).
    step: usize,
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
    chunk_aligned: bool,
    /// Set once anything has been emitted, so the degenerate EMPTY AU still owes exactly one
    /// chunk and not an infinite stream of them.
    emitted: bool,
}

impl AuChunker {
    pub(crate) fn new(frame: crate::EncodedFrame, step: usize) -> AuChunker {
        AuChunker {
            au: frame.data,
            cursor: 0,
            step: step.max(1),
            pts_ns: frame.pts_ns,
            keyframe: frame.keyframe,
            recovery_anchor: frame.recovery_anchor,
            chunk_aligned: frame.chunk_aligned,
            emitted: false,
        }
    }

    /// The next piece, or `None` once the AU is spent. The pieces concatenate to exactly the bytes
    /// [`crate::Encoder::poll`] would have returned; `first` opens the wire frame and `last` closes
    /// it (the host's `handle_chunk` keys its `begin`/`finish` off precisely those two).
    pub(crate) fn next(&mut self) -> Option<crate::AuChunk> {
        if self.cursor >= self.au.len() {
            // A zero-byte AU is not reachable through `build_au` (it always emits at least one
            // window), but the host would leak its open `StreamedAu` if a chunked poll returned
            // nothing at all — so the degenerate case still owes one self-closing chunk.
            if self.emitted {
                return None;
            }
            self.emitted = true;
            return Some(self.chunk(Vec::new(), true, true));
        }
        let first = self.cursor == 0;
        let end = (self.cursor + self.step).min(self.au.len());
        let data = self.au[self.cursor..end].to_vec();
        self.cursor = end;
        self.emitted = true;
        Some(self.chunk(data, first, end == self.au.len()))
    }

    /// AU-level metadata rides every chunk (the `AuChunk` contract only makes it authoritative on
    /// `first`, but a truthful copy on each one costs nothing and keeps a mid-AU log honest).
    fn chunk(&self, data: Vec<u8>, first: bool, last: bool) -> crate::AuChunk {
        crate::AuChunk {
            data,
            pts_ns: self.pts_ns,
            keyframe: self.keyframe,
            recovery_anchor: self.recovery_anchor,
            chunk_aligned: self.chunk_aligned,
            first,
            last,
        }
    }
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
    fn block_count_matches_the_apple_layout_invariant() {
        // 256x144 (the golden-fixture geometry, aligned 256x160): recompute via the same walk
        // the validated Apple WaveletLayout uses and pin a few mode-level facts.
        let manual = |w: u32, h: u32, c444: bool| {
            let align = |v: u32| ((v + 31) & !31).max(128);
            let (aw, ah) = (align(w), align(h));
            let mut n = 0u32;
            for level in (0..5u32).rev() {
                let per = (((aw / 2) >> level).div_ceil(8).div_ceil(4))
                    * (((ah / 2) >> level).div_ceil(8).div_ceil(4));
                let bands = if level == 4 { 4 } else { 3 };
                for c in 0..3 {
                    if level == 0 && c != 0 && !c444 {
                        continue;
                    }
                    n += per * bands;
                }
            }
            n
        };
        for (w, h) in [(256, 144), (1920, 1080), (3840, 2160), (7680, 4320)] {
            assert_eq!(block_count_32x32(w, h, false), manual(w, h, false));
            assert_eq!(block_count_32x32(w, h, true), manual(w, h, true));
        }
        // 4:4:4 fits comfortably at 4K; the 16-bit RDO block index wraps around 8K 4:4:4.
        assert!(block_count_32x32(3840, 2160, true) <= u16::MAX as u32);
        assert!(block_count_32x32(7680, 4320, true) > u16::MAX as u32);
        assert!(block_count_32x32(7680, 4320, false) <= u16::MAX as u32);
        // …and 4:2:0 wraps it too, just later — the hole the old 4:4:4-only open guard left.
        // `Codec::max_dimension()` allows PyroWave 8192px per axis, so these modes were
        // reachable from a client-requested `mode=WxHxFPS`, and the negotiator's 4:4:4 → 4:2:0
        // downgrade routed oversized modes straight into the unguarded branch.
        // `validate_dimensions` now rejects them against this 4:2:0 count.
        assert_eq!(block_count_32x32(8192, 6144, false), 73728);
        assert_eq!(block_count_32x32(8192, 8192, false), 98304);
        assert!(block_count_32x32(8192, 6144, false) > u16::MAX as u32);
        assert!(block_count_32x32(8192, 8192, false) > u16::MAX as u32);
        // The largest 4:2:0 mode that still fits, for the boundary the validator enforces.
        assert!(block_count_32x32(7680, 4320, false) <= u16::MAX as u32);
    }

    /// The wire-budget tracker: converges its EMA onto the measured AU/bitstream inflation,
    /// deflates the budget by exactly that ratio, and clamps runaway samples.
    #[test]
    fn wire_budget_converges_and_deflates() {
        let mut wb = WireBudget::new();
        // Prior ≈ ×1.25 (1280/1024): the first deflation is already conservative.
        assert_eq!(wb.deflate(1_024_000), 819_200);
        // Feed a steady ×1.30 inflation; the EMA must converge onto it.
        for _ in 0..64 {
            wb.observe(1000, 1300);
        }
        let b = wb.deflate(1_024_000);
        let expect = 1_024_000_u64 * 1000 / 1300;
        assert!(
            (b as i64 - expect as i64).unsigned_abs() < 8_000,
            "budget {b} should approach {expect}"
        );
        // A dense-ish run (×1.0) walks it back down to no deflation.
        for _ in 0..64 {
            wb.observe(1000, 1000);
        }
        assert_eq!(wb.deflate(1_024_000), 1_024_000);
        // Garbage samples are clamped: an absurd ratio can at most halve the budget…
        for _ in 0..256 {
            wb.observe(10, 1000);
        }
        assert!(wb.deflate(1_024_000) >= 512_000);
        // …and a zero-length observation is ignored, never a division by zero.
        wb.observe(0, 1000);
    }

    #[test]
    fn stamp_color_bits_sets_range_and_hdr_bits() {
        let mut bs = vec![0u8; 16];
        stamp_color_bits(&mut bs, 0, false);
        // ycbcr_range = bit 30 of the LE second word = bit 6 of byte 7 (0x40); nothing else touched.
        assert_eq!(bs[7], 0x40);
        assert!(bs[..7].iter().all(|&b| b == 0));
        assert!(bs[8..].iter().all(|&b| b == 0));
        // Idempotent; an out-of-range offset is a silent no-op (never panics).
        stamp_color_bits(&mut bs, 0, false);
        assert_eq!(bs[7], 0x40);
        stamp_color_bits(&mut bs, 100, false);
        // HDR adds BT.2020 primaries (0x08) + PQ transfer (0x10) + BT.2020 matrix (0x20);
        // chroma_siting (0x80) stays CENTER.
        stamp_color_bits(&mut bs, 0, true);
        assert_eq!(bs[7], 0x78);
    }

    // --- streamed-AU chunk cutting (PW6) ------------------------------------
    // Appended at module END per the wave plan's ownership rule.

    fn frame(data: Vec<u8>) -> crate::EncodedFrame {
        crate::EncodedFrame {
            data,
            pts_ns: 1_234_567,
            keyframe: true,
            recovery_anchor: false,
            chunk_aligned: true,
        }
    }

    /// Drain a chunker into `(concatenated bytes, per-chunk lengths, first flags, last flags)`.
    fn drain(mut c: AuChunker) -> (Vec<u8>, Vec<usize>, Vec<bool>, Vec<bool>) {
        let (mut bytes, mut lens, mut firsts, mut lasts) = (Vec::new(), Vec::new(), vec![], vec![]);
        while let Some(ch) = c.next() {
            lens.push(ch.data.len());
            firsts.push(ch.first);
            lasts.push(ch.last);
            bytes.extend_from_slice(&ch.data);
            assert_eq!(ch.pts_ns, 1_234_567, "AU metadata rides every chunk");
            assert!(ch.keyframe && ch.chunk_aligned && !ch.recovery_anchor);
        }
        (bytes, lens, firsts, lasts)
    }

    /// The invariant PW6 rests on: chunks concatenate to EXACTLY the AU, every cut lands on a
    /// whole-window boundary (so no window's single `kind` is split across two wire frames), and
    /// the reassembled stream still walks back to the same codec packets. A cut inside a window
    /// would hand the client a 4-byte prefix whose body arrives in a different chunk — the
    /// framing is one-kind-per-window, so there is no way to express that.
    #[test]
    fn stream_chunks_tile_the_au_on_window_boundaries() {
        let bs: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let packets = [(0, 20), (20, 300), (320, 55), (375, 900), (1275, 40)];
        let chunk = 64;
        let au = build_au(&packets, &bs, Some(chunk));
        assert!(au.len() / chunk > 4, "need several windows to cut between");
        let step = chunk_step(chunk, 3 * chunk);
        assert_eq!(step, 3 * chunk);
        let (bytes, lens, firsts, lasts) = drain(AuChunker::new(frame(au.clone()), step));
        assert_eq!(bytes, au, "chunks concatenate to exactly the AU");
        assert!(
            lens.iter().all(|l| l % chunk == 0),
            "every chunk is a whole number of windows: {lens:?}"
        );
        assert!(
            lens[..lens.len() - 1].iter().all(|&l| l == step),
            "only the tail chunk may be short: {lens:?}"
        );
        assert_eq!(
            firsts,
            (0..lens.len()).map(|i| i == 0).collect::<Vec<_>>(),
            "exactly one opening chunk"
        );
        assert_eq!(
            lasts,
            (0..lens.len())
                .map(|i| i + 1 == lens.len())
                .collect::<Vec<_>>(),
            "exactly one closing chunk"
        );
        // And the client's parse is unchanged by the cutting.
        let mut expect = Vec::new();
        for &(o, s) in &packets {
            expect.extend_from_slice(&bs[o..o + s]);
        }
        assert_eq!(walk(&bytes, chunk), expect);
    }

    /// The step always rounds DOWN to whole windows and never to zero — a target below one window
    /// degenerates to one window per chunk rather than an empty chunk (which would spin forever).
    #[test]
    fn chunk_step_rounds_down_to_whole_windows() {
        // 262144 / 1408 = 186.2 → 186 whole windows (261 888 B), never the 262 144 asked for.
        assert_eq!(chunk_step(1408, 256 * 1024), 186 * 1408);
        assert_eq!(chunk_step(1408, 1408), 1408);
        assert_eq!(chunk_step(1408, 1407), 1408); // below one window → one window
        assert_eq!(chunk_step(1408, 0), 1408);
        assert_eq!(chunk_step(0, 4096), 4096); // defensive: never divides by zero
    }

    /// An AU that fits one chunk is a single `first && last` piece — the shape the host's
    /// `handle_chunk` turns into begin+finish on one message, and byte-identical on the wire to
    /// what the whole-AU path would have sealed.
    #[test]
    fn single_chunk_au_opens_and_closes_itself() {
        let au = vec![7u8; 512];
        let (bytes, lens, firsts, lasts) = drain(AuChunker::new(frame(au.clone()), 4096));
        assert_eq!(bytes, au);
        assert_eq!(lens, vec![512]);
        assert_eq!(firsts, vec![true]);
        assert_eq!(lasts, vec![true]);
    }

    /// The degenerate empty AU still owes exactly ONE self-closing chunk: a chunked poll that
    /// returned nothing would leave the host's `StreamedAu` open forever (its `begin` fires on
    /// `first`, its `finish` on `last`).
    #[test]
    fn empty_au_still_emits_one_self_closing_chunk() {
        let mut c = AuChunker::new(frame(Vec::new()), 4096);
        let ch = c.next().expect("one chunk");
        assert!(ch.first && ch.last && ch.data.is_empty());
        assert!(c.next().is_none(), "and never a second one");
    }

    /// Dense (non-windowed) AUs never stream: there is no window framing to cut on, so a chunk
    /// boundary would be neither shard-aligned nor a parse boundary.
    #[test]
    fn dense_mode_never_streams() {
        assert!(stream_chunk_step(None).is_none());
        assert!(stream_chunk_step(Some(0)).is_none());
    }
}
