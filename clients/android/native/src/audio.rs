//! Android audio playback (android-only): pull Opus packets from the connector, decode to
//! interleaved f32 (stereo or 5.1/7.1 surround), and feed AAudio (LowLatency) via its realtime data
//! callback through a jitter ring. Mirrors [`crate::decode`]: one thread we own (the Opus decode
//! producer) plus a shutdown flag; the realtime callback thread is owned by AAudio.
//!
//! The layout is the host-RESOLVED channel count (`NativeClient::audio_channels`, negotiated at
//! connect), so an older/clamping host that can only capture stereo is decoded + played as stereo.
//! 2 = stereo / 6 = 5.1 / 8 = 7.1, in the canonical wire order FL FR FC LFE RL RR SL SR.
//!
//! The ring started as a port of `punktfunk-client-linux/src/audio.rs`, but AAudio — unlike
//! PipeWire, which adaptively rate-matches the stream and absorbs a shallow buffer — hands us a raw
//! realtime callback and makes us own the buffer. So this client diverges deliberately to stop the
//! Android-only crackle: (1) the callback is allocation/free-free — decoded buffers are recycled to
//! the producer via a free-list instead of being freed on the audio thread (Android's Scudo `free`
//! has unbounded tail latency); (2) the jitter ring is deeper (~40 ms prime / ~150 ms hard cap) and
//! decoupled from the tiny LowLatency burst size, with de-prime hysteresis so a transient drain
//! doesn't manufacture a silence; (3) the AAudio HW buffer is primed above its 2-burst default and
//! grown on XRuns (Google's anti-glitch technique).

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder,
};
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

const SAMPLE_RATE: i32 = 48_000;
/// Decoded-chunk hand-off depth: 64 × 5 ms = 320 ms slack (matches the core's AUDIO_QUEUE).
const RING_CHUNKS: usize = 64;

// --- Jitter-ring depths, in MILLISECONDS (scaled to interleaved-f32 samples at runtime). --------
// The channel count is negotiated, not a compile-time const, so these are kept in ms and multiplied
// by `ms` (interleaved-f32 samples per millisecond at the resolved layout) inside `start`.
// Unlike the Linux client (PipeWire adaptively rate-matches the stream to the graph clock, masking
// host↔DAC drift + a shallow ring), AAudio hands us a raw callback and we own the buffer: drift and
// WiFi power-save bunching land as underruns/overflows = crackle. So Android runs a deliberately
// deeper, smoothly-managed ring than Linux — keep the two clients' depths intentionally divergent.
/// Prime/target floor: fill to ~40 ms before playing (and after a sustained drain). Deep enough to
/// ride out WiFi arrival jitter + clock drift; the dominant Android-only anti-crackle lever.
const PRIME_FLOOR_MS: usize = 40;
/// Ceiling for the burst-scaled target (so a large quantum can't push the prime depth too high).
const PRIME_CEIL_MS: usize = 80;
/// Drop-oldest headroom above the target before trimming — a ~80 ms band swallows an arrival burst
/// without overflowing.
const JITTER_HEADROOM_MS: usize = 80;
/// Hard latency bound: never let the ring exceed ~150 ms (the only thing that caps added latency).
const HARD_CAP_MS: usize = 150;
/// Re-prime (go silent to refill) only after this many CONSECUTIVE empty callbacks, so one transient
/// drain doesn't manufacture a fresh 40 ms silence (the old `if ring.is_empty()` re-primed instantly).
const DEPRIME_AFTER_CALLBACKS: u32 = 5;
/// Throttle the AAudio XRun-driven HW-buffer grow check (cheap, but no need to poll every quantum).
const XRUN_CHECK_EVERY: u32 = 128;

