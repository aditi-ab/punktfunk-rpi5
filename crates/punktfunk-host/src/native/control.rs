//! Native `punktfunk/1` mid-stream control task.
//!
//! After handshake the control stream stays open. This task is its sole writer:
//! inbound client requests share a `select!` with outbound probe results, mode
//! corrections, bitrate retargets, pipeline gaps, shard-payload changes, cursor
//! shapes, clipboard offers, and access updates. Validated changes go to the
//! data-plane thread over the session's mpsc bridges.
//!
//! `select!` drops the inbound read future whenever a sibling fires, so framing
//! uses [`io::MsgReader`]. Optional channels whose sender can drop mid-session
//! (`clip_offer_rx`, `shard_change_rx`, `access_rx`) must disable their branch
//! on `None` or a closed mpsc busy-spins.
//!
//! Evidence: `design/shard-payload-reneg.md`, `design/per-client-access.md`,
//! `design/clipboard-and-file-transfer.md`, `design/phase-locked-capture.md`.

use super::*;
use pf_clipboard::ClipCoordCmd;
use punktfunk_core::quic::{ClipControl, ClipOffer, ClipState};

/// Named fields, not a 30-argument spawn: `retarget_rx` and `gap_rx` are both
/// bare `u32`, so a positional swap would compile and fail at runtime.
pub(super) struct Task {
    pub(super) ctrl_send: quinn::SendStream,
    pub(super) ctrl_recv: quinn::RecvStream,
    pub(super) initial_mode: punktfunk_core::Mode,
    pub(super) codec: crate::encode::Codec,
    pub(super) live_reconfig_ok: bool,
    pub(super) adaptive_fec: bool,
    pub(super) session_bitrate_kbps: u32,
    /// Encoder-applied rate, codec ceiling (`0` = unknown), and cadence-miss
    /// flag. Read at `SetBitrate` so the ack never exceeds what the encoder
    /// will actually run.
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// See [`Self::live_bitrate`].
    pub(super) encoder_ceiling_kbps: Arc<AtomicU32>,
    /// See [`Self::live_bitrate`].
    pub(super) cadence_degraded: Arc<AtomicBool>,
    /// See [`Self::live_bitrate`].
    pub(super) cadence_behind_score: Arc<AtomicU32>,
    /// `u32::MAX` is the pre-seed: an old client never sent a `DeliveryReport`.
    pub(super) client_packets_received: Arc<AtomicU32>,
    pub(super) fec_target_ctl: Arc<AtomicU8>,
    /// Encode loop drains at its own cadence (`design/phase-locked-capture.md`).
    pub(super) phase_ctl: Arc<super::stream::PhaseCtl>,
    pub(super) reconfig_tx: std::sync::mpsc::Sender<punktfunk_core::Mode>,
    pub(super) keyframe_tx: std::sync::mpsc::Sender<()>,
    pub(super) rfi_tx: std::sync::mpsc::Sender<(u32, u32)>,
    pub(super) bitrate_tx: std::sync::mpsc::Sender<u32>,
    pub(super) probe_tx: std::sync::mpsc::Sender<ProbeRequest>,
    pub(super) probe_result_rx: tokio::sync::mpsc::UnboundedReceiver<ProbeResult>,
    pub(super) reconfig_result_rx: tokio::sync::mpsc::UnboundedReceiver<Reconfigured>,
    /// Host-initiated Automatic re-resolve. Forwarded as `BitrateChanged` so
    /// the client's climb base tracks the encoder.
    pub(super) retarget_rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Rebuild stall duration in ms. Forwarded as `PipelineGap` so the client
    /// bitrate controller drops the report window that straddled our stall.
    pub(super) gap_rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Wire-MTU watcher → `ShardPayloadChanged` (this task is the sole writer).
    /// Client `ShardPayloadAck`s return on `shard_ack_tx` and gate a grow.
    pub(super) shard_change_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
    pub(super) shard_ack_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    pub(super) cursor_shape_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::CursorShape>,
    pub(super) cursor_client_draws: Arc<AtomicBool>,
    pub(super) clip_enabled: Arc<AtomicBool>,
    pub(super) clip: pf_clipboard::ClipCoord,
    /// LIVE grant mask, same atomic the datagram filter reads. Deadline/watch
    /// folds console edits in, so a later `ClipControl`/`ClipOffer` sees them.
    pub(super) session_grants: Arc<AtomicU32>,
    /// Sole writer for deadline/watch `AccessUpdate`s (expiry + grant edits).
    pub(super) access_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AccessUpdate>,
}

