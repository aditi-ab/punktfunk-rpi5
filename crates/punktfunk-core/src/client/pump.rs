//! The client worker: QUIC handshake + control/input/datagram tasks + the blocking data-plane pump.

use super::frame_channel::{
    CLOCK_RESYNC_INTERVAL, FLUSH_AFTER_FRAMES, FLUSH_COOLDOWN, FLUSH_LATENCY,
    NOOP_CLOCK_FLUSHES_TO_DISARM, NOOP_FLUSH_DATAGRAMS, QUEUE_HIGH, QUEUE_LOW, STANDING_FRAMES,
};
use super::worker::reject_from_close;
use super::*;
use crate::abr::BitrateController;
use crate::config::{CompositorPref, GamepadPref, Role};
use crate::error::PunktfunkError;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    accept_resync, endpoint, io, wall_clock_ns, window_loss_ppm, BitrateChanged, ClockEcho,
    ClockResync, Hello, HidOutput, LossReport, ProbeRequest, ProbeResult, Reconfigure,
    Reconfigured, RequestKeyframe, ResyncStep, SetBitrate, Start, Welcome,
};
use crate::session::Session;
use crate::transport::UdpTransport;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) async fn run_pump(args: WorkerArgs) {
    let WorkerArgs {
        host,
        port,
        mode,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        display_hdr,
        launch,
        pin,
        identity,
        frames,
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        hdr_meta_tx,
        host_timing_tx,
        mut input_rx,
        mut mic_rx,
        mut rich_input_rx,
        mut ctrl_rx,
        ctrl_tx,
        ready_tx,
        shutdown,
        quit,
        mode_slot,
        probe,
        frames_dropped,
        fec_recovered,
        hot_tids,
        clock_offset,
        decode_lat,
    } = args;
    let setup = async {
        let remote: std::net::SocketAddr = join_host_port(&host, port)
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;
        let (ep, observed) = endpoint::client_pinned_with_identity(
            pin,
            identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        );
        let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        let conn = ep
            .connect(remote, "punktfunk")
            .map_err(|_| PunktfunkError::InvalidArg("connect"))?
            .await
            .map_err(|e| {
                // A pin mismatch surfaces as a TLS failure; report it as a crypto error so
                // the embedder can distinguish "wrong host identity" from plain IO trouble.
                let fp_mismatch = pin.is_some()
                    && observed.lock().unwrap().map(|fp| Some(fp) != pin) == Some(true);
                if fp_mismatch {
                    PunktfunkError::Crypto
                } else {
                    PunktfunkError::Io(std::io::Error::other(e.to_string()))
                }
            })?;
        let fingerprint = observed.lock().unwrap().unwrap_or([0u8; 32]);
        // The rest of the handshake runs in an inner future so a failure can consult
        // `conn.close_reason()`: a host that turned us away with a typed application close
        // (pairing not armed / denied / approval timeout / version mismatch / busy) surfaces
        // as `PunktfunkError::Rejected` instead of the generic transport error the failed
        // read produces — the difference between "not accepted" and the actual cause.
        let handshake = async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

            io::write_msg(
                &mut send,
                &Hello {
                    abi_version: crate::WIRE_VERSION,
                    mode,
                    compositor,
                    gamepad,
                    bitrate_kbps,
                    // No device name yet: the connect ABI has no name parameter (pairing does). The
                    // host falls back to a fingerprint-derived label in its pending-approval list.
                    name: None,
                    // Library id to launch this session, if the embedder asked for one.
                    launch: launch.clone(),
                    // The embedder's decode/present caps (e.g. the Windows client advertises
                    // VIDEO_CAP_10BIT | VIDEO_CAP_HDR). The host only upgrades to a 10-bit / HDR encode
                    // when the matching bit is set, so `0` stays an 8-bit BT.709 stream. HOST_TIMING is
                    // OR'd in unconditionally: every NativeClient build demuxes the 0xCF plane, and the
                    // bit only asks the host for observability datagrams (never changes the encode).
                    // PROBE_SEQ likewise: the shared reassembler keeps probe filler in its own window
                    // (every embedder inherits it), so the host may burst speed tests without consuming
                    // video frame indexes.
                    video_caps: video_caps
                        | crate::quic::VIDEO_CAP_HOST_TIMING
                        | crate::quic::VIDEO_CAP_PROBE_SEQ,
                    // Requested surround channel count; the host echoes the resolved value in Welcome.
                    audio_channels,
                    // The codecs this client can decode + its soft preference (0 = auto). The host
                    // resolves the emitted codec from these and reports it in `Welcome::codec`.
                    video_codecs,
                    preferred_codec,
                    // The client display's HDR volume → the host's virtual-display EDID (host apps
                    // tone-map to the client's real panel). `None` = unknown/SDR.
                    display_hdr,
                }
                .encode(),
            )
            .await?;
            let welcome = Welcome::decode(&io::read_msg(&mut recv).await?)?;
            if welcome.compositor != CompositorPref::Auto {
                tracing::info!(
                    compositor = welcome.compositor.as_str(),
                    "host resolved compositor"
                );
            }
            if welcome.gamepad != GamepadPref::Auto {
                tracing::info!(
                    gamepad = welcome.gamepad.as_str(),
                    "host resolved gamepad backend"
                );
            }

            // Reserve our data-plane port, then start the host.
            let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
            let udp_port = probe.local_addr()?.port();
            drop(probe);
            io::write_msg(
                &mut send,
                &Start {
                    client_udp_port: udp_port,
                }
                .encode(),
            )
            .await?;

            // Wall-clock skew handshake on the control stream (before the session's control task takes
            // it): align our clock to the host's so the embedder can express receive/present instants in
            // the host's capture clock (the AU `pts_ns`). 0 ⇒ an old host that didn't answer (shared-clock
            // assumption, as before). This is the substrate for glass-to-glass present-time measurement.
            let (clock_offset_ns, clock_rtt_ns) =
                match crate::quic::clock_sync(&mut send, &mut recv).await {
                    Some(skew) => {
                        tracing::info!(
                            offset_ns = skew.offset_ns,
                            rtt_us = skew.rtt_ns / 1000,
                            rounds = skew.rounds,
                            "clock skew estimated (host-client)"
                        );
                        (skew.offset_ns, Some(skew.rtt_ns))
                    }
                    None => (0, None),
                };

            let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
            let transport =
                UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())?;
            // Hole-punch the host's data port so video traverses a NAT / stateful inter-VLAN firewall
            // (control + side planes ride the client-initiated QUIC; the raw video UDP needs the client
            // to open the path first). Stops with the session via the shared shutdown flag.
            if let Ok(sock) = transport.try_clone_socket() {
                crate::transport::spawn_data_punch(sock, shutdown.clone());
            }
            let mut session =
                Session::new(welcome.session_config(Role::Client), Box::new(transport))?;
            // PyroWave sessions opt into partial delivery (plan §4.4): an aged-out lossy
            // frame arrives as blocks-with-holes instead of vanishing — the all-intra codec
            // renders it as one frame of localized blur, strictly better than a freeze.
            if welcome.codec == crate::quic::CODEC_PYROWAVE {
                session.set_deliver_partial_frames(true);
            }
            Ok::<_, PunktfunkError>((
                session,
                send,
                recv,
                Negotiated {
                    mode: welcome.mode,
                    compositor: welcome.compositor,
                    gamepad: welcome.gamepad,
                    host_fingerprint: fingerprint,
                    bitrate_kbps: welcome.bitrate_kbps,
                    clock_offset_ns,
                    clock_rtt_ns,
                    bit_depth: welcome.bit_depth,
                    color: welcome.color,
                    chroma_format: welcome.chroma_format,
                    audio_channels: welcome.audio_channels,
                    codec: welcome.codec,
                    shard_payload: welcome.shard_payload,
                },
                welcome.host_caps,
            ))
        };
        match handshake.await {
            Ok((session, send, recv, negotiated, host_caps)) => {
                Ok((conn, session, send, recv, negotiated, host_caps))
            }
            Err(e) => Err(match reject_from_close(&conn) {
                Some(r) => PunktfunkError::Rejected(r),
                None => e,
            }),
        }
    };

    let (conn, mut session, mut ctrl_send, mut ctrl_recv, negotiated, host_caps) = match setup.await
    {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    // Copies the pump needs after `negotiated` is handed over to `connect`.
    let clock_rtt_ns = negotiated.clock_rtt_ns;
    let resolved_bitrate_kbps = negotiated.bitrate_kbps;
    let negotiated_codec = negotiated.codec;
    // Seed the live offset with the connect-time estimate BEFORE the embedder can observe the
    // client (ready_tx): clock_offset_now_ns() never reads a pre-handshake 0 on a skewed pair.
    clock_offset.store(negotiated.clock_offset_ns, Ordering::Relaxed);
    // Bumped by the control task each time a re-sync batch is APPLIED; the pump watches it to
    // reset its staleness counters and re-arm the clock-based jump-to-live detector.
    let clock_gen = Arc::new(AtomicU32::new(0));
    let _ = ready_tx.send(Ok(negotiated));

    // Input task: embedder events → QUIC datagrams. Toward a host that advertised
    // HOST_CAP_GAMEPAD_STATE, the per-transition gamepad events every embedder still emits are
    // folded into idempotent, sequence-numbered full-state snapshots (`GamepadSnapshot`): the
    // datagram plane drops and reorders (and sheds oldest-first at the 4 KiB send cap), so a lost
    // per-transition event would corrupt held pad state until the *next* change — a held trigger
    // stuck wrong indefinitely. Snapshots heal on the next send, the seq lets the host drop stale
    // reorders, and a periodic refresh of every touched pad bounds any loss to one refresh
    // interval — the same idempotent-state discipline as the host's 500 ms rumble refresh.
    // Keyboard/mouse/touch events pass through unchanged; an older host (no caps bit) keeps
    // getting the legacy per-transition gamepad events.
    let input_conn = conn.clone();
    let gamepad_snapshots = host_caps & crate::quic::HOST_CAP_GAMEPAD_STATE != 0;
    tokio::spawn(async move {
        use crate::input::{GamepadSnapshot, InputKind, MAX_PADS};
        // Touched pads only: an entry appears on the first gamepad event for that index, so the
        // refresh never conjures a virtual pad the embedder didn't drive.
        let mut pads: [Option<GamepadSnapshot>; MAX_PADS] = [None; MAX_PADS];
        // Per-pad wrapping seq that PERSISTS across a pad's remove/re-add on the same index (the
        // snapshot itself is cleared to `None` on removal). A removal takes `seq[idx] + 1` so it
        // supersedes every prior snapshot; the re-added pad's first snapshot takes the next value
        // after that, so the host's seq gate accepts it instead of rejecting a restarted-at-0 seq.
        let mut seq: [u8; MAX_PADS] = [0; MAX_PADS];
        // Re-sends of a removal still owed on refresh ticks (the removal rides the lossy datagram
        // plane; a single lost one would silently strand a ghost pad on the host — the exact bug
        // the removal fixes). Mirrors the host's rumble stop burst: a few time-spread re-sends,
        // each with a fresh (higher) seq, and canceled the moment the pad is driven again.
        const REMOVE_RESENDS: u8 = 2;
        let mut remove_owed: [u8; MAX_PADS] = [0; MAX_PADS];
        // Per-pad declared controller kind ([`GamepadArrival`]) + its owed re-sends: the host needs
        // the kind before the pad's first frame to build a matching virtual device (mixed types), so
        // like the removal it rides the lossy plane with a small time-spread re-send burst.
        const ARRIVAL_RESENDS: u8 = 2;
        let mut arrival: [Option<u8>; MAX_PADS] = [None; MAX_PADS];
        let mut arrival_owed: [u8; MAX_PADS] = [0; MAX_PADS];
        let mut refresh = tokio::time::interval(Duration::from_millis(100));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = input_rx.recv() => {
                    let Some(ev) = ev else { break };
                    let idx = ev.flags as usize;
                    if gamepad_snapshots
                        && matches!(ev.kind, InputKind::GamepadButton | InputKind::GamepadAxis)
                        && idx < MAX_PADS
                    {
                        // The pad is being driven — cancel any owed removal (a re-plug on this
                        // index; its fresh snapshot seq already supersedes the removal's).
                        remove_owed[idx] = 0;
                        let snap = pads[idx].get_or_insert(GamepadSnapshot {
                            pad: idx as u8,
                            ..Default::default()
                        });
                        // Unknown axis ids don't send (the host's legacy fold drops them too).
                        if snap.fold(&ev) {
                            seq[idx] = seq[idx].wrapping_add(1);
                            snap.seq = seq[idx];
                            let _ = input_conn
                                .send_datagram(snap.to_event().encode().to_vec().into());
                        }
                        continue;
                    }
                    if gamepad_snapshots && ev.kind == InputKind::GamepadRemove && idx < MAX_PADS {
                        // Stop refreshing the pad and forward a seq-stamped removal (in the shared
                        // seq space) so the host tears its virtual device down and no reordered
                        // snapshot can resurrect it; arm the re-send burst against datagram loss.
                        // Drop any owed kind declaration too — a re-plug on this index sends its own.
                        pads[idx] = None;
                        arrival[idx] = None;
                        arrival_owed[idx] = 0;
                        seq[idx] = seq[idx].wrapping_add(1);
                        remove_owed[idx] = REMOVE_RESENDS;
                        let rem = crate::input::InputEvent {
                            flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                            ..ev
                        };
                        let _ = input_conn.send_datagram(rem.encode().to_vec().into());
                        continue;
                    }
                    if gamepad_snapshots && ev.kind == InputKind::GamepadArrival && idx < MAX_PADS {
                        // Remember the declared kind (`code`) and forward it, arming a re-send burst
                        // so the host learns it before the pad's first frame even under loss.
                        arrival[idx] = Some(ev.code as u8);
                        arrival_owed[idx] = ARRIVAL_RESENDS;
                        let _ = input_conn.send_datagram(ev.encode().to_vec().into());
                        continue;
                    }
                    let _ = input_conn.send_datagram(ev.encode().to_vec().into());
                }
                _ = refresh.tick() => {
                    for idx in 0..MAX_PADS {
                        // Re-send an owed kind declaration (independent of whether the pad has state
                        // yet — it may be idle-but-connected). Idempotent on the host.
                        if arrival_owed[idx] > 0 {
                            if let Some(kind) = arrival[idx] {
                                arrival_owed[idx] -= 1;
                                let arr = crate::input::InputEvent {
                                    kind: InputKind::GamepadArrival,
                                    _pad: [0; 3],
                                    code: kind as u32,
                                    x: 0,
                                    y: 0,
                                    flags: idx as u32,
                                };
                                let _ = input_conn.send_datagram(arr.encode().to_vec().into());
                            } else {
                                arrival_owed[idx] = 0;
                            }
                        }
                        if let Some(snap) = pads[idx].as_mut() {
                            seq[idx] = seq[idx].wrapping_add(1);
                            snap.seq = seq[idx];
                            let _ = input_conn.send_datagram(snap.to_event().encode().to_vec().into());
                        } else if remove_owed[idx] > 0 {
                            // Idempotent removal re-send with a fresh seq (the host drops it as a
                            // no-op once the pad is already gone, but a re-plug's later snapshot
                            // still wins by seq).
                            remove_owed[idx] -= 1;
                            seq[idx] = seq[idx].wrapping_add(1);
                            let rem = crate::input::InputEvent {
                                kind: InputKind::GamepadRemove,
                                _pad: [0; 3],
                                code: 0,
                                x: 0,
                                y: 0,
                                flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                            };
                            let _ = input_conn.send_datagram(rem.encode().to_vec().into());
                        }
                    }
                }
            }
        }
    });

    // Mic task: embedder Opus mic frames → 0xCB uplink datagrams (best-effort, dropped on loss).
    let mic_conn = conn.clone();
    tokio::spawn(async move {
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d.into());
        }
    });

    // Rich-input task: embedder DualSense touchpad / motion → 0xCC uplink datagrams.
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(rich) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(rich.encode().into());
        }
    });

    // Adaptive bitrate ack slot: the control task parks the latest BitrateChanged here; the
    // pump's controller drains it on its report tick (`take()` — an ack is consumed once).
    let bitrate_ack: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // Control task: the handshake stream stays open for mid-stream renegotiation + speed tests.
    // Outbound requests (mode switch, probe) and inbound replies (Reconfigured, ProbeResult) are
    // multiplexed with `select!`; a single outbound channel (`ctrl_rx`) keeps one writer so the
    // two `&mut ctrl_send` borrows don't collide across branches.
    {
        let mode_slot = mode_slot.clone();
        let probe = probe.clone();
        let bitrate_ack = bitrate_ack.clone();
        let clock_offset = clock_offset.clone();
        let clock_gen = clock_gen.clone();
        tokio::spawn(async move {
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
                    msg = io::read_msg(&mut ctrl_recv) => {
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
                                        clock_offset.store(offset_ns, Ordering::Relaxed);
                                        clock_gen.fetch_add(1, Ordering::Relaxed);
                                        tracing::debug!(
                                            offset_ns,
                                            rtt_us = rtt_ns / 1000,
                                            "mid-stream clock re-sync applied"
                                        );
                                    } else {
                                        tracing::debug!(
                                            rtt_us = rtt_ns / 1000,
                                            "clock re-sync batch discarded — RTT above the \
                                             connect-time baseline (congested window)"
                                        );
                                    }
                                }
                                ResyncStep::Idle => {}
                            }
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
        });
    }

    // Datagram demux: host → client audio/rumble (try_send: a lagging embedder drops the
    // newest packet rather than backing up the QUIC receive path).
    let dgram_conn = conn.clone();
    // Per-pad reorder gate for v2 rumble envelopes (the seq analog of the host's gamepad-state
    // gate): a datagram the network reordered must not roll a stopped motor back on. Legacy v1
    // datagrams carry no seq and bypass it (an old host's own periodic re-send is the only heal).
    let mut rumble_last_seq: [Option<u8>; crate::input::MAX_PADS] = [None; crate::input::MAX_PADS];
    tokio::spawn(async move {
        while let Ok(d) = dgram_conn.read_datagram().await {
            match d.first() {
                Some(&crate::quic::AUDIO_MAGIC) => {
                    if let Some((seq, pts_ns, opus)) = crate::quic::decode_audio_datagram(&d) {
                        let _ = audio_tx.try_send(AudioPacket {
                            seq,
                            pts_ns,
                            data: opus.to_vec(),
                        });
                    }
                }
                Some(&crate::quic::RUMBLE_MAGIC) => {
                    if let Some(u) = crate::quic::decode_rumble_envelope(&d) {
                        // Gate v2 envelopes on their per-pad seq; forward v1 (envelope: None) as-is.
                        let fresh = match u.envelope {
                            Some(env) => {
                                let idx = u.pad as usize;
                                if idx < crate::input::MAX_PADS {
                                    if crate::input::GamepadSnapshot::seq_newer(
                                        env.seq,
                                        rumble_last_seq[idx],
                                    ) {
                                        rumble_last_seq[idx] = Some(env.seq);
                                        true
                                    } else {
                                        false // reordered/duplicate — drop, keep the newer state
                                    }
                                } else {
                                    true // out-of-range pad (host never sends these): no gate
                                }
                            }
                            None => true,
                        };
                        if fresh {
                            let ttl = u.envelope.map(|e| e.ttl_ms);
                            // Both consumers are fed; an embedder drains exactly one of them
                            // (the legacy queue, or the policy engine's command API).
                            let _ = rumble_tx.try_send((u.pad, u.low, u.high, ttl));
                            rumble_feed.wire_update(u.pad, u.low, u.high, ttl);
                        }
                    }
                }
                Some(&crate::quic::HIDOUT_MAGIC) => {
                    if let Some(h) = HidOutput::decode(&d) {
                        let _ = hidout_tx.try_send(h);
                    }
                }
                Some(&crate::quic::HDR_META_MAGIC) => {
                    if let Some(m) = crate::quic::decode_hdr_meta_datagram(&d) {
                        let _ = hdr_meta_tx.try_send(m);
                    }
                }
                Some(&crate::quic::HOST_TIMING_MAGIC) => {
                    if let Some(t) = crate::quic::decode_host_timing_datagram(&d) {
                        let _ = host_timing_tx.try_send(t);
                    }
                }
                _ => {} // unknown tag — a newer host; ignore
            }
        }
    });

    // Watch for connection close → stop the pump.
    {
        let shutdown = shutdown.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    // Data-plane pump on a blocking thread: poll the session, hand frames to the embedder.
    // try_send drops the newest frame when the embedder lags (freshness over completeness).
    // Speed-test filler ([`FLAG_PROBE`]) is folded into the probe accumulator instead of the
    // decoder queue — it isn't video.
    let pump_shutdown = shutdown.clone();
    let pump_probe = probe.clone();
    let pump_hot_tids = hot_tids.clone();
    let pump_clock_offset = clock_offset.clone();
    let pump_clock_gen = clock_gen.clone();
    let pump_decode_lat = decode_lat.clone();
    let _ = tokio::task::spawn_blocking(move || {
        pin_thread_user_interactive(); // feeds the frame channel → the user-interactive video pump
        register_hot_tid(&pump_hot_tids); // this thread does UDP receive + FEC reassembly — hint it
                                          // Adaptive-FEC loss reporting: every ADAPT_REPORT_INTERVAL, report the loss observed over the
                                          // window (shards FEC recovered, plus a bump if any frame went unrecoverable) so the host can
                                          // size FEC to the link. Suppressed during a speed test (its FLAG_PROBE filler would skew it).
        const ADAPT_REPORT_INTERVAL: Duration = Duration::from_millis(750);
        let mut last_report = Instant::now();
        let (mut last_recovered, mut last_late, mut last_received, mut last_dropped, mut last_bytes) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
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
        let (mut owd_sum_ns, mut owd_frames) = (0i128, 0u32);
        let mut flush_in_window = false;
        // Jump-to-live state (see the guard in the loop below): the clock-based over-bound run
        // (`stale_frames`, armed only when the skew handshake succeeded so the clocks are comparable),
        // the clock-free non-draining-queue run (`standing_frames`), and the last-jump instant for the
        // shared cooldown.
        let mut stale_frames: u32 = 0;
        let mut standing_frames: u32 = 0;
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
        while !pump_shutdown.load(Ordering::SeqCst) {
            // The live host↔client offset: re-loaded every iteration so an applied mid-stream
            // re-sync takes effect on the very next frame's latency math.
            let clock_offset_ns = pump_clock_offset.load(Ordering::Relaxed);
            // An applied re-sync invalidates the staleness run measured under the OLD offset:
            // reset the counters and re-arm the clock-based detector if a step had disarmed it.
            let gen = pump_clock_gen.load(Ordering::Relaxed);
            if gen != seen_clock_gen {
                seen_clock_gen = gen;
                stale_frames = 0;
                noop_clock_flushes = 0;
                if !clock_detector_armed {
                    clock_detector_armed = true;
                    tracing::info!(
                        "clock re-sync applied — clock-based jump-to-live re-armed"
                    );
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
            // Fire the startup link-capacity probe once the stream has settled (see the constants
            // above), and fold its measurement into the ABR ceiling when the result lands.
            if let Some(at) = capacity_probe_at {
                if Instant::now() >= at {
                    capacity_probe_at = None;
                    *pump_probe.lock().unwrap() = ProbeState {
                        active: true,
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
                let window_dropped = st.frames_dropped.wrapping_sub(last_dropped);
                let loss_ppm = window_loss_ppm(
                    st.fec_recovered_shards.wrapping_sub(last_recovered),
                    st.fec_late_shards.wrapping_sub(last_late),
                    st.packets_received.wrapping_sub(last_received),
                    window_dropped,
                );
                let _ = ctrl_tx.try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                // Adaptive bitrate: drain any host ack first (its clamp is authoritative), then
                // feed the controller this window's congestion signals; a decision becomes a
                // SetBitrate on the control stream.
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
                // The window's ACTUAL delivered throughput — what the pipeline really carried, vs
                // the target it was allowed. Wire bytes (headers + FEC) slightly overstate the
                // media rate the decoder ingests; acceptable for the climb gate / proven-mark
                // semantics (both compare against targets with their own headroom).
                let window_ms = last_report.elapsed().as_millis().max(1) as u64;
                let actual_kbps =
                    (st.bytes_received.wrapping_sub(last_bytes).saturating_mul(8) / window_ms)
                        as u32;
                if let Some(kbps) = abr.on_window(
                    Instant::now(),
                    window_dropped,
                    loss_ppm,
                    owd_mean_us,
                    decode_mean_us,
                    actual_kbps,
                    flush_in_window,
                ) {
                    // Log the window's signals alongside the decision so an on-glass session can
                    // tell a decode-driven re-target (the new signal — decode_mean_us elevated with
                    // loss/OWD flat) from a network-driven one.
                    tracing::info!(
                        kbps,
                        loss_ppm,
                        owd_mean_us = owd_mean_us.unwrap_or(-1),
                        decode_mean_us = decode_mean_us.unwrap_or(-1),
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
                                arrivals_us.push((now - prev).as_micros().min(u32::MAX as u128)
                                    as u32);
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
                    //    capture clock for FLUSH_AFTER_FRAMES straight. Needs the skew handshake, and
                    //    also catches kernel/reassembler backlog the hand-off queue hasn't reached yet.
                    //  * clock-free — the pre-decode hand-off queue stopped draining: its depth stayed
                    //    ≥ QUEUE_HIGH (never falling to QUEUE_LOW) for STANDING_FRAMES straight. Works
                    //    with no handshake / a same-clock session (where the clock path is disarmed),
                    //    and is the direct signal that the embedder can't keep up. A transient Wi-Fi
                    //    clump drains in a few frames and never reaches the count.
                    if probe_active {
                        // Keep both detectors disarmed across a speed test so its (deliberately)
                        // saturated queue doesn't leave a primed count that fires the moment it ends.
                        stale_frames = 0;
                        standing_frames = 0;
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
                        }
                        if clock_detector_armed
                            && clock_offset_ns != 0
                            && lat_ns > FLUSH_LATENCY.as_nanos() as i128
                        {
                            stale_frames += 1;
                        } else {
                            stale_frames = 0;
                        }
                        let depth = frames.depth();
                        if depth >= QUEUE_HIGH {
                            standing_frames += 1;
                        } else if depth <= QUEUE_LOW {
                            standing_frames = 0;
                        }
                        let clock_behind = stale_frames >= FLUSH_AFTER_FRAMES;
                        let queue_behind = standing_frames >= STANDING_FRAMES;
                        if (clock_behind || queue_behind)
                            && last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN)
                        {
                            stale_frames = 0;
                            standing_frames = 0;
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
                            if clock_behind && !queue_behind
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
    })
    .await;

    // Deliberate quit (a user "stop") closes with the quit code → the host skips the keep-alive
    // linger; a plain drop / disconnect closes with 0 → the host lingers so a reconnect can resume.
    let close_code = if quit.load(Ordering::SeqCst) {
        crate::quic::QUIT_CLOSE_CODE
    } else {
        0
    };
    conn.close(close_code.into(), b"client closed");
}
