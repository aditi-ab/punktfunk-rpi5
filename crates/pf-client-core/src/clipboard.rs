//! OS-clipboard bridge for the spawned session client (`design/clipboard-and-file-transfer.md`
//! §5). The protocol half already exists in `punktfunk_core::clipboard` — the per-session task
//! that runs fetch streams — and `NativeClient` exposes it as `clip_control` / `clip_offer` /
//! `clip_fetch` / `clip_serve` / `next_clip`. What was missing on Windows is exactly what §5.2
//! writes in Swift for macOS: the code that talks to the actual pasteboard. This is that half.
//!
//! Shape (one thread, owned by the session pump):
//!
//! * **Local → remote** stays lazy by construction. A poll of `GetClipboardSequenceNumber`
//!   spots a local copy, we announce the FORMAT LIST (`clip_offer`) and nothing else; the
//!   bytes are read only if the host actually pastes and sends a `FetchRequest`.
//! * **Remote → local** is EAGER in this first cut, and that is a deliberate deviation from
//!   §5.2's promise-based apply. macOS gets laziness free from `NSPasteboardItemDataProvider`;
//!   the Windows equivalent is delayed rendering (`SetClipboardData(fmt, NULL)` answered on
//!   `WM_RENDERFORMAT`), which needs a clipboard-owning window running its own message pump —
//!   a bigger piece than this. So we fetch on the offer and place real bytes, under
//!   [`EAGER_FETCH_CAP`] so a huge host-side copy can't pull megabytes nobody pastes. Text is
//!   tiny and always crosses; a large image simply isn't mirrored until delayed rendering lands.
//! * **Echo suppression** is §3.4's Windows rule verbatim: record the clipboard sequence
//!   number right after our own `SetClipboardData` and ignore exactly that change, or every
//!   copy ping-pongs between the two machines forever.
//!
//! Secrets are respected: a clipboard carrying `ExcludeClipboardContentFromMonitorProcessing`
//! (what password managers set) is never announced and never served — the Windows counterpart
//! of §5.2's `org.nspasteboard.ConcealedType` skip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use punktfunk_core::client::NativeClient;
use punktfunk_core::clipboard::ClipEventCore;
use punktfunk_core::quic::{ClipKind, CLIP_FILE_INDEX_NONE, HOST_CAP_CLIPBOARD};

/// Wire mime for UTF-8 text — the one format every peer must handle (§3.5).
const MIME_TEXT: &str = "text/plain;charset=utf-8";
/// Wire mime for the image floor (§3.5). Read/written through the "PNG" registered clipboard
/// format; apps that only publish `CF_DIB` are a follow-up (the conversion the host already
/// has in `image_to_dib`).
const MIME_PNG: &str = "image/png";

/// Ceiling on an EAGERLY fetched remote payload (see the module docs). Text never approaches
/// it; it exists so a host-side copy of something enormous doesn't cross for a paste that may
/// never happen. Lifted once delayed rendering makes the fetch lazy.
const EAGER_FETCH_CAP: u64 = 4 << 20;

/// How often the local clipboard is polled for changes. §3.2 asks for ≥ 100 ms between offers;
/// 400 ms keeps a copy→focus→paste round trip comfortably ahead of the user.
const POLL: Duration = Duration::from_millis(400);

/// Drain-and-poll cadence: how long `next_clip` blocks before we re-check the clipboard and
/// the stop flag.
const EVENT_WAIT: Duration = Duration::from_millis(120);

