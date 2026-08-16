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

/// Highest boost `PUNKTFUNK_AUDIO_GAIN` will honour (+18 dB). Past this the soft knee is doing
/// essentially all the work and the result is a squashed signal, not a louder one — so a runaway
/// value (a stray `180` for `1.8`) is capped and said out loud rather than silently shipped.
const MAX_CAPTURE_GAIN: f32 = 8.0;

/// The operator's capture gain, shared by BOTH audio planes (`PUNKTFUNK_AUDIO_GAIN`, default
/// `1.0` = untouched).
///
/// **Why the host needs one at all.** WASAPI loopback is tapped UPSTREAM of the endpoint's master
/// volume, so turning the host's speaker slider up does nothing whatsoever to the level a client
/// receives. Before this, the native `punktfunk/1` plane had no gain of any kind, which left no
/// host-side way to raise a quiet desktop mix — the GameStream plane's knob was the only one, and
/// it applied to the wrong protocol.
///
/// Applied through [`punktfunk_core::audio::apply_gain`], whose soft knee replaces the hard
/// `clamp(-1.0, 1.0)` this used to be. That clamp is why boosting was a trap: it flat-tops peaks,
/// and flat tops are audible as harsh distortion long before the operator reaches the level they
/// were chasing.
///
/// ⚠ This is headroom, not loudness. It cannot close a peak-to-loudness gap against
/// already-limited broadcast content — that needs a real compressor with a time constant, which is
/// deliberately NOT what this is.
pub fn capture_gain() -> f32 {
    let raw: f32 = std::env::var("PUNKTFUNK_AUDIO_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    // A negative or non-finite gain is a typo, never an intent: it would invert or poison every
    // sample. Fall back to unity rather than shipping it.
    if !raw.is_finite() || raw <= 0.0 {
        if std::env::var("PUNKTFUNK_AUDIO_GAIN").is_ok() {
            tracing::warn!(
                "PUNKTFUNK_AUDIO_GAIN must be a positive number (1.0 = unchanged) — ignoring"
            );
        }
        return 1.0;
    }
    if raw > MAX_CAPTURE_GAIN {
        tracing::warn!(
            requested = raw,
            capped = MAX_CAPTURE_GAIN,
            "PUNKTFUNK_AUDIO_GAIN is above the +18 dB ceiling — capping"
        );
        return MAX_CAPTURE_GAIN;
    }
    raw
}

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

    /// [`next_chunk`](Self::next_chunk) with a caller-chosen upper bound on the wait.
    ///
    /// The encode loop owes the wire a frame every 5 ms whether or not capture has anything to
    /// say, and blocking here for as long as the capturer feels like is what made a capture hole
    /// cost far more than the audio it swallowed: nothing left the host for the hole's whole
    /// duration, so the client's de-jitter ring drained, underran and de-primed over a gap it
    /// could otherwise have ridden through (WP-B1). Expiry returns an **empty** chunk — the same
    /// "no samples right now" the idle timeout reports, and equally not an error.
    ///
    /// Backends with no bound of their own just delegate; they simply wake less precisely.
    fn next_chunk_within(&mut self, _budget: std::time::Duration) -> Result<Vec<f32>> {
        self.next_chunk()
    }

    /// The interleaved channel count this capturer delivers (what it was opened with).
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// The sample rate this capturer is **actually** delivering — which is not necessarily the
    /// one it was asked for (`design/hi-res-audio.md` §8.1).
    ///
    /// The whole hi-res feature turns on this distinction. Both backends can be handed a rate
    /// their endpoint does not really run at and will happily resample to it without an error:
    /// WASAPI's `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` reconciles our format with the engine's in
    /// whichever direction is needed (§4.3), and PipeWire's resampler does the same in the
    /// legacy monitor mode (§4.4). A host that reported its *request* would advertise 96 kHz in
    /// the `Welcome`, spend the bandwidth, and deliver interpolated 48 kHz — the same
    /// "label right, content wrong" class of bug as the HDR RB-swap, which survived a long time
    /// precisely because both ends audited clean.
    ///
    /// So the contract is: report what was granted, and let the caller decline. The default is
    /// the legacy rate, which is what every backend that has not been taught to negotiate one
    /// genuinely opens at.
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
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
/// channels at `rate_hz`. Default: a host-owned stream sink claimed as the default output (the
/// sink advertises exactly `channels`, so apps can produce real surround); with
/// `PUNKTFUNK_STREAM_SINK=0`, the default sink's monitor, where a sink with fewer channels
/// gets the missing positions filled with silence (zero upmix).
///
/// `rate_hz` is a REQUEST, exactly like `channels`. What the graph actually granted is read
/// back from [`AudioCapturer::sample_rate`] — see that method for why the difference matters.
#[cfg(target_os = "linux")]
pub fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    linux::PwAudioCapturer::open(channels, rate_hz).map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(target_os = "windows")]
pub fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    // The capture thread runs the audio wiring plan itself (audio_control::wire_now) before
    // resolving its endpoint — a fresh plan per open, because Windows endpoints churn — and
    // parks the default playback device on the plan's loopback endpoint (a silent sink by
    // default: audio plays on the client only) until the capturer is dropped.
    wasapi_cap::WasapiLoopbackCapturer::open(channels, rate_hz)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_audio_capture(_channels: u32, _rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
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
// DualSense pad-audio sink + capture, the Linux analogue of `pad_endpoint` below: the session
// layer mints per-pad sinks and the CLI exposes the `pad-sink-test` devtest.
#[cfg(target_os = "linux")]
pub(crate) use linux::pad_sink;
// DualSense pad-audio endpoint provisioning + loopback capture (design: pad haptics/audio).
// pub(crate): the session layer queries endpoints by pad index and the CLI exposes the
// `pad-endpoint` devtest.
#[cfg(target_os = "windows")]
#[path = "audio/windows/pad_endpoint.rs"]
pub(crate) mod pad_endpoint;
// `audio-probe` devtest — the S1–S3 spike measurements for the Windows audio-substrate design
// (mint Steam-driver instances, measure their render→capture / loopback paths).
#[cfg(target_os = "windows")]
#[path = "audio/windows/audio_probe.rs"]
pub(crate) mod audio_probe;
// The minted "Punktfunk Speakers/Microphone" provider — punktfunk-owned instances of Valve's
// streaming-audio drivers, the wiring plan's tier-0 (the audio-substrate program).
#[cfg(target_os = "windows")]
#[path = "audio/windows/minted.rs"]
pub(crate) mod minted;
// The uninstall sweep over every audio devnode the two providers above (and the probe) mint —
// pub(crate) for `driver uninstall --audio`, the installer's [UninstallRun] leg.
#[cfg(target_os = "windows")]
#[path = "audio/windows/devnode_cleanup.rs"]
pub(crate) mod devnode_cleanup;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_cap.rs"]
mod wasapi_cap;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_mic.rs"]
mod wasapi_mic;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/wiring_plan.rs"]
pub(crate) mod wiring_plan;
// Pure capture-loop policy, split out for the same reason `wiring_plan` is: it encodes field
// behaviour, so its tests must run on every platform's CI, not only Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/capture_policy.rs"]
pub(crate) mod capture_policy;

mod mic_jitter;
mod mic_pump;
pub use mic_pump::{MicFrame, MicPump};

/// The most recent audio wiring verdict — the LAST wiring pass's assignment on a Windows host,
/// `None` elsewhere or before the first pass. A read-only snapshot for the status API; never
/// triggers a pass.
#[cfg(target_os = "windows")]
pub(crate) fn wiring_snapshot() -> Option<wiring_plan::Wiring> {
    audio_control::last_wiring()
}
#[cfg(not(target_os = "windows"))]
pub(crate) fn wiring_snapshot() -> Option<wiring_plan::Wiring> {
    None
}
