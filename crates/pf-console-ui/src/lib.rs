//! The Skia console UI (punktfunk-planning `linux-client-rearchitecture.md` §6): an
//! [`Overlay`] implementation rendering on the PRESENTER's Vulkan device into offscreen
//! RGBA images the presenter composites as one premultiplied quad. Skia never touches
//! the swapchain, and nothing here runs while the overlay has nothing to show — the
//! §6.1 invariants live or die in this crate.
//!
//! Milestone 1 (this file): the stats OSD panel + the capture-hint pill — small on
//! purpose, it proves the whole shared-device pipeline. The gamepad library moves in
//! next.

#[cfg(target_os = "linux")]
pub mod library;
#[cfg(target_os = "linux")]
mod library_ui;
#[cfg(target_os = "linux")]
mod skia_overlay;

#[cfg(target_os = "linux")]
pub use library::{LibraryGame, LibraryPhase, LibraryShared};
#[cfg(target_os = "linux")]
pub use skia_overlay::SkiaOverlay;
