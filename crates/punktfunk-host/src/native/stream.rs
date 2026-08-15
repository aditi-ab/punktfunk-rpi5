//! The native `punktfunk/1` data plane (plan §W1 — carved out of [`super`]'s `serve_session`).
//! This module owns the capture→encode→send pipeline: the synthetic protocol-test source, the
//! virtual-display stream loop ([`virtual_stream`]) with its mid-stream reconfigure / adaptive-
//! bitrate / recovery machinery, the dedicated microburst-paced send thread ([`send_loop`]), the
//! speed-test probe bursts, the mid-stream session-switch watcher, and pipeline construction with
//! bounded retry. `serve_session` stands a session up and hands it a [`SessionContext`].

use super::*;

/// Advance the intra-refresh wave position and decide whether this emitted AU is a wave boundary
/// that should carry [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT).
///
/// `ir_wave_pos` counts frames since the last IDR/wave start; a real IDR re-phases it to 0 (an IDR
/// restarts the encoder's wave AND is itself a clean anchor, so it is never additionally marked).
/// Every `period`-th non-IDR AU is a boundary — the client lifts its post-loss freeze on the SECOND
/// such mark. Pure so the marking cadence is unit-tested without a GPU (see the pump's use in the
/// encode-poll loop).
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
        // Service speed-test probes between synthetic frames (loopback bandwidth tests).
        service_probes(session, stop, probe_rx, probe_result_tx, probe_seq);
        let data = test_frame(idx, 64 * 1024);
        let pts_ns = now_ns();
        session
            .submit_frame(&data, pts_ns, (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
        // Host timing (0xCF) for protocol tests: near-zero here (no capture/encode), but it
        // proves the plane end-to-end on a pure loopback run.
        if let Some(tc) = timing_conn {
            let t = punktfunk_core::quic::HostTiming {
                pts_ns,
                host_us: (now_ns().saturating_sub(pts_ns) / 1000).min(u32::MAX as u64) as u32,
                stages: None, // synthetic loop: no capture/encode stages to split
                applied_phase_ns: None,
            };
            let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t).into());
        }
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// Bounds a speed-test [`ProbeRequest`] before bursting: a 10 Gbps / 5 s ceiling keeps a probe from
/// monopolizing the link or stalling the stream for too long. The ceiling is set ABOVE the session
/// bitrate cap ([`MAX_BITRATE_KBPS`], 2 Gbps) on purpose — a probe should be able to demonstrate
/// headroom past the rate a session will actually be configured to use, so the client can pick a
/// confident 1 Gbps+ bitrate. GF(2¹⁶) FEC makes multi-Gbps reachable on a LAN.
const MAX_PROBE_KBPS: u32 = 10_000_000;
const MAX_PROBE_MS: u32 = 5_000;

/// Run a bandwidth probe over `session`: burst zero-filled access units flagged [`FLAG_PROBE`] at
/// `req.target_kbps` of goodput for `req.duration_ms` (both clamped to `MAX_PROBE_*`), pacing by a
/// "bytes allowed so far" budget so scheduling jitter doesn't overshoot the target. Returns what
/// was actually offered so the client can compute delivery ratio (`received / bytes_sent`) and
/// throughput. Video is paused for the duration (the caller's loop is blocked here) — a speed test
/// is a deliberate, short interruption the client initiates.
fn run_probe_burst(
    session: &mut Session,
    req: ProbeRequest,
    stop: &AtomicBool,
    probe_seq: bool,
) -> ProbeResult {
    let target_kbps = req.target_kbps.min(MAX_PROBE_KBPS);
    let duration_ms = req.duration_ms.min(MAX_PROBE_MS);
    // Probe filler is sealed in the PROBE index space (its own frame counter — video indexes are
    // owned by the encode loop and must stay 1:1 with the encoder's RFI bookkeeping). A client
    // that didn't advertise VIDEO_CAP_PROBE_SEQ reassembles everything in one window and would
    // drop probe-space frames as stale against the video stream — measuring garbage — so its
    // mid-session probe is DECLINED (zeroed result) instead. Old sealing (probe filler consuming
    // video indexes) is not an option anymore: those indexes are invisible to every client gap
    // detector and read as a phantom multi-thousand-frame loss after the burst.
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
    // kbps -> bytes/s (x1000/8).
    let bytes_per_sec = target_kbps as u64 * 125;
    // Keep each AU a SMALL burst (~16 KB ≈ a dozen MTU shards) and let the byte budget below pace
    // the rate finely. The old 256 KB cap blasted ~200 packets into the send buffer per submit, so
    // a small buffer (e.g. the Deck's 416 KB) overflowed on a single AU and the test measured
    // self-inflicted buffer overflow instead of the link — mirror how `paced_submit` spreads the
    // real video path's frames so the probe stresses the same way a real stream does.
    let chunk = (bytes_per_sec / 240).clamp(1200, 16 * 1024) as usize;
    let filler = vec![0u8; chunk];
    // Wire-packet accounting via session-stat deltas: `packets_sent` counts every sealed wire packet
    // (seal_frame), `packets_send_dropped` every one the send buffer rejected (WouldBlock/ENOBUFS).
    // Their delta over the burst is exact — and isolates host-side drops from link loss for the
    // client. Video is paused for the burst (the data-plane loop is blocked here), so these deltas
    // are pure probe traffic.
    let wire0 = session.stats().packets_sent;
    let drop0 = session.stats().packets_send_dropped;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(duration_ms as u64);
    let mut bytes_sent = 0u64;
    let mut packets_sent = 0u32; // probe access-unit count (goodput chunks)
    while std::time::Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        let allowed = (start.elapsed().as_secs_f64() * bytes_per_sec as f64) as u64;
        if bytes_sent < allowed {
            // A full send buffer drops on WouldBlock/ENOBUFS (UdpTransport returns Ok) — that loss is
            // part of what the probe measures (it surfaces as send_dropped), so keep going. Sealed
            // in the probe index space (FLAG_PROBE + its own counter) — never a video frame_index.
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

/// Drain any pending speed-test requests and run each burst, replying with its [`ProbeResult`].
/// Called once per data-plane loop iteration so a probe runs between frames. `probe_seq` = the
/// client advertised [`punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ`] (see [`run_probe_burst`]).
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

/// T1.1 frame-driven encode trigger (latency plan): `PUNKTFUNK_FRAME_DRIVEN=0` restores the
/// legacy fixed-cadence tick everywhere (backends without an arrival wait keep it regardless —
/// see [`pf_capture::Capturer::supports_arrival_wait`]).
fn frame_driven_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_FRAME_DRIVEN").as_deref() != Ok("0"))
}

/// Phase-locked capture (design/phase-locked-capture.md): `PUNKTFUNK_PHASE_LOCK=0` disarms the
/// controller — the rebuild-free A/B lever. Armed alone it does nothing until a client actually
/// sends [`PhaseReport`](punktfunk_core::quic::PhaseReport)s.
fn phase_lock_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_PHASE_LOCK").as_deref() != Ok("0"))
}

/// Control-task → encode-loop bridge for phase-locked capture (the multi-field sibling of the
/// `fec_target` atomic): the control task stores the client's latest
/// [`PhaseReport`](punktfunk_core::quic::PhaseReport) (latest-wins), the encode loop drains it on
/// its ~1 Hz adjust tick, and publishes the hold it is applying for the 0xCF ACK + diagnostics.
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

    /// Latest-wins store (control task).
    pub(crate) fn store(&self, r: punktfunk_core::quic::PhaseReport) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r);
    }

    /// Drain the pending report, if any (encode loop, ~1 Hz).
    fn take(&self) -> Option<punktfunk_core::quic::PhaseReport> {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn set_applied(&self, ns: i64) {
        self.applied_ns.store(ns, Ordering::Relaxed);
    }

    /// The hold currently applied (0 = idle) — the send thread's 0xCF ACK readout.
    pub(crate) fn applied_ns(&self) -> i64 {
        self.applied_ns.load(Ordering::Relaxed)
    }
}

/// The encode loop's phase controller (design/phase-locked-capture.md §3, controller v3):
/// submits lock to an ABSOLUTE grid the host owns — `epoch + k×period + offset` — and the
/// controller walks only the grid OFFSET. Plain data — a loop local, so it survives every
/// in-loop rebuild path; a new session starts disengaged.
///
/// Why a grid and not a hold (v2's on-glass lesson, 2026-07-31 midday): a per-frame ADDITIVE
/// hold on an arrival-slaved loop saturates once `hold + work ≥ interval` — submits then
/// self-pace at `hold + work` free-running against every grid, and the commanded phase shift
/// dissolves instead of arriving at the client (measured: ±2 ms hold steps, zero response in
/// the client's phase). A periodic grid cannot free-run: occupancy is exactly one frame per
/// period whatever the offset, so the phase actuation is linear by construction. Disengaged =
/// no grid sleeps at all — zero added latency, the pre-phase-lock loop.
///
/// Inherited v1/v2 lessons: the median was a dead statistic (v2 moved to circular+coherence);
/// a parked actuation is an e2e tax (failure response = DISENGAGE, never park); the travel
/// budget catches any residual chase the statistics miss. New in v3: ANTIPODE DAMPING — an
/// error within 1 ms of ±period/2 flips sign on sampling noise (measured as 0↔2↔4 ms offset
/// chatter), so near-antipode steps are halved until the error commits to a side.
///
/// New in v4 (design/host-source-stutter-fixes.md WP-A1, from the 2026-08-15 Skynet log): a host
/// whose coherence oscillates around [`COHERENCE_FLOOR_MILLI`](Self::COHERENCE_FLOOR_MILLI) used
/// to flap forever, because engagement needed ONE coherent report and the incoherent disengage
/// asked for NO backoff — 41 engage/disengage cycles in 24 minutes on a KWin host. Each cycle is
/// a self-inflicted timing step in both directions (engage starts holding submits by up to a
/// period; disengage drops the offset to 0 and the next frames leave that much earlier), so the
/// controller was manufacturing the very arrival jitter it exists to remove. Hence: engagement
/// needs SUSTAINED coherence, each incoherent cycle backs off longer than the last, and a host
/// that cannot hold coherence at all parks for the session.
struct PhaseController {
    /// Grid offset, ns ∈ [0, period). Meaningful only while engaged.
    offset_ns: i64,
    /// The grid's epoch; `None` = disengaged (no submit-grid sleeps, zero cost). Doubles as the
    /// lock's age — it is stamped at engage and cleared at disengage, nowhere else.
    epoch: Option<std::time::Instant>,
    /// Last adjust instant (~1 Hz cadence).
    last_adjust: std::time::Instant,
    /// |step| integrated since engage/lock — the chase detector.
    cum_travel_ns: i64,
    /// Consecutive incoherent reports; 3 disengage.
    incoherent_streak: u32,
    /// Consecutive COHERENT reports — the v4 engage gate.
    coherent_streak: u32,
    /// Incoherent disengages this session: escalates the backoff, and blows the fuse. Forgiven
    /// by a lock that holds for [`LOCK_STABLE`](Self::LOCK_STABLE).
    incoherent_cycles: u32,
    /// Fuse blown — parked for the session, no further engagement and no further log lines.
    fused: bool,
    /// Adjust ticks to sit out after a disengage before re-engaging.
    reengage_backoff: u32,
}

impl PhaseController {
    /// Per-adjustment walk bound: 2 ms per second of reports keeps the wire cadence visually
    /// undisturbed while converging a worst-case half-period error in ~2-3 s.
    const MAX_STEP_NS: i64 = 2_000_000;
    /// Ignore errors under this — a locked loop does nothing.
    const DEADBAND_NS: i64 = 300_000;
    /// The lead floor the controller drives toward: SurfaceFlinger-class compositors need the
    /// frame in the queue ~2.5 ms before latch; the client's own `uncertainty_ns` widens this.
    const TARGET_LEAD_FLOOR_NS: i64 = 2_500_000;
    /// Below this circular coherence (‰) the arrival phase is smeared over the period and
    /// alignment is physically pointless. `u16::MAX` (a v1 report) bypasses the gate and
    /// relies on the travel budget alone.
    const COHERENCE_FLOOR_MILLI: u16 = 300;
    /// Errors within this of ±period/2 sit at the antipode discontinuity, where sampling noise
    /// flips the sign — damp the step until the error commits to a side.
    const ANTIPODE_GUARD_NS: i64 = 1_000_000;
    /// Adjust ticks sat out after a disengage (travel exhaustion) before trying again.
    const REENGAGE_BACKOFF: u32 = 10;
    /// Consecutive coherent reports the grid must see before it engages (v4). One was enough in
    /// v3, which is why a host hovering at the coherence floor re-engaged within a second of
    /// every disengage; at the ~1 Hz report cadence this asks for ~5 s of a phase worth locking.
    const ENGAGE_COHERENT_REPORTS: u32 = 5;
    /// Incoherent cycles before the grid parks for the session. A host that has failed this many
    /// times is telling us its arrival phase is not lockable, and permanently disengaged (today's
    /// default, zero added latency) is strictly better than another cycle of steps.
    const INCOHERENT_FUSE: u32 = 8;
    /// Escalation ceiling — `REENGAGE_BACKOFF << 5` = 320 ticks ≈ 5 min.
    const MAX_BACKOFF_SHIFT: u32 = 5;
    /// A lock held this long forgives the session's escalation: a transient bad patch (a shader
    /// storm, a game launch) must not fuse a host that is otherwise perfectly lockable.
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

    /// `coherence_milli` is the report that caused the disengage: without it the log showed how
    /// far the offset had walked but not how far below the gate the phase actually was, which is
    /// the number that says whether the host is marginal or hopeless.
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
        // Both streaks restart: re-engaging costs a fresh ENGAGE_COHERENT_REPORTS of proof, and
        // the next disengage costs a fresh three incoherent reports.
        self.incoherent_streak = 0;
        self.coherent_streak = 0;
        self.reengage_backoff = backoff;
    }

    /// Fold the client's latest report into the grid offset. `period_ns` is the wire interval.
    /// Sign convention: a positive (shortest-way) error means frames arrive too early and wait
    /// at the client — submit LATER (grow the offset); negative — earlier.
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
                // Only a disengage that tore down an ENGAGED grid cost the stream a timing step.
                // Counting the others would let a launch-time shader storm — minutes of genuinely
                // incoherent arrival before the controller ever locks — blow the fuse on a host
                // that then locks perfectly for the next three hours.
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
                // Each cycle waits longer than the last: 10 ticks (~10 s) doubling to 320 (~5 min).
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
        // A lock this old has proven itself — forgive the escalation so a session that hits one
        // bad patch an hour is not slowly fused by it. Checked here rather than at disengage so
        // the state decays while the lock is good, not only when it is lost.
        if self.epoch.is_some_and(|e| e.elapsed() >= Self::LOCK_STABLE) {
            self.incoherent_cycles = 0;
        }
        let target = Self::TARGET_LEAD_FLOOR_NS.max(r.uncertainty_ns as i64 + 1_000_000);
        // Signed SHORTEST-WAY error around the period.
        let raw = (r.arrival_lead_ns as i64 - target).rem_euclid(period_ns);
        let error = if raw > period_ns / 2 {
            raw - period_ns
        } else {
            raw
        };
        if error.abs() < Self::DEADBAND_NS {
            self.cum_travel_ns = 0; // locked — the budget re-arms for the next disturbance
            return;
        }
        if !self.engaged() {
            // Hysteresis: an offset worth holding submits for has to be backed by a phase that
            // stayed coherent, not by the one report that happened to clear the gate.
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
        // Antipode damping: this error sits where its sign is a coin flip — half steps until
        // it commits to a side.
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

    /// The next submit-grid instant at or after `now` — the loop sleeps until it before
    /// submitting a fresh frame (newest-wins keeps the content fresh across the wait).
    /// `None` while disengaged: no sleep, no cost.
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
        // Guard: never schedule more than one period out (clock skew paranoia).
        let target = epoch + std::time::Duration::from_nanos(target_ns.max(0) as u64);
        if target.duration_since(now).as_nanos() as i64 > period_ns {
            return Some(now);
        }
        Some(target)
    }

    /// ACK readout: the engaged grid offset (0 while disengaged).
    fn applied_readout(&self) -> i64 {
        if self.engaged() {
            self.offset_ns
        } else {
            0
        }
    }

    /// Whether the ~1 Hz adjust window has elapsed.
    fn due(&self) -> bool {
        self.last_adjust.elapsed() >= std::time::Duration::from_secs(1)
    }
}

/// Adaptive pipeline depth (latency plan, from the 2026-07-17 on-glass finding on a `.173` RTX
/// 4090): the capturer's pipeline depth of 2 measured **~13 ms of glass-to-glass latency** over
/// depth 1 at 60 fps (17 ms → 4 ms) — the AU is ready in µs but depth-2 holds it a whole frame
/// interval unpolled while N+1 is submitted. Depth-2 exists to overlap the convert of N+1 with
/// the encode of N under GPU contention (the depth-1 ~50 fps collapse), so run **depth-1 by
/// default** and escalate to the capturer's max ONLY when the loop can't hold its cadence at
/// depth-1 (the contention tell), then stick there for the session (escalate-and-hold — no
/// oscillation; de-escalation is a v2 item). `PUNKTFUNK_IDD_ADAPTIVE=0` pins the capturer's full
/// depth (the pre-adaptive behaviour). Off when the capturer's max depth is already 1.
fn idd_adaptive_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_IDD_ADAPTIVE").as_deref() != Ok("0"))
}

/// Seal one access unit and send it with MICROBURST pacing (the shared
/// [`send_pacing`](crate::send_pacing) policy, native parameterization): the first `burst_cap`
/// bytes go out immediately (one absorbed burst the NIC / socket tx-buffer can swallow), and
/// only the OVERFLOW beyond that is spread across the time it needs at `pace_rate_bps` in
/// ADAPTIVE chunks — 16 packets at today's rates, coarsening to at most 64 (the GSO-segment
/// cap) once the rate would otherwise skip every sub-floor sleep, so ≥1 Gbps frames still pace
/// instead of collapsing into an unpaced blast (plan Phase 1.2). `burst_cap` `None` = auto:
/// `max(128 KB, this AU's wire bytes / 4)`, so the burst stays a bounded fraction of a
/// high-rate frame instead of swallowing it whole (plan Phase 1.3); `Some` =
/// PUNKTFUNK_PACE_BURST_KB pinned an absolute cap. So a normal-bitrate frame (≤ cap) leaves in
/// one immediate burst at ~0 added latency, while a genuine IDR / sustained-high-bitrate frame
/// (≫ cap) still spreads — keeping the freeze fix exactly where it's needed (an unpaced
/// line-rate burst overruns the kernel tx buffer → EAGAIN drop → under infinite GOP, a freeze
/// until the next keyframe).
///
/// `pace_rate_bps` (latency plan T1.2; resume-safe form, stall program T2): the caller passes
/// ~3× the live encoder bitrate — a rate the link is proven to carry sustained — and the
/// overflow's wire time at that rate IS the pace budget ([`crate::send_pacing::native_budget`],
/// [`crate::send_pacing::MAX_PACE_SPREAD`]-bounded). The frame deadline no longer under-cuts
/// the spread: for a steady-state frame the rate term was the smaller one anyway (tail gone in
/// a fraction of the interval), and for an oversized frame (stall-resume scene delta, cold
/// IDR) the old deadline clamp was exactly the line-rate blast → tx-overrun → freeze path this
/// module exists to prevent. `0` = uncapped legacy deadline-only spread
/// (PUNKTFUNK_PACE_FACTOR=0, and the fallback when the bitrate isn't known yet).
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
) -> Result<PaceStat> {
    let wires = session
        .seal_frame_at(data, pts_ns, flags, frame_index)
        .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    pace_sealed(session, wires, deadline, burst_cap, pace_rate_bps)
}

/// The pace-and-send half of [`paced_submit`], for wires that are ALREADY sealed — shared with
/// the streamed-AU path, whose seal happens per encoder chunk ([`handle_chunk`]) under the same
/// microburst policy and frame deadline.
fn pace_sealed(
    session: &mut Session,
    wires: Vec<Vec<u8>>,
    deadline: std::time::Instant,
    burst_cap: Option<usize>,
    pace_rate_bps: u64,
) -> Result<PaceStat> {
    let mut refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    // FEC/recovery test knob (PUNKTFUNK_VIDEO_DROP) — same knob the GameStream plane honors.
    crate::send_pacing::inject_video_drop(&mut refs);
    let wire_bytes: usize = refs.iter().map(|p| p.len()).sum();
    let burst_bytes = burst_cap.unwrap_or_else(|| (wire_bytes / 4).max(128 * 1024));
    let cfg = crate::send_pacing::PaceCfg {
        burst_bytes: Some(burst_bytes),
        chunk: crate::send_pacing::ChunkPolicy::Adaptive { base: 16, max: 64 },
        sleep_floor: std::time::Duration::from_micros(500),
    };
    // T1.2 rate cap, resume-safe form (stall program T2): the overflow's wire time at
    // `pace_rate_bps` IS the budget — the deadline no longer under-cuts it, so an oversized
    // frame (a stall-resume scene delta, a cold IDR) paces at the proven 3× rate instead of
    // collapsing into a line-rate blast that overruns the socket buffer and loses the very
    // frame that ends a freeze. See `send_pacing::native_budget` for the full argument.
    let overflow_bytes = wire_bytes.saturating_sub(burst_bytes) as u64;
    let budget = crate::send_pacing::native_budget(deadline, pace_rate_bps, overflow_bytes);
    // Time the socket handoff per chunk and fold it into the session's SealPerf split — the
    // sleeps between chunks stay excluded, so sock_ns is pure send_gso/sendmmsg time.
    let mut sock_ns = 0u64;
    let result = crate::send_pacing::pace_frame(&refs, budget, &cfg, |chunk| {
        let t0 = std::time::Instant::now();
        let r = session.send_sealed(chunk).map(|_| ());
        sock_ns += t0.elapsed().as_nanos() as u64;
        r
    });
    drop(refs); // release the borrow of `wires` so it can return to the seal pool
    session.reclaim_wires(wires);
    session.note_sock_ns(sock_ns);
    result.map_err(|e| anyhow!("send_sealed: {e:?}"))
}

/// One encoded frame handed from the capture/encode thread to the send thread (the encode|send
/// split). The send thread does FEC+seal+paced-send while this thread captures+encodes the next.
struct FrameMsg {
    data: Vec<u8>,
    capture_ns: u64,
    flags: u32,
    /// The wire `frame_index` this AU is sealed with. Assigned by the encode loop's
    /// session-lifetime counter (`au_seq`) — the loop owns the video numbering so the index it
    /// PREDICTED at submit time (`au_seq + inflight`, handed to `Encoder::submit_indexed`) is
    /// exactly what the packetizer stamps, keeping the encoder's RFI bookkeeping 1:1 with the
    /// wire across encoder rebuilds/resets. Sealed via `Session::seal_frame_at`.
    frame_index: u32,
    /// When this frame's packets should have fully left (the next frame's due time) = the pacing
    /// budget. In the past when the send thread is behind → immediate send (catch up).
    deadline: std::time::Instant,
    /// submit→encoded latency (µs), measured on the encode thread, carried for the perf histogram.
    encode_us: u32,
    /// Capture-delivery → encoder-submit age (µs) of a fresh frame — the PipeWire delivery +
    /// channel-queue time the old pre-submit stamp made invisible. Always measured (two integer
    /// ops); 0 for repeats/tail frames. The wire pts (`capture_ns`) anchors at the same delivery
    /// stamp, so client-side latency figures include this window too.
    queue_us: u32,
    /// Per-stage µs splits, measured on the capture/encode thread (0 when neither `PUNKTFUNK_PERF`
    /// nor a stats capture is armed). The send thread accumulates them for the web-console sample:
    /// `cap_us` = `try_latest` (ring read + colour convert), `submit_us` = NVENC `encode_picture`
    /// launch, `wait_us` = `lock_bitstream` (the scheduling wait + ASIC encode = the "encode" stage).
    /// SYNCHRONOUS backends (PyroWave: the whole GPU encode + fence wait runs inside `submit`)
    /// carry their real encode time in `submit_us`, and the "encode" stage reads ~0 by
    /// construction — read the pair together (the 2026-07 field triage read "encode 0.00" as an
    /// instrumentation hole; it's the stage split's shape). The client-facing 0xCF `encode_us`
    /// is unaffected: its anchor is stamped before `submit`, so it spans both.
    cap_us: u32,
    submit_us: u32,
    wait_us: u32,
    /// This frame is a re-encoded hold (the source had no fresh frame): a source-starvation signal
    /// the send thread folds into `repeat_fps`.
    repeat: bool,
    /// Whether the per-stage splits (`cap_us`/`submit_us`/`wait_us`) were actually measured at
    /// capture time (`perf` was on or a stats capture was armed). The send thread trusts this
    /// instead of re-reading `is_armed()`, so a capture that arms while frames are already in flight
    /// doesn't fold their zeroed splits into the first window's percentiles.
    was_measured: bool,
}

