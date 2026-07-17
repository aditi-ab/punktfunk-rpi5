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
            };
            let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t).into());
        }
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// Bounds a speed-test [`ProbeRequest`] before bursting: a 3 Gbps / 5 s ceiling keeps a probe from
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

/// Seal one access unit and send it with MICROBURST pacing (the shared
/// [`send_pacing`](crate::send_pacing) policy, native parameterization): the first `burst_cap`
/// bytes go out immediately (one absorbed burst the NIC / socket tx-buffer can swallow), and
/// only the OVERFLOW beyond that is spread across ~90% of the time to `deadline` in ADAPTIVE
/// chunks — 16 packets at today's rates, coarsening to at most 64 (the GSO-segment cap) once
/// the rate would otherwise skip every sub-floor sleep, so ≥1 Gbps frames still pace instead
/// of collapsing into an unpaced blast (plan Phase 1.2). `burst_cap` `None` = auto:
/// `max(128 KB, this AU's wire bytes / 4)`, so the burst stays a bounded fraction of a
/// high-rate frame instead of swallowing it whole (plan Phase 1.3); `Some` =
/// PUNKTFUNK_PACE_BURST_KB pinned an absolute cap. So a normal-bitrate frame (≤ cap) leaves in
/// one immediate burst at ~0 added latency, while a genuine IDR / sustained-high-bitrate frame
/// (≫ cap) still spreads — keeping the freeze fix exactly where it's needed (an unpaced
/// line-rate burst overruns the kernel tx buffer → EAGAIN drop → under infinite GOP, a freeze
/// until the next keyframe). With no slack (encode ≈ interval) the budget collapses to 0 and
/// even the overflow goes out immediately, so this is never slower than unpaced.
#[allow(clippy::too_many_arguments)]
fn paced_submit(
    session: &mut Session,
    data: &[u8],
    pts_ns: u64,
    flags: u32,
    frame_index: u32,
    deadline: std::time::Instant,
    burst_cap: Option<usize>,
) -> Result<PaceStat> {
    let wires = session
        .seal_frame_at(data, pts_ns, flags, frame_index)
        .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    let mut refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    // FEC/recovery test knob (PUNKTFUNK_VIDEO_DROP) — same knob the GameStream plane honors.
    crate::send_pacing::inject_video_drop(&mut refs);
    let wire_bytes: usize = refs.iter().map(|p| p.len()).sum();
    let cfg = crate::send_pacing::PaceCfg {
        burst_bytes: Some(burst_cap.unwrap_or_else(|| (wire_bytes / 4).max(128 * 1024))),
        chunk: crate::send_pacing::ChunkPolicy::Adaptive { base: 16, max: 64 },
        sleep_floor: std::time::Duration::from_micros(500),
    };
    // Time the socket handoff per chunk and fold it into the session's SealPerf split — the
    // sleeps between chunks stay excluded, so sock_ns is pure send_gso/sendmmsg time.
    let mut sock_ns = 0u64;
    let result = crate::send_pacing::pace_frame(
        &refs,
        crate::send_pacing::PaceBudget::UntilDeadline {
            deadline,
            fraction: 0.9,
        },
        &cfg,
        |chunk| {
            let t0 = std::time::Instant::now();
            let r = session.send_sealed(chunk).map(|_| ());
            sock_ns += t0.elapsed().as_nanos() as u64;
            r
        },
    );
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
}

