//! Shared, UI-agnostic Linux-client plumbing, extracted verbatim from the GTK client
//! (design: punktfunk-planning `linux-client-rearchitecture.md`, Phase 0) so the desktop
//! shell and the Vulkan session binary build on one implementation.
//!
//! Nothing here may depend on a UI toolkit: the presenter contract is `session`'s
//! channels (`SessionHandle`) and `video`'s `DecodedImage` (RGBA bytes or dmabuf fds +
//! plane layout) — how frames reach the screen is the consumer's business.

#[cfg(target_os = "linux")]
pub mod audio;
#[cfg(target_os = "linux")]
pub mod discovery;
#[cfg(target_os = "linux")]
pub mod gamepad;
#[cfg(target_os = "linux")]
pub mod keymap;
#[cfg(target_os = "linux")]
pub mod library;
#[cfg(target_os = "linux")]
pub mod session;
#[cfg(target_os = "linux")]
pub mod trust;
#[cfg(target_os = "linux")]
pub mod video;

pub mod wol;
