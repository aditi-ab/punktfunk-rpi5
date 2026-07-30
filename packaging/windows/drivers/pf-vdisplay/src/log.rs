//! Minimal driver logger, gated as a whole on [`file_log_enabled`] (debug builds, or the
//! `PFVD_DEBUG_LOG` env var): a RELEASE build without the opt-in emits NOTHING — the
//! `OutputDebugStringA` used to fire unconditionally, a syscall + CString + `format!` alloc per
//! logged event on paths that run per IOCTL/frame. The file tee (WUDFHost temp dir, not
//! world-writable — audit §4.4) rides the same gate. Best-effort; ignores all errors. Production
//! driver-state visibility is the SharedHeader `driver_status` channel, not this module.

unsafe extern "system" {
    fn OutputDebugStringA(s: *const u8);
}

/// Whether driver logging (debug string + bring-up file) is enabled (resolved once). Off in release
/// builds unless `PFVD_DEBUG_LOG` is set. `pub(crate)` so `dbglog!` can skip its `format!` too.
pub(crate) fn file_log_enabled() -> bool {
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
            // WUDFHost's own (LocalService) temp dir — NOT world-writable/readable `C:\Users\Public`,
            // where a non-admin could pre-create/hold the file or read the diagnostics
            // (security-review 2026-07-17). Opt-in/debug only.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("pfvd-driver.log"))
                .ok()
                .map(std::sync::Mutex::new)
        })
        .as_ref()
}

pub fn log(s: &str) {
    if !file_log_enabled() {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
    }
    use std::io::Write;
    if let Some(m) = file_appender()
        && let Ok(mut f) = m.lock()
    {
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
}

// The `file_log_enabled()` pre-check skips the `format!` alloc too when logging is off.
macro_rules! dbglog {
    ($($a:tt)*) => { if $crate::log::file_log_enabled() { $crate::log::log(&::std::format!($($a)*)) } };
}

/// Zero-initialise a C POD struct (windows-rs / WDK / IddCx). These are `#[repr(C)]` framework structs
/// whose all-zero bit pattern is a valid zero-initialised value; the caller stamps the required
/// `.Size`/etc fields immediately after. Centralises the `unsafe { core::mem::zeroed() }` the IddCx/WDF
/// bring-up needs — pass the type EXPLICITLY (`pod_init!(T)`) so it works without a binding annotation.
/// Made crate-visible by the same `#[macro_use] mod log;` in `lib.rs` that exports `dbglog!`.
macro_rules! pod_init {
    ($t:ty) => {{
        // SAFETY: $t is a C POD (windows-rs/WDK/IddCx struct); its all-zero bit pattern is a valid
        // zero-initialised value and the caller sets the required .Size/etc fields immediately after.
        // `unused_unsafe`: pod_init! is also expanded at call sites already inside an `unsafe` block
        // (where this `unsafe` is redundant), but it IS required at the non-unsafe sites — so allow it.
        #[allow(unused_unsafe)]
        let zeroed = unsafe { ::core::mem::zeroed::<$t>() };
        zeroed
    }};
}
