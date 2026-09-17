//! Host model: frames at the session fps, the wire-budget arithmetic the host
//! runs, adaptive FEC, and the send pacer.
//!
//! [`encoder_kbps_for_budget`], [`adapt_fec`], [`fec_target`] and
//! [`auto_burst_bytes`] are copies of the host crate's functions, each naming
//! its source. WP1 replaces the copies with a shared module.

use super::Rng;

/// 40-byte header + 24-byte seal in each datagram (`native.rs`
/// `SHARD_WIRE_OVERHEAD`).
pub(super) const SHARD_WIRE_OVERHEAD: u64 = 64;
/// `native.rs` `MIN_BITRATE_KBPS`.
const MIN_BITRATE_KBPS: u32 = 500;
/// `native.rs` FEC band, step, and the percent a session opens at before the
/// first loss report resizes it.
const FEC_MIN: u8 = 5;
const FEC_MAX: u8 = 50;
const FEC_STEP: u8 = 3;
const FEC_STEP_WINDOWS: u32 = 4;
const FEC_ADAPTIVE_START: u8 = 10;
/// `config.rs` `MIN_RECOVERY_SHARDS`, and the `max_data_per_block` the host
/// negotiates (`native/handshake.rs`). 4 096 means an ordinary frame is one
/// block, so its whole parity pool covers loss anywhere in it.
const MIN_RECOVERY_SHARDS: u32 = 2;
const MAX_DATA_PER_BLOCK: u32 = 4_096;
/// `stream.rs` `paced_submit`: the pacer runs at ~3× the live encoder rate.
const PACE_FACTOR: u64 = 3;
/// `send_pacing.rs` `MAX_PACE_SPREAD`.
const MAX_PACE_SPREAD_MS: u64 = 100;

/// Wire budget → encoder rate (`native.rs` `encoder_kbps_for_budget`).
pub(super) fn encoder_kbps_for_budget(
    budget_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    shard_payload: u16,
) -> u32 {
    let payload = shard_payload.max(1) as u64;
    let video_wire = budget_kbps.saturating_sub(audio_kbps) as u64;
    let video =
        video_wire * payload * 100 / ((payload + SHARD_WIRE_OVERHEAD) * (100 + fec_percent as u64));
    u32::try_from(video)
        .unwrap_or(u32::MAX)
        .max(MIN_BITRATE_KBPS)
}

/// Loss ppm → recovery percent (`native.rs` `adapt_fec`). Integer here, `f64`
/// there: `ceil(pct × 1.4) + 1` is `(ppm × 14).div_ceil(100_000) + 1`.
pub(super) fn adapt_fec(loss_ppm: u32) -> u8 {
    let target = (loss_ppm as u64 * 14).div_ceil(100_000) as u32 + 1;
    target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
}

/// One window's FEC target (`native.rs` `fec_target`).
pub(super) fn fec_target(loss_ppm: u32, prev: u8, unrecovered_run: u32) -> u8 {
    let step = if (1..=FEC_STEP_WINDOWS).contains(&unrecovered_run) {
        FEC_STEP
    } else {
        0
    };
    adapt_fec(loss_ppm)
        .saturating_add(step)
        .min(FEC_MAX)
        .max(prev.saturating_sub(1))
}

/// Bytes that leave unpaced (`send_pacing.rs` `auto_burst_bytes`).
pub(super) fn auto_burst_bytes(pace_rate_bps: u64, wire_bytes: usize) -> usize {
    const BURST_MS: u64 = 10;
    const BURST_MIN: usize = 16 * 1024;
    const BURST_MAX: usize = 256 * 1024;
    if pace_rate_bps == 0 {
        return (wire_bytes / 4).max(128 * 1024);
    }
    usize::try_from(pace_rate_bps * BURST_MS / 8000)
        .unwrap_or(BURST_MAX)
        .clamp(BURST_MIN, BURST_MAX)
}

/// Per-block FEC geometry of one frame. Every block but the last holds
/// [`MAX_DATA_PER_BLOCK`] data shards; parity follows all the data on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameShape {
    pub data: u32,
    pub blocks: u32,
    pub last_k: u32,
    pub m_full: u32,
    pub m_last: u32,
}

