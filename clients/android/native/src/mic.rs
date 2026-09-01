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
//!
//! **Mute** is a flag the encode loop reads per 10 ms frame, never a stream teardown: the AAudio
//! input stream, the input-preset ladder it settled on and its primed buffers all survive a
//! mute/unmute untouched, so toggling costs an atomic load and nothing else.

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

/// What one capture open attempt yields: the stream, plus both halves of the PCM hand-off — the
/// receiver the encode worker drains and the sender that returns emptied buffers for reuse. Note
/// the pair is the mirror image of [`crate::audio::OpenedPlayback`]'s: here the callback produces
/// and the worker consumes.
///
/// Named rather than written inline for the same reason as that one — `clippy::type_complexity`,
/// now that the Android target is actually linted (`:kit:cargoNdkClippy`).
type OpenedCapture = ndk::audio::Result<(AudioStream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>)>;

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

// SAFETY: Rust accesses the stream only through this unique owner. Drop first stops and joins the
// encode worker, then AAudio closes the stream and quiesces its own callback; it may run on any
// non-callback JNI thread. The session mutex prevents concurrent application-side stream access.
unsafe impl Send for MicCapture {}

impl MicCapture {
    /// Open low-latency 48 kHz mono capture and start the Opus uplink worker.
    ///
    /// `echo_cancel` selects the voice-communication preset and an audio session for Kotlin's
    /// effects. The realtime callback only copies into recycled buffers and validates every foreign
    /// pointer/length pair before slicing. `muted` remains session-owned across capture restarts.
    /// Returns `None` without disturbing the rest of the stream when setup fails.
    pub fn start(
        client: Arc<NativeClient>,
        echo_cancel: bool,
        muted: Arc<AtomicBool>,
    ) -> Option<MicCapture> {
        let captured = Arc::new(AtomicU64::new(0));
        // Chunks discarded on the capture thread (free-list empty / encoder lagging); logged
        // throttled from the encode worker.
        let dropped = Arc::new(AtomicU64::new(0));

        // One open attempt at a given sharing mode (same pattern as [`crate::audio`]: `open_stream`
        // consumes the builder AND the callback, so each try rebuilds the channels it captures).
        let try_open = |sharing: AudioSharingMode, voice: bool| -> OpenedCapture {
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
                let Some(n) = crate::audio_format::callback_sample_count(num_frames, CHANNELS)
                else {
                    return AudioCallbackResult::Continue;
                };
                if data.is_null() {
                    return AudioCallbackResult::Stop;
                }
                // SAFETY: AAudio supplies `n` captured f32 samples at non-null `data`; the shared
                // length validator rejected nonpositive or unrepresentable callback counts.
                let inp = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), n) };
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
        let join = match std::thread::Builder::new()
            .name("pf-mic".into())
            .spawn(move || encode_loop(client, rx, free_tx, sd, muted, captured, dropped))
        {
            Ok(join) => join,
            Err(e) => {
                log::error!("mic: encode thread spawn failed: {e}");
                return None;
            }
        };

        Some(MicCapture {
            _stream: stream,
            session_id,
            shutdown,
            join: Some(join),
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
///
/// While `muted` is set a formed frame is dropped instead of encoded (see the frame loop) — the
/// capture side keeps running exactly as it does unmuted, so nothing about the stream, its ring or
/// its backlog behaviour changes across a toggle.
fn encode_loop(
    client: Arc<NativeClient>,
    rx: Receiver<Vec<f32>>,
    free_tx: SyncSender<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
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
    let mut muted_frames: u64 = 0; // frames dropped unencoded because the user muted
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
            // Muted: drop the frame at the last point before it would become an Opus packet —
            // room audio is never encoded and nothing goes on the wire. `seq` does NOT advance:
            // it numbers the datagrams the host de-jitters, and that side reads a seq jump as
            // loss (conceal + a counted gap) where a mute is a pause. Freezing it means the
            // frame after an unmute continues the chain, which is what the host's own
            // `reset_stream` doc calls for and what the desktop uplink does. (Encoding silence
            // instead would keep a pointless uplink and a host-side ring alive for the whole
            // mute.) `peak` is the loudest sample the UPLINK carried since the last log, so a
            // dropped frame resets rather than raises it.
            if muted.load(Ordering::Relaxed) {
                ring.drain(..frame);
                muted_frames += 1;
                peak = 0.0;
                continue;
            }
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
                             stale_frames={stale} muted_frames={muted_frames} peak={peak:.3}",
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
        "mic: stopped (sent={sent} captured_frames={} dropped_chunks={} stale_frames={stale} \
         muted_frames={muted_frames})",
        captured.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
    );
}
