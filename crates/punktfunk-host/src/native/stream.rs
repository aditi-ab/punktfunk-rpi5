//! Native `punktfunk/1` capture→encode→send data plane.
//!
//! Owns the synthetic protocol-test source, the virtual-display stream loop
//! ([`virtual_stream`]) with mid-stream reconfigure, adaptive bitrate and recovery,
//! the microburst-paced send thread ([`send_loop`]), speed-test probes, the
//! session-switch watcher, and pipeline construction with bounded retry.
//! `serve_session` stands a session up and hands it a [`SessionContext`].
//!
//! Pin `PUNKTFUNK_PHASE_LOCK=0`, `PUNKTFUNK_IDD_ADAPTIVE=0`, `PUNKTFUNK_PACE_FACTOR=0`,
//! `PUNKTFUNK_STREAMED_AU=0` for the rebuild-free A/B levers. Evidence:
//! `design/phase-locked-capture.md`, `design/midstream-resolution-resize.md`.

use super::*;

/// Tag a wave-boundary AU with [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT).
///
/// `ir_wave_pos` counts frames since the last IDR/wave start. An IDR re-phases it to 0 and is
/// itself a clean anchor, so it is never additionally marked. Every `period`-th non-IDR AU is a
/// boundary — the client lifts its post-loss freeze on the SECOND such mark.
fn mark_recovery_boundary(ir_wave_pos: &mut u32, is_keyframe: bool, period: u32) -> bool {
    if is_keyframe {
        *ir_wave_pos = 0;
        false
    } else {
        *ir_wave_pos += 1;
        if *ir_wave_pos >= period {
            *ir_wave_pos = 0;
            true
        } else {
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn synthetic_stream(
    session: &mut Session,
    frames: u32,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    fec_target: &AtomicU8,
    timing_conn: Option<&quinn::Connection>,
    probe_seq: bool,
) -> Result<()> {
    let interval = std::time::Duration::from_millis(1000 / 60);
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        apply_fec_target(session, fec_target);
        service_probes(session, stop, probe_rx, probe_result_tx, probe_seq);
        let data = test_frame(idx, 64 * 1024);
        let pts_ns = now_ns();
        session
            .submit_frame(&data, pts_ns, (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
        // 0xCF host_us is near-zero here (no capture/encode); the datagram still proves the plane.
        if let Some(tc) = timing_conn {
            let t = punktfunk_core::quic::HostTiming {
                pts_ns,
                host_us: (now_ns().saturating_sub(pts_ns) / 1000).min(u32::MAX as u64) as u32,
                stages: None,
                applied_phase_ns: None,
            };
            let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t).into());
        }
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// Probe ceiling: 10 Gbps / 5 s. Above the session cap ([`MAX_BITRATE_KBPS`], 2 Gbps) so a
/// probe can show headroom past the rate a session will actually use.
const MAX_PROBE_KBPS: u32 = 10_000_000;
const MAX_PROBE_MS: u32 = 5_000;

/// Burst zero-filled [`FLAG_PROBE`] AUs at `req.target_kbps` for `req.duration_ms` (clamped to
/// `MAX_PROBE_*`). Paces by a bytes-allowed-so-far budget so scheduling jitter does not overshoot.
/// Video is paused for the duration — the caller's loop is blocked here.
fn run_probe_burst(
    session: &mut Session,
    req: ProbeRequest,
    stop: &AtomicBool,
    probe_seq: bool,
) -> ProbeResult {
    let target_kbps = req.target_kbps.min(MAX_PROBE_KBPS);
    let duration_ms = req.duration_ms.min(MAX_PROBE_MS);
    // Probe filler uses its own frame-index space. Without VIDEO_CAP_PROBE_SEQ the client has
    // one reassembly window and would drop probe frames as stale — decline rather than consume
    // video indexes the gap detector would read as a multi-thousand-frame loss after the burst.
    if !probe_seq {
        tracing::info!(
            "declining speed-test probe: client predates VIDEO_CAP_PROBE_SEQ (its reassembler \
             cannot window probe-space frames)"
        );
        return ProbeResult {
            bytes_sent: 0,
            packets_sent: 0,
            duration_ms: 0,
            wire_packets_sent: 0,
            send_dropped: 0,
        };
    }
    if target_kbps == 0 || duration_ms == 0 {
        return ProbeResult {
            bytes_sent: 0,
            packets_sent: 0,
            duration_ms: 0,
            wire_packets_sent: 0,
            send_dropped: 0,
        };
    }
    let bytes_per_sec = target_kbps as u64 * 125;
    // ≤16 KiB ≈ a dozen MTU shards; a 256 KiB AU overflowed a ~400 KiB send buffer on one submit.
    let chunk = (bytes_per_sec / 240).clamp(1200, 16 * 1024) as usize;
    let filler = vec![0u8; chunk];
    // Video is paused here, so the sealed/dropped deltas isolate host-side drops from link loss.
    let wire0 = session.stats().packets_sent;
    let drop0 = session.stats().packets_send_dropped;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(duration_ms as u64);
    let mut bytes_sent = 0u64;
    let mut packets_sent = 0u32;
    while std::time::Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        let allowed = (start.elapsed().as_secs_f64() * bytes_per_sec as f64) as u64;
        if bytes_sent < allowed {
            // WouldBlock/ENOBUFS is part of what the probe measures (`send_dropped`) — keep going.
            let _ = session.submit_probe_frame(&filler, now_ns());
            bytes_sent += chunk as u64;
            packets_sent += 1;
        } else {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    let actual_ms = start.elapsed().as_millis() as u32;
    let wire_offered = (session.stats().packets_sent - wire0) as u32;
    let send_dropped = (session.stats().packets_send_dropped - drop0) as u32;
    let wire_packets_sent = wire_offered.saturating_sub(send_dropped);
    tracing::info!(
        target_kbps,
        duration_ms = actual_ms,
        bytes_sent,
        au_count = packets_sent,
        wire_offered,
        wire_packets_sent,
        send_dropped,
        "speed-test probe burst complete"
    );
    ProbeResult {
        bytes_sent,
        packets_sent,
        duration_ms: actual_ms,
        wire_packets_sent,
        send_dropped,
    }
}

/// Drain pending speed-test requests between frames. `probe_seq` is [`VIDEO_CAP_PROBE_SEQ`].
fn service_probes(
    session: &mut Session,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    probe_seq: bool,
) {
    while let Ok(req) = probe_rx.try_recv() {
        let result = run_probe_burst(session, req, stop, probe_seq);
        let _ = probe_result_tx.send(result);
    }
}

use crate::send_pacing::{frame_driven_enabled, CaptureCredit};

/// `PUNKTFUNK_PHASE_LOCK=0` disarms the controller. Armed, it still waits for a [`PhaseReport`].
fn phase_lock_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_PHASE_LOCK").as_deref() != Ok("0"))
}

/// Control-task → encode-loop bridge: latest-wins [`PhaseReport`], drained ~1 Hz, published as
/// the 0xCF ACK hold.
pub(crate) struct PhaseCtl {
    report: std::sync::Mutex<Option<punktfunk_core::quic::PhaseReport>>,
    applied_ns: std::sync::atomic::AtomicI64,
}

impl PhaseCtl {
    pub(crate) fn new() -> PhaseCtl {
        PhaseCtl {
            report: std::sync::Mutex::new(None),
            applied_ns: std::sync::atomic::AtomicI64::new(0),
        }
    }

    pub(crate) fn store(&self, r: punktfunk_core::quic::PhaseReport) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r);
    }

    fn take(&self) -> Option<punktfunk_core::quic::PhaseReport> {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn set_applied(&self, ns: i64) {
        self.applied_ns.store(ns, Ordering::Relaxed);
    }

    pub(crate) fn applied_ns(&self) -> i64 {
        self.applied_ns.load(Ordering::Relaxed)
    }
}

/// Submit lock onto an absolute grid `epoch + k×period + offset` (`design/phase-locked-capture.md`).
///
/// A per-frame additive hold on an arrival-slaved loop saturates once `hold + work ≥ interval`
/// and free-runs; the commanded phase never arrives. A periodic grid cannot free-run: occupancy
/// is one frame per period, so actuation is linear. Loop-local so it survives in-loop rebuilds;
/// a new session starts disengaged (no grid sleeps).
///
/// Failure = DISENGAGE, never park a hold. Near-antipode errors (within 1 ms of ±period/2) flip
/// sign on sampling noise — half-step until the error commits. Engagement needs sustained
/// coherence; each incoherent cycle backs off longer; a host that cannot hold coherence fuses
/// for the session. Evidence: `design/host-source-stutter-fixes.md`.
struct PhaseController {
    /// Grid offset, ns ∈ [0, period). Meaningful only while engaged.
    offset_ns: i64,
    /// Grid epoch; `None` = disengaged. Stamped at engage, cleared at disengage — the lock's age.
    epoch: Option<std::time::Instant>,
    last_adjust: std::time::Instant,
    /// |step| integrated since engage — the chase detector.
    cum_travel_ns: i64,
    /// Consecutive incoherent reports; 3 disengage.
    incoherent_streak: u32,
    /// Consecutive coherent reports — the engage gate.
    coherent_streak: u32,
    /// Incoherent disengages this session. Forgiven by a lock that holds [`LOCK_STABLE`].
    incoherent_cycles: u32,
    fused: bool,
    reengage_backoff: u32,
}

impl PhaseController {
    /// 2 ms/s of reports: wire cadence stays visually still; a half-period error converges in ~2–3 s.
    const MAX_STEP_NS: i64 = 2_000_000;
    const DEADBAND_NS: i64 = 300_000;
    /// SurfaceFlinger-class compositors need the frame ~2.5 ms before latch; `uncertainty_ns` widens this.
    const TARGET_LEAD_FLOOR_NS: i64 = 2_500_000;
    /// Below this circular coherence (‰) the arrival phase is smeared. `u16::MAX` bypasses the gate.
    const COHERENCE_FLOOR_MILLI: u16 = 300;
    /// Within this of ±period/2, sampling noise flips the sign — damp until the error commits.
    const ANTIPODE_GUARD_NS: i64 = 1_000_000;
    const REENGAGE_BACKOFF: u32 = 10;
    /// ~5 s of a lockable phase at the ~1 Hz report cadence. One report re-engages a hovering host.
    const ENGAGE_COHERENT_REPORTS: u32 = 5;
    /// Permanently disengaged (zero added latency) beats another cycle of timing steps.
    const INCOHERENT_FUSE: u32 = 8;
    /// `REENGAGE_BACKOFF << 5` = 320 ticks ≈ 5 min.
    const MAX_BACKOFF_SHIFT: u32 = 5;
    /// A transient bad patch must not fuse a host that is otherwise lockable.
    const LOCK_STABLE: std::time::Duration = std::time::Duration::from_secs(60);

    fn new() -> PhaseController {
        PhaseController {
            offset_ns: 0,
            epoch: None,
            last_adjust: std::time::Instant::now(),
            cum_travel_ns: 0,
            incoherent_streak: 0,
            coherent_streak: 0,
            incoherent_cycles: 0,
            fused: false,
            reengage_backoff: 0,
        }
    }

    fn engaged(&self) -> bool {
        self.epoch.is_some()
    }

    /// `coherence_milli` is the number that says whether the host is marginal or hopeless.
    fn disengage(&mut self, reason: &'static str, backoff: u32, coherence_milli: u16) {
        if self.engaged() {
            tracing::info!(
                offset_ms = self.offset_ns as f64 / 1e6,
                coherence_milli,
                reason,
                "phase lock: disengaging the submit grid"
            );
        }
        self.epoch = None;
        self.offset_ns = 0;
        self.cum_travel_ns = 0;
        self.incoherent_streak = 0;
        self.coherent_streak = 0;
        self.reengage_backoff = backoff;
    }

    /// Positive (shortest-way) error = frames arrive early → grow the offset; negative → earlier.
    fn adjust(&mut self, r: &punktfunk_core::quic::PhaseReport, period_ns: i64) {
        if period_ns <= 0 || self.fused {
            return;
        }
        self.last_adjust = std::time::Instant::now();
        if self.reengage_backoff > 0 {
            self.reengage_backoff -= 1;
            return;
        }
        let coherent =
            r.coherence_milli == u16::MAX || r.coherence_milli >= Self::COHERENCE_FLOOR_MILLI;
        if !coherent {
            self.coherent_streak = 0;
            self.incoherent_streak += 1;
            if self.incoherent_streak >= 3 {
                // Count only an engaged tear-down. Pre-lock incoherent arrival must not blow the fuse.
                if self.engaged() {
                    self.incoherent_cycles += 1;
                    if self.incoherent_cycles >= Self::INCOHERENT_FUSE {
                        self.fused = true;
                        tracing::info!(
                            cycles = self.incoherent_cycles,
                            coherence_milli = r.coherence_milli,
                            "phase lock: arrival phase incoherent on this host — parked for the \
                             session"
                        );
                    }
                }
                let backoff = Self::REENGAGE_BACKOFF
                    << self
                        .incoherent_cycles
                        .saturating_sub(1)
                        .min(Self::MAX_BACKOFF_SHIFT);
                self.disengage("incoherent arrival phase", backoff, r.coherence_milli);
            }
            return;
        }
        self.incoherent_streak = 0;
        self.coherent_streak = self.coherent_streak.saturating_add(1);
        // Forgive while the lock is good, not only when it is lost.
        if self.epoch.is_some_and(|e| e.elapsed() >= Self::LOCK_STABLE) {
            self.incoherent_cycles = 0;
        }
        let target = Self::TARGET_LEAD_FLOOR_NS.max(r.uncertainty_ns as i64 + 1_000_000);
        let raw = (r.arrival_lead_ns as i64 - target).rem_euclid(period_ns);
        let error = if raw > period_ns / 2 {
            raw - period_ns
        } else {
            raw
        };
        if error.abs() < Self::DEADBAND_NS {
            self.cum_travel_ns = 0;
            return;
        }
        if !self.engaged() {
            if self.coherent_streak < Self::ENGAGE_COHERENT_REPORTS {
                return;
            }
            self.epoch = Some(std::time::Instant::now());
            tracing::info!(
                coherence_milli = r.coherence_milli,
                "phase lock: engaging the submit grid"
            );
        }
        let mut step = error.clamp(-Self::MAX_STEP_NS, Self::MAX_STEP_NS);
        if error.abs() > period_ns / 2 - Self::ANTIPODE_GUARD_NS {
            step /= 2;
        }
        self.offset_ns = (self.offset_ns + step).rem_euclid(period_ns);
        self.cum_travel_ns += step.abs();
        if self.cum_travel_ns > period_ns + period_ns / 4 {
            tracing::info!("phase lock: travel budget exhausted without convergence — disengaging");
            self.disengage("travel budget", Self::REENGAGE_BACKOFF, r.coherence_milli);
        }
    }

    /// Next grid instant at or after `now`. Newest-wins keeps content fresh across the wait.
    fn next_submit_target(
        &self,
        now: std::time::Instant,
        period_ns: i64,
    ) -> Option<std::time::Instant> {
        let epoch = self.epoch?;
        if period_ns <= 0 {
            return None;
        }
        let elapsed = now.duration_since(epoch).as_nanos() as i64;
        let k = (elapsed - self.offset_ns).div_euclid(period_ns) + 1;
        let target_ns = k * period_ns + self.offset_ns;
        let target = epoch + std::time::Duration::from_nanos(target_ns.max(0) as u64);
        if target.duration_since(now).as_nanos() as i64 > period_ns {
            return Some(now);
        }
        Some(target)
    }

    fn applied_readout(&self) -> i64 {
        if self.engaged() {
            self.offset_ns
        } else {
            0
        }
    }

    fn due(&self) -> bool {
        self.last_adjust.elapsed() >= std::time::Duration::from_secs(1)
    }
}

/// Depth-1 by default: depth-2 holds a ready AU a whole interval unpolled (~13 ms extra at 60 fps).
/// Escalate to the capturer's max only when cadence cannot hold at depth-1 (GPU contention).
/// `PUNKTFUNK_IDD_ADAPTIVE=0` pins the capturer's full depth. Off when max depth is already 1.
fn idd_adaptive_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_IDD_ADAPTIVE").as_deref() != Ok("0"))
}

/// Seal one AU and send it under [`send_pacing`](crate::send_pacing): first `burst_cap` bytes
/// leave immediately; overflow spreads at `pace_rate_bps` in adaptive chunks (16…64, the GSO
/// cap). `burst_cap` `None` = 10 ms at the pace rate, clamped to [16 KiB, 256 KiB]
/// ([`crate::send_pacing::auto_burst_bytes`]); `Some` = `PUNKTFUNK_PACE_BURST_KB`. An unpaced
/// line-rate burst overruns the kernel tx buffer → EAGAIN → freeze until the next keyframe.
///
/// `pace_rate_bps` is ~3× the live encoder bitrate — the overflow's wire time at that rate is
/// the budget ([`crate::send_pacing::native_budget`], [`MAX_PACE_SPREAD`]-bounded). `0` =
/// deadline-only spread (`PUNKTFUNK_PACE_FACTOR=0`, or bitrate not yet known).
#[allow(clippy::too_many_arguments)]
fn paced_submit(
    session: &mut Session,
    data: &[u8],
    pts_ns: u64,
    flags: u32,
    frame_index: u32,
    deadline: std::time::Instant,
    burst_cap: Option<usize>,
    pace_rate_bps: u64,
    max_spread: std::time::Duration,
) -> Result<PaceStat> {
    let wires = session
        .seal_frame_at(data, pts_ns, flags, frame_index)
        .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    pace_sealed(
        session,
        wires,
        deadline,
        burst_cap,
        pace_rate_bps,
        max_spread,
    )
}

/// Pace already-sealed wires. Shared with the streamed-AU path ([`handle_chunk`]).
fn pace_sealed(
    session: &mut Session,
    wires: Vec<Vec<u8>>,
    deadline: std::time::Instant,
    burst_cap: Option<usize>,
    pace_rate_bps: u64,
    max_spread: std::time::Duration,
) -> Result<PaceStat> {
    let mut refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    crate::send_pacing::inject_video_drop(&mut refs);
    let wire_bytes: usize = refs.iter().map(|p| p.len()).sum();
    let burst_bytes = burst_cap
        .unwrap_or_else(|| crate::send_pacing::auto_burst_bytes(pace_rate_bps, wire_bytes));
    let cfg = crate::send_pacing::PaceCfg {
        burst_bytes: Some(burst_bytes),
        chunk: crate::send_pacing::ChunkPolicy::Adaptive { base: 16, max: 64 },
        sleep_floor: std::time::Duration::from_micros(500),
    };
    let overflow_bytes = wire_bytes.saturating_sub(burst_bytes) as u64;
    let budget =
        crate::send_pacing::native_budget(deadline, pace_rate_bps, overflow_bytes, max_spread);
    // Sleeps between chunks stay excluded: sock_ns is pure send_gso/sendmmsg time.
    let mut sock_ns = 0u64;
    let result = crate::send_pacing::pace_frame(&refs, budget, &cfg, |chunk| {
        let t0 = std::time::Instant::now();
        let r = session.send_sealed(chunk).map(|_| ());
        sock_ns += t0.elapsed().as_nanos() as u64;
        r
    });
    drop(refs);
    session.reclaim_wires(wires);
    session.note_sock_ns(sock_ns);
    result.map_err(|e| anyhow!("send_sealed: {e:?}"))
}

/// One encoded AU handed to the send thread. Encode of N+1 overlaps transmit of N.
struct FrameMsg {
    data: Vec<u8>,
    capture_ns: u64,
    flags: u32,
    /// Predicted at submit as `au_seq + inflight`; stamped on the wire so RFI stays 1:1 across rebuilds.
    frame_index: u32,
    /// Next frame's due time. Past = send immediately (catch up).
    deadline: std::time::Instant,
    encode_us: u32,
    /// Delivery→submit age (µs). 0 for repeats/tail. Wire pts anchors at the same delivery stamp.
    queue_us: u32,
    /// `cap_us` = `try_latest`; `submit_us` = encode launch; `wait_us` = lock_bitstream.
    /// Synchronous backends (PyroWave) put the whole encode in `submit_us` — `wait_us` reads ~0.
    cap_us: u32,
    submit_us: u32,
    wait_us: u32,
    repeat: bool,
    /// Trust this, not a re-read of `is_armed()`: a capture that arms mid-flight must not fold
    /// zeroed splits into the first window's percentiles.
    was_measured: bool,
}

/// Whole AU, or one slice-boundary chunk of a streamed AU (seal/pace while the encoder still runs).
enum SendMsg {
    Frame(FrameMsg),
    Chunk(ChunkMsg),
}

/// One encoder chunk of a streamed AU. AU-level fields match on every chunk; splits matter on `last`.
struct ChunkMsg {
    data: Vec<u8>,
    first: bool,
    last: bool,
    capture_ns: u64,
    flags: u32,
    frame_index: u32,
    deadline: std::time::Instant,
    encode_us: u32,
    queue_us: u32,
    cap_us: u32,
    submit_us: u32,
    wait_us: u32,
    repeat: bool,
    was_measured: bool,
}

/// Open streamed AU: incremental sealer plus pace aggregation across per-chunk flushes.
struct StreamedOpen {
    au: punktfunk_core::packet::StreamedAu,
    spread_us: u32,
    paced: bool,
    /// One microburst budget per AU, consumed across flushes. Per-flush auto granted each block
    /// its own 128 KiB. `None` = pacing off (`PUNKTFUNK_PACE_FACTOR=0`, no burst pin).
    burst_left: Option<usize>,
}

/// Open at `first`, seal+pace completed FEC blocks, close at `last`. `None` mid-AU.
fn handle_chunk(
    session: &mut Session,
    open: &mut Option<StreamedOpen>,
    c: ChunkMsg,
    slice_wire: bool,
    burst_cap: Option<usize>,
    pace_rate_bps: u64,
    max_spread: std::time::Duration,
) -> Result<Option<(FrameMsg, PaceStat)>> {
    if c.first {
        if open.take().is_some() {
            // Rebuild forfeits the in-flight AU; sentinel packets are already on the wire.
            tracing::warn!(
                "streamed AU abandoned mid-flight (encoder rebuild) — client ages it out"
            );
        }
        // USER_FLAG_SLICE_STREAM only toward a client that negotiated streamed AUs AND multi-slice.
        let flags = c.flags
            | if slice_wire {
                punktfunk_core::packet::USER_FLAG_SLICE_STREAM
            } else {
                0
            }
            | if c.repeat {
                punktfunk_core::packet::USER_FLAG_REPEAT
            } else {
                0
            };
        *open = Some(StreamedOpen {
            au: session
                .begin_streamed_frame_at(c.capture_ns, flags, c.frame_index)
                .map_err(|e| anyhow!("begin_streamed_frame: {e:?}"))?,
            spread_us: 0,
            paced: false,
            burst_left: if pace_rate_bps == 0 && burst_cap.is_none() {
                None
            } else {
                Some(
                    burst_cap
                        .unwrap_or_else(|| crate::send_pacing::auto_burst_bytes(pace_rate_bps, 0)),
                )
            },
        });
    }
    let Some(s) = open.as_mut() else {
        return Err(anyhow!(
            "streamed chunk without an open AU (encode-loop bug)"
        ));
    };
    // Chunked poll returns per-slice; the AU's flag gates whether the sealer cuts a block there.
    let wires = session
        .seal_streamed_chunk(&mut s.au, &c.data, true)
        .map_err(|e| anyhow!("seal_streamed_chunk: {e:?}"))?;
    if !wires.is_empty() {
        // Charge the flush's full wire size. Over-count paces later blocks sooner (the safe direction).
        let flush_bytes: usize = wires.iter().map(|w| w.len()).sum();
        let stat = pace_sealed(
            session,
            wires,
            c.deadline,
            s.burst_left.or(burst_cap),
            pace_rate_bps,
            max_spread,
        )?;
        if let Some(left) = s.burst_left.as_mut() {
            *left = left.saturating_sub(flush_bytes);
        }
        s.spread_us = s.spread_us.saturating_add(stat.spread_us);
        s.paced |= stat.paced;
    }
    if !c.last {
        return Ok(None);
    }
    let s = open.take().expect("checked above");
    let tail = session
        .seal_streamed_finish(s.au)
        .map_err(|e| anyhow!("seal_streamed_finish: {e:?}"))?;
    let stat = pace_sealed(
        session,
        tail,
        c.deadline,
        s.burst_left.or(burst_cap),
        pace_rate_bps,
        max_spread,
    )?;
    Ok(Some((
        FrameMsg {
            data: Vec::new(),
            capture_ns: c.capture_ns,
            flags: c.flags,
            frame_index: c.frame_index,
            deadline: c.deadline,
            encode_us: c.encode_us,
            queue_us: c.queue_us,
            cap_us: c.cap_us,
            submit_us: c.submit_us,
            wait_us: c.wait_us,
            repeat: c.repeat,
            was_measured: c.was_measured,
        },
        PaceStat {
            spread_us: s.spread_us.saturating_add(stat.spread_us),
            paced: s.paced || stat.paced,
        },
    )))
}

/// Inputs the send thread needs for the 2 s web-console sample.
struct SendStats {
    rec: Arc<StatsRecorder>,
    /// Packed w:16|h:16|hz:16. Capture thread updates it on a mid-stream mode switch.
    mode: Arc<AtomicU64>,
    codec: &'static str,
    client: String,
    bitrate_kbps: Arc<AtomicU32>,
    bringup: Arc<crate::bringup::Trace>,
}

/// Whether this session may accept a mid-stream `Reconfigure`.
///
/// Off for gamescope (a resize respawns the nested game), a per-client-mode identity (the mode
/// is part of the slot key, so a resize is a different display), and a monitor mirror (the
/// physical head's mode is fixed; see `design/per-monitor-portal-capture.md`). The client scales.
pub(super) fn reconfig_allowed(
    compositor: Option<crate::vdisplay::Compositor>,
    per_client_mode: bool,
    mirrored: bool,
) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope) && !per_client_mode && !mirrored
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    mut session: Session,
    frame_rx: std::sync::mpsc::Receiver<SendMsg>,
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    stop: Arc<AtomicBool>,
    perf: bool,
    // Smoothed whole-AU paced-send µs. The split arbiter prices HEVC overlap from this; only this
    // thread sees a send.
    send_spread_us: Arc<AtomicU32>,
    wire_rekeys: Arc<AtomicU32>,
    slice_wire: bool,
    burst_cap: Option<usize>,
    fec_target: Arc<AtomicU8>,
    // Applied between AUs only — a streamed AU's tiling is derived from the size it began with.
    shard_rx: std::sync::mpsc::Receiver<usize>,
    stats: SendStats,
    timing_conn: Option<quinn::Connection>,
    phase: Arc<PhaseCtl>,
    probe_seq: bool,
) {
    boost_thread_priority(false);
    // 3× default: the link carries 1× sustained, so a bounded 3× excursion is safe (WebRTC uses 2.5×).
    // `PUNKTFUNK_PACE_FACTOR=0` restores deadline-only spread.
    let pace_factor: f64 = std::env::var("PUNKTFUNK_PACE_FACTOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|f: &f64| f.is_finite() && *f >= 0.0)
        .unwrap_or(3.0);
    let mut last_perf = std::time::Instant::now();
    let mut last_bytes = 0u64;
    let mut last_send_dropped = 0u64;
    let mut encode_us: Vec<u32> = Vec::new();
    let mut pace_us: Vec<u32> = Vec::new();
    let (mut paced_frames, mut immediate_frames) = (0u64, 0u64);
    let mut sid: Option<u32> = None;
    let (mut cap_v, mut submit_v, mut wait_v, mut queue_v): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut new_frames, mut repeat_frames) = (0u64, 0u64);
    let mut last_frames_dropped = 0u64;
    let mut last_packets_dropped = 0u64;
    let mut last_fec_recovered = 0u64;
    let mut streamed: Option<StreamedOpen> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Never mid-AU: a burst spliced between streamed chunks would push the tail past its deadline.
        if streamed.is_none() {
            service_probes(&mut session, &stop, &probe_rx, &probe_result_tx, probe_seq);
        }
        apply_fec_target(&mut session, &fec_target);
        if streamed.is_none() {
            let mut want_shard = None;
            while let Ok(s) = shard_rx.try_recv() {
                want_shard = Some(s);
            }
            if let Some(s) = want_shard {
                match session.set_shard_payload(s) {
                    Ok(()) => {
                        wire_rekeys.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(shard_payload = s, "wire shard payload re-keyed");
                    }
                    Err(e) => tracing::warn!(shard_payload = s, error = ?e,
                        "shard re-key refused by session validation"),
                }
            }
        }
        match frame_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(send_msg) => {
                let pace_rate = (stats.bitrate_kbps.load(Ordering::Relaxed) as f64
                    * 1000.0
                    * pace_factor) as u64;
                // Bound one frame's spread to ~2 intervals so a big IDR cannot back the channel
                // into `cadence_degraded`. hz 0 = not yet known → the absolute ceiling alone.
                let (_, _, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                let max_spread = if hz > 0 {
                    std::time::Duration::from_secs_f64(2.0 / hz as f64)
                } else {
                    crate::send_pacing::MAX_PACE_SPREAD
                };
                let outcome = match send_msg {
                    SendMsg::Frame(msg) => paced_submit(
                        &mut session,
                        &msg.data,
                        msg.capture_ns,
                        // HOST_CAP2_REPEAT_MARK makes the bit's absence mean "new content".
                        msg.flags
                            | if msg.repeat {
                                punktfunk_core::packet::USER_FLAG_REPEAT
                            } else {
                                0
                            },
                        msg.frame_index,
                        msg.deadline,
                        burst_cap,
                        pace_rate,
                        max_spread,
                    )
                    .map(|stat| Some((msg, stat))),
                    SendMsg::Chunk(c) => handle_chunk(
                        &mut session,
                        &mut streamed,
                        c,
                        slice_wire,
                        burst_cap,
                        pace_rate,
                        max_spread,
                    ),
                };
                match outcome {
                    Ok(None) => {}
                    Ok(Some((msg, stat))) => {
                        if msg.flags & FLAG_PROBE as u32 == 0 {
                            stats.bringup.finish("first_packet");
                        }
                        // Stamp 0xCF now against the same capture anchor the wire pts carries.
                        if let Some(tc) = &timing_conn {
                            if msg.flags & FLAG_PROBE as u32 == 0 {
                                let host_us = (now_ns().saturating_sub(msg.capture_ns) / 1000)
                                    .min(u32::MAX as u64)
                                    as u32;
                                let t = punktfunk_core::quic::HostTiming {
                                    pts_ns: msg.capture_ns,
                                    host_us,
                                    stages: Some(punktfunk_core::quic::HostStages {
                                        queue_us: msg.queue_us,
                                        encode_us: msg.encode_us,
                                        pace_us: stat.spread_us,
                                    }),
                                    applied_phase_ns: Some(
                                        phase.applied_ns().clamp(i32::MIN as i64, i32::MAX as i64)
                                            as i32,
                                    ),
                                };
                                let _ = tc.send_datagram(
                                    punktfunk_core::quic::encode_host_timing_datagram(&t).into(),
                                );
                            }
                        }
                        // EWMA (3:1): a single AU's spread must not flip the split-arbiter verdict.
                        {
                            let prev = send_spread_us.load(Ordering::Relaxed);
                            let next = if prev == 0 {
                                stat.spread_us
                            } else {
                                ((prev as u64 * 3 + stat.spread_us as u64) / 4) as u32
                            };
                            send_spread_us.store(next, Ordering::Relaxed);
                        }
                        if perf || stats.rec.is_armed() {
                            encode_us.push(msg.encode_us);
                            pace_us.push(stat.spread_us);
                            if msg.was_measured {
                                cap_v.push(msg.cap_us);
                                submit_v.push(msg.submit_us);
                                wait_v.push(msg.wait_us);
                                if !msg.repeat {
                                    queue_v.push(msg.queue_us);
                                }
                            }
                            if msg.repeat {
                                repeat_frames += 1;
                            } else {
                                new_frames += 1;
                            }
                            if stat.paced {
                                paced_frames += 1;
                            } else {
                                immediate_frames += 1;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "send failed — stopping stream");
                        break;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if last_perf.elapsed() >= std::time::Duration::from_secs(2) {
            let s = session.stats();
            let secs = last_perf.elapsed().as_secs_f64();
            let tx_mbps = (s.bytes_sent - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
            if perf {
                let sp = session.take_seal_perf().unwrap_or_default();
                tracing::info!(
                    tx_mbps = format!("{tx_mbps:.0}"),
                    send_dropped = s.packets_send_dropped - last_send_dropped,
                    send_dropped_total = s.packets_send_dropped,
                    encode_us_p50 = percentile(&mut encode_us, 0.50),
                    encode_us_p99 = percentile(&mut encode_us, 0.99),
                    pace_us_p50 = percentile(&mut pace_us, 0.50),
                    pace_us_p99 = percentile(&mut pace_us, 0.99),
                    pace_us_max = pace_us.last().copied().unwrap_or(0),
                    immediate_frames,
                    paced_frames,
                    window_ms = format!("{:.0}", secs * 1000.0),
                    fec_ms = format!("{:.2}", sp.fec_ns as f64 / 1e6),
                    seal_ms = format!("{:.2}", sp.seal_ns as f64 / 1e6),
                    sock_ms = format!("{:.2}", sp.sock_ns as f64 / 1e6),
                    fec_ns_pp = sp.fec_ns.checked_div(sp.packets).unwrap_or(0),
                    seal_ns_pp = sp.seal_ns.checked_div(sp.packets).unwrap_or(0),
                    sock_ns_pp = sp.sock_ns.checked_div(sp.packets).unwrap_or(0),
                    sealed_pkts = sp.packets,
                    "perf"
                );
            }
            if stats.rec.is_armed() {
                let session_id = *sid.get_or_insert_with(|| {
                    let (w, h, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                    stats
                        .rec
                        .register_session("native", w, h, hz, stats.codec, &stats.client)
                });
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0,
                    session_id,
                    stages: vec![
                        crate::stats_recorder::StageTiming {
                            name: "queue".into(),
                            p50_us: percentile(&mut queue_v, 0.50) as f32,
                            p99_us: percentile(&mut queue_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "capture".into(),
                            p50_us: percentile(&mut cap_v, 0.50) as f32,
                            p99_us: percentile(&mut cap_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "submit".into(),
                            p50_us: percentile(&mut submit_v, 0.50) as f32,
                            p99_us: percentile(&mut submit_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "encode".into(),
                            p50_us: percentile(&mut wait_v, 0.50) as f32,
                            p99_us: percentile(&mut wait_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "send".into(),
                            p50_us: percentile(&mut pace_us, 0.50) as f32,
                            p99_us: percentile(&mut pace_us, 0.99) as f32,
                        },
                    ],
                    fps: (new_frames as f64 / secs) as f32,
                    repeat_fps: (repeat_frames as f64 / secs) as f32,
                    mbps: tx_mbps as f32,
                    bitrate_kbps: stats.bitrate_kbps.load(Ordering::Relaxed),
                    frames_dropped: s.frames_dropped.saturating_sub(last_frames_dropped) as u32,
                    packets_dropped: s.packets_dropped.saturating_sub(last_packets_dropped) as u32,
                    send_dropped: s.packets_send_dropped.saturating_sub(last_send_dropped) as u32,
                    fec_recovered: s.fec_recovered_shards.saturating_sub(last_fec_recovered) as u32,
                };
                stats.rec.push_sample(session_id, sample);
            }
            last_perf = std::time::Instant::now();
            last_bytes = s.bytes_sent;
            last_send_dropped = s.packets_send_dropped;
            last_frames_dropped = s.frames_dropped;
            last_packets_dropped = s.packets_dropped;
            last_fec_recovered = s.fec_recovered_shards;
            encode_us.clear();
            pace_us.clear();
            cap_v.clear();
            submit_v.clear();
            wait_v.clear();
            queue_v.clear();
            paced_frames = 0;
            immediate_frames = 0;
            new_frames = 0;
            repeat_frames = 0;
        }
    }
}

/// Mid-stream Gaming↔Desktop flip. Env is applied on the encode thread — the watcher never `setenv`s.
struct SessionSwitch {
    kind: crate::vdisplay::ActiveKind,
    compositor: crate::vdisplay::Compositor,
    env: crate::vdisplay::SessionEnv,
}

/// `PUNKTFUNK_SESSION_WATCH` wins (truthy → on; `0`/`false`/`no`/`off`/empty → off). Unset defaults
/// on for Bazzite/SteamOS (they flip Gaming↔Desktop mid-stream) and off elsewhere.
fn session_watch_enabled() -> bool {
    match std::env::var("PUNKTFUNK_SESSION_WATCH") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        }
        Err(_) => is_steam_htpc_platform(),
    }
}

/// Bazzite or SteamOS (`ID`/`ID_LIKE`). Absent os-release (non-Linux) → false.
fn is_steam_htpc_platform() -> bool {
    let Ok(os) = std::fs::read_to_string("/etc/os-release") else {
        return false;
    };
    os.lines().any(|line| {
        let line = line.trim();
        let Some(val) = line
            .strip_prefix("ID=")
            .or_else(|| line.strip_prefix("ID_LIKE="))
        else {
            return false;
        };
        val.trim_matches('"')
            .split_whitespace()
            .any(|tok| tok.eq_ignore_ascii_case("bazzite") || tok.eq_ignore_ascii_case("steamos"))
    })
}

fn session_watcher_loop(tx: std::sync::mpsc::Sender<SessionSwitch>, stop: Arc<AtomicBool>) {
    use crate::vdisplay;
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(3);
    let mut current = vdisplay::detect_active_session().kind;
    let mut pending: Option<(vdisplay::ActiveKind, std::time::Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let active = vdisplay::detect_active_session();
        // Kind change OR same-kind restart: bump the epoch even when no SessionSwitch will fire.
        vdisplay::observe_session_instance(&active);
        let cur = active.kind;
        if cur == current {
            pending = None;
            continue;
        }
        match pending {
            Some((k, since)) if k == cur && since.elapsed() >= DEBOUNCE => {
                // Unmask before compositor_for_kind: a switch we cannot follow still has to unbar
                // "Return to Gaming Mode" (a masked autologin unit will not start).
                vdisplay::release_autologin_mask(cur);
                match vdisplay::compositor_for_kind(cur) {
                    Some(comp) => {
                        tracing::info!(from = ?current, to = ?cur, compositor = comp.id(),
                            "session watcher: mid-stream switch — signaling backend rebuild");
                        if tx
                            .send(SessionSwitch {
                                kind: cur,
                                compositor: comp,
                                env: active.env,
                            })
                            .is_err()
                        {
                            break;
                        }
                        current = cur;
                    }
                    None => tracing::debug!(to = ?cur,
                        "session watcher: no usable backend for the new session — staying put"),
                }
                pending = None;
            }
            Some((k, _)) if k == cur => {}
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}

/// Owned per-session inputs for [`virtual_stream`]. Receivers move in; the whole context moves
/// onto the stream thread.
pub(super) struct SessionContext {
    pub(super) session: Session,
    pub(super) mode: punktfunk_core::Mode,
    pub(super) seconds: u32,
    pub(super) stop: Arc<AtomicBool>,
    /// Set on `QUIT_CODE`. Display lease skips keep-alive linger for a user stop.
    pub(super) quit: Arc<AtomicBool>,
    pub(super) reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    pub(super) keyframe: std::sync::mpsc::Receiver<()>,
    /// Lost-frame range `(first, last)`. Prefer `invalidate_ref_frames` over a full IDR.
    pub(super) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    pub(super) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    /// Validated + ack-gated by the wire-MTU watcher. Applied between AUs only.
    pub(super) shard_rx: std::sync::mpsc::Receiver<usize>,
    pub(super) compositor: crate::vdisplay::Compositor,
    /// Per-instance, not via `PUNKTFUNK_GAMESCOPE_NODE` — two sessions must not overwrite each other.
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    /// Total wire budget (kbps): video + FEC + framing + audio reservation. PyroWave is identity.
    pub(super) bitrate_kbps: u32,
    pub(super) audio_reserved_kbps: u32,
    pub(super) shard_payload: u16,
    /// ASIC-applied rate, not the request. Shared with pacer, console, mgmt, and climb acks.
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// 0 = none discovered. A request already at the ceiling costs nothing to apply.
    pub(super) encoder_ceiling_kbps: Arc<AtomicU32>,
    /// While set, refuse bitrate climbs — the network is not the bottleneck.
    pub(super) cadence_degraded: Arc<AtomicBool>,
    pub(super) cadence_behind_score: Arc<AtomicU32>,
    /// [`u32::MAX`] = client too old to send a [`DeliveryReport`]. Distinguishes clean-link from
    /// nothing-arriving: both look like `loss_ppm = 0`.
    pub(super) client_packets_received: Arc<AtomicU32>,
    /// `Hello::bitrate_kbps == 0`. PyroWave re-resolves on a mid-stream mode switch; an explicit rate stays.
    pub(super) bitrate_auto: bool,
    /// 8 or 10. Does not imply HDR — `hdr` is separate (10-bit SDR path).
    pub(super) bit_depth: u8,
    pub(super) hdr: bool,
    pub(super) chroma: crate::encode::ChromaFormat,
    pub(super) codec: crate::encode::Codec,
    pub(super) probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    pub(super) probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    /// Corrective `Reconfigured` when a rebuild stayed at the old mode or honored a different refresh.
    pub(super) reconfig_result_tx: tokio::sync::mpsc::UnboundedSender<Reconfigured>,
    pub(super) retarget_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    pub(super) gap_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    pub(super) fec_target: Arc<AtomicU8>,
    pub(super) conn: quinn::Connection,
    pub(super) timing_conn: Option<quinn::Connection>,
    pub(super) phase: Arc<PhaseCtl>,
    pub(super) cursor_forward: bool,
    /// `true` = client draws; `false` = host composites. Always `true` (inert) for non-cap sessions.
    pub(super) cursor_client_draws: Arc<AtomicBool>,
    pub(super) cursor_shape_tx:
        tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::CursorShape>,
    /// Without this, a mid-session probe consumes video indexes the gap detector cannot see.
    pub(super) probe_seq: bool,
    pub(super) streamed_au: bool,
    /// `false` = single-slice. TV-SoC decoders (Amlogic) wedge on multi-slice.
    pub(super) multi_slice: bool,
    pub(super) stats: Arc<StatsRecorder>,
    pub(super) client_label: String,
    pub(super) client_name: Option<String>,
    pub(super) launch: Option<String>,
    pub(super) launch_target: Option<crate::library::LaunchTarget>,
    /// Threaded into the EDID CTA HDR block before `create` so host apps tone-map to the client's panel.
    pub(super) client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    pub(super) bringup: Arc<crate::bringup::Trace>,
    pub(super) resize_ms: Arc<AtomicU32>,
    #[cfg(target_os = "linux")]
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::input::ClientInput>,
    /// Isolated gamescope spawn identity. `None` = shared planes. See `design/gamescope-multiuser.md`.
    #[cfg(target_os = "linux")]
    pub(super) isolation: Option<crate::vdisplay::SessionIsolation>,
    #[cfg(target_os = "linux")]
    pub(super) input_route: super::input::InputRoute,
    #[cfg(target_os = "linux")]
    pub(super) inj_shared_tx: std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    #[cfg(target_os = "linux")]
    pub(super) inj_session_tx: Option<std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,
}

/// Isolated gamescope keeps its pinned injector and must not steal the shared backend
/// (last-write-wins). Everyone else gets the shared sender plus `set_backend_id`.
#[cfg(target_os = "linux")]
fn repoint_session_input(
    input_route: &super::input::InputRoute,
    shared: &std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    session: Option<&std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,
    compositor: crate::vdisplay::Compositor,
    route: Option<&crate::vdisplay::GamescopeRoute>,
) {
    match session.filter(|_| super::compositor::session_is_isolated(compositor, route)) {
        Some(tx) => input_route.set(tx.clone()),
        None => {
            input_route.set(shared.clone());
            crate::inject::set_backend_id(crate::vdisplay::input_backend_id(compositor));
        }
    }
}

/// Park the seat pointer at the streamed surface's centre, through the same injection path
/// client input takes.
///
/// A fresh virtual output leaves the seat wherever it last was. A relative-only client never
/// moves it onto the streamed output, so input lands on the wrong monitor and Mutter suppresses
/// `SPA_META_Cursor` while the pointer is off the recorded view. Retry only for relative-only
/// clients: a desktop-model client's own `MouseMoveAbs` is the retry, and a synthetic one fights it.
#[cfg(target_os = "linux")]
fn park_pointer(input_tx: &std::sync::mpsc::SyncSender<super::input::ClientInput>, w: u32, h: u32) {
    let ev = punktfunk_core::input::InputEvent {
        kind: punktfunk_core::input::InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x: (w / 2) as i32,
        y: (h / 2) as i32,
        flags: (w << 16) | (h & 0xffff),
    };
    // Best-effort: never block the stream loop behind a full input backlog.
    if input_tx
        .try_send(super::input::ClientInput::Event(ev))
        .is_ok()
    {
        tracing::info!(
            w,
            h,
            "parked the seat pointer at the streamed surface's centre"
        );
    }
}

/// Settle the cursor plan against the portal's negotiated mode. Shared by bring-up and capture-loss
/// rebuild so the two cannot drift.
///
/// Returns whether "no cursor overlay" still means the pointer is off the streamed output, and
/// clears `metadata_composite` when the negotiated mode makes it a fiction. wlr portals
/// (`Hidden|Embedded`) paint the pointer into the frames and never send `SPA_META_Cursor`, so a
/// metadata composite can never be fed and "no overlay" is not a park signal. `None` (KWin,
/// Mutter, gamescope, Windows) leaves both answers alone.
///
/// Does not undo [`SessionPlan::cursor_blend`]: blend is resolved before `create`.
#[cfg(target_os = "linux")]
fn settle_portal_cursor(
    vd: &dyn crate::vdisplay::VirtualDisplay,
    metadata_composite: &mut bool,
) -> bool {
    let Some(negotiated) = vd.last_portal_cursor_mode() else {
        return true;
    };
    if negotiated.delivers_metadata() {
        return true;
    }
    if *metadata_composite {
        *metadata_composite = false;
        tracing::info!(
            negotiated = negotiated.name(),
            "the portal negotiated a cursor mode that carries no cursor metadata — dropping the \
             host composite; the pointer in this stream is the compositor's own, burnt into the \
             frames"
        );
    }
    false
}

/// `(gamescope_composite, metadata_composite)` for the live compositor. Shared by bring-up and
/// mid-stream retarget so they cannot drift. `gamescope` is the live compositor, not the original.
fn composite_plan(
    plan: &crate::session_plan::SessionPlan,
    has_cursor_channel: bool,
    gamescope: bool,
) -> (bool, bool) {
    (
        plan.gamescope_cursor && !has_cursor_channel,
        !has_cursor_channel && plan.cursor_blend && !gamescope,
    )
}

pub(super) fn virtual_stream(ctx: SessionContext, prepared: Option<PreparedDisplay>) -> Result<()> {
    boost_thread_priority(true);
    let mut plan = crate::session_plan::SessionPlan::resolve(
        ctx.bit_depth,
        ctx.hdr,
        ctx.chroma,
        ctx.codec,
        crate::session_plan::cursor_blend_for(
            ctx.cursor_forward,
            ctx.compositor == pf_vdisplay::Compositor::Gamescope,
            ctx.codec,
            ctx.bit_depth,
        ),
        ctx.cursor_forward,
        ctx.multi_slice,
    );
    // After resolve: a self-painting gamescope node would otherwise get a second XFixes pointer.
    plan.gamescope_cursor = crate::session_plan::gamescope_cursor_for(
        ctx.compositor == pf_vdisplay::Compositor::Gamescope,
    );
    if ctx.codec == crate::encode::Codec::PyroWave {
        plan.wire_chunk = Some(ctx.session.shard_payload());
    }
    tracing::info!(?plan, "resolved session plan");
    let SessionContext {
        session,
        mode,
        seconds,
        stop,
        quit,
        reconfig,
        keyframe,
        rfi,
        bitrate_rx,
        shard_rx,
        compositor,
        gamescope_route,
        mut bitrate_kbps,
        audio_reserved_kbps,
        shard_payload,
        live_bitrate,
        encoder_ceiling_kbps,
        cadence_degraded,
        cadence_behind_score,
        client_packets_received,
        bitrate_auto,
        bit_depth,
        hdr,
        chroma: _,
        codec: _,
        probe_rx,
        probe_result_tx,
        reconfig_result_tx,
        retarget_tx,
        gap_tx,
        fec_target,
        conn,
        timing_conn,
        phase,
        cursor_forward,
        cursor_shape_tx,
        cursor_client_draws,
        probe_seq,
        streamed_au,
        multi_slice,
        stats,
        client_label,
        client_name,
        launch,
        launch_target,
        client_hdr,
        bringup,
        resize_ms,
        #[cfg(target_os = "linux")]
        input_tx,
        #[cfg(target_os = "linux")]
        isolation,
        #[cfg(target_os = "linux")]
        input_route,
        #[cfg(target_os = "linux")]
        inj_shared_tx,
        #[cfg(target_os = "linux")]
        inj_session_tx,
    } = ctx;
    #[cfg(target_os = "windows")]
    let _ = &gamescope_route;
    // Stamp before the display exists: a reading after launch would reject the process it is meant to find.
    let fresh_stamp = crate::gamelease::launch_clock();
    // Re-dial re-sends `Hello::launch` verbatim. Adopt against the original stamp or procscan refuses it.
    let launch_claim = launch_target.as_ref().map(|t| {
        crate::launchreg::claim(
            endpoint::peer_fingerprint(&conn)
                .map(hex::encode)
                .as_deref(),
            t.game.id.as_deref(),
            fresh_stamp,
        )
    });
    let launch_stamp = launch_claim.as_ref().map_or(fresh_stamp, |c| c.stamp());
    // `PUNKTFUNK_STREAMED_AU=0` reverts to whole-AU sends. Encoder chunking is per-AU.
    // `bitrate_kbps` is the total wire budget; only encoder opens convert via EncDerive.
    let budget_identity = plan.codec == crate::encode::Codec::PyroWave;
    let enc_derive = move |fec: u8| super::EncDerive {
        audio_kbps: audio_reserved_kbps,
        shard_payload,
        fec_percent: fec,
        identity: budget_identity,
    };
    let streamed_wire = streamed_au && std::env::var("PUNKTFUNK_STREAMED_AU").as_deref() != Ok("0");
    let slice_wire = streamed_wire
        && multi_slice
        && std::env::var("PUNKTFUNK_SLICE_STREAM").as_deref() != Ok("0");
    let mut cursor_fwd = cursor_forward.then(super::cursor_fwd::CursorForwarder::new);
    // Starts true so the first composite request triggers the capturer hook.
    let mut cursor_client_drew = true;
    if cursor_forward {
        tracing::info!("cursor channel negotiated — forwarding shape/state, encoder blend off");
    }
    let (mut gamescope_composite, mut metadata_composite) = composite_plan(
        &plan,
        cursor_fwd.is_some(),
        compositor == pf_vdisplay::Compositor::Gamescope,
    );
    if gamescope_composite {
        tracing::info!("gamescope cursor: compositing the XFixes-sourced pointer into the video");
    }
    if metadata_composite {
        tracing::info!(
            "no cursor channel — compositing the metadata cursor into the video (embedded \
             fallback is unreliable on virtual streams)"
        );
    }
    if streamed_wire {
        tracing::info!(
            "client accepts streamed AUs (VIDEO_CAP_STREAMED_AU) — used if this session's \
             encoder supports chunked output"
        );
    }
    // Adopt a mode accepted before bring-up and build once. Two RecordVirtual monitors ~400 ms
    // apart segfault mutter inside `meta_monitor_manager_rebuild`. Prepared pipelines stay as-is.
    let mut mode = mode;
    let mut adopted_at_bringup = false;
    if prepared.is_none() {
        let mut queued = None;
        while let Ok(m) = reconfig.try_recv() {
            queued = Some(m);
        }
        if let Some(m) = queued.filter(|m| *m != mode) {
            adopted_at_bringup = true;
            tracing::info!(
                stale = ?mode,
                adopted = ?m,
                "a mode switch was accepted before bring-up finished — building at the new mode \
                 instead of building twice"
            );
            mode = m;
            if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                bitrate_kbps =
                    resolve_bitrate_kbps_for(plan.codec, 0, &mode, plan.chroma, plan.bit_depth);
            }
        }
    }
    tracing::info!(
        compositor = compositor.id(),
        ?mode,
        bitrate_kbps,
        bit_depth,
        "punktfunk/1 virtual display"
    );
    let (mut vd, pipe) = match prepared {
        Some(p) => (p.vd, p.pipeline),
        None => {
            // Open first: Windows `open` inits the manager; `vdm()` before that panics.
            let mut vd = crate::vdisplay::open(compositor)?;
            vd.set_client_identity(endpoint::peer_fingerprint(&conn));
            vd.set_client_hdr(client_hdr);
            // HDR verdict, not the depth — a 10-bit SDR session leaves the output SDR.
            vd.set_hdr(hdr);
            vd.set_hw_cursor(cursor_forward || metadata_composite);
            vd.set_quit_flag(quit.clone());
            #[cfg(not(target_os = "windows"))]
            vd.set_launch_command(launch.clone());
            #[cfg(not(target_os = "windows"))]
            vd.set_gamescope_route(gamescope_route.clone());
            #[cfg(target_os = "linux")]
            vd.set_session_isolation(isolation.clone());
            // Slot-scoped: preempt only a prior session on THIS client's slot. Held before create.
            #[cfg(target_os = "windows")]
            let _idd_setup_guard = (plan.capture == crate::session_plan::CaptureBackend::IddPush)
                .then(|| {
                    let slot = crate::vdisplay::manager::slot_id_for(
                        endpoint::peer_fingerprint(&conn),
                        (mode.width, mode.height),
                    );
                    crate::vdisplay::manager::vdm().begin_idd_setup(slot, stop.clone())
                });
            let pipe = build_pipeline_with_retry(
                &mut vd,
                mode,
                bitrate_kbps,
                bitrate_auto,
                bit_depth,
                enc_derive(fec_target.load(Ordering::Relaxed)),
                plan,
                &quit,
                &stop,
                8,
                Some(bringup.as_ref()),
            )?;
            (vd, pipe)
        }
    };
    let (
        mut capturer,
        mut enc,
        mut frame,
        mut interval,
        mut cur_node_id,
        mut cur_display_gen,
        built_bitrate,
    ) = pipe;
    // Source can change format/size with no client Reconfigure; in-place encoder reset cannot follow.
    let mut enc_src = (frame.format, frame.width, frame.height);
    #[cfg(target_os = "linux")]
    let mut no_overlay_means_off_output = settle_portal_cursor(&*vd, &mut metadata_composite);
    adopt_built_bitrate(
        &mut bitrate_kbps,
        built_bitrate,
        &live_bitrate,
        &retarget_tx,
    );
    if adopted_at_bringup {
        let actual = delivered_mode(frame.width, frame.height, interval);
        if actual != mode {
            let _ = reconfig_result_tx.send(Reconfigured {
                accepted: true,
                mode: actual,
            });
        }
    }

    // Once per launch, not per session. Mid-stream rebuilds must not re-spawn.
    let adopt_launch = launch_claim.as_ref().is_some_and(|c| !c.must_spawn());
    #[allow(unused_mut)]
    let mut spawned_now = false;
    // A forwarder's pid (`WinRecipe::owns_game` false) is not a lifetime signal.
    #[allow(unused_mut)]
    let mut spawned_pid: Option<u32> = None;
    if !adopt_launch {
        if let Some(t) = launch_target.as_ref() {
            crate::gamelease::end_others_for_new_launch(
                endpoint::peer_fingerprint(&conn)
                    .map(hex::encode)
                    .as_deref(),
                t.game.id.as_deref(),
            );
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(id) = launch.as_deref() {
        if adopt_launch {
            tracing::info!(
                launch_id = id,
                "this client's copy of this title is already running from an earlier session — not \
                 starting a second one"
            );
        } else {
            match crate::library::launch_title(id) {
                Ok(launched) => {
                    spawned_pid = launched.tracked_pid();
                    spawned_now = true;
                }
                Err(e) => {
                    tracing::warn!(launch_id = id, error = %e, "could not launch requested library title")
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    let spawned_launch = match launch.as_deref() {
        Some(cmd) if adopt_launch => {
            tracing::info!(
                command = %cmd,
                "this client's copy of this title is already running from an earlier session — not \
                 starting a second one"
            );
            None
        }
        Some(cmd) if crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref()) => {
            tracing::info!(command = %cmd, "launch nested into the per-session gamescope");
            spawned_now = true;
            None
        }
        Some(cmd) => match crate::library::launch_session_command(compositor, cmd) {
            Ok(spawned) => {
                spawned_now = true;
                Some(spawned)
            }
            Err(e) => {
                tracing::warn!(command = %cmd, error = %e, "could not launch requested title into the session");
                None
            }
        },
        None => None,
    };
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = (&launch, adopt_launch);
    if let Some(c) = launch_claim.as_ref() {
        if spawned_now {
            c.launched();
        } else if c.must_spawn() {
            c.abandon();
        }
    }

    let game_lease = launch_target.as_ref().map(|target| {
        #[cfg(target_os = "linux")]
        let nested = crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref());
        #[cfg(not(target_os = "linux"))]
        let nested = false;
        #[cfg(target_os = "linux")]
        let child = spawned_launch.map(|s| (s.child, s.group_leader));
        #[cfg(not(target_os = "linux"))]
        let child = None;

        let on_exit: crate::gamelease::OnExit = {
            let conn = conn.clone();
            let stop = stop.clone();
            let quit = quit.clone();
            Box::new(move || {
                if !crate::session_settings::get().session_on_game_exit {
                    tracing::info!(
                        "the launched game exited, but ending the session on game exit is off — \
                         leaving the stream up"
                    );
                    return;
                }
                tracing::info!(
                    "the launched game exited — ending the session cleanly (APP_EXITED)"
                );
                conn.close(
                    punktfunk_core::quic::APP_EXITED_CLOSE_CODE.into(),
                    b"game exited",
                );
                quit.store(true, Ordering::SeqCst);
                stop.store(true, Ordering::SeqCst);
            })
        };
        crate::gamelease::open(
            crate::gamelease::LeaseRequest {
                game: target.game.clone(),
                client: client_label.clone(),
                plane: crate::events::Plane::Native,
                spec: target.detect.clone(),
                nested,
                launcher: target.launcher,
                child,
                spawned: spawned_pid,
                launch_stamp,
                procs: launch_claim.as_ref().and_then(|c| c.procs()),
            },
            on_exit,
        )
    });
    let game_shared = game_lease.as_ref().map(|l| l.shared());
    // Declared first so it drops after `_live_session`: `session.ended` then game policy.
    let _game_life = game_lease.map(|lease| {
        crate::gamelease::SessionGuard::new(
            lease,
            quit.clone(),
            endpoint::peer_fingerprint(&conn).map(hex::encode),
            launch_claim,
        )
    });

    let perf = pf_host_config::config().perf;
    let burst_cap: Option<usize> = std::env::var("PUNKTFUNK_PACE_BURST_KB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|kb| kb * 1024);

    // Depth 3: encode blocks if send falls behind, rather than drop a frame (infinite GOP freeze).
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
    // Stats slot only — an ordinary connect is not owed a corrective Reconfigured.
    let delivered = delivered_mode(frame.width, frame.height, interval);
    let live_mode = Arc::new(AtomicU64::new(pack_mode(
        delivered.width,
        delivered.height,
        delivered.refresh_hz,
    )));
    let force_idr = Arc::new(AtomicBool::new(false));
    let send_spread_us = Arc::new(AtomicU32::new(0));
    let send_spread_send = Arc::clone(&send_spread_us);
    let wire_rekeys = Arc::new(AtomicU32::new(0));
    let wire_rekeys_send = Arc::clone(&wire_rekeys);
    let send_stats = SendStats {
        rec: stats.clone(),
        mode: live_mode.clone(),
        codec: plan.codec.label(),
        client: client_label.clone(),
        bitrate_kbps: live_bitrate.clone(),
        bringup: bringup.clone(),
    };
    let send_thread = std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn({
            let stop = stop.clone();
            let phase_send = phase.clone();
            let fec_target_send = fec_target.clone();
            move || {
                send_loop(
                    session,
                    frame_rx,
                    probe_rx,
                    probe_result_tx,
                    stop,
                    perf,
                    send_spread_send,
                    wire_rekeys_send,
                    slice_wire,
                    burst_cap,
                    fec_target_send,
                    shard_rx,
                    send_stats,
                    timing_conn,
                    phase_send,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    let capture_health: Arc<std::sync::Mutex<Option<pf_capture::CaptureHealth>>> =
        Arc::new(std::sync::Mutex::new(None));
    let _live_session = crate::session_status::register(crate::session_status::Registration {
        mode: live_mode.clone(),
        bitrate_kbps: live_bitrate.clone(),
        codec: plan.codec,
        stop: stop.clone(),
        quit: quit.clone(),
        force_idr: force_idr.clone(),
        client: client_label,
        client_name,
        hdr: plan.hdr,
        ttff_ms: bringup.total_slot(),
        last_resize_ms: resize_ms.clone(),
        game: game_shared,
        capture_health: capture_health.clone(),
    });
    // Capture-health publish cadence (WP18): `/status` polls at 2 s; twice a second is plenty
    // and keeps the report's clone off the per-frame path.
    let mut health_published_at = std::time::Instant::now();

    let mut compositor = compositor;
    let (session_tx, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();
    let watch = session_watch_enabled() && pf_host_config::config().compositor.is_none();
    let _watcher = if watch {
        tracing::info!("session watcher on — following a mid-stream Gaming↔Desktop switch");
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("punktfunk1-watcher".into())
            .spawn(move || session_watcher_loop(session_tx, stop))
            .ok()
    } else {
        None
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut next = std::time::Instant::now();
    let mut sent: u64 = 0;
    // Loop-local: survives in-loop rebuilds so a mid-stream rebuild keeps the acquired lock.
    let mut phase_ctl = PhaseController::new();
    // Same: a rebuild must not reopen the overshoot. Bounded burst is all a rebuild gap can buy.
    let mut pace = CaptureCredit::new(std::time::Instant::now());
    // Predicted as `au_seq + inflight.len()`. Encoder-internal counters desync on the first ABR rebuild.
    let mut au_seq: u32 = 0;
    let mut cur_mode = mode;
    const MAX_CAPTURE_REBUILDS: u32 = 5;
    let mut capture_rebuilds: u32 = 0;
    #[cfg(target_os = "windows")]
    let mut seen_reassert_gen = crate::vdisplay::manager::topology_reassert_gen();
    // Non-blocking poll returning None forever while submits succeed. 2 s also sizes the backlog bound.
    const ENCODE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX_ENCODER_RESETS: u32 = 5;
    let mut encoder_resets: u32 = 0;
    let mut last_au_at = std::time::Instant::now();
    let mut last_hdr_meta: Option<punktfunk_core::quic::HdrMeta> = None;
    let mut inflight: std::collections::VecDeque<(u64, u64, std::time::Instant)> =
        std::collections::VecDeque::new();
    // Diagnostic: distinguish NEW captured frames (the source produced a fresh frame) from REPEATS (the
    // loop re-encoded the last frame because `try_latest` had nothing). A low new-frame rate at a high
    // send rate ⇒ the capture source isn't producing frames (e.g. an IDD virtual display DWM isn't
    // compositing), NOT an encoder problem. Logged every 2 s when `PUNKTFUNK_PERF`.
    let (mut diag_new, mut diag_repeat, mut diag_regen) = (0u64, 0u64, 0u64);
    // Seat-pointer park schedule (see `park_pointer`): per (re)built display, and re-armed by
    // the capture-model flip. More than one attempt for a RELATIVE-ONLY session, because the
    // first park can land on a still-cold EIS connection (devices not yet resumed → the injector
    // DROPS it) — observed on-glass; the retry a second later goes through. A client that steers
    // the pointer itself gets the bring-up park only: its own absolute moves are the retry, and
    // a synthetic one on top of them is just a yank to centre. While the session is in the
    // capture model with no live cursor overlay, keep trying up to the cap: no overlay there
    // means the pointer still isn't on the streamed output, and a relative-only client can
    // never fix that itself — but only where an absent overlay is evidence of anything at all
    // (`no_overlay_means_off_output`, settled per display by `settle_portal_cursor`).
    #[cfg(target_os = "linux")]
    let mut parked_display = None;
    #[cfg(target_os = "linux")]
    let mut park_attempts: u32 = 0;
    #[cfg(target_os = "linux")]
    let mut next_park_at = std::time::Instant::now();
    #[cfg(target_os = "linux")]
    const PARK_ATTEMPTS_MAX: u32 = 10;
    #[cfg(not(target_os = "windows"))]
    let (mut composite_saw_overlay, mut composite_saw_none) = (false, false);
    let mut diag_at = std::time::Instant::now();
    // Pipeline opened on an IDR — start the clock so the cold-GOP keyframe storm coalesces.
    let mut last_forced_idr: Option<std::time::Instant> = Some(std::time::Instant::now());
    // Do not re-anchor the IDR cooldown here: sustained loss + RFI would swallow IDR pleas forever.
    let mut last_rfi: Option<std::time::Instant> = None;
    let mut rfi_echo_swallowed: u32 = 0;
    let mut last_kf_request: Option<std::time::Instant> = None;
    let mut recovery_cadence = pf_frame::metronome::Metronome::new();
    let mut ir_wave_pos: u32 = 0;
    let (mut st_cap, mut st_submit, mut st_wait, mut st_queue): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut cur_depth: usize = 1;
    let mut behind_score: u32 = 0;
    let mut last_fec = fec_target.load(Ordering::Relaxed);
    let mut depth_frames: u64 = 0;
    // EMA of real-frame arrivals. Negotiated refresh is the wrong deadline when the game is slower.
    let mut src_period_ns: Option<u64> = None;
    let mut last_real_cap: Option<std::time::Instant> = None;
    let mut was_degraded = false;
    let mut last_cadence_log: Option<std::time::Instant> = None;
    let mut cadence_flips_suppressed: u32 = 0;
    const CADENCE_LOG_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(5);
    let mut pipeline_asked = false;
    // ~20 net behind-frames (≈0.3 s) escalates; warmup skips the first ~1 s of bring-up.
    const DEPTH_ESCALATE: u32 = 20;
    const DEPTH_BEHIND_CAP: u32 = 60;
    const DEPTH_WARMUP_FRAMES: u64 = 60;
    const DEPTH_DEGRADE: u32 = 10;
    // ~5 s clean at 120 fps earns one wind-back. Backoff 1 → 5 → 25 min; never a permanent latch.
    const DEESCALATE_CLEAN_FRAMES: u32 = 600;
    const DEESCALATE_BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(60);
    const DEESCALATE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(25 * 60);
    let mut pipelined_active = false;
    let mut deescalating = false;
    let mut ahead_run: u32 = 0;
    let mut deescalate_not_before: Option<std::time::Instant> = None;
    let mut deescalate_backoff = DEESCALATE_BACKOFF_START;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        let mut switch = None;
        while let Ok(s) = session_rx.try_recv() {
            switch = Some(s);
        }
        if let Some(sw) = switch {
            if sw.compositor != compositor {
                tracing::info!(from = compositor.id(), to = sw.compositor.id(), kind = ?sw.kind,
                    "session switch — rebuilding backend in place");
                // Only writer is not safety: `setenv` races every concurrent `getenv` in the process.
                crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
                    kind: sw.kind,
                    env: sw.env,
                    compositor_pid: None,
                });
                let switched_route = crate::vdisplay::resolve_gamescope_route(sw.compositor, false);
                #[cfg(target_os = "linux")]
                repoint_session_input(
                    &input_route,
                    &inj_shared_tx,
                    inj_session_tx.as_ref(),
                    sw.compositor,
                    switched_route.as_ref(),
                );
                #[cfg(not(target_os = "linux"))]
                crate::inject::set_backend_id(crate::vdisplay::input_backend_id(sw.compositor));
                if matches!(
                    sw.compositor,
                    crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
                ) {
                    crate::vdisplay::settle_desktop_portal(sw.compositor);
                }
                let rebuilt =
                    (|| -> Result<(Box<dyn crate::vdisplay::VirtualDisplay>, Pipeline)> {
                        let mut new_vd = crate::vdisplay::open(sw.compositor)?;
                        new_vd.set_gamescope_route(switched_route.clone());
                        #[cfg(target_os = "linux")]
                        new_vd.set_session_isolation(isolation.clone());
                        let pipe = build_pipeline_with_retry(
                            &mut new_vd,
                            cur_mode,
                            bitrate_kbps,
                            bitrate_auto,
                            bit_depth,
                            enc_derive(fec_target.load(Ordering::Relaxed)),
                            plan,
                            &quit,
                            &stop,
                            8,
                            None,
                        )?;
                        Ok((new_vd, pipe))
                    })();
                match rebuilt {
                    Ok((
                        new_vd,
                        (
                            new_cap,
                            new_enc,
                            new_frame,
                            new_interval,
                            new_node_id,
                            new_gen,
                            new_bitrate,
                        ),
                    )) => {
                        capturer = new_cap;
                        enc = new_enc;
                        frame = new_frame;
                        interval = new_interval;
                        cur_node_id = new_node_id;
                        cur_display_gen = new_gen;
                        adopt_built_bitrate(
                            &mut bitrate_kbps,
                            new_bitrate,
                            &live_bitrate,
                            &retarget_tx,
                        );
                        vd = new_vd;
                        compositor = sw.compositor;
                        next = std::time::Instant::now();
                        inflight.clear();
                        last_au_at = std::time::Instant::now();
                        encoder_resets = 0;
                        tracing::info!(
                            compositor = compositor.id(),
                            "session switch — backend rebuilt, stream continues"
                        );
                    }
                    Err(e) => {
                        let chain = format!("{e:#}");
                        let kind = if is_permanent_build_error(&chain) {
                            "permanent"
                        } else {
                            "transient"
                        };
                        tracing::warn!(error = %chain, kind,
                            "session-switch rebuild failed — staying on the current backend");
                    }
                }
            }
        }
        let mut want = None;
        while let Ok(m) = reconfig.try_recv() {
            want = Some(m);
        }
        if let Some(new_mode) = want {
            tracing::info!(?new_mode, "rebuilding pipeline for mode switch");
            let resize_trace = crate::bringup::Trace::start("resize", resize_ms.clone());
            let mode_bitrate = if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                resolve_bitrate_kbps_for(plan.codec, 0, &new_mode, plan.chroma, plan.bit_depth)
            } else {
                bitrate_kbps
            };
            #[cfg(target_os = "windows")]
            let fast_done = plan.capture == crate::session_plan::CaptureBackend::IddPush
                && try_inplace_resize(
                    &mut vd,
                    &mut capturer,
                    &mut enc,
                    &mut frame,
                    &mut interval,
                    new_mode,
                    mode_bitrate,
                    bit_depth,
                    enc_derive(fec_target.load(Ordering::Relaxed)),
                    plan,
                    &quit,
                    resize_trace.as_ref(),
                    false,
                );
            #[cfg(not(target_os = "windows"))]
            let fast_done = false;
            let mut built_bitrate = mode_bitrate;
            let rebuilt = fast_done
                || match build_pipeline(
                    &mut vd,
                    new_mode,
                    mode_bitrate,
                    bitrate_auto,
                    bit_depth,
                    enc_derive(fec_target.load(Ordering::Relaxed)),
                    plan,
                    &quit,
                    cur_display_gen,
                    None,
                    Some(resize_trace.as_ref()),
                ) {
                    Ok(next_pipe) => {
                        let old_display_gen = cur_display_gen;
                        (
                            capturer,
                            enc,
                            frame,
                            interval,
                            cur_node_id,
                            cur_display_gen,
                            built_bitrate,
                        ) = next_pipe;
                        // Lease drop looks like a disconnect to keep-alive; retire or linger accumulates.
                        if let Some(g) = old_display_gen.filter(|g| cur_display_gen != Some(*g)) {
                            crate::vdisplay::registry::retire(g);
                        }
                        true
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), ?new_mode,
                            "mode-switch rebuild failed — staying on the current mode");
                        let _ = reconfig_result_tx.send(Reconfigured {
                            accepted: true,
                            mode: delivered_mode(frame.width, frame.height, interval),
                        });
                        false
                    }
                };
            if rebuilt {
                adopt_built_bitrate(
                    &mut bitrate_kbps,
                    built_bitrate,
                    &live_bitrate,
                    &retarget_tx,
                );
                cur_mode = new_mode;
                next = std::time::Instant::now();
                enc_src = (frame.format, frame.width, frame.height);
                let actual = delivered_mode(frame.width, frame.height, interval);
                live_mode.store(
                    pack_mode(actual.width, actual.height, actual.refresh_hz),
                    Ordering::Relaxed,
                );
                if actual != new_mode {
                    let _ = reconfig_result_tx.send(Reconfigured {
                        accepted: true,
                        mode: actual,
                    });
                }
                inflight.clear();
                last_au_at = std::time::Instant::now();
                encoder_resets = 0;
                last_forced_idr = Some(std::time::Instant::now());
                resize_trace.finish("pipeline_rebuilt");
                // Reconfigured clears baselines, not the straddling window or slow start.
                announce_pipeline_gap(&gap_tx, resize_trace.total_slot().load(Ordering::Relaxed));
            }
        }
        #[cfg(target_os = "windows")]
        if plan.capture == crate::session_plan::CaptureBackend::IddPush {
            let reassert_gen = crate::vdisplay::manager::topology_reassert_gen();
            if reassert_gen != seen_reassert_gen {
                seen_reassert_gen = reassert_gen;
                tracing::info!(
                    "exclusive-topology eviction bounced the virtual display's modes — rebuilding \
                     the capture attachment in place at the current mode"
                );
                let trace = crate::bringup::Trace::start("reassert-recover", resize_ms.clone());
                if try_inplace_resize(
                    &mut vd,
                    &mut capturer,
                    &mut enc,
                    &mut frame,
                    &mut interval,
                    cur_mode,
                    bitrate_kbps,
                    bit_depth,
                    enc_derive(fec_target.load(Ordering::Relaxed)),
                    plan,
                    &quit,
                    trace.as_ref(),
                    true,
                ) {
                    enc_src = (frame.format, frame.width, frame.height);
                    inflight.clear();
                    last_au_at = std::time::Instant::now();
                    encoder_resets = 0;
                    last_forced_idr = Some(std::time::Instant::now());
                    trace.finish("pipeline_rebuilt");
                    announce_pipeline_gap(&gap_tx, trace.total_slot().load(Ordering::Relaxed));
                } else {
                    return Err(anyhow!(
                        "exclusive-topology eviction recovery failed — ending the session for a \
                         clean reconnect (a fresh bring-up re-attaches capture)"
                    ));
                }
            }
        }
        if !budget_identity {
            let fec_now = fec_target.load(Ordering::Relaxed);
            if fec_now != last_fec {
                let prev = enc_derive(last_fec).enc_kbps(bitrate_kbps);
                let want = enc_derive(fec_now).enc_kbps(bitrate_kbps);
                last_fec = fec_now;
                if want != prev && enc.reconfigure_bitrate(want as u64 * 1000) {
                    tracing::debug!(
                        fec_pct = fec_now,
                        encoder_kbps = want,
                        budget_kbps = bitrate_kbps,
                        "adaptive FEC moved — encoder rate re-derived within the wire budget"
                    );
                }
            }
        }
        let mut want_kbps = None;
        while let Ok(k) = bitrate_rx.try_recv() {
            want_kbps = Some(k);
        }
        enc.set_send_spread_us(send_spread_us.load(Ordering::Relaxed));
        if let Some(k) = want_kbps.as_mut() {
            let ceiling = encoder_ceiling_kbps.load(Ordering::Relaxed);
            if ceiling != 0 && *k > ceiling {
                tracing::info!(
                    requested_kbps = *k,
                    ceiling_kbps = ceiling,
                    "bitrate request clamped to the known encoder ceiling"
                );
                *k = ceiling;
            }
        }
        if let Some(new_kbps) = want_kbps.filter(|&k| k != bitrate_kbps) {
            let ed = enc_derive(fec_target.load(Ordering::Relaxed));
            if enc.reconfigure_bitrate(ed.enc_kbps(new_kbps) as u64 * 1000) {
                let applied_kbps = enc
                    .applied_bitrate_bps()
                    .map(|b| (b / 1000) as u32)
                    .filter(|&k| k > 0)
                    .map(|k| ed.applied_budget_kbps(new_kbps, k))
                    .unwrap_or(new_kbps);
                tracing::info!(
                    from_kbps = bitrate_kbps,
                    to_kbps = applied_kbps,
                    requested_kbps = new_kbps,
                    "encoder bitrate reconfigured in place (adaptive bitrate — no IDR)"
                );
                if applied_kbps < new_kbps {
                    encoder_ceiling_kbps.store(applied_kbps, Ordering::Relaxed);
                    let _ = retarget_tx.send(applied_kbps);
                }
                if applied_kbps < bitrate_kbps {
                    behind_score = 0;
                }
                bitrate_kbps = applied_kbps;
                live_bitrate.store(applied_kbps, Ordering::Relaxed);
            } else {
                let hz = interval_hz(interval);
                let rebuild_t0 = std::time::Instant::now();
                match crate::encode::open_video(
                    plan.codec,
                    frame.format,
                    frame.width,
                    frame.height,
                    hz,
                    ed.enc_kbps(new_kbps) as u64 * 1000,
                    frame.is_cuda(),
                    bit_depth,
                    plan.chroma,
                    plan.cursor_blend,
                    plan.max_slices,
                ) {
                    Ok(mut new_enc) => {
                        let applied_kbps = new_enc
                            .applied_bitrate_bps()
                            .map(|b| (b / 1000) as u32)
                            .filter(|&k| k > 0)
                            .map(|k| ed.applied_budget_kbps(new_kbps, k))
                            .unwrap_or(new_kbps);
                        tracing::info!(
                            from_kbps = bitrate_kbps,
                            to_kbps = applied_kbps,
                            requested_kbps = new_kbps,
                            "encoder rebuilt at new bitrate (adaptive bitrate)"
                        );
                        if let Some(c) = plan.wire_chunk {
                            new_enc.set_wire_chunking(c);
                        }
                        new_enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
                        enc = new_enc;
                        if applied_kbps < new_kbps {
                            encoder_ceiling_kbps.store(applied_kbps, Ordering::Relaxed);
                            let _ = retarget_tx.send(applied_kbps);
                        }
                        bitrate_kbps = applied_kbps;
                        live_bitrate.store(applied_kbps, Ordering::Relaxed);
                        inflight.clear();
                        last_au_at = std::time::Instant::now();
                        encoder_resets = 0;
                        last_forced_idr = Some(std::time::Instant::now());
                        behind_score = 0;
                        depth_frames = 0;
                        ahead_run = 0;
                        announce_pipeline_gap(
                            &gap_tx,
                            rebuild_t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), to_kbps = new_kbps,
                            "bitrate-change encoder rebuild failed — keeping the current rate");
                        let _ = retarget_tx.send(bitrate_kbps);
                    }
                }
            }
        }
        let mut want_kf = false;
        // Staged recovery closed on real source frames (WP14): the client held the last image
        // through the hole, so the next AU is an IDR, owed in-flight records are invalid, and
        // the measured local outage is announced so the straddling window is not scored as
        // congestion.
        if health_published_at.elapsed() >= std::time::Duration::from_millis(500) {
            health_published_at = std::time::Instant::now();
            *capture_health.lock().unwrap_or_else(|e| e.into_inner()) = capturer.health();
        }
        if let Some(outage) = capturer.take_recovered_outage() {
            let outage_ms = outage.as_millis().min(u32::MAX as u128) as u32;
            tracing::info!(
                outage_ms,
                "capture recovered from a source stall — forcing an IDR, announcing the gap"
            );
            want_kf = true;
            inflight.clear();
            last_forced_idr = Some(std::time::Instant::now());
            announce_pipeline_gap(&gap_tx, outage_ms);
        }
        while keyframe.try_recv().is_ok() {
            want_kf = true;
        }
        if force_idr.swap(false, Ordering::Relaxed) {
            want_kf = true;
        }
        let mut rfi_range: Option<(u32, u32)> = None;
        while let Ok((first, last)) = rfi.try_recv() {
            rfi_range = Some(match rfi_range {
                Some((pf, pl)) => (pf.min(first), pl.max(last)),
                None => (first, last),
            });
        }
        if plan.codec == crate::encode::Codec::PyroWave && (want_kf || rfi_range.is_some()) {
            tracing::debug!(
                want_kf,
                ?rfi_range,
                "PyroWave session: recovery request ignored (all-intra — next frame is the recovery)"
            );
            want_kf = false;
            rfi_range = None;
        }
        if !want_kf {
            if let Some((first, last)) = rfi_range {
                let width = last.wrapping_sub(first);
                if width > punktfunk_core::packet::RFI_MAX_RANGE {
                    tracing::debug!(first, last, width, "RFI range too wide — keyframe instead");
                    want_kf = true;
                } else if enc.caps().supports_rfi
                    && enc.invalidate_ref_frames(first as i64, last as i64)
                {
                    last_rfi = Some(std::time::Instant::now());
                } else {
                    want_kf = true;
                }
            }
        }
        if want_kf {
            // One forced IDR per cooldown. Intra-refresh heals over ~0.5 s (2 s window); full-IDR
            // needs a shorter window — swallow the round-trip echo, re-issue a lost IDR promptly.
            const IDR_COOLDOWN_INTRA: std::time::Duration = std::time::Duration::from_secs(2);
            const IDR_COOLDOWN_FULL: std::time::Duration = std::time::Duration::from_millis(750);
            const RFI_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);
            const RFI_ECHO_MAX_SWALLOWED: u32 = 2;
            const KF_EPISODE_RESET: std::time::Duration = std::time::Duration::from_secs(1);
            let window = if enc.caps().intra_refresh {
                IDR_COOLDOWN_INTRA
            } else {
                IDR_COOLDOWN_FULL
            };
            let now = std::time::Instant::now();
            if last_kf_request.is_some_and(|t| now.duration_since(t) > KF_EPISODE_RESET) {
                rfi_echo_swallowed = 0;
            }
            last_kf_request = Some(now);
            let idr_recent = last_forced_idr.is_some_and(|t| t.elapsed() < window);
            let rfi_echo = last_rfi.is_some_and(|t| t.elapsed() < RFI_ECHO_WINDOW)
                && rfi_echo_swallowed < RFI_ECHO_MAX_SWALLOWED;
            if idr_recent {
                // In-flight IDR has not repaired the client yet — do not RFI-anchor over that damage.
                enc.distrust_references();
                tracing::debug!(
                    "keyframe request coalesced — within the IDR cooldown; RFI anchor trust \
                     withdrawn until the IDR repairs the client"
                );
            } else if rfi_echo {
                // Do not distrust: the recovery frame is still in flight. Hedge is RFI_ECHO_MAX_SWALLOWED.
                rfi_echo_swallowed += 1;
                tracing::debug!(
                    swallowed = rfi_echo_swallowed,
                    "keyframe request coalesced — echo of an RFI-recovered loss"
                );
            } else {
                let rfi_unhealed = rfi_echo_swallowed > 0;
                tracing::debug!(rfi_unhealed, "forcing keyframe (client decode recovery)");
                if rfi_unhealed {
                    enc.distrust_references();
                }
                enc.request_keyframe();
                last_forced_idr = Some(now);
                rfi_echo_swallowed = 0;
                if let Some(period) = recovery_cadence.note(now) {
                    let client_rx = client_packets_received.load(Ordering::Relaxed);
                    if client_rx == 0 {
                        tracing::error!(
                            period_s = format!("{:.1}", period.as_secs_f64()),
                            frames_sent = sent,
                            "THE VIDEO DATA PLANE IS NOT REACHING THE CLIENT — it reports 0 \
                             packets received all session while this host has sent the frames \
                             counted here, so the picture is black and every keyframe we force is \
                             wasted. The control plane is healthy (this report arrived on it), so \
                             the session looks alive: audio, input and the library keep working. \
                             READ THE 'data plane bound' LINE ABOVE — it says which leg failed, \
                             and this line cannot. `punched=false`: the client's hole-punch never \
                             arrived, so inbound UDP to this host's per-session data port is \
                             blocked — open it (the ports are ephemeral, so the rule must be \
                             program-scoped, not port-scoped). `punched=true`: inbound is FINE and \
                             the failure is on the return leg — compare that line's `local=` \
                             source address against the host address this client dialed, because \
                             its data socket is connected and its kernel silently drops video from \
                             any other source. If those match, the datagrams left this host \
                             correctly and the client either never received them (a hop on the \
                             path) or received them and could not open them: this counter is \
                             incremented AFTER decrypt and replay checks, so a session whose every \
                             datagram failed to open reports exactly this same zero"
                        );
                    } else if matches_client_recovery_cooldown(period) {
                        if client_rx == u32::MAX {
                            tracing::warn!(
                                period_s = format!("{:.1}", period.as_secs_f64()),
                                frames_sent = sent,
                                "client keyframe recoveries land on a client software cooldown, \
                                 but this client is too old to report whether any video reached \
                                 it — so this is EITHER a client that cannot sustain the stream \
                                 and is shedding a standing receive queue, OR a client that has \
                                 received nothing at all and is re-asking on its no-video timer. \
                                 They are opposite faults; the host cannot tell them apart from \
                                 the period. Its log does: 'receive backlog stopped draining' \
                                 (with queue_depth) means the first, 'no video received … into \
                                 the session' means the second. Upgrading the client makes this \
                                 line decide on its own"
                            );
                        } else {
                            tracing::warn!(
                                period_s = format!("{:.1}", period.as_secs_f64()),
                                client_packets_received = client_rx,
                                "client keyframe recoveries match the client's jump-to-live \
                                 cooldown, and it confirms video IS arriving — the CLIENT cannot \
                                 sustain the stream and is shedding a standing receive queue \
                                 (check its log for 'receive backlog stopped draining' with \
                                 queue_depth, and for a decode rung that demoted); a slower \
                                 decode path or a link below the bitrate does this, and it is NOT \
                                 a host display disturbance"
                            );
                        }
                    } else if wire_rekeys.load(Ordering::Relaxed) > 0 {
                        tracing::warn!(
                            period_s = format!("{:.1}", period.as_secs_f64()),
                            wire_rekeys = wire_rekeys.load(Ordering::Relaxed),
                            "client keyframe recoveries are METRONOMIC on a session whose wire \
                             MTU had to be re-keyed mid-stream — a constrained path (VPN/overlay \
                             adapter, lowered NIC MTU) black-holing full-size video is the prime \
                             suspect, NOT a host/display disturbance; see the 'wire MTU' lines \
                             above, and pin PUNKTFUNK_WIRE_MTU to skip the lossy discovery window \
                             on this path"
                        );
                    } else {
                        tracing::warn!(
                            period_s = format!("{:.1}", period.as_secs_f64()),
                            "client keyframe recoveries are METRONOMIC — a periodic host/display \
                             disturbance (display-topology churn, display-poller software, \
                             virtual-display timing) is the likely cause, not random network \
                             loss; correlate with 'slow display-descriptor poll' / 'display \
                             descriptor changed' / 'IDD-push capture stall' lines"
                        );
                    }
                }
            }
        }
        let measure = perf || stats.is_armed();
        let t_cap = std::time::Instant::now();
        let cap_result = capturer.try_latest();
        let cap_us = if measure {
            t_cap.elapsed().as_micros() as u32
        } else {
            0
        };
        if perf {
            st_cap.push(cap_us);
        }
        let mut repeat = false;
        match cap_result {
            Ok(Some(f)) => {
                // Only a real SOURCE frame is evidence of source progress: a cursor-only
                // regeneration re-encodes the previous desktop image at a new pointer
                // position — encoded and sent like any frame, but never fed to the cadence
                // estimate, new-frame diagnostics, or capture-rebuild reset (a regenerated
                // cursor over one stashed texture is how a dead path used to look healthy).
                let source = f.provenance.origin == pf_frame::FrameOrigin::Source;
                frame = f;
                if source {
                    diag_new += 1;
                } else {
                    diag_regen += 1;
                }
                // Source-cadence estimate (see the declaration above): `t_cap` on the
                // frame-driven path is taken right after `wait_arrival` wakes, so real-frame
                // deltas track the game's actual delivery spacing. Deltas past 8×interval are
                // a gap/hitch (mid-rebuild, alt-tab), not cadence — skipped, not averaged in.
                if source {
                    if let Some(prev) = last_real_cap {
                        let d = t_cap.duration_since(prev).as_nanos() as u64;
                        if d <= interval.as_nanos() as u64 * 8 {
                            src_period_ns = Some(match src_period_ns {
                                Some(e) => (e as i64 + (d as i64 - e as i64) / 8) as u64,
                                None => d,
                            });
                        }
                    }
                    last_real_cap = Some(t_cap);
                }
                // Phase-locked capture: hold the fresh frame so its ARRIVAL at the client lands a
                // constant small lead before the client's display latch (§3 hold-then-submit; the
                // capture slot is newest-wins, so a long hold samples fresher content next tick,
                // never staler). Adjusted ~1 Hz from the client's PhaseReports; 0 until a report
                // arrives or when PUNKTFUNK_PHASE_LOCK=0.
                if phase_lock_enabled() {
                    if phase_ctl.due() {
                        if let Some(r) = phase.take() {
                            phase_ctl.adjust(&r, interval.as_nanos() as i64);
                        } else {
                            phase_ctl.last_adjust = std::time::Instant::now();
                        }
                        phase.set_applied(phase_ctl.applied_readout());
                    }
                    if let Some(t) = phase_ctl
                        .next_submit_target(std::time::Instant::now(), interval.as_nanos() as i64)
                    {
                        let now = std::time::Instant::now();
                        if t > now {
                            std::thread::sleep(t.duration_since(now));
                        }
                    }
                }
                if source {
                    capture_rebuilds = 0; // a delivered SOURCE frame clears the loss counter
                }
                // Re-arm the park schedule for a (re)built display: pin the seat pointer to
                // the streamed surface (see `park_pointer` and the schedule state above).
                // Not gamescope — its nested seat owns the pointer and its cursor comes from
                // the XFixes source regardless of seat position.
                #[cfg(target_os = "linux")]
                if compositor != pf_vdisplay::Compositor::Gamescope
                    && parked_display != Some((cur_node_id, cur_display_gen))
                {
                    parked_display = Some((cur_node_id, cur_display_gen));
                    park_attempts = 0;
                    next_park_at = std::time::Instant::now();
                }
            }
            Ok(None) => {
                diag_repeat += 1;
                repeat = true;
            }
            Err(e) => {
                #[cfg(not(target_os = "linux"))]
                let _ = &cur_node_id;
                #[cfg(target_os = "linux")]
                if launch.is_some()
                    && crate::session_settings::get().session_on_game_exit
                    && crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref())
                    && crate::vdisplay::dedicated_game_exited(cur_node_id)
                {
                    tracing::info!(
                        "dedicated game session: the game exited — ending the session cleanly"
                    );
                    quit.store(true, Ordering::SeqCst);
                    conn.close(
                        punktfunk_core::quic::APP_EXITED_CLOSE_CODE.into(),
                        b"game exited",
                    );
                    break;
                }
                capture_rebuilds += 1;
                if capture_rebuilds > MAX_CAPTURE_REBUILDS {
                    return Err(e).context("capture lost — rebuild attempts exhausted");
                }
                tracing::warn!(error = %format!("{e:#}"), rebuild = capture_rebuilds,
                    "capture lost — rebuilding pipeline in place");
                // A Bazzite/SteamOS Gaming↔Desktop switch tears the old compositor down and can
                // take 15 s+ to bring the new one up. Don't fail the session over that — keep
                // retrying within a budget while the QUIC keepalive holds the connection,
                // RE-DETECTING the live compositor each attempt (same or different kind). The
                // client stays connected, frozen on the last frame, and resumes — no reconnect.
                const REBUILD_BUDGET: std::time::Duration = std::time::Duration::from_secs(40);
                // A managed/attach gamescope (re)launch legitimately takes up to 45 s (the Steam
                // Big Picture cold start), so the 40 s budget used to expire INSIDE the first
                // attempt — a single-shot failure where a second, warm attempt would have
                // succeeded. Gamescope-targeted rebuilds get room for two full launches; checked
                // per iteration because the loop retargets `compositor` as re-detection follows.
                const GAMESCOPE_REBUILD_BUDGET: std::time::Duration =
                    std::time::Duration::from_secs(100);
                // Attach-only holdoff: right after a capture loss the session detection can be
                // STALE, and a rebuild acting on a stale "Gaming" answer restarts
                // gamescope-session.target — on SteamOS that steals the seat back from the
                // session the user just switched to (observed live). Until it lapses, builds
                // attach to live outputs only: never stop/relaunch/take over sessions.
                const PROBE_HOLDOFF: std::time::Duration = std::time::Duration::from_secs(4);
                let loss_at = std::time::Instant::now();
                if pf_host_config::config().compositor.is_some() {
                    let active = crate::vdisplay::detect_active_session();
                    if crate::vdisplay::compositor_for_kind(active.kind) != Some(compositor) {
                        tracing::warn!(
                            pinned = compositor.id(),
                            live = ?active.kind,
                            "capture lost while PUNKTFUNK_COMPOSITOR pins the backend and the \
                             live session no longer matches it — the pin disables \
                             session-following, so this rebuild can only retry the pinned \
                             backend; remove the pin to let the stream follow session switches"
                        );
                    }
                }
                let (
                    new_cap,
                    new_enc,
                    new_frame,
                    new_interval,
                    new_node_id,
                    new_display_gen,
                    new_bitrate,
                ) = loop {
                    if pf_host_config::config().compositor.is_none() {
                        let active = crate::vdisplay::detect_active_session();
                        crate::vdisplay::observe_session_instance(&active);
                        if let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) {
                            crate::vdisplay::apply_session_env(&active);
                            let rebuilt_route = crate::vdisplay::resolve_gamescope_route(c, false);
                            #[cfg(target_os = "linux")]
                            repoint_session_input(
                                &input_route,
                                &inj_shared_tx,
                                inj_session_tx.as_ref(),
                                c,
                                rebuilt_route.as_ref(),
                            );
                            #[cfg(not(target_os = "linux"))]
                            crate::inject::set_backend_id(crate::vdisplay::input_backend_id(c));
                            if c != compositor {
                                if matches!(
                                    c,
                                    crate::vdisplay::Compositor::Kwin
                                        | crate::vdisplay::Compositor::Mutter
                                ) {
                                    crate::vdisplay::settle_desktop_portal(c);
                                }
                                match crate::vdisplay::open(c) {
                                    Ok(v) => {
                                        tracing::info!(from = compositor.id(), to = c.id(),
                                            "capture loss: active session switched compositor — retargeting");
                                        vd = v;
                                        compositor = c;
                                        plan.cursor_blend = crate::session_plan::cursor_blend_for(
                                            plan.cursor_forward,
                                            c == crate::vdisplay::Compositor::Gamescope,
                                            plan.codec,
                                            plan.bit_depth,
                                        );
                                        plan.gamescope_cursor =
                                            crate::session_plan::gamescope_cursor_for(
                                                c == crate::vdisplay::Compositor::Gamescope,
                                            );
                                        (gamescope_composite, metadata_composite) = composite_plan(
                                            &plan,
                                            cursor_fwd.is_some(),
                                            c == crate::vdisplay::Compositor::Gamescope,
                                        );
                                        vd.set_hw_cursor(plan.cursor_forward || metadata_composite);
                                    }
                                    Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                                        "capture loss: opening the newly-detected compositor failed — retrying"),
                                }
                            }
                            vd.set_gamescope_route(rebuilt_route.clone());
                            #[cfg(target_os = "linux")]
                            vd.set_session_isolation(isolation.clone());
                        }
                    }
                    let _probe = (loss_at.elapsed() < PROBE_HOLDOFF)
                        .then(crate::vdisplay::rebuild_probe_scope);
                    match build_pipeline_with_retry(
                        &mut vd,
                        cur_mode,
                        bitrate_kbps,
                        bitrate_auto,
                        bit_depth,
                        enc_derive(fec_target.load(Ordering::Relaxed)),
                        plan,
                        &quit,
                        &stop,
                        1,
                        None,
                    ) {
                        Ok(p) => break p,
                        Err(e2) => {
                            let budget = if compositor == crate::vdisplay::Compositor::Gamescope {
                                GAMESCOPE_REBUILD_BUDGET
                            } else {
                                REBUILD_BUDGET
                            };
                            if stop.load(Ordering::SeqCst)
                                || std::time::Instant::now() >= loss_at + budget
                            {
                                return Err(e2)
                                    .context("capture lost — no compositor came up within the rebuild budget");
                            }
                            tracing::warn!(error = %format!("{e2:#}"),
                                "capture lost — new session not up yet, retrying");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                };
                capturer = new_cap;
                enc = new_enc;
                frame = new_frame;
                interval = new_interval;
                cur_node_id = new_node_id;
                cur_display_gen = new_display_gen;
                enc_src = (frame.format, frame.width, frame.height);
                #[cfg(target_os = "linux")]
                {
                    no_overlay_means_off_output =
                        settle_portal_cursor(&*vd, &mut metadata_composite);
                }
                adopt_built_bitrate(&mut bitrate_kbps, new_bitrate, &live_bitrate, &retarget_tx);
                enc.request_keyframe();
                last_forced_idr = Some(std::time::Instant::now());
                next = std::time::Instant::now();
                inflight.clear();
                last_au_at = std::time::Instant::now();
                encoder_resets = 0;
                tracing::info!(
                    compositor = compositor.id(),
                    "capture loss: pipeline rebuilt — stream resumes"
                );
            }
        }
        if let Some(fwd) = cursor_fwd.as_mut() {
            let client_draws = cursor_client_draws.load(Ordering::Relaxed);
            capturer.set_cursor_forward(client_draws);
            if client_draws != cursor_client_drew {
                cursor_client_drew = client_draws;
                tracing::info!(
                    client_draws,
                    "cursor render mode flipped ({})",
                    if client_draws {
                        "client draws — exclude + forward"
                    } else {
                        "host composites"
                    }
                );
                #[cfg(target_os = "linux")]
                if !client_draws {
                    park_attempts = 0;
                    next_park_at = std::time::Instant::now();
                }
            }
            if client_draws {
                let live = capturer.cursor();
                fwd.tick(
                    live.as_ref().or(frame.cursor.as_ref()),
                    &conn,
                    &cursor_shape_tx,
                );
                frame.cursor = None;
            } else {
                #[cfg(not(target_os = "windows"))]
                {
                    match capturer.cursor() {
                        Some(live) => {
                            if !composite_saw_overlay {
                                composite_saw_overlay = true;
                                tracing::info!(
                                    x = live.x,
                                    y = live.y,
                                    w = live.w,
                                    h = live.h,
                                    visible = live.visible,
                                    "host-composite: first live cursor overlay handed to the \
                                     encoder blend"
                                );
                            }
                            frame.cursor = Some(live);
                        }
                        None => {
                            if !composite_saw_none {
                                composite_saw_none = true;
                                tracing::info!(
                                    "host-composite active but the capture has no live cursor \
                                     overlay (no SPA_META_Cursor bitmap) — nothing for the encoder \
                                     blend to draw; the pointer, if any, is the compositor's own"
                                );
                            }
                        }
                    }
                }
            }
        } else if gamescope_composite || metadata_composite {
            #[cfg(not(target_os = "windows"))]
            match capturer.cursor() {
                Some(live) => {
                    if !composite_saw_overlay {
                        composite_saw_overlay = true;
                        tracing::info!(
                            x = live.x,
                            y = live.y,
                            w = live.w,
                            h = live.h,
                            visible = live.visible,
                            "host-composite: first live cursor overlay handed to the encoder \
                             blend"
                        );
                    }
                    frame.cursor = Some(live);
                }
                None => {
                    if !composite_saw_none {
                        composite_saw_none = true;
                        tracing::info!(
                            "host-composite active but the capture has no live cursor overlay \
                             yet (no SPA_META_Cursor bitmap) — the stream is cursorless until \
                             one arrives"
                        );
                    }
                }
            }
        }
        if frame.cursor.as_ref().is_some_and(|c| !c.visible) {
            frame.cursor = None;
        }
        #[cfg(target_os = "linux")]
        if compositor != pf_vdisplay::Compositor::Gamescope
            && park_attempts < PARK_ATTEMPTS_MAX
            && std::time::Instant::now() >= next_park_at
        {
            let client_steers = cursor_fwd.is_some() && cursor_client_draws.load(Ordering::Relaxed);
            let unconditional = if client_steers { 1 } else { 2 };
            let composite_starved = ((cursor_fwd.is_some() && !client_steers)
                || metadata_composite)
                && capturer.cursor().is_none()
                && no_overlay_means_off_output;
            if park_attempts < unconditional || composite_starved {
                park_pointer(&input_tx, frame.width, frame.height);
                park_attempts += 1;
                next_park_at = std::time::Instant::now() + std::time::Duration::from_secs(1);
            } else {
                park_attempts = PARK_ATTEMPTS_MAX;
            }
        }
        if perf && diag_at.elapsed() >= std::time::Duration::from_secs(2) {
            let secs = diag_at.elapsed().as_secs_f64();
            tracing::info!(
                new_fps = format!("{:.0}", diag_new as f64 / secs),
                repeat_fps = format!("{:.0}", diag_repeat as f64 / secs),
                regen_fps = format!("{:.0}", diag_regen as f64 / secs),
                "capture diag: NEW frames from the source vs REPEATS vs cursor REGENS (low new_fps \
                 at high send rate ⇒ the source isn't producing frames, not an encode stall; \
                 regens alone are a cursor over a frozen image)"
            );
            let wait_max = st_wait.iter().copied().max().unwrap_or(0);
            tracing::info!(
                queue_us_p50 = percentile(&mut st_queue, 0.50),
                queue_us_p99 = percentile(&mut st_queue, 0.99),
                cap_us_p50 = percentile(&mut st_cap, 0.50),
                cap_us_p99 = percentile(&mut st_cap, 0.99),
                submit_us_p50 = percentile(&mut st_submit, 0.50),
                submit_us_p99 = percentile(&mut st_submit, 0.99),
                wait_us_p50 = percentile(&mut st_wait, 0.50),
                wait_us_p99 = percentile(&mut st_wait, 0.99),
                wait_us_max = wait_max,
                "stage perf (µs/call): queue=delivery→submit cap=try_latest(ring+convert) submit=encode_picture wait=lock_bitstream(sched+ASIC)"
            );
            st_cap.clear();
            st_submit.clear();
            st_wait.clear();
            st_queue.clear();
            diag_new = 0;
            diag_repeat = 0;
            diag_regen = 0;
            diag_at = std::time::Instant::now();
        }
        if enc_src != (frame.format, frame.width, frame.height) {
            let actual = delivered_mode(frame.width, frame.height, interval);
            let src_kbps = if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                resolve_bitrate_kbps_for(plan.codec, 0, &actual, plan.chroma, plan.bit_depth)
            } else {
                bitrate_kbps
            };
            let opened = crate::encode::open_video(
                plan.codec,
                frame.format,
                frame.width,
                frame.height,
                actual.refresh_hz,
                src_kbps as u64 * 1000,
                frame.is_cuda(),
                bit_depth,
                plan.chroma,
                plan.cursor_blend,
                plan.max_slices,
            )
            .with_context(|| {
                format!(
                    "the capture source changed to {}x{} {:?} mid-session and the encoder could not \
                     be reopened at it",
                    frame.width, frame.height, frame.format
                )
            });
            let mut new_enc = match opened {
                Ok(e) => e,
                Err(e) => {
                    encoder_resets += 1;
                    if encoder_resets > MAX_ENCODER_RESETS {
                        return Err(e).context("encoder reopen at the source's new mode");
                    }
                    let backoff = std::cmp::max(
                        interval,
                        std::time::Duration::from_millis(100u64 << (encoder_resets - 1).min(4)),
                    );
                    tracing::warn!(error = %format!("{e:#}"), reset = encoder_resets,
                        max = MAX_ENCODER_RESETS,
                        "reopening the encoder at the source's new mode failed — retrying");
                    next = std::time::Instant::now() + backoff;
                    std::thread::sleep(backoff);
                    continue;
                }
            };
            if let Some(c) = plan.wire_chunk {
                new_enc.set_wire_chunking(c);
            }
            new_enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
            tracing::info!(
                from = %format!("{}x{} {:?}", enc_src.1, enc_src.2, enc_src.0),
                to = %format!("{}x{} {:?}", frame.width, frame.height, frame.format),
                "the capture source changed mode mid-session with no client reconfigure — reopened \
                 the encoder at the delivered size"
            );
            enc = new_enc;
            enc_src = (frame.format, frame.width, frame.height);
            adopt_built_bitrate(&mut bitrate_kbps, src_kbps, &live_bitrate, &retarget_tx);
            inflight.clear();
            last_au_at = std::time::Instant::now();
            encoder_resets = 0;
            last_forced_idr = Some(std::time::Instant::now());
            live_mode.store(
                pack_mode(actual.width, actual.height, actual.refresh_hz),
                Ordering::Relaxed,
            );
            let _ = reconfig_result_tx.send(Reconfigured {
                accepted: true,
                mode: actual,
            });
        }
        let hdr_meta = capturer.hdr_meta().map(|m| client_hdr.unwrap_or(m));
        enc.set_hdr_meta(hdr_meta);
        let mut resend_meta = hdr_meta != last_hdr_meta;
        if resend_meta {
            last_hdr_meta = hdr_meta;
        }
        let max_depth = capturer.pipeline_depth().max(1);
        let depth = if idd_adaptive_enabled() {
            cur_depth.clamp(1, max_depth)
        } else {
            max_depth
        };
        let submit_ns = now_ns();
        // SyntheticCapturer counts from 0, not the epoch — fall back to "now".
        let age_ns = submit_ns.saturating_sub(frame.pts_ns);
        let plausible = frame.pts_ns > 0 && frame.pts_ns <= submit_ns && age_ns < 10_000_000_000;
        let (capture_ns, queue_us) = if !repeat && plausible {
            (frame.pts_ns, (age_ns / 1000) as u32)
        } else {
            (submit_ns, 0)
        };
        if perf && !repeat {
            st_queue.push(queue_us);
        }
        let t_submit = std::time::Instant::now();
        let wire_index = au_seq.wrapping_add(inflight.len() as u32);
        if let Err(e) = enc.submit_indexed(&frame, wire_index) {
            if e.downcast_ref::<crate::encode::TerminalEncoderError>()
                .is_some()
            {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "encoder failed with a deterministic configuration error — ending the video \
                     session without rebuild attempts (see the error for the remedy)");
                return Err(e).context("encoder submit");
            }
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS
                || !reset_stalled_encoder(&mut enc, &mut inflight)
            {
                tracing::error!(
                    error = %format!("{e:#}"),
                    resets = encoder_resets,
                    "encoder did not recover after repeated in-place rebuilds — ending the video \
                     session (see the error above for the cause)");
                return Err(e).context("encoder submit");
            }
            tracing::warn!(error = %format!("{e:#}"), reset = encoder_resets,
                max = MAX_ENCODER_RESETS,
                "encoder submit failed — encoder rebuilt in place, forcing an IDR");
            last_au_at = std::time::Instant::now();
            // 100 ms → 1.6 s. One frame period burns all 5 resets within 40 ms at 120 Hz.
            let backoff = std::cmp::max(
                interval,
                std::time::Duration::from_millis(100u64 << (encoder_resets - 1).min(4)),
            );
            next = std::time::Instant::now() + backoff;
            std::thread::sleep(backoff);
            continue;
        }
        let submit_us = if measure {
            t_submit.elapsed().as_micros() as u32
        } else {
            0
        };
        if perf {
            st_submit.push(submit_us);
        }
        next = if frame_driven_enabled() && capturer.supports_arrival_wait() {
            pace.charge();
            std::time::Instant::now() + interval
        } else {
            next + interval
        };
        inflight.push_back((capture_ns, submit_ns, next));
        let mut send_gone = false;
        let mut poll_err: Option<anyhow::Error> = None;
        while inflight.len() >= depth {
            if streamed_wire && enc.supports_chunked_poll() {
                let t_wait = std::time::Instant::now();
                let mut first_chunk_us = 0u32;
                let mut au_flags = 0u32;
                let mut au_done = false;
                loop {
                    let c = match enc.poll_chunk() {
                        Ok(Some(c)) => c,
                        Ok(None) => break,
                        Err(e) => {
                            poll_err = Some(e);
                            break;
                        }
                    };
                    last_au_at = std::time::Instant::now();
                    encoder_resets = 0;
                    if c.first {
                        first_chunk_us = t_wait.elapsed().as_micros() as u32;
                        au_flags = if c.keyframe {
                            (FLAG_PIC | FLAG_SOF) as u32
                        } else {
                            FLAG_PIC as u32
                        };
                        let caps = enc.caps();
                        if caps.intra_refresh_recovery
                            && caps.intra_refresh_period > 0
                            && mark_recovery_boundary(
                                &mut ir_wave_pos,
                                c.keyframe,
                                caps.intra_refresh_period,
                            )
                        {
                            au_flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
                        }
                        if c.recovery_anchor {
                            au_flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
                        }
                        if c.chunk_aligned {
                            au_flags |= punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED;
                        }
                        if let Some(m) = last_hdr_meta {
                            if c.keyframe || resend_meta {
                                let _ = conn.send_datagram(
                                    punktfunk_core::quic::encode_hdr_meta_datagram(&m).into(),
                                );
                                resend_meta = false;
                            }
                        }
                        bringup.mark("first_au");
                    }
                    let last = c.last;
                    let (cap_ns, sub_ns, deadline) = *inflight.front().expect("inflight non-empty");
                    let wait_total_us = t_wait.elapsed().as_micros() as u32;
                    let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
                    let msg = ChunkMsg {
                        data: c.data,
                        first: c.first,
                        last,
                        capture_ns: cap_ns,
                        flags: au_flags,
                        frame_index: au_seq,
                        deadline,
                        encode_us,
                        queue_us,
                        cap_us,
                        submit_us,
                        wait_us: if measure { wait_total_us } else { 0 },
                        repeat,
                        was_measured: measure,
                    };
                    if frame_tx.send(SendMsg::Chunk(msg)).is_err() {
                        send_gone = true;
                        break;
                    }
                    if last {
                        inflight.pop_front();
                        au_seq = au_seq.wrapping_add(1);
                        sent += 1;
                        au_done = true;
                        if perf {
                            st_wait.push(wait_total_us);
                            if sent % 120 == 0 {
                                tracing::info!(
                                    first_slice_us = first_chunk_us,
                                    encode_us,
                                    "streamed AU (sampled): first slice handed to send at \
                                     first_slice_us; encode finished at encode_us"
                                );
                            }
                        }
                        break;
                    }
                }
                if send_gone || poll_err.is_some() {
                    break;
                }
                if au_done {
                    continue;
                }
                break;
            }
            let t_wait = std::time::Instant::now();
            let polled = enc.poll();
            let wait_us = if measure {
                t_wait.elapsed().as_micros() as u32
            } else {
                0
            };
            if perf {
                st_wait.push(wait_us);
            }
            let au = match polled {
                Ok(Some(au)) => au,
                Ok(None) => break,
                Err(e) => {
                    poll_err = Some(e);
                    break;
                }
            };
            last_au_at = std::time::Instant::now();
            encoder_resets = 0;
            let (cap_ns, sub_ns, deadline) = inflight.pop_front().expect("inflight non-empty");
            let mut flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            let caps = enc.caps();
            if caps.intra_refresh_recovery
                && caps.intra_refresh_period > 0
                && mark_recovery_boundary(&mut ir_wave_pos, au.keyframe, caps.intra_refresh_period)
            {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
            }
            if au.recovery_anchor {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
            }
            if au.chunk_aligned {
                flags |= punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED;
            }
            if let Some(m) = last_hdr_meta {
                if au.keyframe || resend_meta {
                    let _ = conn
                        .send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&m).into());
                    resend_meta = false;
                }
            }
            let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
            let msg = FrameMsg {
                data: au.data,
                capture_ns: cap_ns,
                flags,
                frame_index: au_seq,
                deadline,
                encode_us,
                queue_us,
                cap_us,
                submit_us,
                wait_us,
                repeat,
                was_measured: measure,
            };
            bringup.mark("first_au");
            if frame_tx.send(SendMsg::Frame(msg)).is_err() {
                send_gone = true;
                break;
            }
            au_seq = au_seq.wrapping_add(1);
            sent += 1;
        }
        if send_gone {
            break;
        }
        let stall_window = ENCODE_STALL_WINDOW.max(interval * 8);
        let stall_backlog =
            depth + (stall_window.as_secs_f64() / interval.as_secs_f64().max(1e-6)).ceil() as usize;
        if poll_err.is_some()
            || (!inflight.is_empty()
                && (last_au_at.elapsed() >= stall_window || inflight.len() > stall_backlog))
        {
            let why = match &poll_err {
                Some(e) => format!("poll failed: {e:#}"),
                None => format!(
                    "no AU for {} ms with {} frame(s) in flight",
                    last_au_at.elapsed().as_millis(),
                    inflight.len()
                ),
            };
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS
                || !reset_stalled_encoder(&mut enc, &mut inflight)
            {
                return Err(poll_err.unwrap_or_else(|| anyhow!("{why}")))
                    .context("encoder stalled — in-place rebuild unavailable or exhausted");
            }
            tracing::warn!(reset = encoder_resets, max = MAX_ENCODER_RESETS, %why,
                "encode stall detected — encoder rebuilt in place, forcing an IDR");
            last_au_at = std::time::Instant::now();
        }
        if idd_adaptive_enabled() {
            depth_frames += 1;
            if depth_frames > DEPTH_WARMUP_FRAMES {
                let budget = cadence_budget(interval, src_period_ns);
                let behind = std::time::Instant::now() >= next + (budget - interval);
                behind_score = if behind {
                    (behind_score + 1).min(DEPTH_BEHIND_CAP)
                } else {
                    behind_score.saturating_sub(1)
                };
                let escalated = cur_depth > 1 || pipelined_active || deescalating;
                let degraded = encode_behind_cadence(escalated, behind_score, DEPTH_DEGRADE);
                cadence_degraded.store(degraded, Ordering::Relaxed);
                cadence_behind_score.store(behind_score, Ordering::Relaxed);
                if degraded != was_degraded {
                    let now = std::time::Instant::now();
                    if last_cadence_log.is_none_or(|t| now.duration_since(t) >= CADENCE_LOG_MIN_GAP)
                    {
                        let budget = cadence_budget(interval, src_period_ns);
                        if degraded {
                            tracing::info!(
                                behind_score,
                                escalated,
                                budget_us = budget.as_micros() as u64,
                                interval_us = interval.as_micros() as u64,
                                src_period_us =
                                    src_period_ns.map(|p| p / 1_000).unwrap_or_default(),
                                flips_suppressed = cadence_flips_suppressed,
                                "encode behind cadence — ABR climbs will be refused until it \
                                 recovers"
                            );
                        } else {
                            tracing::info!(
                                behind_score,
                                flips_suppressed = cadence_flips_suppressed,
                                "encode cadence recovered — ABR climbs allowed again"
                            );
                        }
                        last_cadence_log = Some(now);
                        cadence_flips_suppressed = 0;
                    } else {
                        cadence_flips_suppressed += 1;
                    }
                    was_degraded = degraded;
                }
                if deescalating {
                    if !enc.set_pipelined(false) {
                        deescalating = false;
                        pipelined_active = false;
                        pipeline_asked = false;
                        tracing::info!(
                            "encoder pipelined retrieve de-escalated — sync retrieve (and \
                             sub-frame streaming, where armed) restored; re-monitoring cadence"
                        );
                        behind_score = 0;
                        depth_frames = 0;
                        ahead_run = 0;
                    }
                } else if behind_score >= DEPTH_ESCALATE
                    && (cur_depth < max_depth || !pipeline_asked)
                {
                    if cur_depth < max_depth {
                        cur_depth = max_depth;
                        tracing::info!(
                            depth = cur_depth,
                            "IDD pipeline depth escalated — encode can't hold cadence at depth-1 \
                             (GPU contention); pipelining until cadence holds clean (latency \
                             trade for throughput)"
                        );
                    } else {
                        pipeline_asked = true;
                        pipelined_active = enc.set_pipelined(true);
                        if pipelined_active {
                            tracing::info!(
                                "encoder pipelined retrieve escalated — encode can't hold \
                                 cadence and the capturer has no depth to give; the encode wait \
                                 moves off the loop until cadence holds clean (latency trade \
                                 for throughput)"
                            );
                        }
                    }
                    behind_score = 0;
                    ahead_run = 0;
                } else if escalated {
                    ahead_run = if behind { 0 } else { ahead_run + 1 };
                    if ahead_run >= DEESCALATE_CLEAN_FRAMES
                        && deescalate_not_before.is_none_or(|t| std::time::Instant::now() >= t)
                    {
                        ahead_run = 0;
                        deescalate_not_before =
                            Some(std::time::Instant::now() + deescalate_backoff);
                        deescalate_backoff = (deescalate_backoff * 5).min(DEESCALATE_BACKOFF_MAX);
                        if pipelined_active {
                            tracing::info!(
                                "cadence held clean while escalated — winding the pipelined \
                                 retrieve back (latency recovery; costs one IDR)"
                            );
                            deescalating = true;
                        } else if cur_depth > 1 {
                            cur_depth = 1;
                            tracing::info!(
                                depth = cur_depth,
                                "IDD pipeline depth de-escalated — cadence held clean at the \
                                 escalated depth (latency recovery)"
                            );
                            behind_score = 0;
                            depth_frames = 0;
                        }
                    }
                }
            }
        }
        if frame_driven_enabled() && capturer.supports_arrival_wait() {
            // Anchor the 0.9× floor to `t_cap`, not `next`: a sync encoder folds encode into cadence.
            let earliest = std::cmp::max(
                t_cap + interval.mul_f32(0.9),
                pace.earliest(std::time::Instant::now(), interval),
            );
            if let Some(d) = earliest.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(d);
            }
            capturer.wait_arrival(next + interval.mul_f32(0.5));
        } else {
            match next.checked_duration_since(std::time::Instant::now()) {
                Some(d) => std::thread::sleep(d),
                None => next = std::time::Instant::now(),
            }
        }
    }
    while let Some((cap_ns, sub_ns, deadline)) = inflight.pop_front() {
        let Ok(Some(au)) = enc.poll() else { break };
        let flags = if au.keyframe {
            (FLAG_PIC | FLAG_SOF) as u32
        } else {
            FLAG_PIC as u32
        };
        let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
        let msg = FrameMsg {
            data: au.data,
            capture_ns: cap_ns,
            flags,
            frame_index: au_seq,
            deadline,
            encode_us,
            queue_us: 0,
            cap_us: 0,
            submit_us: 0,
            wait_us: 0,
            repeat: false,
            was_measured: false,
        };
        if frame_tx.send(SendMsg::Frame(msg)).is_err() {
            break;
        }
        au_seq = au_seq.wrapping_add(1);
        sent += 1;
    }
    drop(frame_tx);
    let _ = send_thread.join();
    tracing::info!(sent, "punktfunk/1 virtual stream complete");
    Ok(())
}

/// ±10 % of [`FLUSH_COOLDOWN`]: a software cooldown is the most periodic thing in the system.
fn matches_client_flush_cadence(period: std::time::Duration) -> bool {
    let flush = punktfunk_core::client::FLUSH_COOLDOWN;
    period.abs_diff(flush) < flush / 10
}

/// ±10 % of [`NO_VIDEO_RETRY`]. Opposite fault from flush cadence; only the delivery count tells which.
fn matches_client_no_video_cadence(period: std::time::Duration) -> bool {
    let no_video = punktfunk_core::client::NO_VIDEO_RETRY;
    period.abs_diff(no_video) < no_video / 10
}

fn matches_client_recovery_cooldown(period: std::time::Duration) -> bool {
    matches_client_flush_cadence(period) || matches_client_no_video_cadence(period)
}

/// (capturer, encoder, first frame, interval, node id, pool gen, opened bitrate kbps).
type Pipeline = (
    Box<dyn crate::capture::Capturer>,
    Box<dyn crate::encode::Encoder>,
    crate::capture::CapturedFrame,
    std::time::Duration,
    u32,
    Option<u64>,
    u32,
);

/// Mode-set the live monitor, resize the ring, swap only the encoder. `false` → full rebuild.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn try_inplace_resize(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    capturer: &mut Box<dyn crate::capture::Capturer>,
    enc: &mut Box<dyn crate::encode::Encoder>,
    frame: &mut crate::capture::CapturedFrame,
    interval: &mut std::time::Duration,
    new_mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    enc_of: super::EncDerive,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
    recover_ring: bool,
) -> bool {
    let Some(cur_target) = capturer.capture_target_id() else {
        return false;
    };
    let new_display_mode = display_mode_for(new_mode);
    let vout = match crate::vdisplay::registry::acquire(vd, new_display_mode, quit.clone(), None) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "in-place resize: acquire failed");
            return false;
        }
    };
    trace.mark("display_resized");
    let achieved_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(new_display_mode.refresh_hz);
    let effective_hz = pacing_hz(new_mode.refresh_hz, achieved_hz);
    if vout.win_capture.as_ref().map(|t| t.target_id) != Some(cur_target) {
        tracing::info!(
            "resize: monitor re-arrived (no in-place support) — running the full pipeline rebuild"
        );
        return false;
    }
    let ring_ok = if recover_ring {
        capturer.recreate_ring_in_place()
    } else {
        capturer.resize_output(new_mode.width, new_mode.height)
    };
    if !ring_ok {
        return false;
    }
    trace.mark("ring_recreated");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let new_frame = loop {
        match capturer.try_latest() {
            Ok(Some(f)) if (f.width, f.height) == (new_mode.width, new_mode.height) => break f,
            Ok(_) => {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        "resize: no new-size frame within 3s of the in-place mode set — running \
                         the full pipeline rebuild"
                    );
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"),
                    "resize: capture failed after the in-place mode set — running the full rebuild");
                return false;
            }
        }
    };
    // First frame is the stash (~50 ms). A second, newer present proves the OS resumed presenting.
    let new_frame = if recover_ring {
        // SOURCE-sequence evidence, not wall-clock PTS — see `source_advanced`.
        let first_seq = new_frame.provenance.source_seq;
        let first_pts = new_frame.pts_ns;
        let live_deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match capturer.try_latest() {
                Ok(Some(f)) if source_advanced(first_seq, first_pts, &f.provenance, f.pts_ns) => {
                    break f
                }
                Ok(_) => {
                    if std::time::Instant::now() >= live_deadline {
                        tracing::warn!(
                            "eviction recovery: ring re-attached but only the stashed frame \
                             arrived — the OS is not presenting; failing the in-place recovery"
                        );
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "eviction recovery: capture failed while waiting for a live frame");
                    return false;
                }
            }
        }
    } else {
        new_frame
    };
    trace.mark("first_new_frame");
    let mut new_enc = match crate::encode::open_video(
        plan.codec,
        new_frame.format,
        new_frame.width,
        new_frame.height,
        effective_hz,
        enc_of.enc_kbps(bitrate_kbps) as u64 * 1000,
        new_frame.is_cuda(),
        bit_depth,
        plan.chroma,
        plan.cursor_blend,
        plan.max_slices,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"),
                "resize: encoder open failed after the in-place mode set — running the full rebuild");
            return false;
        }
    };
    if let Some(c) = plan.wire_chunk {
        new_enc.set_wire_chunking(c);
    }
    new_enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    *enc = new_enc;
    *frame = new_frame;
    *interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    trace.mark("encoder_open");
    true
}

