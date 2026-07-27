//! Persistent client log file: `%LOCALAPPDATA%\punktfunk\logs\client.log`.
//!
//! The shell is a `windows_subsystem` binary and spawns `punktfunk-session` with
//! `CREATE_NO_WINDOW` — a normal GUI/MSIX launch has NO console, so before this module every
//! log line (the shell's and, worse, the session's whole receive/decode/present forensic
//! trail) evaporated exactly when a user hit a problem worth reporting. The 2026-07 PyroWave
//! latency-sawtooth field report had to be triaged from host logs alone because the client
//! side had nowhere to land.
//!
//! Mirrors the host's convention (`%ProgramData%\punktfunk\logs`, size-capped): a file over
//! 10 MB is rotated to `.old` at the next client start, one generation kept. Everything is
//! best-effort — a missing/locked directory degrades to plain stderr, never a startup failure.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Rotate at the next start once the file exceeds this (the host's cap).
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

static SINK: OnceLock<Option<Arc<Mutex<File>>>> = OnceLock::new();

fn log_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"punktfunk\logs"))
}

/// The log file's path, for the "logs land here" startup line (and any future UI affordance).
pub(crate) fn path() -> Option<PathBuf> {
    Some(log_dir()?.join("client.log"))
}

/// Open (rotating first) and cache the sink. Called once at startup, before the tracing
/// subscriber installs; every later [`tee`] shares the handle.
pub(crate) fn init() {
    SINK.get_or_init(|| {
        let dir = log_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("client.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            let old = dir.join("client.log.old");
            // Windows `rename` refuses an existing destination — drop the old generation first.
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Arc::new(Mutex::new(file)))
    });
}

/// A writer that duplicates onto stderr (dev runs from a terminal keep their interleaved
/// output) and the log file (GUI runs finally keep anything at all). The tracing subscriber's
/// `with_writer` factory and the session-stderr forwarder both use it.
pub(crate) struct Tee;

/// `with_writer` factory (`fn() -> Tee` satisfies `MakeWriter`).
pub(crate) fn tee() -> Tee {
    Tee
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(Some(f)) = SINK.get() {
            let _ = f.lock().unwrap().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(Some(f)) = SINK.get() {
            let _ = f.lock().unwrap().flush();
        }
        Ok(())
    }
}

/// Forward a spawned child's stderr into the [`Tee`], line-buffered so its lines never
/// interleave mid-line with the shell's own. Returns immediately; the thread dies with the
/// pipe (child exit).
pub(crate) fn forward_child_stderr(stderr: impl io::Read + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("punktfunk-session-log".into())
        .spawn(move || {
            let mut reader = io::BufReader::new(stderr);
            let mut line = String::new();
            let mut tee = Tee;
            while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                let _ = tee.write_all(line.as_bytes());
                line.clear();
            }
        });
}
