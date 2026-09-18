//! Host audio capture, virtual microphone, and (Windows) wiring-plan facade.
//!
//! Linux default: a host-owned PipeWire stream sink claimed as the session default
//! (`PUNKTFUNK_STREAM_SINK=0` records the default sink's monitor). Windows: WASAPI
//! loopback of the wiring-plan endpoint. Capture is interleaved `f32` PCM; channel
//! count is the open request (GameStream order FL FR FC LFE RL RR [SL SR]).
//! `gamestream::audio` reframes it into Opus. Rate honesty: [`CaptureRate`] and
//! `design/hi-res-audio.md`. Isolated-session names: `design/gamescope-multiuser.md`.

use anyhow::Result;

/// Opus / GameStream rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Default for a backend that leaves `channels()` alone. Either plane opens 5.1 or 7.1 on request; capture clamps to 2, 6 or 8.
pub const CHANNELS: usize = 2;

/// Cap for `PUNKTFUNK_AUDIO_GAIN` (×8 = +18 dB). Past this the soft knee squashes
/// rather than boosts. A stray `180` (meant `1.8`) is capped and warned, not shipped.
const MAX_CAPTURE_GAIN: f32 = 8.0;

/// Operator capture gain (`PUNKTFUNK_AUDIO_GAIN`, default 1.0), shared by both audio planes.
///
/// WASAPI loopback is tapped upstream of the endpoint master volume, so the speaker
/// slider never reaches the client; this is the host-side knob. Applied through
/// [`punktfunk_core::audio::apply_gain`] (soft knee). Headroom, not loudness: it
/// cannot close a peak-to-loudness gap in already-limited content.
pub fn capture_gain() -> f32 {
    let raw: f32 = std::env::var("PUNKTFUNK_AUDIO_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    // Negative or non-finite would invert or poison every sample — unity, not the typo.
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

/// Live capture source. Own thread; the producer drops if the consumer falls behind
/// rather than block the capture loop.
pub trait AudioCapturer: Send {
    /// Block for the next interleaved chunk (variable size). Empty is idle, not death —
    /// keep the capturer. `Err` means the capture thread is gone; reopen.
    fn next_chunk(&mut self) -> Result<Vec<f32>>;

    /// [`next_chunk`](Self::next_chunk) bounded by `budget`.
    ///
    /// The encode loop owes the wire a frame every 5 ms. Blocking past that lets
    /// the client's de-jitter ring drain across a capture gap. Expiry returns empty —
    /// idle, not `Err`. Unbounded backends just delegate.
    fn next_chunk_within(&mut self, _budget: std::time::Duration) -> Result<Vec<f32>> {
        self.next_chunk()
    }

    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// `node.name` of the sink this capturer reads, for a `join` session to tap.
    /// `None` where capture follows the default output.
    fn sink_name(&self) -> Option<&str> {
        None
    }

    /// Rate the backend is actually delivering, not the one it was asked for
    /// (`design/hi-res-audio.md`). WASAPI AUTOCONVERTPCM and PipeWire's monitor
    /// resampler both succeed at a request they then interpolate. Report the granted
    /// rate so the caller can decline. Default: [`SAMPLE_RATE`], which un-negotiating
    /// backends open at.
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    /// Drop buffered chunks on reuse so a new stream does not hear idle capture. Linux
    /// stream-sink also re-claims the default sink here (pair: [`idle`](Self::idle)). Default: no-op.
    fn drain(&mut self) {}

    /// Session parked: drop routing side effects, keep the backend. Linux stream-sink
    /// restores the user's default sink; the claim returns on the next [`drain`](Self::drain)
    /// or a fresh open. Default: no-op.
    fn idle(&mut self) {}
}

/// What capture can honestly deliver, answered before a capturer exists
/// (`design/hi-res-audio.md`).
///
/// The handshake promises a rate in `Welcome` and the client opens its device at it;
/// the capturer starts later. Discovering a mismatch on the audio thread can only kill
/// the lossless plane, and silence is the one unacceptable outcome — so this is a
/// device-level query with no stream. Three-valued on purpose: "host declares the
/// rate" and "device runs at 48 kHz" have different consequences, and folding either
/// into unknown would decline hi-res on the one configuration that is honest by
/// construction (Linux stream-sink).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureRate {
    /// Host declares the rate; apps render into it natively — no upstream resampler.
    /// Linux stream-sink (the default).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Declared,
    /// Device mix rate. A request at another rate still succeeds by resampling, so only
    /// `rate ≤ this` is honest. Windows: WASAPI engine mix format. Linux: the monitor
    /// sink's rate under `PUNKTFUNK_STREAM_SINK=0`.
    #[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
    Engine(u32),
    /// Probe failed (gone/idle sink, unreachable endpoint) or no backend. Hi-res declines.
    Unknown,
}

impl CaptureRate {
    /// Whether a session at `rate_hz` would actually be captured at `rate_hz`.
    /// Unknown declines: advertising 96 kHz while delivering interpolated 48 kHz
    /// spends the bandwidth for no extra content.
    pub fn can_deliver(self, rate_hz: u32) -> bool {
        match self {
            CaptureRate::Declared => true,
            CaptureRate::Engine(hz) => rate_hz <= hz,
            CaptureRate::Unknown => false,
        }
    }
}

/// Honest deliverable rate, with no capture stream and no routing change — see [`CaptureRate`].
///
/// Blocking (Windows endpoint enum + `IAudioClient` activate; Linux PipeWire registry
/// round-trip). Callers on the async path run it off the reactor. Ordinary 48 kHz
/// sessions must not pay this.
pub fn probe_capture_rate() -> CaptureRate {
    plat::probe_capture_rate()
}

/// Open a live capturer for system output. Linux: a host-owned stream sink (or the default
/// sink's monitor under `PUNKTFUNK_STREAM_SINK=0`). Windows: WASAPI loopback of the wiring
/// plan's sink. `rate_hz` is a request; the grant is [`AudioCapturer::sample_rate`].
pub fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    plat::open_audio_capture(channels, rate_hz)
}

