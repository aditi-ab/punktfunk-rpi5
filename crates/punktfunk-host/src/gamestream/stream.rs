//! The video data plane: on RTSP PLAY, learn the client's UDP endpoint (it pings the video
//! port), then run capture → NVENC encode → [`VideoPacketizer`] → UDP send. The source is
//! either real portal desktop capture (`PUNKTFUNK_VIDEO_SOURCE=portal`, the portal PipeWire path) or
//! a synthetic test pattern (default). Runs on its own native thread.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::video::{FrameType, VideoPacketizer};
use super::VIDEO_PORT;
use crate::capture::{self, Capturer, FastSyntheticCapturer};
use crate::encode::{self, Codec};
use anyhow::{Context, Result};
use rand::Rng;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Negotiated video parameters from the RTSP ANNOUNCE.
#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub packet_size: usize,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// Client's `x-nv-vqos[0].fec.minRequiredFecPackets` — parity floor per FEC block.
    pub min_fec: u8,
}

/// Slot for the persistent screen capturer, shared with the control plane and reused across
/// streams so a reconnect doesn't open a second (conflicting) screencast session.
pub type CapturerSlot = Arc<std::sync::Mutex<Option<Box<dyn Capturer>>>>;

/// A pending client reference-frame-invalidation range (lost `firstFrame..=lastFrame`), set by the
/// control plane and drained by the video thread (see [`AppState::rfi_range`](super::AppState)).
pub type RfiSlot = Arc<std::sync::Mutex<Option<(i64, i64)>>>;

