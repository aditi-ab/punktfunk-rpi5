//! Windows display-topology helpers (plan §W6), extracted from the host's `windows/{win_display,
//! monitor_devnode,display_events}.rs` so the IDD-push capturer (`pf-capture`) and the pf-vdisplay
//! backend (the host) depend on them as a leaf PEER instead of the capturer reaching back into the
//! orchestrator. Windows-only; compiles to an empty lib elsewhere.
//!
//! - [`win_display`]: CCD/GDI path activation, mode-setting, HDR advanced-colour toggles, and the
//!   source-desktop geometry the capturer duplicates.
//! - [`monitor_devnode`]: PnP monitor devnode enable/disable (the parallel-display isolation lever).
//! - [`display_events`]: the `WM_DISPLAYCHANGE` / device-arrival watch that lets a capture stall say
//!   whether an OS display event coincided with it.

#[cfg(target_os = "windows")]
pub mod display_events;
#[cfg(target_os = "windows")]
pub mod monitor_devnode;
#[cfg(target_os = "windows")]
pub mod win_display;
