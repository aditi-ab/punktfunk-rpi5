//! Zero-copy capture→encode (plan §9): the PipeWire dmabuf is imported into CUDA via EGL and
//! handed straight to NVENC, eliminating the per-frame CPU copies (at 5K the CPU-copy path
//! moves ~3.5 GB/s). Opt in with `PUNKTFUNK_ZEROCOPY=1`; the CPU-copy path stays the default and
//! the runtime fallback (foreign-allocator / no-dmabuf / import failure).
//!
//! Pieces: [`cuda`] (driver-API FFI + the shared `CUcontext` + device buffers), [`egl`] (the
//! headless EGLDisplay + dmabuf→`EGLImage`→CUDA import). The encoder's CUDA-frame path lives in
//! `encode/linux.rs`; the dmabuf negotiation lives in `capture/linux.rs`.

pub mod cuda;
pub mod egl;
pub mod vulkan;

pub use cuda::DeviceBuffer;
pub use egl::{DmabufPlane, EglImporter};

/// Whether the zero-copy path is opted in (`PUNKTFUNK_ZEROCOPY` truthy).
pub fn enabled() -> bool {
    std::env::var("PUNKTFUNK_ZEROCOPY")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// DRM FourCC for a packed 32-bit format name (little-endian, e.g. `b"XR24"`).
const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// Map a SPA/our [`crate::capture::PixelFormat`] to the DRM FourCC EGL expects for import.
/// SPA byte order `BGRx` ⇒ DRM `XRGB8888` (memory B,G,R,X), etc.
pub fn drm_fourcc(format: crate::capture::PixelFormat) -> Option<u32> {
    use crate::capture::PixelFormat::*;
    Some(match format {
        Bgrx => fourcc(b"XR24"), // DRM_FORMAT_XRGB8888
        Bgra => fourcc(b"AR24"), // DRM_FORMAT_ARGB8888
        Rgbx => fourcc(b"XB24"), // DRM_FORMAT_XBGR8888
        Rgba => fourcc(b"AB24"), // DRM_FORMAT_ABGR8888
        // 24-bit packed RGB/BGR have no straightforward dmabuf import here; use the CPU path.
        Rgb | Bgr => return None,
    })
}

/// Standalone probe (the `zerocopy-probe` subcommand): initialize the EGL importer + CUDA
/// context and report. De-risks the FFI/linking/GPU-access without needing a capture session.
pub fn probe() -> anyhow::Result<()> {
    let _importer = EglImporter::new()?;
    let ctx = cuda::context()?;
    tracing::info!(cuda_ctx = ?ctx, "zero-copy probe OK — EGL display + CUDA context initialized");
    Ok(())
}
