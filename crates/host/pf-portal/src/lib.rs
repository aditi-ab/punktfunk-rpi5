//! Process-lifetime tokio runtime for every portal call in the host, and the session-bus
//! connection each call makes.
//!
//! zbus spawns a connection's reader on the tokio runtime current when it is built. A portal
//! session's connection is built inside `portal_runtime().block_on`, so its reader lives as
//! long as the session, whichever thread asked for it.
//!
//! Never drop this runtime. `block_on` takes `&self`, so every portal thread can park on it
//! concurrently: capture, the virtual displays and the libei injector. A portal session lives
//! while its proxy holds the connection: close it explicitly (`close_session`).
//!
//! Every proxy is built `with_connection(session_bus())`. ashpd's `new()` constructors share
//! one connection, opened from the environment at first use, which never follows the session.
//!
//! [`cursor_mode`] is the pure ScreenCast cursor ladder, compiled on every
//! target so its tests run without a compositor.

pub mod cursor_mode;
#[cfg(target_os = "linux")]
mod handshake;
#[cfg(target_os = "linux")]
pub use handshake::{
    close_session, finish_or_close, negotiate_cursor_mode, to_ashpd, within, CAST_CLOSE_BUDGET,
    HANDSHAKE_BUDGET,
};

#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use tokio::runtime::Runtime;

/// A connection to the live session's bus, by [`pf_host_config::session::dbus_address`], for a
/// caller that names or serves something on it first.
#[cfg(target_os = "linux")]
pub fn session_bus_builder() -> ashpd::zbus::Result<ashpd::zbus::connection::Builder<'static>> {
    use ashpd::zbus::connection::Builder;
    match pf_host_config::session::dbus_address() {
        Some(address) => Builder::address(address.as_str()),
        None => Builder::session(),
    }
}

/// A new connection from [`session_bus_builder`]. Its owner decides how long it lives: Mutter
/// and the portals end the sessions made on it when it closes.
#[cfg(target_os = "linux")]
pub async fn session_bus() -> ashpd::zbus::Result<ashpd::zbus::Connection> {
    session_bus_builder()?.build().await
}

/// `Result` so a failed build fails the handshake with a reason instead of aborting the process.
#[cfg(target_os = "linux")]
static PORTAL_RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();

/// Multi-thread, 2 workers: the zbus reader must run across `create_session`
/// → `select_sources` → `start` while a portal thread blocks on `block_on`.
/// A current-thread runtime cannot pump that.
#[cfg(target_os = "linux")]
pub fn portal_runtime() -> Result<&'static Runtime, String> {
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
