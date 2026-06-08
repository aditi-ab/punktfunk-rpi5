//! Frame capture (plan §7). On Linux: a PipeWire ScreenCast portal stream delivering
//! dmabuf frames with no copy to the CPU. The encoder imports the dmabuf directly.

use anyhow::Result;

/// A captured frame. For zero-copy the real type wraps a dmabuf fd + modifier; the CPU
/// buffer is only a fallback path (plan §9 risk: per-GPU dmabuf import quirks).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pts_ns: u64,
    /// Fallback CPU pixels (empty when a dmabuf is used).
    pub cpu_bytes: Vec<u8>,
}

/// Produces frames from a captured output. Lives on its own thread, feeding the encoder
/// over a bounded drop-oldest channel (never block the compositor).
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;
}

/// Open a capturer for a PipeWire node id (from the ScreenCast portal).
pub fn open_pipewire(_node_id: u32) -> Result<Box<dyn Capturer>> {
    #[cfg(target_os = "linux")]
    {
        anyhow::bail!("pipewire capture not yet implemented (M0)")
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("capture requires Linux + PipeWire")
    }
}