/// Opus decoder for the audio plane: a plain stereo decoder (the validated path) or a multistream
/// decoder for 5.1/7.1, both behind one `decode_float`. Built from the host-RESOLVED channel count
/// via the shared layout table. Mirrors the Linux client's `AudioDec`.
enum AudioDec {
    Stereo(opus::Decoder),
    Surround(opus::MSDecoder),
}

impl AudioDec {
    fn new(channels: u8) -> Result<AudioDec, opus::Error> {
        if channels == 2 {
            Ok(AudioDec::Stereo(opus::Decoder::new(
                SAMPLE_RATE as u32,
                opus::Channels::Stereo,
            )?))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            Ok(AudioDec::Surround(opus::MSDecoder::new(
                SAMPLE_RATE as u32,
                l.streams,
                l.coupled,
                l.mapping,
            )?))
        }
    }

    fn decode_float(
        &mut self,
        input: &[u8],
        out: &mut [f32],
        fec: bool,
    ) -> Result<usize, opus::Error> {
        match self {
            AudioDec::Stereo(d) => d.decode_float(input, out, fec),
            AudioDec::Surround(d) => d.decode_float(input, out, fec),
        }
    }
}

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
    /// Open AAudio (LowLatency, 48 kHz/f32, the host-resolved channel layout) with a realtime
    /// callback draining a jitter ring, then spawn the Opus decode thread. `None` on failure (the
    /// caller leaves video streaming).
    pub fn start(client: Arc<NativeClient>) -> Option<AudioPlayback> {
        // Build playback from the host-RESOLVED channel count (never the request): 2 = stereo /
        // 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR.
        let channels = punktfunk_core::audio::normalize_channels(client.audio_channels) as usize;
        // Interleaved f32 samples per millisecond at this layout (48 kHz × channels); the ms-
        // denominated jitter-ring depths scale by it.
        let ms = (SAMPLE_RATE as usize / 1000) * channels;
        let prime_floor = PRIME_FLOOR_MS * ms;
        let prime_ceil = PRIME_CEIL_MS * ms;
        let jitter_headroom = JITTER_HEADROOM_MS * ms;
        let hard_cap_max = HARD_CAP_MS * ms;
        let counters = Arc::new(Counters::default());

        // One open attempt at a given sharing mode. Everything the realtime callback captures
        // (channels, ring, prime state) is rebuilt per attempt — `open_stream` consumes the builder
        // AND the callback, so nothing survives a failed try to reuse.
        let try_open = |sharing: AudioSharingMode| -> ndk::audio::Result<(
            AudioStream,
            SyncSender<Vec<f32>>,
            Receiver<Vec<f32>>,
        )> {
            let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
            // Recycle free-list: drained PCM buffers go BACK to the decode thread to be refilled, so
            // the realtime callback never frees heap (Android's Scudo allocator has unbounded free()
            // tail latency — a free on the audio thread is an XRun = a click) and the decode thread
            // rarely allocates. Same depth as the data channel.
            let (free_tx, free_rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);

            // Realtime consumer state, owned by the callback (FnMut) — no lock: AAudio calls it from
            // a single high-priority thread, and the decode thread only touches `tx`/`free_rx`.
            let cb_counters = counters.clone();
            // Pre-reserve the ring so `extend` never reallocates on the realtime thread. Worst
            // transient before the trim below = the hard cap plus one full channel of 5 ms (480-f32)
            // frames — the punktfunk protocol always sends 5 ms Opus frames (host `audio_thread`); a
            // larger frame would force a one-time realloc, asserted (not silently corrupted) in
            // `decode_loop`.
            let mut ring: VecDeque<f32> =
                VecDeque::with_capacity(hard_cap_max + RING_CHUNKS * 5 * ms);
            let mut primed = false;
            let mut empties: u32 = 0; // consecutive empty callbacks (de-prime hysteresis)
            let mut cb_count: u32 = 0; // callbacks since open (throttles the XRun grow check)
            let mut last_xrun: i32 = 0; // last AAudio XRun count we grew the buffer for
            let callback = move |s: &AudioStream, data: *mut c_void, num_frames: i32| {
                let want = num_frames as usize * channels;
                // SAFETY: AAudio provides `num_frames * channel_count` F32 slots at `data`.
                let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, want) };
                // Drain decoded chunks into the ring WITHOUT freeing on the RT thread: `drain(..)`
                // empties each Vec but keeps its capacity, then the empty buffer is handed back for
                // reuse. The only RT-thread free is the rare case where the recycle channel is
                // momentarily full.
                while let Ok(mut chunk) = rx.try_recv() {
                    ring.extend(chunk.drain(..));
                    let _ = free_tx.try_send(chunk);
                }
                // Jitter buffer: prime to ~40 ms (prime_floor) before playing and after a sustained
                // drain; drop-oldest only above a wide ~120 ms band. Decoupled from the AAudio burst
                // `want` (tiny on the LowLatency MMAP path) so the depth doesn't collapse to a single
                // quantum.
                let target = (3 * want).clamp(prime_floor, prime_ceil);
                let hard_cap = (target + jitter_headroom).min(hard_cap_max);
                while ring.len() > hard_cap {
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
                // Re-prime only after a RUN of empty callbacks, not a single transient one —
                // otherwise every momentary drain costs a fresh 40 ms silence (the old behaviour,
                // self-inflicted crackle on any jitter spike).
                if ring.is_empty() {
                    empties += 1;
                    if empties >= DEPRIME_AFTER_CALLBACKS {
                        primed = false;
                    }
                } else {
                    empties = 0;
                }
                cb_counters
                    .ring_depth
                    .store(ring.len() as u64, Ordering::Relaxed);
                // Google's AAudio anti-glitch technique: when the device reports new XRuns, grow the
                // HW buffer by one burst (up to capacity). getXRunCount + setBufferSizeInFrames are
                // both callback-safe / non-blocking, and set clamps to capacity so it self-limits.
                // Throttled.
                cb_count = cb_count.wrapping_add(1);
                if cb_count % XRUN_CHECK_EVERY == 0 {
                    let xr = s.x_run_count();
                    if xr > last_xrun {
                        last_xrun = xr;
                        let burst = s.frames_per_burst().max(1);
                        let grown =
                            (s.buffer_size_in_frames() + burst).min(s.buffer_capacity_in_frames());
                        let _ = s.set_buffer_size_in_frames(grown);
                    }
                }
                AudioCallbackResult::Continue
            };

            let stream = AudioStreamBuilder::new()?
                .direction(AudioDirection::Output)
                .sample_rate(SAMPLE_RATE)
                // The wire order (FL FR FC LFE RL RR SL SR) is the standard AAudio/Android channel
                // order, so this is an IDENTITY mapping — no permute. AAudio infers the 5.1/7.1 mask
                // from `channel_count` (the ndk crate's builder exposes no setChannelMask); the host
                // captures + Opus-encodes in exactly this order.
                .channel_count(channels as i32)
                .format(AudioFormat::PCM_Float)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(sharing)
                .data_callback(Box::new(callback))
                .error_callback(Box::new(|_s, e| {
                    log::warn!("audio: AAudio error (device reroute/disconnect?): {e:?}");
                }))
                .open_stream()?;
            Ok((stream, tx, free_rx))
        };

        // Exclusive first — MMAP-exclusive is AAudio's lowest-latency path (once proven on-device it
        // may also allow lowering the jitter-ring depths above; those stay put pending crackle
        // testing) — and fall back to Shared when the device refuses (no MMAP, output claimed, …).
        // The started-log below prints the mode the device actually GRANTED (`share=`): AAudio may
        // still resolve an Exclusive request to Shared.
        let (stream, tx, free_rx) = match try_open(AudioSharingMode::Exclusive) {
            Ok(opened) => opened,
            Err(e) => {
                log::info!("audio: Exclusive open failed ({e}) — retrying Shared");
                match try_open(AudioSharingMode::Shared) {
                    Ok(opened) => opened,
                    Err(e) => {
                        log::error!("audio: open_stream: {e}");
                        return None;
                    }
                }
            }
        };

        if let Err(e) = stream.request_start() {
            log::error!("audio: request_start: {e}");
            return None;
        }
        // Lift the AAudio HW buffer off its brittle ~2-burst LowLatency default so a single late
        // callback doesn't immediately underrun; the in-callback XRun loop grows it further if the
        // device still glitches. set_buffer_size_in_frames clamps to capacity.
        let burst = stream.frames_per_burst().max(1);
        let _ =
            stream.set_buffer_size_in_frames((burst * 3).min(stream.buffer_capacity_in_frames()));
        // perf != LowLatency or rate != 48000 means AAudio silently fell to a resampled legacy path
        // (different burst behaviour) — surface it so the field can tell that apart from plain jitter.
        log::info!(
            "audio: AAudio started rate={} ch={} fmt={:?} perf={:?} share={:?} burst={} buf={}/{}",
            stream.sample_rate(),
            stream.channel_count(),
            stream.format(),
            stream.performance_mode(),
            stream.sharing_mode(),
            stream.frames_per_burst(),
            stream.buffer_size_in_frames(),
            stream.buffer_capacity_in_frames(),
        );

        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-audio".into())
            .spawn(move || decode_loop(client, tx, free_rx, sd, counters, channels))
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
/// Buffers come from (and return to) the realtime callback's recycle free-list so the steady state
/// is allocation-free on both threads.
fn decode_loop(
    client: Arc<NativeClient>,
    tx: SyncSender<Vec<f32>>,
    free_rx: Receiver<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
    counters: Arc<Counters>,
    channels: usize,
) {
    // Fold this Opus→AAudio thread into the client's hot-thread set so the ADPF session the decode
    // thread opens also keeps audio decode on a fast core (registered before the video pump's first
    // frame arrives, so it's captured when that session is created). No-op below API 33.
    client.register_hot_thread();
    // Interleaved f32 samples per millisecond at this layout — the ring's 5 ms reserve check below.
    let ms = (SAMPLE_RATE as usize / 1000) * channels;
    // Opus decode scratch: worst-case 120 ms frame (5760 samples/ch) × channels.
    let pcm_scratch = 5760 * channels;
    let mut dec = match AudioDec::new(channels as u8) {
        Ok(d) => d,
        Err(e) => {
            log::error!("audio: opus decoder init: {e} — audio disabled");
            return;
        }
    };
    let mut pcm = vec![0f32; pcm_scratch];
    let mut window_peak = 0f32; // loudest |sample| since the last log — tells a tone from silence
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_audio(Duration::from_millis(5)) {
            Ok(pkt) => match dec.decode_float(&pkt.data, &mut pcm, false) {
                Ok(samples) => {
                    let n = samples * channels;
                    for &s in &pcm[..n] {
                        window_peak = window_peak.max(s.abs());
                    }
                    // The ring's pre-reservation in `start` assumes the protocol's 5 ms (≤480-f32/ch)
                    // frames; a larger frame would force a one-time realloc on the RT thread. Catch a
                    // future host frame-size change here in debug, not as a silent audio glitch.
                    debug_assert!(
                        n <= 5 * ms,
                        "audio frame {n} f32 exceeds the 5 ms ring reserve"
                    );
                    let count = counters.opus_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                    // Reuse a recycled buffer if the callback handed one back; only allocate when the
                    // free-list is momentarily empty (startup / after a backpressure drop).
                    let mut buf = free_rx
                        .try_recv()
                        .unwrap_or_else(|_| Vec::with_capacity(pcm_scratch));
                    buf.clear();
                    buf.extend_from_slice(&pcm[..n]);
                    match tx.try_send(buf) {
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