/// Spawn the video stream thread (idempotent via `running`). Stops when `running` clears.
/// `force_idr` is set by the control stream on a client recovery request; `video_cap` holds
/// the persistent capturer the thread borrows for the stream's duration.
pub fn start(
    cfg: StreamConfig,
    app: Option<super::apps::AppEntry>,
    running: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    rfi_range: RfiSlot,
    video_cap: CapturerSlot,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
) {
    let _ = std::thread::Builder::new()
        .name("punktfunk-video".into())
        .spawn(move || {
            tracing::info!(?cfg, "video stream starting");
            if let Err(e) = run(
                cfg,
                app.as_ref(),
                &running,
                &force_idr,
                &rfi_range,
                &video_cap,
                &stats,
            ) {
                tracing::error!(error = %format!("{e:#}"), "video stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("video stream stopped");
        });
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    video_cap: &std::sync::Mutex<Option<Box<dyn Capturer>>>,
    // Shared stats recorder for the web-console capture/graph. Threaded into `stream_body` (the
    // encode loop); per-frame sample emission is wired by a later pass.
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
) -> Result<()> {
    // GameStream capture/encode thread: apply Windows session tuning (no-op off Windows).
    crate::session_tuning::on_hot_thread();
    // Reject an out-of-range client mode before allocating capture/encode buffers.
    encode::validate_dimensions(cfg.codec, cfg.width, cfg.height)
        .context("client-requested video mode")?;
    let sock = UdpSocket::bind(("0.0.0.0", VIDEO_PORT)).context("bind video UDP")?;
    // Grow SO_SNDBUF/RCVBUF (avoid host-side ENOBUFS at high bitrate) like the native plane, and
    // opt-in DSCP/QoS-tag this as the video class (PUNKTFUNK_DSCP=1).
    punktfunk_core::transport::grow_socket_buffers(&sock);
    punktfunk_core::transport::set_media_qos(&sock, punktfunk_core::transport::MediaClass::Video);
    // The client pings the video port so we learn where to send; it re-pings until video
    // flows, so a missed early ping is fine.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    tracing::info!(
        port = VIDEO_PORT,
        "video: awaiting client ping to learn endpoint"
    );
    let mut probe = [0u8; 256];
    let (_, client) = sock
        .recv_from(&mut probe)
        .context("video: no client ping within 10s")?;
    sock.connect(client)
        .context("connect client video endpoint")?;
    tracing::info!(%client, "video: client endpoint learned");
    // Short label for web-console stats captures: the client's peer IP.
    let client_label = client.ip().to_string();

    // Native client-resolution source: create a compositor virtual output sized to the client's
    // request and capture it (no scaling). Self-contained — deliberately NOT pooled in
    // `video_cap`, since a reconnect at a different resolution needs a freshly-sized output; the
    // output is released when this capturer drops at stream end (RAII via its keepalive).
    if crate::config::config().video_source.as_deref() == Some("virtual") {
        // Open the virtual-display source: pick the live compositor, normalize the session env
        // (apply_session_env/apply_input_env — gamescope ATTACH/resize + KWin/Mutter retargeting,
        // exactly like the native plane), create a virtual output at the client mode, and capture it.
        // Re-runnable: the encode loop calls it again on a mid-stream capture loss to FOLLOW a
        // Desktop<->Game switch.
        let (mut capturer, compositor) = open_gs_virtual_source(cfg, app)?;
        tracing::info!(
            ?compositor,
            app = ?app.map(|a| &a.title),
            w = cfg.width,
            h = cfg.height,
            "video source: virtual display (native client resolution)"
        );
        // Launch the app's command now that capture is live, for the backends that DON'T nest it via
        // set_launch_command above: Windows (no gamescope) and Linux kwin/mutter/wlroots (which stream
        // the existing desktop, so the app must be spawned into the session to land on the streamed
        // output). Linux gamescope already nested it via set_launch_command, so skip it there.
        #[cfg(windows)]
        let launch_here = true;
        #[cfg(target_os = "linux")]
        let launch_here = compositor != crate::vdisplay::Compositor::Gamescope;
        #[cfg(any(windows, target_os = "linux"))]
        if launch_here {
            if let Some(cmd) = app
                .and_then(|a| a.cmd.as_deref())
                .filter(|c| !c.trim().is_empty())
            {
                if let Err(e) = crate::library::launch_gamestream_command(cmd) {
                    tracing::warn!(command = %cmd, error = %e, "gamestream: could not launch app");
                }
            }
        }
        // Rebuild closure: re-open the source on a mid-stream capture loss, RE-DETECTING the live
        // compositor — so a Desktop<->Game switch (at the client's fixed mode) is FOLLOWED in place
        // without a Moonlight reconnect. (A resolution change can't be followed mid-stream on
        // GameStream — WxH is locked at ANNOUNCE — but a session toggle keeps the negotiated mode.)
        let rebuild = || open_gs_virtual_source(cfg, app).map(|(c, _)| c);
        return stream_body(
            &mut capturer,
            Some(&rebuild),
            &sock,
            cfg,
            running,
            force_idr,
            rfi_range,
            stats,
            &client_label,
        );
    }

    // Reuse the persistent capturer (one screencast session → clean reconnect); create it on
    // the first stream. Borrow it for this stream and return it on exit.
    let mut capturer: Box<dyn Capturer> = match video_cap.lock().unwrap().take() {
        Some(c) => {
            tracing::info!("video source: reusing capturer");
            c
        }
        None if crate::config::config().video_source.as_deref() == Some("portal") => {
            tracing::info!("video source: portal desktop capture");
            capture::open_portal_monitor().context("open portal capturer")?
        }
        None => {
            tracing::info!("video source: synthetic test pattern");
            Box::new(FastSyntheticCapturer::new(cfg.width, cfg.height))
        }
    };
    capturer.set_active(true);
    // Portal/synthetic source: no compositor virtual output to re-detect, so no rebuild closure.
    let result = stream_body(
        &mut capturer,
        None,
        &sock,
        cfg,
        running,
        force_idr,
        rfi_range,
        stats,
        &client_label,
    );
    capturer.set_active(false);
    *video_cap.lock().unwrap() = Some(capturer);
    result
}

/// Open the virtual-display video source for a GameStream session: pick the LIVE compositor + normalize
/// the session env (apply_session_env/apply_input_env — gamescope ATTACH/resize, KWin/Mutter
/// retargeting) exactly like the native plane (punktfunk1.rs resolve_compositor), create a virtual
/// output at the client's mode, and capture it. Returns the capturer (it owns the output's keepalive;
/// the stateless VirtualDisplay factory is dropped here) plus the resolved compositor. An apps.json
/// entry can PIN a compositor (skips the live detect/retarget). Re-run on a mid-stream capture loss to
/// FOLLOW a Desktop<->Game switch: it re-detects the now-live compositor and re-targets at it. Does NOT
/// launch the app (that happens once at stream start; a rebuild must not re-spawn it).
fn open_gs_virtual_source(
    cfg: StreamConfig,
    app: Option<&super::apps::AppEntry>,
) -> Result<(Box<dyn Capturer>, crate::vdisplay::Compositor)> {
    let compositor = if let Some(c) = app.and_then(|a| a.compositor) {
        c
    } else {
        let active = crate::vdisplay::detect_active_session();
        crate::vdisplay::apply_session_env(&active);
        let c = crate::vdisplay::compositor_for_kind(active.kind)
            .map(Ok)
            .unwrap_or_else(crate::vdisplay::detect)
            .context("detect compositor")?;
        crate::vdisplay::apply_input_env(c);
        c
    };
    let mut vd = crate::vdisplay::open(compositor).context("open virtual display")?;
    // Carry the resolved launch command on the backend instance (per-session) rather than a
    // process-global env var, so concurrent sessions can't stomp each other's launch target.
    vd.set_launch_command(app.and_then(|a| a.cmd.clone()));
    let vout = vd
        .create(punktfunk_core::Mode {
            width: cfg.width,
            height: cfg.height,
            refresh_hz: cfg.fps,
        })
        .context("create virtual output at client resolution")?;
    // want_hdr=false: GameStream HDR is not negotiated into StreamConfig here (the default WGC backend
    // still auto-detects HDR from the output colorspace; only the opt-in IDD-push path streams SDR).
    let capturer = capture::capture_virtual_output(
        vout,
        capture::OutputFormat::resolve(false),
        crate::session_plan::CaptureBackend::resolve(),
    )
    .context("capture virtual output")?;
    capturer.set_active(true);
    Ok((capturer, compositor))
}

/// One frame's packets, handed from the encode thread to the send thread.
type PacketBatch = Vec<Vec<u8>>;

/// Send `pkts` with as few syscalls as possible (`sendmmsg`, up to 64 per call). The socket is
/// connected, so no per-message address. Returns an error on the first send failure.
#[cfg(target_os = "linux")]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const CHUNK: usize = 64;
    let fd = sock.as_raw_fd();
    for chunk in pkts.chunks(CHUNK) {
        let mut iovs: Vec<libc::iovec> = chunk
            .iter()
            .map(|p| libc::iovec {
                iov_base: p.as_ptr() as *mut libc::c_void,
                iov_len: p.len(),
            })
            .collect();
        let mut hdrs: Vec<libc::mmsghdr> = iovs
            .iter_mut()
            .map(|iov| {
                // SAFETY: `libc::mmsghdr` is a plain `#[repr(C)]` struct of integers and raw
                // pointers, for which an all-zero bit pattern is valid (null pointers / zero
                // lengths); the fields we rely on (`msg_iov`, `msg_iovlen`) are overwritten on the
                // next two lines before the struct is handed to the kernel.
                let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                h.msg_hdr.msg_iov = iov;
                h.msg_hdr.msg_iovlen = 1;
                h
            })
            .collect();
        let mut off = 0usize;
        while off < hdrs.len() {
            // SAFETY: `fd` is `sock`'s live raw fd (`sock` outlives the call). `hdrs[off..]
            // .as_mut_ptr()` is a live slice of `(hdrs.len() - off)` `mmsghdr`s — exactly the count
            // passed — into which the kernel writes each `msg_len`. Each header's `msg_iov` points
            // into `iovs` (a local that outlives this call, with `msg_iovlen == 1` matching its one
            // entry) and each `iovec.iov_base` points into the `chunk` packet buffers (the caller's
            // `pkts`, alive for the call); the kernel only reads those payloads. Flags 0; the return
            // is error-/progress-checked before advancing `off`.
            let n = unsafe {
                libc::sendmmsg(fd, hdrs[off..].as_mut_ptr(), (hdrs.len() - off) as u32, 0)
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            off += n as usize;
        }
    }
    Ok(())
}

/// Windows: coalesce each paced burst's equal-size packets into `WSASendMsg(UDP_SEND_MSG_SIZE)`
/// super-buffers (UDP Send Offload — the Windows analogue of Linux GSO), so a 16-packet burst is one
/// syscall instead of 16. Reuses the proven core USO primitive; it returns how many leading packets
/// it sent, and we send any remainder (USO off via `PUNKTFUNK_GSO=0`, a size-mixed burst, or a
/// frame's short final packet) with a per-packet `send`. The socket is connected.
#[cfg(target_os = "windows")]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    let refs: Vec<&[u8]> = pkts.iter().map(|p| p.as_slice()).collect();
    let n = punktfunk_core::transport::send_uso_all(sock, &refs)?;
    for p in &pkts[n..] {
        sock.send(p)?;
    }
    Ok(())
}

