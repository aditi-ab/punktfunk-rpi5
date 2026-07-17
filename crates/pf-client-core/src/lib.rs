//! Shared, UI-agnostic client plumbing, extracted verbatim from the GTK client
//! (design: punktfunk-planning `linux-client-rearchitecture.md`, Phase 0) so the desktop
//! shells and the Vulkan session binary build on one implementation — on Linux AND
//! Windows (the session binary runs on both; macOS stays `wol`-only, clients/apple is
//! the client there).
//!
//! Nothing here may depend on a UI toolkit: the presenter contract is `session`'s
//! channels (`SessionHandle`) and `video`'s `DecodedImage` (RGBA bytes, dmabuf fds +
//! plane layout, or a decoded VkImage) — how frames reach the screen is the consumer's
//! business.
//!
//! Audio is the one per-OS module swap: `audio.rs` (PipeWire) on Linux,
//! `audio_wasapi.rs` (WASAPI) on Windows — same public surface, picked here by `#[path]`
//! so `crate::audio` is the only name the session pump ever sees. `keymap` (evdev-keyed)
//! stays Linux: the session path uses pf-presenter's SDL-scancode table instead.

#[cfg(target_os = "linux")]
pub mod audio;
#[cfg(windows)]
#[path = "audio_wasapi.rs"]
pub mod audio;
#[cfg(any(target_os = "linux", windows))]
pub mod discovery;
#[cfg(any(target_os = "linux", windows))]
pub mod gamepad;
#[cfg(target_os = "linux")]
pub mod keymap;
#[cfg(any(target_os = "linux", windows))]
pub mod library;
#[cfg(any(target_os = "linux", windows))]
pub mod session;
#[cfg(any(target_os = "linux", windows))]
pub mod trust;
#[cfg(any(target_os = "linux", windows))]
pub mod video;
#[cfg(any(target_os = "linux", windows))]
mod video_color;
#[cfg(any(target_os = "linux", windows))]
mod video_software;
#[cfg(target_os = "linux")]
mod video_vaapi;
#[cfg(any(target_os = "linux", windows))]
mod video_vulkan;
// PyroWave decode — Linux + `pyrowave` feature only (plan §4.5; the Windows client's
// present-path decision and the Apple Metal port are their own phases).
#[cfg(windows)]
pub mod video_d3d11;
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
pub mod video_pyrowave;

pub mod wol;
