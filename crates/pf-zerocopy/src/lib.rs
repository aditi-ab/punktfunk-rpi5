//! Linux GPU zero-copy plumbing: shared CUDA context and device buffers, EGL/Vulkan dmabuf
//! importers, the isolated import-worker subprocess, zero-copy policy latches, and the dmabuf
//! implicit-fence wait. Linux-only; on other targets this crate is an empty lib so dependents
//! can take a plain (non-target-gated) dependency.
//!
//! `PixelFormat → DRM FourCC` (`drm_fourcc`) does not live here: it consumes the shared frame
//! vocabulary above this crate. This crate provides the `DeviceBuffer` that vocabulary's
//! `FramePayload::Cuda` owns.

// Every `unsafe {}` / `unsafe impl` carries a `// SAFETY:` proof; `unsafe fn` bodies use
// explicit blocks. Both lints are in the workspace `[workspace.lints]` tables.

/// Wait for a dmabuf's implicit read-ready fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + poll).
#[cfg(target_os = "linux")]
pub mod dmabuf_fence;

#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
pub use imp::*;