/// What the encode thread hands the send thread: a whole AU (the legacy path — every session
/// shape except a chunked encoder toward a streamed-capable client), or one slice-boundary
/// chunk of a streamed AU (§7 LN1 Phase 2 — the send thread seals/paces each chunk's completed
/// FEC blocks while the encoder still produces the AU's tail).
enum SendMsg {
    Frame(FrameMsg),
    Chunk(ChunkMsg),
}

/// One encoder chunk of a streamed AU. AU-level fields (`capture_ns`/`flags`/`frame_index`/
/// `deadline`) are identical on every chunk of one AU (the send thread opens the streamed seal
/// at `first`); the perf split fields are meaningful on `last` (whole-AU figures, exactly like
/// [`FrameMsg`]'s).
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

/// A streamed AU the send thread has open: the core's incremental sealer plus the pace
/// aggregation across its per-chunk flushes (the accounting the whole-AU path reads off one
/// [`paced_submit`] call).
struct StreamedOpen {
    au: punktfunk_core::packet::StreamedAu,
    spread_us: u32,
    paced: bool,
}

/// Feed one [`ChunkMsg`] through the streamed sealer: open at `first`, seal + pace every FEC
/// block the chunk completes, close (+ final block, real totals) at `last`. Returns
/// `Some((accounting, aggregated PaceStat))` when the AU finished — the caller runs the same
/// per-AU accounting as the whole-frame path — and `None` mid-AU.
fn handle_chunk(
    session: &mut Session,
    open: &mut Option<StreamedOpen>,
    c: ChunkMsg,
    slice_wire: bool,
    burst_cap: Option<usize>,
    pace_rate_bps: u64,
) -> Result<Option<(FrameMsg, PaceStat)>> {
    if c.first {
        if open.take().is_some() {
            // The encode loop abandoned a mid-flight AU (an encoder stall/rebuild forfeits the
            // in-flight frame). Its sentinel packets are already on the wire — the client ages
            // that frame out and the rebuild's IDR re-anchors; just don't leak the open state.
            tracing::warn!(
                "streamed AU abandoned mid-flight (encoder rebuild) — client ages it out"
            );
        }
        // The AU's own flag bit is what switches the wire to slice-granularity blocks —
        // set only toward a client that negotiated BOTH streamed AUs and multi-slice (the
        // flag's contract in `punktfunk_core::packet`); without it the sealer stays on the
        // legacy full-FEC-block shape shipped receivers require.
        let flags = c.flags
            | if slice_wire {
                punktfunk_core::packet::USER_FLAG_SLICE_STREAM
            } else {
                0
            };
        *open = Some(StreamedOpen {
            au: session
                .begin_streamed_frame_at(c.capture_ns, flags, c.frame_index)
                .map_err(|e| anyhow!("begin_streamed_frame: {e:?}"))?,
            spread_us: 0,
            paced: false,
        });
    }
    let Some(s) = open.as_mut() else {
        return Err(anyhow!(
            "streamed chunk without an open AU (encode-loop bug)"
        ));
    };
    // Every ChunkMsg IS an encoder slice boundary (the chunked poll returns per-slice
    // readbacks), so `slice_end` is unconditionally true — the AU's flag gates whether the
    // sealer may actually cut a block there.
    let wires = session
        .seal_streamed_chunk(&mut s.au, &c.data, true)
        .map_err(|e| anyhow!("seal_streamed_chunk: {e:?}"))?;
    if !wires.is_empty() {
        let stat = pace_sealed(session, wires, c.deadline, burst_cap, pace_rate_bps)?;
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
    let stat = pace_sealed(session, tail, c.deadline, burst_cap, pace_rate_bps)?;
    Ok(Some((
        FrameMsg {
            data: Vec::new(), // already on the wire — accounting only
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

/// The dedicated send thread: it owns the whole [`Session`] (so no socket clone or shared stats are
/// needed) and does FEC+seal + microburst-paced send OFF the capture/encode thread, plus the
/// speed-test probe bursts (which also need the Session). Decoupling the paced send from encoding
/// lets the encode of frame N+1 overlap the transmit of frame N instead of waiting behind its tail.
/// Runs until the encode thread drops the frame channel (end of stream) or `stop` is set.
/// Everything the send thread needs to emit web-console stats samples at its 2 s aggregation
/// boundary: the shared recorder (whose `is_armed()` gates emission) plus the negotiated
/// mode/codec/client to seed the capture's `CaptureMeta` on the first armed registration.
struct SendStats {
    rec: Arc<StatsRecorder>,
    /// Live session mode, packed w:16|h:16|hz:16 ([`pack_mode`]) — the capture thread updates it
    /// on an accepted mid-stream mode switch (mirroring `bitrate_kbps` below), so a stats capture
    /// registers the mode the stream is ACTUALLY running at, not the session-start latch (H3).
    mode: Arc<AtomicU64>,
    codec: &'static str,
    client: String,
    /// Live encoder bitrate (kbps) — the capture thread updates it on a mid-stream adaptive
    /// bitrate change, so the web-console sample reports what the encoder is ACTUALLY targeting.
    bitrate_kbps: Arc<AtomicU32>,
    /// The session's bring-up trace (P0.1): the send thread FINISHES it — `first_packet` — the
    /// moment the first video AU's packets have fully left the socket (finish is once-only, so
    /// the per-frame call is a cheap no-op afterwards).
    bringup: Arc<crate::bringup::Trace>,
}

/// Whether a session on `compositor` (`None` = the synthetic source) with a `per_client_mode`
/// identity policy may LIVE-reconfigure — accept a mid-stream `Reconfigure`
/// (design/midstream-resolution-resize.md H1/H5). Gated OFF for:
///   * **gamescope** (every sub-mode): a resize would respawn the nested game / restart the box's
///     game-mode session — it must never relaunch the title, so the client keeps scaling client-side.
///   * a **per-client-mode identity** policy: the mode is part of the display-identity slot key, so a
///     resize resolves a DIFFERENT slot (a fresh Windows monitor / a differently-named KWin output),
///     defeating the policy — honest downgrade is to reject and let the client scale.
///   * a **monitor mirror** (`mirrored`): the source is a physical head running at the mode its owner
///     set, and `MirrorDisplay::create` ignores the requested one by design
///     (design/per-monitor-portal-capture.md §7.3). A resize would tear the cast down and re-`create`
///     the *same* head at the *same* size — a visible hitch that changes nothing — or, worse, invite
///     the reflex of reconfiguring the display someone is sitting in front of. Reject; the client
///     scales, exactly as it already does for gamescope.
///
/// Every other compositor (and the synthetic protocol-test source) with the default identity accepts.
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
    // Smoothed whole-AU paced-send time (µs) published for the ENCODE loop, which hands it to
    // `Encoder::set_send_spread_us`. The split arbiter needs it to price what engaging split
    // costs on HEVC (sub-frame readback, and with it the send/encode overlap) — a number the
    // encoder cannot observe. Written here because this is the only thread that sees a send.
    send_spread_us: Arc<AtomicU32>,
    // Streamed AUs go out as slice-granularity blocks ([`USER_FLAG_SLICE_STREAM`]'s contract)
    // instead of the legacy full-FEC-block shape.
    slice_wire: bool,
    burst_cap: Option<usize>,
    fec_target: Arc<AtomicU8>,
    // Mid-session shard-payload re-keys from the wire-MTU watcher (validated + ack-gated
    // there) — applied between AUs only (design/shard-payload-reneg.md Phase 1).
    shard_rx: std::sync::mpsc::Receiver<usize>,
    stats: SendStats,
    // `Some` = the client advertised VIDEO_CAP_HOST_TIMING: emit one 0xCF datagram per AU right
    // after its last packet left the socket (capture→sent, the whole host pipeline incl. pacing).
    timing_conn: Option<quinn::Connection>,
    // Phase-lock ACK source: the hold the encode loop currently applies rides the 0xCF tail.
    phase: Arc<PhaseCtl>,
    // The client advertised VIDEO_CAP_PROBE_SEQ — mid-session speed-test bursts may run in the
    // probe index space (else they're declined; see `run_probe_burst`).
    probe_seq: bool,
) {
    boost_thread_priority(false); // transmit thread: above-normal (Apollo's encoder-thread level)
                                  // T1.2 front-loaded pacing: the paced overflow drains at `factor ×` the live encoder
                                  // bitrate instead of stretching to the frame deadline. 3× default (the link carries 1×
                                  // sustained, so a bounded 3× excursion is safe — WebRTC's pacer uses 2.5×);
                                  // `PUNKTFUNK_PACE_FACTOR=0` restores the legacy deadline-only spread.
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
    // Web-console stats accumulation (active when `perf` OR the recorder is armed): the per-stage
    // split carried on each FrameMsg, the new-vs-repeat frame split, the cached registration id, and
    // the previous window's loss snapshot for delta computation.
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
    // The streamed AU currently open (VIDEO_CAP_STREAMED_AU chunked sends) — `Some` strictly
    // between a `ChunkMsg::first` and its `last`.
    let mut streamed: Option<StreamedOpen> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Probes run here (they need the Session); a burst pauses video — the encode thread blocks
        // on the full frame channel meanwhile, which is exactly the intended pause. Never mid-AU:
        // a streamed frame's chunks are already leaving the socket, so a burst spliced between
        // them would push the AU's tail past its deadline (the exact latency the mode removes).
        if streamed.is_none() {
            service_probes(&mut session, &stop, &probe_rx, &probe_result_tx, probe_seq);
        }
        // Adaptive FEC: pick up any new recovery target the control task set from client LossReports.
        apply_fec_target(&mut session, &fec_target);
        // Mid-session shard renegotiation: apply a re-key from the wire-MTU watcher — between
        // AUs only, NEVER with a streamed AU open (its shard-aligned tiling derives from the
        // size it began with; same gate as the probe burst above). Drain to the newest; the
        // protocol side (client advertisement, ack-gated grow) was enforced by the watcher.
        if streamed.is_none() {
            let mut want_shard = None;
            while let Ok(s) = shard_rx.try_recv() {
                want_shard = Some(s);
            }
            if let Some(s) = want_shard {
                match session.set_shard_payload(s) {
                    Ok(()) => tracing::info!(shard_payload = s, "wire shard payload re-keyed"),
                    // Can't fire for a watcher-driven value (it validates the same bounds) —
                    // belt-and-suspenders for a future driver.
                    Err(e) => tracing::warn!(shard_payload = s, error = ?e,
                        "shard re-key refused by session validation"),
                }
            }
        }
        // Short timeout so we keep re-checking `stop` + probes when no frames are flowing.
        match frame_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(send_msg) => {
                // Live ABR-tracked encoder bitrate → pace rate; 0 (not yet known) = uncapped.
                let pace_rate = (stats.bitrate_kbps.load(Ordering::Relaxed) as f64
                    * 1000.0
                    * pace_factor) as u64;
                // `Ok(Some(..))` = an AU fully left the socket (a whole frame, or a streamed
                // AU's last chunk) — run the per-AU accounting; `Ok(None)` = mid-AU chunk.
                let outcome = match send_msg {
                    SendMsg::Frame(msg) => paced_submit(
                        &mut session,
                        &msg.data,
                        msg.capture_ns,
                        msg.flags,
                        msg.frame_index,
                        msg.deadline,
                        burst_cap,
                        pace_rate,
                    )
                    .map(|stat| Some((msg, stat))),
                    SendMsg::Chunk(c) => handle_chunk(
                        &mut session,
                        &mut streamed,
                        c,
                        slice_wire,
                        burst_cap,
                        pace_rate,
                    ),
                };
                match outcome {
                    // Mid-AU chunk: sealed + on the wire; the per-AU accounting runs at `last`.
                    Ok(None) => {}
                    Ok(Some((msg, stat))) => {
                        // First VIDEO packets are on the wire — complete the bring-up trace (P0.1;
                        // once-only, no-op on every later frame). Speed-test filler isn't video.
                        if msg.flags & FLAG_PROBE as u32 == 0 {
                            stats.bringup.finish("first_packet");
                        }
                        // Host timing (0xCF): stamped now — the AU's packets have fully left the
                        // socket — against the same capture anchor the wire pts carries, so the
                        // client's per-frame math tiles exactly (network = its host+network − this).
                        // Best-effort like every side-plane datagram; skipped for speed-test filler
                        // (FLAG_PROBE isn't video and its pts is the burst clock).
                        if let Some(tc) = &timing_conn {
                            if msg.flags & FLAG_PROBE as u32 == 0 {
                                let host_us = (now_ns().saturating_sub(msg.capture_ns) / 1000)
                                    .min(u32::MAX as u64)
                                    as u32;
                                let t = punktfunk_core::quic::HostTiming {
                                    pts_ns: msg.capture_ns,
                                    host_us,
                                    // T0.1 stage split: queue + encode ride the FrameMsg (always
                                    // measured), pace is this send's spread. The client derives
                                    // seal/FEC + channel-wait as the residual against host_us.
                                    stages: Some(punktfunk_core::quic::HostStages {
                                        queue_us: msg.queue_us,
                                        encode_us: msg.encode_us,
                                        pace_us: stat.spread_us,
                                    }),
                                    // Phase-lock ACK: the hold the capture tick is applying
                                    // right now (0 = controller idle/unarmed) — the client's
                                    // closed-loop readout.
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
                        // Smooth before publishing: a single AU's spread swings with content and
                        // FEC shape, and the arbiter turns this into a latency handicap that
                        // decides an arm. EWMA (3:1) over completed AUs is enough to stop one
                        // spike flipping a verdict.
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
                            // `encode_us`/`pace_us`/fps are valid for every frame (always measured),
                            // including the Windows relay + tail-drain frames. The cap/submit/wait splits
                            // are only real when the frame was measured at capture time — a frame captured
                            // before this capture armed carries zeroed splits, so skip those (an empty
                            // window → `percentile()` returns 0) rather than pull the percentiles down.
                            encode_us.push(msg.encode_us);
                            pace_us.push(stat.spread_us);
                            if msg.was_measured {
                                cap_v.push(msg.cap_us);
                                submit_v.push(msg.submit_us);
                                wait_v.push(msg.wait_us);
                                // Queue age is only meaningful for fresh frames (repeats/tail carry 0
                                // by construction — including those would drag the percentiles down).
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
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break, // encode thread done
        }
        if last_perf.elapsed() >= std::time::Duration::from_secs(2) {
            let s = session.stats();
            let secs = last_perf.elapsed().as_secs_f64();
            // Attempted (sealed) transmit rate; `send_dropped` is what didn't reach the wire.
            let tx_mbps = (s.bytes_sent - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
            if perf {
                // Send-thread stage split (Phase 0.4 host half): busy-time sums over this
                // window, so share-of-core = <stage>_ms / window wall ms. The per-packet ns
                // figures are the Phase 1.5 gate metric — seal parallelism is warranted only
                // if seal_ns_pp × pkts/s approaches ~15% of a core at 2 Gbps.
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
            // Web-console capture: this thread owns `session.stats()`, so it emits the COMPLETE
            // sample — the cap/submit/encode split carried over from the capture thread plus this
            // window's pacing/goodput/loss. Loss fields are deltas vs the previous window's snapshot.
            if stats.rec.is_armed() {
                let session_id = *sid.get_or_insert_with(|| {
                    // Read the LIVE mode at registration time (H3): a capture armed after a
                    // mid-stream mode switch gets the mode the stream actually runs at.
                    let (w, h, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                    stats
                        .rec
                        .register_session("native", w, h, hz, stats.codec, &stats.client)
                });
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0, // stamped by push_sample from the capture's monotonic start
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

/// A mid-stream session change the watcher detected (the box flipped Gaming↔Desktop): the new
/// backend + the [`crate::vdisplay::SessionEnv`] snapshot to retarget at it. The env is applied on
/// the encode thread (not the watcher), so the watcher never does a process-global env write.
struct SessionSwitch {
    kind: crate::vdisplay::ActiveKind,
    compositor: crate::vdisplay::Compositor,
    env: crate::vdisplay::SessionEnv,
}

/// Poll the live graphical session ~1 s and, when its kind changes from what the stream opened with
/// (the user switched Gaming↔Desktop mid-stream) and stays changed for a debounce, send one
/// [`SessionSwitch`] so the encode loop rebuilds the backend in place. Self-baselines on the first
/// read (so no handshake plumbing). Opt-in via `PUNKTFUNK_SESSION_WATCH`; readiness of the new
/// backend is left to the encode thread's `build_pipeline_with_retry` (the watcher never writes
/// env). Exits when `stop` is set or the channel closes.
/// Whether to run the mid-stream session-switch watcher. An explicit `PUNKTFUNK_SESSION_WATCH` wins
/// (truthy → on; `0`/`false`/`no`/`off`/empty → off). When unset it defaults **on** for Steam HTPC
/// platforms (Bazzite / SteamOS) — which flip Gaming↔Desktop and need the host to follow the switch
/// mid-stream — and **off** elsewhere, preserving the opt-in default for plain desktop hosts.
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

/// True on Bazzite or SteamOS (matched against os-release `ID`/`ID_LIKE`) — the platforms that flip
/// between Steam Gaming Mode and a Desktop session, where following a mid-stream switch is the
/// sensible default. Anything else (incl. non-Linux, where the file is absent) → false.
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
    // Baseline = what the stream is currently driving (matches the handshake's resolution).
    let mut current = vdisplay::detect_active_session().kind;
    let mut pending: Option<(vdisplay::ActiveKind, std::time::Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let active = vdisplay::detect_active_session();
        // A4: bump the session epoch + invalidate the old backend the moment the compositor instance
        // changes (kind change OR same-kind restart) — even for a same-kind restart the watcher won't
        // signal a full SessionSwitch for. Self-dedupes; the debounced SessionSwitch below still drives
        // the in-place rebuild.
        vdisplay::observe_session_instance(&active);
        let cur = active.kind;
        if cur == current {
            pending = None; // back to the current backend before debounce elapsed — no switch
            continue;
        }
        match pending {
            // Stable at the new kind for the debounce window — the switch is real, signal it.
            Some((k, since)) if k == cur && since.elapsed() >= DEBOUNCE => {
                // Before anything about capture: a managed takeover runtime-masks the box's own
                // autologin gaming unit, and a switch to a desktop session ends the window where
                // that mask is sound. Left on, it silently bars the way back — the user's "Return
                // to Gaming Mode" cannot start a masked unit, so Steam sits on its "Switch to
                // Desktop…" modal until a reboot clears it. Ahead of the `compositor_for_kind`
                // arm below on purpose: a switch we cannot follow still has to unbar the return.
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
                            break; // encode loop gone
                        }
                        current = cur; // new baseline; don't re-signal until it changes again
                    }
                    // Logout / no usable backend for the new session — keep streaming the old one.
                    None => tracing::debug!(to = ?cur,
                        "session watcher: no usable backend for the new session — staying put"),
                }
                pending = None;
            }
            // Still debouncing this kind.
            Some((k, _)) if k == cur => {}
            // A new (or different) change — start the debounce window.
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}

/// All per-session inputs for [`virtual_stream`], bundled so the session entry
/// is one moved value instead of a 13-positional-argument `#[allow(too_many_arguments)]` signature
/// (Goal-1 stage 4, plan §2.4). Everything is **owned** — the receivers move in (`virtual_stream` is their
/// only consumer) — so the whole context moves into the stream thread and the borrow plumbing disappears.
pub(super) struct SessionContext {
    /// The hardened data-plane `Session` (Leopard FEC + AES-GCM over UDP); moved into the send thread.
    pub(super) session: Session,
    /// The client's requested mode — the virtual output is created at exactly this WxH@Hz (no scaling).
    pub(super) mode: punktfunk_core::Mode,
    /// Stream duration cap (the persistent listener bounds back-to-back sessions).
    pub(super) seconds: u32,
    /// Session stop flag (set on disconnect / reconnect-preempt).
    pub(super) stop: Arc<AtomicBool>,
    /// Deliberate-quit flag (set when the client closed with `QUIT_CODE`): the display lease reads it
    /// on teardown to skip the keep-alive linger for a user "stop" (vs. an unwanted disconnect).
    pub(super) quit: Arc<AtomicBool>,
    /// Accepted mid-stream mode switches — the pipeline is rebuilt at the new mode.
    pub(super) reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    /// Client decode-recovery keyframe requests.
    pub(super) keyframe: std::sync::mpsc::Receiver<()>,
    /// Client LTR-RFI recovery requests — the lost-frame range `(first, last)`. The encode loop
    /// prefers `Encoder::invalidate_ref_frames` over a full IDR when the encoder supports it.
    pub(super) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    /// Accepted mid-stream bitrate changes (adaptive bitrate, already clamped) — the encoder
    /// alone is rebuilt in place at the new rate; capture + virtual output are untouched.
    pub(super) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    /// Mid-session shard-payload changes from the wire-MTU watcher (already validated +
    /// protocol-gated there; a grow arrives only after the client's ack). Applied between
    /// AUs via [`Session::set_shard_payload`] — the packetizer re-keys, capture/encoder/
    /// virtual output are untouched (design/shard-payload-reneg.md Phase 1).
    pub(super) shard_rx: std::sync::mpsc::Receiver<usize>,
    /// The resolved compositor backend (moot on Windows — `vdisplay::open` ignores it there).
    pub(super) compositor: crate::vdisplay::Compositor,
    /// This session's resolved gamescope sub-mode, or `None` for every other backend. Carried here
    /// (and on to the backend instance) rather than through `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION`:
    /// two sessions connecting at once used to overwrite each other's decision in the process env.
    pub(super) gamescope_route: Option<crate::vdisplay::GamescopeRoute>,
    /// Negotiated encoder bitrate (kbps).
    pub(super) bitrate_kbps: u32,
    /// The encoder's live APPLIED rate (kbps) — shared with the send pacer, the web console, the
    /// mgmt registry AND the control task (which acks climbs against it). The encode loop stores
    /// `Encoder::applied_bitrate_bps` here after every apply, so everything downstream tracks
    /// what the ASIC really targets, not what was requested (§ABR overdrive).
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// The encoder's discovered codec-level bitrate ceiling (kbps; 0 = none discovered): written
    /// when an apply comes back short, read by this loop (pre-clamp incoming requests — a
    /// request already AT the ceiling then costs nothing) and by the control task (truthful
    /// acks from the first post-discovery request).
    pub(super) encoder_ceiling_kbps: Arc<AtomicU32>,
    /// "Encode can't hold the frame cadence" (the escalation leaky bucket is elevated, or the
    /// session escalated): while set, the control task refuses bitrate CLIMBS — the network
    /// isn't the bottleneck, feeding the encoder more bits deepens the miss.
    pub(super) cadence_degraded: Arc<AtomicBool>,
    /// The client asked for "Automatic" (`Hello::bitrate_kbps == 0`), so `bitrate_kbps` came from
    /// the host's codec-aware default. For PyroWave that default is the ~1.6 bpp operating point of
    /// the NEGOTIATED MODE (`resolve_bitrate_kbps_for`) — a mid-stream mode switch re-resolves it
    /// for the new mode (the pin follows the resolution; an explicit client rate stays put).
    pub(super) bitrate_auto: bool,
    /// Negotiated encode bit depth (8, or 10 = HEVC Main10).
    pub(super) bit_depth: u8,
    /// Negotiated chroma subsampling (4:2:0, or 4:4:4 when the client + host + GPU all support it).
    pub(super) chroma: crate::encode::ChromaFormat,
    /// Negotiated video codec the encoder emits (HEVC by default; H.264 / AV1 when the client
    /// prefers one the GPU encodes; H.264 for a software host). Also used to rebuild the encoder
    /// at the same codec across a mid-stream mode reconfigure.
    pub(super) codec: crate::encode::Codec,
    /// Speed-test burst requests (see [`service_probes`]).
    pub(super) probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    /// Speed-test results back to the control task.
    pub(super) probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    /// Mode-switch outcomes back to the control task (H2): a corrective
    /// `Reconfigured { accepted: true, mode: <actually live> }` when a rebuild failed (stayed at
    /// the old mode) or the backend honored a different refresh than requested.
    pub(super) reconfig_result_tx: tokio::sync::mpsc::UnboundedSender<Reconfigured>,
    /// Host-initiated bitrate re-target → control task → the client's `BitrateChanged`. Fired
    /// by [`adopt_built_bitrate`] when a rebuild lands on a rate the client wasn't told about.
    pub(super) retarget_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    /// Adaptive-FEC target the control task updates from the client's loss reports.
    pub(super) fec_target: Arc<AtomicU8>,
    /// The QUIC control connection (carries host→client 0xCE source-HDR metadata mid-stream).
    pub(super) conn: quinn::Connection,
    /// `Some` when the client advertised [`punktfunk_core::quic::VIDEO_CAP_HOST_TIMING`]: the send
    /// thread emits one 0xCF datagram per AU (capture→sent µs) on it, so the client can split its
    /// `host+network` latency stage. `None` = older client, no emission.
    pub(super) timing_conn: Option<quinn::Connection>,
    /// Phase-locked capture bridge (design/phase-locked-capture.md): the control task stores
    /// client [`PhaseReport`]s here; the encode loop's controller drains them.
    pub(super) phase: Arc<PhaseCtl>,
    /// The session negotiated the cursor channel (design/remote-desktop-sweep.md M2 —
    /// `handshake::cursor_forward`): the encode loop forwards shape (via `cursor_shape_tx`)
    /// + per-tick `0xD0` state while the client draws the pointer locally.
    pub(super) cursor_forward: bool,
    /// LIVE render split for cap sessions (client `CursorRenderMode`, §8 mid-stream flip):
    /// `true` = client draws (exclude from video + forward), `false` = host composites (the
    /// capture mouse model — DWM on Windows, encoder blend on Linux). Control task writes;
    /// the encode loop edge-detects per tick. Always `true` for non-cap sessions (inert).
    pub(super) cursor_client_draws: Arc<AtomicBool>,
    /// SHAPE bridge to the control task (the control stream's sole writer) — mirrors
    /// `probe_result_tx`. Inert when `cursor_forward` is false.
    pub(super) cursor_shape_tx:
        tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::CursorShape>,
    /// The client advertised [`punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ`]: speed-test bursts may
    /// run mid-session in the probe index space (its reassembler keeps a separate probe window).
    /// `false` = older client whose single-window reassembler would drop probe-space frames as
    /// stale — mid-session probes are DECLINED for it (a zeroed [`ProbeResult`]) rather than
    /// consuming video frame indexes its gap detectors can't see (the phantom-gap freeze).
    pub(super) probe_seq: bool,
    /// The client advertised [`punktfunk_core::quic::VIDEO_CAP_STREAMED_AU`]: when the session's
    /// encoder runs chunked poll (multi-slice sub-frame readback, §7 LN1), the host streams each
    /// AU's FEC blocks under sentinel headers as the slices complete instead of waiting for the
    /// whole AU. `false` = older client — chunks (if any) are drained whole-AU, zero wire change.
    pub(super) streamed_au: bool,
    /// The client advertised [`punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE`]: its decoder
    /// accepts multi-slice AUs, so the session's encoder may keep its multi-slice default
    /// (§7 LN1 — becomes [`SessionPlan::max_slices`](crate::session_plan::SessionPlan)).
    /// `false` = single-slice frames, the pre-0.17 wire shape TV-SoC decoders (Amlogic —
    /// Chromecast with Google TV) require to not wedge.
    pub(super) multi_slice: bool,
    /// Shared streaming-stats recorder. The capture loop reads `is_armed()` per frame to decide
    /// whether to measure the per-stage split; the send thread builds + pushes the aggregated
    /// `StatsSample` at its 2 s boundary.
    pub(super) stats: Arc<StatsRecorder>,
    /// Short client label (cert-fingerprint prefix, else peer IP) seeded into the capture meta on
    /// the first armed stats registration.
    pub(super) client_label: String,
    /// The client's display name (trust-store name, else sanitized Hello name; `None` = nameless
    /// knock) — published to the live-session registry for the local summary's connect toast.
    pub(super) client_name: Option<String>,
    /// The session's requested launch, `None` = none. On Windows the store-qualified library id
    /// (spawned into the interactive user session once capture is live); on other hosts the shell
    /// command already resolved against the host's own library — nested into gamescope's bare spawn
    /// via `set_launch_command`, or spawned into the live session once capture is up.
    pub(super) launch: Option<String>,
    /// Identity + detection metadata for the launched title, resolved once at handshake time
    /// alongside `launch`. `None` when nothing was launched. Drives the game's lifetime — its exit
    /// can end this session, and this session ending can end it (design/session-game-lifetime.md).
    pub(super) launch_target: Option<crate::library::LaunchTarget>,
    /// The client display's HDR colour volume (`Hello::display_hdr`; `None` = older client / SDR).
    /// Threaded into the vdisplay backend before `create` (→ the pf-vdisplay EDID's CTA HDR block,
    /// so host apps tone-map to the client's real panel) and preferred over the generic baseline
    /// for the 0xCE mastering metadata.
    pub(super) client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    /// The session's bring-up trace (latency plan P0.1): the pipeline-build stages stamp into it
    /// and the send thread finishes it when the first video packet leaves.
    pub(super) bringup: Arc<crate::bringup::Trace>,
    /// Shared slot the latest completed mid-stream resize total (ms) lands in — registered with
    /// `session_status` so the Dashboard shows it.
    pub(super) resize_ms: Arc<AtomicU32>,
    /// The session's input pipeline (the same channel client datagrams feed) — the stream loop
    /// uses it to PARK the seat pointer on the streamed surface (see [`park_pointer`]).
    #[cfg(target_os = "linux")]
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::input::ClientInput>,
}

/// Park the seat pointer at the centre of the streamed surface, through the SAME injection path
/// client input takes (capability routing, region ladder, anchor — everything).
///
/// Why this exists (the GNOME capture-mode cursor bug, 2026-07): a Linux virtual output is
/// created fresh per session, and the seat pointer stays wherever it last was — usually on a
/// physical monitor. A capture-model (pointer-lock) client sends only RELATIVE deltas, so
/// nothing ever moves the pointer INTO the streamed output: its input lands on the wrong
/// monitor, and on compositors that only embed/report the cursor while it is over the recorded
/// view (Mutter suppresses `SPA_META_Cursor` entirely — `should_cursor_metadata_be_set` — and
/// its embedded mode paints nothing either) the stream has NO cursor at all, in both the
/// embedded and the cursor-channel composite models. Parking once per (re)built display — and
/// again on the mid-stream flip to the capture model, which heals a pointer that drifted off the
/// output's edge — pins the pointer to the surface the client actually sees.
///
/// **Retried only for a relative-only client.** The schedule used to repeat this for every session,
/// on the theory that "a desktop-model client overrides it with its first absolute move, so the
/// jump is invisible in practice". One park at bring-up is; a *repeat* is not. A desktop-model
/// client sends absolute positions — the very same [`MouseMoveAbs`] this synthesizes, only aimed
/// where the user is actually pointing — so once those are flowing the park has nothing left to add
/// and every later attempt is a visible yank to centre that fights them (field report 2026-08-14).
/// The schedule below therefore keeps the single bring-up park for such a client (so its first
/// click cannot land on the monitor the seat pointer was left on) and drops the retry; the
/// capture-model flip re-arms the full schedule.
///
/// [`MouseMoveAbs`]: punktfunk_core::input::InputKind::MouseMoveAbs
#[cfg(target_os = "linux")]
fn park_pointer(input_tx: &std::sync::mpsc::SyncSender<super::input::ClientInput>, w: u32, h: u32) {
    let ev = punktfunk_core::input::InputEvent {
        kind: punktfunk_core::input::InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x: (w / 2) as i32,
        y: (h / 2) as i32,
        // MouseMoveAbs packs its reference extent into `flags` — the injector's region ladder
        // matches the streamed output by exactly these dims.
        flags: (w << 16) | (h & 0xffff),
    };
    // `try_send`, matching the bounded input queue (2026-08-05 review M-3): parking is a
    // best-effort nicety and must never block the stream loop behind a full input backlog.
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

/// Settle this session's cursor plan against the cursor mode the display's portal ACTUALLY
/// negotiated — THE rule, shared by bring-up and the capture-loss rebuild so the two cannot drift.
///
/// Returns whether "the capture has no cursor overlay" still MEANS anything on this display, and
/// clears `metadata_composite` when the negotiated mode makes it a fiction. Both answers come from
/// the same fact, which is why they are settled together.
///
/// The fact: [`set_hw_cursor`] is a *request*. On the whole wlr family (xdph, xdpw) the portal
/// advertises `Hidden|Embedded` — measured, `AvailableCursorModes = 3`, on current packages — so a
/// session that asked for metadata is served **Embedded**: the compositor paints the pointer into
/// the frames and sends no `SPA_META_Cursor`, ever, wherever the pointer is. That breaks two
/// downstream beliefs at once:
///
/// * the host planned a metadata composite it can never feed (the stream logs "host-composite
///   active but the capture has no live cursor overlay yet" forever, and shows no host pointer —
///   the compositor's own burnt-in one is the real cursor there), and
/// * the seat-pointer park schedule reads "no overlay" as "the pointer has not reached the streamed
///   output". That inference is sound on **Mutter**, which suppresses cursor metadata while the
///   pointer is off the recorded view (`should_cursor_metadata_be_set`) — it is the signal
///   [`park_pointer`] was built on — and it is pure noise under Embedded, where the host then
///   re-centres the user's pointer once a second for the whole cap, fighting every mouse movement
///   (field report 2026-08-14, Hyprland).
///
/// A backend that reports no negotiated mode (`None` — KWin, Mutter, gamescope, Windows) is served
/// the cursor mode it asked for through its own protocol, so nothing here applies and both answers
/// stay exactly as they were.
///
/// What this does NOT undo is [`SessionPlan::cursor_blend`]: the blend capability is resolved
/// before any display exists, and the negotiation only happens inside `create`, so a session that
/// lands on Embedded still captures RGB for a blend stage it will never be handed an overlay for.
/// That costs a colour conversion, not correctness — and pre-judging it would mean re-asserting
/// what the wlr portals advertise, which is exactly the hardcode `pf_vdisplay::portal_cursor`
/// exists to have deleted.
///
/// [`set_hw_cursor`]: pf_vdisplay::VirtualDisplay::set_hw_cursor
/// [`SessionPlan::cursor_blend`]: crate::session_plan::SessionPlan::cursor_blend
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

pub(super) fn virtual_stream(ctx: SessionContext, prepared: Option<PreparedDisplay>) -> Result<()> {
    // This thread runs the capture+encode loop (single-process — the only topology: Linux portal /
    // synthetic, Windows in-process IDD-push). Elevate it so a CPU-heavy game can't deschedule our GPU
    // submission.
    boost_thread_priority(true);
    // Resolve the per-session capture / topology / encoder decision ONCE (Goal-1 stage 3): the deployed
    // path now reads this typed `SessionPlan` instead of re-deriving from config at each dispatch site
    // (the latent "capture and encode disagree on the backend" hazard, plan §2.4). `bit_depth` is the
    // only per-session input — capture/topology/encoder are otherwise pure functions of `HostConfig`.
    let mut plan = crate::session_plan::SessionPlan::resolve(
        ctx.bit_depth,
        ctx.chroma,
        ctx.codec,
        // Blend CAPABILITY (the single rule in `cursor_blend_for`): cursor-FORWARD sessions
        // need it for the mid-stream capture-mouse flip (`CursorRenderMode` — WHETHER a
        // frame's pointer is drawn stays per-tick, the encode loop strips `frame.cursor`
        // while the client draws locally); gamescope (Phase C) can't embed a pointer, so the
        // host always composites the XFixes-sourced cursor; and a NO-channel session gets
        // metadata + host blend too wherever the backend composites — the compositor-EMBEDS
        // fallback streams cursorless on a Mutter virtual output (the overlay-visibility
        // gate is stage-global since Mutter 48; see `cursor_blend_for`'s doc). Embedded
        // remains only the can't-blend fallback.
        crate::session_plan::cursor_blend_for(
            ctx.cursor_forward,
            ctx.compositor == pf_vdisplay::Compositor::Gamescope,
            ctx.codec,
            ctx.bit_depth,
        ),
        ctx.cursor_forward,
        ctx.multi_slice,
    );
    // gamescope: the XFixes cursor source feeds the host-side composite (Phase C) — unless the
    // spawned gamescope paints the pointer into its node itself, in which case the reader would
    // produce a second one. Set after resolve so the flag stays a pure function of the compositor
    // (+ that capability).
    plan.gamescope_cursor = crate::session_plan::gamescope_cursor_for(
        ctx.compositor == pf_vdisplay::Compositor::Gamescope,
    );
    // PyroWave rides the datagram-aligned wire mode (§4.4): every encoder this session opens
    // packetizes at the negotiated shard payload, so a lost datagram costs blocks, not frames.
    if ctx.codec == crate::encode::Codec::PyroWave {
        plan.wire_chunk = Some(ctx.session.shard_payload());
    }
    tracing::info!(?plan, "resolved session plan");
    // Single-process path: unpack the context into the locals the loop below uses (names unchanged, so the
    // body is byte-for-byte the same; the receivers are now owned but `try_recv()` is identical).
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
        live_bitrate,
        encoder_ceiling_kbps,
        cadence_degraded,
        bitrate_auto,
        bit_depth,
        // The resolved chroma is already captured in `plan` (above); ignore the duplicate here.
        chroma: _,
        // Likewise the codec — `plan.codec` (resolved from `ctx.codec`) is the source of truth below.
        codec: _,
        probe_rx,
        probe_result_tx,
        reconfig_result_tx,
        retarget_tx,
        fec_target,
        conn,
        timing_conn,
        phase,
        cursor_forward,
        cursor_shape_tx,
        cursor_client_draws,
        probe_seq,
        streamed_au,
        // Folded into `plan.max_slices` by the resolve above; ALSO gates the slice-granularity
        // streamed wire below.
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
    } = ctx;
    // Only the Linux paths (`launch_is_nested`, `set_gamescope_route`) read it; gamescope does not
    // exist on Windows, where every one of those call sites is cfg'd out.
    #[cfg(target_os = "windows")]
    let _ = &gamescope_route;
    // Reference point for adopting the launched game's processes: anything the host will call "this
    // session's game" has to have started after this instant. Taken HERE, before the display (and
    // therefore before a bare-spawn gamescope's nested child) exists, because a reading taken after
    // the launch would reject the very process it is meant to find. Erring early is the safe
    // direction: it can only ever include more of our own launch, never a copy from before it.
    let fresh_stamp = crate::gamelease::launch_clock();
    // ...unless this host ALREADY launched this title for this client and that launch is still ours.
    // A client that re-dials (the presenter's HEVC→H.264 codec fallback, a crash-restart, a network
    // blip) re-sends `Hello::launch` verbatim — it cannot drop the field, because the per-session
    // gamescope is re-adopted through a display-registry key that includes the launch command. So the
    // host is the one that has to notice, on both counts: not starting a second copy of the game, and
    // adopting against the ORIGINAL launch's instant rather than this session's — a fresh reading is
    // minutes after the game started, and `procscan` would refuse to adopt it, leaving this session
    // with no game-exit detection at all. The record only ever holds launches this host performed for
    // this client, so `procscan`'s "never adopt a process that predates the launch" survives intact.
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
    // Streamed-AU wire mode: the client's cap AND the host escape hatch (`PUNKTFUNK_STREAMED_AU=0`
    // reverts to whole-AU sends without touching the encoder's slicing knobs). The third gate —
    // whether the ENCODER actually chunks — is dynamic (`supports_chunked_poll`, per AU).
    let streamed_wire = streamed_au && std::env::var("PUNKTFUNK_STREAMED_AU").as_deref() != Ok("0");
    // Slice-granularity streamed blocks (P2): needs the streamed wire AND the client's
    // multi-slice tolerance (the slices only exist when the encoder splits the frame, which
    // `plan.max_slices` already keyed off the same cap). `PUNKTFUNK_SLICE_STREAM=0` pins the
    // legacy block granularity for A/B without touching slicing or the streamed wire.
    let slice_wire = streamed_wire
        && multi_slice
        && std::env::var("PUNKTFUNK_SLICE_STREAM").as_deref() != Ok("0");
    // Cursor-forward state (M2): shape-serial diffing + the per-tick 0xD0 state send. The
    // encoder was told not to blend (SessionPlan above), so from the first frame the client's
    // locally-drawn cursor is the only one.
    let mut cursor_fwd = cursor_forward.then(super::cursor_fwd::CursorForwarder::new);
    // Edge detector for the live render flip (`cursor_client_draws`) — starts true (the
    // channel's initial state), so the first composite request triggers the capturer hook.
    let mut cursor_client_drew = true;
    if cursor_forward {
        tracing::info!("cursor channel negotiated — forwarding shape/state, encoder blend off");
    }
    // gamescope (Phase C): no channel for a plain capture-mode client and no compositor-embedded
    // pointer, so the host ALWAYS composites the XFixes-sourced cursor into the video. Active only
    // when there's no cursor-forward channel (a future desktop-mode gamescope client takes the
    // `cursor_fwd` path instead). See `plan.gamescope_cursor`.
    // `mut`: a mid-stream Gaming↔Desktop switch (the capture-loss rebuild below) retargets the
    // compositor, so this is recomputed there against the live compositor.
    let mut gamescope_composite =
        compositor == pf_vdisplay::Compositor::Gamescope && cursor_fwd.is_none();
    if gamescope_composite {
        tracing::info!("gamescope cursor: compositing the XFixes-sourced pointer into the video");
    }
    // No-channel metadata composite: the client never draws the pointer (it did not advertise
    // the cursor channel — e.g. a capture-latched client, `console.rs` `latched_mouse`), and
    // the compositor-EMBEDS fallback is a fiction on a Mutter virtual stream (the software
    // cursor overlay is suppressed stage-globally whenever any physical head realizes a HW
    // cursor, Mutter 48+ — dmabuf frames blit the view WITHOUT it, and cursor-only motion
    // schedules no update either, mutter#4939). So the plan asked the backend for
    // cursor-as-metadata and the HOST composites, permanently — the same arm a channel
    // session lands in after its capture-model flip, minus the channel.
    // `mut`: recomputed with `gamescope_composite` on a mid-stream compositor retarget.
    let mut metadata_composite = cursor_fwd.is_none()
        && plan.cursor_blend
        && compositor != pf_vdisplay::Compositor::Gamescope;
    if metadata_composite {
        tracing::info!(
            "no cursor channel — compositing the metadata cursor into the video (embedded \
             fallback is unreliable on virtual streams)"
        );
    }
    if streamed_wire {
        // Client capability only — whether AUs actually stream per-slice depends on the encoder
        // backend's `supports_chunked_poll()` (today: Linux direct-NVENC only), which doesn't
        // exist yet at this point. The old wording ("chunked encoder output will stream
        // per-slice") sent a 2026-07 field triage chasing a streaming path AMF doesn't have.
        tracing::info!(
            "client accepts streamed AUs (VIDEO_CAP_STREAMED_AU) — used if this session's \
             encoder supports chunked output"
        );
    }
    // A mode switch the control task accepted BEFORE the pipeline was built (the client connects at
    // one mode and immediately asks for its real one — a fractional-scale panel resolving its native
    // pixel size does exactly this, ~3 s ahead of bring-up finishing) used to be served the long way
    // round: build the whole pipeline at the now-stale mode, then immediately rebuild at the new one
    // in the loop below. That wastes a display create + capture attach + encoder open on every such
    // connect, and on GNOME it is actively destructive — the rebuild is create-before-drop, so two
    // `RecordVirtual` monitors ~400 ms apart segfault mutter 50.4 inside
    // `meta_monitor_manager_rebuild`, taking down the whole desktop session (and with it the game
    // just launched into it, which then looks like the GAME crashed). Adopt the newest queued mode
    // here and build ONCE.
    //
    // Only on the inline path: a PREPARED pipeline is already built at the old mode, so adopting a
    // new `mode` there would just make this variable disagree with the display that exists. Those
    // sessions keep the rebuild-in-the-loop behavior. No accept ack is owed either way — the
    // client's mode slot already flipped when control accepted the switch (it acks on accept, not
    // on rebuild); the H2/H3 *correction* ack the rebuild would have sent is preserved below.
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
            // Mirror the loop's rebuild: PyroWave's Automatic bitrate is a per-mode ~1.6 bpp pin, so
            // a resolution change moves the operating point. Explicit client rates stay put.
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
    // The vdisplay backend + built pipeline: either PREPARED at Welcome time on this very thread
    // (P1.1/P1.2 — the display bring-up already overlapped the Start RTT + hole-punch), or built
    // inline now (Linux, synthetic-adjacent paths, prep fallback).
    let (mut vd, pipe) = match prepared {
        Some(p) => (p.vd, p.pipeline),
        None => {
            // Open the backend FIRST — on Windows this constructs the vdisplay backend, which
            // initialises the host-lifetime VirtualDisplayManager (§2.5). It does NO monitor work,
            // so it must precede the IDD-push preempt below (which reaches the manager) —
            // otherwise `vdm()` is called before init and panics.
            let mut vd = crate::vdisplay::open(compositor)?;
            // Per-client STABLE monitor identity (Phase 2): hand the backend the connecting
            // client's cert fingerprint so a freshly CREATED virtual monitor gets this client's
            // persistent id — Windows then reapplies the client's saved per-monitor config (DPI
            // scaling) on reconnect. No-op on Linux backends and for anonymous/GameStream clients
            // (no fingerprint → the driver auto-allocates).
            vd.set_client_identity(endpoint::peer_fingerprint(&conn));
            // The client display's HDR volume (Hello) → a freshly created virtual monitor's EDID
            // CTA HDR block (pf-vdisplay), so host apps + the OS tone-map to the client's real
            // panel instead of the driver's built-in ~1000-nit placeholder. No-op on Linux
            // backends and for older/SDR clients.
            vd.set_client_hdr(client_hdr);
            // THIS SESSION's colourimetry (distinct from the client panel's volume above): a
            // 10-bit session needs the output brought up HDR, which on gamescope means spawning
            // it with the HDR flags so nested games get HDR surfaces at all. Decided in the
            // Welcome (`capture::capturer_supports_hdr_for`), so it cannot change under us.
            vd.set_hdr(bit_depth >= 10);
            // Out-of-band cursor request: cursor-forward sessions (Windows pf-vdisplay /
            // IddCx hardware cursor; Linux metadata mode) AND no-channel host-composite
            // sessions (Linux only — `metadata_composite` is `plan.cursor_blend`-gated, so
            // it is always false on Windows). The backend keeps the pointer out of the
            // pixels; the host blend (or the client) puts it back.
            vd.set_hw_cursor(cursor_forward || metadata_composite);
            // Deliberate-quit wiring (Windows pf-vdisplay; no-op elsewhere): every lease the
            // backend mints — the retry-hold below AND the capturer's — carries the session's quit
            // flag, so a user "stop" (⌘D → the QUIT close code) tears the virtual monitor down the
            // moment the pipeline drops instead of lingering 10 s. The reconnect then finds the
            // manager Idle and does a clean fresh ADD (with the user's think-time as driver
            // settle) rather than the Lingering-preempt's REMOVE→ADD churn. `keep_alive = forever`
            // (gaming-rig) outranks the quit — the monitor pins as before.
            vd.set_quit_flag(quit.clone());
            // Per-session launch (non-Windows): hand the resolved command to the backend instance
            // so gamescope's bare spawn nests it — per-instance, no process-global env, so
            // concurrent sessions can't stomp each other's launch target. The other backends'
            // default `set_launch_command` is a no-op; they get the command spawned into the live
            // session after capture is up (below).
            #[cfg(not(target_os = "windows"))]
            vd.set_launch_command(launch.clone());
            // Same per-instance discipline for the gamescope sub-mode this session resolved at
            // handshake time: it used to arrive through PUNKTFUNK_GAMESCOPE_NODE/_SESSION, which a
            // second connect (or the switch watcher below) could overwrite before this `create`.
            #[cfg(not(target_os = "windows"))]
            vd.set_gamescope_route(gamescope_route.clone());
            // IDD-push reconnect preempt (the dance now lives in the manager, Goal-1 §2.5):
            // serialize setup so a reconnect FLOOD can't run concurrent monitor create/teardown,
            // STOP the prior session + WAIT for it to release its monitor (instead of tearing a
            // monitor out from under a still-live session), and register THIS session's stop. The
            // returned guard holds the setup lock across the pipeline build; dropping it (end of
            // this arm) lets the next reconnect begin (and preempt us). Held BEFORE the monitor is
            // created (build_pipeline → vd.create), so the preempt still precedes this session's
            // monitor creation. SLOT-scoped (Stage W1): the preempt targets only a prior session
            // holding THIS client's slot — a different identity's session is an admission
            // question, never a preempt.
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
                plan,
                &quit,
                &stop,
                8,
                Some(bringup.as_ref()),
            )?;
            // Setup done — the IDD-push setup lock releases as the guard leaves this arm's scope,
            // so the next reconnect can begin (and preempt us).
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
    // The display exists now, so the portal has answered: settle the cursor plan against what it
    // actually negotiated rather than what this session asked for (see `settle_portal_cursor`).
    // `mut`: every capture-loss rebuild re-runs `create`, hence re-negotiates.
    #[cfg(target_os = "linux")]
    let mut no_overlay_means_off_output = settle_portal_cursor(&*vd, &mut metadata_composite);
    // The encoder may have opened at a re-resolved rate (a mirrored head delivering a size this
    // session never negotiated). Adopt it before anything downstream reads `bitrate_kbps`.
    adopt_built_bitrate(
        &mut bitrate_kbps,
        built_bitrate,
        &live_bitrate,
        &retarget_tx,
    );
    // H2/H3 correction, carried over from the rebuild this bring-up replaced: the client APPLIED
    // the mode when control accepted it, but the backend may have honored a different one (KWin
    // caps a virtual output's refresh; a fallback delivers the size the source actually produces).
    // Only for a mode adopted at bring-up — an ordinary connect's mode came from the Welcome, not
    // from an accept the client has already acted on, so it is not owed a correction here.
    if adopted_at_bringup {
        let actual = delivered_mode(frame.width, frame.height, interval);
        if actual != mode {
            let _ = reconfig_result_tx.send(Reconfigured {
                accepted: true,
                mode: actual,
            });
        }
    }

    // Capture is live — launch the requested title so it renders onto the streamed output and
    // grabs focus. Windows spawns the library id into the interactive user session; Linux spawns
    // the resolved command into the live session for every backend that didn't already nest it
    // (gamescope's bare spawn ran it inside the fresh gamescope — launching again would start it
    // twice). Best-effort: a launch failure (no recipe, launcher missing, no interactive user)
    // leaves the user on the streamed desktop/session, never tears the stream down. Launched ONCE
    // here — the mid-stream rebuild paths below must not re-spawn it.
    //
    // ...and ONCE PER LAUNCH, not once per session: `adopt_launch` is the registry's verdict that this
    // client's own copy of this title is already running (see `fresh_stamp` above), in which case the
    // spawn is skipped entirely and the lease below adopts what is already there.
    let adopt_launch = launch_claim.as_ref().is_some_and(|c| !c.must_spawn());
    // Whether this session actually started the title. False when the spawn was skipped (adopted),
    // when it failed, and on a platform with no launch path at all — the record is settled from it
    // below, so a launch that did not happen can never be inherited by a later session.
    #[allow(unused_mut)]
    let mut spawned_now = false;
    #[cfg(target_os = "windows")]
    if let Some(id) = launch.as_deref() {
        if adopt_launch {
            tracing::info!(
                launch_id = id,
                "this client's copy of this title is already running from an earlier session — not \
                 starting a second one"
            );
        } else if let Err(e) = crate::library::launch_title(id) {
            tracing::warn!(launch_id = id, error = %e, "could not launch requested library title");
        } else {
            spawned_now = true;
        }
    }
    #[cfg(target_os = "linux")]
    let spawned_launch = match launch.as_deref() {
        // Already ours and still running. On the nested path this is also what the display registry
        // concludes on its own — its reuse key includes the launch command, so the kept gamescope
        // (with the game inside it) is re-attached rather than respawned.
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
            // gamescope spawns it as its own nested child, so the launch DID happen here.
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
    // Settle the record against what actually happened. A spawn that never ran — it failed, or this
    // platform has no launch path — must leave nothing behind, or a retry would inherit a launch that
    // never occurred and then decline to start the title at all.
    if let Some(c) = launch_claim.as_ref() {
        if spawned_now {
            c.launched();
        } else if c.must_spawn() {
            c.abandon();
        }
    }

    // The launched game's lifetime, in both directions (design/session-game-lifetime.md):
    //
    // * **its exit ends this session** — so a client returns to its library instead of sitting on a
    //   hidden launcher or a bare desktop. This generalizes what used to be a Steam-and-gamescope-only
    //   watch: the game is now recognized from whatever its store told us (appid, install dir, exe,
    //   env marker), which covers every compositor and every store. The node-death check in the
    //   capture-loss branch below stays as the backstop for a nested launch we can't otherwise see.
    // * **this session ending can end it** — never by default; only when the operator asked, and for
    //   a mere disconnect only after a reconnect window (`_game_life`'s drop, below).
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
                // Read the setting at fire time, so flipping it mid-session takes effect. The lease
                // itself keeps running either way — the status surface still reports the game.
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
                // Close FIRST so APP_EXITED is the winning close code (quinn keeps the first
                // application close), then set the flags: `quit` skips the display lease's
                // keep-alive linger and `stop` wakes the encode/send loops out.
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
                launch_stamp,
                // For an adopted launch this is the ORIGINAL launch's slot, so the record keeps
                // tracking the same processes across the handover.
                procs: launch_claim.as_ref().and_then(|c| c.procs()),
            },
            on_exit,
        )
    });
    let game_shared = game_lease.as_ref().map(|l| l.shared());
    // Declared here so it drops *after* the live-session registration below (reverse declaration
    // order): `session.ended` fires first, then the game policy runs — the order an operator reading
    // the log expects. The fingerprint is what lets a reconnecting client reclaim its own game and
    // nothing else.
    let _game_life = game_lease.map(|lease| {
        crate::gamelease::SessionGuard::new(
            lease,
            quit.clone(),
            endpoint::peer_fingerprint(&conn).map(hex::encode),
            // The launch record's hold moves in here: its lifetime is exactly this session's, and its
            // drop is what opens the reconnect window the next `Hello::launch` is matched against.
            launch_claim,
        )
    });

    let perf = pf_host_config::config().perf;
    // Microburst cap (applied in send_loop/paced_submit): a frame ≤ the cap bursts out
    // immediately; only a bigger frame's overflow is spread. `None` = auto — max(128 KB, the
    // AU's wire bytes / 4), so the burst stays a bounded fraction of high-rate frames instead
    // of swallowing them whole (plan Phase 1.3). PUNKTFUNK_PACE_BURST_KB pins an absolute cap.
    let burst_cap: Option<usize> = std::env::var("PUNKTFUNK_PACE_BURST_KB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|kb| kb * 1024);

    // Encode|send split: this thread captures+encodes (the GPU work) + handles reconfig, and hands
    // each AU to a dedicated send thread that owns the Session and does FEC+seal+paced-send — so the
    // encode of frame N+1 overlaps the paced transmit of frame N instead of waiting behind its tail.
    // The bounded channel applies backpressure (the encode thread blocks if the send falls behind,
    // so frames slow down rather than a dropped frame freezing the infinite-GOP stream).
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
    // `live_bitrate` (SessionContext) is shared with the send thread's stats sample AND the
    // control task: a mid-stream adaptive bitrate change (bitrate_rx below) stores the
    // encoder-APPLIED rate, so the console, pacer and climb-refusal acks all see the truth.
    // Live session mode, same pattern (H3): a mid-stream mode switch (reconfig below) updates it so
    // a stats capture armed after a resize registers the real mode. Seeded with the refresh the
    // initial build actually achieved (`interval_hz`), not the request — KWin may cap a virtual
    // output at 60 Hz.
    // …and seeded with the SIZE the build actually delivered too, not just the refresh. Attach is
    // what made the asymmetry visible: `gamescope: box drives a physical display — attaching at its
    // own mode (no re-mode)` streams the panel's size, so a client that asked for 5120x1440 is
    // really watching 1920x1080 while `/api/v1/local/summary` echoed the request straight back and
    // the console reported a resolution nobody was looking at. The rebuild path below already
    // publishes `delivered_mode`; bring-up simply never did, so only a mid-stream resize ever
    // corrected it. This publishes the stats slot ONLY — it deliberately does not send the client a
    // corrective `Reconfigured`, which stays owed exactly where it already was (`adopted_at_bringup`
    // above): an ordinary connect's mode came from the Welcome, not from an accept the client has
    // acted on, so it is not owed one.
    let delivered = delivered_mode(frame.width, frame.height, interval);
    let live_mode = Arc::new(AtomicU64::new(pack_mode(
        delivered.width,
        delivered.height,
        delivered.refresh_hz,
    )));
    // One-shot force-keyframe flag driven by the management API (`POST /session/idr`, the web-console
    // Dashboard's "Request IDR" button) — drained in the encode loop below exactly like a client
    // decode-recovery request. Registered with `session_status` so the mgmt handler can reach THIS
    // session (the native plane never touches the GameStream `AppState.force_idr`).
    let force_idr = Arc::new(AtomicBool::new(false));
    // The send thread emits the web-console stats sample (it owns `session.stats()`); clone the
    // recorder so the capture loop keeps its own handle for the per-frame `is_armed()` gate.
    // Shared with the send thread: it is the only place a paced send is observed, and the encode
    // loop is the only place the encoder can be touched.
    let send_spread_us = Arc::new(AtomicU32::new(0));
    let send_spread_send = Arc::clone(&send_spread_us);
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
            move || {
                send_loop(
                    session,
                    frame_rx,
                    probe_rx,
                    probe_result_tx,
                    stop,
                    perf,
                    send_spread_send,
                    slice_wire,
                    burst_cap,
                    fec_target,
                    shard_rx,
                    send_stats,
                    timing_conn,
                    phase_send,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    // Publish this session to the plane-neutral live-session registry so the web-console Dashboard
    // (`GET /status`) shows the native stream — resolution/fps/codec/bitrate resolve live from the
    // same handles a mid-stream mode switch / adaptive-bitrate change updates. The guard clears the
    // entry when this loop exits (return / `?` / panic), so the Dashboard tracks the session's life.
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
    });

    // Mid-stream session-switch watcher (opt-in via PUNKTFUNK_SESSION_WATCH; never under an explicit
    // PUNKTFUNK_COMPOSITOR pin). It self-baselines and signals the loop below to swap the backend in
    // place when the box flips Gaming↔Desktop. When not spawned, session_rx just stays empty.
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
    // Phase-locked capture (design/phase-locked-capture.md): the per-frame hold this loop applies
    // after a fresh capture, walked ~1 Hz toward the client's reported arrival lead. A loop local
    // on purpose — it survives every in-loop rebuild path (session switch, mode/stall rebuilds,
    // encoder backoff), so a mid-stream rebuild keeps the acquired lock.
    let mut phase_ctl = PhaseController::new();
    // Frame-driven wire-rate cap (see [`PaceBudget`]): a loop local like `phase_ctl`, and for the
    // same reason — it must survive every in-loop rebuild path so a mid-stream rebuild can't
    // reopen the overshoot. Bounded burst (CAP) is all a rebuild gap can buy.
    let mut pace = PaceBudget::new(std::time::Instant::now());
    // The session's video frame numbering, owned HERE (the wire `frame_index` of the next AU this
    // loop hands to the send thread; the packetizer seals with exactly this via `seal_frame_at`).
    // A submission's future index is predicted as `au_seq + inflight.len()` — exact because AUs
    // are emitted FIFO, one per submission, and every event that forfeits in-flight frames
    // (reset/rebuild/teardown) clears `inflight` AND the encoder's reference state, so the reused
    // predictions can never meet stale bookkeeping. Passing it to `Encoder::submit_indexed` keeps
    // the RFI backends' frame numbers 1:1 with the client's across encoder rebuilds — an
    // encoder-internal counter desyncs on the first adaptive-bitrate rebuild (NVENC RFI then
    // silently dies; AMF may anchor onto a post-loss LTR).
    let mut au_seq: u32 = 0;
    // Rebuild-in-place on capture loss: track the live mode (a mode switch updates it) so a rebuild
    // targets the CURRENT mode, and cap consecutive rebuilds so a flapping source can't loop the
    // client through endless cold restarts.
    let mut cur_mode = mode;
    const MAX_CAPTURE_REBUILDS: u32 = 5;
    let mut capture_rebuilds: u32 = 0;
    // Exclusive-topology eviction generation last seen (Windows IDD-push; see the recovery block
    // in the loop): the vdisplay watchdog bumps it on every eviction, each of which drives
    // COMMIT_MODES on the live IDD path and orphans this pipeline's capture ring.
    #[cfg(target_os = "windows")]
    let mut seen_reassert_gen = crate::vdisplay::manager::topology_reassert_gen();
    // Encode-stall watchdog: AMF/QSV (and async NVENC) poll non-blocking, so a wedged driver
    // shows up as poll() returning None forever while submits keep succeeding — `inflight` grows,
    // no AU ever reaches the send thread, and the client freezes on the last frame with nothing
    // logged (field reports: AMD/Intel Windows streams freezing after minutes). Track when the
    // encoder last produced an AU and rebuild it in place (bounded, like the capture rebuilds)
    // when it stops. `ENCODE_STALL_WINDOW` also sizes the in-flight backlog bound: a backlog worth
    // more than the window's frames means AUs still trickle (so the gap never trips) but latency
    // is growing without bound — the slow-leak form of the same stall.
    const ENCODE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX_ENCODER_RESETS: u32 = 5;
    let mut encoder_resets: u32 = 0;
    let mut last_au_at = std::time::Instant::now();
    // Last HDR mastering metadata we forwarded — re-sent as 0xCE on change/keyframe (see below).
    let mut last_hdr_meta: Option<punktfunk_core::quic::HdrMeta> = None;
    // Frames submitted to NVENC but not yet polled (wire pts, submit stamp, pacing deadline). With a
    // capturer that hands a fresh output texture per frame, the loop submits N+1 before polling N
    // (pipeline depth > 1), overlapping the convert/copy of N+1 on the 3D engine with the encode of N
    // on the NVENC ASIC. The wire pts and the submit stamp are carried separately so `encode_us`
    // keeps meaning submit→AU while the wire pts anchors at PipeWire delivery (queue age included).
    let mut inflight: std::collections::VecDeque<(u64, u64, std::time::Instant)> =
        std::collections::VecDeque::new();
    // Diagnostic: distinguish NEW captured frames (the source produced a fresh frame) from REPEATS (the
    // loop re-encoded the last frame because `try_latest` had nothing). A low new-frame rate at a high
    // send rate ⇒ the capture source isn't producing frames (e.g. an IDD virtual display DWM isn't
    // compositing), NOT an encoder problem. Logged every 2 s when `PUNKTFUNK_PERF`.
    let (mut diag_new, mut diag_repeat) = (0u64, 0u64);
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
    // Per-session one-shot latches for the host-composite breadcrumbs below.
    #[cfg(not(target_os = "windows"))]
    let (mut composite_saw_overlay, mut composite_saw_none) = (false, false);
    let mut diag_at = std::time::Instant::now();
    // Anchor for the forced-IDR cooldown (see the keyframe-request handling below): the timestamp of
    // the most recent forced/opening IDR. The session's pipeline just opened on an IDR, so start the
    // clock now — that coalesces the keyframe storm a client fires while its decoder wedges on the cold
    // opening GOP, instead of answering it with a redundant second IDR.
    let mut last_forced_idr: Option<std::time::Instant> = Some(std::time::Instant::now());
    // A successful LTR-RFI recovery anchors THIS clock, not the IDR cooldown: it justifies
    // swallowing the client's `frames_dropped`-driven echo of the SAME loss (arriving ~one
    // loss-window later), but must never indefinitely defer the client's ESCALATION — a
    // keyframe request that keeps coming because the RFI recovery did not actually heal its
    // decoder. Re-anchoring the full IDR cooldown here (the old behavior) livelocked under
    // sustained loss: each new loss → RFI → cooldown re-anchored → the wedged client's IDR
    // pleas coalesced away forever, and the picture never recovered (the lid-closed Intel
    // laptop field report: permanent macroblock soup, dozens of swallowed requests per IDR).
    let mut last_rfi: Option<std::time::Instant> = None;
    // Keyframe requests swallowed on RFI-echo grounds since the last real IDR / quiet period.
    // Capped: requests past the cap mean RFI is not healing this client — escalate to the IDR.
    let mut rfi_echo_swallowed: u32 = 0;
    // When the previous keyframe request arrived — a long quiet gap means the client healed
    // and the next request opens a NEW loss episode (the echo-swallow budget resets).
    let mut last_kf_request: Option<std::time::Instant> = None;
    // Self-diagnosis for the periodic-stutter class: warns when the served recovery IDRs settle
    // into a stable multi-second rhythm (see [`pf_frame::metronome::Metronome`]).
    let mut recovery_cadence = pf_frame::metronome::Metronome::new();
    // Position within the current intra-refresh wave (frames since the last IDR/wave start). Only
    // meaningful on a `caps().intra_refresh_recovery` encoder; the pump tags every wave-boundary AU
    // with `USER_FLAG_RECOVERY_POINT` so the client can lift its post-loss freeze on a clean
    // re-anchor without a full IDR. Re-phased to 0 at each emitted IDR (which restarts the wave).
    let mut ir_wave_pos: u32 = 0;
    // Per-stage latency breakdown (PUNKTFUNK_PERF): per-call µs for the GPU-bound stages so we see
    // exactly where the capture→encoded latency goes — cap=try_latest (ring read + colour convert),
    // submit=encode_picture launch, wait=lock_bitstream (the scheduling wait + ASIC encode, the one
    // that dominates under a GPU-saturating game).
    let (mut st_cap, mut st_submit, mut st_wait, mut st_queue): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // Adaptive pipeline depth (see [`idd_adaptive_enabled`]): run depth-1 for latency and
    // escalate to the capturer's max on sustained cadence overrun. `cur_depth` is the live
    // target (clamped to the capturer's current max each iteration — a rebuild can change it);
    // `behind_score` is a leaky bucket over the "fell behind the cadence deadline" signal;
    // `depth_frames` skips the startup warmup so first-frame bring-up cost can't false-escalate.
    let mut cur_depth: usize = 1;
    let mut behind_score: u32 = 0;
    let mut depth_frames: u64 = 0;
    // Second escalation stage (§7 LN3): once depth is maxed (or was never available — Linux),
    // ask the encoder for pipelined retrieve exactly once. Latched whether it accepts or not.
    let mut pipeline_asked = false;
    // ~20 net behind-frames (≈0.3 s sustained) escalates; a lone hitch decays away. Warmup skips
    // the first ~1 s so bring-up (display acquire, encoder open) never triggers it.
    const DEPTH_ESCALATE: u32 = 20;
    const DEPTH_BEHIND_CAP: u32 = 60;
    const DEPTH_WARMUP_FRAMES: u64 = 60;
    // Half the escalate threshold: ~10 net behind-frames is already solid "the encoder, not the
    // network, is the bottleneck" evidence — enough to flag `cadence_degraded` (the control task
    // then refuses bitrate CLIMBS) well before the session pays a latency escalation for it.
    const DEPTH_DEGRADE: u32 = 10;
    // De-escalation (the escalate-and-hold v1's missing half): a sustained clean run at the
    // escalated setting (~5 s at 120 fps, every frame on cadence) earns ONE attempt at winding
    // back — reverse order of the escalation, pipelined retrieve first (its rebuild restores
    // sub-frame streaming and the IO-stream binding), then capture depth back to 1. Each
    // attempt costs the wind-back rebuild's IDR, so attempts are paced by an exponential
    // backoff (1 → 5 → 25 min, capped) — a workload that genuinely needs the escalation
    // converges to keeping it, but NEVER a permanent latch: a latch plus the ABR sawtooth
    // pinned sessions at the floor with the escalation stuck.
    const DEESCALATE_CLEAN_FRAMES: u32 = 600;
    const DEESCALATE_BACKOFF_START: std::time::Duration = std::time::Duration::from_secs(60);
    const DEESCALATE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(25 * 60);
    let mut pipelined_active = false;
    let mut deescalating = false;
    let mut ahead_run: u32 = 0;
    let mut deescalate_not_before: Option<std::time::Instant> = None;
    let mut deescalate_backoff = DEESCALATE_BACKOFF_START;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        // Mid-stream session switch (the box flipped Gaming↔Desktop): rebuild the WHOLE backend in
        // place — a different compositor at the SAME client mode — keeping the Session + send thread
        // (and thus the QUIC control + UDP data plane) up. Takes precedence over a queued mode change.
        let mut switch = None;
        while let Ok(s) = session_rx.try_recv() {
            switch = Some(s); // coalesce to the newest
        }
        if let Some(sw) = switch {
            if sw.compositor != compositor {
                tracing::info!(from = compositor.id(), to = sw.compositor.id(), kind = ?sw.kind,
                    "session switch — rebuilding backend in place");
                // Retarget the process env at the new session BEFORE opening the new backend (this
                // thread is the only env writer; the watcher only snapshots).
                crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
                    kind: sw.kind,
                    env: sw.env,
                    compositor_pid: None,
                });
                // A mid-stream Game↔Desktop switch is not a fresh dedicated launch — route input at the
                // switched-to backend's normal sub-mode.
                let switched_route = crate::vdisplay::apply_input_env(sw.compositor, false);
                // Switching INTO a desktop mid-stream: the xdg portal / systemd-user env may still
                // point at the old session, so input would silently not land until a reconnect.
                // Settle it (env push + KWin portal restart) before the injector reopens against it.
                if matches!(
                    sw.compositor,
                    crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
                ) {
                    crate::vdisplay::settle_desktop_portal(sw.compositor);
                }
                // Build the new backend's pipeline BEFORE dropping the old one (retry absorbs the
                // brief compositor-coexistence race during a switch); on failure keep the old.
                let rebuilt =
                    (|| -> Result<(Box<dyn crate::vdisplay::VirtualDisplay>, Pipeline)> {
                        let mut new_vd = crate::vdisplay::open(sw.compositor)?;
                        // The switch re-resolved the sub-mode; give it to the NEW instance, the
                        // same way the initial build does. Without this the rebuilt backend would
                        // have no route and fall through to a bare spawn.
                        new_vd.set_gamescope_route(switched_route.clone());
                        let pipe = build_pipeline_with_retry(
                            &mut new_vd,
                            cur_mode,
                            bitrate_kbps,
                            bitrate_auto,
                            bit_depth,
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
                        // Replace the pipeline first (drops the old capturer → old PipeWire stream +
                        // virtual output), then the factory (drops e.g. the old KWin connection).
                        capturer = new_cap;
                        enc = new_enc;
                        frame = new_frame;
                        interval = new_interval;
                        cur_node_id = new_node_id;
                        cur_display_gen = new_gen;
                        // The new compositor may deliver a different size than the old one did (a
                        // Game→Desktop switch onto a mirrored 4K panel is exactly that), so adopt
                        // the rate the rebuilt encoder actually opened at.
                        adopt_built_bitrate(
                            &mut bitrate_kbps,
                            new_bitrate,
                            &live_bitrate,
                            &retarget_tx,
                        );
                        vd = new_vd;
                        compositor = sw.compositor;
                        next = std::time::Instant::now();
                        // The owed AUs died with the old encoder — drop their in-flight records
                        // and restart the encode-stall clock for the fresh one.
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
        // Drain to the NEWEST requested mode (a resize drag queues many) so we rebuild once,
        // not once per stale intermediate mode.
        let mut want = None;
        while let Ok(m) = reconfig.try_recv() {
            want = Some(m);
        }
        if let Some(new_mode) = want {
            tracing::info!(?new_mode, "rebuilding pipeline for mode switch");
            // Resize trace (P0.1): reconfigure-received → pipeline rebuilt (incl. the first
            // new-mode frame — `build_pipeline` waits for it). Total lands in the shared
            // `resize_ms` slot (→ `session_status`); a failed rebuild abandons it silently.
            let resize_trace = crate::bringup::Trace::start("resize", resize_ms.clone());
            // PyroWave's Automatic bitrate is a per-mode ~1.6 bpp pin (resolve_bitrate_kbps_for) —
            // a resolution change moves the operating point (1080p→4K quadruples the pixel rate),
            // so re-resolve it for the new mode. Explicit client rates stay put (the operator knows
            // the link), and the H.26x codecs keep their mode-independent rate (ABR owns it).
            let mode_bitrate = if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                resolve_bitrate_kbps_for(plan.codec, 0, &new_mode, plan.chroma, plan.bit_depth)
            } else {
                bitrate_kbps
            };
            // IN-PLACE fast path first (latency plan P2.3, Windows IDD-push): keep the capturer +
            // send thread, mode-set the SAME monitor in place (P2.1/P2.2), resize the ring, swap
            // only the encoder. Any decline (v3 driver → the manager re-arrived, ring recreate
            // failed, no new-size frame) falls through to the full rebuild below.
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
                    plan,
                    &quit,
                    resize_trace.as_ref(),
                    false,
                );
            #[cfg(not(target_os = "windows"))]
            let fast_done = false;
            // The rate the rebuilt encoder ends up opened at. Seeded with the new mode's own
            // re-resolve, which is what the Windows in-place fast path applies (it swaps only the
            // encoder and never reaches `build_pipeline`); a full rebuild overwrites it with the
            // rate `build_pipeline` actually used, which differs when the source delivers a size
            // the mode did not ask for.
            let mut built_bitrate = mode_bitrate;
            // Full rebuild — build the new pipeline BEFORE dropping the old one: the host already
            // acked the switch as accepted, so a rebuild failure must not kill an otherwise
            // healthy session — keep streaming the current mode and log instead.
            let rebuilt = fast_done
                || match build_pipeline(
                    &mut vd,
                    new_mode,
                    mode_bitrate,
                    bitrate_auto,
                    bit_depth,
                    plan,
                    &quit,
                    // The display this rebuild supersedes (retired below once the new pipeline is
                    // up) — its replacement inherits the group topology (Primary/Exclusive).
                    cur_display_gen,
                    // No first-frame shortening here: this direct call has no retry wrapper to
                    // absorb an early bail, and the resize source is a live compositor (the
                    // takeover race doesn't apply) — keep the patient default.
                    None,
                    Some(resize_trace.as_ref()),
                ) {
                    Ok(next_pipe) => {
                        let old_display_gen = cur_display_gen;
                        // The destructuring assignment drops the OLD capturer (→ its display lease)
                        // as each binding is replaced — the new pipeline is already up
                        // (create-before-drop).
                        (
                            capturer,
                            enc,
                            frame,
                            interval,
                            cur_node_id,
                            cur_display_gen,
                            built_bitrate,
                        ) = next_pipe;
                        // H4: the old display's lease drop above is indistinguishable from a
                        // disconnect to the keep-alive machinery — under linger/forever policies
                        // every resize would ACCUMULATE kept monitors at stale modes. Retire the
                        // superseded entry now (a no-op when it was already torn down under
                        // `immediate`, or off Linux; the in-place fast path keeps the SAME display,
                        // so it has nothing to retire).
                        if let Some(g) = old_display_gen.filter(|g| cur_display_gen != Some(*g)) {
                            crate::vdisplay::registry::retire(g);
                        }
                        true
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), ?new_mode,
                            "mode-switch rebuild failed — staying on the current mode");
                        // H2 rollback: the control task acked the switch BEFORE this rebuild, so the
                        // client's mode slot already flipped to `new_mode`. A second accepted ack
                        // carrying the still-live mode corrects it (any accepted ack means "the
                        // active mode is now X" client-side; old clients just log it). `frame` is
                        // untouched here (the fast path returned false before swapping anything and
                        // the destructure only runs on the Ok arm), so it's still the OLD
                        // pipeline's frame — its real dims + interval are what's still on glass.
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
                // H2/H3: the backend may have honored a different mode than requested — KWin caps
                // a virtual output's refresh, or Windows pf-vdisplay rejects a resolution its
                // running monitor doesn't advertise and the host falls back to the actual display
                // mode. `frame` is the NEW pipeline's first frame (just rebound above), so its
                // dims are what the client actually decodes. Publish that ACTUAL mode to the live
                // stats slot, and correct the client's mode slot when it differs from the accept
                // ack it already got.
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
                // The owed AUs died with the old encoder — drop their in-flight records
                // and restart the encode-stall clock for the fresh one.
                inflight.clear();
                last_au_at = std::time::Instant::now();
                encoder_resets = 0;
                last_forced_idr = Some(std::time::Instant::now()); // fresh encoder opens on an IDR — anchor the cooldown
                resize_trace.finish("pipeline_rebuilt");
            }
        }
        // Exclusive-topology eviction recovery (Windows IDD-push): the vdisplay watchdog just
        // evicted a display that crept back into the "exclusive" desktop, via the full isolate —
        // its forced re-commit restarts OS presentation to the virtual display (a gentle
        // supplied-config eviction left capture one stashed frame and then nothing, on-glass),
        // but it also hands the live IDD path a fresh swap-chain while this pipeline's ring
        // keeps waiting on the old attachment; with an unchanged descriptor the poller's
        // two-strike debounce never trips, so frames would just stop. Rebuild the capture
        // attachment in place at the CURRENT mode (same-mode ring recreate + driver re-attach +
        // fresh encoder — the resize fast path's cost). If even that fails, end the session with
        // a clear error: the client's reconnect rebuilds from scratch, which beats streaming a
        // frozen image forever.
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
                    plan,
                    &quit,
                    trace.as_ref(),
                    true,
                ) {
                    // The owed AUs died with the old encoder — same bookkeeping as a resize.
                    inflight.clear();
                    last_au_at = std::time::Instant::now();
                    encoder_resets = 0;
                    last_forced_idr = Some(std::time::Instant::now());
                    trace.finish("pipeline_rebuilt");
                } else {
                    return Err(anyhow!(
                        "exclusive-topology eviction recovery failed — ending the session for a \
                         clean reconnect (a fresh bring-up re-attaches capture)"
                    ));
                }
            }
        }
        // Adaptive bitrate: drain to the NEWEST requested rate (the client's controller may step
        // several times while we stream) and retarget the ENCODER ONLY — the mode didn't change,
        // so capture and the virtual output are untouched. Preferred lever: an IN-PLACE
        // `reconfigure_bitrate` (Phase 3.2 — NVENC nvEncReconfigureEncoder / AMF dynamic props /
        // Vulkan RC control), which keeps the encoder, its reference chain and the in-flight AUs,
        // so the step costs NOTHING on the wire (no IDR, no forfeit — exactly what the Automatic
        // controller's doubling climb wants). A backend that can't (libavcodec paths) or a driver
        // rejection falls back to the full rebuild, which costs the IDR the fresh encoder opens
        // with (the same resync discipline as a mode switch, minus the pipeline churn) and owns
        // the bitrate clamping. Rates arrive pre-clamped by the control task
        // (`resolve_bitrate_kbps`).
        let mut want_kbps = None;
        while let Ok(k) = bitrate_rx.try_recv() {
            want_kbps = Some(k);
        }
        // Known-ceiling pre-clamp (§ABR overdrive): once the encoder's codec-level ceiling is
        // known, resolve an over-asking request HERE — a request that clamps to the rate we're
        // already at then skips the whole apply, where the pre-fix path bounced every overshoot
        // off the driver into a full rebuild + IDR (~0.6 s each, four in one logged minute).
        // (The control task clamps its acks from the same atomic; this covers requests already
        // in flight when the ceiling was discovered.)
        // Give the encoder the send cost it cannot measure. Cheap, and it is what lets the split
        // arbiter price the sub-frame trade instead of refusing to arbitrate HEVC at all.
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
            if enc.reconfigure_bitrate(new_kbps as u64 * 1000) {
                // Adopt the encoder's post-clamp truth, not the request: it feeds the send
                // pacer, the console/mgmt view and the control task's acks, and a short apply
                // teaches the ceiling used above.
                let applied_kbps = enc
                    .applied_bitrate_bps()
                    .map(|b| (b / 1000) as u32)
                    .filter(|&k| k > 0)
                    .unwrap_or(new_kbps);
                tracing::info!(
                    from_kbps = bitrate_kbps,
                    to_kbps = applied_kbps,
                    requested_kbps = new_kbps,
                    "encoder bitrate reconfigured in place (adaptive bitrate — no IDR)"
                );
                if applied_kbps < new_kbps {
                    encoder_ceiling_kbps.store(applied_kbps, Ordering::Relaxed);
                    // The control task already acked the client with its own resolve, which was
                    // higher than what the encoder took. Correct it, or the controller climbs
                    // from a rate the encoder never ran at until its NEXT request happens to be
                    // pre-clamped by the ceiling we just stored.
                    let _ = retarget_tx.send(applied_kbps);
                }
                if applied_kbps < bitrate_kbps {
                    // Down-step: the behind-cadence backlog was scored against the old,
                    // heavier rate — clean slate so it can't feed a false escalation.
                    behind_score = 0;
                }
                bitrate_kbps = applied_kbps;
                live_bitrate.store(applied_kbps, Ordering::Relaxed);
                // Same encoder, same stream: the in-flight AUs and the wire-index prediction
                // stay valid — no inflight forfeit, no IDR-cooldown anchor.
            } else {
                // `interval` was built as 1/effective_hz, so the round-trip recovers the integer
                // rate.
                let hz = interval_hz(interval);
                match crate::encode::open_video(
                    plan.codec,
                    frame.format,
                    frame.width,
                    frame.height,
                    hz,
                    new_kbps as u64 * 1000,
                    frame.is_cuda(),
                    bit_depth,
                    plan.chroma,
                    plan.cursor_blend,
                    plan.max_slices,
                ) {
                    Ok(mut new_enc) => {
                        // The fresh encoder may have clamped to its codec-level ceiling —
                        // adopt (and record) ITS rate, not the request; see the in-place arm.
                        let applied_kbps = new_enc
                            .applied_bitrate_bps()
                            .map(|b| (b / 1000) as u32)
                            .filter(|&k| k > 0)
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
                        // (`max_depth` is computed later in the iteration — read the capturer
                        // directly so an ABR rebuild re-establishes the bound immediately.)
                        new_enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
                        enc = new_enc;
                        if applied_kbps < new_kbps {
                            encoder_ceiling_kbps.store(applied_kbps, Ordering::Relaxed);
                            // As in the in-place arm: the ack the client already has promises
                            // more than the fresh encoder accepted — correct it.
                            let _ = retarget_tx.send(applied_kbps);
                        }
                        bitrate_kbps = applied_kbps;
                        live_bitrate.store(applied_kbps, Ordering::Relaxed);
                        // The owed AUs died with the old encoder — same bookkeeping as a
                        // mode-switch rebuild; the fresh encoder opens on an IDR, so anchor the
                        // IDR cooldown too.
                        inflight.clear();
                        last_au_at = std::time::Instant::now();
                        encoder_resets = 0;
                        last_forced_idr = Some(std::time::Instant::now());
                        // The rebuild stall itself (~0.6 s ≈ 70 missed deadlines at 120 fps,
                        // 3.5× the escalate threshold) must not feed the contention
                        // escalation — clean slate + re-run the warmup before judging again.
                        behind_score = 0;
                        depth_frames = 0;
                        ahead_run = 0;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), to_kbps = new_kbps,
                            "bitrate-change encoder rebuild failed — keeping the current rate");
                    }
                }
            }
        }
        // Client recovery: it asked for a fresh IDR (its decoder wedged on the cold opening
        // GOP). Coalesce the backlog — several requests fire before the IDR lands — and force
        // the next encoded frame to be a keyframe. (A reconfig rebuild above already opens with
        // an IDR, so this is for the steady-state wedge, not mode switches.)
        let mut want_kf = false;
        while keyframe.try_recv().is_ok() {
            want_kf = true;
        }
        // Management API `POST /session/idr` (web-console Dashboard) targets this session's registry
        // flag; drain it into the same forced-keyframe path a client decode-recovery request takes.
        if force_idr.swap(false, Ordering::Relaxed) {
            want_kf = true;
        }
        // Client LTR-RFI recovery: prefer re-referencing a known-good older frame (a clean recovery
        // P-frame — no 20-40× IDR spike) over a full keyframe when the encoder supports it (native
        // AMF LTR / Windows NVENC). Drain the backlog (the client re-requests until the recovery
        // frame lands) coalesced to the widest lost range. Attempt the invalidate only when a full
        // IDR isn't already queued — an explicit keyframe request means a fully wedged decoder that
        // needs the IDR, which supersedes an RFI recovery. A failure (range older than the encoder's
        // live references, or no RFI backend) falls through to the coalesced keyframe path below.
        let mut rfi_range: Option<(u32, u32)> = None;
        while let Ok((first, last)) = rfi.try_recv() {
            rfi_range = Some(match rfi_range {
                Some((pf, pl)) => (pf.min(first), pl.max(last)),
                None => (first, last),
            });
        }
        // All-intra (§4.6): every PyroWave AU is a keyframe, so the NEXT frame already is
        // the recovery a request asks for — drop the drained requests instead of running
        // the forced-IDR cooldown / RFI / storm machinery (whose frame-size reasoning is
        // meaningless when frames are uniform). Defense in depth: the backend's
        // request_keyframe/invalidate_ref_frames are no-ops anyway.
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
                // Sanity-cap the range before consulting the encoder: RFI can only re-reference
                // history the encoder still holds (NVENC: a 5-frame DPB; AMD LTR: ~1 s of marks).
                // A range wider than RFI_MAX_RANGE is either a seconds-long outage (no valid
                // reference anywhere) or a phantom jump from a desynced counter — both belong on
                // the keyframe path, never a force-reference that could ship corruption as a
                // recovery anchor. Wrapping width: frame indexes are u32 counters.
                let width = last.wrapping_sub(first);
                if width > punktfunk_core::packet::RFI_MAX_RANGE {
                    tracing::debug!(first, last, width, "RFI range too wide — keyframe instead");
                    want_kf = true;
                } else if enc.caps().supports_rfi
                    && enc.invalidate_ref_frames(first as i64, last as i64)
                {
                    // The RFI recovered the loss with a clean re-anchor P-frame (no IDR). Anchor
                    // the RFI-echo window (NOT the IDR cooldown — see `last_rfi`) so the client's
                    // echo of the SAME loss — its frames_dropped-driven keyframe request, arriving
                    // ~one loss-window later — is coalesced away instead of emitting a redundant
                    // full IDR right after the cheap recovery.
                    last_rfi = Some(std::time::Instant::now());
                } else {
                    want_kf = true; // range too old / no RFI backend → coalesced keyframe below
                }
            }
        }
        if want_kf {
            // Clients request a keyframe on EVERY FEC-unrecoverable frame (`frames_dropped` polling)
            // and keep asking until the IDR actually arrives + decodes — a full round-trip on a link
            // that is already behind. Answering each request with a full IDR is a 20-40× bitrate spike
            // that DEEPENS the very loss it is recovering from: a burst of loss → a storm of IDRs →
            // more loss, the periodic double-jolt a Wi-Fi client sees. So coalesce a request storm into
            // at most ONE forced IDR per cooldown, ALWAYS — not only under intra-refresh (the old gate;
            // a full-IDR recovery is exactly where the storm is worst). Serve the first request
            // immediately (a genuinely wedged decoder recovers at once), then suppress for the window.
            //
            // Intra-refresh heals via its own gradual wave (~0.5 s) and can afford a long window; a
            // full-IDR recovery relies on the keyframe itself, so its window is shorter — long enough to
            // swallow the round-trip echo of one recovery event, short enough to re-issue a *lost* IDR
            // promptly.
            const IDR_COOLDOWN_INTRA: std::time::Duration = std::time::Duration::from_secs(2);
            const IDR_COOLDOWN_FULL: std::time::Duration = std::time::Duration::from_millis(750);
            // The RFI-echo window: how long after a successful LTR-RFI recovery a keyframe
            // request is presumed to be the client's echo of the SAME loss (the recovery frame
            // is still in flight / just decoding) rather than an escalation. Field data: the
            // echo lands ~110-130 ms after the RFI on a LAN-ish RTT.
            const RFI_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);
            // How many requests the echo window may swallow per loss episode. Requests past
            // this budget mean the LTR-RFI recoveries are NOT healing the client (anchor lost,
            // or corrupt client-side) — serve the IDR it is asking for. Without the cap, a
            // sustained-loss session (RFI every few hundred ms, each re-opening the window)
            // suppressed the client's escalation indefinitely.
            const RFI_ECHO_MAX_SWALLOWED: u32 = 2;
            // A quiet gap since the last keyframe request = the client healed; the next
            // request opens a NEW loss episode with a fresh echo-swallow budget.
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
                tracing::debug!("keyframe request coalesced — within the IDR cooldown");
            } else if rfi_echo {
                rfi_echo_swallowed += 1;
                tracing::debug!(
                    swallowed = rfi_echo_swallowed,
                    "keyframe request coalesced — echo of an RFI-recovered loss"
                );
            } else {
                tracing::debug!("forcing keyframe (client decode recovery)");
                enc.request_keyframe();
                last_forced_idr = Some(now);
                rfi_echo_swallowed = 0; // the IDR resets the episode — echoes of IT coalesce via the cooldown
                if let Some(period) = recovery_cadence.note(now) {
                    // A period that lands on the CLIENT's jump-to-live cooldown is not evidence
                    // of a periodic disturbance here at all — it is the client shedding a
                    // standing receive queue, which it is rate-limited to do exactly this often
                    // (`punktfunk_core::client::FLUSH_COOLDOWN`), so the cadence is a property of
                    // our own backpressure code rather than of anything physical. Naming display
                    // churn there sent a 2026-08-13 field investigation at three innocent
                    // subsystems while the real chain was: client refused the codec → demoted to
                    // a slower decode rung → could not sustain the rate → standing queue.
                    // Perfect periodicity argues FOR a software cooldown, not against it.
                    if matches_client_flush_cadence(period) {
                        tracing::warn!(
                            period_s = format!("{:.1}", period.as_secs_f64()),
                            "client keyframe recoveries match the client's jump-to-live cooldown \
                             — the CLIENT cannot sustain the stream and is shedding a standing \
                             receive queue (check its log for 'receive backlog stopped draining' \
                             with queue_depth, and for a decode rung that demoted); a slower \
                             decode path or a link below the bitrate does this, and it is NOT a \
                             host display disturbance"
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
        // Measure the per-stage split when `PUNKTFUNK_PERF` is set OR a web-console stats capture is
        // armed (a cheap Relaxed atomic, re-read each frame). The values feed the existing perf log
        // unchanged and ride each FrameMsg to the send thread, which builds the aggregated sample.
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
                frame = f;
                diag_new += 1;
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
                    // v3 grid actuation: sleep to the next submit-grid instant (an absolute
                    // grid — a per-frame additive hold free-runs once it saturates the loop and
                    // the phase dissolves; a periodic grid cannot). Disengaged = no sleep.
                    if let Some(t) = phase_ctl
                        .next_submit_target(std::time::Instant::now(), interval.as_nanos() as i64)
                    {
                        let now = std::time::Instant::now();
                        if t > now {
                            std::thread::sleep(t.duration_since(now));
                        }
                    }
                }
                capture_rebuilds = 0; // a delivered frame clears the consecutive-loss counter
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
                diag_repeat += 1; // no new frame (static desktop / mid-rebuild) — repeat the last
                repeat = true;
            }
            // The capture source died (PipeWire/compositor thread ended, virtual output gone). Rather
            // than tear the whole session down — the client has no reconnect path and would have to
            // cold-restart the handshake — rebuild the pipeline IN PLACE at the current mode, exactly
            // like a mode/session switch. A genuinely dead source still ends the session once the
            // bounded retry is exhausted; the consecutive cap stops a flapping source from looping the
            // client through endless cold IDRs.
            Err(e) => {
                // B2: a DEDICATED gamescope game session whose gamescope node is gone = the game
                // exited (gamescope is a single-app compositor — it dies with its app). End the session
                // CLEANLY — close with `APP_EXITED_CLOSE_CODE` so a launcher client returns to its
                // library instead of surfacing a failure — rather than the capture-loss rebuild + 40 s
                // timeout. Gated to the dedicated bare-spawn launch (`launch_is_nested`), so a normal
                // Bazzite/desktop capture loss still rebuilds in place.
                // `cur_node_id` (the capture 5-tuple's node id) is read only by the Linux
                // dedicated-game-exit check below; keep it read on other platforms so it isn't a
                // write-only variable under `-D warnings` (the `let _ = &launch` idiom above).
                #[cfg(not(target_os = "linux"))]
                let _ = &cur_node_id;
                // Backstop for a nested launch the lease can't recognize (no detect signals): a
                // bare-spawn gamescope exits with its child, so its node staying gone means the game
                // quit. Honors the same operator setting as the lease's own exit path — with
                // end-session-on-game-exit off, a lost capture is just a rebuild.
                #[cfg(target_os = "linux")]
                if launch.is_some()
                    && crate::session_settings::get().session_on_game_exit
                    && crate::vdisplay::launch_is_nested(compositor, gamescope_route.as_ref())
                    && crate::vdisplay::dedicated_game_exited(cur_node_id)
                {
                    tracing::info!(
                        "dedicated game session: the game exited — ending the session cleanly"
                    );
                    quit.store(true, Ordering::SeqCst); // skip keep-alive linger — the game is gone
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
                // A Bazzite/SteamOS Gaming↔Desktop switch tears the old compositor down and can take
                // 15s+ to bring the new one up. Don't fail the session over that (the client would
                // have to cold-reconnect, surfacing a "session failed") — keep retrying within a
                // generous budget while the QUIC keepalive (its own thread) holds the connection,
                // RE-DETECTING the live compositor each attempt so we follow the box to whatever
                // session comes up: a fresh instance of the same compositor, OR a different one
                // (the kind-change case the session watcher also handles). The client stays
                // connected, frozen on the last frame, and the stream resumes when the new output
                // appears — no reconnect.
                const REBUILD_BUDGET: std::time::Duration = std::time::Duration::from_secs(40);
                // A managed/attach gamescope (re)launch legitimately takes up to 45 s — the Steam
                // Big Picture cold start that `launch_session`/`ensure_box_gamescope_mode` poll
                // for — so the 40 s budget used to expire INSIDE the first attempt (a single-shot
                // failure ending the session even when a second, warm attempt would have
                // succeeded). Give gamescope-targeted rebuilds room for two full launch attempts;
                // desktop compositors keep the tighter budget. Checked per iteration because the
                // loop retargets `compositor` as re-detection follows the box.
                const GAMESCOPE_REBUILD_BUDGET: std::time::Duration =
                    std::time::Duration::from_secs(100);
                // Attach-only holdoff: for the first seconds after a capture loss the session
                // detection can be STALE (the new session isn't up yet), and a rebuild acting on
                // a stale "Gaming" answer restarts gamescope-session.target — which on SteamOS
                // steals the seat back from the session the user just switched to (observed
                // live). While the holdoff lasts, builds run under a vdisplay rebuild-probe
                // scope: attach to live outputs only, never stop/relaunch/take over sessions.
                const PROBE_HOLDOFF: std::time::Duration = std::time::Duration::from_secs(4);
                let loss_at = std::time::Instant::now();
                // An explicit PUNKTFUNK_COMPOSITOR pin disables the re-detection below — the
                // stream cannot follow a session switch. When the live session no longer matches
                // the pin, say so loudly ONCE per loss: this rebuild can only retry the pinned
                // backend and will die at the budget (the "mid-stream switch to game mode kills
                // the stream" field reports all traced back to a stale pin).
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
                    // Follow the active session unless an explicit PUNKTFUNK_COMPOSITOR pin forbids
                    // retargeting (then we stick to the pinned backend and just rebuild it).
                    if pf_host_config::config().compositor.is_none() {
                        let active = crate::vdisplay::detect_active_session();
                        // A4: fold any compositor-instance change into the epoch/invalidation before we
                        // rebuild, so the rebuild's acquire won't reuse a dead-instance node.
                        crate::vdisplay::observe_session_instance(&active);
                        if let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) {
                            crate::vdisplay::apply_session_env(&active);
                            // Capture-loss rebuild follows the live box session, not a fresh dedicated launch.
                            let rebuilt_route = crate::vdisplay::apply_input_env(c, false);
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
                                        // remote-desktop-sweep Phase C: the cursor pipeline was
                                        // resolved for the OLD compositor (e.g. a Desktop session
                                        // that then launched a game). Re-gate against the LIVE one,
                                        // mirroring SessionPlan::resolve: a switch TO gamescope must
                                        // build the encoder blend + attach the XFixes source on the
                                        // rebuild below (gamescope can't embed a pointer or carry a
                                        // capture-mode channel); a switch AWAY restores the prior
                                        // gating. `plan` is `Copy` — this is the value the rebuild
                                        // (and its `build_pipeline` attach) reads.
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
                                        gamescope_composite =
                                            plan.gamescope_cursor && cursor_fwd.is_none();
                                        metadata_composite = cursor_fwd.is_none()
                                            && plan.cursor_blend
                                            && c != crate::vdisplay::Compositor::Gamescope;
                                        // The retargeted backend starts with `hw_cursor`
                                        // unset — without re-applying the session's
                                        // out-of-band cursor request, the rebuilt display
                                        // would come up EMBEDDED: double-drawn for a
                                        // desktop-model channel client, cursorless for
                                        // every host-composite session.
                                        vd.set_hw_cursor(plan.cursor_forward || metadata_composite);
                                    }
                                    Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                                        "capture loss: opening the newly-detected compositor failed — retrying"),
                                }
                            }
                            // The rebuild re-resolved the sub-mode; hand it to whichever backend
                            // instance the rebuild will use — the freshly opened one on a
                            // compositor switch, or the existing one when the backend is unchanged.
                            // Skipping this would leave the rebuilt display on a bare spawn.
                            vd.set_gamescope_route(rebuilt_route.clone());
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
                        plan,
                        &quit,
                        &stop,
                        // 1, not 8: this loop re-detects the active session per iteration — short
                        // inner cycles are what let it FOLLOW a session switch instead of burning
                        // retries against a compositor that no longer exists. One attempt per
                        // cycle also keeps every probe on the SHORT first-frame window (attempt 1
                        // = 2.5 s): a patient 10 s attempt here just waits on a stale backend
                        // (observed live: it made a Game→Desktop switch 20 s instead of ~9 —
                        // the winning KWin rebuild took 0.7 s once detection caught up). The
                        // slow-new-session case is the OUTER loop's job (40 s budget, fresh
                        // 2.5 s probes until the new compositor delivers).
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
                            // Probe failures are instant (attach-only bail) — pace the loop so
                            // re-detection runs at ~2 Hz instead of spinning.
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
                // The rebuild re-ran `create`, so the portal answered again — possibly a different
                // backend's portal (the retarget above), possibly with a different verdict. Settle
                // the cursor plan against THIS display, exactly as bring-up did: the retarget arm
                // recomputes `metadata_composite` from the compositor alone (it has to — it runs
                // BEFORE the rebuild, to set `hw_cursor`), so without this a switch onto a wlr
                // backend would re-arm both defects mid-session.
                #[cfg(target_os = "linux")]
                {
                    no_overlay_means_off_output =
                        settle_portal_cursor(&*vd, &mut metadata_composite);
                }
                // A capture-loss rebuild can land on a different source than it lost (this loop
                // re-detects the session every cycle, precisely so it can follow a switch), so the
                // delivered size — and with it an Automatic rate — may have changed under us.
                adopt_built_bitrate(&mut bitrate_kbps, new_bitrate, &live_bitrate, &retarget_tx);
                enc.request_keyframe(); // belt-and-suspenders; a fresh encoder opens on an IDR anyway
                last_forced_idr = Some(std::time::Instant::now()); // anchor the IDR cooldown from the rebuild
                next = std::time::Instant::now();
                // The owed AUs died with the old encoder — drop their in-flight records and
                // restart the encode-stall clock (the rebuild loop above may have eaten seconds,
                // which must not count against the fresh encoder).
                inflight.clear();
                last_au_at = std::time::Instant::now();
                encoder_resets = 0;
                tracing::info!(
                    compositor = compositor.id(),
                    "capture loss: pipeline rebuilt — stream resumes"
                );
            }
        }
        // Cursor channel (M2 + the §8 mid-stream render flip). While the CLIENT draws the
        // pointer (the desktop mouse model): every iteration — new frame OR repeat — states
        // the pointer (self-healing under datagram loss) and forwards a changed shape via the
        // control bridge; `frame.cursor` is stripped so no blend path double-draws it. While
        // the HOST composites (the capture model, `CursorRenderMode { client_draws: false }`):
        // the forwarder goes quiet and `frame.cursor` rides into the encoder blend (Linux —
        // on Windows the flip re-enables DWM composition via the capturer hook below, and
        // frames never carry an overlay). A hidden-but-known pointer (overlay with
        // `visible: false`) is the M3 relative-mode hint. The capturer's LIVE cursor (the
        // Windows GDI-poller channel, where pointer-only moves produce no frame) outranks the
        // frame-attached overlay (the Linux portal path).
        if let Some(fwd) = cursor_fwd.as_mut() {
            let client_draws = cursor_client_draws.load(Ordering::Relaxed);
            // EVERY tick, not edge-gated: the capturer caches the applied state (an Option
            // compare in steady state) and clears it on channel re-deliveries — so the render
            // state survives capturer rebuilds AND driver-side monitor re-arrivals, which an
            // edge detector here silently lost. Windows IDD (un)declares the driver's hardware
            // cursor; no-op on every other capturer.
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
                // Entering the capture model: the client is now relative-only, so re-arm the
                // park schedule — the pointer may have drifted onto another monitor while the
                // desktop model steered it, and a capture-model session can never bring it
                // back on its own (see `park_pointer`).
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
                // The client draws the pointer — a blend-capable encoder must not also draw it.
                frame.cursor = None;
            } else {
                // Host composites (Linux): the encoder blend IS the composite mechanism, but the
                // frame-attached overlay is the position at the LAST DAMAGE frame — repeats
                // re-encoding a static desktop froze the blended pointer between redraws
                // (on-glass: composite cursor stuttered while window drags, constant damage,
                // were smooth). Refresh the repeat's overlay from the capturer's LIVE cursor so
                // pointer-only motion re-blends at tick rate — the same bandwidth the pre-channel
                // embedded mode paid, where the compositor damaged frames for cursor moves.
                // NOT Windows: its capturer composites internally (cursor_blend.rs) and frames
                // must never carry an overlay a blend path would double-draw.
                #[cfg(not(target_os = "windows"))]
                {
                    // One-shot breadcrumbs (per session), both directions: capture-mode field
                    // triage starts with "did the composite arm ever SEE an overlay" —
                    // pf-capture's sibling lines say whether the meta/bitmap arrived; these say
                    // whether the encoder was ever handed one.
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
                                // NOT necessarily "cursorless", which this line used to assert:
                                // where the portal negotiated Embedded (the whole wlr family) no
                                // `SPA_META_Cursor` is ever sent and the compositor's own pointer
                                // is already in the pixels — the host blend has nothing to add.
                                // `settle_portal_cursor` logs which of the two this session is.
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
            // No channel, host always composites: gamescope (Phase C — the XFixes source
            // publishes on `capturer.cursor()`) and the metadata-composite session (the portal
            // `SPA_META_Cursor` live overlay publishes there too). Refresh the (repeat or new)
            // frame's overlay from the capturer's LIVE cursor so pointer-only motion on a
            // static desktop re-blends at tick rate instead of freezing at the last damage
            // frame (the same reason the channel's composite arm above re-reads it). A
            // grabbed/hidden pointer arrives `visible: false` and is stripped just below.
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
        // The overlay surfaces hidden pointers too (for the hint above) — strip them
        // HERE, after forwarding, so no blend path ever draws an invisible cursor.
        if frame.cursor.as_ref().is_some_and(|c| !c.visible) {
            frame.cursor = None;
        }
        // The seat-pointer park schedule (state + rationale at the declarations above; armed by
        // the first frame of every (re)built display and by the capture-model flip).
        //
        // Two unconditional attempts for a RELATIVE-ONLY session — attempt 1 can be swallowed by
        // a cold EIS connection, and nothing else will ever move that pointer onto the output.
        // Exactly ONE for a client that steers the seat pointer itself (a channel client in the
        // desktop model): the park still runs once at bring-up, so the session's first click
        // cannot land on whatever monitor the seat pointer was left on, but the retry a second
        // later is dropped — by then the client's own absolute moves are doing this job, better
        // (same event, aimed where the user is actually pointing), and the retry is a visible
        // yank to centre that fights them (field report 2026-08-14; see `park_pointer`). A cold
        // EIS swallows the client's moves too, and those keep coming, so it needs no retry.
        //
        // Past the unconditional attempts, only a host-composite session that STILL has no live
        // overlay keeps trying — a channel session in the capture model, or a no-channel
        // metadata-composite session: no overlay there means the pointer has not reached the
        // streamed output (the compositor reports cursor metadata only while it is over the
        // recorded view), and a relative-only client cannot get it there on its own.
        //
        // ...but ONLY where that inference holds — `no_overlay_means_off_output`, settled per
        // display by `settle_portal_cursor`. Under a portal that negotiated Embedded no cursor
        // metadata is EVER sent, so "no overlay" says nothing about where the pointer is, and
        // this heuristic re-centred the user's pointer for the full cap on every Hyprland/sway
        // session (the same field report). Those sessions still get the two unconditional
        // attempts, which are what puts the pointer — and thus the compositor's burnt-in cursor,
        // and the client's input — on the streamed output in the first place.
        //
        // Armed from the loop's first tick — a static desktop may never deliver a fresh frame
        // (`parked_display` is only bookkeeping for rebuild re-arming), and the pointer must be
        // parked regardless.
        #[cfg(target_os = "linux")]
        if compositor != pf_vdisplay::Compositor::Gamescope
            && park_attempts < PARK_ATTEMPTS_MAX
            && std::time::Instant::now() >= next_park_at
        {
            let client_steers = cursor_fwd.is_some() && cursor_client_draws.load(Ordering::Relaxed);
            let unconditional = if client_steers { 1 } else { 2 };
            // Never true while `client_steers`: the channel term excludes it outright, and
            // `metadata_composite` implies no channel at all.
            let composite_starved = ((cursor_fwd.is_some() && !client_steers)
                || metadata_composite)
                && capturer.cursor().is_none()
                && no_overlay_means_off_output;
            if park_attempts < unconditional || composite_starved {
                park_pointer(&input_tx, frame.width, frame.height);
                park_attempts += 1;
                next_park_at = std::time::Instant::now() + std::time::Duration::from_secs(1);
            } else {
                // Settled (the client steers, the overlay is flowing, or its absence carries no
                // information here): stop scheduling until a rebuild or a capture-model flip
                // re-arms it.
                park_attempts = PARK_ATTEMPTS_MAX;
            }
        }
        if perf && diag_at.elapsed() >= std::time::Duration::from_secs(2) {
            let secs = diag_at.elapsed().as_secs_f64();
            tracing::info!(
                new_fps = format!("{:.0}", diag_new as f64 / secs),
                repeat_fps = format!("{:.0}", diag_repeat as f64 / secs),
                "capture diag: NEW frames from the source vs REPEATS (low new_fps at high send rate ⇒ \
                 the source isn't producing frames, not an encode stall)"
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
            diag_at = std::time::Instant::now();
        }
        // The source's static HDR mastering metadata is the single source of truth: hand it to the
        // encoder (in-band SEI on keyframes) and, when it changes, to the client (0xCE). Re-sent on
        // each keyframe below so a dropped best-effort datagram converges within a GOP. PRESENCE is
        // the capturer's call (Some iff the virtual display is in HDR mode); the VALUE prefers the
        // client's own display volume when it sent one — the virtual display's EDID advertises
        // exactly that volume, so host apps already tone-mapped the content into it and the honest
        // mastering description IS the client's panel. (The IDD capturer only knows the generic
        // baseline; if the driver ever forwards per-content IDDCX_HDR10_METADATA, prefer that here.)
        let hdr_meta = capturer.hdr_meta().map(|m| client_hdr.unwrap_or(m));
        enc.set_hdr_meta(hdr_meta);
        let mut resend_meta = hdr_meta != last_hdr_meta;
        if resend_meta {
            last_hdr_meta = hdr_meta;
        }
        // How deep to pipeline (1 = synchronous submit→poll, the original behaviour). The IDD-push
        // capturer hands a rotating ring of output textures, so it returns >1; other capturers default 1.
        // Adaptive (default): start at 1 for latency, `cur_depth` escalates on sustained overrun (the
        // tail below). Pinned to the capturer's max when adaptive is off or the max is already 1.
        let max_depth = capturer.pipeline_depth().max(1);
        let depth = if idd_adaptive_enabled() {
            cur_depth.clamp(1, max_depth)
        } else {
            max_depth
        };
        let submit_ns = now_ns();
        // Wire pts: a fresh frame anchors at its capture-delivery stamp (`CapturedFrame.pts_ns`,
        // stamped when the capture thread handed it over) so client-measured latency covers
        // delivery + queue age, not just submit→glass; `queue_us` splits that age out as its own
        // stage. A re-encoded hold anchors at "now" (its content age is unbounded by design). The
        // stamp must be a recent wall-clock time — a synthetic/index-based or ahead-of-clock stamp
        // (SyntheticCapturer counts from 0, not the epoch) falls back to "now".
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
        // This submission's future wire frame index (see `au_seq`): AUs are emitted FIFO one per
        // submission, so it lands `inflight.len()` AUs after the `au_seq` the loop is about to
        // assign next. The RFI backends pin their frame numbering to it.
        let wire_index = au_seq.wrapping_add(inflight.len() as u32);
        if let Err(e) = enc.submit_indexed(&frame, wire_index) {
            // A typed-terminal error is a deterministic configuration failure — the identical
            // wall on every attempt, so rebuilds can't help. End the session at once with the
            // carried cause (observed: a stale PUNKTFUNK_ENCODER pin vs. the selected adapter
            // burned all 5 rebuilds per connect while the client reconnected forever).
            if e.downcast_ref::<crate::encode::TerminalEncoderError>()
                .is_some()
            {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "encoder failed with a deterministic configuration error — ending the video \
                     session without rebuild attempts (see the error for the remedy)");
                return Err(e).context("encoder submit");
            }
            // The input half of an encode stall: once the driver stops draining AUs, libavcodec's
            // one-frame buffer fills and avcodec_send_frame starts failing (EAGAIN) — the same
            // wedge the watchdog below catches, seen from submit. Rebuild the encoder in place
            // (bounded) instead of killing an otherwise healthy session; a backend without an
            // in-place rebuild keeps today's fail-fast behavior.
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS
                || !reset_stalled_encoder(&mut enc, &mut inflight)
            {
                // Terminal: rebuilds are exhausted (or the backend can't rebuild in place). Say so
                // plainly with the underlying cause — the per-reset lines above only ever repeat
                // "rebuilt in place", so without this the session just vanishes. The error carries
                // its own actionable text now (e.g. an NVENC version mismatch → "update/reboot the
                // driver"), so this is the one line an operator needs.
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
            // Back off exponentially between rebuild attempts (100 ms → 1.6 s, ~3 s total across
            // the reset budget). One frame period is NOT enough: a 2026-07 field report showed all
            // 5 resets burning within 40 ms at 120 Hz against a driver-side condition (NVENC
            // session open failing after a codec switch) that no 8 ms retry could outlive — any
            // transient like the previous session's deferred driver teardown needs real time. A
            // genuinely dead encoder now costs ~3 s before the session ends with the terminal
            // error, which the client's stall UI already covers.
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
        // This frame's pacing deadline (the next frame's due time); the send thread spreads a big frame
        // up to here. Each in-flight frame carries its own (capture_ns, deadline) for when it's polled.
        // Frame-driven mode (T1.1) re-anchors to the ACTUAL submit — arrivals are the clock, and a
        // fixed `+= interval` grid would drift against them and squeeze the pacing budget; the
        // legacy tick keeps its fixed grid (with the catch-up reset in the tail). The rate-cap
        // charge lives under the same guard as the tail's gate: the legacy tick paces by its grid
        // alone, and charging it without ever accruing would bank unbounded debt that stalls the
        // loop if a rebuild later flips the capturer to arrival-wait.
        next = if frame_driven_enabled() && capturer.supports_arrival_wait() {
            pace.charge();
            std::time::Instant::now() + interval
        } else {
            next + interval
        };
        inflight.push_back((capture_ns, submit_ns, next));
        // Drain the OLDEST in-flight frames, keeping at most depth-1 deferred. At depth 1 this polls
        // immediately after every submit (synchronous); at depth 2 it polls N right after submitting N+1,
        // so the encode of N overlaps the convert/copy of N+1. NVENC's `pending` is FIFO, so poll() returns
        // the oldest submitted frame's AU — matching `inflight.pop_front()`.
        let mut send_gone = false;
        // A poll error is the explicit form of an encode stall (e.g. a QSV device failure);
        // carry it to the shared stall recovery below instead of killing the session outright.
        let mut poll_err: Option<anyhow::Error> = None;
        while inflight.len() >= depth {
            // Streamed chunked drain (§7 LN1 Phase 2): toward a STREAMED_AU client with the
            // encoder's chunked poll live, forward each slice chunk to the send thread the
            // moment it's readable, so packetize/FEC/pacing overlap the encode tail. Re-queried
            // per AU (never cached): a pipelined-retrieve escalation or a session rebuild turns
            // the mode off and the next AU falls back to the whole-AU path below.
            if streamed_wire && enc.supports_chunked_poll() {
                let t_wait = std::time::Instant::now();
                let mut first_chunk_us = 0u32;
                let mut au_flags = 0u32;
                let mut au_done = false;
                loop {
                    let c = match enc.poll_chunk() {
                        Ok(Some(c)) => c,
                        Ok(None) => break, // defensive: nothing in flight
                        Err(e) => {
                            poll_err = Some(e);
                            break;
                        }
                    };
                    // Every chunk proves the encoder is alive.
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
                            // The overlap measurement the Phase-3 gate needs (sampled): how
                            // early the first slice reached the send thread vs. the whole
                            // encode — the win is roughly their difference per AU.
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
                    continue; // drain the next in-flight frame, if depth allows
                }
                break; // defensive Ok(None): leave the frame in flight, re-poll next tick
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
                // No AU ready for a submitted frame. Routine on the non-blocking backends (the
                // libavcodec AMF/QSV wrapper holds ~2 frames; async NVENC drains a ready queue) —
                // the frame stays in flight and the next tick re-polls. The stall watchdog below
                // decides when "not ready yet" has become "the driver is wedged".
                Ok(None) => break,
                Err(e) => {
                    poll_err = Some(e);
                    break;
                }
            };
            // The encoder is alive: feed the stall watchdog, clear the consecutive-reset counter.
            last_au_at = std::time::Instant::now();
            encoder_resets = 0;
            let (cap_ns, sub_ns, deadline) = inflight.pop_front().expect("inflight non-empty");
            let mut flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            // Intra-refresh recovery marking (inert unless the backend validated its constrained GDR
            // via `intra_refresh_recovery`): tag every wave-boundary AU with USER_FLAG_RECOVERY_POINT
            // so the client lifts its post-loss freeze on the second mark — a proven clean re-anchor —
            // instead of forcing a full IDR. See [`mark_recovery_boundary`] for the cadence.
            let caps = enc.caps();
            if caps.intra_refresh_recovery
                && caps.intra_refresh_period > 0
                && mark_recovery_boundary(&mut ir_wave_pos, au.keyframe, caps.intra_refresh_period)
            {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
            }
            // Reference-frame-invalidation recovery frame (AMD LTR force-reference): a clean P-frame
            // off a known-good reference. Tag it so the client lifts its post-loss freeze on this one
            // AU without an IDR — the definitive single-frame re-anchor (see USER_FLAG_RECOVERY_ANCHOR).
            if au.recovery_anchor {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
            }
            // Datagram-aligned PyroWave AU (plan §4.4): the client windows its parse at the
            // shard payload and may opt into partial delivery of lossy frames.
            if au.chunk_aligned {
                flags |= punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED;
            }
            // Re-send the HDR mastering metadata (0xCE) on each keyframe (a decoder-resync point) and
            // whenever it changed, so a client that dropped the best-effort datagram re-converges.
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
            // Hand to the send thread; this blocks (backpressure) if it's behind. An Err means it
            // exited (send failure / stop) — end the encode loop too.
            bringup.mark("first_au"); // P0.1 (first-crossing only; free afterwards)
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
        // Encode-stall watchdog. Trip on: an explicit poll error; no AU within the window while
        // frames are owed (the full wedge — AMF/QSV's non-blocking poll returns None forever and
        // nothing else ever errors); or an owed backlog worth more than the window's frames (the
        // slow leak — AUs still trickle, so the gap never trips, but latency grows without bound).
        // Recovery rebuilds the encoder in place and forces an IDR — a logged ~one-second hiccup
        // instead of a silent permanent freeze — bounded so a genuinely dead encoder still ends
        // the session with a clear error. The window scales with the frame interval so low-fps
        // modes (where the AMF wrapper's ~2-frame hold spans seconds) can't false-trip.
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
        // Adaptive-depth escalate signal (measured BEFORE the trailing sleep): "behind" = the
        // frame's work overran its cadence deadline `next`, so the trailing sleep would be
        // zero/negative. At depth-1 that means the synchronous poll (encode + WDDM wait) can't
        // fit a frame interval — the contention case pipelining is for — so escalate, and hold
        // there. Leaky bucket + warmup skip reject one-off hitches and bring-up; no
        // de-escalation in v1. Two stages: first the CAPTURER's max depth (Windows IDD depth-2
        // overlap); where depth can't grow (Linux portal is permanently depth-1, §7 LN3), the
        // ENCODER's pipelined retrieve is the same trade on the other side of submit — the
        // two-thread lock moves the encode wait off this loop so capture/submit keep cadence,
        // at ~one tick of AU latency. `enc.set_pipelined` may decline (unsupported backend or
        // an explicit PUNKTFUNK_NVENC_ASYNC=0); either way it is asked exactly once.
        if idd_adaptive_enabled() {
            depth_frames += 1;
            if depth_frames > DEPTH_WARMUP_FRAMES {
                let behind = std::time::Instant::now() >= next;
                behind_score = if behind {
                    (behind_score + 1).min(DEPTH_BEHIND_CAP)
                } else {
                    behind_score.saturating_sub(1)
                };
                let escalated = cur_depth > 1 || pipelined_active || deescalating;
                // Export "encode can't hold cadence" for the control task's climb refusal.
                // An escalated session is held to a stricter standard — ANY net behind-frame
                // keeps it flagged, where an unescalated one is given the full bucket — because
                // its climb headroom really is partly spent and a climb would saw against the
                // escalation and starve the de-escalation clean run below.
                //
                // But being escalated cannot flag it BY ITSELF, which is what this used to do.
                // The client can't tell a transient refusal from an encoder's real ceiling: two
                // identical short acks latch a cap, so a session that escalated once — the
                // bucket needs ~20 net misses, which a startup hitch supplies while the ABR is
                // still in slow start at the 20 Mbps default — got pinned there, and stayed
                // pinned long after the escalation had bought back the headroom it was for.
                // Escalating exists precisely so cadence CAN be held; once it is (bucket
                // drained, every frame on time), refusing climbs is refusing the thing that
                // worked.
                cadence_degraded.store(
                    encode_behind_cadence(escalated, behind_score, DEPTH_DEGRADE),
                    Ordering::Relaxed,
                );
                if deescalating {
                    // A requested wind-back completes at the encoder's drained safe point —
                    // poll it (the call is a cheap latch check until then).
                    if !enc.set_pipelined(false) {
                        deescalating = false;
                        pipelined_active = false;
                        // Re-arm the ask: a future sustained overrun may escalate again (the
                        // backoff below paces how soon another wind-back may follow it).
                        pipeline_asked = false;
                        tracing::info!(
                            "encoder pipelined retrieve de-escalated — sync retrieve (and \
                             sub-frame streaming, where armed) restored; re-monitoring cadence"
                        );
                        // The wind-back rebuild's own stall must not re-escalate on the spot.
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
                    // Give the action time to take effect before judging again.
                    behind_score = 0;
                    ahead_run = 0;
                } else if escalated {
                    // De-escalation: a sustained every-frame-on-cadence run at the escalated
                    // setting is the evidence the contention passed (a lower ABR rate, the
                    // game scene lightened) — wind back in reverse order, paced by the
                    // exponential backoff (see the consts above).
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
            // T1.1 frame-driven trigger: instead of sleeping out the whole tick and then
            // SAMPLING (which holds a frame that arrived just after the previous sample for up
            // to a full interval — ~half on average), sleep only to the rate floor and then
            // wake on the capture's actual arrival. The 0.9×interval floor leaves per-gap jitter
            // headroom; the `pace` budget pins the long-run AVERAGE at the pacing rate — the
            // floor alone let a source that always has a frame pending (an HZ_MULT-overdriven
            // display under uncapped content) settle at 0.9-interval spacing, 1.11× the
            // negotiated rate on the wire, frames the client's panel can only drop. The
            // +0.5×interval keepalive keeps a static desktop re-encoding (bitrate shape,
            // client liveness) at 1.5×interval cadence and bounds control-servicing latency.
            //
            // Anchor the floor to THIS frame's arrival (`t_cap`), not to `next` — `next` is
            // re-based to the instant *after* submit(), so a synchronous encoder folds its whole
            // encode into the cadence: PyroWave's ~2 ms inline encode pushes the floor out by
            // that much, the loop misses the next arrival and samples one interval late, and the
            // period becomes interval + encode (≈158 fps off a 240 Hz source; 360 Hz → ~200).
            // An async encoder (NVENC) returns from submit in ≈0, so t_cap ≈ post-submit and this
            // is a no-op for it — which is why H.26x already holds full rate. Arrival-anchoring
            // lets the synchronous encode overlap the interval; the budget, not the floor, is
            // what bounds the sustained rate.
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
    // Drain the in-flight tail (the depth-1 frames submitted but not yet polled) so the last frames still
    // reach the client instead of being dropped on the way out.
    while let Some((cap_ns, sub_ns, deadline)) = inflight.pop_front() {
        let Ok(Some(au)) = enc.poll() else { break };
        let flags = if au.keyframe {
            (FLAG_PIC | FLAG_SOF) as u32
        } else {
            FLAG_PIC as u32
        };
        let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
        // End-of-stream tail drain: the per-stage split isn't measured here (the capture loop has
        // exited), so leave it zero — these last few frames are negligible for the aggregates.
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
    // Signal the send thread to drain + exit (drop the channel), then join it.
    drop(frame_tx);
    let _ = send_thread.join();
    tracing::info!(sent, "punktfunk/1 virtual stream complete");
    Ok(())
}

/// Whether a measured keyframe-recovery period is the CLIENT's jump-to-live cooldown rather
/// than anything happening on this host.
///
/// Every jump-to-live sends a keyframe request and is rate-limited to one per
/// [`punktfunk_core::client::FLUSH_COOLDOWN`], so a client that simply cannot sustain the
/// stream asks at exactly that spacing for as long as it stays behind. The recovery-cadence
/// detector reads perfect periodicity as evidence of a periodic *disturbance*, which is
/// backwards here: a fixed software cooldown is the most periodic thing in the system.
///
/// ±10 % — wide enough for scheduling jitter and the request's network trip, narrow enough that
/// it cannot swallow the disturbance cadences the other branch exists to report (display-mode
/// churn and descriptor polls run at their own, unrelated periods).
fn matches_client_flush_cadence(period: std::time::Duration) -> bool {
    let flush = punktfunk_core::client::FLUSH_COOLDOWN;
    period.abs_diff(flush) < flush / 10
}

/// One mode's capture/encode pipeline: (capturer, encoder, first frame, frame interval).
/// Dropping the capturer tears down the PipeWire stream and the virtual output with it.
type Pipeline = (
    Box<dyn crate::capture::Capturer>,
    Box<dyn crate::encode::Encoder>,
    crate::capture::CapturedFrame,
    std::time::Duration,
    // The virtual output's PipeWire node id — used by the B2 dedicated game-exit probe to check THIS
    // session's own node (scoped), not any gamescope node. `0` for backends without a PipeWire node
    // (Windows IDD-push), which never take the dedicated-gamescope B2 path anyway.
    u32,
    // The display's registry pool generation (Linux keep-alive pool only; `None` on Windows — the
    // manager leases in place — and for non-poolable outputs). A mode-switch rebuild uses it to
    // `registry::retire` the superseded old display, so linger/forever keep-alive policies don't
    // accumulate kept monitors at stale modes (design/midstream-resolution-resize.md H4).
    Option<u64>,
    // The bitrate the encoder was ACTUALLY opened at (kbps). Normally the one asked for; different
    // when an Automatic rate was re-resolved because the source delivers a size the session did not
    // negotiate (a monitor mirror). The caller adopts it so the ABR controller, the console sample
    // and every later `SetBitrate` resolve against what the encoder is really doing.
    u32,
);

/// The in-place resize fast path (latency plan P2.3, Windows IDD-push): the manager mode-sets the
/// SAME monitor in place (driver protocol v4 — `IOCTL_UPDATE_MODES`; internally falls back to
/// re-arrival against an older driver), then the existing capturer re-sizes its ring immediately
/// (no descriptor-poll debounce) and only the ENCODER is swapped once the first new-size frame
/// arrives — the capture pipeline, its send thread and the whole session transport survive.
/// Returns `true` when the stream is now delivering the new mode on the same capturer; `false`
/// routes the caller to the full rebuild (which is also the correct path when the manager had to
/// re-arrive a fresh monitor — this capturer's ring/broker are bound to the departed target).
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
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
    // Same-mode swap-chain recovery (the exclusive re-assert bounced the IDD's modes): recreate
    // the ring even though the size is unchanged — `resize_output`'s same-size fast path would
    // no-op exactly the case being recovered.
    recover_ring: bool,
) -> bool {
    let Some(cur_target) = capturer.capture_target_id() else {
        return false; // not an IDD-push capturer — nothing to reuse
    };
    // Acquire at the new mode: the manager's resize branch runs the in-place mode set (or its
    // re-arrival fallback) and returns a +1-ref lease, released again when `vout` drops below —
    // the capturer keeps holding its own original lease (`gen` is preserved by both paths).
    // In-place resize keeps the SAME display (no supersede — the manager resizes the live monitor).
    // Same display-rate multiplier the initial build applies, so a mid-stream resize doesn't
    // silently drop back to 1×.
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
        // The manager re-arrived a fresh monitor (old driver / in-place failure): this capturer is
        // bound to the departed target. The full rebuild re-acquires (JOINing the already-resized
        // monitor) with a fresh capturer.
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
    // Bounded wait for the first frame at the new size (the driver re-attaches to the fresh ring;
    // the mode-set full redraw composes promptly). Mirrors the capturer's own 3 s recover-or-drop.
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
    // Liveness gate for the eviction recovery: the driver re-delivers its STASH on re-attach, so
    // the first frame proves only the ring — not that the OS resumed presenting (measured: the
    // stash arrives in ~50 ms, then new_fps=0 forever). Require a SECOND, newer present — the
    // forced mode reset just triggered a full redraw, so a live display produces one promptly —
    // before declaring recovery; a stash-only re-attach must FAIL so the caller ends the session
    // cleanly (a reconnect's fresh bring-up always recovers) instead of streaming a frozen frame.
    let new_frame = if recover_ring {
        let first_pts = new_frame.pts_ns;
        let live_deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        loop {
            match capturer.try_latest() {
                Ok(Some(f)) if f.pts_ns != first_pts => break f,
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
    // Fresh encoder at the delivered size — the one component that can't follow a resolution
    // change in place today (P2.4 stays unimplemented: `open_video` is ms-scale, measured).
    let mut new_enc = match crate::encode::open_video(
        plan.codec,
        new_frame.format,
        new_frame.width,
        new_frame.height,
        effective_hz,
        bitrate_kbps as u64 * 1000,
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
    // Re-report the capturer's ring depth: in-place backends bound async pipelining by it, and a
    // rebuilt encoder starts with it unset.
    new_enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    *enc = new_enc;
    *frame = new_frame;
    *interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    trace.mark("encoder_open");
    true
}

/// The Welcome-time display-prep hand-off (latency plan P1.1/P1.2): the opened vdisplay backend +
/// the fully built pipeline — monitor create, activation, settle, capture attach, first frame,
/// encoder open — produced on the prep/stream thread while the client's Start round-trip and the
/// UDP hole-punch are still in flight, so the entire display bring-up hides behind the network
/// waits. Constructed on the Windows native path only today: the Linux backends bind launch
/// semantics before create (gamescope nests the launch command), which must not run for a client
/// that never sends Start.
pub(super) struct PreparedDisplay {
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) pipeline: Pipeline,
}

/// The prep thread's hand-off pair: the sender delivers the post-punch [`SessionContext`] to the
/// thread (which then runs [`virtual_stream`] on its prepared display); the join handle returns
/// the stream result. Dropping the sender un-received aborts the prep cleanly (the prepared
/// display's lease releases into keep-alive policy).
pub(super) type PrepHandle = (
    std::sync::mpsc::SyncSender<SessionContext>,
    std::thread::JoinHandle<Result<()>>,
);

/// Build the session's display + pipeline at Welcome time (latency plan P1.1/P1.2), before the
/// client's `Start` and the hole-punch — the negotiated mode is final once the Welcome is built,
/// and nothing in monitor create → activation → settle → capture attach → encoder open needs the
/// punched socket. Mirrors `virtual_stream`'s inline bring-up exactly: same backend setters, same
/// slot-scoped `begin_idd_setup` serialization (the guard releases when this returns), same
/// retry-wrapped build. The caller threads the SAME values the Welcome committed, so the prepared
/// pipeline and the later `SessionContext` can never disagree.
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
    // Passed through to [`build_pipeline`] — see its parameter of the same name.
    bitrate_auto: bool,
    bit_depth: u8,
    chroma: crate::encode::ChromaFormat,
    codec: crate::encode::Codec,
    shard_payload: u16,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
) -> Result<PreparedDisplay> {
    // Same plan resolution as `virtual_stream` (pure in these inputs + host config), including
    // PyroWave's datagram-aligned wire mode — `Session::shard_payload()` echoes the negotiated
    // Welcome value passed here.
    let mut plan = crate::session_plan::SessionPlan::resolve(
        bit_depth,
        chroma,
        codec,
        // Blend capability — must MATCH virtual_stream's resolve. Windows-only path, where
        // the rule is a constant `false` (the IDD capturer composites itself); passed through
        // the shared rule anyway so the two resolves cannot drift.
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
    vd.set_hdr(bit_depth >= 10);
    vd.set_hw_cursor(cursor_forward);
    vd.set_quit_flag(quit.clone());
    // Slot-scoped setup serialization + reconnect preempt — see the inline arm in
    // `virtual_stream` for the full rationale; released when this fn returns.
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
        plan,
        quit,
        stop,
        8,
        Some(trace),
    )?;
    Ok(PreparedDisplay { vd, pipeline })
}

/// Build the pipeline, retrying *transient* failures with bounded exponential backoff.
///
/// Bringing a virtual output to first-frame races several async steps — the compositor parenting
/// the output, the portal/RemoteDesktop grant, PipeWire format negotiation — any of which can
/// momentarily time out on a cold session. A single timed-out attempt shouldn't abort the whole
/// punktfunk/1 session. But a *permanent* failure (unsupported compositor/mode, a KWin too old to
/// create virtual outputs, a missing tool) must fail fast instead of burning the budget — so the
/// error chain is classified and permanent ones short-circuit. Each failed attempt drops its
/// capturer, which (via `PortalCapturer::Drop`) tears the PipeWire thread + virtual output down
/// before the next attempt — no leak across retries.
#[allow(clippy::too_many_arguments)]
fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    // Passed through to [`build_pipeline`] — see its parameter of the same name.
    bitrate_auto: bool,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    // Retry budget: 8 everywhere EXCEPT the capture-loss rebuild (2). That path wraps this call
    // in its own outer loop that RE-DETECTS the active session between calls — during a
    // Gaming↔Desktop switch the old compositor is simply gone, so burning 8 attempts (~13 s)
    // against its dead socket only delays following the box to the session that replaced it
    // (observed live: a Desktop→Gaming switch spent 13 of its 27 s retrying gone-KWin).
    max_attempts: u32,
    // Transition trace (P0.1): `Some` for the traced builds (bring-up, resize); each stage stamps
    // once (first crossing) so the retry loop can pass it through unconditionally.
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    // ~10s first-frame wait per attempt (attempt 1: see FIRST_ATTEMPT_FRAME_BUDGET below). 8
    // gives a ~80s budget for the SLOW case: a host-managed gamescope session cold-starting Steam
    // Big Picture (the SteamOS/Bazzite takeover) can take 30-60s to produce its first frame, and
    // a first-connect timeout would tear down the warm session (forcing another cold start on
    // reconnect). A genuinely permanent failure still fails fast via `is_permanent_build_error`;
    // only transient "no frame yet" retries consume the budget.
    // IDD-push only: HOLD one monitor lease across all build attempts. A failed attempt's capturer
    // drop releases ITS lease, but this held lease keeps the shared monitor Active (refs >= 1), so the
    // next attempt's `vd.create` JOINS it (refcount++) instead of finding it Lingering and tripping the
    // IDD-push reconnect PREEMPT (teardown + recreate). That preempt-per-retry was the REMOVE→ADD churn
    // that exhausts the IddCx monitor-slot pool and wedges ADD at 0x80070490 — one ADD per cold start
    // now, not one per attempt. Non-IDD-push backends (Linux portal, WGC) don't use the refcount manager
    // and aren't churn-wedge-prone, so they keep create-per-attempt (a held lease there would allocate a
    // second virtual output). Dropped when this fn returns — on success the Pipeline's own lease keeps
    // the monitor Active; on failure refs falls to 0 → Lingering → linger-timeout teardown.
    let _retry_hold = if matches!(plan.capture, crate::session_plan::CaptureBackend::IddPush) {
        Some(
            vd.create(mode)
                .context("acquire virtual output for the session (retry-hold lease)")?,
        )
    } else {
        None
    };
    // Attempt 1 waits only briefly for the first frame: a PipeWire stream connected while
    // gamescope re-initializes its headless takeover negotiates a format and reaches `Streaming`
    // but never receives a buffer — a FRESH connect then delivers within ~0.5 s (observed on
    // SteamOS: every gamescope bring-up burned the full 10 s on attempt 1, then attempt 2 got
    // frames instantly → 17 s bring-ups). Healthy compositors deliver the first frame well inside
    // this window (KWin ~0.3 s), and the genuinely-slow cold start above still gets the patient
    // 10 s window on every later attempt. The truncated attempt is PROVISIONAL end to end: its
    // expiry must not latch the capturer's sticky downgrades (see
    // `Capturer::next_frame_within_provisional`) — only the full-length attempts hand down
    // negotiation verdicts.
    const FIRST_ATTEMPT_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=max_attempts {
        // The client is gone (connection closed → `stop`): every further attempt only churns the
        // box for a session no one is watching — on a Bazzite takeover that means SIGKILLing and
        // relaunching the box's Steam session once per attempt for minutes (the .181 storm
        // 2026-07-07). One in-flight attempt can still overhang; this bounds the damage to it.
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
            plan,
            quit,
            None, // fresh bring-up — no display superseded
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

/// Is a pipeline-build error permanent (retrying won't help within this session)? Matches the
/// error chain against signatures that don't change between attempts: unsupported compositor or
/// mode, a KWin too old to expose virtual outputs, a missing/unparseable config, a tool that
/// isn't installed. Everything else — portal/PipeWire negotiation timeouts, "no frame within
/// 10s", transient node races — is treated as transient and retried. Biased toward "transient":
/// a misjudged permanent error only costs a few seconds before it fails anyway.
fn is_permanent_build_error(chain: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "virtual displays require linux",
        "unknown punktfunk_compositor",
        "could not detect compositor",
        "could not find output", // KWin < 6.5.6: createVirtualOutput unsupported
        "must be a node id",     // PUNKTFUNK_GAMESCOPE_NODE not an integer
        "is it installed",       // gamescope / kscreen-doctor not on PATH
        // 4:4:4 NVENC got a CUDA frame — should never happen now the Linux capturer honors gpu=false,
        // but fail fast instead of 8× retry (~90 s) rather than wedge the session if it ever recurs.
        "capture/encoder negotiation mismatch",
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// The mode the VIRTUAL DISPLAY is created at, which is the session's mode with its refresh rate
/// multiplied by `PUNKTFUNK_VDISPLAY_HZ_MULT` (default 1 — off, and then this is the identity).
///
/// The stream is NOT paced at this rate; [`pacing_hz`] clamps it straight back down. Overdriving
/// only the display buys freshness: a compositor paints on its own vblank, so at 1× a frame that
/// finished just after the capture sampled waits nearly a full interval to be picked up. Doubling
/// the display's rate halves that worst case without putting one extra frame on the wire.
///
/// Capped at the 0xffff the mode word packs into ([`crate::native::pack_mode`]) so an absurd
/// combination can't wrap; in practice the backend refuses long before that and reports what it
/// achieved instead.
fn display_mode_for(session: punktfunk_core::Mode) -> punktfunk_core::Mode {
    let mult = pf_host_config::config().vdisplay_hz_mult.max(1);
    punktfunk_core::Mode {
        refresh_hz: session.refresh_hz.saturating_mul(mult).min(0xffff),
        ..session
    }
}

/// The rate the stream is PACED and the encoder opened at: never above what the session
/// negotiated, and never above what the display actually achieved.
///
/// Two independent reasons the two differ. Downward: a backend can refuse the requested refresh
/// (KWin caps a virtual output at 60 Hz when the custom-mode install is rejected), and pacing
/// above the source would only emit phantom duplicates. Upward: [`display_mode_for`] deliberately
/// asked for a multiple of the session rate, and honoring that on the wire would send the client
/// frames it never negotiated.
fn pacing_hz(session_hz: u32, achieved_hz: u32) -> u32 {
    achieved_hz.min(session_hz).max(1)
}

/// Long-run rate limiter for the frame-driven trigger (T1.1): pins the AVERAGE encode rate at the
/// pacing rate while the 0.9×interval arrival floor keeps its jitter headroom.
///
/// The floor alone bounds only each gap, so a source that always has a frame pending — a display
/// overdriven by `PUNKTFUNK_VDISPLAY_HZ_MULT`, uncapped content — settles at 0.9-interval spacing:
/// 1.11× the negotiated rate on the wire (field report: 132 fps on a 120 fps session, frames the
/// client's 120 Hz panel can only drop). Credit accrues at one frame per interval of real elapsed
/// time (capped at [`Self::CAP`]) and every submitted frame spends one; a grab may run early only
/// against banked credit, so per-gap jitter still passes while the average cannot exceed the
/// pacing rate. A source at or below the pacing rate banks credit faster than it spends and is
/// never delayed.
struct PaceBudget {
    /// Banked frames, in `[-1.0, CAP]`. Transiently dips below 0 when a grab spent credit it had
    /// only partly banked; the owed fraction is repaid before the next grab.
    credit: f32,
    /// When credit last accrued (the previous [`Self::earliest`] call).
    last: std::time::Instant,
}

impl PaceBudget {
    /// Burst allowance: at most this many frames may follow a stall back-to-back before the
    /// bucket re-gates. One frame of instant catch-up plus the floor's own headroom.
    const CAP: f32 = 1.25;

    fn new(now: std::time::Instant) -> PaceBudget {
        PaceBudget {
            credit: Self::CAP,
            last: now,
        }
    }

    /// Accrue the elapsed credit and return the earliest instant the next grab may run: `now`
    /// once a full frame is banked, else the missing fraction of an interval out.
    fn earliest(
        &mut self,
        now: std::time::Instant,
        interval: std::time::Duration,
    ) -> std::time::Instant {
        let secs = interval.as_secs_f32();
        if secs > 0.0 {
            let accrued = now.duration_since(self.last).as_secs_f32() / secs;
            self.credit = (self.credit + accrued).min(Self::CAP);
        }
        self.last = now;
        now + interval.mul_f32((1.0 - self.credit).max(0.0))
    }

    /// One frame submitted — spend its credit.
    fn charge(&mut self) {
        self.credit -= 1.0;
    }
}

/// Does the encoder currently fail to hold the frame cadence? Exported to the control task, which
/// refuses bitrate CLIMBS while it is true (descents always pass — they are the cure).
///
/// `escalated` = the session has already spent an adaptive-depth / pipelined-retrieve step to buy
/// headroom; `behind_score` is the leaky bucket of frames whose work overran the cadence deadline.
/// An escalated session is judged strictly — ANY net behind-frame keeps it flagged — but being
/// escalated does not flag it on its own. That distinction is the whole point: the client cannot
/// tell a transient refusal from an encoder's hard ceiling (two identical short acks latch a cap),
/// so "escalated ⇒ degraded, permanently" pinned Automatic sessions at whatever rate they happened
/// to hold when a startup hitch escalated them — routinely the 20 Mbps default, while slow start
/// had barely begun. Escalation exists so cadence CAN be held; once it is, refusing climbs refuses
/// the thing that worked.
fn encode_behind_cadence(escalated: bool, behind_score: u32, degrade_at: u32) -> bool {
    behind_score >= degrade_at || (escalated && behind_score > 0)
}

/// Adopt the rate a freshly built pipeline's encoder was actually opened at.
///
/// The session's own `bitrate_kbps` is the number every later decision reads — the ABR controller's
/// climb base, the console's sample, what a `SetBitrate` ack is measured against — so letting it
/// disagree with the live encoder means each of those reasons about a stream that doesn't exist.
/// Silent when nothing changed, which is the overwhelmingly common case.
///
/// The client keeps its OWN copy of that number, and it used to move only on an ack — so a
/// rebuild that re-resolved an Automatic rate (`build_pipeline` does, whenever the source
/// delivers a size the session did not negotiate) left the two disagreeing for the rest of the
/// session. The ABR's next climb then computed from the stale base and asked for a rate BELOW
/// what the host was already sending: a re-target downward, paying an encoder rebuild to get
/// there. So tell the client too — `BitrateChanged` is the same message the `SetBitrate` path
/// answers with, and means the same thing arriving unprompted.
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
    let _ = retarget.send(built); // control task gone ⇒ the session is ending anyway
}

/// Encode-stall recovery: rebuild the encoder in place (keeping capture + the session up) and
/// discard the owed in-flight frame records — their AUs died with the old encoder instance.
/// Returns `false` when the backend has no in-place rebuild ([`crate::encode::Encoder::reset`]'s
/// default); the caller then surfaces the stall as a session error instead. The forced keyframe
/// makes the rebuilt encoder's first frame an immediate decoder resync point (belt-and-suspenders:
/// a fresh encoder opens on an IDR anyway).
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
    // The client asked for "Automatic", so `bitrate_kbps` is the host's own codec-aware answer for
    // `mode` — and may be re-resolved below when the source delivers a different size than `mode`.
    // An explicit client rate is left exactly as given.
    bitrate_auto: bool,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    // The pool gen of the display this build REPLACES (`Some` only on the mode-switch full
    // rebuild, which retires that gen once the new pipeline is up) — the registry lets the
    // replacement inherit group topology ownership instead of "extending" behind its dying
    // predecessor (the resize would silently demote a Primary/Exclusive virtual output).
    supersedes: Option<u64>,
    // First-frame wait override (`None` = the backend's default 10 s): the retry loop shortens
    // its FIRST attempt so a stream stuck in the gamescope takeover race fails over to the
    // reconnect that fixes it (see FIRST_ATTEMPT_FRAME_BUDGET in `build_pipeline_with_retry`).
    first_frame_budget: Option<std::time::Duration>,
    // Transition trace (P0.1): stamps the build's stages (display acquire, capture attach, first
    // frame, encoder open) into the bring-up/resize timeline. `None` on untraced rebuilds.
    trace: Option<&crate::bringup::Trace>,
) -> Result<Pipeline> {
    // Acquire through the registry (design/display-management.md): on Linux this pools the display
    // for keep-alive (reuse a kept one, or create + keep the backend's keepalive so it outlives the
    // session per policy); on Windows it delegates to `vd.create` (the manager already leases). The
    // returned `VirtualOutput`'s keepalive is a registry lease — the capturer holds it as before. The
    // `quit` flag rides into the lease so a deliberate-quit teardown skips the keep-alive linger.
    let display_mode = display_mode_for(mode);
    let vout = crate::vdisplay::registry::acquire(vd, display_mode, quit.clone(), supersedes)
        .context("create virtual output")?;
    if let Some(t) = trace {
        t.mark("display_acquired");
    }
    // A2: if this was a REUSED kept display and its first frame fails, tear the (dead) pool entry down
    // so the retry loop's next acquire creates fresh instead of re-wedging on the same corpse. Read the
    // gen BEFORE `capture_virtual_output` consumes `vout`. (Linux-only — the pool is Linux.)
    #[cfg(target_os = "linux")]
    let reused_gen = vout.reused_gen;
    // The display's pool generation (fresh AND reused), threaded out so a mode-switch rebuild can
    // `registry::retire` the display this pipeline supersedes (H4). `None` off Linux / non-poolable.
    #[cfg(target_os = "linux")]
    let pool_gen = vout.pool_gen;
    #[cfg(not(target_os = "linux"))]
    let pool_gen = None;
    // The virtual output's PipeWire node id — kept for the B2 dedicated game-exit probe (scoped to
    // this session's own node). Read before `capture_virtual_output` consumes `vout`.
    let node_id = vout.node_id;
    // The backend reports the refresh it actually achieved in `preferred_mode.2` (KWin may cap a
    // virtual output at 60 Hz if the custom-mode install was rejected). Falls back to the
    // requested rate when a backend reports nothing.
    let achieved_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(display_mode.refresh_hz);
    // A shortfall BELOW the session's own rate is the one that costs the client frames — warn.
    // Falling short of a multiplied ask while still meeting the session rate is the expected
    // outcome of an opt-in knob on a backend that won't overdrive, not a fault, so it only
    // informs. `pacing_hz` below keeps the session correct in both cases.
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
    // Pace the encoder + frame clock at the session's rate, floored by what the display achieved
    // — never above either.
    let effective_hz = pacing_hz(mode.refresh_hz, achieved_hz);
    // HDR vs SDR for the IDD-push conversion: a negotiated 10-bit session (client advertised
    // VIDEO_CAP_10BIT + host opted in via PUNKTFUNK_10BIT) is our HDR path → BT.2020 PQ Rgb10a2;
    // otherwise the FP16 IDD frames are converted to 8-bit SDR. (Ignored by non-IDD-push backends,
    // which auto-detect HDR from the monitor state.)
    //
    // KWin rewrites `SPA_META_Cursor` on every buffer, so its id-0 metas are an authoritative
    // "pointer hidden" the cursor blend/forward must honor — without this, the composited arrow
    // outlives every in-game/Big Picture hide (0.22.0 field report). Derived from the backend
    // (correct for pooled reuse too — a kept display only matches its own backend).
    let cursor_id0_hides = vd.name() == pf_vdisplay::Compositor::Kwin.id();
    let mut capturer = crate::capture::capture_virtual_output(
        vout,
        plan.output_format(),
        plan.capture,
        cursor_id0_hides,
    )
    .context("capture virtual output")?;
    // gamescope (Phase C): gamescope paints no `SPA_META_Cursor`, so hand the capturer a way to
    // reach gamescope's nested Xwaylands — it reads the pointer over X11 (XFixes shape +
    // QueryPointer position) and feeds `cursor()`, which the encode loop composites.
    // Non-gamescope plans skip this entirely.
    //
    // A PROVIDER, not the discovered list: gamescope creates the game's Xwayland when the game
    // launches and advertises only the FIRST in any child's environ, so a list captured here misses
    // it — and the cursor source would then blank the pointer for the whole game session (it asks
    // the connected display "are you drawing the pointer?" and gets "no"). The source re-runs this
    // every couple of seconds, so a stream that starts before the game converges, and a display
    // that dies is retried. Same one-way-edge shape as the Windows channel senders: the closure
    // wraps the host's discovery, and pf-capture never reaches back into pf-vdisplay.
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
        // Provisional: this is the retry loop's deliberately truncated first attempt, and its
        // expiry is the schedule firing, not a negotiation verdict — the capturer must not latch
        // its sticky process-wide downgrades (HDR capture, the dmabuf-only offers) from it. A
        // gamescope cold start regularly outlives this window and then accepts every offer on the
        // full-length attempt that follows (observed on .41: one truncated expiry pinned the whole
        // host process to SDR + CPU capture).
        Some(budget) => capturer.next_frame_within_provisional(budget),
        None => capturer.next_frame(),
    };
    let frame = match first.context("first frame") {
        Ok(f) => f,
        Err(e) => {
            // A reused kept display was dead — invalidate it so the next attempt creates fresh (A2).
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
    // A source may deliver a different frame size than the session negotiated, and the encoder below
    // is opened at the DELIVERED one. The case that matters is a monitor MIRROR: `MirrorDisplay`
    // ignores the requested mode by design (a physical head runs at the mode its owner set —
    // design/per-monitor-portal-capture.md §7.3), so a client that asked for 1080p and mirrors a 4K
    // panel encodes four times the pixels. An Automatic bitrate resolved from the *negotiated* mode
    // then hands the codec a quarter of the bits per pixel it was sized for, which arrives as a soft,
    // stuttering picture rather than as the mismatch it actually is. Re-resolve for what we will
    // really encode.
    //
    // Automatic ONLY. An explicit client rate is the operator's statement about their own link and is
    // never second-guessed — the same contract `resolve_bitrate_kbps_for` keeps everywhere else. The
    // refresh is left at the negotiated one so this changes exactly one input (the pixel count) to a
    // formula that is otherwise untouched.
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
    // `bit_depth` is the handshake-negotiated value (8, or 10 = HEVC Main10 when the client
    // advertised VIDEO_CAP_10BIT and the host opted in). Threaded down from the Welcome.
    let mut enc = crate::encode::open_video(
        plan.codec,
        frame.format,
        frame.width,
        frame.height,
        effective_hz,
        bitrate_kbps as u64 * 1000,
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
    // Tell in-place backends (Windows direct-NVENC) how deep they may pipeline against the
    // capturer's texture ring — without it they use only the env/pool cap and can encode a texture
    // the capturer has already rotated and overwritten.
    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    // Post-open cross-check: the Welcome already committed `chroma_format` from the pre-open probe, so
    // warn loudly if the encoder actually opened a different chroma than negotiated (the in-band SPS is
    // authoritative for the decoder, but a mismatch means the probe and the live open disagreed).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2026-08-13 field log's exact reading — `period_s=2.0` — must be attributed to the
    /// client's backlog shedding, not to a host display disturbance. The whole point of routing
    /// on the shared constant is that this stays true if the cooldown is ever retuned, so the
    /// test derives its cases from `FLUSH_COOLDOWN` instead of hardcoding two seconds.
    #[test]
    fn a_recovery_cadence_on_the_clients_cooldown_is_not_blamed_on_the_display() {
        let flush = punktfunk_core::client::FLUSH_COOLDOWN;
        assert!(matches_client_flush_cadence(flush), "the field reading");
        // Scheduling jitter and the request's trip across the link stay inside the band.
        assert!(matches_client_flush_cadence(flush + flush / 20));
        assert!(matches_client_flush_cadence(flush - flush / 20));

        // Cadences that are NOT the cooldown still reach the display-disturbance branch — the
        // band must not be so wide that it swallows them.
        assert!(!matches_client_flush_cadence(flush / 2));
        assert!(!matches_client_flush_cadence(flush * 2));
        assert!(!matches_client_flush_cadence(flush + flush / 5));
        assert!(!matches_client_flush_cadence(std::time::Duration::ZERO));
    }

    #[test]
    fn an_escalated_but_caught_up_encoder_stops_refusing_climbs() {
        const DEGRADE: u32 = 10;
        // Not escalated: the full bucket is allowed before climbs are refused.
        assert!(!encode_behind_cadence(false, 0, DEGRADE));
        assert!(!encode_behind_cadence(false, 9, DEGRADE));
        assert!(encode_behind_cadence(false, 10, DEGRADE));
        // Escalated and still missing deadlines: strict — one net behind-frame is enough.
        assert!(encode_behind_cadence(true, 1, DEGRADE));
        // Escalated, bucket fully drained: cadence is being HELD, which is what escalating was
        // for. This is the case that used to stay latched for the rest of the session and pin an
        // Automatic client at its slow-start rate.
        assert!(!encode_behind_cadence(true, 0, DEGRADE));
    }

    #[test]
    fn adopting_a_rebuilt_rate_tells_the_client() {
        let live = Arc::new(AtomicU32::new(20_000));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let mut current = 20_000;
        // The overwhelmingly common case: the rebuild landed on the same rate — silent.
        adopt_built_bitrate(&mut current, 20_000, &live, &tx);
        assert_eq!(rx.try_recv().ok(), None);
        // A re-resolve (the client asked 1080p, the source delivers a mirrored 4K panel): the
        // host's rate moves, so the client has to hear about it — its controller's climb base is
        // its own copy of this number, and a stale one makes the next "climb" a cut.
        adopt_built_bitrate(&mut current, 60_000, &live, &tx);
        assert_eq!(current, 60_000);
        assert_eq!(live.load(Ordering::Relaxed), 60_000);
        assert_eq!(rx.try_recv().ok(), Some(60_000));
    }

    #[test]
    fn pacing_never_exceeds_the_session_rate_or_the_display() {
        // Backend honored the request exactly (the multiplier off): pace at it.
        assert_eq!(pacing_hz(120, 120), 120);
        // Backend fell short (KWin capping a virtual output at 60): pace at what it gives,
        // or we emit phantom duplicates over a slower source.
        assert_eq!(pacing_hz(120, 60), 60);
        // Display overdriven by PUNKTFUNK_VDISPLAY_HZ_MULT: the extra composites buy freshness,
        // but the wire stays at the rate the client negotiated.
        assert_eq!(pacing_hz(60, 120), 60);
        assert_eq!(pacing_hz(120, 240), 120);
        // Overdriven AND short of the multiplied ask, but still at or above the session rate —
        // the session is unaffected.
        assert_eq!(pacing_hz(60, 90), 60);
        // Never zero: a 0 would divide into an infinite interval.
        assert_eq!(pacing_hz(60, 0), 1);
    }

    /// Drive [`PaceBudget`] against a source that ALWAYS has a frame pending (the overdriven
    /// display + uncapped content case): each cycle grabs the instant the gate opens, the next
    /// `earliest` call runs right after (encode folded into the wait, like the loop). Returns the
    /// grab instants.
    fn grab_saturated(
        b: &mut PaceBudget,
        start: std::time::Instant,
        interval: std::time::Duration,
        n: usize,
    ) -> Vec<std::time::Instant> {
        let mut now = start;
        let mut grabs = Vec::with_capacity(n);
        for _ in 0..n {
            let gate = b.earliest(now, interval);
            let grab = gate.max(now);
            b.charge();
            grabs.push(grab);
            now = grab;
        }
        grabs
    }

    #[test]
    fn pace_budget_pins_a_saturated_source_at_the_interval() {
        let interval = std::time::Duration::from_millis(10);
        let t0 = std::time::Instant::now();
        let mut b = PaceBudget::new(t0);
        let grabs = grab_saturated(&mut b, t0, interval, 120);
        // Whatever the initial credit bought, the total may exceed the on-rate schedule by at
        // most the burst cap — 120 grabs span no less than (120 - 1 - CAP) intervals.
        let span = grabs[119].duration_since(grabs[0]);
        assert!(
            span >= interval.mul_f32(120.0 - 1.0 - PaceBudget::CAP),
            "span {span:?} admits more than CAP frames of overshoot"
        );
        // And the steady state is EXACTLY the interval: past the warmup, consecutive grabs are
        // one interval apart (not 0.9 — the 132-fps bug).
        for w in grabs[20..].windows(2) {
            let gap = w[1].duration_since(w[0]);
            assert!(
                gap >= interval.mul_f32(0.999) && gap <= interval.mul_f32(1.001),
                "steady-state gap {gap:?} != interval {interval:?}"
            );
        }
    }

    #[test]
    fn pace_budget_never_delays_an_on_rate_or_slow_source() {
        let interval = std::time::Duration::from_millis(10);
        let t0 = std::time::Instant::now();
        let mut b = PaceBudget::new(t0);
        // A source at half the pacing rate (a 60 fps game on a 120 fps session): every arrival
        // banks two frames of credit and spends one — the gate is always already open.
        let mut now = t0;
        for _ in 0..50 {
            now += interval * 2;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "slow source must not be gated"
            );
            b.charge();
        }
        // Exactly on-rate: still never gated (credit hovers at the cap, never below 1).
        let mut b = PaceBudget::new(t0);
        let mut now = t0;
        for _ in 0..50 {
            now += interval;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "on-rate source must not be gated"
            );
            b.charge();
        }
    }

    #[test]
    fn pace_budget_burst_after_a_stall_is_capped() {
        let interval = std::time::Duration::from_millis(10);
        let t0 = std::time::Instant::now();
        let mut b = PaceBudget::new(t0);
        // Settle into the gated steady state, then stall the source for 10 intervals.
        let grabs = grab_saturated(&mut b, t0, interval, 20);
        let stall_end = grabs[19] + interval * 10;
        // However long the stall, the recovery may run ahead of the on-rate schedule by at most
        // CAP frames: the second post-stall grab is already re-gated.
        let after = grab_saturated(&mut b, stall_end, interval, 3);
        assert_eq!(after[0], stall_end, "first post-stall grab is immediate");
        assert!(
            after[1].duration_since(after[0]) >= interval.mul_f32(2.0 - PaceBudget::CAP),
            "second post-stall grab spent more than the burst cap"
        );
        assert!(
            after[2].duration_since(after[1]) >= interval.mul_f32(0.999),
            "third post-stall grab must be back on the interval grid"
        );
    }

    #[test]
    fn display_mode_multiplier_scales_only_the_refresh() {
        // Default (no env set in the test process) is 1× — the identity, which is what every
        // host that never touches the knob must keep getting.
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
        // gamescope ALWAYS rejects — a resize would respawn the nested game (H1/D3), regardless of
        // the identity policy.
        assert!(!reconfig_allowed(Some(Gamescope), false, false));
        assert!(!reconfig_allowed(Some(Gamescope), true, false));
        // A per-client-mode identity policy rejects on every backend — the resize resolves a
        // different display-identity slot (H5).
        assert!(!reconfig_allowed(Some(Kwin), true, false));
        assert!(!reconfig_allowed(Some(Mutter), true, false));
        assert!(!reconfig_allowed(None, true, false));
        // Every other compositor with the default identity ACCEPTS (recreate / re-arrival / in-place).
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false, false),
                "{c:?} should allow live reconfigure"
            );
        }
        // The synthetic source (no compositor) is the protocol-test path — always reconfigurable.
        assert!(reconfig_allowed(None, false, false));
    }

    /// A mirrored physical head has a fixed mode (§7.3): every backend that would otherwise accept
    /// a live reconfigure must reject one while the session is streaming someone's real monitor.
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
        // Frames 1..=3 are mid-wave (no mark), frame 4 is the boundary; then it repeats.
        let marks: Vec<bool> = (0..10)
            .map(|_| mark_recovery_boundary(&mut pos, false, period))
            .collect();
        assert_eq!(
            marks,
            vec![false, false, false, true, false, false, false, true, false, false]
        );

        // An IDR mid-wave re-phases: the counter restarts, so the next boundary is a full period
        // later (an IDR is itself a clean anchor, so it is not additionally marked).
        let mut pos = 0u32;
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 1
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 2
        assert!(!mark_recovery_boundary(&mut pos, true, period)); // IDR → pos 0, no mark
                                                                  // Now a fresh full period is needed, not just the 2 remaining frames.
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 1
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 2
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 3
        assert!(mark_recovery_boundary(&mut pos, false, period)); // pos 4 → mark
    }

    #[test]
    fn permanent_errors_short_circuit_retry() {
        // Permanent: config / version / missing-tool — retrying within a session can't fix these.
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Could not find output"
        ));
        assert!(is_permanent_build_error(
            "unknown PUNKTFUNK_COMPOSITOR 'foo' (kwin|wlroots|mutter|gamescope)"
        ));
        assert!(is_permanent_build_error(
            "spawn gamescope (is it installed? `apt install gamescope`)"
        ));
        assert!(is_permanent_build_error("virtual displays require Linux"));
        // Transient: negotiation/timeout races — exactly what backoff is for.
        assert!(!is_permanent_build_error(
            "first frame: no PipeWire frame within 10s (node 42): format negotiation never completed"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: timed out creating the KWin virtual output"
        ));
        assert!(!is_permanent_build_error("open NVENC: device busy"));
    }

    // ---- Phase-controller closed-loop simulation (design/phase-locked-capture.md §3, v3) ----
    //
    // Plants model what glass falsified, not what a controller would like:
    //  * GRID plant — v3's actuator: measured lead responds linearly to the grid offset
    //    (lead = (base − offset) mod P). Linear BY CONSTRUCTION of the absolute grid.
    //  * DECOUPLED plant — v2's on-glass failure: the measured lead ignores the actuation
    //    entirely (a saturated additive hold / a decoder pipeline re-anchoring the phase).
    //    The controller must give up (disengage), never orbit or chatter.
    // Reports are generated through the SHARED punktfunk_core::phase statistic.

    const SIM_P: i64 = 8_333_333; // 120 Hz
    /// The controller's ACTUAL target with the reports' 1 ms uncertainty:
    /// `max(TARGET_LEAD_FLOOR = 2.5 ms, uncertainty + 1 ms = 2 ms)` — the floor dominates.
    /// (The first harness draft claimed 3.5 ms from a mis-derived max; the controller locked
    /// at 2.5 exactly as coded and the assertions measured it against the wrong number.)
    const SIM_TARGET: i64 = 2_500_000;

    /// Deterministic LCG in ±spread_ns around zero (no OS randomness in tests).
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

    /// One simulated 1 Hz report: 120 lead samples folded through the SHARED circular
    /// statistic — the identical path the Android reporter ships.
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

    /// GRID plant readout: the lead the client would measure given the engaged offset.
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
        // base lead ≈ target + P/2: the initial error sits AT the antipode where its sign is a
        // coin flip — the exact 0↔2↔4 ms offset chatter measured on-glass 2026-07-31 midday.
        // Damped half-steps must carry it through; convergence within a bounded travel proves
        // no sign-flip oscillation.
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
        // The measured lead IGNORES the actuation (v2's saturated-hold regime, and any client
        // pipeline that re-anchors phase): the travel budget must trip, the grid must
        // DISENGAGE (zero cost — no residual sleeps), and stay out through the backoff.
        let mut c = PhaseController::new();
        let mut rng = Lcg(13);
        let mut engaged_at_some_point = false;
        for _ in 0..40 {
            // Pinned lead + tight noise: coherent, so the gate passes — only the budget saves us.
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
            let r = report_from_lead(7_500_000, SIM_P, &mut rng); // full-period smear
            c.adjust(&r, SIM_P);
        }
        assert!(
            !c.engaged(),
            "an incoherent phase must never engage the grid"
        );
    }

    #[test]
    fn regime_change_reengages_after_backoff() {
        // Decoupled → budget trip → backoff; then the plant becomes linear (regime change):
        // the controller must re-engage and lock.
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
        // The actuator itself: targets advance by exactly one period, and an offset change
        // moves the target by the same amount mod the period — the linearity the whole design
        // rests on (a per-frame additive hold has no such property once saturated).
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

    // ---- v4 flap hygiene (design/host-source-stutter-fixes.md WP-A1) ----
    //
    // These exercise the controller's reaction to the COHERENCE STATISTIC rather than the
    // statistic itself (`report_from_lead` above covers that), because the shape being replayed
    // is "coherence oscillating around the floor" — far easier to state directly than to conjure
    // out of a sample spread, and the number under test is the gate, not the estimator.

    /// Just above / just below the gate — the neighbourhood the field host lived in.
    const COHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI + 40;
    const INCOHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI - 40;
    /// A lead far enough from the target to be worth acting on (error ≫ deadband).
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

    /// One full cycle on a host that keeps losing its phase: engage on a sustained good phase,
    /// then lose it. Returns the backoff the disengage asked for, and leaves the controller with
    /// that backoff spent so the caller can run another cycle.
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

        // …and the proof is CONSECUTIVE: one bad report puts it back to zero, which is the whole
        // difference from v3 on a host that clears the gate every other second.
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
        // Parked means parked: even a v1-style bypass report (u16::MAX) cannot wake it.
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
        // Buy the minute by moving the epoch back, the same way the grid test places its epoch —
        // the lock's age IS the epoch's age.
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

    /// The 2026-08-15 Skynet log in one function: 24 minutes of a host whose arrival phase sits
    /// ON the coherence gate, flipping sides every few reports. v3 engaged on the FIRST coherent
    /// report after a zero-backoff disengage and logged **41 engage/disengage cycles** — 82
    /// self-inflicted timing steps (each engage holds submits by up to a period, each disengage
    /// drops the offset to zero) on a session whose transport was provably clean.
    #[test]
    fn flap_replay_stops_the_engage_churn() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(23);
        let (mut engagements, mut reports, mut coherent_side) = (0u32, 0u32, true);
        while reports < 24 * 60 {
            // Runs of 2-4 reports a side: "every few reports", the logged shape.
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
        // …and the cure must not be "never engage again": a host that settles still locks. This
        // is R1 in the plan's register, asserted rather than argued.
        assert!(
            !c.fused,
            "flapping that never engaged must not blow the fuse"
        );
        // The replay ends mid-backoff — a bad patch always does — so recovery costs that backoff
        // plus the engage proof, ~15 s at the report cadence. That delay IS the trade: every
        // second of it is spent in today's disengaged default, which adds no latency at all.
        for _ in 0..PhaseController::REENGAGE_BACKOFF + PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(
            c.engaged(),
            "a phase that finally holds must still get the grid"
        );
    }

    /// The 2026-08-14 Hyprland field report, in one function: xdph advertises `Hidden|Embedded`,
    /// so a session that asked for cursor metadata is served **Embedded** — no `SPA_META_Cursor`
    /// is ever sent, whatever the pointer does. The host must then (a) stop planning a metadata
    /// composite it can never feed, and (b) stop reading "no cursor overlay" as "the seat pointer
    /// is not on the streamed output" — the inference that re-centred the user's pointer once a
    /// second for the whole park cap.
    ///
    /// The `None` case is the regression guard for GNOME: Mutter is served the cursor mode it
    /// asks for through its own protocol and DOES suppress metadata while the pointer is off the
    /// recorded view, which is the signal `park_pointer` exists for. Nothing here may touch it.
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

        // The whole wlr family, today: metadata wanted, Embedded served.
        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Embedded)),
            &mut composite
        ));
        assert!(!composite, "the composite can never be fed — drop it");

        // Hidden is the same story from the other end: no pointer, so no overlay, so no signal.
        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Hidden)),
            &mut composite
        ));
        assert!(!composite);

        // A portal that really does serve metadata (xdph ≥ #366, or the portal path on
        // KWin/Mutter): everything stays exactly as it was.
        let mut composite = true;
        assert!(settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Metadata)),
            &mut composite
        ));
        assert!(composite);

        // Not portal-negotiated at all — KWin `zkde_screencast`, Mutter `RecordVirtual`,
        // gamescope, Windows. THE no-regression case.
        let mut composite = true;
        assert!(settle_portal_cursor(&Fake(None), &mut composite));
        assert!(composite);
    }
}
