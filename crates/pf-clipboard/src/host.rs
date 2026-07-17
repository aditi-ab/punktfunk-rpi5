//! Host-side shared-clipboard backend.
//!
//! The wire protocol and the client half live in `punktfunk-core`
//! (`punktfunk_core::quic` + `punktfunk_core::clipboard`); this module drives the **host's** real
//! session clipboard so it can offer what a host app copied and paste what the remote client
//! offered (`design/clipboard-and-file-transfer.md` §4).
//!
//! Concrete backends, selected at session start ([`HostClipboard::open`]) and presented as one
//! [`HostClipboard`] to the [`session`] coordinator:
//! * [`wayland`] (Linux) — `ext-data-control-v1` (KWin, wlroots / Sway, Hyprland). Preferred when present.
//! * [`mutter`] (Linux) — GNOME. Mutter implements **no** wlr/ext data-control, but its *direct*
//!   `org.gnome.Mutter.RemoteDesktop.Session` D-Bus API carries the same clipboard operations (the
//!   xdg `org.freedesktop.portal.Clipboard` would need an interactive grant a headless host can't
//!   answer — so we skip it and talk to Mutter directly, as the input injector already does).
//! * [`windows`] — the Win32 clipboard: a hidden message-only window watches `WM_CLIPBOARDUPDATE`
//!   and serves client content via OLE delayed rendering (`WM_RENDERFORMAT`).
//!
//! The `zwlr-data-control-unstable-v1` fallback (older wlroots/KWin) is a follow-up. The module
//! compiles on Linux and Windows; the [`session`] coordinator is backend-agnostic.

#[cfg(target_os = "linux")]
mod mutter;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "windows")]
mod windows;
/// Pure Win32-clipboard ↔ wire byte conversions (CF_HTML offset math, UTF-16 text, RTF NUL
/// trimming). Free of any Win32 dependency, so it compiles — and its unit tests run — on any host
/// (`cfg(test)`); the Windows backend is the only production consumer.
#[cfg(any(target_os = "windows", test))]
mod winfmt;

pub mod session;

#[cfg(target_os = "linux")]
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::sync::Arc;

/// A clipboard event surfaced by a host backend to the [`session`] coordinator. Both the
/// data-control and Mutter backends emit this identical shape.
pub enum ClipEvent {
    /// The host selection changed (a host app copied). `mimes` are the **wire** MIMEs offered (empty
    /// = the clipboard was cleared). The coordinator forwards these as a `ClipOffer` to the client;
    /// bytes cross only if the client later fetches.
    Selection { mimes: Vec<String> },
    /// A host app is pasting content the client offered. The coordinator fetches the wire-`mime`
    /// bytes from the client and hands them to `responder`.
    Paste {
        mime: String,
        responder: PasteResponder,
    },
    /// The backend ended (compositor / session gone).
    Closed,
}

/// How a backend receives the bytes answering a [`ClipEvent::Paste`]. The two host clipboard
/// mechanisms complete a paste differently, so the coordinator stays agnostic by handing bytes to
/// whichever responder the backend attached.
pub enum PasteResponder {
    /// data-control: the compositor handed us the destination pipe on the `send` event — write the
    /// bytes and close it (EOF completes the paste).
    #[cfg(target_os = "linux")]
    Fd(OwnedFd),
    /// Mutter: hand the bytes back to the backend actor, which owns the `SelectionWrite` fd and the
    /// trailing `SelectionWriteDone` call that Mutter's transfer requires.
    #[cfg(target_os = "linux")]
    Channel(tokio::sync::oneshot::Sender<Vec<u8>>),
    /// Windows: hand the bytes to the `WM_RENDERFORMAT` handler blocking the clipboard message-loop
    /// thread, which then `SetClipboardData`s them for the pasting app (`std::sync::mpsc`, since that
    /// thread waits synchronously — see [`windows`]).
    #[cfg(target_os = "windows")]
    Sync(std::sync::mpsc::Sender<Vec<u8>>),
}

