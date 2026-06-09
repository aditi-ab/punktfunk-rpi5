//! The video data plane: on RTSP PLAY, learn the client's UDP endpoint (it pings the video
//! port), then run capture → NVENC encode → [`VideoPacketizer`] → UDP send. The source is
//! either real portal desktop capture (`LUMEN_VIDEO_SOURCE=portal`, the M0 PipeWire path) or
//! a synthetic test pattern (default). Runs on its own native thread.

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

/// Spawn the video stream thread (idempotent via `running`). Stops when `running` clears.
/// `force_idr` is set by the control stream on a client recovery request; `video_cap` holds
/// the persistent capturer the thread borrows for the stream's duration.
pub fn start(
    cfg: StreamConfig,
    running: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    video_cap: CapturerSlot,
) {
    let _ = std::thread::Builder::new()
        .name("lumen-video".into())
        .spawn(move || {
            tracing::info!(?cfg, "video stream starting");
            if let Err(e) = run(cfg, &running, &force_idr, &video_cap) {
                tracing::error!(error = %format!("{e:#}"), "video stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("video stream stopped");
        });
}

fn run(
    cfg: StreamConfig,
    running: &AtomicBool,
    force_idr: &AtomicBool,
    video_cap: &std::sync::Mutex<Option<Box<dyn Capturer>>>,
) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", VIDEO_PORT)).context("bind video UDP")?;
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

    // Reuse the persistent capturer (one screencast session → clean reconnect); create it on
    // the first stream. Borrow it for this stream and return it on exit.
    let mut capturer: Box<dyn Capturer> = match video_cap.lock().unwrap().take() {
        Some(c) => {
            tracing::info!("video source: reusing capturer");
            c
        }
        None if std::env::var("LUMEN_VIDEO_SOURCE").is_ok_and(|v| v == "portal") => {
            tracing::info!("video source: portal desktop capture");
            capture::open_portal_monitor().context("open portal capturer")?
        }
        None => {
            tracing::info!("video source: synthetic test pattern");
            Box::new(FastSyntheticCapturer::new(cfg.width, cfg.height))
        }
    };
    capturer.set_active(true);
    let result = stream_body(&mut *capturer, &sock, cfg, running, force_idr);
    capturer.set_active(false);
    *video_cap.lock().unwrap() = Some(capturer);
    result
}

/// The encode → packetize → paced-send loop, over a borrowed capturer.
fn stream_body(
    capturer: &mut dyn Capturer,
    sock: &UdpSocket,
    cfg: StreamConfig,
    running: &AtomicBool,
    force_idr: &AtomicBool,
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
    )
    .context("open NVENC for stream")?;
    // FEC overhead percent (Sunshine default 20). Override with LUMEN_FEC_PCT (0 = data-only).
    let fec_pct: u8 = std::env::var("LUMEN_FEC_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut pk = VideoPacketizer::new(cfg.packet_size, fec_pct, cfg.min_fec);

    // Pace at a steady rate (capped at 60fps), re-encoding the last captured frame when the
    // compositor produced no new one. wlroots only emits frames on damage, so a static or
    // slow-updating desktop would otherwise starve the client into a "network too slow" abort.
    // Re-encoding an unchanged frame is cheap — NVENC emits a near-empty P-frame.
    let target_fps = cfg.fps.clamp(1, 60);
    let frame_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let mut sent_pkts: u64 = 0;
    let mut fps_count: u32 = 0;
    let mut fps_t = Instant::now();
    let stream_start = Instant::now();
    // Test knob: drop this % of outbound packets to exercise FEC recovery (0 = off).
    let drop_pct: u32 = std::env::var("LUMEN_VIDEO_DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut rng = rand::thread_rng();
    let mut dropped: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();
        // Advance to the freshest captured frame if one arrived; otherwise reuse the last.
        if let Some(f) = capturer.try_latest().context("capture frame")? {
            frame = f;
        }
        // Honor a client recovery request (RFI / request-IDR): force a keyframe so the client
        // resyncs immediately instead of waiting for the next GOP boundary.
        if force_idr.swap(false, Ordering::SeqCst) {
            enc.request_keyframe();
        }
        enc.submit(&frame).context("encoder submit")?;

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

        // Pace the frame's packets evenly across the frame interval rather than blasting them
        // at line rate (a real link drops microbursts). The per-packet schedule also paces the
        // frame itself; sub-500µs sleeps are skipped (unreliable), batching the spread.
        let mut client_gone = false;
        let n = batch.len();
        if n > 0 {
            let per_ns = frame_interval.as_nanos() as u64 / n as u64;
            for (i, pkt) in batch.iter().enumerate() {
                if drop_pct > 0 && rng.gen_range(0..100) < drop_pct {
                    dropped += 1; // simulated loss: built the packet, skip the send
                } else if sock.send(pkt).is_err() {
                    client_gone = true;
                    break;
                } else {
                    sent_pkts += 1;
                }
                let target = tick + Duration::from_nanos(per_ns * (i as u64 + 1));
                if let Some(ahead) = target.checked_duration_since(Instant::now()) {
                    if ahead >= Duration::from_micros(500) {
                        std::thread::sleep(ahead);
                    }
                }
            }
        }
        if client_gone {
            tracing::info!(sent_pkts, "video: client unreachable — stopping stream");
            break;
        }

        fps_count += 1;
        if fps_t.elapsed() >= Duration::from_secs(1) {
            tracing::info!(fps = fps_count, sent_pkts, dropped, "video: streaming");
            fps_count = 0;
            fps_t = Instant::now();
        }
        // Backstop the frame rate when few/no packets were produced (the packet pacing above
        // otherwise consumes ~one frame interval on its own).
        let elapsed = tick.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
    Ok(())
}
