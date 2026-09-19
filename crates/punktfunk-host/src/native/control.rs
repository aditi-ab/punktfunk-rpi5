//! Native `punktfunk/1` mid-stream control task.
//!
//! After handshake the control stream stays open. This task is its sole writer:
//! inbound client requests share a `select!` with outbound probe results, mode
//! corrections, bitrate retargets, pipeline gaps, shard-payload changes, cursor
//! shapes, clipboard offers, and access updates. Validated changes go to the
//! data-plane thread over the session's mpsc bridges.
//!
//! Every loss report, RFI and keyframe ask lands here, so the per-minute `link health` line
//! ([`crate::link_health`]) is counted and emitted here too.
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
use punktfunk_core::abr::governor::ShareWindow;
use punktfunk_core::quic::{AckReason, ClipControl, ClipOffer, ClipState};

/// The ack this client can read. The reason byte goes only to a client that
/// asked for it (`EXT_ABR_ACK_REASON`); every other client gets the nine bytes
/// it has always got, whatever the host knows.
fn bitrate_ack(kbps: u32, why: AckReason, client_reads_reason: bool) -> BitrateChanged {
    BitrateChanged {
        bitrate_kbps: kbps,
        reason: client_reads_reason.then_some(why),
    }
}

/// When each of the governor's up-moves may next go out.
struct ShareClocks {
    room: std::time::Instant,
    lift: std::time::Instant,
}

impl ShareClocks {
    fn new(now: std::time::Instant) -> Self {
        ShareClocks {
            room: now + punktfunk_core::abr::governor::SHARE_CLOCK,
            lift: now + punktfunk_core::abr::governor::SHARE_LIFT_CLOCK,
        }
    }

    fn take(&mut self, now: std::time::Instant) -> punktfunk_core::abr::governor::Clocks {
        let out = punktfunk_core::abr::governor::Clocks {
            room: now >= self.room,
            lift: now >= self.lift,
        };
        if out.room {
            self.room = now + punktfunk_core::abr::governor::SHARE_CLOCK;
        }
        if out.lift {
            self.lift = now + punktfunk_core::abr::governor::SHARE_LIFT_CLOCK;
        }
        out
    }
}

/// What a client's [`punktfunk_core::quic::DeliveryReport`] leaves this session:
/// the share of the path it is on, if the governor has one to send.
///
/// The report is the boundary both figures are read over, so closing the window
/// and asking are one step ([`crate::session_status::share_for`]). A group of
/// one never has a share. The id is `0` until the video loop registers the
/// session, and before that there is nothing for a sibling to share with.
#[allow(clippy::too_many_arguments)]
fn delivery_share(
    now: std::time::Instant,
    packets_received: u64,
    counters: &crate::session_status::SessionCounters,
    window: &mut ShareWindow,
    clocks: &mut ShareClocks,
    automatic: bool,
    wire_bytes: u64,
) -> Option<u32> {
    let (offered, delivered) = window.close(
        now,
        counters.link.egress_bytes(),
        packets_received,
        wire_bytes,
    );
    counters.share.publish(automatic, offered, delivered);
    let id = counters.link.session_id();
    (id != 0)
        .then(|| crate::session_status::share_for(id, clocks.take(now)))
        .flatten()
}

/// Whether this probe request skips the one-per-10 s spacing: a bring-up ramp
/// step, which is short and lands on a data plane with no video on it.
///
/// The length bound is the exemption's own limit. Without it a client could
/// hold the window open with 5 s bursts at the probe ceiling, which is the
/// uplink-pinning the spacing exists against.
fn is_ramp_step(req: &ProbeRequest, ramp_open: bool) -> bool {
    ramp_open && req.duration_ms <= super::stream::RAMP_STEP_MAX_MS
}

