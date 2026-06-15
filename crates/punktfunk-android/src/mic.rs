//! Android microphone uplink (android-only): capture mic PCM via AAudio (LowLatency **input**),
//! Opus-encode 20 ms stereo frames, and push them to the host over the connector's mic plane
//! (`send_mic` → 0xCB datagram). The mirror of [`crate::audio`] in reverse: AAudio's realtime input
//! callback hands captured interleaved f32 to a channel; a worker thread we own does the Opus encode
//! + send (encoding is too heavy for the realtime callback, exactly as decode is on the playback
//! side). Format matches the host decoder + the Linux client: 48 kHz **stereo**, 20 ms, Opus VOIP.

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder,
};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHANNELS: usize = 2;
const SAMPLE_RATE: i32 = 48_000;
/// 20 ms per channel @ 48 kHz — the Linux client's frame; the host accepts ≤ 120 ms.
const FRAME_SAMPLES: usize = 960;
/// Captured-chunk hand-off depth (each ~ one burst); drops on overflow (best-effort uplink).
const RING_CHUNKS: usize = 64;
/// Opus VOIP target bitrate (speech; tunable).
const MIC_BITRATE: i32 = 64_000;

/// Owned by [`crate::session::SessionHandle`]: the live AAudio input stream + the encode thread.
pub struct MicCapture {
    _stream: AudioStream, // dropping it stops + closes the AAudio input stream
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MicCapture {
    /// Open AAudio (LowLatency, 48 kHz/stereo/f32) for **input** with a realtime callback that
    /// forwards captured PCM to a channel, then spawn the Opus encode + uplink thread. `None` on
    /// failure (the caller leaves the rest of the session streaming).
    pub fn start(client: Arc<NativeClient>) -> Option<MicCapture> {
        let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
        let captured = Arc::new(AtomicU64::new(0));
        let cb_captured = captured.clone();

        let callback = move |_s: &AudioStream, data: *mut c_void, num_frames: i32| {
            let n = num_frames as usize * CHANNELS;
            // SAFETY: for an input stream AAudio provides `num_frames * channel_count` captured F32
            // samples at `data` (read-only for us).
            let inp = unsafe { std::slice::from_raw_parts(data as *const f32, n) };
            match tx.try_send(inp.to_vec()) {
                Ok(()) | Err(TrySendError::Full(_)) => {} // drop-newest if the encoder lags
                Err(TrySendError::Disconnected(_)) => return AudioCallbackResult::Stop,
            }
            cb_captured.fetch_add(num_frames as u64, Ordering::Relaxed);
            AudioCallbackResult::Continue
        };

        let stream = AudioStreamBuilder::new()
            .map_err(|e| log::error!("mic: AudioStreamBuilder::new: {e}"))
            .ok()?
            .direction(AudioDirection::Input)
            .sample_rate(SAMPLE_RATE)
            .channel_count(CHANNELS as i32)
            .format(AudioFormat::PCM_Float)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sharing_mode(AudioSharingMode::Shared)
            .data_callback(Box::new(callback))
            .error_callback(Box::new(|_s, e| {
                log::warn!("mic: AAudio error (device reroute/disconnect?): {e:?}");
            }))
            .open_stream()
            .map_err(|e| log::error!("mic: open_stream (RECORD_AUDIO granted?): {e}"))
            .ok()?;

        if let Err(e) = stream.request_start() {
            log::error!("mic: request_start: {e}");
            return None;
        }
        log::info!(
            "mic: AAudio input started rate={} ch={} fmt={:?}",
            stream.sample_rate(),
            stream.channel_count(),
            stream.format(),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-mic".into())
            .spawn(move || encode_loop(client, rx, sd, captured))
            .ok();

        Some(MicCapture {
            _stream: stream,
            shutdown,
            join,
        })
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // `_stream` drops here → AAudio request_stop + close.
    }
}

/// Consumer: drain captured f32 → accumulate → Opus `encode_float` 20 ms stereo frames → `send_mic`.
fn encode_loop(
    client: Arc<NativeClient>,
    rx: Receiver<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
) {
    let mut enc = match opus::Encoder::new(
        SAMPLE_RATE as u32,
        opus::Channels::Stereo,
        opus::Application::Voip,
    ) {
        Ok(e) => e,
        Err(e) => {
            log::error!("mic: opus encoder init: {e} — mic disabled");
            return;
        }
    };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(MIC_BITRATE));

    let frame = FRAME_SAMPLES * CHANNELS;
    let mut ring: VecDeque<f32> = VecDeque::with_capacity(frame * 4);
    let mut out = vec![0u8; 4000]; // max Opus packet for a 20 ms frame fits easily
    let mut seq: u32 = 0;
    let mut sent: u64 = 0;
    let mut peak = 0f32; // loudest |sample| since the last log — tells speech from silence

    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => ring.extend(chunk),
            Err(RecvTimeoutError::Timeout) => continue, // wake to re-check shutdown
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while ring.len() >= frame {
            let pcm: Vec<f32> = ring.drain(..frame).collect();
            for &s in &pcm {
                peak = peak.max(s.abs());
            }
            match enc.encode_float(&pcm, &mut out) {
                Ok(len) => {
                    let pts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    let _ = client.send_mic(seq, pts, out[..len].to_vec());
                    seq = seq.wrapping_add(1);
                    sent += 1;
                    if sent % 250 == 0 {
                        log::info!(
                            "mic: sent={sent} captured_frames={} peak={peak:.3}",
                            captured.load(Ordering::Relaxed),
                        );
                        peak = 0.0;
                    }
                }
                Err(e) => log::debug!("mic: opus encode: {e}"),
            }
        }
    }
    log::info!(
        "mic: stopped (sent={sent} captured_frames={})",
        captured.load(Ordering::Relaxed),
    );
}
