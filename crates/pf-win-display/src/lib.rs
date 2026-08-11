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

// `win_display` has denied both unsafe-proof lints since its CCD helpers stopped being `unsafe fn`;
// hoist that to the crate root so the smaller modules (`input_desktop`, `monitor_devnode`,
// `display_events`) and any future one are covered by default rather than by remembering to opt in.

#[cfg(target_os = "windows")]
pub mod display_events;
/// Bind display-config writes to the input desktop so a UAC / lock screen can't refuse them.
#[cfg(target_os = "windows")]
mod input_desktop;
#[cfg(target_os = "windows")]
pub mod monitor_devnode;
/// Cross-crate "topology churn in flight" latch (pure std — no Windows surface, so unconditionally
/// compiled and unit-tested on every platform).
pub mod topology_churn;
#[cfg(target_os = "windows")]
pub mod win_display;

/// `Some((own_session, console_session))` when this process is NOT in the active console session —
/// the state where every `SetDisplayConfig`/CDS write fails `ERROR_ACCESS_DENIED`, GDI reads
/// describe the wrong session's displays, and input compose kicks go nowhere (the lid-closed /
/// non-console field failure mode). The IDD-push capturer uses it to phrase a diagnostic when the
/// driver won't attach. (A byte-copy of the host's `interactive::console_session_mismatch`: it lives
/// here too so `pf-capture` gets it as a leaf peer instead of reaching into the orchestrator.)
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
