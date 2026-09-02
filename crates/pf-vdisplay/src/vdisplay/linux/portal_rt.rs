//! Process-lifetime tokio runtime for every portal handshake.
//!
//! ashpd caches its D-Bus connection in a process-global `OnceLock`. The first
//! `Screencast::new()` creates it, and zbus spawns the connection's reader on
//! whichever tokio runtime is current at that moment.
//!
//! A per-cast runtime that is dropped at teardown leaves that cached connection
//! with no executor. Later `Screencast::new()` then waits for a reply nothing
//! is left alive to read.
//!
//! Never build a per-cast runtime, and never drop this one. `block_on` takes
//! `&self`, so every cast thread can park on it concurrently.

use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// `Result` so a failed build fails the cast with a reason instead of aborting the process.
static PORTAL_RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();

/// Multi-thread, 2 workers: the zbus reader must run across `create_session`
/// → `select_sources` → `start` while a cast thread blocks on `block_on`.
/// A current-thread runtime cannot pump that.
pub(crate) fn portal_runtime() -> Result<&'static Runtime, String> {
    match PORTAL_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("punktfunk-portal-rt")
            .enable_all()
            .build()
    }) {
        Ok(rt) => Ok(rt),
        Err(e) => Err(format!("build the shared portal runtime: {e}")),
    }
}
