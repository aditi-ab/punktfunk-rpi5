//! The client worker: QUIC handshake + control/input/datagram tasks + the blocking data-plane pump.

use super::frame_channel::{
    StandingLatAction, StandingLatency, CLOCK_RESYNC_INTERVAL, FLUSH_AFTER, FLUSH_COOLDOWN,
    FLUSH_LATENCY, NOOP_CLOCK_FLUSHES_TO_DISARM, NOOP_FLUSH_DATAGRAMS, QUEUE_HIGH, QUEUE_LOW,
    STANDING_TIME,
};
use super::worker::reject_from_close;
use super::*;
use crate::abr::BitrateController;
use crate::config::Role;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    io, wall_clock_ns, window_loss_ppm, BitrateChanged, ClipState, ClockEcho, ClockResync, Hello,
    LossReport, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, ResyncAdmit, ResyncGuard,
    ResyncStep, SetBitrate, Start, Welcome,
};
use crate::session::Session;
use crate::transport::UdpTransport;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

mod control_task;
mod data;
mod datagram_task;
mod handshake;
mod input_task;

pub(super) async fn run_pump(args: WorkerArgs) {
    let hs = match handshake::connect_and_handshake(&args).await {
        Ok(hs) => hs,
        Err(e) => {
            let _ = args.ready_tx.send(Err(e));
            return;
        }
    };
    let handshake::HandshakeOut {
        conn,
        ep,
        session,
        ctrl_send,
        ctrl_recv,
        negotiated,
        host_caps,
    } = hs;
    let WorkerArgs {
        bitrate_kbps,
        frames,
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        hdr_meta_tx,
        host_timing_tx,
        cursor_shape_tx,
        cursor_state_tx,
        input_rx,
        mut mic_rx,
        mut rich_input_rx,
        ctrl_rx,
        ctrl_tx,
        clip_event_tx,
        clip_cmd_rx,
        ready_tx,
        shutdown,
        quit,
        mode_slot,
        probe,
        frames_dropped,
        fec_recovered,
        mic_stats,
        hot_tids,
        clock_offset,
        decode_lat,
        ..
    } = args;
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

    // Input task: embedder events → uplink datagrams, with per-transition gamepad events
    // folded into idempotent seq-stamped snapshots toward a HOST_CAP_GAMEPAD_STATE host
    // (see [`input_task`]).
    let gamepad_snapshots = host_caps & crate::quic::HOST_CAP_GAMEPAD_STATE != 0;
    tokio::spawn(input_task::run(conn.clone(), input_rx, gamepad_snapshots));

    // Mic task: embedder Opus mic frames → 0xCB uplink datagrams (best-effort, dropped on loss).
    // Self-healing latency bound: every frame still queued once this task catches up is standing
    // mic delay from then on, so a frame with more than [`MIC_BACKLOG_MAX`] successors already
    // waiting is shed as stale instead of sent — a stall costs a short dropout, not a
    // session-long lag (see [`MIC_QUEUE`]). The counters feed [`NativeClient::mic_stats`].
    let mic_conn = conn.clone();
    tokio::spawn(async move {
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            if mic_rx.len() > MIC_BACKLOG_MAX {
                mic_stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d.into());
            mic_stats.sent.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Rich-input task: pre-encoded 0xCC uplink datagrams (DualSense touchpad / motion, pen
    // batches — encoded at the NativeClient surface so new plane kinds never touch the pump).
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(d) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(d.into());
        }
    });

    // Adaptive bitrate ack slot: the control task parks the latest BitrateChanged here; the
    // pump's controller drains it on its report tick (`take()` — an ack is consumed once).
    let bitrate_ack: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    // Decode-recovery keyframe asks (the ABR recovery signal): the control task counts every
    // outbound `CtrlRequest::Keyframe` — the one choke point all emitters funnel through — and
    // the pump drains the count per report window.
    let recovery_kf = Arc::new(AtomicU32::new(0));
    // Host-encode-latency accumulator (the ABR encode signal, see [`EncodeLatAcc`]): the
    // datagram task adds one sample per 0xCF; the pump drains a window mean per report tick.
    let encode_lat = Arc::new(Mutex::new(super::frame_channel::EncodeLatAcc::default()));
    // Bumped by the control task on every accepted mode switch (the `clock_gen` pattern): the
    // pump resets the controller's mode-scoped learned state (host cap, encode baseline).
    let mode_gen = Arc::new(AtomicU32::new(0));

    // Control task (see [`control_task`]): the handshake stream stays open for mid-stream
    // renegotiation, speed tests, clock re-sync, and clipboard metadata.
    tokio::spawn(
        control_task::ControlTask {
            ctrl_rx,
            ctrl_send,
            ctrl_recv,
            clock_rtt_ns,
            mode_slot,
            probe: probe.clone(),
            bitrate_ack: bitrate_ack.clone(),
            recovery_kf: recovery_kf.clone(),
            clock_offset: clock_offset.clone(),
            clock_gen: clock_gen.clone(),
            clip_event_tx: clip_event_tx.clone(),
            cursor_shape_tx,
            mode_gen: mode_gen.clone(),
        }
        .run(),
    );

    // Datagram demux (see [`datagram_task`]): host → client audio/rumble/HID/HDR/timing planes.
    tokio::spawn(datagram_task::run(
        conn.clone(),
        audio_tx,
        rumble_tx,
        rumble_feed,
        hidout_tx,
        hdr_meta_tx,
        host_timing_tx,
        encode_lat.clone(),
        cursor_state_tx,
    ));

    // Clipboard task: the fetch-stream accept loop (host pulls what we offered) + outbound fetches
    // (we pull what the host offered). Metadata (enable/offer/state) rides the control task above;
    // only bulk bytes flow here. Dies with the connection (accept_bi errors) or when the embedder
    // drops the command sender. Always spawned — a host without HOST_CAP_CLIPBOARD simply never
    // opens a clip stream, and our control-plane offers hit its "unknown message" arm harmlessly.
    tokio::spawn(crate::clipboard::run(
        conn.clone(),
        clip_event_tx,
        clip_cmd_rx,
    ));

    // Watch for connection close → stop the pump.
    {
        let shutdown = shutdown.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    // Data-plane pump on a blocking thread (see [`data::DataPump`]).
    let pump = data::DataPump {
        session,
        frames,
        ctrl_tx,
        shutdown,
        probe,
        hot_tids,
        clock_offset,
        clock_gen,
        decode_lat,
        encode_lat,
        mode_gen,
        frames_dropped,
        fec_recovered,
        bitrate_ack,
        recovery_kf,
        bitrate_kbps,
        resolved_bitrate_kbps,
        negotiated_codec,
    };
    let _ = tokio::task::spawn_blocking(move || pump.run()).await;

    // Deliberate quit (a user "stop") closes with the quit code → the host skips the keep-alive
    // linger; a plain drop / disconnect closes with 0 → the host lingers so a reconnect can resume.
    let close_code = if quit.load(Ordering::SeqCst) {
        crate::quic::QUIT_CLOSE_CODE
    } else {
        0
    };
    conn.close(close_code.into(), b"client closed");
    // Flush the CONNECTION_CLOSE before the runtime is dropped (the same discipline as the pairing
    // + probe paths). `close` only queues the frame — the endpoint driver puts it on the wire, and
    // this fn is the body of a `block_on` whose runtime is dropped the instant it returns, so
    // without this the driver could simply never be polled again. The host then saw a deliberate
    // quit as silence: no `QUIT_CLOSE_CODE`, an 8 s idle timeout, and the keep-alive linger meant
    // for an UNWANTED disconnect. Bounded — a host already gone must not delay the client's exit.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), ep.wait_idle()).await;
}
