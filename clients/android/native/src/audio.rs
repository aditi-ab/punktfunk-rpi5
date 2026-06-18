//! Android audio playback (android-only): pull Opus packets from the connector, decode to
//! interleaved f32 stereo, and feed AAudio (LowLatency) via its realtime data callback through a
//! jitter ring. Mirrors [`crate::decode`]: one thread we own (the Opus decode producer) plus a
//! shutdown flag; the realtime callback thread is owned by AAudio. Ring logic ported from
//! `punktfunk-client-linux/src/audio.rs` (prime ~3 quanta, drop-oldest cap, re-prime on drain).

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder,
};
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

const CHANNELS: usize = 2;
const SAMPLE_RATE: i32 = 48_000;
/// Decoded-chunk hand-off depth: 64 × 5 ms = 320 ms slack (matches the core's AUDIO_QUEUE).
const RING_CHUNKS: usize = 64;
/// Opus decode scratch: worst-case 120 ms stereo frame (5760 samples/ch × 2 ch).
const PCM_SCRATCH: usize = 5760 * CHANNELS;

/// Diagnostics — written by the decode thread + the realtime callback, logged periodically. The
/// audio analogue of the video `fed`/`rendered` counters (we can't "screenshot" sound).
#[derive(Default)]
struct Counters {
    opus_decoded: AtomicU64, // Opus packets decoded OK (~200/s at 5 ms frames)
    pcm_written: AtomicU64,  // PCM frames copied out to AAudio (device clock is pulling)
    underruns: AtomicU64,    // callbacks that emitted silence (ring not primed / drained)
    ring_depth: AtomicU64,   // ring sample count at the last callback
}

/// Owned by [`crate::session::SessionHandle`]: the live AAudio stream + the decode thread.
pub struct AudioPlayback {
    _stream: AudioStream, // dropping it stops + closes the AAudio stream
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayback {
    /// Open AAudio (LowLatency, 48 kHz/stereo/f32) with a realtime callback draining a jitter ring,
    /// then spawn the Opus decode thread. `None` on failure (the caller leaves video streaming).
    pub fn start(client: Arc<NativeClient>) -> Option<AudioPlayback> {
        let counters = Arc::new(Counters::default());
        let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);

        // Realtime consumer state, owned by the callback (FnMut) — no lock: AAudio calls it from a
        // single high-priority thread, and the decode thread only touches `tx`.
        let cb_counters = counters.clone();
        let mut ring: VecDeque<f32> = VecDeque::with_capacity(PCM_SCRATCH);
        let mut primed = false;
        let callback = move |_s: &AudioStream, data: *mut c_void, num_frames: i32| {
            let want = num_frames as usize * CHANNELS;
            // SAFETY: AAudio provides `num_frames * channel_count` F32 slots at `data`.
            let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, want) };
            while let Ok(chunk) = rx.try_recv() {
                ring.extend(chunk);
            }
            // Prime to ~3 quanta (15 ms; floor 15 ms / ceiling 200 ms); drop OLDEST above the cap.
            let target = (3 * want).clamp(720 * CHANNELS, 9600 * CHANNELS);
            while ring.len() > target.max(want) + want {
                ring.pop_front();
            }
            if !primed && ring.len() >= target {
                primed = true;
            }
            if primed {
                for slot in out.iter_mut() {
                    *slot = ring.pop_front().unwrap_or(0.0);
                }
                cb_counters
                    .pcm_written
                    .fetch_add(num_frames as u64, Ordering::Relaxed);
            } else {
                out.fill(0.0);
                cb_counters.underruns.fetch_add(1, Ordering::Relaxed);
            }
            if ring.is_empty() {
                primed = false; // re-prime after a genuine drain (avoids sustained crackle on loss)
            }
            cb_counters
                .ring_depth
                .store(ring.len() as u64, Ordering::Relaxed);
            AudioCallbackResult::Continue
        };

        let stream = AudioStreamBuilder::new()
            .map_err(|e| log::error!("audio: AudioStreamBuilder::new: {e}"))
            .ok()?
            .direction(AudioDirection::Output)
            .sample_rate(SAMPLE_RATE)
            .channel_count(CHANNELS as i32)
            .format(AudioFormat::PCM_Float)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sharing_mode(AudioSharingMode::Shared)
            .data_callback(Box::new(callback))
            .error_callback(Box::new(|_s, e| {
                log::warn!("audio: AAudio error (device reroute/disconnect?): {e:?}");
            }))
            .open_stream()
            .map_err(|e| log::error!("audio: open_stream: {e}"))
            .ok()?;

        if let Err(e) = stream.request_start() {
            log::error!("audio: request_start: {e}");
            return None;
        }
        log::info!(
            "audio: AAudio started rate={} ch={} fmt={:?} burst={}",
            stream.sample_rate(),
            stream.channel_count(),
            stream.format(),
            stream.frames_per_burst(),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-audio".into())
            .spawn(move || decode_loop(client, tx, sd, counters))
            .ok();

        Some(AudioPlayback {
            _stream: stream,
            shutdown,
            join,
        })
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // `_stream` drops here → AAudio request_stop + close.
    }
}

/// Producer: `next_audio` → Opus `decode_float` → push interleaved f32 into the ring channel.
fn decode_loop(
    client: Arc<NativeClient>,
    tx: SyncSender<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    let mut dec = match opus::Decoder::new(SAMPLE_RATE as u32, opus::Channels::Stereo) {
        Ok(d) => d,
        Err(e) => {
            log::error!("audio: opus decoder init: {e} — audio disabled");
            return;
        }
    };
    let mut pcm = vec![0f32; PCM_SCRATCH];
    let mut window_peak = 0f32; // loudest |sample| since the last log — tells a tone from silence
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_audio(Duration::from_millis(5)) {
            Ok(pkt) => match dec.decode_float(&pkt.data, &mut pcm, false) {
                Ok(samples) => {
                    let n = samples * CHANNELS;
                    for &s in &pcm[..n] {
                        window_peak = window_peak.max(s.abs());
                    }
                    let count = counters.opus_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                    match tx.try_send(pcm[..n].to_vec()) {
                        Ok(()) | Err(TrySendError::Full(_)) => {} // drop-newest under backpressure
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                    if count % 600 == 0 {
                        log::info!(
                            "audio: opus={count} pcm_frames={} underruns={} ring={} peak={window_peak:.3}",
                            counters.pcm_written.load(Ordering::Relaxed),
                            counters.underruns.load(Ordering::Relaxed),
                            counters.ring_depth.load(Ordering::Relaxed),
                        );
                        window_peak = 0.0;
                    }
                }
                Err(e) => log::debug!("audio: opus decode: {e}"),
            },
            Err(PunktfunkError::NoFrame) => {} // timeout
            Err(_) => break,                   // session closed
        }
    }
    log::info!(
        "audio: stopped (opus={} pcm_frames={} underruns={})",
        counters.opus_decoded.load(Ordering::Relaxed),
        counters.pcm_written.load(Ordering::Relaxed),
        counters.underruns.load(Ordering::Relaxed),
    );
}