/// Run the clipboard bridge until `stop` is set or the session closes. Returns immediately
/// (doing nothing) when the host didn't advertise `HOST_CAP_CLIPBOARD` — an older host, or one
/// whose backend can't do it — so this is safe to spawn unconditionally.
pub fn run(client: Arc<NativeClient>, stop: Arc<AtomicBool>) {
    if client.host_caps() & HOST_CAP_CLIPBOARD == 0 {
        tracing::info!("host has no clipboard capability — shared clipboard off");
        return;
    }
    // Opt-in per §3.1: nothing is announced or served until this crosses enabled.
    if let Err(e) = client.clip_control(true, 0) {
        tracing::warn!(error = %e, "clipboard: enable failed");
        return;
    }
    tracing::info!("shared clipboard enabled");

    let mut state = State {
        // Adopt the CURRENT sequence number without announcing: whatever is on the clipboard
        // from before the session started is the user's, not a copy they made for this stream.
        last_seq: os::sequence_number(),
        ..Default::default()
    };
    let mut next_poll = Instant::now() + POLL;

    while !stop.load(Ordering::SeqCst) {
        // Inbound first — a pending FetchRequest is the host waiting on us. `NoFrame` is the
        // ordinary poll timeout (nothing pending); anything else means the connection is gone
        // and the session teardown is already on its way.
        match client.next_clip(EVENT_WAIT) {
            Ok(ev) => handle_event(&client, &mut state, ev),
            Err(punktfunk_core::error::PunktfunkError::NoFrame) => {}
            Err(_) => break,
        }
        // The local clipboard is polled on its OWN cadence, not once per inbound wait: the
        // event wait is short (it bounds teardown latency), and hammering the Win32 clipboard
        // eight times a second would contend with whatever app the user is actually copying in.
        let now = Instant::now();
        if now >= next_poll {
            poll_local(&client, &mut state);
            next_poll = now + POLL;
        }
    }
    // Best-effort: tell the host to stop announcing into a session that's ending.
    let _ = client.clip_control(false, 0);
}

#[derive(Default)]
struct State {
    /// Clipboard sequence number as of our last look — a change means someone copied.
    last_seq: u32,
    /// The sequence number our OWN `SetClipboardData` produced (§3.4 echo suppression).
    self_written_seq: Option<u32>,
    /// Monotonic offer counter (§3.2 — newest wins).
    offer_seq: u32,
    /// Rate-limit guard for offers (§3.2 asks ≥ 100 ms).
    last_offer: Option<Instant>,
    /// The host's current offer, so a fetch can name its `seq`.
    remote_offer: Option<u32>,
    /// In-flight eager fetch → the mime it will deliver, so `Data` knows how to place it.
    pending_fetch: Option<(u32, String)>,
    /// The last payload we placed locally, kept only to answer a host fetch of our own echo
    /// without re-reading the OS clipboard.
    last_applied: Option<(String, Vec<u8>)>,
}

/// A local clipboard change → announce the format list (never the bytes).
fn poll_local(client: &NativeClient, state: &mut State) {
    let seq = os::sequence_number();
    if seq == state.last_seq {
        return;
    }
    state.last_seq = seq;
    // Our own apply — swallow it, or the two clipboards chase each other forever.
    if state.self_written_seq == Some(seq) {
        state.self_written_seq = None;
        return;
    }
    if let Some(t) = state.last_offer {
        if t.elapsed() < Duration::from_millis(100) {
            return;
        }
    }
    if os::is_concealed() {
        tracing::debug!("clipboard: concealed content — not announced");
        return;
    }
    let mut kinds: Vec<ClipKind> = Vec::new();
    for (mime, size) in os::available_kinds() {
        kinds.push(ClipKind {
            mime: mime.to_string(),
            size_hint: size,
        });
    }
    if kinds.is_empty() {
        return;
    }
    state.offer_seq = state.offer_seq.wrapping_add(1);
    state.last_offer = Some(Instant::now());
    let seq_id = state.offer_seq;
    tracing::debug!(
        seq = seq_id,
        kinds = kinds.len(),
        "clipboard: offering local copy"
    );
    if let Err(e) = client.clip_offer(seq_id, kinds) {
        tracing::warn!(error = %e, "clipboard: offer failed");
    }
}