/// Ends when the control stream closes or a data-plane channel drops.
pub(super) async fn run(task: Task) {
    let Task {
        mut ctrl_send,
        ctrl_recv,
        initial_mode,
        codec,
        live_reconfig_ok,
        adaptive_fec,
        session_bitrate_kbps,
        live_bitrate,
        encoder_ceiling_kbps,
        cadence_degraded,
        cadence_behind_score,
        client_packets_received,
        fec_target_ctl,
        phase_ctl,
        reconfig_tx,
        keyframe_tx,
        rfi_tx,
        bitrate_tx,
        probe_tx,
        mut probe_result_rx,
        mut reconfig_result_rx,
        mut retarget_rx,
        mut gap_rx,
        mut shard_change_rx,
        shard_ack_tx,
        mut cursor_shape_rx,
        cursor_client_draws,
        clip_enabled,
        clip,
        session_grants,
        mut access_rx,
    } = task;
    let pf_clipboard::ClipCoord {
        available: clip_available,
        cmd_tx: clip_cmd_tx,
        offer_rx: mut clip_offer_rx,
    } = clip;
    // Once `clip_offer_rx` closes, disable its `select!` arm — a closed
    // mpsc is perpetually ready with `None`.
    let mut clip_offer_closed = false;
    // First-of-class `warn!` for grant drops. A revoked client spamming the
    // gated messages must not flood the log.
    let denied = GrantDrops::new();
    // Same closed-channel discipline as `clip_offer_closed`.
    let mut shard_change_closed = false;
    // `--open` anonymous sessions never spawn deadline/watch; the sender
    // drops immediately.
    let mut access_closed = false;
    let mut active = initial_mode;
    // Backstop against Reconfigure spam. Data-plane drain-to-newest already
    // coalesces a resize drag; 500 ms is half the client's 1 s self-limit.
    const MIN_SWITCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut last_accepted_switch: Option<std::time::Instant> = None;
    // One probe per 10 s. Each probe is already clamped (5 s, 10 Gbps);
    // without a count cap a client can pause video and pin the uplink.
    const MIN_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    let mut last_probe: Option<std::time::Instant> = None;
    // `select!` drops this future whenever a sibling fires. `io::read_msg`
    // would lose a partial frame and misalign the rest of the session.
    let mut ctrl_reader = io::MsgReader::new(ctrl_recv);
    // After real loss, decay stops at 5 % not 1 % (`FecFloor`).
    let mut fec_floor = FecFloor::default();
    loop {
        tokio::select! {
            msg = ctrl_reader.read_msg() => {
                let Ok(msg) = msg else { break };
                if let Ok(req) = Reconfigure::decode(&msg) {
                    let now = std::time::Instant::now();
                    let valid = req.mode.refresh_hz > 0
                        && crate::encode::validate_dimensions(
                            codec,
                            req.mode.width,
                            req.mode.height,
                        )
                        .is_ok();
                    let too_soon = last_accepted_switch
                        .is_some_and(|t| now.duration_since(t) < MIN_SWITCH_INTERVAL);
                    let ok = if !live_reconfig_ok {
                        // Backend cannot live-reconfigure (gamescope / synthetic
                        // / per-client-mode identity). Client keeps scaling.
                        tracing::info!(mode = ?req.mode,
                            "mode switch rejected (backend cannot live-reconfigure)");
                        false
                    } else if !valid {
                        tracing::warn!(mode = ?req.mode, "mode switch rejected (invalid dimensions)");
                        false
                    } else if too_soon {
                        tracing::warn!(mode = ?req.mode, "mode switch rejected (rate-limited)");
                        false
                    } else {
                        true
                    };
                    if ok {
                        active = req.mode;
                        last_accepted_switch = Some(now);
                        tracing::info!(mode = ?req.mode, "mode switch accepted");
                    }
                    let ack = Reconfigured { accepted: ok, mode: active };
                    if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                        break;
                    }
                    if ok && reconfig_tx.send(req.mode).is_err() {
                        break;
                    }
                } else if RequestKeyframe::decode(&msg).is_ok() {
                    // Encode loop coalesces: a wedge fires several requests
                    // before the IDR lands.
                    tracing::debug!("client requested keyframe (decode recovery)");
                    if keyframe_tx.send(()).is_err() {
                        break;
                    }
                } else if let Ok(req) = RfiRequest::decode(&msg) {
                    // Encode loop falls back to a coalesced IDR when the range
                    // is too old or the encoder has no RFI.
                    tracing::debug!(
                        first = req.first_frame,
                        last = req.last_frame,
                        "client requested reference-frame invalidation (loss recovery)"
                    );
                    if rfi_tx.send((req.first_frame, req.last_frame)).is_err() {
                        break;
                    }
                } else if let Ok(rep) = punktfunk_core::quic::DeliveryReport::decode(&msg) {
                    // Unconditional: stall diagnosis needs `loss_ppm = 0` even
                    // when FEC is pinned or adaptive FEC is off. Saturate into
                    // the u32 bridge; the value only matters near zero.
                    client_packets_received.store(
                        rep.packets_received.min(u32::MAX as u64 - 1) as u32,
                        Ordering::Relaxed,
                    );
                } else if let Ok(rep) = LossReport::decode(&msg) {
                    // Data-plane send loop applies `fec_target_ctl` per frame.
                    // No-op when FEC is pinned (`PUNKTFUNK_FEC_PCT`).
                    if adaptive_fec {
                        // Jump to what this report needs; decay one point per
                        // clean ~750 ms window so a burst every few seconds
                        // does not drop FEC to the floor between hits.
                        let prev = fec_target_ctl.load(Ordering::Relaxed);
                        // Floor binds decay, not attack. Real loss raises it to
                        // 5 %; ~2 clean minutes re-earn the 1 % floor.
                        let floor = fec_floor.on_report(rep.loss_ppm);
                        let target = adapt_fec(rep.loss_ppm)
                            .max(prev.saturating_sub(1))
                            .max(floor);
                        fec_target_ctl.store(target, Ordering::Relaxed);
                        if prev != target {
                            tracing::debug!(
                                loss_ppm = rep.loss_ppm,
                                fec_pct = target,
                                prev_fec_pct = prev,
                                "adaptive FEC adjusted"
                            );
                        }
                    }
                } else if let Ok(req) = SetBitrate::decode(&msg) {
                    // Data plane rebuilds the encoder in place (first frame is
                    // an IDR with in-band SPS). PyroWave is pinned: ack the
                    // session rate so a foreign client cannot AIMD it down.
                    let resolved = if codec == crate::encode::Codec::PyroWave {
                        tracing::info!(
                            requested_kbps = req.bitrate_kbps,
                            pinned_kbps = session_bitrate_kbps,
                            "PyroWave session: mid-stream bitrate retarget refused (pinned)"
                        );
                        session_bitrate_kbps
                    } else {
                        let mut r = resolve_bitrate_kbps(req.bitrate_kbps);
                        // Ack is the client's climb base: never promise past
                        // the encoder's discovered codec ceiling (`0` = none).
                        let ceiling = encoder_ceiling_kbps.load(Ordering::Relaxed);
                        if ceiling != 0 && r > ceiling {
                            r = ceiling;
                        }
                        // On a fat LAN nothing else stops the climb, and past
                        // the compute knee more bits deepen the miss. Hold a
                        // climb at the applied rate; descents pass.
                        let live = live_bitrate.load(Ordering::Relaxed);
                        if cadence_degraded.load(Ordering::Relaxed) && live != 0 && r > live {
                            tracing::info!(
                                requested_kbps = req.bitrate_kbps,
                                held_kbps = live,
                                // Why the hold: ABR-floor vs network look the
                                // same without this score.
                                behind_score = cadence_behind_score.load(Ordering::Relaxed),
                                "bitrate climb refused — encode is behind cadence"
                            );
                            r = live;
                        }
                        r
                    };
                    tracing::debug!(
                        requested_kbps = req.bitrate_kbps,
                        resolved_kbps = resolved,
                        "mid-stream bitrate change requested"
                    );
                    let ack = BitrateChanged {
                        bitrate_kbps: resolved,
                    };
                    if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                        break;
                    }
                    if bitrate_tx.send(resolved).is_err() {
                        break;
                    }
                } else if let Ok(ack) = punktfunk_core::quic::ShardPayloadAck::decode(&msg) {
                    // Grow gate: packetizer may exceed the old size only after
                    // this ack. A dropped send means the watcher already ended.
                    tracing::info!(
                        shard_payload = ack.shard_payload,
                        "client acked shard-payload change"
                    );
                    let _ = shard_ack_tx.send(ack.shard_payload);
                } else if let Ok(req) = ProbeRequest::decode(&msg) {
                    let now = std::time::Instant::now();
                    if last_probe.is_some_and(|t| now.duration_since(t) < MIN_PROBE_INTERVAL) {
                        tracing::warn!(
                            target_kbps = req.target_kbps,
                            "speed-test probe rejected (rate-limited)"
                        );
                        continue;
                    }
                    last_probe = Some(now);
                    tracing::info!(
                        target_kbps = req.target_kbps,
                        duration_ms = req.duration_ms,
                        "speed-test probe requested"
                    );
                    if probe_tx.send(req).is_err() {
                        break;
                    }
                } else if let Ok(probe) = ClockProbe::decode(&msg) {
                    // t2/t3 are in the AU pts_ns clock. Inline; no data-plane hop.
                    let t2_ns = now_ns();
                    let echo = ClockEcho {
                        t1_ns: probe.t1_ns,
                        t2_ns,
                        t3_ns: now_ns(),
                    };
                    if io::write_msg(&mut ctrl_send, &echo.encode()).await.is_err() {
                        break;
                    }
                } else if let Ok(pr) = punktfunk_core::quic::PhaseReport::decode(&msg) {
                    // Inert when `PUNKTFUNK_PHASE_LOCK=0` (stored, never drained).
                    phase_ctl.store(pr);
                } else if let Ok(m) = punktfunk_core::quic::CursorRenderMode::decode(&msg) {
                    // Data-plane edge-detects per tick (forward+exclude vs
                    // composite). Inert without the cursor cap.
                    cursor_client_draws.store(m.client_draws, Ordering::Relaxed);
                    tracing::info!(
                        client_draws = m.client_draws,
                        "cursor render mode set by client"
                    );
                } else if let Ok(ctl) = ClipControl::decode(&msg) {
                    let granted = session_grants.load(Ordering::Relaxed)
                        & punktfunk_core::quic::GRANT_CLIPBOARD
                        != 0;
                    let (enabled, resolved_policy, reason) =
                        resolve_clip_control(pf_clipboard::policy(), granted, clip_available, ctl);
                    clip_enabled.store(enabled, Ordering::SeqCst);
                    // Enable re-announces the host clipboard; disable drops
                    // our selection. Inert handle: dropped send is fine.
                    let _ = clip_cmd_tx.send(ClipCoordCmd::SetEnabled(enabled));
                    tracing::info!(
                        enabled,
                        files = enabled
                            && resolved_policy & punktfunk_core::quic::CLIP_POLICY_FILES != 0,
                        "clipboard control"
                    );
                    let state = ClipState {
                        enabled,
                        policy: resolved_policy,
                        reason,
                    };
                    if io::write_msg(&mut ctrl_send, &state.encode()).await.is_err() {
                        break;
                    }
                } else if let Ok(offer) = ClipOffer::decode(&msg) {
                    // WRITE half of CLIPBOARD: installs a client selection on
                    // the host clipboard.
                    if clip_offer_permitted(
                        session_grants.load(Ordering::Relaxed),
                        clip_enabled.load(Ordering::SeqCst),
                    ) {
                        tracing::debug!(
                            seq = offer.seq,
                            kinds = offer.kinds.len(),
                            "clipboard offer from client"
                        );
                        let mimes = offer.kinds.iter().map(|k| k.mime.clone()).collect();
                        let _ = clip_cmd_tx.send(ClipCoordCmd::RemoteOffer {
                            seq: offer.seq,
                            mimes,
                        });
                    } else {
                        denied.note(GrantClass::Clipboard);
                    }
                } else {
                    tracing::warn!("unknown control message — ignoring");
                }
            }
            result = probe_result_rx.recv() => {
                let Some(result) = result else { break };
                if io::write_msg(&mut ctrl_send, &result.encode()).await.is_err() {
                    break;
                }
            }
            n = shard_change_rx.recv(), if !shard_change_closed => {
                // `None` is the watcher's bounded lifetime, not session end:
                // disable this branch or a closed mpsc busy-spins `select!`.
                let Some(n) = n else { shard_change_closed = true; continue };
                let msg = punktfunk_core::quic::ShardPayloadChanged { shard_payload: n };
                if io::write_msg(&mut ctrl_send, &msg.encode()).await.is_err() {
                    break;
                }
            }
            shape = cursor_shape_rx.recv() => {
                // ≤ ~58 KiB fits the u16 frame (`cursor_fwd` downscales).
                let Some(shape) = shape else { break };
                if io::write_msg(&mut ctrl_send, &shape.encode()).await.is_err() {
                    break;
                }
            }
            update = access_rx.recv(), if !access_closed => {
                // `None` = deadline/watch ended or never existed (`--open`):
                // disable the branch.
                match update {
                    Some(u) => {
                        // Clearing `clip_enabled` only stops host→client. The
                        // selection this device installed stays on the host
                        // clipboard until `SetEnabled(false)` drops it.
                        if u.grants & punktfunk_core::quic::GRANT_CLIPBOARD == 0 {
                            let _ = clip_cmd_tx.send(ClipCoordCmd::SetEnabled(false));
                        }
                        if io::write_msg(&mut ctrl_send, &u.encode()).await.is_err() {
                            break;
                        }
                    }
                    None => access_closed = true,
                }
            }
            offer = clip_offer_rx.recv(), if !clip_offer_closed => {
                // Forward while sync is on — a race with a just-received
                // disable would leak a stale offer. `None` = coordinator gone.
                match offer {
                    Some(offer) => {
                        if clip_enabled.load(Ordering::SeqCst)
                            && io::write_msg(&mut ctrl_send, &offer.encode()).await.is_err()
                        {
                            break;
                        }
                    }
                    None => clip_offer_closed = true,
                }
            }
            retarget = retarget_rx.recv() => {
                // Same `BitrateChanged` as `SetBitrate`. PyroWave is pinned
                // against client retargets, but a mode switch re-resolves the
                // pin (~1.6 bpp for the new pixel rate) and the live-rate
                // display otherwise stays on the old number.
                let Some(kbps) = retarget else { break };
                tracing::info!(
                    kbps,
                    "encoder re-targeted by a pipeline rebuild — telling the client"
                );
                if io::write_msg(&mut ctrl_send, &BitrateChanged { bitrate_kbps: kbps }.encode())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            gap = gap_rx.recv() => {
                // Client bitrate controller must drop the straddling report
                // window, not read our stall as congestion. After the fact:
                // `gap_ms` is measured.
                let Some(gap_ms) = gap else { break };
                tracing::info!(
                    gap_ms,
                    "pipeline rebuilt in place — telling the client the stream had a gap"
                );
                if io::write_msg(&mut ctrl_send, &PipelineGap { gap_ms }.encode())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            correction = reconfig_result_rx.recv() => {
                // Mode actually live after a failed rebuild or a refresh the
                // backend honored differently. Keep `active` truthful for
                // later rejection echoes.
                let Some(ack) = correction else { break };
                active = ack.mode;
                if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Operator policy (`None` = off), then CLIPBOARD grant (AND, never override),
/// then backend availability. Returns `(enabled, resolved_policy, reason)`.
///
/// A grant refusal still reports the operator policy bits so the client can
/// say "not permitted for this device" without greying the file toggle.
fn resolve_clip_control(
    policy: Option<u8>,
    granted: bool,
    clip_available: bool,
    ctl: ClipControl,
) -> (bool, u8, u8) {
    match policy {
        None => (false, 0, punktfunk_core::quic::CLIP_REASON_POLICY_DISABLED),
        Some(p) if !granted => (false, p, punktfunk_core::quic::CLIP_REASON_NOT_PERMITTED),
        Some(p) if ctl.enabled && !clip_available => (
            false,
            p,
            punktfunk_core::quic::CLIP_REASON_BACKEND_UNAVAILABLE,
        ),
        Some(p) => {
            let files_ok = p & punktfunk_core::quic::CLIP_POLICY_FILES != 0;
            let wants_files = ctl.flags & punktfunk_core::quic::CLIP_FLAG_FILES != 0;
            let reason = if wants_files && !files_ok {
                punktfunk_core::quic::CLIP_REASON_NO_FILES
            } else {
                punktfunk_core::quic::CLIP_REASON_OK
            };
            (ctl.enabled, p, reason)
        }
    }
}

/// LIVE CLIPBOARD grant ANDed with the last resolved sync state. Both are
/// read at offer time so a mid-session revoke closes this direction too.
fn clip_offer_permitted(grants: u32, clip_enabled: bool) -> bool {
    grants & punktfunk_core::quic::GRANT_CLIPBOARD != 0 && clip_enabled
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{
        CLIP_FLAG_FILES, CLIP_POLICY_FILES, CLIP_POLICY_TEXT, CLIP_REASON_BACKEND_UNAVAILABLE,
        CLIP_REASON_NOT_PERMITTED, CLIP_REASON_NO_FILES, CLIP_REASON_OK,
        CLIP_REASON_POLICY_DISABLED, GRANT_ALL, GRANT_CLIPBOARD,
    };

    const ON: ClipControl = ClipControl {
        enabled: true,
        flags: 0,
    };

    #[test]
    fn clip_resolution_three_way() {
        let both = CLIP_POLICY_TEXT | CLIP_POLICY_FILES;

        assert_eq!(
            resolve_clip_control(None, false, true, ON),
            (false, 0, CLIP_REASON_POLICY_DISABLED)
        );
        assert_eq!(
            resolve_clip_control(None, true, true, ON),
            (false, 0, CLIP_REASON_POLICY_DISABLED)
        );

        assert_eq!(
            resolve_clip_control(Some(both), false, true, ON),
            (false, both, CLIP_REASON_NOT_PERMITTED)
        );

        assert_eq!(
            resolve_clip_control(Some(both), true, false, ON),
            (false, both, CLIP_REASON_BACKEND_UNAVAILABLE)
        );
        assert_eq!(
            resolve_clip_control(
                Some(CLIP_POLICY_TEXT),
                true,
                true,
                ClipControl {
                    enabled: true,
                    flags: CLIP_FLAG_FILES,
                }
            ),
            (true, CLIP_POLICY_TEXT, CLIP_REASON_NO_FILES)
        );
        assert_eq!(
            resolve_clip_control(Some(both), true, true, ON),
            (true, both, CLIP_REASON_OK)
        );
    }

    #[test]
    fn clip_offer_needs_the_live_grant() {
        assert!(clip_offer_permitted(GRANT_ALL, true));

        assert!(!clip_offer_permitted(GRANT_ALL & !GRANT_CLIPBOARD, true));
        assert!(!clip_offer_permitted(0, true));

        assert!(!clip_offer_permitted(GRANT_ALL & !GRANT_CLIPBOARD, false));
        assert!(!clip_offer_permitted(GRANT_ALL, false));
    }
}
