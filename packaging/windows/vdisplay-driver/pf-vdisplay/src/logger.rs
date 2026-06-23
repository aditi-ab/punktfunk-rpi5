//! Minimal `log` backend that writes to `OutputDebugString` AND tees to a file — UMDF redirects a
//! hosted driver's `OutputDebugString` to ETW (invisible to DebugView), so the file tee is how we
//! actually read driver logs during bring-up. Keeping the `log` facade lets the ported
//! callbacks/context use `error!`/`info!`/`debug!` unchanged.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

use log::{LevelFilter, Metadata, Record};
use windows::core::PCSTR;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringA;

/// World-writable so the restricted WUDFHost token can append. Read it during bring-up.
const LOG_PATH: &str = r"C:\Users\Public\pfvd-driver.log";

struct DbgLogger {
    file: Mutex<()>,
}

impl log::Log for DbgLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let msg = format!("[pf-vdisplay] {:<5} {}\0", record.level(), record.args());
        // SAFETY: `msg` is a NUL-terminated byte string valid for the call.
        unsafe { OutputDebugStringA(PCSTR(msg.as_ptr())) };
        // Tee to the file (best-effort): the real channel during bring-up.
        let _guard = self.file.lock();
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
            let _ = writeln!(f, "{:<5} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: DbgLogger = DbgLogger {
    file: Mutex::new(()),
};

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
    // Boot marker so each load is distinguishable in the file.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = writeln!(f, "==== pf-vdisplay logger init ====");
    }
}