/// Portable fallback (other non-Linux dev builds, e.g. macOS — GameStream hosting never ships there):
/// one syscall per packet.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn sendmmsg_all(sock: &UdpSocket, pkts: &[Vec<u8>]) -> std::io::Result<()> {
    for p in pkts {
        sock.send(p)?;
    }
    Ok(())
}

/// Dedicated send thread: one [`PacketBatch`] per frame arrives on `rx`; its packets go out in
/// `sendmmsg` chunks, paced so the frame's data spreads over ~3/4 of the frame interval
/// (microburst shaping at chunk granularity — a real link drops line-rate bursts; the encode
/// thread is never blocked by this). On send failure (client gone) it clears `running`.
fn spawn_sender(
    sock: UdpSocket,
    rx: std::sync::mpsc::Receiver<PacketBatch>,
    frame_interval: Duration,
    running: Arc<AtomicBool>,
    drop_pct: u32,
) -> Result<()> {
    std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn(move || {
            // GameStream send thread: Windows session tuning + MMCSS (no-op off Windows).
            crate::session_tuning::on_hot_thread();
            // Chunk pacing: 16 packets per burst, bursts spread across the send budget.
            const PACE_CHUNK: usize = 16;
            let budget = frame_interval.mul_f32(0.75);
            let mut rng = rand::thread_rng();
            let mut sent: u64 = 0;
            let mut dropped: u64 = 0;
            while let Ok(mut batch) = rx.recv() {
                if drop_pct > 0 {
                    batch.retain(|_| {
                        let keep = rng.gen_range(0..100) >= drop_pct;
                        if !keep {
                            dropped += 1;
                        }
                        keep
                    });
                }
                let n = batch.len();
                if n == 0 {
                    continue;
                }
                let per_chunk = budget.mul_f64((PACE_CHUNK as f64 / n as f64).min(1.0));
                let start = Instant::now();
                for (i, chunk) in batch.chunks(PACE_CHUNK).enumerate() {
                    if let Err(e) = sendmmsg_all(&sock, chunk) {
                        tracing::info!(error = %e, sent, "video: client unreachable — stopping stream");
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                    sent += chunk.len() as u64;
                    // Sleep toward the next chunk's deadline; skip sub-500µs sleeps (jitter).
                    let target = start + per_chunk.mul_f64((i + 1) as f64);
                    if let Some(ahead) = target.checked_duration_since(Instant::now()) {
                        if ahead >= Duration::from_micros(500) {
                            std::thread::sleep(ahead);
                        }
                    }
                }
            }
            tracing::debug!(sent, dropped, "video sender exiting");
        })
        .context("spawn send thread")?;
    Ok(())
}

/// Percentile of a slice (sorts it in place first). `q` in `0.0..=1.0`. Used for the web-console
/// stats sample's per-stage p50/p99.
fn percentile(v: &mut [u32], q: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * q) as usize).min(v.len() - 1);
    v[i]
}

