//! Desktop audio capture for the GameStream audio stream. On Linux: a PipeWire stream that by
//! default registers a host-owned **stream sink** claimed as the session's default output
//! (apps play into it directly — immune to hardware-sink churn; `PUNKTFUNK_STREAM_SINK=0`
//! falls back to recording the default sink's monitor). Either way the capture is delivered
//! as interleaved `f32` PCM at 48 kHz in the requested channel count (stereo, 5.1 or 7.1 —
//! GameStream surround order FL FR FC LFE RL RR [SL SR]). The audio data plane
//! (`gamestream::audio`) reframes this into fixed Opus frames, encodes, and sends it.

use anyhow::Result;

/// Opus/GameStream audio is 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo channel count — the default and the punktfunk/1 audio plane's fixed layout.
pub const CHANNELS: usize = 2;

/// Produces interleaved `f32` PCM at [`SAMPLE_RATE`] in the channel count it was opened
/// with. Lives on its own thread; never blocks the capture loop (drops if the consumer
/// falls behind).
pub trait AudioCapturer: Send {
    /// Block until the next chunk of interleaved samples is available (variable size). The
    /// caller reframes into fixed Opus frames. An **empty** chunk means "no samples right now"
    /// (e.g. a quiet sink that hit the internal idle timeout) — NOT an error: the caller keeps the
    /// capturer. `Err` is reserved for a genuinely dead capture thread, signalling the caller to
    /// reopen.
    fn next_chunk(&mut self) -> Result<Vec<f32>>;

    /// The interleaved channel count this capturer delivers (what it was opened with).
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// Discard any buffered chunks (called when a persistent capturer is reused for a new
    /// stream, so the client doesn't hear stale audio captured while idle). On Linux this is
    /// also the session-start hook: the stream-sink capturer re-claims the default sink here
    /// (see [`idle`](Self::idle)). Default: no-op.
    fn drain(&mut self) {}

    /// Called when a session parks the capturer back into the persistent slot: release
    /// session-scoped routing side effects while keeping the capture backend alive. On Linux
    /// the stream-sink capturer restores the user's default sink here, so host apps play to
    /// the real output again between streams; the claim returns with the next
    /// [`drain`](Self::drain) (reuse) or a fresh open. Default: no-op.
    fn idle(&mut self) {}
}

/// Open a live capturer for system output via PipeWire, asking for `channels` interleaved
/// channels. Default: a host-owned stream sink claimed as the default output (the sink
/// advertises exactly `channels`, so apps can produce real surround); with
/// `PUNKTFUNK_STREAM_SINK=0`, the default sink's monitor, where a sink with fewer channels
/// gets the missing positions filled with silence (zero upmix).
#[cfg(target_os = "linux")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    linux::PwAudioCapturer::open(channels).map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(target_os = "windows")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    // The capture thread runs the audio wiring plan itself (audio_control::wire_now) before
    // resolving its endpoint — a fresh plan per open, because Windows endpoints churn — and
    // parks the default playback device on the plan's loopback endpoint (a silent sink by
    // default: audio plays on the client only) until the capturer is dropped.
    wasapi_cap::WasapiLoopbackCapturer::open(channels)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_audio_capture(_channels: u32) -> Result<Box<dyn AudioCapturer>> {
    anyhow::bail!("audio capture requires Linux + PipeWire or Windows + WASAPI")
}

/// Park a capturer at session end. Linux: store it in the persistent slot so the next session
/// reuses it (no PipeWire thread churn). Windows: DROP it instead — closing the capture restores
/// the operator's default playback device (it was parked on the loopback sink for the stream's
/// lifetime, silencing the host), and a WASAPI reopen at the next session start is cheap and
/// re-runs the wiring plan against the then-current endpoints.
pub fn park_audio_capture(
    slot: &std::sync::Mutex<Option<Box<dyn AudioCapturer>>>,
    cap: Box<dyn AudioCapturer>,
) {
    if cfg!(target_os = "windows") {
        drop(cap);
    } else {
        *slot.lock().unwrap() = Some(cap);
    }
}

/// The inverse of [`AudioCapturer`]: a virtual microphone the host *produces*. It registers a
/// PipeWire `Audio/Source` node that host apps can record from; the host [`push`](Self::push)es
/// decoded client-mic PCM (interleaved `f32` at [`SAMPLE_RATE`]) into it, and PipeWire delivers
/// it to whichever app records the source — silence when no input is flowing. This is how the
/// client's microphone reaches host applications (mic passthrough).
///
/// **Liveness contract.** Both backends run a worker thread that CAN die under the host's feet
/// (Linux: the PipeWire daemon restarts with the session; Windows: the audio endpoint is
/// invalidated/removed). A dead backend must be observable — [`push`](Self::push) returns `false`
/// and [`alive`](Self::alive) turns false — so the owning [`MicPump`] drops the instance and
/// reopens. Before this contract existed, a single backend death left `push` feeding a dead
/// queue for the rest of the host's life: the historical "mic passthrough works on no host" bug.
pub trait VirtualMic: Send {
    /// Push one chunk of interleaved `f32` PCM. Non-blocking — drops if the backend is behind
    /// (mic audio is lossy/real-time; a stale chunk is worse than a dropped one). Returns
    /// `false` iff the backend is DEAD (worker thread gone) — the caller must reopen; a merely
    /// congested backend drops the chunk and returns `true`.
    fn push(&self, pcm: &[f32]) -> bool;

