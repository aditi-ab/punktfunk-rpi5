//! `punktfunk-client` — the native Linux punktfunk/1 desktop shell (relm4/libadwaita).
//!
//! Hosts, pairing/trust, settings, and the desktop library page; every stream (and the
//! console game library) runs in the spawned `punktfunk-session` Vulkan binary — the
//! shell never touches video (punktfunk-planning `linux-client-rearchitecture.md`).
// `deny`, not `forbid`, since edition 2024: clearing Steam's SDL device filter and the spawn
// test's `HOME` scoping mutate the process env, which is now an unsafe call. Both carry a named
// `#[allow(unsafe_code)]` with the proof at the site; everything else stays compiler-refused.
#![deny(unsafe_code)]

// The UI-agnostic plumbing lives in `pf-client-core`, shared with the session binary.
// Root re-exports keep every `crate::trust`-style path resolving unchanged.
#[cfg(target_os = "linux")]
pub use pf_client_core::{discovery, gamepad, library, os, trust, video, wol};

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod cli;
// "Create shortcut…" — the desktop-entry writer (design/client-deep-links.md §5).
#[cfg(target_os = "linux")]
mod shortcuts;
#[cfg(target_os = "linux")]
mod spawn;
#[cfg(target_os = "linux")]
mod ui_hosts;
#[cfg(target_os = "linux")]
mod ui_library;
#[cfg(target_os = "linux")]
mod ui_settings;
#[cfg(target_os = "linux")]
mod ui_trust;

#[cfg(target_os = "linux")]
fn main() -> gtk::glib::ExitCode {
    app::run()
}

/// GTK4/SDL3 are Linux turf; this stub keeps `cargo build --workspace` green on macOS
/// (the Mac client lives in clients/apple).
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("punktfunk-client is Linux-only — the macOS client lives in clients/apple");
    std::process::exit(2);
}
