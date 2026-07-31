//! Android microphone uplink (android-only): capture mic PCM via AAudio (LowLatency **input**),
//! Opus-encode 10 ms mono frames, and push them to the host over the connector's mic plane
//! (`send_mic` → 0xCB datagram). The mirror of [`crate::audio`] in reverse: AAudio's realtime input
//! callback hands captured f32 to a channel; a worker thread we own does the Opus encode + send
//! (encoding is too heavy for the realtime callback, exactly as decode is on the playback side).
//! Like the playback path, the realtime callback is allocation-free: captured bursts are copied
//! into pre-allocated buffers from a recycle free-list (pool empty = drop the chunk, never
//! allocate on the capture thread). Format: 48 kHz **mono**, 10 ms, Opus VOIP with in-band FEC —
//! the host decodes any Opus frame ≤ 120 ms with its stereo decoder (mono packets upmix), so this
//! needs no protocol change; speech gains nothing from stereo, and the shorter frame shaves a
//! buffering interval off the uplink.

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, SessionId,
};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHANNELS: usize = 1;
const SAMPLE_RATE: i32 = 48_000;
/// 10 ms per channel @ 48 kHz — half the desktop clients' 20 ms frame, trading a little Opus
/// header overhead for one less buffered interval; the host accepts ≤ 120 ms.
const FRAME_SAMPLES: usize = 480;
/// Captured-chunk hand-off depth (each ~ one burst); drops on overflow (best-effort uplink).
/// Bursts are sized in frames, so the wall-time depth is unchanged by the stereo→mono move.
const RING_CHUNKS: usize = 64;
/// Free-list buffer capacity, in interleaved f32 samples: comfortably above a LowLatency input
/// burst (typically ≤ ~480 frames — mono, so samples = frames). A device with larger bursts costs
/// each buffer a one-time grow on the capture thread, after which the steady state is
/// allocation-free again.
const CHUNK_CAP_SAMPLES: usize = 960; // 20 ms mono — the same wall-time as the old stereo value
/// Opus VOIP target bitrate (mono speech; tunable).
const MIC_BITRATE: i32 = 48_000;
/// Encode-side self-heal threshold, in queued 10 ms frames (~60 ms): waking to more than this
/// means the uplink stalled — and because the capture callback drops the NEWEST chunk when the
/// channel is full, a stall otherwise converts to standing mic delay that never drains (real-time
/// playback host-side never makes time back up). Skip to the newest few frames instead.
const BACKLOG_MAX_FRAMES: usize = 6;
/// What a self-heal keeps: ~20 ms of the freshest audio (one audible blip, live again).
const BACKLOG_KEEP_FRAMES: usize = 2;