impl PasteResponder {
    /// Deliver the fetched bytes (empty on a failed fetch → an empty paste, never a hang).
    pub async fn respond(self, bytes: Vec<u8>) {
        match self {
            #[cfg(target_os = "linux")]
            PasteResponder::Fd(fd) => {
                let _ = tokio::task::spawn_blocking(move || fulfill_paste(fd, &bytes)).await;
            }
            #[cfg(target_os = "linux")]
            PasteResponder::Channel(tx) => {
                let _ = tx.send(bytes);
            }
            #[cfg(target_os = "windows")]
            PasteResponder::Sync(tx) => {
                let _ = tx.send(bytes);
            }
        }
    }
}

/// Write `bytes` into a paste pipe `fd` and close it (EOF signals the reader). Blocking — run off the
/// reactor for large payloads.
#[cfg(target_os = "linux")]
fn fulfill_paste(fd: OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    Ok(())
}

/// The active host clipboard backend, chosen per session: `ext-data-control`
/// (KWin/wlroots/Hyprland/Sway) or Mutter's direct RemoteDesktop clipboard (GNOME) on Linux, or the
/// Win32 clipboard on Windows. Presented as one type so the [`session`] coordinator is
/// backend-agnostic.
pub enum HostClipboard {
    #[cfg(target_os = "linux")]
    DataControl(wayland::ClipboardBackend),
    #[cfg(target_os = "linux")]
    Mutter(mutter::MutterClipboard),
    #[cfg(target_os = "windows")]
    Windows(windows::WindowsClipboard),
}

impl HostClipboard {
    /// Open whichever backend this session supports. Linux tries data-control first
    /// (KWin/wlroots/Hyprland/Sway) then Mutter's direct clipboard (GNOME); Windows opens the Win32
    /// clipboard. Errors when none is available (gamescope, no live compositor) — the caller then
    /// reports `BACKEND_UNAVAILABLE`.
    pub async fn open() -> anyhow::Result<(
        HostClipboard,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        #[cfg(target_os = "linux")]
        {
            // data-control's bind does blocking Wayland roundtrips — keep them off the reactor.
            let dc = tokio::task::spawn_blocking(wayland::ClipboardBackend::open)
                .await
                .map_err(|e| anyhow::anyhow!("data-control open join: {e}"))?;
            match dc {
                Ok((b, rx)) => return Ok((HostClipboard::DataControl(b), rx)),
                Err(e) => tracing::debug!(
                    error = format!("{e:#}"),
                    "no ext-data-control — trying Mutter direct clipboard"
                ),
            }
            let (m, rx) = mutter::MutterClipboard::open().await.map_err(|e| {
                e.context("no clipboard backend (neither ext-data-control nor Mutter)")
            })?;
            Ok((HostClipboard::Mutter(m), rx))
        }
        #[cfg(target_os = "windows")]
        {
            let (b, rx) = windows::WindowsClipboard::open().await?;
            Ok((HostClipboard::Windows(b), rx))
        }
    }

    /// The current host selection's wire MIMEs (empty = nothing to offer).
    pub fn current_wire_mimes(&self) -> Vec<String> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.current_wire_mimes(),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => m.current_wire_mimes(),
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => w.current_wire_mimes(),
        }
    }

    /// Install a client's offered formats as the host selection.
    pub fn set_offer(&self, wire_mimes: &[String]) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.set_offer(wire_mimes),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => {
                m.set_offer(wire_mimes);
                Ok(())
            }
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => {
                w.set_offer(wire_mimes);
                Ok(())
            }
        }
    }

    /// Drop the host selection we own.
    pub fn clear_offer(&self) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.clear_offer(),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => {
                m.clear_offer();
                Ok(())
            }
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => {
                w.clear_offer();
                Ok(())
            }
        }
    }

    /// Read one wire format of the current host selection (a client's fetch). Async: data-control
    /// blocks on a pipe (offloaded), Mutter round-trips D-Bus + reads a pipe, Windows reads the
    /// clipboard on a blocking thread.
    pub async fn read_current(self: &Arc<Self>, wire_mime: &str) -> anyhow::Result<Vec<u8>> {
        match &**self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(_) => {
                let me = Arc::clone(self);
                let wire = wire_mime.to_string();
                tokio::task::spawn_blocking(move || match &*me {
                    HostClipboard::DataControl(b) => b.read_current(&wire),
                    _ => unreachable!("variant checked above"),
                })
                .await
                .map_err(|e| anyhow::anyhow!("data-control read join: {e}"))?
            }
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => m.read_current(wire_mime).await,
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => w.read_current(wire_mime).await,
        }
    }
}

