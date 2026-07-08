//! Windows-only window branding for the SDL session window.
//!
//! SDL registers its window class with a generic icon, so without help the taskbar and
//! Alt-Tab show the SDL default even when the exe embeds ours. [`stamp_window_icon`]
//! stamps the exe's icon resource (ordinal 1, when embedded — see the session binary's
//! `build.rs`) onto the window via `WM_SETICON`, at the native small/big metrics so
//! nothing is scaled at draw time — the same treatment the WinUI shell gives its window.
//!
//! [`set_app_user_model_id`] gives unpackaged (dev/CLI) runs the same explicit
//! AppUserModelID as the shell, so the two windows group as ONE taskbar app across the
//! shell⇄session visibility handoff. MSIX runs already share the package identity, and
//! overriding it would detach the window from the Start-menu pin — so packaged processes
//! are left alone.

use windows_sys::core::w;
use windows_sys::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    SM_CXICON, SM_CXSMICON, WM_SETICON,
};

/// The shared taskbar identity — must stay in sync with the WinUI shell's
/// (`clients/windows/src/main.rs`), or the two windows stop grouping.
const APP_USER_MODEL_ID: *const u16 = w!("unom.punktfunk.client");

/// Tag this process with the shared AppUserModelID — unpackaged runs only (a packaged
/// process already carries the MSIX identity, which must win). Call before any window
/// exists; later calls don't re-tag existing windows.
pub(crate) fn set_app_user_model_id() {
    unsafe {
        let mut len: u32 = 0;
        // No buffer: just probe whether the process has package identity.
        if GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) != APPMODEL_ERROR_NO_PACKAGE {
            return; // packaged (or indeterminate) — leave the identity alone
        }
        let _ = SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID);
    }
}

/// Stamp the exe-embedded icon (resource ordinal 1) onto the SDL window's title bar,
/// taskbar and Alt-Tab. A quiet no-op when the exe embeds no icon.
pub(crate) fn stamp_window_icon(window: &sdl3::video::Window) {
    unsafe {
        // The HWND behind the SDL window, via SDL3's window properties (the same route
        // the sdl3 crate's raw-window-handle integration takes).
        let hwnd: HWND = sdl3::sys::properties::SDL_GetPointerProperty(
            sdl3::sys::video::SDL_GetWindowProperties(window.raw()),
            sdl3::sys::video::SDL_PROP_WINDOW_WIN32_HWND_POINTER,
            std::ptr::null_mut(),
        ) as HWND;
        if hwnd.is_null() {
            return;
        }
        let module = GetModuleHandleW(std::ptr::null());
        for (which, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = GetSystemMetrics(metric);
            let icon = LoadImageW(module, 1 as *const u16, IMAGE_ICON, px, px, LR_DEFAULTCOLOR);
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as WPARAM, icon as LPARAM);
            }
        }
    }
}
