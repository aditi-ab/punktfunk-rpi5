//! Shared clipboard, host side (plan §W6 shape; `design/clipboard-and-file-transfer.md` §4).
//!
//! The wire protocol and the client half live in `punktfunk-core` (`punktfunk_core::quic` +
//! `punktfunk_core::clipboard`); this crate drives the **host's** real session clipboard through
//! the per-OS backends in [`host`] and bridges it to the QUIC clipboard plane through the
//! [`host::session`] coordinator.
//!
//! The orchestrator consumes only this portable facade — [`policy`] / [`enabled`] /
//! [`cap_advertised`], the [`ClipCoordCmd`] channel vocabulary, [`start`], and
//! [`spawn_decline_loop`] — so its control loop compiles unchanged on every host platform; the
//! platform split lives entirely behind [`start`].

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use punktfunk_core::quic::ClipOffer;

/// The per-OS backends (`ext-data-control-v1` / Mutter direct / Win32) behind one
/// `HostClipboard`, plus the backend-agnostic [`host::session`] coordinator.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod host;

/// Operator clipboard policy from `PUNKTFUNK_CLIPBOARD` (`design/clipboard-and-file-transfer.md`
/// §4.2): `off` (default — the whole feature is dark), `on` / `1` (text + files), `text-only` /
/// `no-files` (text/RTF/HTML/image only). Returns `None` when clipboard is off (the host neither
/// advertises the cap nor accepts fetch streams); otherwise the permitted-format
/// [`punktfunk_core::quic::CLIP_POLICY_TEXT`] / `CLIP_POLICY_FILES` bitfield.
///
/// The policy gates the advertised capability and whether the [`host::session`] coordinator
/// starts. `off` keeps the whole feature dark.
pub fn policy() -> Option<u8> {
    use punktfunk_core::quic::{CLIP_POLICY_FILES, CLIP_POLICY_TEXT};
    match std::env::var("PUNKTFUNK_CLIPBOARD")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "0" | "off" | "false" => None,
        "text-only" | "no-files" | "text" => Some(CLIP_POLICY_TEXT),
        _ => Some(CLIP_POLICY_TEXT | CLIP_POLICY_FILES), // "on" / "1" / anything truthy
    }
}

/// Whether the shared clipboard is enabled at all for this host (policy not `off`).
pub fn enabled() -> bool {
    policy().is_some()
}

/// Whether the host should advertise `HOST_CAP_CLIPBOARD` in the `Welcome`: the operator policy
/// enables it AND this platform has a backend (Linux data-control / Mutter, or the Win32
/// clipboard) — the client greys the toggle out otherwise. A Linux host whose compositor lacks
/// data-control still advertises it and answers a later enable with `BACKEND_UNAVAILABLE`, so the
/// client can surface *why* it's unavailable.
pub fn cap_advertised() -> bool {
    enabled() && cfg!(any(target_os = "linux", target_os = "windows"))
}

/// A command from the session control loop into the host clipboard coordinator
/// ([`host::session`]). Defined here — portable — so the control loop compiles on every host
/// platform; the coordinator that consumes it exists only where a backend does.
pub enum ClipCoordCmd {
    /// The client toggled sync. When enabled, the coordinator (re)announces the current host
    /// clipboard; when disabled, it drops any selection it owns and stops forwarding host copies.
    SetEnabled(bool),
    /// The client copied: install its offered wire MIMEs as a lazy host selection (empty = clear).
    RemoteOffer { seq: u32, mimes: Vec<String> },
}

/// Handle to the host clipboard coordinator, held by the session control loop.
pub struct ClipCoord {
    /// Whether a real backend is live. `false` on gamescope / older GNOME / an unsupported
    /// platform; the control loop then answers an enable request with
    /// `CLIP_REASON_BACKEND_UNAVAILABLE` and [`spawn_decline_loop`] handles any stray fetch stream.
    pub available: bool,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<ClipCoordCmd>,
    /// Host-copy announcements from the coordinator → control loop → client.
    pub offer_rx: tokio::sync::mpsc::UnboundedReceiver<ClipOffer>,
}

/// Open the host clipboard backend (when the operator policy allows it, this session mirrors a
/// real compositor, and the platform has a backend) and spawn its coordinator, returning a handle.
/// Otherwise the handle is inert (`available = false`, channels dropped) so the caller's control
/// loop stays platform-agnostic. `has_compositor` is false for the synthetic protocol-test source,
/// which has no display/clipboard to share — keeping it out of the real session clipboard.
pub async fn start(
    conn: quinn::Connection,
    clip_enabled: Arc<AtomicBool>,
    has_compositor: bool,
) -> ClipCoord {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let available = if has_compositor && enabled() {
        host::session::start(conn, clip_enabled, cmd_rx, offer_tx).await
    } else {
        drop((conn, clip_enabled, cmd_rx, offer_tx));
        false
    };
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let available = {
        let _ = (conn, clip_enabled, cmd_rx, offer_tx, has_compositor);
        false
    };
    ClipCoord {
        available,
        cmd_tx,
        offer_rx,
    }
}

/// Clipboard fetch-stream accept loop, fallback flavor (`design/clipboard-and-file-transfer.md`
/// §3.3, §4.2). When a backend is live the coordinator (spawned by [`start`]) owns `accept_bi` and
/// serves real host clipboard bytes. This is for the other case: the operator allowed the cap but
/// no backend bound (gamescope / older GNOME / a not-yet-implemented platform), so a stray or
/// hostile fetch stream is answered `CLIP_FETCH_UNAVAILABLE` instead of hanging. Exactly one
/// `accept_bi` consumer runs (this OR the coordinator). The control stream is the FIRST bi-stream
/// (already accepted at the handshake), so this loop only ever sees clipboard fetch streams; it
/// dies with the connection.
pub fn spawn_decline_loop(conn: quinn::Connection) {
    tokio::spawn(async move {
        use punktfunk_core::quic::{clipstream, ClipFetchHdr, CLIP_FETCH_UNAVAILABLE};
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                // Validate the stream header + request; a malformed/unknown stream is dropped.
                match clipstream::read_stream_header(&mut recv).await {
                    Ok(k) if k == clipstream::CLIP_STREAM_KIND_FETCH => {}
                    _ => {
                        let _ = send.reset(clipstream::cancelled_code());
                        return;
                    }
                }
                if clipstream::read_fetch(&mut recv).await.is_err() {
                    return;
                }
                let _ = clipstream::write_fetch_hdr(
                    &mut send,
                    &ClipFetchHdr {
                        status: CLIP_FETCH_UNAVAILABLE,
                        total_size: 0,
                    },
                )
                .await;
            });
        }
    });
}
