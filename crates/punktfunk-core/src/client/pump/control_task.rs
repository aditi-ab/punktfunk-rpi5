//! Control task: the handshake stream stays open for mid-stream renegotiation + speed tests.
//! Outbound requests (mode switch, probe) and inbound replies (Reconfigured, ProbeResult) are
//! multiplexed with `select!`; a single outbound channel (`ctrl_rx`) keeps one writer so the
//! two `&mut ctrl_send` borrows don't collide across branches.

use super::super::*;
use super::*;

pub(super) struct ControlTask {
    pub(super) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(super) ctrl_send: quinn::SendStream,
    pub(super) ctrl_recv: io::MsgReader,
    /// `None` = no connect-time skew handshake (old host) — clock re-sync stays off.
    pub(super) clock_rtt_ns: Option<u64>,
    pub(super) mode_slot: Arc<Mutex<Mode>>,
    pub(super) probe: Arc<Mutex<ProbeState>>,
    /// The latest host `BitrateChanged` ack, drained by the pump's ABR on its report tick.
    pub(super) bitrate_ack: Arc<Mutex<Option<u32>>>,
    /// The live encoder-target mirror ([`NativeClient::current_bitrate_kbps`]): unlike the
    /// drain-once ack slot above, this one always holds the latest acked rate for stats HUDs.
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// Outbound decode-recovery KEYFRAME asks, counted here because this is the one choke point
    /// every emitter funnels through (embedder, `note_frame_index`, the pump's own asks) — the
    /// pump drains the count per report window as the ABR's recovery signal.
    pub(super) recovery_kf: Arc<AtomicU32>,
    pub(super) clock_offset: Arc<std::sync::atomic::AtomicI64>,
    pub(super) clock_gen: Arc<AtomicU32>,
    /// Clipboard metadata events (ClipState/ClipOffer) feed the same event plane the
    /// clipboard task uses for fetch data.
    pub(super) clip_event_tx: std::sync::mpsc::SyncSender<ClipEventCore>,
    /// Host cursor shapes ([`CursorShape`], sent on pointer-bitmap change) → the embedder's
    /// shape plane ([`NativeClient::next_cursor_shape`]).
    pub(super) cursor_shape_tx: std::sync::mpsc::SyncSender<crate::quic::CursorShape>,
    /// Bumped on every ACCEPTED mode switch (the `clock_gen` pattern): the pump watches it and
    /// resets the bitrate controller's mode-scoped learned state — the encoder ceiling / compute
    /// knee it was taught belong to the OLD mode.
    pub(super) mode_gen: Arc<AtomicU32>,
}

