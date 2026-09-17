//! Client model: FEC repair, keyframe asks, decode time, and the report
//! window the real [`BitrateController`] is driven from.
//!
//! The window is assembled the way `client/pump/data.rs` assembles it — see
//! the table in `abr-wp0-simulator-handoff.md`. Only the wire is a model: the
//! controller, [`window_loss_ppm`](crate::quic::window_loss_ppm) and the
//! activity classification are the shipped code.

use super::host::{Frame, FrameShape, SHARD_WIRE_OVERHEAD};
use super::link::LossDraw;
use super::Rng;
use crate::abr::{BitrateController, WindowActivity};
use crate::client::{ADAPT_REPORT_INTERVAL, FLUSH_COOLDOWN};
use std::collections::VecDeque;
use std::time::Instant;

/// Jump-to-live's thresholds (`client/frame_channel.rs`, private there):
/// delay past `FLUSH_LATENCY` held for `FLUSH_AFTER`, or `QUEUE_HIGH` frames
/// of decode backlog held for `STANDING_TIME`.
const FLUSH_LATENCY_MS: u64 = 400;
const FLUSH_AFTER_MS: u64 = 250;
const QUEUE_HIGH: u32 = 6;
const STANDING_MS: u64 = 250;
/// The webOS client's recovery throttle: one ask per 100 ms until a keyframe
/// lands.
const KEYFRAME_ASK_MS: u64 = 100;

/// Decode latency: a floor plus a rise past the rate the decoder is happy at.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DecodeCfg {
    pub base_us: u32,
    pub jitter_us: u32,
    pub knee_kbps: u32,
    pub us_per_mbps: u32,
}

#[derive(Clone, Debug)]
pub(super) struct ClientCfg {
    pub start_kbps: u32,
    pub refresh_hz: u32,
    pub stream_cap_kbps: u32,
    pub audio_kbps: u32,
    pub shard_payload: u16,
    /// Older host: repeats are not flagged, so no window is ever idle.
    pub marks_repeats: bool,
    pub decode: DecodeCfg,
    /// Startup probe result, injected as the pump injects it: `(at_ms, kbps)`.
    /// The window it lands in is discarded, as the probe tail is.
    pub ceiling_at: Option<(u64, u32)>,
    /// `false` = an explicit bitrate, so no controller.
    pub automatic: bool,
}

impl Default for ClientCfg {
    fn default() -> Self {
        ClientCfg {
            start_kbps: 20_000,
            refresh_hz: 60,
            stream_cap_kbps: u32::MAX,
            audio_kbps: 256,
            shard_payload: 1408,
            marks_repeats: true,
            decode: DecodeCfg::default(),
            ceiling_at: None,
            automatic: true,
        }
    }
}

/// What the client sends the host in one tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    SetBitrate(u32),
    Keyframe,
    Loss { ppm: u32, unrecovered: bool },
}

/// One closed report window, kept for the metrics.
#[derive(Clone, Copy, Debug)]
pub(super) struct WindowRec {
    pub t_ms: u64,
    /// Rate the host has acked — what the session is actually running at.
    pub rate_kbps: u32,
    pub actual_kbps: u32,
    pub dropped: u64,
    pub cut_from_kbps: Option<u32>,
    pub discarded: bool,
    /// The encode down-driver's stand-down, sampled after the verdict.
    pub encode_disarmed: bool,
}

struct InFlight {
    id: u32,
    capture_ms: u64,
    remaining: u64,
    refused: u64,
    shape: FrameShape,
    encode_us: u32,
    repeat: bool,
    idr: bool,
    /// Scenario-injected unrecoverable frame.
    forced: bool,
}

pub(super) struct Client {
    cfg: ClientCfg,
    rng: Rng,
    pub(super) abr: BitrateController,
    flight: VecDeque<InFlight>,
    acks: Vec<u32>,
    lost_blocks: Vec<u32>,
    /// Window accumulators, in the pump's order.
    received_packets: u64,
    repaired: u64,
    received_bytes: u64,
    owd_sum_us: i64,
    owd_frames: u32,
    decode_sum_us: i64,
    decode_count: u32,
    encode_sum_us: i64,
    encode_count: u32,
    au_frames: u32,
    au_repeats: u32,
    dropped: u64,
    recovery_kf: u32,
    flushed: bool,
    discard: bool,
    next_window_ms: u64,
    /// Jump-to-live detectors and their shared cooldown.
    owd_over_since: Option<u64>,
    queue_over_since: Option<u64>,
    last_flush_ms: Option<u64>,
    decode_free_at_ms: u64,
    /// Keyframe throttle: asking until an IDR lands.
    awaiting_idr: bool,
    kf_next_ms: u64,
    force_loss_at_ms: Option<u64>,
    pub(super) windows: Vec<WindowRec>,
    pub(super) owd_samples: Vec<u32>,
}

