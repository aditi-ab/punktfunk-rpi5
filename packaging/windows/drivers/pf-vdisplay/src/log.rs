//! Minimal driver logger (matches the DualSense driver). DebugView can't capture the UMDF host across
//! session 0, so besides `OutputDebugStringA` we append to a world-writable file readable over SSH. Used
//! only for bring-up/diagnostics; cheap and best-effort (ignores all errors).

unsafe extern "system" {
    fn OutputDebugStringA(s: *const u8);
}

pub fn log(s: &str) {
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
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