/// Owned by [`crate::session::SessionHandle`]: the live AAudio input stream + the encode thread.
pub struct MicCapture {
    _stream: AudioStream, // dropping it stops + closes the AAudio input stream
    /// The audio-session id AAudio allocated (`> 0`) when echo cancellation asked for one — the
    /// hook Kotlin hangs the Java `AcousticEchoCanceler`/`NoiseSuppressor` on. `0` = none.
    session_id: i32,
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MicCapture {
    /// Open AAudio (LowLatency, 48 kHz/mono/f32) for **input** with a realtime callback that
    /// forwards captured PCM to a channel, then spawn the Opus encode + uplink thread. With
    /// `echo_cancel` the stream opens under the `VoiceCommunication` input preset — the HAL's own
    /// echo canceller / noise suppressor on the capture path (the default `VoiceRecognition`
    /// preset deliberately bypasses them, which is why the host used to hear its own stream back
    /// from a speaker-playing phone) — and allocates an audio session id for Kotlin's Java-effect
    /// backstop. `None` on failure (the caller leaves the rest of the session streaming).
    pub fn start(client: Arc<NativeClient>, echo_cancel: bool) -> Option<MicCapture> {
        let captured = Arc::new(AtomicU64::new(0));
        // Chunks discarded on the capture thread (free-list empty / encoder lagging); logged
        // throttled from the encode worker.
        let dropped = Arc::new(AtomicU64::new(0));

        // One open attempt at a given sharing mode (same pattern as [`crate::audio`]: `open_stream`
        // consumes the builder AND the callback, so each try rebuilds the channels it captures).
        let try_open = |sharing: AudioSharingMode,
                        voice: bool|
         -> ndk::audio::Result<(
            AudioStream,
            Receiver<Vec<f32>>,
            SyncSender<Vec<f32>>,
        )> {
            let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
            // Recycle free-list, mirroring the playback path: the realtime capture callback must
            // not touch the allocator (Android's Scudo has unbounded malloc/free tail latency — an
            // allocation here is a missed burst), so it pops a pre-allocated buffer, copies the
            // burst in and sends it; the encode worker returns drained buffers. Pool empty = DROP
            // the chunk (counted) rather than allocate.
            let (free_tx, free_rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
            for _ in 0..RING_CHUNKS {
                let _ = free_tx.try_send(Vec::with_capacity(CHUNK_CAP_SAMPLES));
            }
            let cb_captured = captured.clone();
            let cb_dropped = dropped.clone();
            let cb_free_tx = free_tx.clone(); // returns the buffer when the data channel is full

            let callback = move |_s: &AudioStream, data: *mut c_void, num_frames: i32| {
                let n = num_frames as usize * CHANNELS;
                // SAFETY: for an input stream AAudio provides `num_frames * channel_count` captured
                // F32 samples at `data` (read-only for us).
                let inp = unsafe { std::slice::from_raw_parts(data as *const f32, n) };
                cb_captured.fetch_add(num_frames as u64, Ordering::Relaxed);
                match free_rx.try_recv() {
                    Ok(mut buf) => {
                        buf.clear();
                        buf.extend_from_slice(inp); // retained capacity — no realloc past the first
                        match tx.try_send(buf) {
                            Ok(()) => {}
                            Err(TrySendError::Full(buf)) => {
                                // Encoder lagging: drop the chunk, hand the buffer straight back.
                                let _ = cb_free_tx.try_send(buf);
                                cb_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(TrySendError::Disconnected(_)) => return AudioCallbackResult::Stop,
                        }
                    }
                    // Pool empty (every buffer in flight): drop, never allocate on this thread.
                    Err(_) => {
                        cb_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                AudioCallbackResult::Continue
            };

            // NOTE: no `.frames_per_data_callback(...)`: AAudio's own docs call leaving it unset
            // the lowest-latency path (the callback then runs at the device's optimal burst,
            // while pinning a size inserts an adaptation buffer), and the encode side re-chunks
            // to 10 ms frames regardless of how the bursts arrive.
            let mut builder = AudioStreamBuilder::new()?
                .direction(AudioDirection::Input)
                .sample_rate(SAMPLE_RATE)
                .channel_count(CHANNELS as i32)
                .format(AudioFormat::PCM_Float)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(sharing);
            if voice {
                // VoiceCommunication routes the capture through the HAL's AEC/NS; the allocated
                // session id (`None` = allocate) is what Kotlin attaches the Java effects to.
                builder = builder
                    .input_preset(AudioInputPreset::VoiceCommunication)
                    .session_id(None);
            }
            let stream = builder
                .data_callback(Box::new(callback))
                .error_callback(Box::new(|_s, e| {
                    log::warn!("mic: AAudio error (device reroute/disconnect?): {e:?}");
                }))
                .open_stream()?;
            Ok((stream, rx, free_tx))
        };

        // Exclusive first — MMAP-exclusive is AAudio's lowest-latency path — falling back to
        // Shared when the device refuses (no MMAP, mic claimed, …); and each sharing mode with
        // the voice preset before without it, because some HALs reject VoiceCommunication (or a
        // session id) outright and a mic without echo cancellation still beats no mic. The
        // ladder's last rungs are exactly the preset-less open this always did. The started-log
        // below prints what the device actually GRANTED (`share=`/`session=`).
        let attempts: &[(AudioSharingMode, bool)] = if echo_cancel {
            &[
                (AudioSharingMode::Exclusive, true),
                (AudioSharingMode::Shared, true),
                (AudioSharingMode::Exclusive, false),
                (AudioSharingMode::Shared, false),
            ]
        } else {
            &[
                (AudioSharingMode::Exclusive, false),
                (AudioSharingMode::Shared, false),
            ]
        };
        let mut opened = None;
        for &(sharing, voice) in attempts {
            match try_open(sharing, voice) {
                Ok(o) => {
                    opened = Some(o);
                    break;
                }
                Err(e) => log::info!(
                    "mic: open {sharing:?}{} failed ({e}) — trying the next fallback",
                    if voice { "+VoiceCommunication" } else { "" },
                ),
            }
        }
        let (stream, rx, free_tx) = match opened {
            Some(o) => o,
            None => {
                log::error!("mic: open_stream (RECORD_AUDIO granted?): every mode refused");
                return None;
            }
        };

        // The session id AAudio actually allocated (only a voice rung asks for one): `> 0` is the
        // handle Kotlin hangs the Java AcousticEchoCanceler/NoiseSuppressor off as the HAL
        // preset's backstop; `0` = none, nothing to attach.
        let session_id = match stream.session_id() {
            SessionId::Allocated(id) => id.get(),
            SessionId::None => 0,
        };

        if let Err(e) = stream.request_start() {
            log::error!("mic: request_start: {e}");
            return None;
        }
        log::info!(
            "mic: AAudio input started rate={} ch={} fmt={:?} share={:?} session={session_id}",
            stream.sample_rate(),
            stream.channel_count(),
            stream.format(),
            stream.sharing_mode(),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-mic".into())
            .spawn(move || encode_loop(client, rx, free_tx, sd, captured, dropped))
            .ok();

        Some(MicCapture {
            _stream: stream,
            session_id,
            shutdown,
            join,
        })
    }

    /// The audio-session id AAudio allocated (`> 0`; see [`MicCapture::start`]), `0` = none.
    pub fn session_id(&self) -> i32 {
        self.session_id
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

/// Consumer: drain captured f32 → accumulate → Opus `encode_float` 10 ms mono frames → `send_mic`.
/// Drained chunk buffers go back to the callback's free-list; the encode scratch is reused across
/// frames (only the packet Vec handed to `send_mic` is allocated per frame — it's sent away owned).
fn encode_loop(
    client: Arc<NativeClient>,
    rx: Receiver<Vec<f32>>,
    free_tx: SyncSender<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
) {
    // Fold this Opus-encode/uplink thread into the client's hot-thread set so the ADPF session the
    // decode thread opens keeps mic encode on a fast core too (the playback side's decode_loop
    // does the same). No-op below API 33.
    client.register_hot_thread();
    let mut enc = match opus::Encoder::new(
        SAMPLE_RATE as u32,
        opus::Channels::Mono,
        opus::Application::Voip,
    ) {
        Ok(e) => e,
        Err(e) => {
            log::error!("mic: opus encoder init: {e} — mic disabled");
            return;
        }
    };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(MIC_BITRATE));
    // Speech tuning: complexity 5 roughly halves encode cost for no audible loss at this rate,
    // and in-band FEC at an assumed 10% loss lets the host's decoder reconstruct a dropped
    // datagram from its successor instead of playing a hole (the uplink is fire-and-forget).
    let _ = enc.set_complexity(5);
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(10);

    let frame = FRAME_SAMPLES * CHANNELS;
    let mut ring: VecDeque<f32> = VecDeque::with_capacity(frame * 4);
    let mut pcm = vec![0f32; frame]; // reusable encode scratch (one 10 ms frame)
    let mut out = vec![0u8; 4000]; // max Opus packet for a 10 ms frame fits easily
    let mut seq: u32 = 0;
    let mut sent: u64 = 0;
    let mut stale: u64 = 0; // frames shed by the backlog self-heal (see BACKLOG_MAX_FRAMES)
    let mut peak = 0f32; // loudest |sample| since the last log — tells speech from silence

    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(mut chunk) => {
                // `drain(..)` keeps the Vec's capacity; hand the emptied buffer back to the
                // callback's free-list (dropped only if the pool is momentarily full).
                ring.extend(chunk.drain(..));
                let _ = free_tx.try_send(chunk);
                // Drain whatever else queued while we were away, so a post-stall backlog lands as
                // ONE lump the self-heal below can size up — chunk-at-a-time it would be encoded
                // (and inflicted on the host as standing delay) before it ever looked deep.
                while let Ok(mut chunk) = rx.try_recv() {
                    ring.extend(chunk.drain(..));
                    let _ = free_tx.try_send(chunk);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue, // wake to re-check shutdown
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Self-heal the latency ratchet: a stall (scheduler hiccup, a slow send) queues stale
        // audio, and every ms of it would ride the stream as mic delay for the rest of the
        // session. Jump to the newest ~20 ms (one audible blip), counting the shed.
        if ring.len() > BACKLOG_MAX_FRAMES * frame {
            let excess = ring.len() - BACKLOG_KEEP_FRAMES * frame;
            ring.drain(..excess);
            stale += (excess / frame) as u64;
        }
        while ring.len() >= frame {
            for (dst, src) in pcm.iter_mut().zip(ring.drain(..frame)) {
                *dst = src;
            }
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
                    if sent % 500 == 0 {
                        log::info!(
                            "mic: sent={sent} captured_frames={} dropped_chunks={} \
                             stale_frames={stale} peak={peak:.3}",
                            captured.load(Ordering::Relaxed),
                            dropped.load(Ordering::Relaxed),
                        );
                        peak = 0.0;
                    }
                }
                Err(e) => log::debug!("mic: opus encode: {e}"),
            }
        }
    }
    log::info!(
        "mic: stopped (sent={sent} captured_frames={} dropped_chunks={} stale_frames={stale})",
        captured.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
    );
}