// ---- Format normalization (design/clipboard-and-file-transfer.md §3.5) ------------------------
//
// One portable vocabulary crosses the wire; each end maps to platform types at fetch time. Phase 1
// covers text / RTF / HTML / PNG (files are Phase 2). The wire MIMEs match the core's table.

/// Wire MIME for UTF-8 plain text.
pub const WIRE_TEXT: &str = "text/plain;charset=utf-8";
/// Wire MIME for HTML.
pub const WIRE_HTML: &str = "text/html";
/// Wire MIME for rich text.
pub const WIRE_RTF: &str = "text/rtf";
/// Wire MIME for a PNG image.
pub const WIRE_PNG: &str = "image/png";
/// Wire MIME for a JPEG image — passed through VERBATIM when the source clipboard carries one
/// (no PNG transcode: a lossy original re-encoded lossless is pure bloat). [`WIRE_PNG`] remains
/// the universal fallback every peer must accept; JPEG/GIF are richer options beside it.
pub const WIRE_JPEG: &str = "image/jpeg";
/// Wire MIME for a GIF image — verbatim pass-through preserves animation end to end.
pub const WIRE_GIF: &str = "image/gif";

/// Map a Wayland selection MIME to its canonical wire MIME, or `None` to drop it (internal targets
/// like `TARGETS`/`TIMESTAMP`/`SAVE_TARGETS`, and formats we don't sync in Phase 1). Aliases
/// collapse onto one canonical wire name so the offered list dedups cleanly.
#[cfg(target_os = "linux")]
pub fn wayland_to_wire(wl: &str) -> Option<&'static str> {
    // Strip any parameter noise for the plain-text aliases (some apps send `text/plain;charset=...`
    // with odd charsets, or bare `text/plain`).
    let base = wl.split(';').next().unwrap_or(wl).trim();
    match wl {
        "text/html" => Some(WIRE_HTML),
        "text/rtf" | "application/rtf" | "text/richtext" => Some(WIRE_RTF),
        "image/png" => Some(WIRE_PNG),
        "image/jpeg" => Some(WIRE_JPEG),
        "image/gif" => Some(WIRE_GIF),
        _ => match base {
            "text/plain" | "UTF8_STRING" | "STRING" | "TEXT" => Some(WIRE_TEXT),
            _ => None,
        },
    }
}

/// The Wayland MIME candidates to request, in preference order, when a client fetches `wire` from
/// the host clipboard. The first one present in the current offer is used.
#[cfg(target_os = "linux")]
pub fn wayland_candidates(wire: &str) -> &'static [&'static str] {
    match wire {
        WIRE_TEXT => &[
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ],
        WIRE_HTML => &["text/html"],
        WIRE_RTF => &["text/rtf", "application/rtf", "text/richtext"],
        WIRE_PNG => &["image/png"],
        WIRE_JPEG => &["image/jpeg"],
        WIRE_GIF => &["image/gif"],
        _ => &[],
    }
}

/// Pick the Wayland MIME to `receive()` for a wire fetch: the first [`wayland_candidates`] entry the
/// current selection actually advertises.
#[cfg(target_os = "linux")]
pub fn pick_wayland_mime(wire: &str, available: &[String]) -> Option<String> {
    wayland_candidates(wire)
        .iter()
        .find(|c| available.iter().any(|a| a == *c))
        .map(|c| c.to_string())
}

/// Normalize a raw Wayland offer's MIME list into the deduplicated wire MIME list announced to the
/// client (drops internal targets; collapses aliases; preserves a stable order).
#[cfg(target_os = "linux")]
pub fn offer_wire_mimes(raw: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for m in raw {
        if let Some(wire) = wayland_to_wire(m) {
            if !out.contains(&wire) {
                out.push(wire);
            }
        }
    }
    out
}