/// Named fields, not a 30-argument spawn: `retarget_rx` and `gap_rx` are both
/// bare `u32`, so a positional swap would compile and fail at runtime.
pub(super) struct Task {
    pub(super) ctrl_send: super::link::CtlSend,
    pub(super) ctrl_recv: super::link::CtlRecv,
    pub(super) initial_mode: punktfunk_core::Mode,
    pub(super) codec: crate::encode::Codec,
    pub(super) live_reconfig_ok: bool,
    pub(super) adaptive_fec: bool,
    pub(super) session_bitrate_kbps: u32,
    /// Automatic bitrate, so the shared-path governor may move this session. A
    /// client-set rate and a PyroWave pin are never touched.
    pub(super) bitrate_automatic: bool,
    /// One wire packet, bytes. Turns the client's delivery count into the rate
    /// the governor divides.
    pub(super) wire_bytes: u64,
    /// Client set `EXT_ABR_ACK_REASON` in its `Start` block: its `BitrateChanged`
    /// may carry the reason byte. Clear for every shipped client, which rejects
    /// a longer ack, and for every client behind a host without `HOST_CAP2_EXT`.
    pub(super) ack_reason: bool,
    /// Encoder-applied rate, codec ceiling (`0` = unknown), and cadence-miss
    /// flag. Read at `SetBitrate` so the ack never exceeds what the encoder
    /// will actually run.
    pub(super) live_bitrate: Arc<AtomicU32>,
    /// See [`Self::live_bitrate`]. The sole place a ceiling decides an ask:
    /// the data plane learns it, this task spends it.
    pub(super) encoder_ceiling: Arc<std::sync::Mutex<super::EncoderCeiling>>,
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
    /// The client's bring-up ramp may still be running: its steps are served
    /// on the punched-but-idle data plane, and the spacing below would let
    /// one step through and refuse the rest. Cleared when the send thread
    /// takes the session over (`stream::ramp`).
    pub(super) ramp_open: Arc<AtomicBool>,
    pub(super) probe_result_rx: tokio::sync::mpsc::UnboundedReceiver<ProbeResult>,
    pub(super) reconfig_result_rx: tokio::sync::mpsc::UnboundedReceiver<Reconfigured>,
    /// The rate the encoder settled on, with what settled it. Forwarded as
    /// `BitrateChanged` so the client's climb base tracks the encoder: a
    /// rebuild's own re-resolve is `Granted`, a short apply an `EncoderLimit`.
    pub(super) retarget_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, AckReason)>,
    /// Rebuild stall duration in ms. Forwarded as `PipelineGap` so the client
    /// bitrate controller drops the report window that straddled our stall.
    pub(super) gap_rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
    /// Wire-MTU watcher → `ShardPayloadChanged` (this task is the sole writer).
    /// Client `ShardPayloadAck`s return on `shard_ack_tx` and gate a grow.
    pub(super) shard_change_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
    pub(super) shard_ack_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    /// Depth-1 latest-wins slot: the encode loop overwrites a shape this task
    /// has not drained, so a stalled peer cannot grow the host.
    pub(super) cursor_shape_rx:
        tokio::sync::watch::Receiver<Option<punktfunk_core::quic::CursorShape>>,
    pub(super) cursor_client_draws: Arc<AtomicBool>,
    pub(super) clip_enabled: Arc<AtomicBool>,
    pub(super) clip: pf_clipboard::ClipCoord,
    /// LIVE grant mask, same atomic the datagram filter reads. Deadline/watch
    /// folds console edits in, so a later `ClipControl`/`ClipOffer` sees them.
    pub(super) session_grants: Arc<AtomicU32>,
    /// Sole writer for `AccessUpdate`s: the deadline/watch task and the per-session
    /// management route both send here, so both clear the clipboard the same way.
    pub(super) access_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AccessUpdate>,
    /// Operator mute for this session (mgmt `PUT /session/{id}/audio`), so the client
    /// can name the silence rather than conceal a gap.
    pub(super) audio_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::AudioState>,
    /// OS pad slots this session holds, from the input thread. The client names the
    /// player it is from this; wire indices are per client and say nothing about it.
    pub(super) pad_slots_rx: tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::PadSlots>,
    /// What became of this session's library launch, from the launch site and
    /// from the lease when the game dies on the spot.
    pub(super) launch_outcome_rx:
        tokio::sync::mpsc::UnboundedReceiver<punktfunk_core::quic::LaunchOutcome>,
    /// Named on the per-minute `link health` line, so a journal sorts by client.
    pub(super) peer: std::net::IpAddr,
    /// Shared block the encode and send threads bump; this task drains its link half.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
    /// Armed capture the per-minute line is also written into, so a bug report is one file.
    pub(super) stats: Arc<crate::stats_recorder::StatsRecorder>,
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
        bitrate_automatic,
        wire_bytes,
        ack_reason,
        live_bitrate,
        encoder_ceiling,
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
        ramp_open,
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
        mut audio_rx,
        mut pad_slots_rx,
        mut launch_outcome_rx,
        peer,
        counters,
        stats,
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
    // Same closed-channel discipline: the mute lane outlives nothing of its own.
    let mut audio_closed = false;
    // Pad slots end with the input thread.
    let mut pad_slots_closed = false;
    // Same again. The launch site drops its sender when the session ends.
    let mut launch_outcome_closed = false;
    let mut active = initial_mode;
    // Backstop against Reconfigure spam. Data-plane drain-to-newest already
    // coalesces a resize drag; 500 ms is half the client's 1 s self-limit.
    const MIN_SWITCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut last_accepted_switch: Option<std::time::Instant> = None;
    // One probe per 10 s. Each probe is already clamped (5 s, 10 Gbps);
    // without a count cap a client can pause video and pin the uplink.
    const MIN_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    let mut last_probe: Option<std::time::Instant> = None;
    // An RFI ask is a frame parity could not repair; the LossReport that
    // closes the window carries only what parity did repair.
    let mut unrecovered = UnrecoveredRun::default();
    // Shared-path governor: what this session offered and what reached it over
    // the last window, read at the same boundary so a shortfall describes one
    // stretch of link, plus the two clocks an up-move rides.
    let mut window = ShareWindow::new(std::time::Instant::now(), counters.link.egress_bytes());
    let mut share_clocks = ShareClocks::new(std::time::Instant::now());
    // One `link health` line a minute, ticking whether or not anything arrived: a reader must
    // be able to tell a clean minute from a host that stopped logging.
    let mut link = crate::link_health::LinkWindow::new(&counters.link);
    let mut link_tick = tokio::time::interval(std::time::Duration::from_secs(60));
    link_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately; the first tick would close an empty window at 0 s.
    link_tick.tick().await;
    // `select!` drops this future whenever a sibling fires. `io::read_msg`
    // would lose a partial frame and misalign the rest of the session.
    let mut ctrl_reader = io::MsgReader::new(ctrl_recv);
    loop {
        tokio::select! {
            msg = ctrl_reader.read_msg() => {
                let Ok(msg) = msg else { break };
                if let Ok(req) = Reconfigure::decode(&msg) {
                    let now = std::time::Instant::now();
                    // Same bound as the handshake: `> 0` alone acked a mode that cannot land.
                    let valid = crate::encode::validate_refresh(req.mode.refresh_hz).is_ok()
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
                    link.note_keyframe_req();
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
                    unrecovered.rfi();
                    link.note_rfi();
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
                    if let Some(share) = delivery_share(
                        std::time::Instant::now(),
                        rep.packets_received,
                        &counters,
                        &mut window,
                        &mut share_clocks,
                        bitrate_automatic,
                        wire_bytes,
                    ) {
                        // A share under the live rate is a retarget the encoder
                        // takes now; one above it is a ceiling the client still
                        // has to earn, so nothing is applied for it.
                        let live = live_bitrate.load(Ordering::Relaxed);
                        if share > 0 && live > share && bitrate_tx.send(share).is_err() {
                            break;
                        }
                        let ack = bitrate_ack(share, AckReason::Governor, ack_reason);
                        if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                            break;
                        }
                    }
                } else if let Ok(rep) = LossReport::decode(&msg) {
                    let unrecovered_run = unrecovered.report(std::time::Instant::now());
                    link.note_loss(rep.loss_ppm, unrecovered_run);
                    link.sample_bands(
                        fec_target_ctl.load(Ordering::Relaxed),
                        live_bitrate.load(Ordering::Relaxed),
                    );
                    // Data-plane send loop applies `fec_target_ctl` per frame.
                    // No-op when FEC is pinned (`PUNKTFUNK_FEC_PCT`).
                    if adaptive_fec {
                        let prev = fec_target_ctl.load(Ordering::Relaxed);
                        let target = fec_target(rep.loss_ppm, prev, unrecovered_run);
                        fec_target_ctl.store(target, Ordering::Relaxed);
                        if prev != target {
                            tracing::debug!(
                                loss_ppm = rep.loss_ppm,
                                unrecovered_run,
                                fec_pct = target,
                                prev_fec_pct = prev,
                                "adaptive FEC adjusted"
                            );
                        }
                    }
                } else if let Ok(req) = SetBitrate::decode(&msg) {
                    link.note_bitrate_ask(
                        req.bitrate_kbps,
                        live_bitrate.load(Ordering::Relaxed),
                    );
                    // Data plane rebuilds the encoder in place (first frame is
                    // an IDR with in-band SPS). PyroWave is pinned: ack the
                    // session rate so a foreign client cannot AIMD it down.
                    let (resolved, why) = if codec == crate::encode::Codec::PyroWave {
                        tracing::info!(
                            requested_kbps = req.bitrate_kbps,
                            pinned_kbps = session_bitrate_kbps,
                            "PyroWave session: mid-stream bitrate retarget refused (pinned)"
                        );
                        (session_bitrate_kbps, AckReason::Pinned)
                    } else {
                        let mut want = resolve_bitrate_kbps(req.bitrate_kbps);
                        let mut why = AckReason::Granted;
                        // On a fat LAN nothing else stops the climb, and past
                        // the compute knee more bits deepen the miss. Hold a
                        // climb at the applied rate; descents pass. Held first
                        // because a rate the encoder never sees must not spend
                        // the ceiling's re-test.
                        let live = live_bitrate.load(Ordering::Relaxed);
                        if cadence_degraded.load(Ordering::Relaxed) && live != 0 && want > live {
                            tracing::info!(
                                requested_kbps = req.bitrate_kbps,
                                held_kbps = live,
                                // Why the hold: ABR-floor vs network look the
                                // same without this score.
                                behind_score = cadence_behind_score.load(Ordering::Relaxed),
                                "bitrate climb refused — encode is behind cadence"
                            );
                            want = live;
                            why = AckReason::Cadence;
                        }
                        // Ack is the client's climb base: never promise past a
                        // ceiling the encoder taught, unless its wait has run
                        // out and this ask is the re-test. A ceiling under the
                        // held rate binds tighter, and names the ack instead.
                        let (mut r, ceiling_why) = encoder_ceiling
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .resolve(want);
                        if r < want {
                            why = ceiling_why;
                        }
                        // The share this session holds on a path it is not
                        // alone on binds last: whatever else allows, it may not
                        // take a sibling's half.
                        let share = counters.share.share_kbps();
                        if share > 0 && r > share {
                            r = share;
                            why = AckReason::Governor;
                        }
                        (r, why)
                    };
                    tracing::debug!(
                        requested_kbps = req.bitrate_kbps,
                        resolved_kbps = resolved,
                        "mid-stream bitrate change requested"
                    );
                    let ack = bitrate_ack(resolved, why, ack_reason);
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
                    let ramping = is_ramp_step(&req, ramp_open.load(Ordering::SeqCst));
                    if !ramping
                        && last_probe.is_some_and(|t| now.duration_since(t) < MIN_PROBE_INTERVAL)
                    {
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
            changed = cursor_shape_rx.changed() => {
                // Err = encode loop gone, i.e. session end.
                if changed.is_err() {
                    break;
                }
                // ≤ ~58 KiB fits the u16 frame (`cursor_fwd` downscales).
                let shape = cursor_shape_rx.borrow_and_update().clone();
                let Some(shape) = shape else { continue };
                if io::write_msg(&mut ctrl_send, &shape.encode()).await.is_err() {
                    break;
                }
            }
            outcome = launch_outcome_rx.recv(), if !launch_outcome_closed => {
                // `None` = every sender gone; disable the arm rather than spin.
                let Some(outcome) = outcome else { launch_outcome_closed = true; continue };
                tracing::info!(
                    outcome = outcome.kind.as_str(),
                    said = %outcome.message,
                    "told the client how its launch turned out"
                );
                if io::write_msg(&mut ctrl_send, &outcome.encode()).await.is_err() {
                    break;
                }
            }
            state = audio_rx.recv(), if !audio_closed => {
                // `None` = every sender gone. Disable the arm; a closed mpsc is
                // perpetually ready and would spin `select!`.
                let Some(state) = state else { audio_closed = true; continue };
                if io::write_msg(&mut ctrl_send, &state.encode()).await.is_err() {
                    break;
                }
            }
            slots = pad_slots_rx.recv(), if !pad_slots_closed => {
                // Same closed-mpsc rule as the audio arm above. The input thread is
                // the only sender, and it ends with the session.
                let Some(slots) = slots else { pad_slots_closed = true; continue };
                if io::write_msg(&mut ctrl_send, &slots.encode()).await.is_err() {
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
                let Some((kbps, why)) = retarget else { break };
                tracing::info!(
                    kbps,
                    "encoder re-targeted by a pipeline rebuild — telling the client"
                );
                let ack = bitrate_ack(kbps, why, ack_reason);
                if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                    break;
                }
            }
            gap = gap_rx.recv() => {
                // Client bitrate controller must drop the straddling report
                // window, not read our stall as congestion. After the fact:
                // `gap_ms` is measured.
                let Some(gap_ms) = gap else { break };
                link.note_gap();
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
            _ = link_tick.tick() => {
                emit_link(&mut link, &counters, &fec_target_ctl, &live_bitrate, &stats, peer);
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
    // A session that ends mid-minute still reports what it had.
    emit_link(
        &mut link,
        &counters,
        &fec_target_ctl,
        &live_bitrate,
        &stats,
        peer,
    );
}

/// Close the window, log it, and append it to an armed capture.
///
/// The FEC and ABR bands are sampled here as well as per report window, so the line names the
/// rates a silent minute ran at rather than a zero it never measured.
fn emit_link(
    link: &mut crate::link_health::LinkWindow,
    counters: &crate::session_status::SessionCounters,
    fec_target_ctl: &AtomicU8,
    live_bitrate: &AtomicU32,
    stats: &crate::stats_recorder::StatsRecorder,
    peer: std::net::IpAddr,
) {
    let m = link.close(
        &counters.link,
        fec_target_ctl.load(Ordering::Relaxed),
        live_bitrate.load(Ordering::Relaxed),
    );
    crate::link_health::emit(&m, peer);
    if stats.is_armed() {
        stats.push_link(m);
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

    /// One wire packet, and the rate both modelled sessions open at.
    const WIRE: u64 = 1_448;
    const START_KBPS: u32 = 12_000;

    /// One modelled session: the [`punktfunk_core::abr::Driver`] a real client
    /// runs, the registry entry and counters the host keeps for it, and the
    /// share window its reports close.
    struct Peer {
        abr: punktfunk_core::abr::Driver,
        counters: Arc<crate::session_status::SessionCounters>,
        /// Encoder target, the same Arc the governor reads as `current_kbps`.
        rate: Arc<AtomicU32>,
        window: ShareWindow,
        clocks: ShareClocks,
        stats: punktfunk_core::stats::Stats,
        egress: u64,
        arrived: u64,
        /// The share standing over this session, and every one it was told.
        share: u32,
        told: Vec<(u64, u32)>,
        _live: crate::session_status::LiveSessionGuard,
    }

    fn peer(
        name: &str,
        at: std::net::IpAddr,
        base: std::time::Instant,
        reads_delivery: bool,
    ) -> Peer {
        let (live, counters, rate) =
            crate::session_status::tests::fake_member(name, at, START_KBPS);
        Peer {
            abr: punktfunk_core::abr::Driver::new(
                punktfunk_core::abr::DriverConfig {
                    start_kbps: START_KBPS,
                    ceiling_cap_kbps: None,
                    stream_cap_kbps: 200_000,
                    refresh_hz: 60,
                    codec: punktfunk_core::quic::CODEC_HEVC,
                    bit_depth: 8,
                    chroma_format: punktfunk_core::quic::CHROMA_IDC_420,
                    audio_reserved_kbps: 256,
                    marks_repeats: true,
                    reads_delivery,
                    probe: false,
                    probe_target_kbps: None,
                    ramp: false,
                },
                base,
            ),
            counters,
            rate,
            window: ShareWindow::new(base, 0),
            clocks: ShareClocks::new(base),
            stats: punktfunk_core::stats::Stats::default(),
            egress: 0,
            arrived: 0,
            share: 0,
            told: Vec::new(),
            _live: live,
        }
    }

    impl Peer {
        /// One millisecond of this session: the host puts its encoder rate on
        /// the wire, the link carries what it has room for, and whatever the
        /// driver asks to send reaches the host's own handling of it.
        fn step(&mut self, ms: u64, at: std::time::Instant, link_kbps: u32) {
            let offer = u64::from(self.rate.load(Ordering::Relaxed));
            self.egress += offer * 125 / 1_000;
            self.counters.link.publish_egress_bytes(self.egress);
            self.arrived += offer.min(u64::from(link_kbps)) * 125 / 1_000;
            self.stats.packets_received = self.arrived / WIRE;
            self.stats.bytes_received = self.arrived;
            if ms % 16 == 0 {
                self.stats.frames_completed += 1;
                self.abr.on_au(false);
            }
            self.abr.on_stats(&self.stats);
            for action in self.abr.tick(at).actions {
                match action {
                    punktfunk_core::abr::Action::Delivery(packets) => {
                        let Some(share) = delivery_share(
                            at,
                            packets,
                            &self.counters,
                            &mut self.window,
                            &mut self.clocks,
                            true,
                            WIRE,
                        ) else {
                            continue;
                        };
                        self.share = share;
                        self.told.push((ms, share));
                        // As the branch does: a share under the live rate is a
                        // retarget the encoder takes now.
                        if share > 0 && self.rate.load(Ordering::Relaxed) > share {
                            self.rate.store(share, Ordering::Relaxed);
                        }
                    }
                    // As the `SetBitrate` branch does: the share binds last.
                    punktfunk_core::abr::Action::SetBitrate(kbps) => {
                        let want = if self.share > 0 {
                            kbps.min(self.share)
                        } else {
                            kbps
                        };
                        self.rate.store(want, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Eighteen seconds of a path that falls twice under these sessions: nine
    /// megabits, then five, then three, halved because two of them share it.
    fn falls_twice(peers: &mut [Peer], base: std::time::Instant) {
        for ms in 0..18_000 {
            let link = match ms {
                0..6_000 => 9_000,
                6_000..12_000 => 5_000,
                _ => 3_000,
            } / 2;
            for p in peers.iter_mut() {
                p.step(ms, base + std::time::Duration::from_millis(ms), link);
            }
        }
    }

    /// The sequence a real client sends, through the host's own handling of it:
    /// two sessions at one address over a path that falls twice under them.
    ///
    /// Each is told a share again at every fall, because the driver closes the
    /// host's share window every report window. A client that reports once
    /// leaves the host one reading of the path and no way to take another, and
    /// this is the test that fails on it. The third session is alone at its own
    /// address and is never told anything.
    #[test]
    fn a_real_clients_reports_keep_dividing_a_shared_path() {
        let base = std::time::Instant::now();
        let shared: std::net::IpAddr = "203.0.113.41".parse().unwrap();
        let mut peers = [
            peer("phone", shared, base, true),
            peer("pc", shared, base, true),
            peer("tv", "203.0.113.42".parse().unwrap(), base, true),
        ];
        falls_twice(&mut peers, base);
        for p in &peers[..2] {
            assert!(
                p.told.len() >= 3,
                "three falls, {} shares: {:?}",
                p.told.len(),
                p.told
            );
            assert!(
                p.told.last().is_some_and(|&(ms, _)| ms > 12_000),
                "the governor stopped early: {:?}",
                p.told
            );
        }
        assert!(
            peers[2].told.is_empty(),
            "a session alone at its address was told {:?}",
            peers[2].told
        );
    }

    /// New host, old client: a client that does not stream its count leaves the
    /// host one reading of the path, so the pair is divided at most once and in
    /// the first window — today's behaviour, reached by never being told.
    #[test]
    fn a_client_that_reports_once_is_governed_at_most_once() {
        let base = std::time::Instant::now();
        let shared: std::net::IpAddr = "203.0.113.43".parse().unwrap();
        let mut peers = [
            peer("phone", shared, base, false),
            peer("pc", shared, base, false),
        ];
        falls_twice(&mut peers, base);
        for p in &peers {
            assert!(
                p.told.len() <= 1 && p.told.iter().all(|&(ms, _)| ms < 2_000),
                "an old client was governed past its first window: {:?}",
                p.told
            );
        }
    }

    /// Old client → new host: a client whose `Start` carried no `EXT_TAG_ABR`
    /// gets the ack it has always got — nine bytes, whatever the host knows
    /// about the refusal.
    #[test]
    fn a_client_that_did_not_ask_still_gets_a_nine_byte_ack() {
        for why in [
            AckReason::Granted,
            AckReason::EncoderLimit,
            AckReason::Cadence,
            AckReason::Pinned,
        ] {
            let old = bitrate_ack(41_852, why, false);
            assert_eq!(old.reason, None);
            assert_eq!(old.encode().len(), 9);
            let new = bitrate_ack(41_852, why, true);
            assert_eq!(new.reason, Some(why));
            assert_eq!(new.encode().len(), 10);
            assert_eq!(new.bitrate_kbps, old.bitrate_kbps);
        }
    }

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

    /// The bring-up exemption is bounded twice: the window has to be open,
    /// and the step short. An 800 ms burst or a 5 s one is spaced like any
    /// other, open window or not.
    #[test]
    fn only_a_short_step_inside_the_window_skips_the_spacing() {
        let req = |duration_ms| ProbeRequest {
            target_kbps: 40_000,
            duration_ms,
        };
        assert!(is_ramp_step(&req(25), true));
        assert!(is_ramp_step(&req(super::stream::RAMP_STEP_MAX_MS), true));
        assert!(!is_ramp_step(
            &req(super::stream::RAMP_STEP_MAX_MS + 1),
            true
        ));
        assert!(!is_ramp_step(&req(800), true));
        assert!(!is_ramp_step(&req(25), false));
    }
}
