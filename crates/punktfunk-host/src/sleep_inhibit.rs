//! Session-scoped suspend/idle inhibition: while at least one client is streaming, the host
//! holds a logind `sleep:idle` BLOCK inhibitor so the box doesn't auto-suspend out from under a
//! passive viewer. Remote INPUT resets the compositor's idle timers, but a video-only viewer
//! sends none — observed live on a SteamOS Game-Mode host, which s2idled mid-stream-day and
//! dropped off the network (and, in a VM with GPU passthrough, never woke again). Refcounted
//! across planes (native sessions + GameStream media): the first hold acquires, the last drop
//! releases. Best-effort — no logind (containers, non-systemd boxes) logs once and streams on.
//! Off Linux this is a no-op: macOS/Windows hosts manage their own power assertions.

use std::sync::{Mutex, OnceLock};

/// RAII share of the host-wide inhibitor — hold one per live session/stream.
pub struct StreamHold(());

struct State {
    count: u32,
    /// The logind inhibitor pipe fd — inhibition lasts exactly as long as it stays open.
    #[cfg(target_os = "linux")]
    fd: Option<ashpd::zbus::zvariant::OwnedFd>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            count: 0,
            #[cfg(target_os = "linux")]
            fd: None,
        })
    })
}

/// Take a share; the underlying inhibitor is acquired on the 0→1 edge.
pub fn hold() -> StreamHold {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.count += 1;
    #[cfg(target_os = "linux")]
    if st.count == 1 && st.fd.is_none() {
        st.fd = acquire();
    }
    StreamHold(())
}

impl Drop for StreamHold {
    fn drop(&mut self) {
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        st.count = st.count.saturating_sub(1);
        #[cfg(target_os = "linux")]
        if st.count == 0 && st.fd.take().is_some() {
            tracing::info!("released the sleep/idle inhibitor (no live sessions)");
        }
    }
}

/// One logind `Inhibit` call on a dedicated plain thread — zbus's blocking API must not run on
/// a tokio worker (its internal `block_on` panics there), and callers of [`hold`] may be either.
/// The join blocks the caller for the D-Bus round-trip (~ms), which every call site tolerates.
#[cfg(target_os = "linux")]
fn acquire() -> Option<ashpd::zbus::zvariant::OwnedFd> {
    let fd = std::thread::spawn(|| -> Option<ashpd::zbus::zvariant::OwnedFd> {
        use ashpd::zbus;
        // zbus's blocking API is configured out by ashpd's feature set — drive the async API on
        // a private current-thread runtime instead (still on this plain thread; see fn doc).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let attempt: zbus::Result<zbus::zvariant::OwnedFd> = rt.block_on(async {
            let conn = zbus::Connection::system().await?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &("sleep:idle", "Punktfunk", "a client is streaming", "block"),
                )
                .await?;
            reply.body().deserialize()
        });
        match attempt {
            Ok(fd) => Some(fd),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not take a logind sleep/idle inhibitor — the box may auto-suspend \
                     under a passive (video-only) viewer"
                );
                None
            }
        }
    })
    .join()
    .ok()
    .flatten();
    if fd.is_some() {
        tracing::info!("holding a logind sleep/idle inhibitor while clients stream");
    }
    fd
}
