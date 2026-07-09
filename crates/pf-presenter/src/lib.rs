//! The Vulkan session presenter (punktfunk-planning `linux-client-rearchitecture.md`,
//! Phase 1): an SDL3 window + ash swapchain that presents the shared session pump's
//! decoded frames, captures input on the `ui_stream` state-machine contract, and reports
//! the unified stats window on stdout. No UI toolkit anywhere in the dependency tree.
//!
//! Three frame paths: software (`CpuFrame` RGBA staging upload), Vulkan Video (the
//! decoder's VkImage on THIS device — plane views + the CICP-driven CSC pass), and on
//! Linux additionally VAAPI hardware (NV12 dmabuf imported per-plane — `dmabuf.rs`),
//! all composited by a letterboxed blit. Devices without the import extensions, and any
//! import/present failure streak, demote the decoder to software via the session pump's
//! `force_software` contract, same as the GTK presenter.
//!
//! Builds on Linux AND Windows; `dmabuf` is Linux-only (DRM-PRIME does not exist on
//! Windows) and `d3d11` is its Windows counterpart (D3D11VA shared-texture import) —
//! the decode chain there is Vulkan → D3D11VA → software.

#[cfg(any(target_os = "linux", windows))]
pub mod csc;
#[cfg(windows)]
pub mod d3d11;
#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(any(target_os = "linux", windows))]
pub mod input;
#[cfg(any(target_os = "linux", windows))]
pub mod keymap_sdl;
#[cfg(any(target_os = "linux", windows))]
pub mod overlay;
#[cfg(any(target_os = "linux", windows))]
mod run;
#[cfg(any(target_os = "linux", windows))]
pub mod vk;
#[cfg(windows)]
mod win32;

#[cfg(any(target_os = "linux", windows))]
pub use run::{run_browse, run_session, ActionOutcome, Outcome, SessionOpts};
