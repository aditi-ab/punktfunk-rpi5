//! Hardware video encode (plan §7). Binds FFmpeg (VAAPI / NVENC); never rewrites codecs.
//! Low-latency preset, lookahead off, dmabuf import for zero-copy from [`crate::capture`].

use crate::capture::CapturedFrame;
use anyhow::Result;

/// An encoded access unit (one NAL/AU) to hand to `lumen_core` for FEC + packetization.
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

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// Pull the next encoded AU if one is ready.
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
}

/// Open an encoder. `bitrate_bps` and `codec` come from session negotiation.
pub fn open(_codec: Codec, _bitrate_bps: u64) -> Result<Box<dyn Encoder>> {
    #[cfg(target_os = "linux")]
    {
        anyhow::bail!("VAAPI/NVENC encode not yet implemented (M0)")
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("encode requires Linux (VAAPI/NVENC via FFmpeg)")
    }
}
