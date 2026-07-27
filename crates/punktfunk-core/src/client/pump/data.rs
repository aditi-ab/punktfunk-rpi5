//! The blocking data-plane pump: poll the session for access units, run the adaptive-FEC
//! loss reports, the ABR controller + startup capacity probe, the jump-to-live detectors,
//! and the standing-latency bleed, and hand frames to the embedder.

use super::super::*;
use super::*;

/// Data-plane pump on a blocking thread: poll the session, hand frames to the embedder.
/// try_send drops the newest frame when the embedder lags (freshness over completeness).
/// Speed-test filler ([`FLAG_PROBE`]) is folded into the probe accumulator instead of the
/// decoder queue — it isn't video.
pub(super) struct DataPump {
    pub(super) session: Session,
    pub(super) frames: Arc<FrameChannel>,
    pub(super) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    pub(super) shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub(super) probe: Arc<Mutex<ProbeState>>,
    pub(super) hot_tids: Arc<Mutex<Vec<i32>>>,
    pub(super) clock_offset: Arc<std::sync::atomic::AtomicI64>,
    pub(super) clock_gen: Arc<AtomicU32>,
    pub(super) decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Host encode-stage latency window accumulator (the ABR encode signal — see
    /// [`super::super::frame_channel::EncodeLatAcc`]); fed by the datagram task.
    pub(super) encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    /// Accepted-mode-switch generation (control task bumps): a change resets the controller's
    /// mode-scoped learned state ([`BitrateController::on_mode_switch`]).
    pub(super) mode_gen: Arc<AtomicU32>,
    pub(super) frames_dropped: Arc<std::sync::atomic::AtomicU64>,
    pub(super) fec_recovered: Arc<std::sync::atomic::AtomicU64>,
    pub(super) bitrate_ack: Arc<Mutex<Option<u32>>>,
    /// The embedder's REQUESTED rate (0 = Automatic — the only case the ABR arms).
    pub(super) bitrate_kbps: u32,
    /// The rate the host actually configured (echoed in Welcome).
    pub(super) resolved_bitrate_kbps: u32,
    pub(super) negotiated_codec: u8,
}

impl DataPump {
    pub(super) fn run(self) {
        let DataPump {
            mut session,
            frames,
            ctrl_tx,
            shutdown: pump_shutdown,
            probe: pump_probe,
            hot_tids: pump_hot_tids,
            clock_offset: pump_clock_offset,
            clock_gen: pump_clock_gen,
            decode_lat: pump_decode_lat,
            encode_lat: pump_encode_lat,
            mode_gen: pump_mode_gen,
            frames_dropped,
            fec_recovered,
            bitrate_ack,
            bitrate_kbps,
            resolved_bitrate_kbps,
            negotiated_codec,
        } = self;
        pin_thread_user_interactive(); // feeds the frame channel → the user-interactive video pump
        register_hot_tid(&pump_hot_tids); // this thread does UDP receive + FEC reassembly — hint it
                                          // Adaptive-FEC loss reporting: every ADAPT_REPORT_INTERVAL, report the loss observed over the
                                          // window (shards FEC recovered, plus a bump if any frame went unrecoverable) so the host can
                                          // size FEC to the link. Suppressed during a speed test (its FLAG_PROBE filler would skew it).
        const ADAPT_REPORT_INTERVAL: Duration = Duration::from_millis(750);
        let mut last_report = Instant::now();
        let (
            mut last_recovered,
            mut last_late,
            mut last_received,
            mut last_dropped,
            mut last_bytes,
        ) = (0u64, 0u64, 0u64, 0u64, 0u64);
        // PUNKTFUNK_PERF: per-window pump observability — the Session's receive stage split
        // (recv / decrypt / reassemble+FEC, see `Session::take_pump_perf`) and completed-AU
        // inter-arrival jitter. Smoothness has no metric otherwise: jump-to-live counters only
        // fire after the stream is already seconds behind.
        let pump_perf_on = std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0");
        let mut arrivals_us: Vec<u32> = Vec::new();
        let mut last_arrival: Option<Instant> = None;
        // Adaptive bitrate (see `crate::abr`): armed only when the embedder asked for Automatic
        // (`bitrate_kbps == 0`) and the host echoed the rate it actually configured (an old host
        // echoes 0 → controller stays permanently off). Fed once per report window with the same
        // deltas the LossReport uses, plus the window's mean skew-corrected one-way delay, the
        // actual delivered throughput (climb gate + proven-throughput mark), and whether a
        // jump-to-live flush fired.
        // PyroWave sessions PIN their rate (§4.6): AIMD descent turns wavelets to mush well
        // above its floor, and the climb probe's VBV reasoning doesn't apply to hard
        // per-frame CBR — controller and capacity probe stay off (0 = permanently off).
        let rate_pinned = negotiated_codec == crate::quic::CODEC_PYROWAVE;
        // All-intra streams have no reference chains: the frame channel drains to the newest
        // AU instead of strict FIFO (see `FrameChannel::set_all_intra`), so a slow consumer
        // caps its standing queue at ~1 frame with zero recovery cost.
        frames.set_all_intra(negotiated_codec == crate::quic::CODEC_PYROWAVE);
        let mut abr = BitrateController::new(if bitrate_kbps == 0 && !rate_pinned {
            resolved_bitrate_kbps
        } else {
            0
        });
        // Startup link-capacity probe (Automatic sessions): the controller's ceiling is the
        // negotiated start rate — the conservative 20 Mbps default, historically a box Automatic
        // could NEVER climb out of. One speed-test burst shortly after the stream settles
        // measures what the link actually delivers; ×0.7 (headroom for FEC overhead + variance)
        // becomes the climb ceiling and slow start does the rest. Old hosts decline (all-zero
        // reply) or never answer (timeout clears the state so LossReports resume) — either way
        // the ceiling stays negotiated, exactly the old behavior. PUNKTFUNK_ABR_PROBE=0 opts out.
        const CAPACITY_PROBE_KBPS: u32 = 2_000_000;
        const CAPACITY_PROBE_MS: u32 = 800;
        const CAPACITY_PROBE_DELAY: Duration = Duration::from_secs(2);
        const CAPACITY_PROBE_TIMEOUT: Duration = Duration::from_secs(6);
        let mut capacity_probe_at: Option<Instant> = (bitrate_kbps == 0
            && !rate_pinned
            && resolved_bitrate_kbps > 0
            && std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"))
        .then(|| Instant::now() + CAPACITY_PROBE_DELAY);
        let mut capacity_probe_deadline: Option<Instant> = None;
        // Edge detector + watchdog for a probe of EITHER origin (the startup capacity probe or an
        // embedder speed test via `NativeClient::request_probe`). The startup path had both built
        // in; the embedder path had neither, so an unanswered request wedged the report tick and a
        // finished one left the ABR window anchored before the burst.
        let mut was_probing = false;
        let mut probe_watchdog: Option<Instant> = None;
        let (mut owd_sum_ns, mut owd_frames) = (0i128, 0u32);
        let mut flush_in_window = false;
        // Jump-to-live state (see the guard in the loop below): when the clock-based over-bound
        // run began (`stale_since`, armed only when the skew handshake succeeded so the clocks
        // are comparable), when the clock-free non-draining-queue run began (`standing_since`),
        // and the last-jump instant for the shared cooldown. Wall-clock runs (T1.4), not frame
        // counts — the detectors' sensitivity must not scale with fps or repeat cadence.
        let mut stale_since: Option<Instant> = None;
        let mut standing_since: Option<Instant> = None;
        let mut last_flush: Option<Instant> = None;
        // Clock-detector health: consecutive clock-triggered flushes that found no local backlog
        // (see NOOP_FLUSH_DATAGRAMS). Reaching NOOP_CLOCK_FLUSHES_TO_DISARM turns the clock-based
        // detector off (a clock step / upstream queue it can't fix) — until a mid-stream clock
        // re-sync lands and re-arms it (`pump_clock_gen` below). The FIRST no-op flush also asks
        // the control task for an immediate re-sync (via the report tick): the flush finding no
        // local backlog IS the "the wall clock stepped under me" signal.
        let mut noop_clock_flushes: u32 = 0;
        let mut clock_detector_armed = true;
        let mut resync_wanted = false;
        let mut seen_clock_gen = pump_clock_gen.load(Ordering::Relaxed);
        let mut seen_mode_gen = pump_mode_gen.load(Ordering::Relaxed);
        // Standing-latency bleed (see StandingLatency): the third detector, for the small,
        // constant, loss-free OWD elevation the two jump-to-live detectors deliberately
        // tolerate (< QUEUE_HIGH frames, < FLUSH_LATENCY behind) — a sub-frame standing
        // backlog, or a stale clock offset after a wall-clock step, either of which otherwise
        // reads as permanent extra "network" latency for the rest of the session.
        let mut standing_lat = StandingLatency::new();
        while !pump_shutdown.load(Ordering::SeqCst) {
            // The live host↔client offset: re-loaded every iteration so an applied mid-stream
            // re-sync takes effect on the very next frame's latency math.
            let clock_offset_ns = pump_clock_offset.load(Ordering::Relaxed);
            // An applied re-sync invalidates the staleness run measured under the OLD offset:
            // reset the counters and re-arm the clock-based detector if a step had disarmed it.
            let gen = pump_clock_gen.load(Ordering::Relaxed);
            if gen != seen_clock_gen {
                seen_clock_gen = gen;
                stale_since = None;
                noop_clock_flushes = 0;
                // Every OWD reading shifted with the offset — the standing-latency floor and
                // any elevation measured under the old one are meaningless now. If a stale
                // offset WAS the elevation, this is also the moment it gets fixed.
                standing_lat.rebase();
                if !clock_detector_armed {
                    clock_detector_armed = true;
                    tracing::info!("clock re-sync applied — clock-based jump-to-live re-armed");
                }
            }
            // Mirror the reassembler's unrecoverable-drop count for the client's keyframe-recovery
            // loop, and (during a speed test) the packet-level receive counters for the throughput
            // measurement. Updated every iteration (not just on a produced frame) so they stay current
            // through a total-loss drought where no AU completes. Cheap: a few relaxed atomic loads.
            let st = session.stats();
            frames_dropped.store(st.frames_dropped, Ordering::Relaxed);
            fec_recovered.store(st.fec_recovered_shards, Ordering::Relaxed);
            let probe_active = {
                let mut p = pump_probe.lock().unwrap();
                if p.active && !p.done {
                    p.rx_packets_now = st.packets_received;
                    p.rx_bytes_now = st.bytes_received;
                    p.base_packets.get_or_insert(st.packets_received);
                    p.base_bytes.get_or_insert(st.bytes_received);
                }
                p.active && !p.done
            };
            // A probe just ended (either kind): rebase EVERY window anchor past the burst. Its
            // FLAG_PROBE filler landed in `bytes_received`/`packets_received` (session.rs counts
            // every accepted datagram) but never reached the decoder, and the report tick was
            // suppressed for the whole burst, so `last_*` still points before it. Without this the
            // first post-burst window reads the burst rate as `actual_kbps` and poisons the ABR's
            // monotone proven-throughput high-water mark — which never decays — and divides the
            // window's loss by a packet count inflated with filler.
            if was_probing && !probe_active {
                last_recovered = st.fec_recovered_shards;
                last_late = st.fec_late_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
                last_bytes = st.bytes_received;
                last_report = Instant::now();
            }
            // Arm a watchdog on the leading edge of ANY probe, so a host that silently ignores
            // `ProbeRequest` (an old build — anticipated, see the capacity-probe timeout below)
            // cannot latch `active` forever and suppress the report tick for the whole session.
            if !was_probing && probe_active {
                let burst = Duration::from_millis(pump_probe.lock().unwrap().duration_ms as u64);
                probe_watchdog = Some(Instant::now() + burst + CAPACITY_PROBE_TIMEOUT);
            }
            if !probe_active {
                probe_watchdog = None;
            } else if let Some(deadline) = probe_watchdog {
                if Instant::now() >= deadline {
                    probe_watchdog = None;
                    pump_probe.lock().unwrap().active = false;
                    tracing::warn!(
                        "speed-test probe unanswered — clearing it so loss reports and ABR resume"
                    );
                }
            }
            was_probing = probe_active;
            // Fire the startup link-capacity probe once the stream has settled (see the constants
            // above), and fold its measurement into the ABR ceiling when the result lands.
            // Never steal the slot from an embedder speed test in flight: there is one `ProbeState`
            // and no correlation id, so a clobber both wrecks the user's "Test connection" figure
            // (its base counters get re-snapshotted mid-burst against the full-burst denominator)
            // and mis-scales our own ceiling. Retry once it finishes.
            if capacity_probe_at.is_some_and(|at| Instant::now() >= at) && probe_active {
                capacity_probe_at = Some(Instant::now() + CAPACITY_PROBE_DELAY);
            } else if capacity_probe_at.is_some_and(|at| Instant::now() >= at) {
                capacity_probe_at = None;
                *pump_probe.lock().unwrap() = ProbeState {
                    active: true,
                    duration_ms: CAPACITY_PROBE_MS,
                    ..Default::default()
                };
                if ctrl_tx
                    .try_send(CtrlRequest::Probe(ProbeRequest {
                        target_kbps: CAPACITY_PROBE_KBPS,
                        duration_ms: CAPACITY_PROBE_MS,
                    }))
                    .is_ok()
                {
                    capacity_probe_deadline = Some(Instant::now() + CAPACITY_PROBE_TIMEOUT);
                    tracing::info!(
                        target_kbps = CAPACITY_PROBE_KBPS,
                        duration_ms = CAPACITY_PROBE_MS,
                        "adaptive bitrate: startup link-capacity probe"
                    );
                } else {
                    pump_probe.lock().unwrap().active = false; // ctrl queue full — skip
                }
            }
            if let Some(deadline) = capacity_probe_deadline {
                let mut p = pump_probe.lock().unwrap();
                if p.done {
                    capacity_probe_deadline = None;
                    // An all-zero reply is a decline (old host / probe-less build) — keep the
                    // negotiated ceiling. Otherwise: delivered wire kbps × 0.7.
                    if p.host_duration_ms > 0 && p.delivered_bytes > 0 {
                        let delivered_kbps = (p.delivered_bytes.saturating_mul(8)
                            / p.host_duration_ms.max(1) as u64)
                            as u32;
                        let ceiling = delivered_kbps.saturating_mul(7) / 10;
                        abr.set_ceiling(ceiling);
                        tracing::info!(
                            delivered_kbps,
                            ceiling_kbps = ceiling,
                            "adaptive bitrate: link-capacity probe done — climb ceiling set"
                        );
                    } else {
                        tracing::info!(
                            "adaptive bitrate: capacity probe declined — keeping negotiated ceiling"
                        );
                    }
                    // The probe's FLAG_PROBE filler landed in `bytes_received` but never reached
                    // the decoder — rebase the ABR window's byte counter past it, or the next
                    // window's "actual throughput" reads as the burst rate and poisons the
                    // controller's proven-throughput high-water mark with the LINK rate.
                    last_bytes = st.bytes_received;
                } else if Instant::now() >= deadline {
                    // The host never answered (a build that ignores ProbeRequest): clear the
                    // stuck-active state so LossReports resume, keep the negotiated ceiling.
                    p.active = false;
                    capacity_probe_deadline = None;
                    tracing::info!(
                        "adaptive bitrate: capacity probe timed out (old host?) — keeping negotiated ceiling"
                    );
                }
            }
            if !probe_active && last_report.elapsed() >= ADAPT_REPORT_INTERVAL {
                // A no-op clock flush earlier in this window suspected a wall-clock step: fire
                // the mid-stream re-sync now (once — the 60 s periodic covers everything else).
                if resync_wanted {
                    resync_wanted = false;
                    let _ = ctrl_tx.try_send(CtrlRequest::ClockResync);
                }
                // All-intra drain-to-newest skips are NOT losses (the wire delivered them) —
                // surface them at debug so a slow consumer is visible without alarming the
                // OSD loss counters.
                let skipped = frames.take_skipped();
                if skipped > 0 {
                    tracing::debug!(skipped, "all-intra frame channel drained to newest");
                }
                let window_dropped = st.frames_dropped.wrapping_sub(last_dropped);
                let loss_ppm = window_loss_ppm(
                    st.fec_recovered_shards.wrapping_sub(last_recovered),
                    st.fec_late_shards.wrapping_sub(last_late),
                    st.packets_received.wrapping_sub(last_received),
                    window_dropped,
                );
                let _ = ctrl_tx.try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                // Standing-latency bleed: close the detector's window with this report's loss
                // verdict and run its escalation ladder — re-sync first (free; a stale offset
                // from a stepped wall clock produces exactly this signature and the applied
                // re-sync rebases the floor), then a bounded flush+keyframe (drains a real
                // sub-threshold standing backlog the jump-to-live thresholds tolerate), then a
                // loud disarm (the path latency itself changed; nothing local fixes that).
                match standing_lat.on_window(loss_ppm == 0 && window_dropped == 0) {
                    StandingLatAction::None => {}
                    StandingLatAction::Resync { above_ms } => {
                        tracing::info!(
                            above_ms,
                            "standing latency above the session floor with zero loss — \
                             requesting a clock re-sync first (a stale offset reads exactly \
                             like this)"
                        );
                        let _ = ctrl_tx.try_send(CtrlRequest::ClockResync);
                    }
                    StandingLatAction::Bleed { above_ms } => {
                        // Shares the jump-to-live cooldown: an unexecuted bleed simply re-arms
                        // over the next windows (the detector's run rebuilds).
                        if last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN) {
                            last_flush = Some(Instant::now());
                            // Deliberately NOT `flush_in_window = true`: that flag is the ABR's
                            // SEVERE verdict (an immediate ×0.7 back-off), and the bleed fires
                            // only after ~6 provably loss-free windows with a sub-25ms elevation
                            // the controller itself scores as fine. The bleed's effect reaches
                            // the ABR through the window's own honest signals (OWD/loss/decode);
                            // the flag stays exclusive to the jump-to-live path below.
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                            standing_lat.bled();
                            tracing::warn!(
                                above_ms,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "standing latency survived a clock re-sync — bled the local \
                                 backlog (flush + keyframe)"
                            );
                        }
                    }
                    StandingLatAction::Disarm { above_ms } => {
                        tracing::warn!(
                            above_ms,
                            "standing latency persists after a re-sync and every bleed — not \
                             local, not clock; the path latency changed. Leaving it be \
                             (reconnect re-baselines)"
                        );
                    }
                }
                // Adaptive bitrate: an accepted mode switch first (it invalidates the
                // mode-scoped learned state), then drain any host ack (its clamp is
                // authoritative), then feed the controller this window's congestion signals; a
                // decision becomes a SetBitrate on the control stream.
                let mg = pump_mode_gen.load(Ordering::Relaxed);
                if mg != seen_mode_gen {
                    seen_mode_gen = mg;
                    abr.on_mode_switch();
                }
                if let Some(acked) = bitrate_ack.lock().unwrap().take() {
                    abr.on_ack(acked);
                }
                let owd_mean_us =
                    (owd_frames > 0).then(|| (owd_sum_ns / owd_frames as i128 / 1000) as i64);
                (owd_sum_ns, owd_frames) = (0, 0);
                // Drain the embedder's decode-latency window (always, so it stays bounded even when
                // the controller is disabled) → the mean feeds the decode signal; `None` when the
                // embedder reported nothing this window (old embedder / no decoded frames).
                let decode_mean_us = {
                    let mut acc = pump_decode_lat.lock().unwrap();
                    let (sum, count) = (acc.sum_us, acc.count);
                    *acc = DecodeLatAcc::default();
                    (count > 0).then(|| (sum / count as u64) as i64)
                };
                // Same drain for the host-encode window (0xCF `encode_us` via the datagram
                // task) — `None` on an old host that doesn't send stage timings.
                let encode_mean_us = {
                    let mut acc = pump_encode_lat.lock().unwrap();
                    let (sum, count) = (acc.sum_us, acc.count);
                    *acc = Default::default();
                    (count > 0).then(|| (sum / count as u64) as i64)
                };
                // The window's ACTUAL delivered throughput — what the pipeline really carried, vs
                // the target it was allowed. Wire bytes (headers + FEC) slightly overstate the
                // media rate the decoder ingests; acceptable for the climb gate / proven-mark
                // semantics (both compare against targets with their own headroom).
                let window_ms = last_report.elapsed().as_millis().max(1) as u64;
                let actual_kbps = (st.bytes_received.wrapping_sub(last_bytes).saturating_mul(8)
                    / window_ms) as u32;
                if let Some(kbps) = abr.on_window(
                    Instant::now(),
                    window_dropped,
                    loss_ppm,
                    owd_mean_us,
                    decode_mean_us,
                    encode_mean_us,
                    actual_kbps,
                    flush_in_window,
                ) {
                    // Log the window's signals alongside the decision so an on-glass session can
                    // tell a decode-/encode-driven re-target (the new signals — elevated with
                    // loss/OWD flat) from a network-driven one.
                    tracing::info!(
                        kbps,
                        loss_ppm,
                        owd_mean_us = owd_mean_us.unwrap_or(-1),
                        decode_mean_us = decode_mean_us.unwrap_or(-1),
                        encode_mean_us = encode_mean_us.unwrap_or(-1),
                        actual_kbps,
                        flushed = flush_in_window,
                        "adaptive bitrate: requesting encoder re-target"
                    );
                    let _ = ctrl_tx.try_send(CtrlRequest::SetBitrate(kbps));
                }
                flush_in_window = false;
                last_report = Instant::now();
                last_recovered = st.fec_recovered_shards;
                last_late = st.fec_late_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
                last_bytes = st.bytes_received;
                if pump_perf_on {
                    if let Some(p) = session.take_pump_perf() {
                        let per_pkt_ns = |ns: u64| ns.checked_div(p.packets).unwrap_or(0);
                        tracing::info!(
                            recv_ms = p.recv_ns / 1_000_000,
                            decrypt_ms = p.decrypt_ns / 1_000_000,
                            reasm_ms = p.reasm_ns / 1_000_000,
                            packets = p.packets,
                            batches = p.batches,
                            pkts_per_batch = p.packets.checked_div(p.batches).unwrap_or(0),
                            decrypt_ns_pkt = per_pkt_ns(p.decrypt_ns),
                            reasm_ns_pkt = per_pkt_ns(p.reasm_ns),
                            "pump stage split (window)"
                        );
                    }
                    // Inter-arrival jitter over the window's completed AUs. `late` counts gaps
                    // over 2× the window median — the "a frame arrived visibly off-beat" tally.
                    if arrivals_us.len() >= 8 {
                        arrivals_us.sort_unstable();
                        let pct = |q: usize| arrivals_us[(arrivals_us.len() - 1) * q / 100];
                        let (p50, p95) = (pct(50), pct(95));
                        let late = arrivals_us.iter().filter(|&&d| d > p50 * 2).count();
                        tracing::info!(
                            frames = arrivals_us.len() + 1,
                            arrival_p50_us = p50,
                            arrival_p95_us = p95,
                            arrival_max_us = arrivals_us.last().copied().unwrap_or(0),
                            late,
                            "frame inter-arrival jitter (window)"
                        );
                    }
                    arrivals_us.clear();
                }
            }
            match session.poll_frame() {
                Ok(frame) => {
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        continue; // speed-test filler, not video — measured via the counters above
                    }
                    if pump_perf_on {
                        let now = Instant::now();
                        if let Some(prev) = last_arrival.replace(now) {
                            // 4096 ≈ 17 s at 240 fps — a stuck window can't grow it unbounded.
                            if arrivals_us.len() < 4096 {
                                arrivals_us
                                    .push((now - prev).as_micros().min(u32::MAX as u128) as u32);
                            }
                        }
                    }
                    // Jump-to-live guard. A standing receive/hand-off queue never drains by itself —
                    // the pump consumes strictly in order at the arrival rate, so once behind, the
                    // stream stays behind for good (observed live: stuck 6–7 s). Pre-decode AUs are
                    // reference-chained (infinite GOP), so we can NOT drop a frame mid-stream to catch
                    // up; the only safe recovery is to discard the whole backlog and re-anchor decode
                    // on a fresh keyframe. Two independent "we're behind" signals arm it, both gated by
                    // FLUSH_COOLDOWN, both suspended during a speed test (the probe MEASURES a saturated
                    // queue; flushing would corrupt its counters):
                    //  * clock-based — completed frames sit > FLUSH_LATENCY behind the skew-corrected
                    //    capture clock continuously for FLUSH_AFTER. Needs the skew handshake, and
                    //    also catches kernel/reassembler backlog the hand-off queue hasn't reached yet.
                    //  * clock-free — the pre-decode hand-off queue stopped draining: its depth stayed
                    //    ≥ QUEUE_HIGH (never falling to QUEUE_LOW, still high at the trip) for
                    //    STANDING_TIME. Works with no handshake / a same-clock session (where the
                    //    clock path is disarmed), and is the direct signal that the embedder can't
                    //    keep up. A transient Wi-Fi clump drains within ~100 ms and never trips it.
                    if probe_active {
                        // Keep both detectors disarmed across a speed test so its (deliberately)
                        // saturated queue doesn't leave a primed run that fires the moment it ends.
                        stale_since = None;
                        standing_since = None;
                    } else {
                        let lat_ns = if clock_offset_ns != 0 {
                            now_realtime_ns() + clock_offset_ns as i128 - frame.pts_ns as i128
                        } else {
                            0
                        };
                        // Feed the adaptive-bitrate controller's OWD window (mean capture→received
                        // delay): rising delay under zero loss is queue growth — the pre-loss
                        // congestion signal. Only meaningful with a clock handshake.
                        if clock_offset_ns != 0 && lat_ns > 0 {
                            owd_sum_ns += lat_ns;
                            owd_frames += 1;
                            // The standing-latency detector rides the same signal, but off the
                            // window MINIMUM (robust against jitter/burst spikes — a standing
                            // state elevates the floor itself). Same 10 s plausibility clamp as
                            // the hn stats use.
                            if lat_ns < 10_000_000_000 {
                                standing_lat.note_frame(lat_ns);
                            }
                        }
                        if clock_detector_armed
                            && clock_offset_ns != 0
                            && lat_ns > FLUSH_LATENCY.as_nanos() as i128
                        {
                            stale_since.get_or_insert_with(Instant::now);
                        } else {
                            stale_since = None;
                        }
                        let depth = frames.depth();
                        if depth >= QUEUE_HIGH {
                            standing_since.get_or_insert_with(Instant::now);
                        } else if depth <= QUEUE_LOW {
                            standing_since = None;
                        }
                        // The queue trip additionally requires the depth to still be high NOW, so
                        // a run that started ≥ high but is hovering in the hysteresis band (a
                        // clump mid-drain) never fires on elapsed time alone.
                        let clock_behind = stale_since.is_some_and(|t| t.elapsed() >= FLUSH_AFTER);
                        let queue_behind = depth >= QUEUE_HIGH
                            && standing_since.is_some_and(|t| t.elapsed() >= STANDING_TIME);
                        if (clock_behind || queue_behind)
                            && last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN)
                        {
                            stale_since = None;
                            standing_since = None;
                            last_flush = Some(Instant::now());
                            flush_in_window = true; // strongest "link can't hold the rate" signal
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                            tracing::warn!(
                                behind_ms = if clock_behind { lat_ns / 1_000_000 } else { -1 },
                                queue_depth = depth,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "receive backlog stopped draining — jumped to live (flush + keyframe)"
                            );
                            // Clock-detector health check: a clock-only trigger whose flush found
                            // no local backlog is a false "behind" reading (a wall-clock step, or
                            // an upstream queue a local flush can't drain) — repeated, it would
                            // cost a recovery IDR every cooldown forever. Disarm after two in a
                            // row; the clock-free queue detector keeps covering real backlogs.
                            if clock_behind
                                && !queue_behind
                                && flushed < NOOP_FLUSH_DATAGRAMS
                                && dropped == 0
                            {
                                noop_clock_flushes += 1;
                                if noop_clock_flushes == 1 {
                                    // First no-op flush = a wall-clock step is the prime
                                    // suspect: ask for an immediate re-sync (sent on the next
                                    // report tick). Applied, it resets these counters and
                                    // re-arms the detector before the disarm below triggers.
                                    resync_wanted = true;
                                }
                                if noop_clock_flushes >= NOOP_CLOCK_FLUSHES_TO_DISARM {
                                    clock_detector_armed = false;
                                    tracing::warn!(
                                        "clock-based jump-to-live disarmed — its flushes found no \
                                         local backlog (clock step or upstream queueing suspected); \
                                         the queue-depth detector stays armed"
                                    );
                                }
                            } else {
                                noop_clock_flushes = 0;
                            }
                            continue; // this frame is part of the stale past — don't render it
                        }
                    }
                    frames.push(frame);
                }
                Err(PunktfunkError::NoFrame) => {
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(_) => break,
            }
        }
        // The pump exited (shutdown / fatal session error) — wake any consumer blocked in
        // `next_frame` with a Closed signal instead of a spurious timeout (the old mpsc did this
        // implicitly when the sender dropped).
        frames.close();
    }
}
