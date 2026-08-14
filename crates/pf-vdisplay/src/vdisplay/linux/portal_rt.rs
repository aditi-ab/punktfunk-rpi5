//! The ONE tokio runtime every portal handshake runs on, for the life of the process.
//!
//! 🛑🛑🛑 This exists because of a lifetime bug that cost a full day of misdiagnosis, so the reason
//! is written down rather than left to be rediscovered.
//!
//! ashpd caches its D-Bus connection **process-globally** — `static SESSION: OnceLock<Connection>`
//! (ashpd 0.13.13, `src/proxy.rs:27`). The first `Screencast::new()` in the process creates that
//! connection, and zbus spawns the connection's background reader as a task **on whichever tokio
//! runtime happens to be current at that moment**.
//!
//! Each backend used to build its own multi-thread runtime per cast and drop it at teardown. So the
//! FIRST cast of a host process created the cached connection on a runtime that was then destroyed
//! when that cast ended — and the `OnceLock` went on handing the same, now-executor-less connection
//! to every later `Screencast::new()`, which then awaited a reply nothing was left alive to read.
//!
//! MEASURED 2026-08-14 (Hyprland 0.55.4 + xdph 1.3.12): the first cast of a host process streamed;
//! every cast after it hung, in a process whose surviving cast thread sat in `futex_do_wait` inside
//! runtime shutdown. The discriminator that pins it on us rather than on the compositor stack: a
//! freshly spawned process completed the identical handshake against the identical xdph, repeatedly,
//! while the long-lived host could complete none — and xdph itself was idle (28 ms of CPU).
//!
//! ⚠ Therefore: **never build a per-cast runtime, and never drop this one.** A `OnceLock` that is
//! only ever read keeps the connection's reader alive for the process lifetime, which is exactly as
//! long as the cached connection itself lives. `block_on` takes `&self`, so every cast thread can
//! park on this one runtime concurrently.

use std::sync::OnceLock;
use tokio::runtime::Runtime;

/// Build failures are reported to the caller rather than panicking: a host that cannot build a
/// runtime should fail the cast with a reason, not abort the process.
static PORTAL_RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();

/// The shared portal runtime, or the error from trying to build it.
///
/// Multi-thread with 2 workers: the zbus background reader must be pumped *across* the
/// `create_session` → `select_sources` → `start` handshake while a cast thread blocks on it, which a
/// current-thread runtime cannot do.
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
