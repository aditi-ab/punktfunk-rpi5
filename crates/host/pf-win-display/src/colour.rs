//! The colour calls the IDD-push capturer makes into this crate, behind one seam.
//!
//! [`DisplayColour`] is how pf-capture's depth pin and its open reach the OS: advanced colour,
//! SDR wide colour, the active mode, the secure desktop and the display actor's snapshot.
//! `WinColour` (Windows) is the one production implementor and passes each call straight on.
//! The trait compiles everywhere so a test can script what the display answers.

use std::sync::Arc;

use crate::snapshot::{CcdTargetKey, ColorMode, DisplaySnapshot};

/// `Send + Sync`: the descriptor poller's thread reads the snapshot through it.
pub trait DisplayColour: Send + Sync {
    /// `win_display::set_advanced_color`.
    fn set_advanced_color(&self, key: CcdTargetKey, enable: bool) -> bool;
    /// `win_display::advanced_color_enabled`.
    fn advanced_color_enabled(&self, key: CcdTargetKey) -> Option<bool>;
    /// `win_display::set_wcg`.
    fn set_wcg(&self, key: CcdTargetKey, enable: bool) -> bool;
    /// `win_display::color_mode`.
    fn color_mode(&self, key: CcdTargetKey) -> Option<ColorMode>;
    /// `win_display::wcg_supported`.
    fn wcg_supported(&self, key: CcdTargetKey) -> bool;
    /// `win_display::active_resolution`.
    fn active_resolution(&self, key: CcdTargetKey) -> Option<(u32, u32)>;
    /// `secure_desktop`.
    fn secure_desktop(&self) -> bool;
    /// `display_events::snapshot`.
    fn snapshot(&self) -> Arc<DisplaySnapshot>;
}

/// The OS, through this crate.
#[cfg(target_os = "windows")]
pub struct WinColour;

#[cfg(target_os = "windows")]
impl DisplayColour for WinColour {
    fn set_advanced_color(&self, key: CcdTargetKey, enable: bool) -> bool {
        crate::win_display::set_advanced_color(key, enable)
    }

    fn advanced_color_enabled(&self, key: CcdTargetKey) -> Option<bool> {
        crate::win_display::advanced_color_enabled(key)
    }

    fn set_wcg(&self, key: CcdTargetKey, enable: bool) -> bool {
        crate::win_display::set_wcg(key, enable)
    }

    fn color_mode(&self, key: CcdTargetKey) -> Option<ColorMode> {
        crate::win_display::color_mode(key)
    }

    fn wcg_supported(&self, key: CcdTargetKey) -> bool {
        crate::win_display::wcg_supported(key)
    }

    fn active_resolution(&self, key: CcdTargetKey) -> Option<(u32, u32)> {
        crate::win_display::active_resolution(key)
    }

    fn secure_desktop(&self) -> bool {
        crate::secure_desktop()
    }

    fn snapshot(&self) -> Arc<DisplaySnapshot> {
        crate::display_events::snapshot()
    }
}