impl ControlTask {
    pub(super) async fn run(self) {
        let ControlTask {
            mut ctrl_rx,
            mut ctrl_send,
            mut ctrl_recv,
            clock_rtt_ns,
            mode_slot,
            probe,
            bitrate_ack,
            live_bitrate,
            recovery_kf,
            clock_offset,
            clock_gen,
            clip_event_tx,
            cursor_shape_tx,
            mode_gen,
        } = self;
        // Mid-stream clock re-sync (see [`ClockResync`]): a batch runs every
        // CLOCK_RESYNC_INTERVAL and whenever the pump asks (CtrlRequest::ClockResync after
        // its first no-op clock flush). Echoes interleave with the other control replies in
        // the read arm below; only when the host answered the connect-time handshake — an
        // old host would just eat the probes.
        let mut resync = ClockResync::new();
        let mut resync_guard = clock_rtt_ns.map(ResyncGuard::new);
        // Inter-round spacing: without it the whole 8-round batch completes inside one
        // ~6 ms video burst and every round samples the same congestion state — a batch
        // that starts mid-burst is then wholly congested and gets rejected. 7 ms staggers
        // the rounds across the ~16.7 ms frame cycle so the min-RTT round almost always
        // lands in a quiet inter-burst gap, even at PyroWave-class bitrates.
        const RESYNC_ROUND_SPACING: std::time::Duration = std::time::Duration::from_millis(7);
        let mut staged_round: Option<tokio::time::Instant> = None;
        let mut resync_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + CLOCK_RESYNC_INTERVAL,
            CLOCK_RESYNC_INTERVAL,
        );
        resync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                req = ctrl_rx.recv() => {
                    let Some(req) = req else { break }; // client dropped
                    let bytes = match req {
                        CtrlRequest::Mode(m) => Reconfigure { mode: m }.encode(),
                        CtrlRequest::Probe(p) => p.encode(),
                        CtrlRequest::Keyframe => {
                            recovery_kf.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            RequestKeyframe.encode()
                        }
                        CtrlRequest::Rfi(r) => r.encode(),
                        CtrlRequest::Loss(r) => r.encode(),
                        CtrlRequest::SetBitrate(k) => SetBitrate { bitrate_kbps: k }.encode(),
                        CtrlRequest::ClockResync => {
                            if clock_rtt_ns.is_none() {
                                continue; // no connect-time handshake — host can't answer
                            }
                            staged_round = None; // a new batch abandons any staged round
                            resync.begin(wall_clock_ns()).encode()
                        }
                        CtrlRequest::ClipControl(c) => c.encode(),
                        CtrlRequest::ClipOffer(o) => o.encode(),
                        CtrlRequest::CursorRender(m) => m.encode(),
                        CtrlRequest::Phase(p) => p.encode(),
                    };
                    if io::write_msg(&mut ctrl_send, &bytes).await.is_err() {
                        break;
                    }
                }
                _ = resync_tick.tick(), if clock_rtt_ns.is_some() => {
                    staged_round = None; // a new batch abandons any staged round
                    let probe = resync.begin(wall_clock_ns());
                    if io::write_msg(&mut ctrl_send, &probe.encode()).await.is_err() {
                        break;
                    }
                }
                _ = async { tokio::time::sleep_until(staged_round.unwrap()).await },
                        if staged_round.is_some() => {
                    staged_round = None;
                    // Stamped at send time so the inter-round spacing stays out of the RTT.
                    let probe = resync.next_probe(wall_clock_ns());
                    if io::write_msg(&mut ctrl_send, &probe.encode()).await.is_err() {
                        break;
                    }
                }
                msg = ctrl_recv.read_msg() => {
                    let Ok(msg) = msg else { break }; // stream closed
                    if let Ok(ack) = Reconfigured::decode(&msg) {
                        if ack.accepted {
                            *mode_slot.lock().unwrap() = ack.mode;
                            mode_gen.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(mode = ?ack.mode, "host accepted mode switch");
                        } else {
                            tracing::warn!(active = ?ack.mode, "host rejected mode switch");
                        }
                    } else if let Ok(result) = ProbeResult::decode(&msg) {
                        let mut p = probe.lock().unwrap();
                        // Freeze the delivered figures now (the burst is done). The mirrored
                        // counters are probe-scoped (stamped at the reassembler's FLAG_PROBE
                        // routing), so video around the burst inflates nothing; the client's
                        // first→last arrival interval is frozen with them — the denominator
                        // that measures when the bytes actually ARRIVED, not when the host
                        // stopped sending (its window closes while the bottleneck queue is
                        // still draining this way, which is how a 1 GbE link once "measured"
                        // 1266 Mbps).
                        let base_p = p.base_packets.unwrap_or(p.rx_packets_now);
                        let base_b = p.base_bytes.unwrap_or(p.rx_bytes_now);
                        p.delivered_packets = p.rx_packets_now.saturating_sub(base_p);
                        p.delivered_bytes = p.rx_bytes_now.saturating_sub(base_b);
                        p.client_interval_ms = ProbeState::measured_interval_ms(
                            p.first_arrival_ns,
                            p.last_arrival_ns,
                            p.delivered_packets,
                        )
                        .unwrap_or(0);
                        p.host_goodput_bytes = result.bytes_sent;
                        p.host_au = result.packets_sent;
                        p.host_wire_packets = result.wire_packets_sent;
                        p.host_send_dropped = result.send_dropped;
                        p.host_duration_ms = result.duration_ms;
                        p.done = true;
                        p.active = false; // burst over — the pump stops mirroring counters
                        tracing::info!(
                            host_goodput_bytes = result.bytes_sent,
                            wire_packets_sent = result.wire_packets_sent,
                            send_dropped = result.send_dropped,
                            duration_ms = result.duration_ms,
                            delivered_packets = p.delivered_packets,
                            client_interval_ms = p.client_interval_ms,
                            "speed-test probe result"
                        );
                    } else if let Ok(ack) = BitrateChanged::decode(&msg) {
                        // Adaptive bitrate: the host's clamp is authoritative — park it for
                        // the pump's controller (which also reads any ack as "this host
                        // renegotiates", arming further steps).
                        tracing::info!(
                            kbps = ack.bitrate_kbps,
                            "host re-targeted encoder bitrate"
                        );
                        // 0 would be a nonsense ack (the controller ignores it too) — don't
                        // let it wipe the HUD's live target.
                        if ack.bitrate_kbps > 0 {
                            live_bitrate.store(ack.bitrate_kbps, Ordering::Relaxed);
                        }
                        *bitrate_ack.lock().unwrap() = Some(ack.bitrate_kbps);
                    } else if let Ok(echo) = ClockEcho::decode(&msg) {
                        match resync.on_echo(&echo, wall_clock_ns()) {
                            ResyncStep::MoreRounds => {
                                staged_round = Some(
                                    tokio::time::Instant::now() + RESYNC_ROUND_SPACING,
                                );
                            }
                            ResyncStep::Done { offset_ns, rtt_ns } => {
                                let Some(guard) = resync_guard.as_mut() else {
                                    continue; // no connect handshake — batches never start
                                };
                                let (apply, best_of_streak) = match guard.admit(offset_ns, rtt_ns)
                                {
                                    ResyncAdmit::Fresh => (Some((offset_ns, rtt_ns)), false),
                                    ResyncAdmit::BestOfStreak { offset_ns, rtt_ns } => {
                                        (Some((offset_ns, rtt_ns)), true)
                                    }
                                    ResyncAdmit::Rejected { streak } => {
                                        // warn, not debug: repeated rejections are exactly the
                                        // stale-offset starvation signature the 2026-07
                                        // PyroWave-sawtooth report had to be diagnosed without.
                                        tracing::warn!(
                                            rtt_us = rtt_ns / 1000,
                                            floor_us = guard.floor_rtt_ns() / 1000,
                                            streak,
                                            "clock re-sync batch rejected — RTT above the \
                                             session floor (congested window)"
                                        );
                                        (None, false)
                                    }
                                };
                                if let Some((offset_ns, rtt_ns)) = apply {
                                    // info, not debug: ≤1/min, and it is THE forensic
                                    // trail for a stale-offset (stepped/slewed wall clock)
                                    // latency plateau — the 2026-07 two-pair investigation
                                    // had to reconstruct this blind.
                                    tracing::info!(
                                        offset_ns,
                                        rtt_us = rtt_ns / 1000,
                                        best_of_streak,
                                        "mid-stream clock re-sync applied"
                                    );
                                    clock_offset.store(offset_ns, Ordering::Relaxed);
                                    clock_gen.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            ResyncStep::Idle => {}
                        }
                    } else if let Ok(state) = ClipState::decode(&msg) {
                        // Host ack / policy / backend update for the toggle UI (try_send: a
                        // lagging embedder drops the newest — a stale toggle heals on the next).
                        let _ = clip_event_tx.try_send(ClipEventCore::State {
                            enabled: state.enabled,
                            policy: state.policy,
                            reason: state.reason,
                        });
                    } else if let Ok(offer) = ClipOffer::decode(&msg) {
                        // The host copied something: surface the lazy format list; the embedder
                        // fetches only if a local app pastes.
                        let _ = clip_event_tx.try_send(ClipEventCore::RemoteOffer {
                            seq: offer.seq,
                            kinds: offer.kinds,
                        });
                    } else if let Ok(chg) = crate::quic::ShardPayloadChanged::decode(&msg) {
                        // Mid-session shard renegotiation (design/shard-payload-reneg.md): the
                        // host re-keys the sealed video geometry. Per-frame pinning means there
                        // is nothing to re-key on the receive path — the reassembler follows
                        // each frame's own header and every buffer is statically sized for the
                        // ceiling — so the dispatch is validate + ack. The ack is telemetry for
                        // a shrink and the GATE for a grow (the host emits nothing above the
                        // old size until it lands). Validate against our own receive bounds —
                        // the same ceiling we advertised in `Hello::max_shard_payload` — and
                        // answer an out-of-bounds request with SILENCE, not an ack: a buggy
                        // host must never read a granted grow out of garbage.
                        let n = chg.shard_payload as usize;
                        if (crate::config::MIN_SHARD_PAYLOAD..=crate::config::max_shard_payload())
                            .contains(&n)
                            && n % 2 == 0
                        {
                            tracing::info!(
                                shard_payload = n,
                                "host re-keyed the wire shard payload — acking"
                            );
                            let ack = crate::quic::ShardPayloadAck {
                                shard_payload: chg.shard_payload,
                            };
                            if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                                break;
                            }
                        } else {
                            tracing::warn!(
                                shard_payload = n,
                                "out-of-bounds shard-payload change — ignoring (no ack)"
                            );
                        }
                    } else if let Ok(shape) = crate::quic::CursorShape::decode(&msg) {
                        // Pointer bitmap changed (cursor channel, only when negotiated). try_send:
                        // an overflowing ring drops the newest shape — the next change resends.
                        let _ = cursor_shape_tx.try_send(shape);
                    } else {
                        tracing::warn!(
                            tag = ?msg.first(),
                            len = msg.len(),
                            "unknown control message — ignoring"
                        );
                    }
                }
            }
        }
    }
}
