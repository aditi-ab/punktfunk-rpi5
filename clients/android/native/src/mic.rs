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
//! mute/unmute untouched, so toggling costs an atomic load and nothing else. A disconnect (a
//! headset plugged or pulled) reopens the stream on the same encoder; see [`supervise`].

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
use std::time::Duration;

/// What one capture open attempt yields: the stream, plus both halves of the PCM hand-off — the
/// receiver the encode worker drains and the sender that returns emptied buffers for reuse. Note
/// the pair is the mirror image of [`crate::audio::OpenedPlayback`]'s: here the callback produces
/// and the worker consumes.
///
/// Named rather than written inline for the same reason as that one — `clippy::type_complexity`,
/// now that the Android target is actually linted (`:kit:cargoNdkClippy`).
type OpenedCapture = ndk::audio::Result<(AudioStream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>)>;
/// A started stream from [`open_started`]: the capture plus the session id AAudio allocated.
type StartedCapture = (AudioStream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>, i32);

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
/// Settling time between a disconnect and the reopen: a headset handing over the route.
const REOPEN_SETTLE_MS: u64 = 250;
/// Reopens that may find no input before the mic gives up: ~2 s, past a Bluetooth handover.
const REOPEN_ATTEMPTS: u32 = 8;

/// Owned by [`crate::session::SessionHandle`]: the encode thread, which owns the AAudio input
/// stream (see [`supervise`]). Dropping this stops the thread, and the thread closes the stream.
pub struct MicCapture {
    /// The audio-session id AAudio allocated (`> 0`) when echo cancellation asked for one — the
    /// hook Kotlin hangs the Java `AcousticEchoCanceler`/`NoiseSuppressor` on. `0` = none. The
    /// first open's; a reopen after a disconnect keeps the HAL preset but not the Java effects.
    session_id: i32,
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Why [`Uplink::run`] returned.
#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Shutdown,
    /// AAudio reported the stream disconnected; it is dead by contract, reopen.
    Disconnected,
}

/// The open ladder: Exclusive first — MMAP-exclusive is AAudio's lowest-latency path — falling
/// back to Shared when the device refuses (no MMAP, mic claimed, …); and each sharing mode with
/// the voice preset before without it, because some HALs reject VoiceCommunication (or a session
/// id) outright and a mic without echo cancellation still beats no mic. The started-log prints
/// what the device actually GRANTED (`share=`/`session=`).
fn capture_rungs(echo_cancel: bool) -> &'static [(AudioSharingMode, bool)] {
    if echo_cancel {
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
    }
}

/// One open attempt at a given sharing mode (same pattern as [`crate::audio`]: `open_stream`
/// consumes the builder AND the callback, so each try rebuilds the channels it captures).
/// `captured`/`dropped` are the counters the realtime callback bumps.
fn open_capture(
    sharing: AudioSharingMode,
    voice: bool,
    captured: &Arc<AtomicU64>,
    dropped: &Arc<AtomicU64>,
    disconnected: &Arc<AtomicBool>,
) -> OpenedCapture {
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
        let Some(n) = crate::audio_format::callback_sample_count(num_frames, CHANNELS) else {
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
    let cb_disconnected = disconnected.clone();
    let stream = builder
        .data_callback(Box::new(callback))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("mic: AAudio error (device reroute/disconnect?): {e:?}");
            cb_disconnected.store(true, Ordering::SeqCst);
        }))
        .open_stream()?;
    Ok((stream, rx, free_tx))
}

/// Walk the ladder and start the first stream that opens: the stream, its hand-off pair, and
/// the session id AAudio allocated (`0` = none). `None` = every rung refused, or start failed.
fn open_started(
    echo_cancel: bool,
    captured: &Arc<AtomicU64>,
    dropped: &Arc<AtomicU64>,
    disconnected: &Arc<AtomicBool>,
) -> Option<StartedCapture> {
    let mut opened = None;
    for &(sharing, voice) in capture_rungs(echo_cancel) {
        match open_capture(sharing, voice, captured, dropped, disconnected) {
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
    let (stream, rx, free_tx) = opened?;
    // `> 0` is the handle Kotlin hangs the Java AcousticEchoCanceler/NoiseSuppressor off as the
    // HAL preset's backstop; only a voice rung asks for one.
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
    Some((stream, rx, free_tx, session_id))
}

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
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        // The worker opens the stream and owns it for life, so it never crosses threads; the
        // first open's outcome comes back here for the session id.
        let (ready_tx, ready_rx) = sync_channel::<Option<i32>>(1);
        let join = match std::thread::Builder::new()
            .name("pf-mic".into())
            .spawn(move || supervise(client, echo_cancel, muted, sd, ready_tx))
        {
            Ok(join) => join,
            Err(e) => {
                log::error!("mic: encode thread spawn failed: {e}");
                return None;
            }
        };
        match ready_rx.recv() {
            Ok(Some(session_id)) => Some(MicCapture {
                session_id,
                shutdown,
                join: Some(join),
            }),
            _ => {
                let _ = join.join();
                None
            }
        }
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
            let _ = j.join(); // the worker stops and closes the AAudio stream on its way out
        }
    }
}