fn handle_event(client: &NativeClient, state: &mut State, ev: ClipEventCore) {
    match ev {
        ClipEventCore::State {
            enabled,
            policy,
            reason,
        } => {
            tracing::info!(enabled, policy, reason, "clipboard: host state");
        }
        // The host copied. Pull the best format we can place (see the module docs on why this
        // is eager for now) — text preferred, then PNG.
        ClipEventCore::RemoteOffer { seq, kinds } => {
            state.remote_offer = Some(seq);
            let pick = kinds
                .iter()
                .find(|k| k.mime == MIME_TEXT)
                .or_else(|| kinds.iter().find(|k| k.mime == MIME_PNG));
            let Some(kind) = pick else {
                tracing::debug!("clipboard: remote offer has no format we can place");
                return;
            };
            if kind.size_hint > EAGER_FETCH_CAP {
                tracing::info!(
                    mime = %kind.mime,
                    size = kind.size_hint,
                    "clipboard: remote payload over the eager-fetch cap — not mirrored"
                );
                return;
            }
            match client.clip_fetch(seq, kind.mime.clone(), CLIP_FILE_INDEX_NONE) {
                Ok(xfer) => state.pending_fetch = Some((xfer, kind.mime.clone())),
                Err(e) => tracing::warn!(error = %e, "clipboard: fetch failed to start"),
            }
        }
        // Bytes for the fetch above — place them, then record the sequence number they cause.
        ClipEventCore::Data {
            xfer_id,
            bytes,
            last,
        } => {
            let Some((pending, mime)) = state.pending_fetch.clone() else {
                return;
            };
            if pending != xfer_id {
                return;
            }
            if last {
                state.pending_fetch = None;
            }
            match os::set(&mime, &bytes) {
                Ok(()) => {
                    // §3.4: this is the change WE caused; ignore exactly it.
                    state.self_written_seq = Some(os::sequence_number());
                    state.last_applied = Some((mime.clone(), bytes));
                    tracing::debug!(mime = %mime, "clipboard: applied remote content");
                }
                Err(e) => tracing::warn!(error = %e, mime = %mime, "clipboard: apply failed"),
            }
        }
        // The host is pasting what we offered: read the bytes NOW (this is the lazy half) and
        // answer. A read failure still answers — with a cancel — so the host isn't left waiting.
        ClipEventCore::FetchRequest {
            req_id,
            seq: _,
            file_index: _,
            mime,
        } => {
            if os::is_concealed() {
                let _ = client.clip_cancel(req_id);
                return;
            }
            // Serve our own last-applied payload verbatim when it still matches — avoids a
            // lossy OS round trip for content that originated on the host anyway.
            let bytes = match &state.last_applied {
                Some((m, b)) if *m == mime && state.self_written_seq.is_some() => Ok(b.clone()),
                _ => os::get(&mime),
            };
            match bytes {
                Ok(b) => {
                    tracing::debug!(mime = %mime, len = b.len(), "clipboard: serving to host");
                    if let Err(e) = client.clip_serve(req_id, b, true) {
                        tracing::warn!(error = %e, "clipboard: serve failed");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, mime = %mime, "clipboard: nothing to serve");
                    let _ = client.clip_cancel(req_id);
                }
            }
        }
        ClipEventCore::Cancelled { id } => tracing::debug!(id, "clipboard: transfer cancelled"),
        ClipEventCore::Error { id, code } => {
            tracing::debug!(id, code, "clipboard: transfer error");
            if state.pending_fetch.as_ref().is_some_and(|(x, _)| *x == id) {
                state.pending_fetch = None;
            }
        }
    }
}

/// Put plain text on this machine's clipboard — the "Copy link" affordance, which has nothing
/// to do with the streaming bridge above but wants the same OS plumbing. Best-effort: a
/// clipboard another process is holding open is a transient nuisance, never worth failing a
/// user action over. A no-op where this build has no OS clipboard.
pub fn set_text(text: &str) {
    if let Err(e) = os::set(MIME_TEXT, text.as_bytes()) {
        tracing::warn!(error = %format!("{e:#}"), "copying to the clipboard");
    }
}