/// [`open_audio_capture`] pinned to a sink `node.name` (`design/gamescope-multiuser.md`):
/// gamescope apps get `PULSE_SINK` and we capture that sink's monitor. `None` =
/// [`open_audio_capture`]. `tap` captures a sink another session owns without minting it,
/// holding the default-sink claim on it until this session ends. Non-Linux ignores the name.
pub fn open_audio_capture_named(
    channels: u32,
    rate_hz: u32,
    sink: Option<&str>,
    tap: bool,
) -> Result<Box<dyn AudioCapturer>> {
    plat::open_audio_capture_named(channels, rate_hz, sink, tap)
}

/// Whether this host can mint a per-session sink. Linux stream/null-sink only;
/// monitor mode and other platforms share the default output.
pub fn per_session_sink_possible() -> bool {
    plat::per_session_sink_possible()
}

/// Park a capturer at session end. Linux: persist so the next session reuses the
/// PipeWire thread. Windows: drop — that restores the operator's default playback
/// device (parked on the loopback sink for the stream) and the next open re-runs
/// the wiring plan against then-current endpoints.
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

/// Inverse of [`AudioCapturer`]: a PipeWire `Audio/Source` (or Windows virtual render
/// endpoint) the host [`push`](Self::push)es decoded client-mic PCM into. Host apps
/// record it; silence when nothing is flowing.
///
/// Both backends' worker can die (PipeWire session restart; WASAPI endpoint gone).
/// [`push`](Self::push) then returns `false` and [`alive`](Self::alive) is false so
/// [`MicPump`] drops and reopens. A dead queue that still accepts `push` stays silent
/// for the rest of the host's life.
pub trait VirtualMic: Send {
    /// Non-blocking push of interleaved `f32`. Drops a stale chunk rather than block.
    /// `false` = worker dead, reopen; congested drop still returns `true`.
    fn push(&self, pcm: &[f32]) -> bool;

    /// Liveness without a push — an idle pump can reopen between sessions.
    fn alive(&self) -> bool;

    /// Drop unplayed audio after an uplink gap so a recorder never hears a stale burst.
    fn discard(&self);

    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// Adaptive de-jitter target in per-channel samples (see `mic_jitter`). A ring primes
    /// to this PLUS one of its own consumer quanta: arrival burstiness is the pump's
    /// number, pull granularity is the backend's, and a 2048-frame recorder gets one
    /// quantum, not three. Never called ⇒ legacy constants (`PUNKTFUNK_MIC_LEGACY_BUFFER=1`
    /// is the pump never driving this). Default: no ring, ignored.
    fn set_target_depth(&self, _samples_per_ch: usize) {}

    /// `(buffered, prime_target)` of the jitter ring, per-channel samples. `None` until
    /// the consumer is running, or if the backend has no ring.
    fn depth(&self) -> Option<(usize, usize)> {
        None
    }

    /// Reset-on-read backend counters (see [`MicBackendStats`]).
    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats::default()
    }
}

/// Reset-on-read counters for the pump's "mic uplink health" line.
#[derive(Debug, Default, Clone, Copy)]
pub struct MicBackendStats {
    /// Full-drain re-primes: ring emptied, gates on silence until target rebuilds.
    /// One per talk spurt is normal; several per second mid-speech is crackle.
    pub reprimes: u64,
    /// Per-channel samples dropped by the ring's drop-oldest overflow cap.
    pub overflow_dropped: u64,
}