impl FrameShape {
    pub(super) fn of(bytes: u64, shard_payload: u16, fec_percent: u8) -> Self {
        let data = (bytes.div_ceil(shard_payload.max(1) as u64) as u32).max(1);
        let blocks = data.div_ceil(MAX_DATA_PER_BLOCK);
        let last_k = data - (blocks - 1) * MAX_DATA_PER_BLOCK;
        FrameShape {
            data,
            blocks,
            last_k,
            m_full: recovery_for(MAX_DATA_PER_BLOCK.min(data), fec_percent),
            m_last: recovery_for(last_k, fec_percent),
        }
    }

    pub(super) fn parity(&self) -> u32 {
        (self.blocks - 1) * self.m_full + self.m_last
    }

    pub(super) fn shards(&self) -> u32 {
        self.data + self.parity()
    }

    /// Block a wire index belongs to. Wire order is every block's data, then
    /// every block's parity (`packetize.rs`).
    pub(super) fn block_of(&self, idx: u32) -> u32 {
        if idx < self.data {
            return idx / MAX_DATA_PER_BLOCK;
        }
        let p = idx - self.data;
        let full = (self.blocks - 1) * self.m_full;
        if p < full {
            p / self.m_full.max(1)
        } else {
            self.blocks - 1
        }
    }

    /// Parity that block holds.
    pub(super) fn parity_of(&self, block: u32) -> u32 {
        if block + 1 == self.blocks {
            self.m_last
        } else {
            self.m_full
        }
    }
}

/// `config.rs` `FecConfig::recovery_for`.
fn recovery_for(data_shards: u32, fec_percent: u8) -> u32 {
    if fec_percent == 0 || data_shards == 0 {
        return 0;
    }
    (data_shards * fec_percent as u32)
        .div_ceil(100)
        .max(MIN_RECOVERY_SHARDS)
}

/// What the source hands the encoder over one stretch of the session.
#[derive(Clone, Copy, Debug)]
pub(super) struct ContentPhase {
    pub until_ms: u64,
    /// Share of the encoder's per-frame bit allowance the content fills.
    pub fill_pct: u32,
    /// Share of the session's frames the source actually produces.
    pub active_pct: u32,
    /// Idle: the host repeats the last picture as a keepalive instead.
    pub idle: bool,
    /// One frame `cut_pct` of normal size every `cut_every_ms`.
    pub cut_every_ms: u64,
    pub cut_pct: u32,
    /// Frame sizes vary ± this much. Without it every frame rounds to the
    /// same shard count and the wire rate moves in 2 900 kbps steps at
    /// 4K165 — an artefact of identical frames, not of the link.
    pub size_jitter_pct: u32,
}

