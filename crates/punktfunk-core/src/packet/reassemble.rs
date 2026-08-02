//! Client side: buffer incoming shards, FEC-recover lost ones, and emit whole access units.
//! The per-packet [`Reassembler::push`] hot path is kept whole (disjoint field borrows).

use super::*;
use crate::config::Config;
use crate::error::Result;
use crate::fec::ErasureCoder;
use crate::session::{Frame, FramePart};
use crate::stats::StatsCounters;
use std::collections::HashMap;
use zerocopy::FromBytes;

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
pub(super) const LOSS_WINDOW_NS: u64 = 120_000_000;

/// Hard cap on how many frame INDICES behind the newest an incomplete frame may sit, whatever its
/// pts claims — bounds the reassembler's memory against a corrupt/hostile pts (which
/// [`LOSS_WINDOW_NS`] alone would trust) and against pathologically high frame rates. At 120 fps,
/// 120 ms ≈ 14 indices, so 64 leaves ample slack up to ~500 fps.
const HARD_LOSS_WINDOW: u32 = 64;

/// The much tighter fuse for PARTIAL-deliverable frames (chunk-aligned AUs with a consumer
/// that opted in): once anything newer exists and this much capture time passed, the frame
/// is delivered as-is — its stragglers can only make it less late, and each frame is
/// independently decodable, so waiting the full loss window (120 ms) would inject ancient
/// frames into a live stream. ~2 frame periods at 60 fps rides out normal reorder.
const PARTIAL_WINDOW_NS: u64 = 30_000_000;

