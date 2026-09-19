//! The send thread: sealed access units out of the encode loop and onto the wire.
//!
//! [`send_loop`] owns the socket side — chunking under `send_pacing`, the reconfig gate, and
//! the per-second [`SendStats`] line. The encode loop hands it [`FrameMsg`]s and never blocks
//! on a write.

use super::*;

/// One encoded AU handed to the send thread. Encode of N+1 overlaps transmit of N.
pub(super) struct FrameMsg {
    pub(super) data: Vec<u8>,
    pub(super) capture_ns: u64,
    pub(super) flags: u32,
    /// Predicted at submit as `au_seq + inflight`; stamped on the wire so RFI stays 1:1 across rebuilds.
    pub(super) frame_index: u32,
    /// Next frame's due time. Past = send immediately (catch up).
    pub(super) deadline: std::time::Instant,
    pub(super) encode_us: u32,
    /// Delivery→submit age (µs). 0 for repeats/tail. Wire pts anchors at the same delivery stamp.
    pub(super) queue_us: u32,
    /// `cap_us` = `try_latest`; `submit_us` = encode launch; `wait_us` = lock_bitstream.
    /// Synchronous backends (PyroWave) put the whole encode in `submit_us` — `wait_us` reads ~0.
    pub(super) cap_us: u32,
    pub(super) submit_us: u32,
    pub(super) wait_us: u32,
    pub(super) repeat: bool,
    /// Trust this, not a re-read of `is_armed()`: a capture that arms mid-flight must not fold
    /// zeroed splits into the first window's percentiles.
    pub(super) was_measured: bool,
    /// The Windows driver encoded this AU: `encode_us` is its present → arrival lump.
    pub(super) driver: bool,
    /// The driver stamped its slot, so `queue_us` is its pool wait, `encode_us` its encode and
    /// `ipc_us` the hand-off from publish to the host's take.
    pub(super) split: bool,
    pub(super) ipc_us: u32,
}

/// Whole AU, or one slice-boundary chunk of a streamed AU (seal/pace while the encoder still runs).
pub(super) enum SendMsg {
    Frame(FrameMsg),
    Chunk(ChunkMsg),
}

/// One encoder chunk of a streamed AU. AU-level fields match on every chunk; splits matter on `last`.
pub(super) struct ChunkMsg {
    pub(super) data: Vec<u8>,
    pub(super) first: bool,
    pub(super) last: bool,
    pub(super) capture_ns: u64,
    pub(super) flags: u32,
    pub(super) frame_index: u32,
    pub(super) deadline: std::time::Instant,
    pub(super) encode_us: u32,
    pub(super) queue_us: u32,
    pub(super) cap_us: u32,
    pub(super) submit_us: u32,
    pub(super) wait_us: u32,
    pub(super) repeat: bool,
    pub(super) was_measured: bool,
    pub(super) driver: bool,
    pub(super) split: bool,
    pub(super) ipc_us: u32,
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
            driver: c.driver,
            split: c.split,
            ipc_us: c.ipc_us,
        },
        PaceStat {
            spread_us: s.spread_us.saturating_add(stat.spread_us),
            paced: s.paced || stat.paced,
        },
    )))
}

/// Inputs the send thread needs for the 2 s web-console sample.
pub(super) struct SendStats {
    pub(super) rec: Arc<StatsRecorder>,
    /// Packed w:16|h:16|hz:16. Capture thread updates it on a mid-stream mode switch.
    pub(super) mode: Arc<AtomicU64>,
    pub(super) codec: &'static str,
    pub(super) client: String,
    pub(super) bitrate_kbps: Arc<AtomicU32>,
    pub(super) bringup: Arc<crate::bringup::Trace>,
    /// Data-socket clone for the kernel-queue probe behind the `wire egress` line.
    pub(super) wire_sock: Option<std::net::UdpSocket>,
    /// Frames the Windows driver dropped at its pool, session-cumulative. Written by the
    /// encode thread from the driver's telemetry; stays 0 off the driver.
    pub(super) driver_dropped: Arc<AtomicU64>,
    /// Sealed wire bytes go here each aggregation tick; the control task diffs them into the
    /// per-minute `link health` line's `egress_mbps`.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
}

/// Whether this session may accept a mid-stream `Reconfigure`.
///
/// Off for gamescope (a resize respawns the nested game), a per-client-mode identity (the mode
/// is part of the slot key, so a resize is a different display), and a `shared` display: a
/// monitor mirror (`design/per-monitor-portal-capture.md`) or a `mode_conflict: join` session.
/// Both stream a display whose mode belongs to someone else. The client scales.
pub(crate) fn reconfig_allowed(
    compositor: Option<crate::vdisplay::Compositor>,
    per_client_mode: bool,
    shared: bool,
) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope) && !per_client_mode && !shared
}

