//! Minimal driver logger. `OutputDebugStringA` always (ETW/DebugView); the optional world-writable file
//! (`C:\Users\Public\pfvd-driver.log`, readable over SSH) is now OPT-IN — debug builds, or the
//! `PFVD_DEBUG_LOG` env var, only — so a RELEASE build never writes it (audit §4.4: it was an
//! info-leak/DoS surface). Best-effort; ignores all errors. Production driver-state visibility is the
//! SharedHeader `driver_status` channel, not this file.

unsafe extern "system" {
    fn OutputDebugStringA(s: *const u8);
}

/// Whether the world-writable bring-up file log is enabled (resolved once). Off in release builds unless
/// `PFVD_DEBUG_LOG` is set.
fn file_log_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(debug_assertions) || std::env::var_os("PFVD_DEBUG_LOG").is_some())
}

pub fn log(s: &str) {
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
    }
    if !file_log_enabled() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("C:\\Users\\Public\\pfvd-driver.log")
    {
        let _ = writeln!(f, "{s}");
    }
}

macro_rules! dbglog {
    ($($a:tt)*) => { $crate::log::log(&::std::format!($($a)*)) };
}
