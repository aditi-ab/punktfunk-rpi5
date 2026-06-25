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

/// Process-lifetime append handle to the bring-up log, opened ONCE (by whichever thread logs first) and
/// shared via a `Mutex` — so the swap-chain WORKER thread's writes land too. Per-call open/append raced
/// the control thread and/or could fail under the worker's restricted token, hiding exactly the
/// swap-chain-processor lines a game-break repro needs (game-capture bug S3). `flush` after each line so a
/// crash/stall doesn't lose the tail.
fn file_appender() -> Option<&'static std::sync::Mutex<std::fs::File>> {
    use std::sync::OnceLock;
    static APPENDER: OnceLock<Option<std::sync::Mutex<std::fs::File>>> = OnceLock::new();
    APPENDER
        .get_or_init(|| {
            if !file_log_enabled() {
                return None;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("C:\\Users\\Public\\pfvd-driver.log")
                .ok()
                .map(std::sync::Mutex::new)
        })
        .as_ref()
}

pub fn log(s: &str) {
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
    }
    use std::io::Write;
    if let Some(m) = file_appender() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

macro_rules! dbglog {
    ($($a:tt)*) => { $crate::log::log(&::std::format!($($a)*)) };
}
