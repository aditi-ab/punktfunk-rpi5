//! `punktfunk-client` — the native Linux punktfunk/1 client (design: Option A, 2026-06-12).
//!
//! GTK4/libadwaita shell · `NativeClient` linked as a crate (no C ABI) · FFmpeg decode →
//! `GtkGraphicsOffload` present · PipeWire audio · SDL3 gamepads. The trust surface
//! mirrors the Apple client: persistent identity, TOFU prompt with the host fingerprint,
//! SPAKE2 PIN pairing.

// The UI-agnostic plumbing lives in `pf-client-core`, shared with the upcoming Vulkan
// session binary (design: punktfunk-planning linux-client-rearchitecture.md, Phase 0).
// Root re-exports keep every existing `crate::video`-style path resolving unchanged.
#[cfg(target_os = "linux")]
pub use pf_client_core::{audio, discovery, gamepad, keymap, library, session, trust, video, wol};

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod cli;
#[cfg(target_os = "linux")]
mod launch;
#[cfg(target_os = "linux")]
mod ui_gamepad_library;
#[cfg(target_os = "linux")]
mod ui_hosts;
#[cfg(target_os = "linux")]
mod ui_library;
#[cfg(target_os = "linux")]
mod ui_settings;
#[cfg(target_os = "linux")]
mod ui_stream;
#[cfg(target_os = "linux")]
mod ui_trust;
#[cfg(target_os = "linux")]
mod video_gl;

#[cfg(target_os = "linux")]
fn main() -> gtk::glib::ExitCode {
    app::run()
}

/// GTK4/PipeWire/SDL3 are Linux turf; this stub keeps `cargo build --workspace` green on
/// macOS (the Mac client lives in clients/apple).
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("punktfunk-client is Linux-only — the macOS client lives in clients/apple");
    std::process::exit(2);
}
