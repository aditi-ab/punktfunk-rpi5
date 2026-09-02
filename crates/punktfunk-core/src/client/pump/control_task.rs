//! Control task: the handshake stream stays open for mid-stream
//! renegotiation and speed tests. Outbound requests and inbound replies
//! multiplex on `select!`; one `ctrl_rx` writer so the two `&mut ctrl_send`
//! borrows never collide across branches.

use super::super::*;
use super::*;

pub(super) struct ControlTask {
    pub(super) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(super) ctrl_send: quinn::SendStream,
    pub(super) ctrl_recv: io::MsgReader,
    /// `None` = no connect-time skew handshake (old host); clock re-sync stays off.
    pub(super) clock_rtt_ns: Option<u64>,
    pub(super) mode_slot: Arc<Mutex<Mode>>,
    pub(super) probe: Arc<Mutex<ProbeState>>,
    /// Latest host `BitrateChanged` ack; the pump ABR drains it on the report tick.
    pub(super) bitrate_ack: Arc<Mutex<std::collections::VecDeque<u32>>>,
    /// Live encoder-target ([`NativeClient::current_bitrate_kbps`]). Unlike the
    /// drain-once ack above, this always holds the latest acked rate for HUDs.
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// Outbound KEYFRAME asks. Counted here: every emitter (embedder,
    /// `note_frame_index`, pump) funnels through this choke point. The pump
    /// drains the count per report window as the ABR recovery signal.
    pub(super) recovery_kf: Arc<AtomicU32>,
    /// Last host pipeline gap in ms ([`crate::quic::PipelineGap`]); `0` = none.
    /// Pump drains it and discards the in-flight report window — a host-local
    /// rebuild is not congestion. Atomic because the pump only ever swaps it.
    pub(super) pipeline_gap: Arc<AtomicU32>,
    pub(super) clock_offset: Arc<std::sync::atomic::AtomicI64>,
    pub(super) clock_gen: Arc<AtomicU32>,
    /// ClipState/ClipOffer share the fetch-data event plane.
    pub(super) clip_event_tx: std::sync::mpsc::SyncSender<ClipEventCore>,
    /// Host [`CursorShape`] → [`NativeClient::next_cursor_shape`].
    pub(super) cursor_shape_tx: std::sync::mpsc::SyncSender<crate::quic::CursorShape>,
    /// Bumped on every ACCEPTED mode switch (`clock_gen` pattern). The pump
    /// resets bitrate-controller state that belonged to the old mode.
    pub(super) mode_gen: Arc<AtomicU32>,
    /// Live access grants ([`NativeClient::access_grants`]). Every inbound
    /// [`AccessUpdate`] overwrites this BEFORE the event is forwarded, so a
    /// reader woken by the event never sees the pre-update mask.
    pub(super) access_grants: Arc<AtomicU32>,
    /// Live access deadline (client unix seconds; `0` = permanent). Re-anchored
    /// from each `AccessUpdate`'s relative `remaining_secs`.
    pub(super) access_deadline_unix: Arc<std::sync::atomic::AtomicU64>,
    /// Access updates → [`NativeClient::next_access_update`]. try_send: a
    /// lagging embedder drops the oldest; the two live slots already hold truth.
    pub(super) access_tx: std::sync::mpsc::SyncSender<crate::quic::AccessUpdate>,
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
            pipeline_gap,
            clock_offset,
            clock_gen,
            clip_event_tx,
            cursor_shape_tx,
            mode_gen,
            access_grants,
            access_deadline_unix,
            access_tx,
        } = self;
        // Mid-stream clock re-sync ([`ClockResync`]): a batch every
        // CLOCK_RESYNC_INTERVAL and when the pump asks (CtrlRequest::ClockResync
        // after its first no-op flush). Echoes land in the read arm; skip if
        // the host never answered the connect-time handshake.
        let mut resync = ClockResync::new();
        let mut resync_guard = clock_rtt_ns.map(ResyncGuard::new);
        // 7 ms: without spacing the 8-round batch finishes inside one ~6 ms
        // video burst and every round samples the same congestion. 7 ms
        // staggers rounds across the ~16.7 ms frame so min-RTT usually
        // lands in a quiet gap.
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
                        CtrlRequest::Delivery(r) => r.encode(),
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
                    // Stamp at send so inter-round spacing is not in the RTT.
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
                        // Freeze delivered figures now. Counters are probe-scoped
                        // (FLAG_PROBE), so video around the burst does not inflate
                        // them. Freeze first→last arrival with them: that interval
                        // is when the bytes arrived, not when the host stopped sending.
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
                        p.active = false; // burst over — pump stops mirroring
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
                        // Host clamp is authoritative. Park it for the pump
                        // controller; any ack also means this host renegotiates.
                        tracing::info!(
                            kbps = ack.bitrate_kbps,
                            "host re-targeted encoder bitrate"
                        );
                        // 0 is a nonsense ack (controller ignores it too); don't
                        // wipe the HUD's live target.
                        if ack.bitrate_kbps > 0 {
                            live_bitrate.store(ack.bitrate_kbps, Ordering::Relaxed);
                        }
                        bitrate_ack.lock().unwrap().push_back(ack.bitrate_kbps);
                    } else if let Ok(gap) = crate::quic::PipelineGap::decode(&msg) {
                        // Host rebuilt capture+encoder; park for the pump to discard
                        // the in-flight report window (not congestion). Latest-wins.
                        // Floor at 1: 0 means nothing pending, so a rounded-down
                        // gap must not silently disarm the discard.
                        tracing::info!(
                            gap_ms = gap.gap_ms,
                            "host rebuilt its capture/encode pipeline — discarding the report \
                             window in flight"
                        );
                        pipeline_gap.store(gap.gap_ms.max(1), Ordering::Relaxed);
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
                                        // Repeated rejections are the stale-offset
                                        // starvation signature (RTT above session floor).
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
                        // Host ack/policy for the toggle UI. try_send: lagging
                        // embedder drops newest; a stale toggle heals on the next.
                        let _ = clip_event_tx.try_send(ClipEventCore::State {
                            enabled: state.enabled,
                            policy: state.policy,
                            reason: state.reason,
                        });
                    } else if let Ok(offer) = ClipOffer::decode(&msg) {
                        // Host copied: surface the lazy format list; fetch only on paste.
                        let _ = clip_event_tx.try_send(ClipEventCore::RemoteOffer {
                            seq: offer.seq,
                            kinds: offer.kinds,
                        });
                    } else if let Ok(chg) = crate::quic::ShardPayloadChanged::decode(&msg) {
                        // Mid-session shard re-key (design/shard-payload-reneg.md).
                        // Per-frame pinning: nothing to re-key on receive; ack is
                        // telemetry on shrink and the GATE on grow. Silence (no ack)
                        // on out-of-bounds — never grant a grow from garbage.
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
                    } else if let Ok(upd) = crate::quic::AccessUpdate::decode(&msg) {
                        // Console edit or T−5m/T−1m expiry. Fold into live slots
                        // FIRST, then wake the embedder so a reader never sees the
                        // pre-update mask. Host still enforces; this is courtesy.
                        tracing::info!(
                            grants = upd.grants,
                            remaining_secs = upd.remaining_secs,
                            "host updated this session's access"
                        );
                        access_grants.store(upd.grants, Ordering::Relaxed);
                        access_deadline_unix.store(
                            crate::client::access_deadline_from(
                                wall_clock_ns(),
                                upd.remaining_secs,
                            ),
                            Ordering::Relaxed,
                        );
                        let _ = access_tx.try_send(upd);
                    } else if let Ok(shape) = crate::quic::CursorShape::decode(&msg) {
                        // Pointer bitmap changed. try_send: overflow drops newest;
                        // the next shape change resends.
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
