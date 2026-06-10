//! Desktop audio capture for the GameStream audio stream. On Linux: a PipeWire stream that
//! records the default sink's monitor (i.e. everything playing out of the system), delivered
//! as interleaved `f32` stereo PCM at 48 kHz. The audio data plane (`gamestream::audio`)
//! reframes this into fixed Opus frames, encodes, and sends it.

use anyhow::Result;

/// Opus/GameStream audio is 48 kHz stereo.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// Produces interleaved `f32` stereo PCM (L,R,L,R,…) at [`SAMPLE_RATE`]. Lives on its own
/// thread; never blocks the capture loop (drops if the consumer falls behind).
pub trait AudioCapturer: Send {
    /// Block until the next chunk of interleaved samples is available (variable size). The
    /// caller reframes into fixed Opus frames.
    fn next_chunk(&mut self) -> Result<Vec<f32>>;

    /// Discard any buffered chunks (called when a persistent capturer is reused for a new
    /// stream, so the client doesn't hear stale audio captured while idle). Default: no-op.
    fn drain(&mut self) {}
}

/// Open a live capturer for the default sink monitor (system output) via PipeWire.
#[cfg(target_os = "linux")]
pub fn open_audio_capture() -> Result<Box<dyn AudioCapturer>> {
    linux::PwAudioCapturer::open().map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(not(target_os = "linux"))]
pub fn open_audio_capture() -> Result<Box<dyn AudioCapturer>> {
    anyhow::bail!("audio capture requires Linux + PipeWire")
}

#[cfg(target_os = "linux")]
mod linux;