/// Display + pipeline built on the prep thread while Start RTT and hole-punch are in flight.
pub(super) struct PreparedDisplay {
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) pipeline: Pipeline,
}

/// Prep thread: sender delivers [`SessionContext`]; drop un-received aborts into keep-alive.
pub(super) type PrepHandle = (
    std::sync::mpsc::SyncSender<SessionContext>,
    std::thread::JoinHandle<Result<()>>,
);

/// Build display + pipeline at Welcome time. Same setters as `virtual_stream`'s inline arm.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_display(
    compositor: crate::vdisplay::Compositor,
    mode: punktfunk_core::Mode,
    client_identity: Option<[u8; 32]>,
    client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    cursor_forward: bool,
    multi_slice: bool,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    hdr: bool,
    enc_of: super::EncDerive,
    chroma: crate::encode::ChromaFormat,
    codec: crate::encode::Codec,
    shard_payload: u16,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
) -> Result<PreparedDisplay> {
    let mut plan = crate::session_plan::SessionPlan::resolve(
        bit_depth,
        hdr,
        chroma,
        codec,
        crate::session_plan::cursor_blend_for(
            cursor_forward,
            compositor == pf_vdisplay::Compositor::Gamescope,
            codec,
            bit_depth,
        ),
        cursor_forward,
        multi_slice,
    );
    plan.gamescope_cursor =
        crate::session_plan::gamescope_cursor_for(compositor == pf_vdisplay::Compositor::Gamescope);
    if codec == crate::encode::Codec::PyroWave {
        plan.wire_chunk = Some(shard_payload as usize);
    }
    let mut vd = crate::vdisplay::open(compositor)?;
    vd.set_client_identity(client_identity);
    vd.set_client_hdr(client_hdr);
    vd.set_hdr(hdr);
    vd.set_hw_cursor(cursor_forward);
    vd.set_quit_flag(quit.clone());
    let _idd_setup_guard =
        (plan.capture == crate::session_plan::CaptureBackend::IddPush).then(|| {
            let slot =
                crate::vdisplay::manager::slot_id_for(client_identity, (mode.width, mode.height));
            crate::vdisplay::manager::vdm().begin_idd_setup(slot, stop.clone())
        });
    let pipeline = build_pipeline_with_retry(
        &mut vd,
        mode,
        bitrate_kbps,
        bitrate_auto,
        bit_depth,
        enc_of,
        plan,
        quit,
        stop,
        8,
        Some(trace),
    )?;
    Ok(PreparedDisplay { vd, pipeline })
}