    /// Backend liveness without pushing data — lets an idle pump notice a death between
    /// sessions, so the mic is already healthy again when the next client connects.
    fn alive(&self) -> bool;

    /// Drop any buffered-but-unplayed audio. Called after an uplink gap (client muted,
    /// session ended) so a recorder never hears a stale burst when audio resumes.
    fn discard(&self);

    /// The interleaved channel count the source was opened with.
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// The adaptive de-jitter target (per-channel samples) the pump measured from uplink
    /// arrival jitter (see `mic_jitter`). A backend with a jitter ring primes around this PLUS
    /// one of its own consumer quanta: the ring must absorb arrival burstiness (the pump's
    /// number) and pull granularity (the backend's own), and neither may buy the other's depth
    /// — a 2048-frame recorder gets its one quantum, not three. Never called ⇒ the backend
    /// keeps its legacy fixed constants, which is also how `PUNKTFUNK_MIC_LEGACY_BUFFER=1`
    /// works (the pump simply never drives the target). Default: no ring, ignored.
    fn set_target_depth(&self, _samples_per_ch: usize) {}

    /// `(buffered, prime_target)` of the backend's jitter ring in per-channel samples — read
    /// by the pump's creep trim + telemetry. `None` while unknown (consumer not yet running,
    /// or a backend without a ring).
    fn depth(&self) -> Option<(usize, usize)> {
        None
    }

    /// Reset-on-read telemetry counters (see [`MicBackendStats`]). Default: all zero.
    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats::default()
    }
}

/// Reset-on-read counters a [`VirtualMic`] backend reports into the pump's periodic mic
/// telemetry line ("mic uplink health").
#[derive(Debug, Default, Clone, Copy)]
pub struct MicBackendStats {
    /// Full-drain re-prime arms: the ring emptied and gates on silence until the target depth
    /// rebuilds. One per talk spurt is normal; several per second mid-speech is the crackle.
    pub reprimes: u64,
    /// Per-channel samples dropped by the ring's overflow cap (drop-oldest).
    pub overflow_dropped: u64,
}

/// One-release escape hatch (docs: configuration → Audio / microphone):
/// `PUNKTFUNK_MIC_LEGACY_BUFFER=1` keeps the pre-adaptive fixed mic buffering — the pump never
/// drives the backend target (so the rings stay on their legacy constants: 48 ms prime /
/// 120 ms cap on Windows, the 3-quanta clamp on Linux) and never creep-trims depth.
pub(crate) fn mic_legacy_buffer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PUNKTFUNK_MIC_LEGACY_BUFFER").is_some_and(|v| v != "0"))
}

/// Open a virtual microphone with `channels` interleaved channels (1 or 2). Linux: a PipeWire
/// `Audio/Source`. Windows: writes into an existing virtual audio device's render endpoint (whose
/// capture endpoint apps see as a mic) — see [`wasapi_mic`].
#[cfg(target_os = "linux")]
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    linux::PwMicSource::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

#[cfg(target_os = "windows")]
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    // The render thread runs the wiring plan itself (audio_control::wire_now) to resolve — and,
    // via the plan's default-device changes, to RESERVE — its target endpoint.
    wasapi_mic::WasapiVirtualMic::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_virtual_mic(_channels: u32) -> Result<Box<dyn VirtualMic>> {
    anyhow::bail!("virtual mic requires Linux + PipeWire or Windows + a virtual audio device")
}

#[cfg(target_os = "windows")]
#[path = "audio/windows/audio_control.rs"]
mod audio_control;
#[cfg(target_os = "linux")]
mod linux;
// DualSense pad-audio endpoint provisioning + loopback capture (design: pad haptics/audio).
// pub(crate): the session layer queries endpoints by pad index and the CLI exposes the
// `pad-endpoint` devtest.
#[cfg(target_os = "windows")]
#[path = "audio/windows/pad_endpoint.rs"]
pub(crate) mod pad_endpoint;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_cap.rs"]
mod wasapi_cap;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_mic.rs"]
mod wasapi_mic;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/wiring_plan.rs"]
pub(crate) mod wiring_plan;

mod mic_jitter;
mod mic_pump;
pub use mic_pump::{MicFrame, MicPump};