impl Client {
    pub(super) fn new(cfg: ClientCfg, seed: u64) -> Self {
        let mut abr = BitrateController::with_ceiling_cap(
            if cfg.automatic { cfg.start_kbps } else { 0 },
            None,
        );
        abr.set_stream_cap(cfg.stream_cap_kbps);
        abr.set_frame_budget(cfg.refresh_hz);
        Client {
            rng: Rng::new(seed),
            abr,
            flight: VecDeque::new(),
            acks: Vec::new(),
            lost_blocks: Vec::new(),
            received_packets: 0,
            repaired: 0,
            received_bytes: 0,
            owd_sum_us: 0,
            owd_frames: 0,
            decode_sum_us: 0,
            decode_count: 0,
            encode_sum_us: 0,
            encode_count: 0,
            au_frames: 0,
            au_repeats: 0,
            dropped: 0,
            recovery_kf: 0,
            flushed: false,
            discard: false,
            next_window_ms: ADAPT_REPORT_INTERVAL.as_millis() as u64,
            owd_over_since: None,
            queue_over_since: None,
            last_flush_ms: None,
            decode_free_at_ms: 0,
            awaiting_idr: false,
            kf_next_ms: 0,
            force_loss_at_ms: None,
            windows: Vec::new(),
            owd_samples: Vec::new(),
            cfg,
        }
    }

    /// What the session is running at. A fixed-rate session has no
    /// controller, so its rate is the one it negotiated.
    pub(super) fn rate_kbps(&self) -> u32 {
        if self.cfg.automatic {
            self.abr.current_kbps
        } else {
            self.cfg.start_kbps
        }
    }

    /// Make the next frame unrecoverable, whatever the link does.
    pub(super) fn inject_lost_frame(&mut self, at_ms: u64) {
        self.force_loss_at_ms = Some(at_ms);
    }

    pub(super) fn push_ack(&mut self, kbps: u32) {
        self.acks.push(kbps);
    }

    /// A frame left the host: the client now knows what to wait for.
    pub(super) fn expect(&mut self, f: &Frame, now_ms: u64) {
        let forced = matches!(self.force_loss_at_ms, Some(t) if now_ms >= t);
        if forced {
            self.force_loss_at_ms = None;
        }
        self.flight.push_back(InFlight {
            id: f.id,
            capture_ms: f.capture_ms,
            remaining: f.wire_bytes,
            refused: 0,
            shape: f.shape,
            encode_us: f.encode_us,
            repeat: f.repeat,
            idr: f.idr,
            forced,
        });
    }

    /// Bytes the link's depth refused — shards that never left the host.
    /// `Some(shards)` when they were the frame's tail.
    pub(super) fn refuse(&mut self, frame: u32, bytes: u64) -> Option<u32> {
        let f = self.flight.iter_mut().find(|f| f.id == frame)?;
        f.refused += bytes;
        f.remaining = f.remaining.saturating_sub(bytes);
        (f.remaining == 0).then(|| f.shape.shards())
    }

    /// Bytes arrived. `Some(shards)` when this was the frame's tail and the
    /// link owes it a loss draw.
    pub(super) fn deliver(&mut self, frame: u32, bytes: u64) -> Option<u32> {
        let f = self.flight.iter_mut().find(|f| f.id == frame)?;
        f.remaining = f.remaining.saturating_sub(bytes);
        (f.remaining == 0).then(|| f.shape.shards())
    }