impl Default for ContentPhase {
    fn default() -> Self {
        ContentPhase {
            until_ms: u64::MAX,
            fill_pct: 100,
            active_pct: 100,
            idle: false,
            cut_every_ms: 0,
            cut_pct: 100,
            size_jitter_pct: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HostCfg {
    pub fps: u32,
    pub audio_kbps: u32,
    pub shard_payload: u16,
    /// Rate the encoder can actually apply; an ask above it acks short.
    pub encoder_ceiling_kbps: Option<u32>,
    /// Ack and rebuild latency for one retarget.
    pub retarget_ms: u64,
    /// Encode time per frame, µs, plus its spread. The rate a saturated GPU
    /// can sustain is one frame per `encode_us`.
    pub encode_us: u32,
    pub encode_jitter_us: u32,
    /// Encode time after `loaded_from_ms` (GPU contention arriving), and the
    /// slow swing on top of it: contention ebbs over seconds, which is what
    /// the controller's rolling minimum reads as a rise.
    pub loaded_encode_us: u32,
    pub loaded_from_ms: u64,
    pub encode_swing_us: u32,
    pub encode_swing_ms: u64,
    /// A keyframe is this many times an ordinary frame.
    pub idr_pct: u32,
    pub content: Vec<ContentPhase>,
    /// Older host: never marks idle repeats.
    pub marks_repeats: bool,
}

impl Default for HostCfg {
    fn default() -> Self {
        HostCfg {
            fps: 60,
            audio_kbps: 256,
            shard_payload: 1408,
            encoder_ceiling_kbps: None,
            retarget_ms: 120,
            encode_us: 3_500,
            encode_jitter_us: 0,
            loaded_encode_us: 0,
            loaded_from_ms: u64::MAX,
            encode_swing_us: 0,
            encode_swing_ms: 1_500,
            idr_pct: 400,
            content: vec![ContentPhase::default()],
            marks_repeats: true,
        }
    }
}

/// One frame on its way to the client.
#[derive(Clone, Copy, Debug)]
pub(super) struct Frame {
    pub id: u32,
    pub capture_ms: u64,
    pub wire_bytes: u64,
    pub shape: FrameShape,
    pub encode_us: u32,
    pub repeat: bool,
    pub idr: bool,
}

pub(super) struct Host {
    cfg: HostCfg,
    rng: Rng,
    /// Wire budget the encoder is running at, and the ask in flight.
    budget_kbps: u32,
    pending: Option<(u64, u32)>,
    fec_percent: u8,
    unrecovered_run: u32,
    next_id: u32,
    next_frame_us: u64,
    /// Keyframe owed to the client, and the pacer's remainder.
    idr_owed: bool,
    swing_us: u32,
    swing_until_ms: u64,
    pace_left: u64,
    pace_frame: u32,
    pace_per_ms: u64,
    pace_until_ms: u64,
    /// Tail of the frame the next one preempted: the send thread is serial,
    /// so it finishes what it holds before picking the new frame up.
    flush: Option<(u32, u64)>,
    next_cut_ms: u64,
}

impl Host {
    pub(super) fn new(cfg: HostCfg, start_kbps: u32, seed: u64) -> Self {
        Host {
            cfg,
            rng: Rng::new(seed),
            budget_kbps: start_kbps,
            pending: None,
            fec_percent: FEC_ADAPTIVE_START,
            unrecovered_run: 0,
            next_id: 1,
            next_frame_us: 0,
            idr_owed: true,
            swing_us: 0,
            swing_until_ms: 0,
            pace_left: 0,
            pace_frame: 0,
            pace_per_ms: 0,
            pace_until_ms: 0,
            flush: None,
            next_cut_ms: 0,
        }
    }

    /// A `SetBitrate` landed. The ack the client gets back is what the encoder
    /// can apply, not what was asked.
    pub(super) fn on_set_bitrate(&mut self, now_ms: u64, kbps: u32) {
        let applied = kbps.min(self.cfg.encoder_ceiling_kbps.unwrap_or(u32::MAX));
        self.pending = Some((now_ms + self.cfg.retarget_ms, applied));
    }

    pub(super) fn on_keyframe_request(&mut self) {
        self.idr_owed = true;
    }

    /// Host adaptive FEC closes on the client's loss report.
    pub(super) fn on_loss_report(&mut self, loss_ppm: u32, unrecovered: bool) {
        self.unrecovered_run = if unrecovered {
            self.unrecovered_run.saturating_add(1)
        } else {
            0
        };
        self.fec_percent = fec_target(loss_ppm, self.fec_percent, self.unrecovered_run);
    }

    /// The rate the encoder is now running at, `Some` on the tick it changes.
    pub(super) fn apply_pending(&mut self, now_ms: u64) -> Option<u32> {
        match self.pending {
            Some((at, kbps)) if now_ms >= at => {
                self.pending = None;
                self.budget_kbps = kbps;
                Some(kbps)
            }
            _ => None,
        }
    }

    fn phase(&self, now_ms: u64) -> ContentPhase {
        *self
            .cfg
            .content
            .iter()
            .find(|p| now_ms < p.until_ms)
            .unwrap_or(&self.cfg.content[self.cfg.content.len() - 1])
    }

    fn encode_us(&mut self, now_ms: u64) -> u32 {
        let loaded = now_ms >= self.cfg.loaded_from_ms;
        let base = if loaded {
            self.cfg.loaded_encode_us
        } else {
            self.cfg.encode_us
        };
        if loaded && self.cfg.encode_swing_us > 0 && now_ms >= self.swing_until_ms {
            self.swing_us = self.rng.below(u64::from(self.cfg.encode_swing_us) + 1) as u32;
            self.swing_until_ms = now_ms + self.cfg.encode_swing_ms;
        }
        let swing = if loaded { self.swing_us } else { 0 };
        if self.cfg.encode_jitter_us == 0 {
            return base + swing;
        }
        base + swing + self.rng.below(u64::from(self.cfg.encode_jitter_us) + 1) as u32
    }

    /// Produce this millisecond's frame, if the frame clock fired. A loaded
    /// encoder caps the rate at one frame per `encode_us`, which is what turns
    /// GPU contention into a short window.
    pub(super) fn tick(&mut self, now_ms: u64) -> Option<Frame> {
        if now_ms * 1_000 < self.next_frame_us {
            return None;
        }
        let phase = self.phase(now_ms);
        let encode_us = self.encode_us(now_ms);
        let source_fps = (self.cfg.fps * phase.active_pct / 100).max(1);
        let encoder_fps = (1_000_000 / encode_us.max(1)).max(1);
        let fps_eff = source_fps.min(encoder_fps);
        // Accumulate the period so the long-run rate is the frame rate and not
        // the millisecond it was rounded to; resync only when a stall put the
        // clock a whole frame behind.
        let period_us = 1_000_000 / fps_eff as u64;
        self.next_frame_us += period_us;
        if self.next_frame_us + period_us < now_ms * 1_000 {
            self.next_frame_us = now_ms * 1_000 + period_us;
        }

        let enc_kbps = encoder_kbps_for_budget(
            self.budget_kbps,
            self.cfg.audio_kbps,
            self.fec_percent,
            self.cfg.shard_payload,
        );
        // Per-frame allowance is the session fps, not the rate the source
        // manages: a frame-driven source spends a slice of the budget.
        let frame_bytes = enc_kbps as u64 * 1_000 / 8 / self.cfg.fps.max(1) as u64;
        let idr = std::mem::take(&mut self.idr_owed);
        let cut = phase.cut_every_ms > 0 && now_ms >= self.next_cut_ms;
        if cut {
            self.next_cut_ms = now_ms + phase.cut_every_ms;
        }
        let bytes = if phase.idle {
            // Keepalive repeat: one shard's worth, flagged so the controller
            // reads the window as stillness.
            self.cfg.shard_payload as u64
        } else {
            let mut b = frame_bytes * phase.fill_pct as u64 / 100;
            if phase.size_jitter_pct > 0 {
                let j = u64::from(phase.size_jitter_pct);
                b = b * (100 + self.rng.below(2 * j + 1) - j) / 100;
            }
            if cut {
                b = b * phase.cut_pct as u64 / 100;
            }
            if idr {
                b = b * self.cfg.idr_pct as u64 / 100;
            }
            b.max(1)
        };
        let shape = FrameShape::of(bytes, self.cfg.shard_payload, self.fec_percent);
        let wire_bytes =
            shape.shards() as u64 * (self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD);
        let id = self.next_id;
        self.next_id += 1;
        // Pacer: the burst leaves now, the overflow over its wire time at 3×
        // the budget, bounded by MAX_PACE_SPREAD.
        let pace_rate_bps = self.budget_kbps as u64 * 1_000 * PACE_FACTOR;
        let burst = auto_burst_bytes(pace_rate_bps, wire_bytes as usize) as u64;
        let overflow = wire_bytes.saturating_sub(burst);
        let spread_ms = if overflow > 0 && pace_rate_bps > 0 {
            (overflow * 8 * 1_000 / pace_rate_bps).clamp(1, MAX_PACE_SPREAD_MS)
        } else {
            0
        };
        if self.pace_left > 0 {
            self.flush = Some((self.pace_frame, self.pace_left));
        }
        self.pace_frame = id;
        self.pace_left = overflow;
        self.pace_per_ms = if spread_ms > 0 {
            overflow.div_ceil(spread_ms)
        } else {
            0
        };
        self.pace_until_ms = now_ms + spread_ms;
        Some(Frame {
            id,
            capture_ms: now_ms,
            wire_bytes,
            shape,
            encode_us,
            repeat: phase.idle,
            idr,
        })
    }

    /// The preempted frame's tail, offered before the new frame's burst.
    pub(super) fn take_flush(&mut self) -> Option<(u32, u64)> {
        self.flush.take()
    }

    /// Bytes the pacer releases this millisecond, after the burst.
    pub(super) fn release(&mut self, now_ms: u64) -> (u32, u64) {
        if self.pace_left == 0 {
            return (self.pace_frame, 0);
        }
        // Past the spread the remainder goes at once: the send thread finishes
        // the frame before it picks up the next one.
        let take = if now_ms >= self.pace_until_ms {
            self.pace_left
        } else {
            self.pace_per_ms.min(self.pace_left)
        };
        self.pace_left -= take;
        (self.pace_frame, take)
    }

    /// Bytes of a new frame that leave unpaced.
    pub(super) fn burst_of(&self, f: &Frame) -> u64 {
        f.wire_bytes - self.pace_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget mirrors agree with the host's own test vectors: a roundtrip
    /// through the wire derivation lands back on the budget.
    #[test]
    fn the_budget_mirror_matches_the_host_derivation() {
        for (budget, audio, fec) in [
            (20_000u32, 256u32, 5u8),
            (171_294, 512, 10),
            (2_000, 96, 50),
        ] {
            let enc = encoder_kbps_for_budget(budget, audio, fec, 1408);
            let wire = enc as u64 * (1408 + SHARD_WIRE_OVERHEAD) * (100 + fec as u64)
                / (1408 * 100)
                + audio as u64;
            assert!(
                wire <= budget as u64 + 2 && wire + budget as u64 / 50 >= budget as u64,
                "{budget} kbps → {enc} kbps encoder → {wire} kbps wire"
            );
        }
        // MIN_BITRATE_KBPS floors a budget the audio reservation swallows.
        assert_eq!(encoder_kbps_for_budget(300, 256, 5, 1408), 500);
    }

    /// `adapt_fec` on the host's own band: clean decays to the floor, loss
    /// ramps past it, and 50 % is the ceiling.
    #[test]
    fn adaptive_fec_maps_loss_to_the_hosts_recovery_band() {
        assert_eq!(adapt_fec(0), 5);
        assert_eq!(adapt_fec(10_000), 5, "1 % loss is still inside the floor");
        assert_eq!(adapt_fec(50_000), 8, "5 % → ceil(7) + 1");
        assert_eq!(adapt_fec(400_000), 50);
        // A run of unrecovered frames adds the step for four windows, then
        // lets it go; decay is one point per window.
        assert_eq!(fec_target(0, 5, 1), 8);
        assert_eq!(fec_target(0, 8, 5), 7, "past the run, decay by one");
        assert_eq!(fec_target(0, 8, 0), 7);
    }

    /// Parity is at least two shards per block, and the wire index of every
    /// shard maps back to the block that can repair it.
    #[test]
    fn a_frames_blocks_carry_their_own_parity() {
        let shape = FrameShape::of(100_000, 1408, 10);
        assert_eq!(shape.data, 72);
        assert_eq!(shape.blocks, 1, "an ordinary frame is one block");
        assert_eq!(shape.parity(), 8);
        assert_eq!(shape.shards(), 80);
        assert_eq!(shape.block_of(0), 0);
        assert_eq!(shape.block_of(79), 0);
        // The 2-shard floor: 5 % of 9 shards is one, and one is not enough.
        assert_eq!(FrameShape::of(12_000, 1408, 5).parity(), 2);
        // Past 4 096 data shards a second block starts, with its own parity.
        let big = FrameShape::of(4_097 * 1408, 1408, 10);
        assert_eq!(big.blocks, 2);
        assert_eq!(big.last_k, 1);
        assert_eq!(big.m_full, 410);
        assert_eq!(big.m_last, 2);
        assert_eq!(big.block_of(4_096), 1);
        assert_eq!(big.parity_of(1), 2);
    }

    /// The burst rule: a 5 Mbps stream bursts about 19 KiB, a fat link clamps
    /// at 256 KiB (`send_pacing.rs`).
    #[test]
    fn the_pacing_burst_follows_the_hosts_rule() {
        assert_eq!(auto_burst_bytes(15_000_000, 200_000), 18_750);
        assert_eq!(auto_burst_bytes(90_000_000, 200_000), 112_500);
        assert_eq!(auto_burst_bytes(400_000_000, 900_000), 256 * 1024);
        assert_eq!(auto_burst_bytes(1_000_000, 200_000), 16 * 1024);
        assert_eq!(auto_burst_bytes(0, 900_000), 225_000);
    }

    /// A frame-driven source spends its share of the budget, and a saturated
    /// encoder caps the frame rate at one frame per encode.
    #[test]
    fn the_source_rate_bounds_what_the_budget_buys() {
        let cfg = HostCfg {
            fps: 165,
            content: vec![ContentPhase {
                active_pct: 50,
                ..ContentPhase::default()
            }],
            ..HostCfg::default()
        };
        let mut host = Host::new(cfg, 100_000, 7);
        let frames = (0..1_000).filter_map(|t| host.tick(t)).count();
        assert_eq!(frames, 82, "165 fps × 50 % over one second");

        let mut loaded = Host::new(
            HostCfg {
                fps: 165,
                encode_us: 18_000,
                ..HostCfg::default()
            },
            100_000,
            7,
        );
        let frames = (0..1_000).filter_map(|t| loaded.tick(t)).count();
        assert_eq!(frames, 55, "one frame per 18 ms of encode");
    }
}