#[allow(clippy::too_many_arguments)]
pub(super) fn send_loop(
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
    timing_conn: Option<crate::native::link::SessionLink>,
    phase: Arc<PhaseCtl>,
    probe_seq: bool,
) {
    boost_thread_priority(false);
    // Idle tick: with no AU in hand the loop still revisits `stop`, the FEC target and the
    // 2 s stats window.
    const IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(50);
    // 3× default: the link carries 1× sustained, so a bounded 3× excursion is safe (WebRTC uses 2.5×).
    // `PUNKTFUNK_PACE_FACTOR=0` restores deadline-only spread.
    let pace_factor: f64 = std::env::var("PUNKTFUNK_PACE_FACTOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|f: &f64| f.is_finite() && *f >= 0.0)
        .unwrap_or(3.0);
    let mut last_perf = std::time::Instant::now();
    let mut last_bytes = 0u64;
    // The layer under this thread. Always on: a stall there leaves every other line clean.
    let mut wire = crate::net_health::WireProbe::new(stats.wire_sock);
    let mut last_wire = std::time::Instant::now();
    let (mut wire_sent, mut wire_dropped) = (0u64, 0u64);
    let mut last_send_dropped = 0u64;
    let mut encode_us: Vec<u32> = Vec::new();
    let mut pace_us: Vec<u32> = Vec::new();
    let (mut paced_frames, mut immediate_frames) = (0u64, 0u64);
    let mut sid: Option<(u64, u32)> = None;
    // Capture → fully sent per AU, and the driver path's present → arrival lump.
    let (mut host_v, mut driver_v): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
    let mut driver_path = false;
    // The driver's own split of that lump, once it stamps its slots.
    let (mut pool_v, mut denc_v, mut ipc_v): (Vec<u32>, Vec<u32>, Vec<u32>) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut driver_split = false;
    let mut last_driver_dropped = stats.driver_dropped.load(Ordering::Relaxed);
    let (mut cap_v, mut submit_v, mut wait_v, mut queue_v): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut new_frames, mut repeat_frames) = (0u64, 0u64);
    let mut streamed: Option<StreamedOpen> = None;
    let mut burst: Option<ProbeBurst> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Never mid-AU: a burst spliced between streamed chunks would push the tail past its deadline.
        if streamed.is_none() {
            service_burst(
                &mut session,
                &mut burst,
                &probe_rx,
                &probe_result_tx,
                probe_seq,
            );
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
        // Wake when the burst's next filler is due, so its rate holds while video shares the
        // loop. Mid-AU it cannot be pumped, so the idle tick stands.
        let wait = match burst.as_ref() {
            Some(b) if streamed.is_none() => b.next_due().min(IDLE_TICK),
            _ => IDLE_TICK,
        };
        match frame_rx.recv_timeout(wait) {
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
                        let probe = msg.flags & FLAG_PROBE as u32 != 0;
                        let host_us = (now_ns().saturating_sub(msg.capture_ns) / 1000)
                            .min(u32::MAX as u64) as u32;
                        // Stamp 0xCF now against the same capture anchor the wire pts carries.
                        if let Some(tc) = &timing_conn {
                            if !probe {
                                let t = punktfunk_core::quic::HostTiming {
                                    pts_ns: msg.capture_ns,
                                    host_us,
                                    // On the driver: queue = its pool wait, encode = its encode,
                                    // and the client's residual is hand-off + copy + seal.
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
                                    punktfunk_core::quic::encode_host_timing_datagram(&t),
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
                            if !probe {
                                host_v.push(host_us);
                            }
                            if msg.was_measured {
                                // The driver path's stages: its split when stamped, else its
                                // lump; then the copy and the send. A zero pool is unmeasured.
                                if msg.driver {
                                    driver_path = true;
                                    if msg.split {
                                        driver_split = true;
                                        if msg.queue_us > 0 {
                                            pool_v.push(msg.queue_us);
                                        }
                                        denc_v.push(msg.encode_us);
                                        ipc_v.push(msg.ipc_us);
                                    } else {
                                        driver_v.push(msg.encode_us);
                                    }
                                } else {
                                    cap_v.push(msg.cap_us);
                                    submit_v.push(msg.submit_us);
                                    if !msg.repeat {
                                        queue_v.push(msg.queue_us);
                                    }
                                }
                                wait_v.push(msg.wait_us);
                            }
                            if msg.repeat {
                                repeat_frames += 1;
                            } else {
                                new_frames += 1;
                            }
                            wire.sample();
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
        // The share window closes on a client delivery report, one per 750 ms
        // report window. A counter published on a 2 s clock gives that window
        // nothing twice and then 2 s of bytes, which reads as a path refusing
        // everything it was offered ([`crate::session_status::share_for`]).
        stats
            .counters
            .link
            .publish_egress_bytes(session.stats().bytes_sent);
        if last_wire.elapsed() >= std::time::Duration::from_secs(30) {
            let s = session.stats();
            let w = wire.window();
            tracing::info!(
                sent = s.packets_sent - wire_sent,
                send_dropped = s.packets_send_dropped - wire_dropped,
                outq_max_kb = w.outq_max_kb,
                tx_dropped = w.tx_dropped,
                tx_errors = w.tx_errors,
                carrier_changes = w.carrier_changes,
                udp_sndbuf_errors = w.udp_sndbuf_errors,
                iface = wire.iface.as_deref().unwrap_or("?"),
                "wire egress"
            );
            wire_sent = s.packets_sent;
            wire_dropped = s.packets_send_dropped;
            last_wire = std::time::Instant::now();
        }
        if last_perf.elapsed() >= std::time::Duration::from_secs(2) {
            let s = session.stats();
            let secs = last_perf.elapsed().as_secs_f64();
            let tx_mbps = (s.bytes_sent - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
            // One window of seal timing feeds both the perf line and the recorder. It runs only
            // while one of them reads it.
            let seal_perf = session.take_seal_perf();
            session.set_seal_perf(perf || stats.rec.is_armed());
            if perf {
                let sp = seal_perf.unwrap_or_default();
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
            let driver_dropped = stats.driver_dropped.load(Ordering::Relaxed);
            if stats.rec.is_armed() {
                let capture_gen = stats.rec.generation();
                let session_id = match sid {
                    Some((g, id)) if g == capture_gen => id,
                    _ => {
                        let (w, h, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                        let id = stats.rec.register_session(
                            "native",
                            w,
                            h,
                            hz,
                            stats.codec,
                            &stats.client,
                        );
                        sid = Some((capture_gen, id));
                        id
                    }
                };
                let stage = |name: &str, v: &mut Vec<u32>| crate::stats_recorder::StageTiming {
                    name: name.into(),
                    p50_us: percentile(v, 0.50) as f32,
                    p99_us: percentile(v, 0.99) as f32,
                };
                let stages = if driver_split {
                    vec![
                        stage("pool", &mut pool_v),
                        stage("encode", &mut denc_v),
                        stage("ipc", &mut ipc_v),
                        stage("copy", &mut wait_v),
                        stage("send", &mut pace_us),
                    ]
                } else if driver_path {
                    vec![
                        stage("driver", &mut driver_v),
                        stage("copy", &mut wait_v),
                        stage("send", &mut pace_us),
                    ]
                } else {
                    vec![
                        stage("queue", &mut queue_v),
                        stage("capture", &mut cap_v),
                        stage("submit", &mut submit_v),
                        stage("encode", &mut wait_v),
                        stage("send", &mut pace_us),
                    ]
                };
                let host = (!host_v.is_empty()).then(|| {
                    (
                        percentile(&mut host_v, 0.50) as f32,
                        percentile(&mut host_v, 0.99) as f32,
                    )
                });
                let (fec_us, seal_us, sock_us) = match seal_perf.filter(|p| p.frames > 0) {
                    Some(p) => {
                        let per_frame = |ns: u64| Some(ns as f32 / p.frames as f32 / 1000.0);
                        (
                            per_frame(p.fec_ns),
                            per_frame(p.seal_ns),
                            per_frame(p.sock_ns),
                        )
                    }
                    None => (None, None, None),
                };
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0,
                    session_id,
                    stages,
                    fec_us,
                    seal_us,
                    sock_us,
                    fps: (new_frames as f64 / secs) as f32,
                    repeat_fps: (repeat_frames as f64 / secs) as f32,
                    mbps: tx_mbps as f32,
                    bitrate_kbps: stats.bitrate_kbps.load(Ordering::Relaxed),
                    frames_dropped: driver_path
                        .then(|| driver_dropped.saturating_sub(last_driver_dropped) as u32),
                    packets_dropped: None,
                    send_dropped: Some(
                        s.packets_send_dropped.saturating_sub(last_send_dropped) as u32
                    ),
                    fec_recovered: None,
                    host_p50_us: host.map(|h| h.0),
                    host_p99_us: host.map(|h| h.1),
                    rtt_us: timing_conn
                        .as_ref()
                        .map(|c| c.rtt().as_micros().min(u128::from(u32::MAX)) as u32),
                };
                stats.rec.push_sample(session_id, sample);
            }
            last_driver_dropped = driver_dropped;
            host_v.clear();
            driver_v.clear();
            driver_path = false;
            pool_v.clear();
            denc_v.clear();
            ipc_v.clear();
            driver_split = false;
            last_perf = std::time::Instant::now();
            last_bytes = s.bytes_sent;
            last_send_dropped = s.packets_send_dropped;
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
    // Stop, teardown, or a dead channel mid-burst: report what went out, leave nothing armed.
    if let Some(b) = burst {
        let _ = probe_result_tx.send(b.finish());
    }
}