    /// Close one frame: repair what parity covers, count the rest.
    pub(super) fn complete(&mut self, frame: u32, draw: LossDraw, now_ms: u64) {
        let Some(pos) = self.flight.iter().position(|f| f.id == frame) else {
            return;
        };
        let f = self.flight.remove(pos).expect("position just found");
        let shards = f.shape.shards();
        let shard_wire = self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD;
        // The depth drops the frame's tail, and the tail on the wire is
        // parity: data-first order means a shallow overflow costs recovery
        // before it costs picture.
        let refused_shards = (f.refused.div_ceil(shard_wire) as u32).min(shards);
        self.lost_blocks.clear();
        self.lost_blocks.resize(f.shape.blocks as usize, 0);
        let mut lost = 0u32;
        let mark = |shape: &FrameShape, blocks: &mut [u32], idx: u32| {
            blocks[shape.block_of(idx) as usize] += 1;
        };
        for i in 0..refused_shards {
            mark(&f.shape, &mut self.lost_blocks, shards - 1 - i);
            lost += 1;
        }
        // Uniform loss spreads over the frame; a burst is one contiguous run.
        for i in 0..draw.random.min(shards) {
            let idx = (i as u64 * shards as u64 / draw.random.max(1) as u64) as u32;
            mark(&f.shape, &mut self.lost_blocks, idx.min(shards - 1));
            lost += 1;
        }
        for i in 0..draw.burst_len {
            let idx = (draw.burst_at + i).min(shards - 1);
            mark(&f.shape, &mut self.lost_blocks, idx);
            lost += 1;
        }
        let mut repaired = 0u32;
        let mut unrecoverable = f.forced;
        for (b, &lost_b) in self.lost_blocks.iter().enumerate() {
            if lost_b == 0 {
                continue;
            }
            if lost_b <= f.shape.parity_of(b as u32) {
                repaired += lost_b;
            } else {
                unrecoverable = true;
            }
        }
        let arrived = shards.saturating_sub(lost.min(shards));
        self.received_packets += u64::from(arrived);
        self.received_bytes += u64::from(arrived) * shard_wire;
        self.repaired += u64::from(repaired);
        if unrecoverable {
            self.dropped += 1;
            self.awaiting_idr = true;
            return;
        }
        self.au_frames = self.au_frames.saturating_add(1);
        if f.repeat && self.cfg.marks_repeats {
            self.au_repeats = self.au_repeats.saturating_add(1);
        }
        if f.idr {
            self.awaiting_idr = false;
        }
        let owd_us = (now_ms.saturating_sub(f.capture_ms) * 1_000) as i64;
        self.owd_sum_us += owd_us;
        self.owd_frames += 1;
        self.owd_samples.push((owd_us / 1_000) as u32);
        self.encode_sum_us += i64::from(f.encode_us);
        self.encode_count += 1;
        let decode_us = self.decode_us();
        self.decode_sum_us += i64::from(decode_us);
        self.decode_count += 1;
        self.decode_free_at_ms =
            self.decode_free_at_ms.max(now_ms) + u64::from(decode_us).div_ceil(1_000);
        self.note_latency(owd_us / 1_000, now_ms);
    }

    fn decode_us(&mut self) -> u32 {
        let d = self.cfg.decode;
        let over = self.abr.current_kbps.saturating_sub(d.knee_kbps) / 1_000;
        let jitter = if d.jitter_us == 0 {
            0
        } else {
            self.rng.below(u64::from(d.jitter_us) + 1) as u32
        };
        d.base_us + jitter + over * d.us_per_mbps
    }

    /// Jump-to-live, both halves: one-way delay past [`FLUSH_LATENCY_MS`] for
    /// [`FLUSH_AFTER_MS`], or a decode backlog at [`QUEUE_HIGH`] for
    /// [`STANDING_MS`]. A flush is ABR's severe `flushed`.
    fn note_latency(&mut self, owd_ms: i64, now_ms: u64) {
        if owd_ms > FLUSH_LATENCY_MS as i64 {
            self.owd_over_since.get_or_insert(now_ms);
        } else {
            self.owd_over_since = None;
        }
        let backlog = (self.decode_free_at_ms.saturating_sub(now_ms)
            * u64::from(self.cfg.refresh_hz.max(1))
            / 1_000) as u32;
        if backlog >= QUEUE_HIGH {
            self.queue_over_since.get_or_insert(now_ms);
        } else if backlog <= 2 {
            self.queue_over_since = None;
        }
        let over = |since: Option<u64>, ms: u64| since.is_some_and(|t| now_ms - t >= ms);
        let behind = over(self.owd_over_since, FLUSH_AFTER_MS)
            || (backlog >= QUEUE_HIGH && over(self.queue_over_since, STANDING_MS));
        let cooled = self
            .last_flush_ms
            .is_none_or(|t| now_ms - t >= FLUSH_COOLDOWN.as_millis() as u64);
        if behind && cooled {
            self.owd_over_since = None;
            self.queue_over_since = None;
            self.last_flush_ms = Some(now_ms);
            self.decode_free_at_ms = now_ms;
            self.flushed = true;
            self.awaiting_idr = true;
        }
    }