/// Retry transient first-frame races. Permanent errors short-circuit; each failed attempt drops
/// its capturer so the next create is clean.
#[allow(clippy::too_many_arguments)]
fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    enc_of: super::EncDerive,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    max_attempts: u32,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    // IDD-push: hold one lease across attempts so a failed capturer drop does not Lingering-preempt.
    let _retry_hold = if matches!(plan.capture, crate::session_plan::CaptureBackend::IddPush) {
        Some(
            vd.create(display_mode_for(mode))
                .context("acquire virtual output for the session (retry-hold lease)")?,
        )
    } else {
        None
    };
    const FIRST_ATTEMPT_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=max_attempts {
        if attempt > 1 && stop.load(Ordering::SeqCst) {
            anyhow::bail!(
                "session ended (client disconnected) during pipeline build — aborting retries \
                 after {} attempt(s)",
                attempt - 1
            );
        }
        let first_frame_budget = (attempt == 1).then_some(FIRST_ATTEMPT_FRAME_BUDGET);
        match build_pipeline(
            vd,
            mode,
            bitrate_kbps,
            bitrate_auto,
            bit_depth,
            enc_of,
            plan,
            quit,
            None,
            first_frame_budget,
            trace,
        ) {
            Ok(pipe) => {
                if attempt > 1 {
                    tracing::info!(attempt, "pipeline up after retry");
                }
                return Ok(pipe);
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let permanent = is_permanent_build_error(&chain);
                if permanent || attempt == max_attempts {
                    let why = if permanent {
                        "permanent"
                    } else {
                        "out of retries"
                    };
                    return Err(e).with_context(|| {
                        format!("pipeline build failed ({why}) after {attempt} attempt(s)")
                    });
                }
                tracing::warn!(
                    attempt,
                    max = max_attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %chain,
                    "pipeline build failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
        }
    }
    unreachable!("the final attempt returns inside the loop")
}

/// Permanent = retrying cannot help this session. Match our English prefix, not KWin's translated payload.
fn is_permanent_build_error(chain: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "virtual displays require linux",
        "unknown punktfunk_compositor",
        "could not detect compositor",
        "kwin virtual output failed",
        "must be a node id",
        "is it installed",
        "capture/encoder negotiation mismatch",
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// Session mode with refresh × `PUNKTFUNK_VDISPLAY_HZ_MULT`. Wire rate is still [`pacing_hz`].
fn display_mode_for(session: punktfunk_core::Mode) -> punktfunk_core::Mode {
    let mult = pf_host_config::config().vdisplay_hz_mult.max(1);
    punktfunk_core::Mode {
        refresh_hz: session.refresh_hz.saturating_mul(mult).min(0xffff),
        ..session
    }
}

/// Pace and encode at min(session, achieved). Overdrive is display-only.
fn pacing_hz(session_hz: u32, achieved_hz: u32) -> u32 {
    achieved_hz.min(session_hz).max(1)
}

/// Escalated sessions flag on any net behind-frame; being escalated alone does not latch a cap.
fn encode_behind_cadence(escalated: bool, behind_score: u32, degrade_at: u32) -> bool {
    behind_score >= degrade_at || (escalated && behind_score > 0)
}

/// Observed source period, clamped to [interval, 4×]. No estimate yet keeps the interval.
fn cadence_budget(
    interval: std::time::Duration,
    src_period_ns: Option<u64>,
) -> std::time::Duration {
    match src_period_ns {
        Some(p) => std::time::Duration::from_nanos(p).clamp(interval, interval * 4),
        None => interval,
    }
}

/// Store the encoder's opened rate and tell the client. Silent when nothing changed.
fn adopt_built_bitrate(
    current: &mut u32,
    built: u32,
    live: &Arc<AtomicU32>,
    retarget: &tokio::sync::mpsc::UnboundedSender<u32>,
) {
    if built == *current {
        return;
    }
    tracing::info!(
        from_kbps = *current,
        to_kbps = built,
        "adopted the rebuilt pipeline's bitrate (re-resolved for what it actually encodes)"
    );
    *current = built;
    live.store(built, Ordering::Relaxed);
    let _ = retarget.send(built);
}

/// Announce a host-local rebuild gap so the client does not score a straddling window as congestion.
fn announce_pipeline_gap(gap: &tokio::sync::mpsc::UnboundedSender<u32>, gap_ms: u32) {
    if gap_ms == 0 {
        return;
    }
    let _ = gap.send(gap_ms);
}

/// Rebuild the encoder in place and drop owed in-flight AUs. `false` = no in-place reset.
fn reset_stalled_encoder(
    enc: &mut Box<dyn crate::encode::Encoder>,
    inflight: &mut std::collections::VecDeque<(u64, u64, std::time::Instant)>,
) -> bool {
    if !enc.reset() {
        return false;
    }
    inflight.clear();
    enc.request_keyframe();
    true
}

#[allow(clippy::too_many_arguments)]
fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    enc_of: super::EncDerive,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    supersedes: Option<u64>,
    first_frame_budget: Option<std::time::Duration>,
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    let display_mode = display_mode_for(mode);
    let vout = crate::vdisplay::registry::acquire(vd, display_mode, quit.clone(), supersedes)
        .context("create virtual output")?;
    if let Some(t) = trace {
        t.mark("display_acquired");
    }
    #[cfg(target_os = "linux")]
    let reused_gen = vout.reused_gen;
    #[cfg(target_os = "linux")]
    let pool_gen = vout.pool_gen;
    #[cfg(not(target_os = "linux"))]
    let pool_gen = None;
    let node_id = vout.node_id;
    let achieved_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(display_mode.refresh_hz);
    if achieved_hz < mode.refresh_hz {
        tracing::warn!(
            requested = display_mode.refresh_hz,
            achieved = achieved_hz,
            session = mode.refresh_hz,
            "compositor did not honor the requested refresh — encoding at the achieved rate"
        );
    } else if achieved_hz < display_mode.refresh_hz {
        tracing::info!(
            requested = display_mode.refresh_hz,
            achieved = achieved_hz,
            session = mode.refresh_hz,
            "compositor did not honor the multiplied display refresh — the session rate is unaffected"
        );
    }
    let effective_hz = pacing_hz(mode.refresh_hz, achieved_hz);
    let cursor_id0_hides = vd.name() == pf_vdisplay::Compositor::Kwin.id();
    let mut capturer = crate::capture::capture_virtual_output(
        vout,
        plan.output_format(),
        plan.capture,
        cursor_id0_hides,
    )
    .context("capture virtual output")?;
    #[cfg(target_os = "linux")]
    if plan.gamescope_cursor {
        capturer.attach_gamescope_cursor(std::sync::Arc::new(
            pf_vdisplay::gamescope_xwayland_cursor_targets,
        ));
    }
    if let Some(t) = trace {
        t.mark("capture_attached");
    }
    capturer.set_active(true);
    let first = match first_frame_budget {
        Some(budget) => capturer.next_frame_within_provisional(budget),
        None => capturer.next_frame(),
    };
    let frame = match first.context("first frame") {
        Ok(f) => f,
        Err(e) => {
            #[cfg(target_os = "linux")]
            if let Some(g) = reused_gen {
                crate::vdisplay::registry::mark_failed(g);
            }
            return Err(e);
        }
    };
    if let Some(t) = trace {
        t.mark("first_frame");
    }
    let bitrate_kbps = if bitrate_auto && (frame.width, frame.height) != (mode.width, mode.height) {
        let delivered = punktfunk_core::Mode {
            width: frame.width,
            height: frame.height,
            ..mode
        };
        let re = resolve_bitrate_kbps_for(plan.codec, 0, &delivered, plan.chroma, bit_depth);
        if re != bitrate_kbps {
            tracing::info!(
                negotiated = %format!("{}x{}", mode.width, mode.height),
                delivered = %format!("{}x{}", frame.width, frame.height),
                from_kbps = bitrate_kbps,
                to_kbps = re,
                "the source delivers a different size than the session negotiated — re-resolved the \
                 Automatic bitrate for the pixels actually being encoded"
            );
        }
        re
    } else {
        bitrate_kbps
    };
    let mut enc = crate::encode::open_video(
        plan.codec,
        frame.format,
        frame.width,
        frame.height,
        effective_hz,
        enc_of.enc_kbps(bitrate_kbps) as u64 * 1000,
        frame.is_cuda(),
        bit_depth,
        plan.chroma,
        plan.cursor_blend,
        plan.max_slices,
    )
    .context("open video encoder")?;
    if let Some(t) = trace {
        t.mark("encoder_open");
    }
    if let Some(c) = plan.wire_chunk {
        enc.set_wire_chunking(c);
    }
    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    let opened_444 = enc.caps().chroma_444;
    if opened_444 != plan.chroma.is_444() {
        tracing::warn!(
            negotiated_444 = plan.chroma.is_444(),
            opened_444,
            "encoder chroma disagrees with the negotiated Welcome — the client was told the other value"
        );
    }
    let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    Ok((
        capturer,
        enc,
        frame,
        interval,
        node_id,
        pool_gen,
        bitrate_kbps,
    ))
}

/// Has the SOURCE advanced past the first (possibly stash-delivered) frame of a recovery?
/// Sequence evidence where the capturer tracks one (`first_seq != 0`): only a NEW source image
/// advances it — a cursor regeneration or hold re-stamps `pts_ns` over unchanged pixels and must
/// not count. The pts comparison survives solely as the fallback for an untracked capturer.
fn source_advanced(
    first_seq: u64,
    first_pts: u64,
    provenance: &pf_frame::Provenance,
    pts_ns: u64,
) -> bool {
    if first_seq != 0 {
        provenance.source_seq > first_seq
    } else {
        pts_ns != first_pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eviction-recovery liveness gate must demand SOURCE progress, not a changed wall-clock
    /// PTS: a cursor regeneration (and a repeat) stamps a fresh `pts_ns` over unchanged source
    /// pixels, which is exactly how a dead presentation path used to "prove" recovery.
    #[test]
    fn recovery_needs_a_new_source_frame_not_a_new_pts() {
        use pf_frame::Provenance;
        // Tracked capturer (IDD): only an ADVANCED source sequence counts…
        assert!(source_advanced(5, 100, &Provenance::source(6, 0), 999));
        // …a regen/hold/stalled-source frame with a fresh pts does not.
        assert!(!source_advanced(5, 100, &Provenance::cursor_regen(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::hold(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::source(5, 0), 999));
        // Untracked capturer (seq 0): the historical pts comparison stands.
        assert!(source_advanced(0, 100, &Provenance::UNTRACKED, 999));
        assert!(!source_advanced(0, 100, &Provenance::UNTRACKED, 100));
    }

    /// The 2026-08-13 field log's exact reading — `period_s=2.0` — must be attributed to the
    /// client's backlog shedding, not to a host display disturbance. The whole point of routing
    /// on the shared constant is that this stays true if the cooldown is ever retuned, so the
    /// test derives its cases from `FLUSH_COOLDOWN` instead of hardcoding two seconds.
    #[test]
    fn a_recovery_cadence_on_the_clients_cooldown_is_not_blamed_on_the_display() {
        let flush = punktfunk_core::client::FLUSH_COOLDOWN;
        assert!(matches_client_flush_cadence(flush), "the field reading");
        assert!(matches_client_flush_cadence(flush + flush / 20));
        assert!(matches_client_flush_cadence(flush - flush / 20));

        assert!(!matches_client_flush_cadence(flush / 2));
        assert!(!matches_client_flush_cadence(flush * 2));
        assert!(!matches_client_flush_cadence(flush + flush / 5));
        assert!(!matches_client_flush_cadence(std::time::Duration::ZERO));
    }

    #[test]
    fn the_two_client_cooldowns_are_distinguishable_and_both_excluded_from_display_blame() {
        let flush = punktfunk_core::client::FLUSH_COOLDOWN;
        let no_video = punktfunk_core::client::NO_VIDEO_RETRY;
        assert_ne!(
            flush, no_video,
            "identical cooldowns make the host's verdict a coin flip"
        );
        // Neither may fall inside the other's ±10% band, or the period stops discriminating.
        assert!(!matches_client_flush_cadence(no_video));
        assert!(!matches_client_no_video_cadence(flush));
        // Both are client software cooldowns: never the metronomic display-disturbance branch.
        assert!(matches_client_recovery_cooldown(flush));
        assert!(matches_client_recovery_cooldown(no_video));
        // A real periodic disturbance still reaches that branch.
        assert!(!matches_client_recovery_cooldown(flush * 3));
        assert!(!matches_client_recovery_cooldown(std::time::Duration::ZERO));
    }

    #[test]
    fn an_escalated_but_caught_up_encoder_stops_refusing_climbs() {
        const DEGRADE: u32 = 10;
        assert!(!encode_behind_cadence(false, 0, DEGRADE));
        assert!(!encode_behind_cadence(false, 9, DEGRADE));
        assert!(encode_behind_cadence(false, 10, DEGRADE));
        assert!(encode_behind_cadence(true, 1, DEGRADE));
        assert!(!encode_behind_cadence(true, 0, DEGRADE));
    }

    #[test]
    fn the_behind_budget_tracks_the_source_not_the_negotiated_refresh() {
        let interval = std::time::Duration::from_micros(8333);
        let us = |d: std::time::Duration| d.as_micros() as u64;

        assert_eq!(cadence_budget(interval, None), interval);
        assert_eq!(
            us(cadence_budget(interval, Some(16_600_000))),
            16_600,
            "a 60 fps source's frames have 16.6 ms of real budget"
        );
        assert_eq!(us(cadence_budget(interval, Some(8_333_000))), 8_333);
        assert_eq!(cadence_budget(interval, Some(4_000_000)), interval);
        assert_eq!(cadence_budget(interval, Some(500_000_000)), interval * 4);
    }

    #[test]
    fn adopting_a_rebuilt_rate_tells_the_client() {
        let live = Arc::new(AtomicU32::new(20_000));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let mut current = 20_000;
        adopt_built_bitrate(&mut current, 20_000, &live, &tx);
        assert_eq!(rx.try_recv().ok(), None);
        adopt_built_bitrate(&mut current, 60_000, &live, &tx);
        assert_eq!(current, 60_000);
        assert_eq!(live.load(Ordering::Relaxed), 60_000);
        assert_eq!(rx.try_recv().ok(), Some(60_000));
    }

    #[test]
    fn pacing_never_exceeds_the_session_rate_or_the_display() {
        assert_eq!(pacing_hz(120, 120), 120);
        assert_eq!(pacing_hz(120, 60), 60);
        assert_eq!(pacing_hz(60, 120), 60);
        assert_eq!(pacing_hz(120, 240), 120);
        assert_eq!(pacing_hz(60, 90), 60);
        assert_eq!(pacing_hz(60, 0), 1);
    }

    #[test]
    fn display_mode_multiplier_scales_only_the_refresh() {
        let session = punktfunk_core::Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        };
        let display = display_mode_for(session);
        assert_eq!((display.width, display.height), (2560, 1440));
        assert_eq!(
            display.refresh_hz,
            session.refresh_hz * pf_host_config::config().vdisplay_hz_mult.max(1)
        );
    }

    #[test]
    fn reconfig_allowed_gates_gamescope_and_per_client_mode() {
        use crate::vdisplay::Compositor::{Gamescope, Hyprland, Kwin, Mutter, Wlroots};
        assert!(!reconfig_allowed(Some(Gamescope), false, false));
        assert!(!reconfig_allowed(Some(Gamescope), true, false));
        assert!(!reconfig_allowed(Some(Kwin), true, false));
        assert!(!reconfig_allowed(Some(Mutter), true, false));
        assert!(!reconfig_allowed(None, true, false));
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false, false),
                "{c:?} should allow live reconfigure"
            );
        }
        assert!(reconfig_allowed(None, false, false));
    }

    #[test]
    fn reconfig_allowed_rejects_a_monitor_mirror_on_every_backend() {
        use crate::vdisplay::Compositor::{Hyprland, Kwin, Mutter, Wlroots};
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false, false),
                "{c:?} without a pin should still allow live reconfigure"
            );
            assert!(
                !reconfig_allowed(Some(c), false, true),
                "{c:?} mirroring a physical head must reject a resize"
            );
        }
    }

    #[test]
    fn recovery_marks_land_every_period_and_rephase_at_idr() {
        let period = 4;
        let mut pos = 0u32;
        let marks: Vec<bool> = (0..10)
            .map(|_| mark_recovery_boundary(&mut pos, false, period))
            .collect();
        assert_eq!(
            marks,
            vec![false, false, false, true, false, false, false, true, false, false]
        );

        let mut pos = 0u32;
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, true, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(!mark_recovery_boundary(&mut pos, false, period));
        assert!(mark_recovery_boundary(&mut pos, false, period));
    }

    #[test]
    fn permanent_errors_short_circuit_retry() {
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Could not find output"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Não foi possível encontrar saída"
        ));
        assert!(is_permanent_build_error(
            "unknown PUNKTFUNK_COMPOSITOR 'foo' (kwin|wlroots|mutter|gamescope)"
        ));
        assert!(is_permanent_build_error(
            "spawn gamescope (is it installed? `apt install gamescope`)"
        ));
        assert!(is_permanent_build_error("virtual displays require Linux"));
        assert!(!is_permanent_build_error(
            "create virtual output: KWin created the virtual output disabled and refused to \
             stream it (stream_virtual_output failed: Não foi possível encontrar saída); enabled \
             it over output management (head Virtual-punktfunk-a1b2) — the retry picks up the \
             configuration KWin just persisted"
        ));
        assert!(!is_permanent_build_error(
            "first frame: no PipeWire frame within 10s (node 42): format negotiation never completed"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: timed out creating the KWin virtual output"
        ));
        assert!(!is_permanent_build_error("open NVENC: device busy"));
    }

    const SIM_P: i64 = 8_333_333;
    const SIM_TARGET: i64 = 2_500_000;

    struct Lcg(u64);
    impl Lcg {
        fn next_noise(&mut self, spread_ns: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if spread_ns == 0 {
                return 0;
            }
            ((self.0 >> 33) as i64 % (2 * spread_ns)) - spread_ns
        }
    }

    fn report_from_lead(
        base_lead_ns: i64,
        noise_spread_ns: i64,
        rng: &mut Lcg,
    ) -> punktfunk_core::quic::PhaseReport {
        let samples_us: Vec<u64> = (0..120)
            .map(|_| {
                let lead = (base_lead_ns + rng.next_noise(noise_spread_ns)).rem_euclid(SIM_P);
                (lead / 1000) as u64
            })
            .collect();
        let (mean_ns, coherence) =
            punktfunk_core::phase::circular_latch(&samples_us, SIM_P).expect("120 samples");
        punktfunk_core::quic::PhaseReport {
            next_latch_host_ns: 0,
            latch_period_ns: SIM_P as u32,
            uncertainty_ns: 1_000_000,
            arrival_lead_ns: mean_ns as u32,
            coherence_milli: coherence,
        }
    }

    fn grid_lead(base_lead_ns: i64, c: &PhaseController) -> i64 {
        (base_lead_ns - c.applied_readout()).rem_euclid(SIM_P)
    }

    #[test]
    fn grid_plant_tight_jitter_locks_and_stays() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(7);
        for _ in 0..12 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 500_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(7_500_000, &c) - SIM_TARGET;
        assert!(c.engaged(), "a coherent linear plant must engage");
        assert!(
            err.abs() < 1_000_000,
            "tight jitter must converge near the target lead, residual {err} ns"
        );
        let before = c.offset_ns;
        for _ in 0..10 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 500_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(
            (c.offset_ns - before).abs() <= 2 * PhaseController::MAX_STEP_NS,
            "a locked loop must not wander"
        );
    }

    #[test]
    fn grid_plant_antipode_start_converges_without_chatter() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(11);
        let base = (SIM_TARGET + SIM_P / 2).rem_euclid(SIM_P);
        for _ in 0..25 {
            let r = report_from_lead(grid_lead(base, &c), 400_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(base, &c) - SIM_TARGET;
        assert!(
            err.abs() < 1_000_000,
            "an antipode start must still converge, residual {err} ns"
        );
        assert!(
            c.cum_travel_ns <= SIM_P,
            "damped antipode stepping spent {} ns of travel — it chattered",
            c.cum_travel_ns
        );
    }

    #[test]
    fn decoupled_plant_disengages_and_holds_nothing() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(13);
        let mut engaged_at_some_point = false;
        for _ in 0..40 {
            let r = report_from_lead(7_500_000, 300_000, &mut rng);
            c.adjust(&r, SIM_P);
            engaged_at_some_point |= c.engaged();
        }
        assert!(
            engaged_at_some_point,
            "the chase must have started before the budget tripped"
        );
        assert!(
            !c.engaged(),
            "a decoupled plant must end DISENGAGED, not parked"
        );
        assert_eq!(
            c.applied_readout(),
            0,
            "disengaged means zero applied offset"
        );
    }

    #[test]
    fn incoherent_phase_never_engages() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(17);
        for _ in 0..20 {
            let r = report_from_lead(7_500_000, SIM_P, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(
            !c.engaged(),
            "an incoherent phase must never engage the grid"
        );
    }

    #[test]
    fn regime_change_reengages_after_backoff() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(19);
        for _ in 0..40 {
            let r = report_from_lead(7_500_000, 300_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(!c.engaged());
        for _ in 0..30 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 400_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(7_500_000, &c) - SIM_TARGET;
        assert!(
            c.engaged(),
            "a linearized plant after backoff must re-engage"
        );
        assert!(err.abs() < 1_000_000, "…and lock, residual {err} ns");
    }

    #[test]
    fn submit_grid_is_periodic_and_offset_shifted() {
        let mut c = PhaseController::new();
        c.epoch = Some(std::time::Instant::now() - std::time::Duration::from_millis(50));
        c.offset_ns = 1_000_000;
        let now = std::time::Instant::now();
        let t1 = c.next_submit_target(now, SIM_P).unwrap();
        let t2 = c
            .next_submit_target(t1 + std::time::Duration::from_nanos(1), SIM_P)
            .unwrap();
        let dt = t2.duration_since(t1).as_nanos() as i64;
        assert!(
            (dt - SIM_P).abs() < 1_000,
            "grid ticks must advance by exactly one period, got {dt}"
        );
        c.offset_ns = 3_000_000;
        let t1b = c.next_submit_target(now, SIM_P).unwrap();
        let shift =
            t1b.duration_since(now).as_nanos() as i64 - t1.duration_since(now).as_nanos() as i64;
        assert!(
            (shift - 2_000_000).rem_euclid(SIM_P) < 1_000
                || (shift - 2_000_000).rem_euclid(SIM_P) > SIM_P - 1_000,
            "a +2 ms offset must shift the next target by +2 ms mod P, got {shift}"
        );
    }

    const COHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI + 40;
    const INCOHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI - 40;
    const ACTIONABLE_LEAD: i64 = 7_500_000;

    fn report_at(coherence_milli: u16, lead_ns: i64) -> punktfunk_core::quic::PhaseReport {
        punktfunk_core::quic::PhaseReport {
            next_latch_host_ns: 0,
            latch_period_ns: SIM_P as u32,
            uncertainty_ns: 1_000_000,
            arrival_lead_ns: lead_ns.rem_euclid(SIM_P) as u32,
            coherence_milli,
        }
    }

    fn one_incoherent_cycle(c: &mut PhaseController) -> u32 {
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(c.engaged(), "the cycle must engage before it tears down");
        for _ in 0..3 {
            c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        let asked = c.reengage_backoff;
        while !c.fused && c.reengage_backoff > 0 {
            c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        asked
    }

    #[test]
    fn engage_requires_sustained_coherence() {
        let mut c = PhaseController::new();
        for i in 1..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
            assert!(!c.engaged(), "engaged on only {i} coherent report(s)");
        }
        c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        assert!(
            c.engaged(),
            "sustained coherence must still engage the grid"
        );

        let mut c = PhaseController::new();
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS - 1 {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS - 1 {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(
            !c.engaged(),
            "a broken streak must not count toward engaging"
        );
    }

    #[test]
    fn incoherent_disengage_backoff_escalates() {
        let mut c = PhaseController::new();
        let asked: Vec<u32> = (0..4).map(|_| one_incoherent_cycle(&mut c)).collect();
        assert_eq!(
            asked,
            vec![10, 20, 40, 80],
            "each cycle must wait longer than the last (v3 asked for zero, every time)"
        );
    }

    #[test]
    fn fuse_after_repeated_cycles() {
        let mut c = PhaseController::new();
        for _ in 0..PhaseController::INCOHERENT_FUSE {
            one_incoherent_cycle(&mut c);
        }
        assert!(
            c.fused,
            "a host that never holds a lock must park for the session"
        );
        for _ in 0..50 {
            c.adjust(&report_at(u16::MAX, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(!c.engaged(), "a fused controller must stay disengaged");
    }

    #[test]
    fn stable_lock_resets_escalation() {
        let mut c = PhaseController::new();
        one_incoherent_cycle(&mut c);
        one_incoherent_cycle(&mut c);
        assert_eq!(c.incoherent_cycles, 2, "two cycles should have escalated");

        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(c.engaged());
        c.epoch = Some(
            std::time::Instant::now()
                - PhaseController::LOCK_STABLE
                - std::time::Duration::from_secs(1),
        );
        c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        assert_eq!(
            c.incoherent_cycles, 0,
            "a lock that held past LOCK_STABLE must forgive the escalation"
        );
    }

    #[test]
    fn flap_replay_stops_the_engage_churn() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(23);
        let (mut engagements, mut reports, mut coherent_side) = (0u32, 0u32, true);
        while reports < 24 * 60 {
            let run = 2 + rng.next_noise(3).rem_euclid(3) as u32;
            for _ in 0..run {
                let was = c.engaged();
                let side = if coherent_side { COHERENT } else { INCOHERENT };
                c.adjust(&report_at(side, ACTIONABLE_LEAD), SIM_P);
                engagements += u32::from(!was && c.engaged());
                reports += 1;
            }
            coherent_side = !coherent_side;
        }
        assert!(
            engagements <= 2,
            "24 min of gate-hovering must not churn the grid: {engagements} engagements"
        );
        assert!(
            !c.fused,
            "flapping that never engaged must not blow the fuse"
        );
        for _ in 0..PhaseController::REENGAGE_BACKOFF + PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(
            c.engaged(),
            "a phase that finally holds must still get the grid"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_embedded_portal_voids_both_the_composite_and_the_starvation_signal() {
        struct Fake(Option<pf_vdisplay::PortalCursorMode>);
        impl crate::vdisplay::VirtualDisplay for Fake {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn create(
                &mut self,
                _mode: pf_vdisplay::Mode,
            ) -> anyhow::Result<crate::vdisplay::VirtualOutput> {
                anyhow::bail!("this test never creates a display")
            }
            fn last_portal_cursor_mode(&self) -> Option<pf_vdisplay::PortalCursorMode> {
                self.0
            }
        }

        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Embedded)),
            &mut composite
        ));
        assert!(!composite, "the composite can never be fed — drop it");

        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Hidden)),
            &mut composite
        ));
        assert!(!composite);

        let mut composite = true;
        assert!(settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Metadata)),
            &mut composite
        ));
        assert!(composite);

        let mut composite = true;
        assert!(settle_portal_cursor(&Fake(None), &mut composite));
        assert!(composite);
    }
}
