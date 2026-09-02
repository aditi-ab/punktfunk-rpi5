//! Windows display-topology helpers. Leaf peer for the IDD-push capturer (`pf-capture`) and the
//! pf-vdisplay host backend; Windows-only, empty lib elsewhere.
//!
//! - [`win_display`]: CCD/GDI path activation, mode-setting, HDR advanced-colour toggles, and the
//!   source-desktop geometry the capturer duplicates.
//! - [`monitor_devnode`]: PnP monitor devnode enable/disable (the parallel-display isolation lever).
//! - [`display_events`]: the display ACTOR — `WM_DISPLAYCHANGE` / device-arrival watch that
//!   publishes the cached [`snapshot::DisplaySnapshot`] every hot reader takes instead of the
//!   display-config lock, and timestamps events so a capture stall can say whether an OS display
//!   event coincided with it.
//! - [`snapshot`]: the platform-neutral snapshot types and cache rules (tested everywhere).

#[cfg(target_os = "windows")]
pub mod adl_emul;
#[cfg(target_os = "windows")]
pub mod display_events;
/// Bind display-config writes to the input desktop so a UAC / lock screen can't refuse them.
#[cfg(target_os = "windows")]
mod input_desktop;
#[cfg(target_os = "windows")]
pub mod monitor_devnode;
/// Display identity, inventory and the snapshot cache — pure std, unit-tested on every platform.
pub mod snapshot;
/// Cross-crate "topology churn in flight" latch. Pure std — no Windows surface, so compiled and
/// unit-tested on every platform.
pub mod topology_churn;
#[cfg(target_os = "windows")]
pub mod win_display;

/// `Some((own_session, console_session))` when this process is not in the active console session —
/// every `SetDisplayConfig`/CDS write then fails `ERROR_ACCESS_DENIED`, GDI reads describe the
/// wrong session, and input compose kicks go nowhere. The IDD-push capturer uses it to phrase a
/// diagnostic when the driver won't attach.
#[cfg(target_os = "windows")]
pub fn console_session_mismatch() -> Option<(u32, u32)> {
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut own: u32 = 0;
    // SAFETY: `own` is a live local out-param for this synchronous call; no pointer escapes it.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut own) }.is_err() {
        return None;
    }
    // SAFETY: takes no arguments and returns the console session id by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    (console != 0xFFFF_FFFF && own != console).then_some((own, console))
}