    /// One millisecond: the keyframe throttle, then the report window when it
    /// comes due.
    pub(super) fn tick(&mut self, now_ms: u64, base: Instant, out: &mut Vec<Action>) {
        if self.awaiting_idr && now_ms >= self.kf_next_ms {
            self.kf_next_ms = now_ms + KEYFRAME_ASK_MS;
            self.recovery_kf += 1;
            out.push(Action::Keyframe);
        }
        if let Some((at, kbps)) = self.cfg.ceiling_at {
            if now_ms >= at {
                self.cfg.ceiling_at = None;
                self.abr.set_ceiling(kbps);
                // The burst's tail is still draining into this window.
                self.discard = true;
            }
        }
        if now_ms < self.next_window_ms {
            return;
        }
        let window_ms = ADAPT_REPORT_INTERVAL.as_millis() as u64;
        self.next_window_ms += window_ms;
        let discard = std::mem::take(&mut self.discard);
        let loss_ppm = crate::quic::window_loss_ppm(self.repaired, 0, self.received_packets);
        if !discard {
            out.push(Action::Loss {
                ppm: loss_ppm,
                unrecovered: self.dropped > 0,
            });
        }
        for kbps in self.acks.drain(..) {
            self.abr.on_ack(kbps);
        }
        let owd_mean_us =
            (self.owd_frames > 0).then(|| self.owd_sum_us / i64::from(self.owd_frames));
        let decode_mean_us =
            (self.decode_count > 0).then(|| self.decode_sum_us / i64::from(self.decode_count));
        let encode_mean_us =
            (self.encode_count > 0).then(|| self.encode_sum_us / i64::from(self.encode_count));
        let activity = if self.au_frames == 0 {
            WindowActivity::Empty
        } else if self.cfg.marks_repeats {
            WindowActivity::Active(self.au_frames.saturating_sub(self.au_repeats))
        } else {
            WindowActivity::Unmarked
        };
        let actual_kbps =
            ((self.received_bytes * 8 / window_ms) as u32).saturating_add(self.cfg.audio_kbps);
        let was = self.rate_kbps();
        let verdict = (!discard).then(|| {
            self.abr.on_window(
                base + std::time::Duration::from_millis(now_ms),
                self.dropped,
                loss_ppm,
                owd_mean_us,
                decode_mean_us,
                encode_mean_us,
                actual_kbps,
                self.flushed,
                self.recovery_kf,
                activity,
            )
        });
        let request = verdict.flatten();
        if let Some(kbps) = request {
            out.push(Action::SetBitrate(kbps));
        }
        self.windows.push(WindowRec {
            t_ms: now_ms,
            rate_kbps: was,
            actual_kbps,
            dropped: self.dropped,
            cut_from_kbps: request.filter(|&k| k < was).map(|_| was),
            discarded: discard,
            encode_disarmed: self.abr.encode_disarmed,
        });
        self.received_packets = 0;
        self.repaired = 0;
        self.received_bytes = 0;
        self.owd_sum_us = 0;
        self.owd_frames = 0;
        self.decode_sum_us = 0;
        self.decode_count = 0;
        self.encode_sum_us = 0;
        self.encode_count = 0;
        self.au_frames = 0;
        self.au_repeats = 0;
        self.dropped = 0;
        self.recovery_kf = 0;
        self.flushed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::super::host::{Host, HostCfg};
    use super::*;

    fn frame(id: u32, bytes: u64, fec: u8) -> Frame {
        Frame {
            id,
            capture_ms: 0,
            wire_bytes: 0,
            shape: FrameShape::of(bytes, 1408, fec),
            encode_us: 1_000,
            repeat: false,
            idr: false,
        }
    }

    fn client() -> Client {
        Client::new(ClientCfg::default(), 11)
    }

    /// Parity covers its block's losses or the frame dies; either way
    /// `loss_ppm` counts only what was repaired, which is why the field sees
    /// `loss_ppm=0` beside a lost frame.
    #[test]
    fn parity_repairs_its_block_and_an_unrecoverable_frame_reports_no_loss() {
        let mut c = client();
        // 45 000 bytes = 32 data shards, 4 parity at 10 %.
        let f = frame(1, 45_000, 10);
        assert_eq!(f.shape.data, 32);
        assert_eq!(f.shape.parity_of(0), 4);
        c.expect(&f, 0);
        c.complete(
            1,
            LossDraw {
                random: 0,
                burst_at: 10,
                burst_len: 4,
            },
            10,
        );
        assert_eq!(c.dropped, 0, "four shards, four parity");
        assert_eq!(c.repaired, 4);
        assert!(crate::quic::window_loss_ppm(c.repaired, 0, c.received_packets) > 0);

        let mut c = client();
        c.expect(&frame(2, 45_000, 10), 0);
        c.complete(
            2,
            LossDraw {
                random: 0,
                burst_at: 10,
                burst_len: 5,
            },
            10,
        );
        assert_eq!(c.dropped, 1, "one shard past the parity loses the frame");
        assert_eq!(
            crate::quic::window_loss_ppm(c.repaired, 0, c.received_packets),
            0,
            "an unrecoverable frame teaches loss_ppm nothing"
        );
    }

    /// One frame is one FEC block, so its whole parity pool covers loss
    /// wherever it lands — spread or in one burst.
    #[test]
    fn one_pool_covers_loss_wherever_it_lands() {
        for draw in [
            LossDraw {
                random: 22,
                ..LossDraw::default()
            },
            LossDraw {
                random: 0,
                burst_at: 40,
                burst_len: 22,
            },
        ] {
            let mut c = client();
            c.expect(&frame(1, 300_000, 10), 0);
            c.complete(1, draw, 5);
            assert_eq!(c.dropped, 0, "22 of 22 parity shards");
            assert_eq!(c.repaired, 22);
        }
        let mut c = client();
        c.expect(&frame(2, 300_000, 10), 0);
        c.complete(
            2,
            LossDraw {
                random: 23,
                ..LossDraw::default()
            },
            5,
        );
        assert_eq!(c.dropped, 1, "one shard past the pool loses the frame");
    }

    /// The window the ceiling injection lands in is discarded, exactly as the
    /// pump discards the probe tail: no report, no verdict.
    #[test]
    fn the_probe_window_is_discarded() {
        let mut c = Client::new(
            ClientCfg {
                ceiling_at: Some((100, 170_000)),
                ..ClientCfg::default()
            },
            1,
        );
        let base = Instant::now();
        let mut out = Vec::new();
        for t in 0..=750 {
            c.tick(t, base, &mut out);
        }
        assert!(
            !out.iter().any(|a| matches!(a, Action::Loss { .. })),
            "a discarded window sends no loss report"
        );
        assert!(c.windows[0].discarded);
    }

    /// The host's frames reach the client whole across the pacer, and one
    /// window of them is the wire rate the controller is handed: the budget
    /// plus what rounding each frame up to whole shards and the two-shard
    /// parity floor cost it.
    #[test]
    fn a_clean_window_reports_the_wire_rate_the_host_spent() {
        let mut host = Host::new(
            HostCfg {
                fps: 60,
                idr_pct: 100,
                ..HostCfg::default()
            },
            20_000,
            2,
        );
        let mut c = client();
        let base = Instant::now();
        let mut out = Vec::new();
        for t in 0..=750 {
            if let Some(f) = host.tick(t) {
                c.expect(&f, t);
                let burst = host.burst_of(&f);
                if c.deliver(f.id, burst).is_some() {
                    c.complete(f.id, LossDraw::default(), t);
                }
            }
            let (id, bytes) = host.release(t);
            if bytes > 0 && c.deliver(id, bytes).is_some() {
                c.complete(id, LossDraw::default(), t);
            }
            c.tick(t, base, &mut out);
        }
        let w = c.windows[0];
        assert!(
            (20_000..=22_000).contains(&w.actual_kbps),
            "a 20 000 kbps budget delivered {} kbps",
            w.actual_kbps
        );
    }
}
