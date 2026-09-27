//! Process-lifetime tokio runtime for every portal handshake.
//!
//! ashpd caches its D-Bus connection in a process-global `OnceLock`. The first
//! portal proxy creates it, and zbus spawns the connection's reader on
//! whichever tokio runtime is current at that moment.
//!
//! A per-session runtime that is dropped at teardown leaves that cached
//! connection with no executor. Every later portal call in the process then
//! waits for a reply nothing is left alive to read.
//!
//! Never build a per-session runtime, and never drop this one. `block_on`
//! takes `&self`, so every portal thread can park on it concurrently. A portal
//! session made here outlives the thread that made it: close it explicitly.
//!
//! Every handshake also shares the cursor-mode negotiation
//! ([`negotiate_cursor_mode`]).

use ashpd::desktop::screencast::{CursorMode, Screencast};
use ashpd::enumflags2::BitFlags;
use pf_frame::cursor_mode::{parse_pin, pick, Mode, Pin};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::runtime::Runtime;

/// `Result` so a failed build fails the handshake with a reason instead of aborting the process.
static PORTAL_RT: OnceLock<std::io::Result<Runtime>> = OnceLock::new();

/// Multi-thread, 2 workers: the zbus reader must run across `create_session`
/// → `select_sources` → `start` while a portal thread blocks on `block_on`.
/// A current-thread runtime cannot pump that.
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

/// `AvailableCursorModes`, re-read while it is empty.
///
/// A portal that the ScreenCast call itself D-Bus-activated publishes `0`
/// until its backend answers. xdg-desktop-portal validates `SelectSources`
/// against this same property, so the settled value is the one that counts.
/// Returns whatever it reads last, empty included, after 2 s.
pub async fn available_cursor_modes(proxy: &Screencast) -> ashpd::Result<BitFlags<CursorMode>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let avail = proxy.available_cursor_modes().await?;
        if !avail.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(avail);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `Metadata` when the session has a cursor channel (the client or encoder
/// draws, so the compositor must not burn the pointer in), else `Embedded`.
/// `PUNKTFUNK_PORTAL_CURSOR_MODE` overrides both. `backend` is the log line only.
fn want(hw_cursor: bool, backend: &str) -> Mode {
    let negotiated = if hw_cursor {
        Mode::Metadata
    } else {
        Mode::Embedded
    };
    let raw = match pf_host_config::config().portal_cursor_mode.as_deref() {
        Some(raw) => raw,
        None => return negotiated,
    };
    match parse_pin(raw) {
        Pin::Auto => negotiated,
        Pin::Mode(pinned) => {
            tracing::info!(
                backend,
                pinned = pinned.name(),
                negotiated = negotiated.name(),
                "ScreenCast: cursor mode pinned by PUNKTFUNK_PORTAL_CURSOR_MODE"
            );
            pinned
        }
        Pin::Unrecognised => {
            tracing::warn!(
                backend,
                value = raw,
                negotiated = negotiated.name(),
                "ScreenCast: unrecognised PUNKTFUNK_PORTAL_CURSOR_MODE (want auto|hidden|embedded|\
                 metadata) — ignoring"
            );
            negotiated
        }
    }
}

/// The `SelectSources` cursor mode for every portal cast, through the one
/// ladder in [`pf_frame::cursor_mode`]. Never an unadvertised bit: the portal
/// closes a session that asks for one. An empty or failed read requests
/// `Embedded`; the mode is fixed for the session.
pub async fn negotiate_cursor_mode(proxy: &Screencast, hw_cursor: bool, backend: &str) -> Mode {
    let want = want(hw_cursor, backend);
    let advertised = match available_cursor_modes(proxy).await {
        Ok(avail) if !avail.is_empty() => avail.bits(),
        Ok(_) => {
            tracing::warn!(
                backend,
                "ScreenCast: portal advertised no cursor modes — requesting Embedded cursor"
            );
            return Mode::Embedded;
        }
        Err(e) => {
            // ScreenCast v2 property. A portal that cannot publish it is too old for Metadata.
            tracing::warn!(
                backend,
                error = %e,
                "ScreenCast: AvailableCursorModes query failed — requesting Embedded cursor"
            );
            return Mode::Embedded;
        }
    };
    let choice = pick(advertised, want);
    match choice.wanted {
        None => tracing::info!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            mode = choice.mode.name(),
            "ScreenCast: cursor mode negotiated"
        ),
        Some(wanted) => tracing::warn!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            wanted = wanted.name(),
            mode = choice.mode.name(),
            "ScreenCast: requested cursor mode is not advertised by this portal — downgrading \
             (requesting it anyway would close the session)"
        ),
    }
    choice.mode
}

/// ashpd's flag for a negotiated [`Mode`].
pub fn to_ashpd(mode: Mode) -> CursorMode {
    match mode {
        Mode::Hidden => CursorMode::Hidden,
        Mode::Embedded => CursorMode::Embedded,
        Mode::Metadata => CursorMode::Metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ashpd `CursorMode` bits follow enumflags2 declaration order; a reorder
    /// silently repoints every mode.
    #[test]
    fn mode_bits_match_ashpd() {
        for m in [Mode::Hidden, Mode::Embedded, Mode::Metadata] {
            assert_eq!(
                BitFlags::from_flag(to_ashpd(m)).bits(),
                m.bit(),
                "{} drifted from ashpd",
                m.name()
            );
        }
        assert_eq!(BitFlags::from_flag(CursorMode::Metadata).bits(), 4);
    }
}
