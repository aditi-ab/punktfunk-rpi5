//! Desktop audio capture for the GameStream audio stream. On Linux: a PipeWire stream that
//! records the default sink's monitor (i.e. everything playing out of the system), delivered
//! as interleaved `f32` PCM at 48 kHz in the requested channel count (stereo, 5.1 or 7.1 —
//! GameStream surround order FL FR FC LFE RL RR [SL SR]). The audio data plane
//! (`gamestream::audio`) reframes this into fixed Opus frames, encodes, and sends it.

use anyhow::Result;

/// Opus/GameStream audio is 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo channel count — the default and the punktfunk/1 (M3) audio plane's fixed layout.
pub const CHANNELS: usize = 2;

/// Produces interleaved `f32` PCM at [`SAMPLE_RATE`] in the channel count it was opened
/// with. Lives on its own thread; never blocks the capture loop (drops if the consumer
/// falls behind).
pub trait AudioCapturer: Send {
    /// Block until the next chunk of interleaved samples is available (variable size). The
    /// caller reframes into fixed Opus frames.
    fn next_chunk(&mut self) -> Result<Vec<f32>>;

    /// The interleaved channel count this capturer delivers (what it was opened with).
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// Discard any buffered chunks (called when a persistent capturer is reused for a new
    /// stream, so the client doesn't hear stale audio captured while idle). Default: no-op.
    fn drain(&mut self) {}
}

/// Open a live capturer for the default sink monitor (system output) via PipeWire, asking
/// for `channels` interleaved channels. If the sink has fewer channels than requested,
/// PipeWire's channel-mixer fills the missing positions with silence (zero upmix).
#[cfg(target_os = "linux")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    linux::PwAudioCapturer::open(channels).map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(target_os = "windows")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    wasapi_cap::WasapiLoopbackCapturer::open(channels)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_audio_capture(_channels: u32) -> Result<Box<dyn AudioCapturer>> {
    anyhow::bail!("audio capture requires Linux + PipeWire or Windows + WASAPI")
}

/// The inverse of [`AudioCapturer`]: a virtual microphone the host *produces*. It registers a
/// PipeWire `Audio/Source` node that host apps can record from; the host [`push`](Self::push)es
/// decoded client-mic PCM (interleaved `f32` at [`SAMPLE_RATE`]) into it, and PipeWire delivers
/// it to whichever app records the source — silence when no input is flowing. This is how the
/// client's microphone reaches host applications (mic passthrough).
pub trait VirtualMic: Send {
    /// Push one chunk of interleaved `f32` PCM. Non-blocking — drops if PipeWire is behind
    /// (mic audio is lossy/real-time; a stale chunk is worse than a dropped one).
    fn push(&self, pcm: &[f32]);

    /// The interleaved channel count the source was opened with.
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }
}

/// Open a virtual microphone PipeWire source with `channels` interleaved channels (1 or 2).
#[cfg(target_os = "linux")]
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    linux::PwMicSource::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

#[cfg(not(target_os = "linux"))]
pub fn open_virtual_mic(_channels: u32) -> Result<Box<dyn VirtualMic>> {
    anyhow::bail!("virtual mic requires Linux + PipeWire")
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod wasapi_cap;
