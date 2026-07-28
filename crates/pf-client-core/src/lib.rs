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

// Unsafe-proof program: every `unsafe {}` / `unsafe impl` in this crate carries a `// SAFETY:`
// proof of why it is sound. This crate held ~91 unsafe items with NO enforcement while every
// other subsystem crate denied it — the decoders' `unsafe impl Send`s had a one-line aside
// instead of an argument precisely because nothing required one.
#![deny(clippy::undocumented_unsafe_blocks)]

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
// The `punktfunk://` grammar (design/client-deep-links.md §2): one parser/emitter for the
// shells, the session and the CLI, held to the Swift/Kotlin ports by a shared vector file.
#[cfg(any(target_os = "linux", windows))]
pub mod deeplink;
// The brain layer (design/client-architecture-split.md §3): what a connect is, the wake
// state machine every front-end drives, and the session spawn + stdout contract.
#[cfg(any(target_os = "linux", windows))]
pub mod orchestrate;
// Client settings profiles: the override catalog + the one connect-time resolver
// (design/client-settings-profiles.md §4). Sits beside `trust`, which owns the host records
// the bindings live on.
#[cfg(any(target_os = "linux", windows))]
pub mod profiles;
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
// libav ownership helpers shared by the hardware decoders below (`AvBuffer`).
#[cfg(any(target_os = "linux", windows))]
mod video_libav;
#[cfg(target_os = "linux")]
mod video_vaapi;
#[cfg(any(target_os = "linux", windows))]
mod video_vulkan;
// The OS-clipboard bridge for the shared clipboard (design/clipboard-and-file-transfer.md §5).
// Built everywhere the session client is; the platform seam inside is Windows-real,
// stub elsewhere.
#[cfg(any(target_os = "linux", windows))]
pub mod clipboard;
// PyroWave decode — Linux + Windows (plan §4.5; the Apple Metal port is its own phase).
// Windows joined once its client moved to the SAME spawned Vulkan session presenter as
// Linux's: the decoder is plain Vulkan compute on the presenter's device (no fds, no
// dmabuf, no D3D11 interop), so the old "Windows present-path decision" that gated it
// resolved itself — the present path is now literally the same code.
#[cfg(windows)]
pub mod video_d3d11;
#[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
pub mod video_pyrowave;

pub mod wol;