/// How many frames behind the newest the reassembler remembers emitted/abandoned frame indices
/// (`completed`), so a straggler shard can neither resurrect an abandoned frame nor re-open an
/// emitted one. Must cover at least [`HARD_LOSS_WINDOW`]: stragglers can trickle in later than the
/// loss verdict.
const REORDER_WINDOW: u32 = 64;

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
    /// The block's base offset in SHARD units — where its data shards land in the frame buffer.
    /// Uniform (legacy) frames: `block_index × max_data_shards`. Slice-streamed frames
    /// ([`USER_FLAG_SLICE_STREAM`]): sentinels carry it on the wire (`frame_bytes ÷
    /// shard_bytes`), the final block derives it from the totals
    /// (`total_data − final_data_shards`).
    base_shard: usize,
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
    /// Exact AU size. 0 = unknown: the frame was opened by a streamed-AU SENTINEL packet
    /// ([`crate::quic::VIDEO_CAP_STREAMED_AU`]) and the final block's real totals haven't
    /// arrived yet — the frame can't complete before they do (and retro-validate).
    frame_bytes: usize,
    /// Block count; 0 = unknown (sentinel-opened, totals not yet pinned). A legacy-opened
    /// frame always has ≥ 1 here, so 0 doubles as the "unpinned streamed" marker.
    block_count: usize,
    pts_ns: u64,
    user_flags: u32,
    /// Slice-progressive delivery cursor ([`Reassembler::deliver_parts`]): count of leading
    /// blocks already handed up as prefix parts...
    next_part_block: u16,
    /// ...and their total data shards — the AU shard offset where the next part starts.
    delivered_shards: usize,
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
        // Size the ceiling from the whole range adaptive FEC may reach, NOT from the percentage
        // negotiated at session start: the sender moves `fec_percent` live (`Packetizer::
        // set_fec_percent`, clamped to ≤ 90) and the wire is self-describing, so it never
        // renegotiates. Deriving this from the start value made every packet of a large block
        // fail the `total > max_total_shards` check once FEC ramped up — the block never
        // accumulated a shard, the frame aged out, and the resulting loss drove FEC *higher*,
        // wedging large frames at 100% loss exactly when FEC was meant to rescue the link. A
        // current sender also clamps its side (`Packetizer::recovery_for`); this keeps an
        // already-deployed sender that doesn't from wedging a current receiver. Still a hard
        // pre-allocation bound against hostile headers — just the sender's clamp, not a stale
        // snapshot of it.
        let max_total =
            (max_data + (max_data * 90).div_ceil(100)).min(c.fec.scheme.max_total_shards());
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
    /// Deliver aged-out incomplete frames whose AUs are [`USER_FLAG_CHUNK_ALIGNED`] instead of
    /// silently dropping them (client opt-in — the PyroWave decode path): the frame buffer is
    /// already the right shape (received shards at their final offsets, zeros elsewhere).
    /// They still count into `frames_dropped` — a partial IS lost data for the loss reports.
    deliver_partial: bool,
    /// The newest such partial awaiting pickup (newest-wins: partials are a lossy byproduct).
    pending_partial: Option<Frame>,
    /// Deliver each video AU's newly-contiguous PREFIX as [`Frame`]s with
    /// [`Frame::part`]`= Some` while the rest is still in flight (client opt-in — the
    /// slice-progressive decode path). Works for any multi-block frame — both streamed shapes
    /// and legacy uniform frames tile their blocks contiguously (`BlockState::base_shard`).
    deliver_parts: bool,
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
            deliver_partial: false,
            pending_partial: None,
            deliver_parts: false,
            video: ReassemblyWindow::default(),
            probe: ReassemblyWindow::default(),
            recovery_pool: Vec::new(),
            in_flight_bytes: 0,
        }
    }

    /// Opt into partial delivery of chunk-aligned frames (see [`Reassembler::deliver_partial`]).
    pub fn set_deliver_partial(&mut self, on: bool) {
        self.deliver_partial = on;
        if !on {
            self.pending_partial = None;
        }
    }

    /// Opt into slice-progressive prefix delivery (see [`Reassembler::deliver_parts`]).
    pub fn set_deliver_parts(&mut self, on: bool) {
        self.deliver_parts = on;
    }

    /// Take the newest aged-out partial frame, if one is pending (see `set_deliver_partial`).
    pub fn take_partial(&mut self) -> Option<Frame> {
        self.pending_partial.take()
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
            deliver_partial,
            pending_partial,
            deliver_parts,
            video,
            probe,
            recovery_pool,
            in_flight_bytes,
        } = self;
        let deliver_parts = *deliver_parts;
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
            || frame_bytes > lim.max_frame_bytes
        {
            drop(stats);
            return Ok(None);
        }
        // Streamed-AU sentinel ([`crate::quic::VIDEO_CAP_STREAMED_AU`]): `block_count == 0` — a
        // value no legacy sender ever emits — marks a NON-FINAL block of an AU whose total size
        // doesn't exist yet (the host is still encoding its tail). The exact derived-geometry
        // check below can't run without a total, so a sentinel is bounded by the negotiated
        // limits instead — and by construction: full-K exactly (the offset formula needs it),
        // never the last block the limits allow (the real final block must still fit after it),
        // and no total to lie about. The frame-wide exact check runs retroactively the moment
        // the final block's real totals arrive (the pinning arm below).
        let sentinel = block_count == 0;
        // Slice-streamed frames (the P2 pipeline): variable-size, base-addressed blocks — the
        // uniform-geometry rules below don't apply to ANY of their blocks (see
        // [`USER_FLAG_SLICE_STREAM`]). Flagged on every packet because reorder can deliver the
        // final block first.
        let slice_stream = hdr.user_flags & crate::packet::USER_FLAG_SLICE_STREAM != 0;
        let block_idx = hdr.block_index as usize;
        // For a sentinel-opened frame the buffer must hold ANY final geometry the totals may
        // later pin — the maximum the negotiated limits allow (the design's "allocate at
        // max_frame_bytes"; the existing in-flight budget bounds the amplification).
        let total_data_max = lim.max_frame_bytes.div_ceil(shard_bytes).max(1);
        // The slice pipeline's per-frame block ceiling: every non-final slice block carries at
        // least `min(MIN_STREAM_BLOCK_SHARDS, max_data_per_block)` data shards (the sender's
        // flush floor, clamped by the block size), so a max-size frame bounds the block count
        // (+ the final block and one rounding block). Mirrors `Packetizer::slice_block_cap`.
        let slice_block_cap = total_data_max
            / super::packetize::MIN_STREAM_BLOCK_SHARDS.min(lim.max_data_shards.max(1))
            + 2;
        let total_data = frame_bytes.div_ceil(shard_bytes).max(1);
        if sentinel && slice_stream {
            // Slice-granularity sentinel: `frame_bytes` is the block's BASE byte offset —
            // shard-aligned by contract, and the block's whole range must fit the negotiated
            // frame budget (placement re-checks against the live buffer too).
            if frame_bytes % shard_bytes != 0
                || frame_bytes + data_shards * shard_bytes > lim.max_frame_bytes
                || block_idx + 1 >= slice_block_cap
            {
                drop(stats);
                return Ok(None);
            }
        } else if sentinel {
            if frame_bytes != 0
                || data_shards != lim.max_data_shards
                || block_idx + 1 >= lim.max_blocks
            {
                drop(stats);
                return Ok(None);
            }
        } else {
            let block_cap = if slice_stream {
                slice_block_cap
            } else {
                lim.max_blocks
            };
            if block_count > block_cap || block_idx >= block_count {
                drop(stats);
                return Ok(None);
            }
            if slice_stream {
                // Slice-streamed frames: the earlier blocks are variable-size, so the uniform
                // derivation below is meaningless — only the FINAL block is ever non-sentinel,
                // and its geometry must be self-consistent: it IS the last block, its K fits
                // the negotiated block size, and its shards fit inside the frame (its base
                // derives as `total_data − data_shards`).
                if block_idx + 1 != block_count
                    || data_shards > lim.max_data_shards
                    || data_shards > total_data
                {
                    drop(stats);
                    return Ok(None);
                }
            } else {
                // Derived-geometry firewall: every sender (our Packetizer, any version) slices
                // a frame into consecutive blocks of exactly `max_data_per_block` data shards
                // with only the LAST block smaller, and stamps the exact `frame_bytes` in every
                // non-sentinel header. That makes every data shard's final AU offset computable
                // on arrival —
                //     offset = (block_index × max_data_per_block + shard_index) × shard_bytes
                // — which is what lets shards land straight in the frame buffer below. Enforce
                // the invariant so a header lying about its geometry is dropped instead of
                // scribbling into another shard's range.
                let expect_blocks = total_data.div_ceil(lim.max_data_shards).max(1);
                let expect_data_shards = if block_idx + 1 == expect_blocks {
                    total_data - (expect_blocks - 1) * lim.max_data_shards
                } else {
                    lim.max_data_shards
                };
                if block_count != expect_blocks || data_shards != expect_data_shards {
                    drop(stats);
                    return Ok(None);
                }
            }
        }
        let body = &pkt[HEADER_LEN..HEADER_LEN + shard_bytes];

        // Route by index space: speed-test probe filler (FLAG_PROBE in user_flags) reassembles in
        // its own window so its indexes never interact with the video loss window — a probe burst
        // can neither advance the video anchor nor be dropped as stale against it (and its aged-out
        // frames never count as `frames_dropped`, which would fire video loss recovery).
        let is_probe = hdr.user_flags & (FLAG_PROBE as u32) != 0;
        if is_probe {
            // Probe-scoped receive accounting (the speed-test numerator + denominator, see
            // `Stats::probe_first_arrival_ns`), stamped at the routing decision so video in
            // flight around the burst contaminates neither the byte count nor the arrival
            // stamps. Byte unit mirrors `bytes_received` (whole plaintext packet). The first
            // probe packet since the pump armed the probe claims the first-arrival slot (the
            // pump zeroes it before the burst can reach the host); every probe packet
            // refreshes the last-arrival stamp.
            let now_ns = crate::stats::now_monotonic_ns();
            StatsCounters::add(&stats.probe_packets_received, 1);
            StatsCounters::add(&stats.probe_bytes_received, pkt.len() as u64);
            let _ = stats.probe_first_arrival_ns.compare_exchange(
                0,
                now_ns,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
            stats
                .probe_last_arrival_ns
                .store(now_ns, std::sync::atomic::Ordering::Relaxed);
        } else if hdr.shard_index < hdr.data_shards {
            // Media accounting (see `Stats::media_bytes_received`): DATA shards only, payload
            // only. Stamped at the same routing decision as the probe counters and for the same
            // reason — the adaptive-bitrate utilization gate compares delivered throughput
            // against an ENCODER target, so parity, headers and probe filler have no business
            // in the numerator.
            StatsCounters::add(&stats.media_bytes_received, shard_bytes as u64);
        }
        let win = if is_probe { probe } else { video };
        win.advance_window(
            hdr.frame_index,
            hdr.pts_ns,
            stats,
            !is_probe,
            recovery_pool,
            in_flight_bytes,
            lim.max_data_shards,
            (*deliver_partial && !is_probe).then_some(pending_partial),
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

        // How many shards of frame buffer THIS packet proves the frame needs. A sentinel carries
        // no total, but it does pin its own block's extent — a slice sentinel by its wire base,
        // a legacy one by its full-K position — and that is what the buffer must cover to place
        // the shard. The frame grows as later blocks reveal more, and the final (non-sentinel)
        // block's totals settle it.
        //
        // ⚠ NOT `total_data_max` (= the negotiated `max_frame_bytes`, 8-64 MiB): that shape
        // shipped in 0.23.0 and was survivable only while sentinels were rare — the streamed
        // path emitted one solely for an AU exceeding a whole FEC block (~281 KB). The slice
        // wire flushes at `MIN_STREAM_BLOCK_SHARDS`, so EVERY ordinary AU became sentinel-opened
        // and every one of them committed the full ceiling: a multi-megabyte zeroed allocation
        // per access unit, and an in-flight budget (`IN_FLIGHT_BUF_FACTOR × max_frame_bytes`)
        // exhausted after ~3 concurrent frames — beyond which every packet of every further
        // frame was dropped outright. On a jittery link that is a permanent loss storm.
        let need_shards = if sentinel && slice_stream {
            frame_bytes / shard_bytes + data_shards
        } else if sentinel {
            // Legacy sentinels are full-K uniform blocks (firewall-enforced), so the block's
            // index alone gives its end.
            (block_idx + 1).saturating_mul(lim.max_data_shards)
        } else {
            total_data
        }
        .min(total_data_max);
        // First packet of a frame allocates its (zeroed) buffer, budget-gated; later packets must
        // agree with its geometry.
        let buf_len = need_shards * shard_bytes;
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
                    // A slice-stream sentinel's `frame_bytes` is its block's BASE offset, not a
                    // frame size — the unpinned marker stays 0 until the final block's totals.
                    frame_bytes: if sentinel { 0 } else { frame_bytes },
                    block_count,
                    pts_ns: hdr.pts_ns,
                    user_flags: hdr.user_flags,
                    next_part_block: 0,
                    delivered_shards: 0,
                    buf: vec![0; buf_len],
                    blocks: HashMap::new(),
                    blocks_ok: 0,
                })
            }
        };
        // The slice marker must be frame-consistent: a mixed frame would firewall under one
        // placement rule and place under the other. The per-packet checks above and the
        // placement bounds guard below stay memory-safe without this — it's the tighter drop.
        if (frame.user_flags ^ hdr.user_flags) & crate::packet::USER_FLAG_SLICE_STREAM != 0 {
            drop(stats);
            return Ok(None);
        }
        if sentinel {
            // Sentinel packets carry no totals to cross-check: while the frame is unpinned
            // (`block_count == 0`) they match by construction, and once the totals are pinned —
            // whichever packet arrived first, sentinel or final (reorder is normal) — a
            // sentinel block must still be NON-final under them. Its full-K shape was already
            // enforced by the firewall, which is exactly what the pinned geometry demands of
            // every non-final block, and its shard offsets are within the pinned range by
            // `block_idx + 1 < block_count`.
            if frame.block_count != 0 {
                if block_idx + 1 >= frame.block_count {
                    drop(stats);
                    return Ok(None);
                }
                if slice_stream {
                    // The final block pinned the totals, so it exists in `blocks` (the pinning
                    // packet created it) — a sentinel arriving after the pin must fit strictly
                    // below its base or it could scribble over the final block's landed shards.
                    let final_k = frame
                        .blocks
                        .get(&((frame.block_count - 1) as u16))
                        .map(|b| b.data_shards)
                        .unwrap_or(0);
                    let pinned_total = frame.frame_bytes.div_ceil(shard_bytes).max(1);
                    if frame_bytes / shard_bytes + data_shards > pinned_total - final_k {
                        drop(stats);
                        return Ok(None);
                    }
                }
            }
        } else if frame.block_count == 0 {
            // A streamed frame meets its FINAL block's real totals: retro-validate every block
            // the sentinels created against the exact geometry these totals derive (the same
            // invariant the firewall enforces per-packet for legacy frames), then PIN them. A
            // header that lies — totals under which an already-received sentinel block is
            // out of range or not full-K — kills the WHOLE frame: its landed shards can't be
            // trusted to be at the offsets this geometry means, so delivering any of it would
            // hand the decoder spliced bytes.
            let lied = if slice_stream {
                // Slice frames have no uniform shape to demand — the invariant is positional:
                // every sentinel block must sit strictly below the final block's base
                // (`total_data − data_shards`; the firewall already proved the subtraction
                // safe) and be a non-final index. Sentinel-vs-sentinel overlap is not policed —
                // the sender is AEAD-authenticated, so a lying base can only corrupt this
                // frame's own pixels, never memory (placement stays in-bounds by these checks).
                let final_base = total_data - data_shards;
                frame.blocks.iter().any(|(&bi, b)| {
                    let bi = bi as usize;
                    bi + 1 >= block_count || b.base_shard + b.data_shards > final_base
                })
            } else {
                let expect_blocks = total_data.div_ceil(lim.max_data_shards).max(1);
                let final_k = total_data - (expect_blocks - 1) * lim.max_data_shards;
                frame.blocks.iter().any(|(&bi, b)| {
                    let bi = bi as usize;
                    bi >= expect_blocks
                        || (bi + 1 < expect_blocks && b.data_shards != lim.max_data_shards)
                        || (bi + 1 == expect_blocks && b.data_shards != final_k)
                })
            };
            if lied {
                let mut f = win
                    .frames
                    .remove(&hdr.frame_index)
                    .expect("frame entry exists");
                *in_flight_bytes -= f.buf.len();
                // Remember the index (with its late-shard memory, exactly like an aged-out
                // frame) so stragglers can't resurrect it, reclaim the parity buffers, and
                // count the loss — the client's recovery request is the right outcome for a
                // frame that was destroyed by a lying header.
                win.completed.insert(
                    hdr.frame_index,
                    reconstructed_shards(&f.blocks, lim.max_data_shards),
                );
                for block in f.blocks.values_mut() {
                    for slot in block.recovery.iter_mut() {
                        if let Some(rb) = slot.take() {
                            if recovery_pool.len() < RECOVERY_POOL_MAX {
                                recovery_pool.push(rb);
                            }
                        }
                    }
                }
                if !is_probe {
                    StatsCounters::add(&stats.frames_dropped, 1);
                }
                drop(stats);
                return Ok(None);
            }
            frame.frame_bytes = frame_bytes;
            frame.block_count = block_count;
        } else if frame.block_count != block_count || frame.frame_bytes != frame_bytes {
            drop(stats);
            return Ok(None);
        }
        // Grow to this packet's proven extent. A streamed frame opens at whichever block arrived
        // first and learns its real size from the final block's totals (or a later, higher
        // sentinel base) — reorder means either can come first, so the buffer is sized by
        // whatever the frame has proven so far. Never shrinks: the totals only settle the frame's
        // END, and completion truncates to `frame_bytes` anyway. The budget is re-checked here
        // for exactly the reason it is checked at open — growth commits memory too.
        if buf_len > frame.buf.len() {
            let delta = buf_len - frame.buf.len();
            if *in_flight_bytes + delta > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                drop(stats);
                return Ok(None);
            }
            *in_flight_bytes += delta;
            frame.buf.resize(buf_len, 0);
        }
        let FrameBuf {
            buf,
            blocks,
            blocks_ok,
            block_count: frame_block_count,
            pts_ns: frame_pts_ns,
            user_flags: frame_user_flags,
            next_part_block,
            delivered_shards,
            ..
        } = frame;
        let (frame_pts_ns, frame_user_flags) = (*frame_pts_ns, *frame_user_flags);

        // First packet of a block sizes its state; `data_shards` is already pinned by the
        // derived geometry above, but `recovery_shards` is per-block wire input (adaptive FEC
        // varies it per frame) — later packets must match the block's first.
        let base_shard = if slice_stream {
            if sentinel {
                frame_bytes / shard_bytes // the sentinel's wire base (firewall: shard-aligned)
            } else {
                total_data - data_shards // the final block sits at the end of the frame
            }
        } else {
            block_idx * lim.max_data_shards
        };
        let block = blocks.entry(hdr.block_index).or_insert_with(|| BlockState {
            data_shards,
            recovery_shards,
            base_shard,
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
        // A slice-stream block's base is per-block wire input like `recovery_shards` — later
        // packets must agree with the block's first, or two packets of "one" block would place
        // their shards at different frame offsets.
        if block.base_shard != base_shard {
            drop(stats);
            return Ok(None);
        }
        // Defense-in-depth (2026-07 security review): the geometry invariants above guarantee
        // every packet of a block agrees on K, so this can't fire today — but `have_data`
        // indexing and the recovery-slot math below assume it, and an explicit check keeps a
        // future firewall refactor from turning that assumption into an OOB panic.
        if block.data_shards != data_shards {
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
                let off = (block.base_shard + shard_index) * shard_bytes;
                // The firewall + pin checks keep every accepted base in range; this guard is
                // the same future-refactor insurance as the `data_shards` check above.
                if off + shard_bytes > buf.len() {
                    drop(stats);
                    return Ok(None);
                }
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
                let base = block.base_shard * shard_bytes;
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

        // Whole frame ready? Judged against the FRAME's pinned block count, not this packet's
        // header — a streamed frame can complete on a reordered sentinel packet (header says 0)
        // after the final block already pinned the real totals, and can never complete before
        // they're pinned (`0` never equals a non-zero `blocks_ok`).
        let block_count = *frame_block_count;
        if block_count != 0 && *blocks_ok == block_count {
            let mut done = win.frames.remove(&hdr.frame_index).unwrap();
            win.completed.insert(
                hdr.frame_index,
                reconstructed_shards(&done.blocks, lim.max_data_shards),
            );
            *in_flight_bytes -= done.buf.len();
            done.buf.truncate(done.frame_bytes); // trim trailing-shard zero padding
                                                 // Slice-progressive consumers already hold the delivered prefix — the completing
                                                 // packet hands up only the SUFFIX (with `last`), or the degenerate whole-AU part
                                                 // when nothing was delivered early. Probe filler stays whole either way — the
                                                 // speed test accounts AUs, not slices.
            let (data, part) = if deliver_parts && !is_probe {
                let lo = (done.delivered_shards * shard_bytes).min(done.frame_bytes);
                let part = FramePart {
                    offset: lo as u32,
                    first: lo == 0,
                    last: true,
                };
                if lo == 0 {
                    (done.buf, Some(part))
                } else {
                    (done.buf[lo..].to_vec(), Some(part))
                }
            } else {
                (done.buf, None)
            };
            return Ok(Some(Frame {
                data,
                frame_index: hdr.frame_index,
                pts_ns: done.pts_ns,
                flags: done.user_flags,
                complete: true,
                part,
                received_ns: 0, // stamped by Session::poll_frame at the session boundary
            }));
        }
        // Slice-progressive delivery: when this packet completed a block that extends the AU's
        // contiguous prefix (possibly unlocking blocks that finished out of order behind it),
        // hand the newly-contiguous bytes up as ONE part. Only successfully-completed blocks
        // count (`done` alone includes failed reconstructs), and the cursor stops short of the
        // FINAL block — its zero-padded tail is only trimmed at completion above.
        if deliver_parts && !is_probe {
            let start = *delivered_shards;
            while let Some(b) = blocks.get(&*next_part_block) {
                if !(b.done && (b.reconstructed || b.data_received == b.data_shards)) {
                    break;
                }
                if block_count != 0 && (*next_part_block as usize) + 1 >= block_count {
                    break;
                }
                *delivered_shards = b.base_shard + b.data_shards;
                *next_part_block += 1;
            }
            if *delivered_shards > start {
                let (lo, hi) = (start * shard_bytes, *delivered_shards * shard_bytes);
                return Ok(Some(Frame {
                    data: buf[lo..hi].to_vec(),
                    frame_index: hdr.frame_index,
                    pts_ns: frame_pts_ns,
                    flags: frame_user_flags,
                    complete: false,
                    part: Some(FramePart {
                        offset: lo as u32,
                        first: start == 0,
                        last: false,
                    }),
                    received_ns: 0, // stamped by Session::poll_frame at the session boundary
                }));
            }
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
        // An aged-out partial parked for delivery is from the discarded past too — without this
        // it survives `flush_backlog` and gets handed up as the first "frame" after the
        // jump-to-live, exactly the stale content the flush existed to discard.
        self.pending_partial = None;
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
        // `Some(sink)` = deliver aged-out CHUNK_ALIGNED frames instead of only dropping them.
        mut partial_sink: Option<&mut Option<Frame>>,
    ) {
        let (newest, newest_pts) = match self.newest_frame {
            // `frame_index` is newer iff it's within the forward half of the index space.
            Some((n, p)) if frame_index.wrapping_sub(n) > u32::MAX / 2 => (n, p),
            _ => (frame_index, pts_ns),
        };
        self.newest_frame = Some((newest, newest_pts));

        let before = self.frames.len();
        let completed = &mut self.completed;
        let partial_on = partial_sink.is_some();
        self.frames.retain(|&idx, f| {
            // Partial-deliverable frames age out on the TIGHT fuse (see PARTIAL_WINDOW_NS);
            // everything else keeps the full loss window.
            let window_ns = if partial_on && f.user_flags & USER_FLAG_CHUNK_ALIGNED != 0 {
                PARTIAL_WINDOW_NS
            } else {
                LOSS_WINDOW_NS
            };
            let keep = newest.wrapping_sub(idx) <= HARD_LOSS_WINDOW
                && newest_pts.saturating_sub(f.pts_ns) <= window_ns;
            if !keep {
                // Remember the abandoned index so a straggler shard is dropped (below, and in
                // `push`) instead of resurrecting the frame — which would re-allocate its buffers
                // and double-count the drop when it aged out again. Blocks that reconstructed
                // before the frame died still counted `fec_recovered_shards`, so their restored
                // shards join the late-shard memory exactly like an emitted frame's.
                completed.insert(idx, reconstructed_shards(&f.blocks, max_data_shards));
                // Release its buffer budget and reclaim its parity bufs for the pool.
                *in_flight_bytes -= f.buf.len();
                // Partial delivery (chunk-aligned AUs only): the buffer is already exactly
                // what the consumer needs — received shards at their final offsets, zeros
                // where shards are missing (the codec's block walk skips zero windows).
                // Newest-wins if several age out in one prune. Still counted dropped below.
                if let Some(sink) = partial_sink.as_deref_mut() {
                    // `frame_bytes > 0` also excludes an UNPINNED streamed frame (its total is
                    // still the 0 sentinel value): truncating its max-sized buffer to 0 would
                    // deliver an empty "partial" — worse than the plain drop it gets instead.
                    if f.user_flags & USER_FLAG_CHUNK_ALIGNED != 0 && f.frame_bytes > 0 {
                        let mut buf = std::mem::take(&mut f.buf);
                        buf.truncate(f.frame_bytes);
                        let newer = sink
                            .as_ref()
                            .is_none_or(|p| idx.wrapping_sub(p.frame_index) <= u32::MAX / 2);
                        if newer {
                            *sink = Some(Frame {
                                data: buf,
                                frame_index: idx,
                                pts_ns: f.pts_ns,
                                flags: f.user_flags,
                                complete: false,
                                part: None,
                                received_ns: 0, // stamped by Session::poll_frame at the session boundary
                            });
                        }
                    }
                }
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
mod reset_tests {
    use super::*;

    /// `flush_backlog` discards the past wholesale — an aged-out partial parked for delivery is
    /// part of that past and must not survive [`Reassembler::reset`] to be handed up as the
    /// first "frame" after a jump-to-live.
    #[test]
    fn reset_drops_a_parked_partial() {
        let mut r = Reassembler::new(ReassemblerLimits {
            shard_bytes: 64,
            max_data_shards: 8,
            max_total_shards: 16,
            max_blocks: 4,
            max_frame_bytes: 4096,
        });
        r.pending_partial = Some(Frame {
            data: vec![0u8; 64],
            frame_index: 7,
            pts_ns: 1,
            flags: 0,
            complete: false,
            part: None,
            received_ns: 0,
        });
        r.reset();
        assert!(
            r.take_partial().is_none(),
            "a pre-flush partial must not survive reset()"
        );
    }
}
