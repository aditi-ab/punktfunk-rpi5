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
}

/// Spawn the video stream thread (idempotent via `running`). Stops when `running` clears.
pub fn start(cfg: StreamConfig, running: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("lumen-video".into())
        .spawn(move || {
            tracing::info!(?cfg, "video stream starting");
            if let Err(e) = run(cfg, &running) {
                tracing::error!(error = %format!("{e:#}"), "video stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("video stream stopped");
        });
}

fn run(cfg: StreamConfig, running: &AtomicBool) -> Result<()> {
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

    let use_portal = std::env::var("LUMEN_VIDEO_SOURCE").is_ok_and(|v| v == "portal");
    let mut capturer: Box<dyn Capturer> = if use_portal {
        tracing::info!("video source: portal desktop capture");
        capture::open_portal_monitor().context("open portal capturer")?
    } else {
        tracing::info!("video source: synthetic test pattern");
        Box::new(FastSyntheticCapturer::new(cfg.width, cfg.height))
    };

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
    let mut pk = VideoPacketizer::new(cfg.packet_size, fec_pct);

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
        enc.submit(&frame).context("encoder submit")?;

        // 90 kHz RTP timestamp from wall-clock, so a variable capture rate stays correct.
        let ts = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
        let mut client_gone = false;
        while let Some(au) = enc.poll().context("encoder poll")? {
            let ft = if au.keyframe {
                FrameType::Idr
            } else {
                FrameType::P
            };
            for pkt in pk.packetize(&au.data, ft, ts) {
                // Simulated network loss: build the packet (advances seq) but skip the send.
                if drop_pct > 0 && rng.gen_range(0..100) < drop_pct {
                    dropped += 1;
                    continue;
                }
                if sock.send(&pkt).is_err() {
                    client_gone = true;
                    break;
                }
                sent_pkts += 1;
            }
            if client_gone {
                break;
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
        let elapsed = tick.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
    Ok(())
}