/// Whether a session on `compositor` (`None` = the synthetic source) with a `per_client_mode`
/// identity policy may LIVE-reconfigure — accept a mid-stream `Reconfigure`
/// (design/midstream-resolution-resize.md H1/H5). Gated OFF for:
///   * **gamescope** (every sub-mode): a resize would respawn the nested game / restart the box's
///     game-mode session — it must never relaunch the title, so the client keeps scaling client-side.
///   * a **per-client-mode identity** policy: the mode is part of the display-identity slot key, so a
///     resize resolves a DIFFERENT slot (a fresh Windows monitor / a differently-named KWin output),
///     defeating the policy — honest downgrade is to reject and let the client scale.
///
/// Every other compositor (and the synthetic protocol-test source) with the default identity accepts.
pub(super) fn reconfig_allowed(
    compositor: Option<crate::vdisplay::Compositor>,
    per_client_mode: bool,
) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope) && !per_client_mode
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    mut session: Session,
    frame_rx: std::sync::mpsc::Receiver<FrameMsg>,
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    stop: Arc<AtomicBool>,
    perf: bool,
    burst_cap: Option<usize>,
    fec_target: Arc<AtomicU8>,
    stats: SendStats,
    // `Some` = the client advertised VIDEO_CAP_HOST_TIMING: emit one 0xCF datagram per AU right
    // after its last packet left the socket (capture→sent, the whole host pipeline incl. pacing).
    timing_conn: Option<quinn::Connection>,
    // The client advertised VIDEO_CAP_PROBE_SEQ — mid-session speed-test bursts may run in the
    // probe index space (else they're declined; see `run_probe_burst`).
    probe_seq: bool,
) {
    boost_thread_priority(false); // transmit thread: above-normal (Apollo's encoder-thread level)
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
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Probes run here (they need the Session); a burst pauses video — the encode thread blocks
        // on the full frame channel meanwhile, which is exactly the intended pause.
        service_probes(&mut session, &stop, &probe_rx, &probe_result_tx, probe_seq);
        // Adaptive FEC: pick up any new recovery target the control task set from client LossReports.
        apply_fec_target(&mut session, &fec_target);
        // Short timeout so we keep re-checking `stop` + probes when no frames are flowing.
        match frame_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(msg) => match paced_submit(
                &mut session,
                &msg.data,
                msg.capture_ns,
                msg.flags,
                msg.frame_index,
                msg.deadline,
                burst_cap,
            ) {
                Ok(stat) => {
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
                            };
                            let _ = tc.send_datagram(
                                punktfunk_core::quic::encode_host_timing_datagram(&t).into(),
                            );
                        }
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
            },
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
    /// The resolved compositor backend (moot on Windows — `vdisplay::open` ignores it there).
    pub(super) compositor: crate::vdisplay::Compositor,
    /// Negotiated encoder bitrate (kbps).
    pub(super) bitrate_kbps: u32,
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
    /// Adaptive-FEC target the control task updates from the client's loss reports.
    pub(super) fec_target: Arc<AtomicU8>,
    /// The QUIC control connection (carries host→client 0xCE source-HDR metadata mid-stream).
    pub(super) conn: quinn::Connection,
    /// `Some` when the client advertised [`punktfunk_core::quic::VIDEO_CAP_HOST_TIMING`]: the send
    /// thread emits one 0xCF datagram per AU (capture→sent µs) on it, so the client can split its
    /// `host+network` latency stage. `None` = older client, no emission.
    pub(super) timing_conn: Option<quinn::Connection>,
    /// The client advertised [`punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ`]: speed-test bursts may
    /// run mid-session in the probe index space (its reassembler keeps a separate probe window).
    /// `false` = older client whose single-window reassembler would drop probe-space frames as
    /// stale — mid-session probes are DECLINED for it (a zeroed [`ProbeResult`]) rather than
    /// consuming video frame indexes its gap detectors can't see (the phantom-gap freeze).
    pub(super) probe_seq: bool,
    /// Shared streaming-stats recorder. The capture loop reads `is_armed()` per frame to decide
    /// whether to measure the per-stage split; the send thread builds + pushes the aggregated
    /// `StatsSample` at its 2 s boundary.
    pub(super) stats: Arc<StatsRecorder>,
    /// Short client label (cert-fingerprint prefix, else peer IP) seeded into the capture meta on
    /// the first armed stats registration.
    pub(super) client_label: String,
    /// The session's requested launch, `None` = none. On Windows the store-qualified library id
    /// (spawned into the interactive user session once capture is live); on other hosts the shell
    /// command already resolved against the host's own library — nested into gamescope's bare spawn
    /// via `set_launch_command`, or spawned into the live session once capture is up.
    pub(super) launch: Option<String>,
    /// The client display's HDR colour volume (`Hello::display_hdr`; `None` = older client / SDR).
    /// Threaded into the vdisplay backend before `create` (→ the pf-vdisplay EDID's CTA HDR block,
    /// so host apps tone-map to the client's real panel) and preferred over the generic baseline
    /// for the 0xCE mastering metadata.
    pub(super) client_hdr: Option<punktfunk_core::quic::HdrMeta>,
}