#[cfg(windows)]
mod os {
    //! The Win32 clipboard seam. Every entry point opens the clipboard, does one thing and
    //! closes it — holding it across a network fetch would block every other app on the box.

    use super::{MIME_PNG, MIME_TEXT};
    use anyhow::{anyhow, bail, Result};
    use windows::core::PCWSTR;
    use windows::Win32::minwindef::HGLOBAL;
    use windows::Win32::winbase::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::winuser::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };

    const CF_UNICODETEXT: u32 = 13;

    /// A registered clipboard format id, by name (`PNG`, the concealed marker, …).
    fn registered(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a local NUL-terminated UTF-16 buffer that outlives this synchronous call.
        unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
    }

    fn png_format() -> u32 {
        registered("PNG")
    }

    /// RAII clipboard open — `CloseClipboard` must run even on the error paths.
    struct Clip;
    impl Clip {
        fn open() -> Result<Clip> {
            // A retry loop: another app can hold the clipboard for a moment.
            for _ in 0..10 {
                // SAFETY: takes no pointer; `None` is the documented "associate with no window" argument.
                if unsafe { OpenClipboard(None) }.as_bool() {
                    return Ok(Clip);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            bail!("clipboard busy")
        }
    }
    impl Drop for Clip {
        fn drop(&mut self) {
            // SAFETY: pairs with the `OpenClipboard` that built this guard; closing is what `Drop` is for.
            let _ = unsafe { CloseClipboard() };
        }
    }

    pub fn sequence_number() -> u32 {
        // SAFETY: a no-argument query that only reads the OS clipboard's sequence counter.
        unsafe { GetClipboardSequenceNumber() }
    }

    /// Password managers mark secrets with this format; §5.2's concealed-type rule.
    pub fn is_concealed() -> bool {
        let fmt = registered("ExcludeClipboardContentFromMonitorProcessing");
        // SAFETY: a scalar format id in, a status out; reads nothing through a pointer.
        unsafe { IsClipboardFormatAvailable(fmt) }.as_bool()
    }

    /// The wire kinds the current clipboard can supply, with size hints where they're free.
    pub fn available_kinds() -> Vec<(&'static str, u64)> {
        let mut out = Vec::new();
        // SAFETY: as above — a scalar format id, status out.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.as_bool() {
            out.push((MIME_TEXT, 0));
        }
        // SAFETY: as above — a scalar format id, status out.
        if unsafe { IsClipboardFormatAvailable(png_format()) }.as_bool() {
            out.push((MIME_PNG, 0));
        }
        out
    }

    /// Read one wire format off the clipboard.
    pub fn get(mime: &str) -> Result<Vec<u8>> {
        let _clip = Clip::open()?;
        match mime {
            MIME_TEXT => {
                // SAFETY: the clipboard is open (the `Clip` guard); the handle returned is BORROWED from the clipboard and stays valid while it is open, so it is never freed here.
                let g: HGLOBAL = unsafe { GetClipboardData(CF_UNICODETEXT) };
                if g.0.is_null() {
                    bail!("clipboard text unavailable");
                }
                // SAFETY: `g` is that borrowed clipboard handle; `GlobalLock` yields a pointer valid until the matching `GlobalUnlock` below.
                let p = unsafe { GlobalLock(g) } as *const u16;
                if p.is_null() {
                    bail!("clipboard text lock failed");
                }
                // GlobalSize is a byte count of a NUL-terminated UTF-16 buffer.
                // SAFETY: a size query on the same live handle.
                let bytes = unsafe { GlobalSize(g) };
                let mut len = bytes / 2;
                // SAFETY: `p` is the locked buffer and `len` is derived from the `GlobalSize` above, so the slice stays inside the allocation; it is read before the unlock.
                let slice = unsafe { std::slice::from_raw_parts(p, len) };
                if let Some(nul) = slice.iter().position(|&c| c == 0) {
                    len = nul;
                }
                // SAFETY: as above — same locked buffer and same length, read before the unlock.
                let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
                // SAFETY: releases the lock taken above, exactly once.
                let _ = unsafe { GlobalUnlock(g) };
                Ok(text.into_bytes())
            }
            MIME_PNG => {
                // SAFETY: as the text path — the handle is borrowed from the open clipboard, never freed here.
                let g: HGLOBAL = unsafe { GetClipboardData(png_format()) };
                if g.0.is_null() {
                    bail!("clipboard png unavailable");
                }
                // SAFETY: `g` is that borrowed handle; the pointer is valid until the matching unlock.
                let p = unsafe { GlobalLock(g) } as *const u8;
                if p.is_null() {
                    bail!("clipboard png lock failed");
                }
                // SAFETY: a size query on the same live handle.
                let len = unsafe { GlobalSize(g) };
                // SAFETY: `p` is the locked buffer and `len` came from `GlobalSize`, so the slice is in bounds; it is copied out before the unlock.
                let out = unsafe { std::slice::from_raw_parts(p, len) }.to_vec();
                // SAFETY: releases the lock taken above, exactly once.
                let _ = unsafe { GlobalUnlock(g) };
                Ok(out)
            }
            other => Err(anyhow!("unsupported clipboard format {other}")),
        }
    }

    /// Place one wire format on the clipboard, replacing its contents.
    pub fn set(mime: &str, bytes: &[u8]) -> Result<()> {
        let (fmt, payload) = match mime {
            MIME_TEXT => {
                let text = String::from_utf8_lossy(bytes);
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let raw: Vec<u8> = wide.iter().flat_map(|c| c.to_le_bytes()).collect();
                (CF_UNICODETEXT, raw)
            }
            MIME_PNG => (png_format(), bytes.to_vec()),
            other => bail!("unsupported clipboard format {other}"),
        };
        let _clip = Clip::open()?;
        // SAFETY: no arguments; the clipboard is open and owned by this thread via the `Clip` guard.
        if !unsafe { EmptyClipboard() }.as_bool() {
            bail!("clipboard clear failed");
        }
        // The clipboard OWNS this block once SetClipboardData succeeds — do not free it.
        // SAFETY: a size in, an owned moveable handle out — ownership passes to the clipboard at `SetClipboardData` below.
        let g = unsafe { GlobalAlloc(GMEM_MOVEABLE as u32, payload.len()) };
        if g.0.is_null() {
            bail!("clipboard alloc failed");
        }
        // SAFETY: `g` is the handle just allocated; the pointer is valid until the matching unlock.
        let p = unsafe { GlobalLock(g) } as *mut u8;
        if p.is_null() {
            bail!("clipboard alloc lock failed");
        }
        // SAFETY: `p` addresses that locked block, allocated at exactly `payload.len()` bytes on the line above, and the two regions are distinct allocations.
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), p, payload.len()) };
        // SAFETY: releases the lock taken above, before the handle is handed to the clipboard.
        let _ = unsafe { GlobalUnlock(g) };
        // SAFETY: ownership of `g` transfers to the clipboard here, which is why nothing frees it afterwards.
        if unsafe { SetClipboardData(fmt, Some(g)) }.0.is_null() {
            bail!("clipboard set failed");
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod os {
    //! Non-Windows stub. Linux needs the Wayland `data-control` seam (the same protocol the
    //! host side already speaks) — the bridge above is platform-neutral and will drive it
    //! unchanged once this module grows a real implementation.
    use anyhow::{bail, Result};

    pub fn sequence_number() -> u32 {
        0
    }
    pub fn is_concealed() -> bool {
        false
    }
    pub fn available_kinds() -> Vec<(&'static str, u64)> {
        Vec::new()
    }
    pub fn get(_mime: &str) -> Result<Vec<u8>> {
        bail!("clipboard unsupported on this platform")
    }
    pub fn set(_mime: &str, _bytes: &[u8]) -> Result<()> {
        bail!("clipboard unsupported on this platform")
    }
}