/// The encode → packetize loop, over a borrowed capturer. Sending runs on a dedicated thread
/// (see [`spawn_sender`]) so a send spike can never stall capture/encode.
#[allow(clippy::too_many_arguments)]
fn stream_body(
    // `&mut Box` (not `&mut dyn`) so a mid-stream capture-loss rebuild can SWAP the capturer in place.
    capturer: &mut Box<dyn Capturer>,
    // Re-open the video source on capture loss (virtual-display path → follow a Desktop<->Game switch);
    // `None` for the portal/synthetic source, which has nothing to re-detect (propagate the error).
    rebuild: Option<&dyn Fn() -> Result<Box<dyn Capturer>>>,
    sock: &UdpSocket,
    cfg: StreamConfig,
    running: &Arc<AtomicBool>,
    force_idr: &AtomicBool,
    rfi_range: &std::sync::Mutex<Option<(i64, i64)>>,
    // Shared stats recorder. The encode loop reads `stats.is_armed()` per frame to decide whether
    // to accumulate the per-stage split, then emits a `StatsSample` at its 1 s aggregation boundary.
    stats: &Arc<crate::stats_recorder::StatsRecorder>,
    // Short client label (peer IP) seeded into the capture meta on the first armed registration.
    client_label: &str,
) -> Result<()> {
    // The first frame establishes the authoritative size/format for the encoder.
    let mut frame = capturer.next_frame().context("capture first frame")?;
    if frame.width != cfg.width || frame.height != cfg.height {
        tracing::warn!(
            captured = ?(frame.width, frame.height),
            negotiated = ?(cfg.width, cfg.height),
            "captured size != negotiated size — Moonlight expects the negotiated size; resize the output"
        );
    }
    let mut enc = encode::open_video(
        cfg.codec,
        frame.format,
        frame.width,
        frame.height,
        cfg.fps,
        cfg.bitrate_kbps as u64 * 1000,
        frame.is_cuda(),
        8, // GameStream/Moonlight path: 8-bit (its own codec negotiation)
        // GameStream/Moonlight stays 4:2:0 — stock Moonlight clients can't decode 4:4:4, and the
        // protocol has no chroma negotiation. 4:4:4 is punktfunk/1-native only.
        encode::ChromaFormat::Yuv420,
    )
    .context("open video encoder for stream")?;
    // FEC overhead percent (Sunshine default 20). Override with PUNKTFUNK_FEC_PCT (0 = data-only).
    let fec_pct: u8 = std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut pk = VideoPacketizer::new(cfg.packet_size, fec_pct, cfg.min_fec);

    // Pace at the client's negotiated frame rate, re-encoding the last captured frame when the
    // compositor produced no new one. Compositors only emit frames on damage, so a static or
    // slow-updating desktop would otherwise starve the client into a "network too slow" abort.
    // Re-encoding an unchanged frame is cheap — NVENC emits a near-empty P-frame. The upper
    // bound just guards against an absurd client request (the encoder is opened at `cfg.fps`).
    let target_fps = cfg.fps.clamp(1, 240);
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let mut fps_count: u32 = 0;
    let mut fps_t = Instant::now();
    let stream_start = Instant::now();
    // Test knob: drop this % of outbound packets to exercise FEC recovery (0 = off).
    let drop_pct: u32 = std::env::var("PUNKTFUNK_VIDEO_DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut sent_batches: u64 = 0;
    let mut dropped_batches: u64 = 0;

    // The send thread: one frame's batch at a time over a small bounded queue. Depth 2 means a
    // slow send can buffer one frame while the next encodes; beyond that the NEWEST batch is
    // dropped (the client recovers via FEC/RFI) rather than ever stalling the encode loop.
    let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
    spawn_sender(
        sock.try_clone().context("clone video socket")?,
        batch_rx,
        Duration::from_secs_f64(1.0 / target_fps as f64),
        running.clone(),
        drop_pct,
    )?;

    // Per-stage timing (PUNKTFUNK_PERF=1): max µs/stage per second + unique vs re-encoded frames,
    // to pinpoint stalls. `unique` counts genuinely-new captured frames (vs re-encoded holds).
    let perf = crate::config::config().perf;
    let (mut mx_cap, mut mx_enc, mut mx_pkt, mut mx_send, mut mx_pkts, mut uniq) =
        (0u128, 0u128, 0u128, 0u128, 0usize, 0u32);
    // Web-console stats accumulation (active when `perf` OR a capture is armed): per-stage vectors
    // for p50/p99, the goodput bytes queued to the sender this window, the previous window's
    // dropped-frame count for delta computation, and the registration id cached on the first sample.
    let codec_name = match cfg.codec {
        Codec::H264 => "h264",
        Codec::H265 => "hevc",
        Codec::Av1 => "av1",
    };
    let mut sid: Option<u32> = None;
    let (mut v_cap, mut v_enc, mut v_pkt, mut v_send): (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut bytes_win: u64 = 0;
    let mut last_dropped_batches: u64 = 0;
    // Absolute next-frame deadline — the single pacing clock for the loop.
    let mut next_frame = Instant::now();
    // RFI capability is fixed for the session (probed at encoder open). Query it once so the
    // recovery path skips the always-`false` invalidate call on encoders without NVENC RFI and
    // forces a keyframe directly instead.
    let mut supports_rfi = enc.caps().supports_rfi;

    // Bound consecutive capture-loss rebuilds (a delivered frame clears the counter) so a permanently
    // dead source can't loop forever — it ends the stream after the cap, falling back to a reconnect.
    const MAX_REBUILDS: u32 = 5;
    let mut rebuilds: u32 = 0;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();
        // Measure per-stage timing when `PUNKTFUNK_PERF` is set OR a web-console stats capture is
        // armed (cheap Relaxed atomic, re-read each frame).
        let measure = perf || stats.is_armed();
        // Advance to the freshest captured frame if one arrived; otherwise reuse the last.
        match capturer.try_latest() {
            Ok(Some(f)) => {
                frame = f;
                uniq += 1;
                rebuilds = 0; // a delivered frame clears the consecutive-loss counter
            }
            Ok(None) => {} // no new frame — reuse the last (static/idle desktop)
            Err(e) => {
                // The capture source went away — the compositor was torn down on a Desktop<->Game
                // switch, or the virtual output was removed. On the virtual-display path, re-detect the
                // now-live compositor and re-attach IN PLACE (the send thread + packetizer + socket +
                // RTP clock all survive), then force an IDR so Moonlight resyncs — so the stream FOLLOWS
                // the switch with no client reconnect. Build the new source BEFORE dropping the old.
                // Bounded by a counter + a ~40s budget; on exhaustion, end the stream (Moonlight
                // reconnect). The portal/synthetic path has no rebuild closure → propagate as before.
                let Some(rebuild) = rebuild else {
                    return Err(e).context("capture frame");
                };
                rebuilds += 1;
                if rebuilds > MAX_REBUILDS {
                    return Err(e).context("capture lost — rebuild attempts exhausted");
                }
                tracing::warn!(error = %format!("{e:#}"), rebuild = rebuilds,
                    "gamestream: capture lost — rebuilding source in place (following a session switch)");
                let rebuild_deadline = Instant::now() + Duration::from_secs(40);
                let new_cap = loop {
                    match rebuild() {
                        Ok(c) => break c,
                        Err(e2) => {
                            if !running.load(Ordering::SeqCst) || Instant::now() >= rebuild_deadline
                            {
                                return Err(e2)
                                    .context("capture lost — no source within the rebuild budget");
                            }
                            tracing::warn!(error = %format!("{e2:#}"),
                                "gamestream: source not up yet — retrying");
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                };
                *capturer = new_cap;
                capturer.set_active(true);
                frame = capturer.next_frame().context("first frame after rebuild")?;
                // Re-open the encoder for the new source (same negotiated WxH → same SPS profile) and
                // force an IDR so Moonlight resyncs on the first emitted AU.
                enc = encode::open_video(
                    cfg.codec,
                    frame.format,
                    frame.width,
                    frame.height,
                    cfg.fps,
                    cfg.bitrate_kbps as u64 * 1000,
                    frame.is_cuda(),
                    8,
                    encode::ChromaFormat::Yuv420, // GameStream stays 4:2:0
                )
                .context("reopen encoder after rebuild")?;
                supports_rfi = enc.caps().supports_rfi;
                enc.request_keyframe();
                next_frame = Instant::now();
                tracing::info!("gamestream: source rebuilt — stream continues");
                continue;
            }
        }
        let t_cap = tick.elapsed();
        // Honor a client recovery request. Prefer reference-frame invalidation (the encoder
        // re-references an older still-valid frame — no costly IDR spike); if the encoder can't
        // invalidate (range too old, or no NVENC RFI) it returns false and we force a keyframe.
        if let Some((first, last)) = rfi_range.lock().unwrap().take() {
            // Prefer reference-frame invalidation when the encoder supports it (no costly IDR
            // spike); otherwise — or if the range is too old to invalidate — force a keyframe.
            if !(supports_rfi && enc.invalidate_ref_frames(first, last)) {
                enc.request_keyframe();
            }
        }
        // An explicit IDR request (or a rangeless RFI) forces a keyframe so the client resyncs
        // immediately instead of waiting for the next GOP boundary.
        if force_idr.swap(false, Ordering::SeqCst) {
            enc.request_keyframe();
        }
        enc.submit(&frame).context("encoder submit")?;
        let t_enc = tick.elapsed();

        // 90 kHz RTP timestamp from wall-clock, so a variable capture rate stays correct.
        let ts = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
        let mut batch: Vec<Vec<u8>> = Vec::new();
        while let Some(au) = enc.poll().context("encoder poll")? {
            let ft = if au.keyframe {
                FrameType::Idr
            } else {
                FrameType::P
            };
            batch.extend(pk.packetize(&au.data, ft, ts));
        }
        let t_pkt = tick.elapsed();

        // Hand the frame's packets to the send thread; never block here. A full queue means
        // the sender is behind — drop this batch (FEC/RFI covers the client) and keep encoding.
        let n = batch.len();
        // Goodput this window = bytes actually queued to the sender (a dropped batch never reaches
        // the wire, so it's excluded). Summed only when measuring, to keep the idle path free.
        let batch_bytes: u64 = if measure {
            batch.iter().map(|p| p.len() as u64).sum()
        } else {
            0
        };
        if n > 0 {
            match batch_tx.try_send(batch) {
                Ok(()) => {
                    sent_batches += 1;
                    bytes_win += batch_bytes;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    dropped_batches += 1;
                    if dropped_batches.is_power_of_two() {
                        tracing::warn!(dropped_batches, "video: send queue full — frame dropped");
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    break; // sender exited (client gone)
                }
            }
        }
        if measure {
            let t_send = tick.elapsed();
            let cap_us = t_cap.as_micros();
            let enc_us = (t_enc - t_cap).as_micros();
            let pkt_us = (t_pkt - t_enc).as_micros();
            let send_us = (t_send - t_pkt).as_micros();
            mx_cap = mx_cap.max(cap_us);
            mx_enc = mx_enc.max(enc_us);
            mx_pkt = mx_pkt.max(pkt_us);
            mx_send = mx_send.max(send_us);
            mx_pkts = mx_pkts.max(n);
            v_cap.push(cap_us as u32);
            v_enc.push(enc_us as u32);
            v_pkt.push(pkt_us as u32);
            v_send.push(send_us as u32);
        }

        fps_count += 1;
        if fps_t.elapsed() >= Duration::from_secs(1) {
            let secs = fps_t.elapsed().as_secs_f64();
            if perf {
                // Max µs/stage this second: cap=drain channel, enc=submit (zero-copy device
                // copy + NVENC), pkt=poll+FEC+packetize, send=paced packet send. `uniq`=new
                // captured frames (vs re-encoded). `pkts`=max packets in one frame (IDR spike).
                tracing::info!(
                    fps = fps_count,
                    uniq,
                    enc_us = mx_enc,
                    pkt_us = mx_pkt,
                    send_us = mx_send,
                    cap_us = mx_cap,
                    max_pkts = mx_pkts,
                    "video: streaming (perf)"
                );
            } else {
                tracing::info!(
                    fps = fps_count,
                    sent_batches,
                    dropped_batches,
                    "video: streaming"
                );
            }
            // Web-console capture: build the aggregated sample. The host send side exposes no
            // receiver-side packet loss / FEC-recovery / send-buffer EAGAIN counters, so those stay
            // 0 (not fabricated); `frames_dropped` is the per-frame send-queue overflow delta.
            if stats.is_armed() {
                let session_id = *sid.get_or_insert_with(|| {
                    stats.register_session(
                        "gamestream",
                        cfg.width,
                        cfg.height,
                        cfg.fps,
                        codec_name,
                        client_label,
                    )
                });
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0, // stamped by push_sample from the capture's monotonic start
                    session_id,
                    stages: vec![
                        crate::stats_recorder::StageTiming {
                            name: "capture".into(),
                            p50_us: percentile(&mut v_cap, 0.50) as f32,
                            p99_us: percentile(&mut v_cap, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "encode".into(),
                            p50_us: percentile(&mut v_enc, 0.50) as f32,
                            p99_us: percentile(&mut v_enc, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "packetize".into(),
                            p50_us: percentile(&mut v_pkt, 0.50) as f32,
                            p99_us: percentile(&mut v_pkt, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "send".into(),
                            p50_us: percentile(&mut v_send, 0.50) as f32,
                            p99_us: percentile(&mut v_send, 0.99) as f32,
                        },
                    ],
                    fps: (uniq as f64 / secs) as f32,
                    repeat_fps: (fps_count.saturating_sub(uniq) as f64 / secs) as f32,
                    mbps: (bytes_win as f64 * 8.0 / secs / 1_000_000.0) as f32,
                    bitrate_kbps: cfg.bitrate_kbps,
                    frames_dropped: dropped_batches.saturating_sub(last_dropped_batches) as u32,
                    packets_dropped: 0,
                    send_dropped: 0,
                    fec_recovered: 0,
                };
                stats.push_sample(session_id, sample);
            }
            mx_cap = 0;
            mx_enc = 0;
            mx_pkt = 0;
            mx_send = 0;
            mx_pkts = 0;
            uniq = 0;
            v_cap.clear();
            v_enc.clear();
            v_pkt.clear();
            v_send.clear();
            bytes_win = 0;
            last_dropped_batches = dropped_batches;
            fps_count = 0;
            fps_t = Instant::now();
        }
        // Single pacing authority: hold a steady cadence at the target rate from an absolute
        // clock. No double-sleep. If a slow frame put us behind, resync to now rather than
        // bursting to catch up.
        next_frame += frame_interval;
        match next_frame.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next_frame = Instant::now(),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end check of the send thread: batches pushed on the channel arrive, complete and
    /// byte-identical, at a peer socket via the paced sendmmsg path.
    #[test]
    fn sender_delivers_batches() {
        let rx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx_sock
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let tx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx_sock.connect(rx_sock.local_addr().unwrap()).unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = std::sync::mpsc::sync_channel::<PacketBatch>(2);
        spawn_sender(
            tx_sock,
            rx,
            Duration::from_millis(8), // ~120fps frame interval
            running.clone(),
            0,
        )
        .unwrap();

        // 3 frames of 100 packets, content-tagged for verification.
        let mut sent = Vec::new();
        for f in 0..3u8 {
            let batch: PacketBatch = (0..100u8)
                .map(|i| {
                    let mut p = vec![0u8; 1200];
                    p[0] = f;
                    p[1] = i;
                    p
                })
                .collect();
            sent.extend(batch.iter().cloned());
            tx.send(batch).unwrap();
        }
        drop(tx); // sender drains then exits

        let mut got = 0usize;
        let mut buf = [0u8; 2048];
        while got < sent.len() {
            let n = rx_sock.recv(&mut buf).expect("packet within timeout");
            assert_eq!(n, 1200);
            let (f, i) = (buf[0] as usize, buf[1] as usize);
            assert_eq!(&buf[..n], &sent[f * 100 + i][..], "payload intact");
            got += 1;
        }
        assert_eq!(got, 300);
        assert!(running.load(Ordering::SeqCst), "no spurious client-gone");
    }
}
