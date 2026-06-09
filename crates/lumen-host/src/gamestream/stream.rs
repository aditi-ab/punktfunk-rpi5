//! The video data plane: on RTSP PLAY, learn the client's UDP endpoint (it pings the video
//! port), then run capture → NVENC encode → [`VideoPacketizer`] → UDP send. P1.3 uses a fast
//! synthetic test pattern (proves the wire format with no compositor); swapping in real
//! portal desktop capture is a one-line source change. Runs on its own native thread.

use super::video::{FrameType, VideoPacketizer};
use super::VIDEO_PORT;
use crate::capture::{CapturedFrame, PixelFormat};
use crate::encode::{self, Codec};
use anyhow::{Context, Result};
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

    let (w, h, fps) = (cfg.width, cfg.height, cfg.fps);
    let mut enc = encode::open_video(
        cfg.codec,
        PixelFormat::Bgrx,
        w,
        h,
        fps,
        cfg.bitrate_kbps as u64 * 1000,
    )
    .context("open NVENC for stream")?;
    let mut pk = VideoPacketizer::new(cfg.packet_size);

    let bpp = 4usize;
    let row = w as usize * bpp;
    let mut buf = vec![0u8; row * h as usize];
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut frame_idx: u32 = 0;
    let mut sent_pkts: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let tick = Instant::now();

        // Fast synthetic test pattern: a pulsing grey field + a white band sweeping down.
        let shade = (frame_idx % 256) as u8;
        buf.fill(shade);
        let band_h = (h as usize / 20).max(1);
        let band_y = (frame_idx as usize * 6) % h as usize;
        for y in band_y..(band_y + band_h).min(h as usize) {
            buf[y * row..(y + 1) * row].fill(0xff);
        }

        let frame = CapturedFrame {
            width: w,
            height: h,
            pts_ns: frame_idx as u64 * 1_000_000_000 / fps as u64,
            format: PixelFormat::Bgrx,
            cpu_bytes: std::mem::take(&mut buf),
        };
        enc.submit(&frame).context("encoder submit")?;
        buf = frame.cpu_bytes; // reclaim the buffer (no per-frame realloc)

        let ts = (frame_idx as u64 * 90_000 / fps as u64) as u32;
        let mut client_gone = false;
        while let Some(au) = enc.poll().context("encoder poll")? {
            let ft = if au.keyframe {
                FrameType::Idr
            } else {
                FrameType::P
            };
            for pkt in pk.packetize(&au.data, ft, ts) {
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

        frame_idx += 1;
        if frame_idx % (fps.max(1) * 2) == 0 {
            tracing::info!(frame_idx, sent_pkts, "video: streaming");
        }
        let elapsed = tick.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }
    Ok(())
}
