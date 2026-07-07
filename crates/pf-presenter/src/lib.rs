//! The Vulkan session presenter (punktfunk-planning `linux-client-rearchitecture.md`,
//! Phase 1): an SDL3 window + ash swapchain that presents the shared session pump's
//! decoded frames, captures input on the `ui_stream` state-machine contract, and reports
//! the unified stats window on stdout. No UI toolkit anywhere in the dependency tree.
//!
//! Two frame paths: software (`CpuFrame` RGBA staging upload) and hardware (the
//! decoder's NV12 dmabuf imported per-plane into Vulkan + the CICP-driven CSC pass —
//! `dmabuf.rs`/`csc.rs`), both composited by a letterboxed blit. Devices without the
//! import extensions, and any import/present failure streak, demote the decoder to
//! software via the session pump's `force_software` contract, same as the GTK presenter.

#[cfg(target_os = "linux")]
pub mod csc;
#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(target_os = "linux")]
pub mod input;
#[cfg(target_os = "linux")]
pub mod keymap_sdl;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
pub mod vk;

#[cfg(target_os = "linux")]
pub use run::{run_session, Outcome, SessionOpts};
