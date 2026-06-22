//! Minimal `log` backend that writes to `OutputDebugString` — no `driver-logger`/event-log/`tokio`.
//! View with DebugView/WinDbg. Keeping the `log` facade lets the ported callbacks/context use
//! `error!`/`info!`/`debug!` unchanged.

use log::{LevelFilter, Metadata, Record};
use windows::core::PCSTR;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringA;

struct DbgLogger;

impl log::Log for DbgLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let msg = format!("[pf-vdisplay] {:<5} {}\0", record.level(), record.args());
        // SAFETY: `msg` is a NUL-terminated byte string valid for the call.
        unsafe { OutputDebugStringA(PCSTR(msg.as_ptr())) };
    }

    fn flush(&self) {}
}

static LOGGER: DbgLogger = DbgLogger;

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
}
