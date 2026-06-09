//! Hardware video encode (plan §7). Binds FFmpeg (NVENC); never rewrites codecs.
//! Low-latency preset, B-frames off. M0 feeds BGRx CPU frames directly — `*_nvenc`
//! accepts `bgr0` input and converts to YUV on the GPU, so no host-side swscale is
//! needed (dmabuf zero-copy import is deferred; plan §9).

use crate::capture::{CapturedFrame, PixelFormat};
use anyhow::Result;

/// An encoded access unit (one NAL/AU) to hand to `lumen_core` for FEC + packetization.
/// `data` is in-band Annex-B (the encoder is opened without a global header), so each
/// keyframe carries its own VPS/SPS/PPS — the bytes are both a playable elementary
/// stream and a self-contained AU for the wire.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    /// True for IDR/keyframes (sets the SOF/keyframe wire flags).
    pub keyframe: bool,
}

/// Codec selection negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    /// The FFmpeg NVENC encoder name (selected by name, not codec id — the latter would
    /// pick the software encoder).
    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }
}

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// Force the next submitted frame to be an IDR keyframe (e.g. after a client
    /// reference-frame-invalidation request). Default: no-op.
    fn request_keyframe(&mut self) {}
    /// Pull the next encoded AU if one is ready.
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
    /// Signal end-of-stream. After this, drain the remaining AUs with [`poll`](Self::poll)
    /// until it returns `None` — NVENC buffers frames internally even at `delay=0`.
    fn flush(&mut self) -> Result<()>;
}

/// Open an NVENC encoder for packed RGB/BGR CPU frames of the given `format` and mode.
/// `format`, `bitrate_bps`, `codec`, and the mode come from session negotiation; M0 takes
/// them from the first captured frame.
pub fn open_video(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
) -> Result<Box<dyn Encoder>> {
    #[cfg(target_os = "linux")]
    {
        let enc = linux::NvencEncoder::open(codec, format, width, height, fps, bitrate_bps)?;
        Ok(Box::new(enc) as Box<dyn Encoder>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (codec, format, width, height, fps, bitrate_bps);
        anyhow::bail!("NVENC encode requires Linux (FFmpeg + NVIDIA driver)")
    }
}

#[cfg(target_os = "linux")]
mod linux;