/// Own the input stream for the session: open it, run the uplink against it, and open it again
/// when AAudio disconnects it (a headset plugged or pulled, Bluetooth taking the route). The
/// stream is dead by AAudio's contract then; without a reopen the mic stayed silent while the
/// session still offered its mute. The encoder and `seq` carry over, so the host sees one stream.
fn supervise(
    client: Arc<NativeClient>,
    echo_cancel: bool,
    muted: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    ready: SyncSender<Option<i32>>,
) {
    // Fold this Opus-encode/uplink thread into the client's hot-thread set so the ADPF session the
    // decode thread opens keeps mic encode on a fast core too (the playback side's decode_loop
    // does the same). No-op below API 33.
    client.register_hot_thread();
    let Some(mut up) = Uplink::new() else {
        let _ = ready.send(None);
        return;
    };
    let captured = Arc::new(AtomicU64::new(0));
    // Chunks discarded on the capture thread (free-list empty / encoder lagging); logged
    // throttled from the encode loop.
    let dropped = Arc::new(AtomicU64::new(0));
    let mut ready = Some(ready);
    let mut attempts: u32 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let Some((stream, rx, free_tx, session_id)) =
            open_started(echo_cancel, &captured, &dropped, &disconnected)
        else {
            if let Some(r) = ready.take() {
                log::error!("mic: open_stream (RECORD_AUDIO granted?): every mode refused");
                let _ = r.send(None);
                return;
            }
            attempts += 1;
            if attempts > REOPEN_ATTEMPTS {
                log::warn!("mic: no input device after the disconnect — the mic stays off");
                break;
            }
            crate::audio::nap(&shutdown, REOPEN_SETTLE_MS);
            continue;
        };
        attempts = 0;
        match ready.take() {
            Some(r) => {
                let _ = r.send(Some(session_id));
            }
            None => log::info!("mic: reopened after a disconnect (session={session_id})"),
        }
        let exit = up.run(
            &client,
            &rx,
            &free_tx,
            &shutdown,
            &disconnected,
            &muted,
            &captured,
            &dropped,
        );
        let _ = stream.request_stop();
        drop(stream); // → AAudioStream_close
        if exit == Exit::Shutdown {
            break;
        }
        log::warn!("mic: input disconnected — reopening in {REOPEN_SETTLE_MS} ms");
        crate::audio::nap(&shutdown, REOPEN_SETTLE_MS);
    }
    log::info!(
        "mic: stopped (sent={} captured_frames={} dropped_chunks={} stale_frames={} \
         muted_frames={})",
        up.sent,
        captured.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        up.stale,
        up.muted_frames,
    );
}

/// The encode side, kept across reopens: encoder state, the sequence the host de-jitters, and
/// the counters. Drained chunk buffers go back to the callback's free-list; the encode scratch is
/// reused across frames (only the packet Vec handed to `send_mic` is allocated per frame).
struct Uplink {
    enc: opus::Encoder,
    ring: VecDeque<f32>,
    pcm: Vec<f32>,
    out: Vec<u8>,
    seq: u32,
    sent: u64,
    /// Frames shed by the backlog self-heal (see [`BACKLOG_MAX_FRAMES`]).
    stale: u64,
    /// Frames dropped unencoded because the user muted.
    muted_frames: u64,
    /// Loudest |sample| since the last log — tells speech from silence.
    peak: f32,
}

impl Uplink {
    fn new() -> Option<Uplink> {
        let mut enc = match opus::Encoder::new(
            SAMPLE_RATE as u32,
            opus::Channels::Mono,
            opus::Application::Voip,
        ) {
            Ok(e) => e,
            Err(e) => {
                log::error!("mic: opus encoder init: {e} — mic disabled");
                return None;
            }
        };
        // Speech tuning: complexity 5 roughly halves encode cost for no audible loss at this rate,
        // and in-band FEC at an assumed 10% loss lets the host's decoder reconstruct a dropped
        // datagram from its successor instead of playing a hole (the uplink is fire-and-forget).
        // A refused setter leaves the encoder on libopus defaults — audible, so say which.
        for (what, r) in [
            ("bitrate", enc.set_bitrate(opus::Bitrate::Bits(MIC_BITRATE))),
            ("complexity", enc.set_complexity(5)),
            ("inband_fec", enc.set_inband_fec(true)),
            ("packet_loss_perc", enc.set_packet_loss_perc(10)),
        ] {
            if let Err(e) = r {
                log::warn!("mic: opus {what} not applied: {e}");
            }
        }
        let frame = FRAME_SAMPLES * CHANNELS;
        Some(Uplink {
            enc,
            ring: VecDeque::with_capacity(frame * 4),
            pcm: vec![0f32; frame],
            out: vec![0u8; 4000], // max Opus packet for a 10 ms frame fits easily
            seq: 0,
            sent: 0,
            stale: 0,
            muted_frames: 0,
            peak: 0.0,
        })
    }

