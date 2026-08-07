//! The Skia console UI (punktfunk-planning `linux-client-rearchitecture.md` §6): an
//! [`Overlay`](pf_presenter::overlay::Overlay) implementation rendering on the
//! PRESENTER's Vulkan device into offscreen RGBA images the presenter composites as one
//! premultiplied quad. Skia never touches the swapchain, and nothing here runs while
//! the overlay has nothing to show — the §6.1 invariants live or die in this crate.
//!
//! The console is a full couch shell now — home (host carousel), the game library
//! coverflow, settings, add-host, and PIN pairing, with screen transitions, per-pad
//! button glyphs, and a controller keyboard (suppressed on Steam Deck, where Steam's
//! own keyboard types through SDL text input) — plus the in-stream chrome: stats OSD,
//! capture hint, start banner.

// Unsafe-proof program: every `unsafe {}` in the Skia/Vulkan overlay carries a `// SAFETY:` proof.
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(any(target_os = "linux", windows))]
mod anim;
#[cfg(any(target_os = "linux", windows))]
mod glyphs;
#[cfg(any(target_os = "linux", windows))]
pub mod library;
#[cfg(any(target_os = "linux", windows))]
pub mod model;
#[cfg(any(target_os = "linux", windows))]
mod pointer;
#[cfg(any(target_os = "linux", windows))]
mod screens;
#[cfg(any(target_os = "linux", windows))]
mod shell;
#[cfg(any(target_os = "linux", windows))]
mod skia_overlay;
#[cfg(any(target_os = "linux", windows))]
mod theme;
#[cfg(any(target_os = "linux", windows))]
mod widgets;

#[cfg(any(target_os = "linux", windows))]
pub use library::{LibraryGame, LibraryPhase, LibraryShared};
#[cfg(any(target_os = "linux", windows))]
pub use model::{
    ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, ProfileChip, WakeStatus,
};
#[cfg(any(target_os = "linux", windows))]
pub use shell::ConsoleOptions;
#[cfg(any(target_os = "linux", windows))]
pub use skia_overlay::{ConsoleEntry, ConsoleHandles, SkiaOverlay};
