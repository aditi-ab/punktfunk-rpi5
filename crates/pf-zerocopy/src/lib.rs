//! Linux GPU zero-copy plumbing (plan §9), extracted from the host's `linux/zerocopy/` module
//! tree (plan §W6): the shared CUDA context + device buffers, the EGL/Vulkan dmabuf importers,
//! the isolated import-worker subprocess (client/proto/worker), the zero-copy policy latches, and
//! the dmabuf implicit-fence wait. Everything is Linux-only; on other targets this crate compiles
//! to an empty lib so dependents can carry a plain (non-target-gated) dependency.
//!
//! The `PixelFormat → DRM FourCC` mapping (`drm_fourcc`) deliberately does NOT live here — it
//! consumes the shared frame vocabulary, which sits ABOVE this crate (this crate provides the
//! `DeviceBuffer` that vocabulary's `FramePayload::Cuda` owns).

// Unsafe-proof program: every `unsafe {}` / `unsafe impl` must carry a `// SAFETY:` proof. Each
// file keeps its own `#![deny(...)]` too; this crate-root deny is the catch-all gate.
#![deny(clippy::undocumented_unsafe_blocks)]

/// Wait for a dmabuf's implicit read-ready fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + poll).
#[cfg(target_os = "linux")]
pub mod dmabuf_fence;

#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
pub use imp::*;