    /// Consumer for one stream: drain captured f32 → accumulate → Opus `encode_float` 10 ms mono
    /// frames → `send_mic`, until shutdown or a disconnect.
    ///
    /// While `muted` is set a formed frame is dropped instead of encoded (see the frame loop) — the
    /// capture side keeps running exactly as it does unmuted, so nothing about the stream, its ring
    /// or its backlog behaviour changes across a toggle.
    #[allow(clippy::too_many_arguments)] // one call site, `supervise`
    fn run(
        &mut self,
        client: &NativeClient,
        rx: &Receiver<Vec<f32>>,
        free_tx: &SyncSender<Vec<f32>>,
        shutdown: &AtomicBool,
        disconnected: &AtomicBool,
        muted: &AtomicBool,
        captured: &AtomicU64,
        dropped: &AtomicU64,
    ) -> Exit {
        let frame = FRAME_SAMPLES * CHANNELS;
        while !shutdown.load(Ordering::Relaxed) {
            if disconnected.load(Ordering::SeqCst) {
                return Exit::Disconnected;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(mut chunk) => {
                    // `drain(..)` keeps the Vec's capacity; hand the emptied buffer back to the
                    // callback's free-list (dropped only if the pool is momentarily full).
                    self.ring.extend(chunk.drain(..));
                    let _ = free_tx.try_send(chunk);
                    // Drain whatever else queued while we were away, so a post-stall backlog
                    // lands as ONE lump the self-heal below can size up — chunk-at-a-time it
                    // would be encoded (and inflicted on the host as standing delay) before it
                    // ever looked deep.
                    while let Ok(mut chunk) = rx.try_recv() {
                        self.ring.extend(chunk.drain(..));
                        let _ = free_tx.try_send(chunk);
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue, // wake to re-check shutdown/disconnect
                // The callback dropped its sender: the stream is gone.
                Err(RecvTimeoutError::Disconnected) => return Exit::Disconnected,
            }
            // Self-heal the latency ratchet: a stall (scheduler hiccup, a slow send) queues stale
            // audio, and every ms of it would ride the stream as mic delay for the rest of the
            // session. Jump to the newest ~20 ms (one audible blip), counting the shed.
            if self.ring.len() > BACKLOG_MAX_FRAMES * frame {
                let excess = self.ring.len() - BACKLOG_KEEP_FRAMES * frame;
                self.ring.drain(..excess);
                self.stale += (excess / frame) as u64;
            }
            while self.ring.len() >= frame {
                // Muted: drop the frame before it becomes an Opus packet; nothing goes on the wire.
                // `seq` does NOT advance: the host reads a seq jump as loss where a mute is a pause,
                // so the frame after an unmute continues the chain. `peak` is what the UPLINK
                // carried, so a dropped frame resets it.
                if muted.load(Ordering::Relaxed) {
                    self.ring.drain(..frame);
                    self.muted_frames += 1;
                    self.peak = 0.0;
                    continue;
                }
                for (dst, src) in self.pcm.iter_mut().zip(self.ring.drain(..frame)) {
                    *dst = src;
                }
                for &s in &self.pcm {
                    self.peak = self.peak.max(s.abs());
                }
                match self.enc.encode_float(&self.pcm, &mut self.out) {
                    Ok(len) => {
                        let pts = punktfunk_core::quic::wall_clock_ns();
                        let _ = client.send_mic(self.seq, pts, self.out[..len].to_vec());
                        self.seq = self.seq.wrapping_add(1);
                        self.sent += 1;
                        if self.sent % 500 == 0 {
                            log::info!(
                                "mic: sent={} captured_frames={} dropped_chunks={} \
                                 stale_frames={} muted_frames={} peak={:.3}",
                                self.sent,
                                captured.load(Ordering::Relaxed),
                                dropped.load(Ordering::Relaxed),
                                self.stale,
                                self.muted_frames,
                                self.peak,
                            );
                            self.peak = 0.0;
                        }
                    }
                    Err(e) => log::debug!("mic: opus encode: {e}"),
                }
            }
        }
        Exit::Shutdown
    }
}
