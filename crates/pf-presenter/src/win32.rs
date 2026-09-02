//! Windows-only window branding for the SDL session window.
//!
//! SDL's window class carries a generic icon. [`stamp_window_icon`] copies the
//! exe's icon resource (ordinal 1, embedded by the session binary's `build.rs`)
//! onto the HWND via `WM_SETICON` at `SM_CXSMICON` / `SM_CXICON` so Win32 does
//! not scale at draw time.
//!
//! [`set_app_user_model_id`] stamps the shared AppUserModelID on unpackaged
//! runs so the shell and session windows group as one taskbar app. Packaged
//! processes already share MSIX identity; overriding it detaches the Start-menu
//! pin, so they are left alone.

use windows_sys::core::w;
use windows_sys::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    SM_CXICON, SM_CXSMICON, WM_SETICON,
};

/// Must match the WinUI shell (`clients/windows/src/main.rs`) or the two windows stop grouping.
const APP_USER_MODEL_ID: *const u16 = w!("unom.punktfunk.client");

/// Unpackaged runs only. Call before any window exists; later calls do not re-tag existing windows.
pub(crate) fn set_app_user_model_id() {
    // SAFETY: `GetCurrentPackageFullName` is called with `len = 0` and a NULL buffer, which is the
    // documented identity PROBE — it writes nothing and only reports whether the process is
    // packaged. `SetCurrentProcessExplicitAppUserModelID` takes a static wide literal.
    unsafe {
        let mut len: u32 = 0;
        if GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) != APPMODEL_ERROR_NO_PACKAGE {
            return; // packaged or indeterminate — do not override MSIX identity
        }
        let _ = SetCurrentProcessExplicitAppUserModelID(APP_USER_MODEL_ID);
    }
}

/// Resource ordinal 1 onto this HWND. No-op when the exe embeds no icon.
pub(crate) fn stamp_window_icon(window: &sdl3::video::Window) {
    // SAFETY: the SDL property lookups return the `HWND` SDL itself owns for this live `window`
    // borrow; the icon calls only pass that handle and a resource ordinal back to Win32, and every
    // result is checked before use.
    unsafe {
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
            // MAKEINTRESOURCE(1): integer ordinal through the name pointer, never
            // dereferenced. `without_provenance` states that; `1 as *const u16` is a
            // dangling pointer to clippy.
            let icon = LoadImageW(
                module,
                std::ptr::without_provenance(1),
                IMAGE_ICON,
                px,
                px,
                LR_DEFAULTCOLOR,
            );
            if !icon.is_null() {
                SendMessageW(hwnd, WM_SETICON, which as WPARAM, icon as LPARAM);
            }
        }
    }
}
