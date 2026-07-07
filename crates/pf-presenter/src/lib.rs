//! The Vulkan session presenter (punktfunk-planning `linux-client-rearchitecture.md`,
//! Phase 1): an SDL3 window + ash swapchain that presents the shared session pump's
//! decoded frames, captures input on the `ui_stream` state-machine contract, and reports
//! the unified stats window on stdout. No UI toolkit anywhere in the dependency tree.
//!
//! Phase 1 is the software path: `CpuFrame` RGBA uploads + a transfer-only letterbox
//! blit (no graphics pipeline, no shaders — those arrive with the Phase 2 dmabuf/CSC
//! pass). A hardware (dmabuf) frame slipping through demotes the decoder to software via
//! the session pump's `force_software` contract, same as the GTK presenter.

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
