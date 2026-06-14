//! Session controller: one worker thread runs connect → pump (video pull + decode, audio
//! pull + Opus decode, stats), feeding the GTK main loop over channels. The UI keeps the
//! `Arc<NativeClient>` from the `Connected` event for direct input sends (no extra hop on
//! the input path) — `NativeClient` is `Sync`, planes stay one-consumer-per-thread:
//! video+audio here, rumble+hidout on the gamepad thread.

use crate::audio;
use crate::video::{DecodedFrame, Decoder};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use punktfunk_core::PunktfunkError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct SessionParams {
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    pub compositor: CompositorPref,
    pub gamepad: GamepadPref,
    pub bitrate_kbps: u32,
    /// Stream the default microphone to the host's virtual mic source.
    pub mic_enabled: bool,
    /// Pinned host fingerprint; `None` = trust on first use (caller persists the observed one).
    pub pin: Option<[u8; 32]>,
    pub identity: (String, String),
}

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub fps: f32,
    pub mbps: f32,
    pub decode_ms: f32,
    /// Median capture→decoded latency over the last window (host-clock corrected).
    pub latency_ms: f32,
}

pub enum SessionEvent {
    Connected {
        connector: Arc<NativeClient>,
        mode: Mode,
        fingerprint: [u8; 32],
    },
    Failed(String),
    Ended(Option<String>),
    Stats(Stats),
}

pub struct SessionHandle {
    pub events: async_channel::Receiver<SessionEvent>,
    pub frames: async_channel::Receiver<DecodedFrame>,
    pub stop: Arc<AtomicBool>,
}

pub fn start(params: SessionParams) -> SessionHandle {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    // Tiny frame queue, newest wins: force_send displaces the oldest when the UI lags.
    let (frame_tx, frame_rx) = async_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, stop_w))
        .expect("spawn session thread");
    SessionHandle {
        events: ev_rx,
        frames: frame_rx,
        stop,
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn pump(
    params: SessionParams,
    ev_tx: async_channel::Sender<SessionEvent>,
    frame_tx: async_channel::Sender<DecodedFrame>,
    stop: Arc<AtomicBool>,
) {
    let connector = match NativeClient::connect(
        &params.host,
        params.port,
        params.mode,
        params.compositor,
        params.gamepad,
        params.bitrate_kbps,
        None, // launch: the Linux client has no library picker yet
        params.pin,
        Some(params.identity),
        Duration::from_secs(15),
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            let msg = match e {
                PunktfunkError::Crypto => {
                    "Host identity rejected — wrong fingerprint, or the host requires pairing"
                        .to_string()
                }
                PunktfunkError::Timeout => "Connection timed out".to_string(),
                other => format!("Connect failed: {other:?}"),
            };
            let _ = ev_tx.send_blocking(SessionEvent::Failed(msg));
            return;
        }
    };
    let _ = ev_tx.send_blocking(SessionEvent::Connected {
        connector: connector.clone(),
        mode: connector.mode(),
        fingerprint: connector.host_fingerprint,
    });

    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            let _ = ev_tx.send_blocking(SessionEvent::Ended(Some(format!("video decoder: {e}"))));
            return;
        }
    };
    // Audio is best-effort: a session without it still streams. Gamepads are the
    // app-lifetime service's job (the UI attaches it on Connected).
    let player = audio::AudioPlayer::spawn()
        .map_err(|e| tracing::warn!(error = %e, "audio disabled"))
        .ok();
    let mut opus_dec = opus::Decoder::new(48_000, opus::Channels::Stereo)
        .map_err(|e| tracing::warn!(error = %e, "opus decoder failed — audio disabled"))
        .ok();
    let _mic = params
        .mic_enabled
        .then(|| {
            audio::MicStreamer::spawn(connector.clone())
                .map_err(|e| tracing::warn!(error = %e, "mic uplink disabled"))
                .ok()
        })
        .flatten();

    let clock_offset = connector.clock_offset_ns;
    let mut total_frames = 0u64;
    let mut window_start = Instant::now();
    let mut frames_n = 0u32;
    let mut bytes_n = 0u64;
    let mut decode_us_sum = 0u64;
    let mut lat_us: Vec<u64> = Vec::with_capacity(256);
    let mut pcm = vec![0f32; 5760 * 2]; // decode scratch: max Opus frame (120 ms stereo)

    let end: Option<String> = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        match connector.next_frame(Duration::from_millis(4)) {
            Ok(frame) => {
                let t0 = Instant::now();
                match decoder.decode(&frame.data) {
                    Ok(Some(decoded)) => {
                        total_frames += 1;
                        if total_frames == 1 {
                            let (w, h, path) = match &decoded {
                                DecodedFrame::Cpu(c) => (c.width, c.height, "software"),
                                DecodedFrame::Dmabuf(d) => (d.width, d.height, "vaapi-dmabuf"),
                            };
                            tracing::info!(width = w, height = h, path, "first frame decoded");
                        }
                        // Latency: our wall clock expressed in the host's capture clock,
                        // minus the host-stamped capture pts (same math as client-rs).
                        let lat = (now_ns() as i128 + clock_offset as i128 - frame.pts_ns as i128)
                            .max(0) as u64;
                        if lat > 0 && lat < 10_000_000_000 {
                            lat_us.push(lat / 1000);
                        }
                        decode_us_sum += t0.elapsed().as_micros() as u64;
                        frames_n += 1;
                        bytes_n += frame.data.len() as u64;
                        let _ = frame_tx.force_send(decoded);
                    }
                    Ok(None) => {}
                    // Survivable (loss until the next IDR/RFI recovery) — keep feeding.
                    Err(e) => tracing::debug!(error = %e, "decode error (recovering)"),
                }
            }
            Err(PunktfunkError::NoFrame) => {}
            Err(PunktfunkError::Closed) => break Some("Host ended the session".to_string()),
            Err(e) => break Some(format!("session: {e:?}")),
        }

        // Drain audio between frames (packets land every 5 ms; the queue holds 320 ms).
        while let Ok(pkt) = connector.next_audio(Duration::ZERO) {
            if let (Some(player), Some(dec)) = (&player, opus_dec.as_mut()) {
                match dec.decode_float(&pkt.data, &mut pcm, false) {
                    Ok(samples) => player.push(pcm[..samples * 2].to_vec()),
                    Err(e) => tracing::debug!(error = %e, "opus decode"),
                }
            }
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f32();
            lat_us.sort_unstable();
            let p50 = lat_us.get(lat_us.len() / 2).copied().unwrap_or(0);
            tracing::debug!(
                fps = frames_n,
                lat_p50_us = p50,
                total_frames,
                "stream window"
            );
            let _ = ev_tx.try_send(SessionEvent::Stats(Stats {
                fps: frames_n as f32 / secs,
                mbps: bytes_n as f32 * 8.0 / 1e6 / secs,
                decode_ms: if frames_n > 0 {
                    decode_us_sum as f32 / frames_n as f32 / 1000.0
                } else {
                    0.0
                },
                latency_ms: p50 as f32 / 1000.0,
            }));
            window_start = Instant::now();
            frames_n = 0;
            bytes_n = 0;
            decode_us_sum = 0;
            lat_us.clear();
        }
    };

    tracing::info!(
        total_frames,
        reason = end.as_deref().unwrap_or("user"),
        "session ended"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = ev_tx.send_blocking(SessionEvent::Ended(end));
}
