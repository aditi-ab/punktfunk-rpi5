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
    pub(super) clock_offset: Arc<std::sync::atomic::AtomicI64>,
    pub(super) clock_gen: Arc<AtomicU32>,
    /// Clipboard metadata events (ClipState/ClipOffer) feed the same event plane the
    /// clipboard task uses for fetch data.
    pub(super) clip_event_tx: std::sync::mpsc::SyncSender<ClipEventCore>,
    /// Host cursor shapes ([`CursorShape`], sent on pointer-bitmap change) → the embedder's
    /// shape plane ([`NativeClient::next_cursor_shape`]).
    pub(super) cursor_shape_tx: std::sync::mpsc::SyncSender<crate::quic::CursorShape>,
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
            clock_offset,
            clock_gen,
            clip_event_tx,
            cursor_shape_tx,
        } = self;
        // Mid-stream clock re-sync (see [`ClockResync`]): a batch runs every
        // CLOCK_RESYNC_INTERVAL and whenever the pump asks (CtrlRequest::ClockResync after
        // its first no-op clock flush). Echoes interleave with the other control replies in
        // the read arm below; only when the host answered the connect-time handshake — an
        // old host would just eat the probes.
        let mut resync = ClockResync::new();
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
                        CtrlRequest::Keyframe => RequestKeyframe.encode(),
                        CtrlRequest::Rfi(r) => r.encode(),
                        CtrlRequest::Loss(r) => r.encode(),
                        CtrlRequest::SetBitrate(k) => SetBitrate { bitrate_kbps: k }.encode(),
                        CtrlRequest::ClockResync => {
                            if clock_rtt_ns.is_none() {
                                continue; // no connect-time handshake — host can't answer
                            }
                            resync.begin(wall_clock_ns()).encode()
                        }
                        CtrlRequest::ClipControl(c) => c.encode(),
                        CtrlRequest::ClipOffer(o) => o.encode(),
                        CtrlRequest::CursorRender(m) => m.encode(),
                    };
                    if io::write_msg(&mut ctrl_send, &bytes).await.is_err() {
                        break;
                    }
                }
                _ = resync_tick.tick(), if clock_rtt_ns.is_some() => {
                    let probe = resync.begin(wall_clock_ns());
                    if io::write_msg(&mut ctrl_send, &probe.encode()).await.is_err() {
                        break;
                    }
                }
                msg = ctrl_recv.read_msg() => {
                    let Ok(msg) = msg else { break }; // stream closed
                    if let Ok(ack) = Reconfigured::decode(&msg) {
                        if ack.accepted {
                            *mode_slot.lock().unwrap() = ack.mode;
                            tracing::info!(mode = ?ack.mode, "host accepted mode switch");
                        } else {
                            tracing::warn!(active = ?ack.mode, "host rejected mode switch");
                        }
                    } else if let Ok(result) = ProbeResult::decode(&msg) {
                        let mut p = probe.lock().unwrap();
                        // Freeze the delivered figures now (the burst is done), before resumed
                        // video can inflate the packet counters.
                        let base_p = p.base_packets.unwrap_or(p.rx_packets_now);
                        let base_b = p.base_bytes.unwrap_or(p.rx_bytes_now);
                        p.delivered_packets = p.rx_packets_now.saturating_sub(base_p);
                        p.delivered_bytes = p.rx_bytes_now.saturating_sub(base_b);
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
                        *bitrate_ack.lock().unwrap() = Some(ack.bitrate_kbps);
                    } else if let Ok(echo) = ClockEcho::decode(&msg) {
                        match resync.on_echo(&echo, wall_clock_ns()) {
                            ResyncStep::Probe(p) => {
                                if io::write_msg(&mut ctrl_send, &p.encode()).await.is_err() {
                                    break;
                                }
                            }
                            ResyncStep::Done { offset_ns, rtt_ns } => {
                                // Never let a congested window bias the offset (frames read
                                // late exactly then) — keep the old estimate and let the next
                                // periodic batch try again.
                                if accept_resync(rtt_ns, clock_rtt_ns.unwrap_or(0)) {
                                    // info, not debug: ≤1/min, and it is THE forensic
                                    // trail for a stale-offset (stepped/slewed wall clock)
                                    // latency plateau — the 2026-07 two-pair investigation
                                    // had to reconstruct this blind.
                                    tracing::info!(
                                        offset_ns,
                                        rtt_us = rtt_ns / 1000,
                                        "mid-stream clock re-sync applied"
                                    );
                                    clock_offset.store(offset_ns, Ordering::Relaxed);
                                    clock_gen.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    tracing::info!(
                                        rtt_us = rtt_ns / 1000,
                                        "clock re-sync batch discarded — RTT above the \
                                         connect-time baseline (congested window)"
                                    );
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
