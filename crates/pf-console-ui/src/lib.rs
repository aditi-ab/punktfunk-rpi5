//! The Skia console UI (punktfunk-planning `linux-client-rearchitecture.md` §6, and
//! `android-skia-console-port.md` for the second host): one shell — home (host carousel),
//! the game library (coverflow, grid, Collections), settings, add-host, PIN pairing —
//! with screen transitions, per-pad button glyphs and a controller keyboard, rendered onto
//! whatever `skia_safe::Canvas` a host hands it.
//!
//! Two hosts sit on the portable [`Console`] driver:
//! - the Vulkan session binary's [`SkiaOverlay`] (feature `vulkan-overlay`, the default) —
//!   an [`Overlay`](pf_presenter::overlay::Overlay) implementation rendering on the
//!   PRESENTER's Vulkan device into offscreen RGBA images the presenter composites as one
//!   premultiplied quad, plus the in-stream chrome (stats OSD, capture hint, start banner).
//!   Skia never touches the swapchain, and nothing here runs while the overlay has nothing
//!   to show — the §6.1 invariants live or die in `skia_overlay.rs`;
//! - the Android client's GL host (`clients/android/native`), which owns its own EGL
//!   surface and drives [`Console`] directly, `default-features = false`.
//!
//! Everything but `skia_overlay.rs` is platform-free: screens draw to `&Canvas`, settings
//! persist through [`store::SettingsStore`], keys arrive as [`input::Key`], the platform's
//! row set is a [`platform::Platform`] question. That is what the CPU-raster tests in
//! `shell/tests.rs` and the screenshot dump prove every run.

// Unsafe-proof program: every `unsafe {}` in the Skia/Vulkan overlay carries a `// SAFETY:` proof.

#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod anim;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod collate;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod console;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod glyphs;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod input;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod launcher_icons;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod library;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod model;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod os_marks;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod platform;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod pointer;
// The in-stream ring on Skia is the DESKTOP shell's (Android has its Compose ring, and the
// host-action cache it reads is desktop-gated).
#[cfg(any(target_os = "linux", windows))]
mod ring;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod screens;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod shell;
#[cfg(all(any(target_os = "linux", windows), feature = "vulkan-overlay"))]
mod skia_overlay;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod store;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod theme;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
mod widgets;

#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use console::{Console, ConsoleEntry, ConsoleHandles, InputSource, Insets, Viewport};
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use input::Key;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use library::{LibraryGame, LibraryPhase, LibraryShared, Stale};
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use model::{
    ConsoleBus, ConsoleCmd, ConsoleShared, HostAction, HostRow, PairPhase, ProfileChip, WakeStatus,
};
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use platform::{Platform, PlatformScreen};
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use shell::{ConsoleOptions, DEFAULT_GPU_CACHE_BYTES};
#[cfg(all(any(target_os = "linux", windows), feature = "vulkan-overlay"))]
pub use skia_overlay::SkiaOverlay;
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub use store::{SettingsStore, SnapshotStore};