/// `PUNKTFUNK_MIC_LEGACY_BUFFER=1`: pump never drives the backend target (rings stay
/// on 48 ms prime / 120 ms cap on Windows, 3-quanta clamp on Linux) and never
/// creep-trims depth.
pub(crate) fn mic_legacy_buffer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PUNKTFUNK_MIC_LEGACY_BUFFER").is_some_and(|v| v != "0"))
}

/// Open a virtual mic (1 or 2 channels). Linux: PipeWire `Audio/Source`. Windows:
/// render into a virtual device whose capture side apps see as a mic.
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    plat::open_virtual_mic(channels)
}

/// [`open_virtual_mic`] pinned to a source `node.name` (`design/gamescope-multiuser.md`:
/// `punktfunk-mic-{id}`, gamescope `PULSE_SOURCE`). `None` = shared `punktfunk-mic`.
/// Other platforms ignore the name.
pub fn open_virtual_mic_named(channels: u32, source: Option<&str>) -> Result<Box<dyn VirtualMic>> {
    plat::open_virtual_mic_named(channels, source)
}

#[cfg(target_os = "windows")]
mod windows;

/// `punktfunk-host voice-route …`: writes the per-app output pins in whatever user context
/// it runs in — the capture thread spawns it as the console user ([`windows::voice_route`]).
#[cfg(target_os = "windows")]
pub(crate) fn voice_route_cli(args: &[String]) -> anyhow::Result<()> {
    windows::voice_route::cli(args)
}
#[cfg(target_os = "windows")]
use self::windows as plat;
// Flat names for the session, the devtests and the installer: `crate::audio::pad_endpoint`.
#[cfg(target_os = "windows")]
pub(crate) use self::windows::{
    audio_control, audio_probe, devnode_cleanup, minted, pad_capture, pad_endpoint,
};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use self::linux as plat;
// DualSense pad-audio sink + capture (Linux analogue of `pad_endpoint`): session
// mints per-pad sinks; CLI `pad-sink-test`. USB DualSense: capture the isochronous
// endpoint instead of minting a PipeWire node.
#[cfg(target_os = "linux")]
pub(crate) use linux::{pad_sink, pad_usb};
/// No capture backend: `open_audio_capture` bails, so there is no rate to promise either.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod plat {
    use super::*;
    pub(super) fn probe_capture_rate() -> CaptureRate {
        CaptureRate::Unknown
    }
    pub(super) fn open_audio_capture(
        _channels: u32,
        _rate_hz: u32,
    ) -> Result<Box<dyn AudioCapturer>> {
        anyhow::bail!("audio capture requires Linux + PipeWire or Windows + WASAPI")
    }
    pub(super) fn open_audio_capture_named(
        channels: u32,
        rate_hz: u32,
        _sink: Option<&str>,
        _tap: bool,
    ) -> Result<Box<dyn AudioCapturer>> {
        open_audio_capture(channels, rate_hz)
    }
    pub(super) fn per_session_sink_possible() -> bool {
        false
    }
    pub(super) fn open_virtual_mic(_channels: u32) -> Result<Box<dyn VirtualMic>> {
        anyhow::bail!("virtual mic requires Linux + PipeWire or Windows + a virtual audio device")
    }
    pub(super) fn open_virtual_mic_named(
        channels: u32,
        _source: Option<&str>,
    ) -> Result<Box<dyn VirtualMic>> {
        open_virtual_mic(channels)
    }
    pub(super) fn wiring_snapshot() -> Option<wiring_plan::Wiring> {
        None
    }
}
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/wiring_plan.rs"]
pub(crate) mod wiring_plan;
// Capture-loop policy, split out like `wiring_plan`: tests must run on every
// platform's CI, not only Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/capture_policy.rs"]
pub(crate) mod capture_policy;

mod mic_jitter;
mod mic_pump;
pub use mic_pump::{MicFrame, MicPump};

/// Apps playing audio on the host right now, lowercased. Empty where the host cannot list them.
/// Blocks on a PipeWire round trip; call it off the async runtime.
pub(crate) fn playing_apps() -> Vec<String> {
    #[cfg(target_os = "linux")]
    return linux::playing_apps().unwrap_or_else(|e| {
        tracing::debug!(error = %format!("{e:#}"), "playing apps not listed");
        Vec::new()
    });
    #[cfg(not(target_os = "linux"))]
    Vec::new()
}

/// Last wiring-pass assignment on Windows; `None` elsewhere or before the first pass.
/// Read-only for the status API — never triggers a pass.
pub(crate) fn wiring_snapshot() -> Option<wiring_plan::Wiring> {
    plat::wiring_snapshot()
}