/// The Wayland MIMEs to advertise when installing a source for a client's offer. Each wire MIME
/// expands to its canonical Wayland name(s); a rich-text-only offer also advertises `text/plain`
/// so plain-text targets always paste (§3.5 synthesis — destination-side, one direction only).
#[cfg(target_os = "linux")]
pub fn wayland_offers_for(wire_mimes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    let mut has_plain = false;
    let mut has_rich = false;
    for w in wire_mimes {
        match w.as_str() {
            WIRE_TEXT => {
                has_plain = true;
                push("text/plain;charset=utf-8");
                push("text/plain");
                push("UTF8_STRING");
                push("STRING");
            }
            WIRE_HTML => {
                has_rich = true;
                push("text/html");
            }
            WIRE_RTF => {
                has_rich = true;
                push("text/rtf");
            }
            WIRE_PNG => push("image/png"),
            WIRE_JPEG => push("image/jpeg"),
            WIRE_GIF => push("image/gif"),
            other => push(other),
        }
    }
    // Synthesis: rich text without plain text → also advertise plain (the source derives it lazily).
    if has_rich && !has_plain {
        push("text/plain;charset=utf-8");
        push("text/plain");
        push("UTF8_STRING");
        push("STRING");
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn wayland_to_wire_canonicalizes_and_drops_targets() {
        assert_eq!(wayland_to_wire("text/plain"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("UTF8_STRING"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("text/plain;charset=utf-8"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("text/html"), Some(WIRE_HTML));
        assert_eq!(wayland_to_wire("application/rtf"), Some(WIRE_RTF));
        assert_eq!(wayland_to_wire("image/png"), Some(WIRE_PNG));
        // Internal targets and unsupported formats are dropped.
        assert_eq!(wayland_to_wire("TARGETS"), None);
        assert_eq!(wayland_to_wire("TIMESTAMP"), None);
        assert_eq!(wayland_to_wire("image/jpeg"), None);
    }

    #[test]
    fn offer_wire_mimes_dedups_aliases() {
        let raw = vec![
            "TARGETS".to_string(),
            "UTF8_STRING".to_string(),
            "text/plain;charset=utf-8".to_string(),
            "text/plain".to_string(),
            "text/html".to_string(),
        ];
        // text aliases collapse to one WIRE_TEXT; TARGETS dropped; html kept.
        assert_eq!(offer_wire_mimes(&raw), vec![WIRE_TEXT, WIRE_HTML]);
    }

    #[test]
    fn pick_wayland_mime_prefers_canonical() {
        let avail = vec!["text/plain".to_string(), "UTF8_STRING".to_string()];
        // Canonical charset form isn't present, so it falls to the next candidate.
        assert_eq!(
            pick_wayland_mime(WIRE_TEXT, &avail),
            Some("text/plain".to_string())
        );
        let avail2 = vec![
            "text/plain;charset=utf-8".to_string(),
            "text/plain".to_string(),
        ];
        assert_eq!(
            pick_wayland_mime(WIRE_TEXT, &avail2),
            Some("text/plain;charset=utf-8".to_string())
        );
        assert_eq!(pick_wayland_mime(WIRE_PNG, &avail2), None);
    }

    #[test]
    fn wayland_offers_synthesizes_plain_for_rich_only() {
        let offers = wayland_offers_for(&[WIRE_HTML.to_string()]);
        assert!(offers.iter().any(|m| m == "text/html"));
        assert!(
            offers.iter().any(|m| m == "text/plain;charset=utf-8"),
            "rich-only offer must synthesize plain text: {offers:?}"
        );
        // Plain already present → no duplicate synthesis, and text aliases included.
        let offers2 = wayland_offers_for(&[WIRE_TEXT.to_string()]);
        assert!(offers2.iter().any(|m| m == "UTF8_STRING"));
        assert_eq!(offers2.iter().filter(|m| *m == "text/plain").count(), 1);
    }
}