pub(super) fn virtual_stream(ctx: SessionContext) -> Result<()> {
    // This thread runs the capture+encode loop (single-process — the only topology: Linux portal /
    // synthetic, Windows in-process IDD-push). Elevate it so a CPU-heavy game can't deschedule our GPU
    // submission.
    boost_thread_priority(true);
    // Resolve the per-session capture / topology / encoder decision ONCE (Goal-1 stage 3): the deployed
    // path now reads this typed `SessionPlan` instead of re-deriving from config at each dispatch site
    // (the latent "capture and encode disagree on the backend" hazard, plan §2.4). `bit_depth` is the
    // only per-session input — capture/topology/encoder are otherwise pure functions of `HostConfig`.
    let mut plan = crate::session_plan::SessionPlan::resolve(ctx.bit_depth, ctx.chroma, ctx.codec);
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
        compositor,
        mut bitrate_kbps,
        bitrate_auto,
        bit_depth,
        // The resolved chroma is already captured in `plan` (above); ignore the duplicate here.
        chroma: _,
        // Likewise the codec — `plan.codec` (resolved from `ctx.codec`) is the source of truth below.
        codec: _,
        probe_rx,
        probe_result_tx,
        reconfig_result_tx,
        fec_target,
        conn,
        timing_conn,
        probe_seq,
        stats,
        client_label,
        launch,
        client_hdr,
    } = ctx;
    tracing::info!(
        compositor = compositor.id(),
        ?mode,
        bitrate_kbps,
        bit_depth,
        "punktfunk/1 virtual display"
    );
    // Open the backend FIRST — on Windows this constructs the vdisplay backend, which initialises the
    // host-lifetime VirtualDisplayManager (§2.5). It does NO monitor work, so it must precede the IDD-push
    // preempt below (which reaches the manager) — otherwise `vdm()` is called before init and panics.
    let mut vd = crate::vdisplay::open(compositor)?;
    // Per-client STABLE monitor identity (Phase 2): hand the backend the connecting client's cert
    // fingerprint so a freshly CREATED virtual monitor gets this client's persistent id — Windows then
    // reapplies the client's saved per-monitor config (DPI scaling) on reconnect. No-op on Linux backends
    // and for anonymous/GameStream clients (no fingerprint → the driver auto-allocates).
    vd.set_client_identity(endpoint::peer_fingerprint(&conn));
    // The client display's HDR volume (Hello) → a freshly created virtual monitor's EDID CTA HDR
    // block (pf-vdisplay), so host apps + the OS tone-map to the client's real panel instead of the
    // driver's built-in ~1000-nit placeholder. No-op on Linux backends and for older/SDR clients.
    vd.set_client_hdr(client_hdr);
    // Deliberate-quit wiring (Windows pf-vdisplay; no-op elsewhere): every lease the backend mints —
    // the retry-hold below AND the capturer's — carries the session's quit flag, so a user "stop"
    // (⌘D → the QUIT close code) tears the virtual monitor down the moment the pipeline drops instead
    // of lingering 10 s. The reconnect then finds the manager Idle and does a clean fresh ADD (with
    // the user's think-time as driver settle) rather than the Lingering-preempt's REMOVE→ADD churn.
    // `keep_alive = forever` (gaming-rig) outranks the quit — the monitor pins as before.
    vd.set_quit_flag(quit.clone());
    // Per-session launch (non-Windows): hand the resolved command to the backend instance so
    // gamescope's bare spawn nests it — per-instance, no process-global env, so concurrent sessions
    // can't stomp each other's launch target. The other backends' default `set_launch_command` is a
    // no-op; they get the command spawned into the live session after capture is up (below).
    #[cfg(not(target_os = "windows"))]
    vd.set_launch_command(launch.clone());
    // IDD-push reconnect preempt (the dance now lives in the manager, Goal-1 §2.5): serialize setup so a
    // reconnect FLOOD can't run concurrent monitor create/teardown, STOP the prior session + WAIT for it
    // to release its monitor (instead of tearing a monitor out from under a still-live session), and
    // register THIS session's stop. The returned guard holds the setup lock across the pipeline build;
    // dropping it lets the next reconnect begin (and preempt us). Held BEFORE the monitor is created
    // (build_pipeline → vd.create), so the preempt still precedes this session's monitor creation.
    // SLOT-scoped (Stage W1): the preempt targets only a prior session holding THIS client's slot —
    // a different identity's session is an admission question, never a preempt.
    #[cfg(target_os = "windows")]
    let _idd_setup_guard =
        (plan.capture == crate::session_plan::CaptureBackend::IddPush).then(|| {
            let slot = crate::vdisplay::manager::slot_id_for(
                endpoint::peer_fingerprint(&conn),
                (mode.width, mode.height),
            );
            crate::vdisplay::manager::vdm().begin_idd_setup(slot, stop.clone())
        });
    let (mut capturer, mut enc, mut frame, mut interval, mut cur_node_id, mut cur_display_gen) =
        build_pipeline_with_retry(&mut vd, mode, bitrate_kbps, bit_depth, plan, &quit, &stop)?;
    // Setup done — release the IDD-push setup lock so the next reconnect can begin (and preempt us).
    #[cfg(target_os = "windows")]
    drop(_idd_setup_guard);

    // Capture is live — launch the requested title so it renders onto the streamed output and
    // grabs focus. Windows spawns the library id into the interactive user session; Linux spawns
    // the resolved command into the live session for every backend that didn't already nest it
    // (gamescope's bare spawn ran it inside the fresh gamescope — launching again would start it
    // twice). Best-effort: a launch failure (no recipe, launcher missing, no interactive user)
    // leaves the user on the streamed desktop/session, never tears the stream down. Launched ONCE
    // here — the mid-stream rebuild paths below must not re-spawn it.
    #[cfg(target_os = "windows")]
    if let Some(id) = launch.as_deref() {
        if let Err(e) = crate::library::launch_title(id) {
            tracing::warn!(launch_id = id, error = %e, "could not launch requested library title");
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(cmd) = launch.as_deref() {
        if crate::vdisplay::launch_is_nested(compositor) {
            tracing::info!(command = %cmd, "launch nested into the per-session gamescope");
        } else if let Err(e) = crate::library::launch_session_command(compositor, cmd) {
            tracing::warn!(command = %cmd, error = %e, "could not launch requested title into the session");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = &launch;

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
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<FrameMsg>(3);
    // Live encoder bitrate, shared with the send thread's stats sample: a mid-stream adaptive
    // bitrate change (bitrate_rx below) updates it so the console shows the actual target.
    let live_bitrate = Arc::new(AtomicU32::new(bitrate_kbps));
    // Live session mode, same pattern (H3): a mid-stream mode switch (reconfig below) updates it so
    // a stats capture armed after a resize registers the real mode. Seeded with the refresh the
    // initial build actually achieved (`interval_hz`), not the request — KWin may cap a virtual
    // output at 60 Hz.
    let live_mode = Arc::new(AtomicU64::new(pack_mode(
        mode.width,
        mode.height,
        interval_hz(interval),
    )));
    // One-shot force-keyframe flag driven by the management API (`POST /session/idr`, the web-console
    // Dashboard's "Request IDR" button) — drained in the encode loop below exactly like a client
    // decode-recovery request. Registered with `session_status` so the mgmt handler can reach THIS
    // session (the native plane never touches the GameStream `AppState.force_idr`).
    let force_idr = Arc::new(AtomicBool::new(false));
    // The send thread emits the web-console stats sample (it owns `session.stats()`); clone the
    // recorder so the capture loop keeps its own handle for the per-frame `is_armed()` gate.
    let send_stats = SendStats {
        rec: stats.clone(),
        mode: live_mode.clone(),
        codec: plan.codec.label(),
        client: client_label.clone(),
        bitrate_kbps: live_bitrate.clone(),
    };
    let send_thread = std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn({
            let stop = stop.clone();
            move || {
                send_loop(
                    session,
                    frame_rx,
                    probe_rx,
                    probe_result_tx,
                    stop,
                    perf,
                    burst_cap,
                    fec_target,
                    send_stats,
                    timing_conn,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    // Publish this session to the plane-neutral live-session registry so the web-console Dashboard
    // (`GET /status`) shows the native stream — resolution/fps/codec/bitrate resolve live from the
    // same handles a mid-stream mode switch / adaptive-bitrate change updates. The guard clears the
    // entry when this loop exits (return / `?` / panic), so the Dashboard tracks the session's life.
    let _live_session = crate::session_status::register(
        live_mode.clone(),
        live_bitrate.clone(),
        plan.codec,
        stop.clone(),
        force_idr.clone(),
        client_label,
        plan.hdr,
    );

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
    let mut diag_at = std::time::Instant::now();
    // Anchor for the forced-IDR cooldown (see the keyframe-request handling below): the timestamp of
    // the most recent forced/opening IDR. The session's pipeline just opened on an IDR, so start the
    // clock now — that coalesces the keyframe storm a client fires while its decoder wedges on the cold
    // opening GOP, instead of answering it with a redundant second IDR.
    let mut last_forced_idr: Option<std::time::Instant> = Some(std::time::Instant::now());
    // Self-diagnosis for the periodic-stutter class: warns when the served recovery IDRs settle
    // into a stable multi-second rhythm (see [`crate::metronome::Metronome`]).
    let mut recovery_cadence = crate::metronome::Metronome::new();
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
                crate::vdisplay::apply_input_env(sw.compositor, false);
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
                        let pipe = build_pipeline_with_retry(
                            &mut new_vd,
                            cur_mode,
                            bitrate_kbps,
                            bit_depth,
                            plan,
                            &quit,
                            &stop,
                        )?;
                        Ok((new_vd, pipe))
                    })();
                match rebuilt {
                    Ok((
                        new_vd,
                        (new_cap, new_enc, new_frame, new_interval, new_node_id, new_gen),
                    )) => {
                        // Replace the pipeline first (drops the old capturer → old PipeWire stream +
                        // virtual output), then the factory (drops e.g. the old KWin connection).
                        capturer = new_cap;
                        enc = new_enc;
                        frame = new_frame;
                        interval = new_interval;
                        cur_node_id = new_node_id;
                        cur_display_gen = new_gen;
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
            // PyroWave's Automatic bitrate is a per-mode ~1.6 bpp pin (resolve_bitrate_kbps_for) —
            // a resolution change moves the operating point (1080p→4K quadruples the pixel rate),
            // so re-resolve it for the new mode. Explicit client rates stay put (the operator knows
            // the link), and the H.26x codecs keep their mode-independent rate (ABR owns it).
            let mode_bitrate = if bitrate_auto && plan.codec == crate::encode::Codec::PyroWave {
                resolve_bitrate_kbps_for(plan.codec, 0, &new_mode)
            } else {
                bitrate_kbps
            };
            // Build the new pipeline BEFORE dropping the old one: the host already acked
            // the switch as accepted, so a rebuild failure must not kill an otherwise
            // healthy session — keep streaming the current mode and log instead.
            match build_pipeline(&mut vd, new_mode, mode_bitrate, bit_depth, plan, &quit) {
                Ok(next_pipe) => {
                    if mode_bitrate != bitrate_kbps {
                        tracing::info!(
                            from_kbps = bitrate_kbps,
                            to_kbps = mode_bitrate,
                            "pinned PyroWave bitrate re-resolved for the new mode"
                        );
                        bitrate_kbps = mode_bitrate;
                        live_bitrate.store(mode_bitrate, Ordering::Relaxed);
                    }
                    let old_display_gen = cur_display_gen;
                    // The destructuring assignment drops the OLD capturer (→ its display lease) as
                    // each binding is replaced — the new pipeline is already up (create-before-drop).
                    (capturer, enc, frame, interval, cur_node_id, cur_display_gen) = next_pipe;
                    cur_mode = new_mode;
                    next = std::time::Instant::now();
                    // H4: the old display's lease drop above is indistinguishable from a disconnect
                    // to the keep-alive machinery — under linger/forever policies every resize would
                    // ACCUMULATE kept monitors at stale modes. Retire the superseded entry now (a
                    // no-op when it was already torn down under `immediate`, or off Linux).
                    if let Some(g) = old_display_gen.filter(|g| cur_display_gen != Some(*g)) {
                        crate::vdisplay::registry::retire(g);
                    }
                    // H2/H3: the backend may have honored a different mode than requested — KWin
                    // caps a virtual output's refresh, or Windows pf-vdisplay rejects an in-place
                    // SetMode to a resolution its running monitor doesn't advertise and the host
                    // falls back to the actual display mode. `frame` is the NEW pipeline's first
                    // frame (just rebound above), so its dims are what the client actually decodes.
                    // Publish that ACTUAL mode to the live stats slot, and correct the client's mode
                    // slot when it differs from the accept ack it already got.
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
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), ?new_mode,
                        "mode-switch rebuild failed — staying on the current mode");
                    // H2 rollback: the control task acked the switch BEFORE this rebuild, so the
                    // client's mode slot already flipped to `new_mode`. A second accepted ack
                    // carrying the still-live mode corrects it (any accepted ack means "the active
                    // mode is now X" client-side; old clients just log it). `frame` is untouched
                    // here (the destructure only runs on the Ok arm), so it's still the OLD
                    // pipeline's frame — its real dims + interval are exactly what's still on glass.
                    let _ = reconfig_result_tx.send(Reconfigured {
                        accepted: true,
                        mode: delivered_mode(frame.width, frame.height, interval),
                    });
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
        if let Some(new_kbps) = want_kbps.filter(|&k| k != bitrate_kbps) {
            if enc.reconfigure_bitrate(new_kbps as u64 * 1000) {
                tracing::info!(
                    from_kbps = bitrate_kbps,
                    to_kbps = new_kbps,
                    "encoder bitrate reconfigured in place (adaptive bitrate — no IDR)"
                );
                bitrate_kbps = new_kbps;
                live_bitrate.store(new_kbps, Ordering::Relaxed);
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
                ) {
                    Ok(mut new_enc) => {
                        tracing::info!(
                            from_kbps = bitrate_kbps,
                            to_kbps = new_kbps,
                            "encoder rebuilt at new bitrate (adaptive bitrate)"
                        );
                        if let Some(c) = plan.wire_chunk {
                            new_enc.set_wire_chunking(c);
                        }
                        enc = new_enc;
                        bitrate_kbps = new_kbps;
                        live_bitrate.store(new_kbps, Ordering::Relaxed);
                        // The owed AUs died with the old encoder — same bookkeeping as a
                        // mode-switch rebuild; the fresh encoder opens on an IDR, so anchor the
                        // IDR cooldown too.
                        inflight.clear();
                        last_au_at = std::time::Instant::now();
                        encoder_resets = 0;
                        last_forced_idr = Some(std::time::Instant::now());
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
                    // The RFI recovered the loss with a clean re-anchor P-frame (no IDR). Anchor the
                    // keyframe cooldown so the client's echo of the SAME loss — its frames_dropped-
                    // driven keyframe request, arriving ~one loss-window later — is coalesced away
                    // instead of emitting a redundant full IDR right after the cheap recovery.
                    last_forced_idr = Some(std::time::Instant::now());
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
            let window = if enc.caps().intra_refresh {
                IDR_COOLDOWN_INTRA
            } else {
                IDR_COOLDOWN_FULL
            };
            let suppress = last_forced_idr.is_some_and(|t| t.elapsed() < window);
            if suppress {
                tracing::debug!("keyframe request coalesced — within the IDR cooldown");
            } else {
                tracing::debug!("forcing keyframe (client decode recovery)");
                enc.request_keyframe();
                let now = std::time::Instant::now();
                last_forced_idr = Some(now);
                if let Some(period) = recovery_cadence.note(now) {
                    tracing::warn!(
                        period_s = format!("{:.1}", period.as_secs_f64()),
                        "client keyframe recoveries are METRONOMIC — a periodic host/display \
                         disturbance (display-topology churn, display-poller software, \
                         virtual-display timing) is the likely cause, not random network loss; \
                         correlate with 'slow display-descriptor poll' / 'display descriptor \
                         changed' / 'IDD-push capture stall' lines"
                    );
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
                capture_rebuilds = 0; // a delivered frame clears the consecutive-loss counter
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
                #[cfg(target_os = "linux")]
                if launch.is_some()
                    && crate::vdisplay::launch_is_nested(compositor)
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
                let rebuild_deadline = std::time::Instant::now() + REBUILD_BUDGET;
                let (new_cap, new_enc, new_frame, new_interval, new_node_id, new_display_gen) = loop {
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
                            crate::vdisplay::apply_input_env(c, false);
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
                                    }
                                    Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                                        "capture loss: opening the newly-detected compositor failed — retrying"),
                                }
                            }
                        }
                    }
                    match build_pipeline_with_retry(
                        &mut vd,
                        cur_mode,
                        bitrate_kbps,
                        bit_depth,
                        plan,
                        &quit,
                        &stop,
                    ) {
                        Ok(p) => break p,
                        Err(e2) => {
                            if stop.load(Ordering::SeqCst)
                                || std::time::Instant::now() >= rebuild_deadline
                            {
                                return Err(e2)
                                    .context("capture lost — no compositor came up within the rebuild budget");
                            }
                            tracing::warn!(error = %format!("{e2:#}"),
                                "capture lost — new session not up yet, retrying");
                        }
                    }
                };
                capturer = new_cap;
                enc = new_enc;
                frame = new_frame;
                interval = new_interval;
                cur_node_id = new_node_id;
                cur_display_gen = new_display_gen;
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
        let depth = capturer.pipeline_depth().max(1);
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
        next += interval;
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
            if frame_tx.send(msg).is_err() {
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
        match next.checked_duration_since(std::time::Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = std::time::Instant::now(),
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
        if frame_tx.send(msg).is_err() {
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
);

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
fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
) -> Result<Pipeline> {
    // ~10s first-frame wait per attempt. 8 gives a ~90s budget for the SLOW case: a host-managed
    // gamescope session cold-starting Steam Big Picture (the SteamOS/Bazzite takeover) can take
    // 30-60s to produce its first frame, and a first-connect timeout would tear down the warm
    // session (forcing another cold start on reconnect). A genuinely permanent failure still fails
    // fast via `is_permanent_build_error`; only transient "no frame yet" retries consume the budget.
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
    const MAX_ATTEMPTS: u32 = 8;
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=MAX_ATTEMPTS {
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
        match build_pipeline(vd, mode, bitrate_kbps, bit_depth, plan, quit) {
            Ok(pipe) => {
                if attempt > 1 {
                    tracing::info!(attempt, "pipeline up after retry");
                }
                return Ok(pipe);
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let permanent = is_permanent_build_error(&chain);
                if permanent || attempt == MAX_ATTEMPTS {
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
                    max = MAX_ATTEMPTS,
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

fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
) -> Result<Pipeline> {
    // Acquire through the registry (design/display-management.md): on Linux this pools the display
    // for keep-alive (reuse a kept one, or create + keep the backend's keepalive so it outlives the
    // session per policy); on Windows it delegates to `vd.create` (the manager already leases). The
    // returned `VirtualOutput`'s keepalive is a registry lease — the capturer holds it as before. The
    // `quit` flag rides into the lease so a deliberate-quit teardown skips the keep-alive linger.
    let vout = crate::vdisplay::registry::acquire(vd, mode, quit.clone())
        .context("create virtual output")?;
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
    // virtual output at 60 Hz if the custom-mode install was rejected). Pace the encoder + frame
    // clock to that, not the requested rate, so we don't emit phantom duplicate frames over a
    // slower source. Falls back to the requested rate when a backend reports nothing.
    let effective_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(mode.refresh_hz);
    if effective_hz != mode.refresh_hz {
        tracing::warn!(
            requested = mode.refresh_hz,
            effective = effective_hz,
            "compositor did not honor the requested refresh — encoding at the achieved rate"
        );
    }
    // HDR vs SDR for the IDD-push conversion: a negotiated 10-bit session (client advertised
    // VIDEO_CAP_10BIT + host opted in via PUNKTFUNK_10BIT) is our HDR path → BT.2020 PQ Rgb10a2;
    // otherwise the FP16 IDD frames are converted to 8-bit SDR. (Ignored by non-IDD-push backends,
    // which auto-detect HDR from the monitor state.)
    let mut capturer =
        crate::capture::capture_virtual_output(vout, plan.output_format(), plan.capture)
            .context("capture virtual output")?;
    capturer.set_active(true);
    let frame = match capturer.next_frame().context("first frame") {
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
    )
    .context("open video encoder")?;
    if let Some(c) = plan.wire_chunk {
        enc.set_wire_chunking(c);
    }
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
    Ok((capturer, enc, frame, interval, node_id, pool_gen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconfig_allowed_gates_gamescope_and_per_client_mode() {
        use crate::vdisplay::Compositor::{Gamescope, Hyprland, Kwin, Mutter, Wlroots};
        // gamescope ALWAYS rejects — a resize would respawn the nested game (H1/D3), regardless of
        // the identity policy.
        assert!(!reconfig_allowed(Some(Gamescope), false));
        assert!(!reconfig_allowed(Some(Gamescope), true));
        // A per-client-mode identity policy rejects on every backend — the resize resolves a
        // different display-identity slot (H5).
        assert!(!reconfig_allowed(Some(Kwin), true));
        assert!(!reconfig_allowed(Some(Mutter), true));
        assert!(!reconfig_allowed(None, true));
        // Every other compositor with the default identity ACCEPTS (recreate / re-arrival / in-place).
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false),
                "{c:?} should allow live reconfigure"
            );
        }
        // The synthetic source (no compositor) is the protocol-test path — always reconfigurable.
        assert!(reconfig_allowed(None, false));
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
}
