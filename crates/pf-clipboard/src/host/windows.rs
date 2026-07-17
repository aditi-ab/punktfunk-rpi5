//! Host-side shared-clipboard backend for Windows (`design/clipboard-and-file-transfer.md` §4, Phase
//! 3). The Win32 clipboard is thread-affine and message-driven, so the whole backend lives on one
//! dedicated **message-loop thread** owning a hidden message-only window:
//!
//! * **host copy → client** — `AddClipboardFormatListener` delivers `WM_CLIPBOARDUPDATE`; we map the
//!   available formats to wire MIMEs, cache them, and emit [`ClipEvent::Selection`]. Our own offers
//!   are suppressed by the owner-check (we never forward what we ourselves put on the clipboard).
//! * **client fetch of the host clipboard** — a `Cmd::Read` reads the requested format's HGLOBAL and
//!   converts it to wire bytes ([`super::winfmt`]).
//! * **client copy → host** — a `Cmd::SetOffer` installs the client's formats via OLE **delayed
//!   rendering**: `SetClipboardData(fmt, NULL)`, so no bytes cross until a host app actually pastes.
//! * **host paste of client content** — the paste triggers `WM_RENDERFORMAT`; the message-loop thread
//!   blocks (bounded) while the coordinator fetches the bytes from the client, then `SetClipboardData`s
//!   them for the pasting app.
//!
//! The async coordinator drives the thread through a [`Cmd`] channel woken by `PostMessage(WM_APP_CMD)`
//! (`PostMessage` is the documented thread-safe way to poke a message loop). Per-window state hangs
//! off `GWLP_USERDATA`, so multiple concurrent sessions each get their own window + state.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;

use ::windows::core::{w, PCWSTR};
use ::windows::Win32::Foundation::{
    GetLastError, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use ::windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use ::windows::Win32::System::Ole::CF_UNICODETEXT;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW,
    TranslateMessage, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WM_DESTROY, WM_RENDERFORMAT, WNDCLASSW,
};

use super::winfmt;
use super::{ClipEvent, PasteResponder, WIRE_HTML, WIRE_PNG, WIRE_RTF, WIRE_TEXT};

/// Custom app message that wakes the pump to drain the [`Cmd`] channel.
const WM_APP_CMD: u32 = WM_APP + 1;
/// Upper bound the message-loop thread waits for the client's bytes during a `WM_RENDERFORMAT` paste.
/// The pasting app is frozen until we answer, so this caps how long a paste can hang; on expiry the
/// format is left unrendered (an empty paste) rather than blocking indefinitely.
const RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// `OpenClipboard` fails while another process transiently holds the clipboard (clipboard managers do
/// this constantly); retry briefly before giving up.
const OPEN_RETRIES: u32 = 20;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// `RegisterClassW` returns this when the (process-global) class already exists — expected on the 2nd+
/// concurrent session, and not an error (we never unregister the class).
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

type ClipTx = tokio::sync::mpsc::UnboundedSender<ClipEvent>;

/// A command from the async coordinator into the message-loop thread. Delivered over a tokio channel
/// and drained on `WM_APP_CMD`.
enum Cmd {
    /// Install the client's wire MIMEs as a delayed-render host selection (empty ⇒ clear).
    SetOffer(Vec<String>),
    /// Drop the selection we own.
    Clear,
    /// Read one wire format of the current host selection for a client fetch.
    Read {
        wire: String,
        resp: tokio::sync::oneshot::Sender<anyhow::Result<Vec<u8>>>,
    },
    /// Tear the window + thread down.
    Shutdown,
}

/// Message-loop-thread-owned state, reached from the `WndProc` via `GWLP_USERDATA`. Only the message
/// thread ever dereferences it, so the `RefCell`s are sound (no cross-thread sharing); the fields the
/// async handle also touches (`current_wire`) are behind their own `Arc<Mutex>`.
struct WinClip {
    /// Backend → coordinator events.
    clip_tx: ClipTx,
    /// The current host selection's wire MIMEs, shared with the [`WindowsClipboard`] handle.
    current_wire: Arc<Mutex<Vec<String>>>,
    /// Coordinator → backend commands, drained on `WM_APP_CMD`.
    cmd_rx: RefCell<tokio::sync::mpsc::UnboundedReceiver<Cmd>>,
    /// Clipboard format ids we currently promise via delayed rendering (for `WM_RENDERFORMAT`).
    offered: RefCell<Vec<u32>>,
    fmt_html: u32,
    fmt_rtf: u32,
    fmt_png: u32,
    /// Our own message window — used for the owner-check and clipboard opens.
    own_hwnd: HWND,
}

impl WinClip {
    /// `WM_CLIPBOARDUPDATE`: a host app copied (or the clipboard was cleared). Suppress our own
    /// delayed-render echoes via the owner-check, else announce the new wire MIMEs.
    fn on_clipboard_update(&self, hwnd: HWND) {
        // SAFETY: GetClipboardOwner has no preconditions and needs no open clipboard.
        let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
        if owner.0 == hwnd.0 {
            // Our own offer's echo (we own the clipboard) — not a host copy.
            return;
        }
        let mimes = self.available_wire_mimes();
        *self.current_wire.lock().unwrap() = mimes.clone();
        let _ = self.clip_tx.send(ClipEvent::Selection { mimes });
    }

    /// The wire MIMEs the current clipboard advertises, in a stable order.
    fn available_wire_mimes(&self) -> Vec<String> {
        // SAFETY: IsClipboardFormatAvailable has no preconditions and needs no open clipboard.
        let avail = |fmt: u32| unsafe { IsClipboardFormatAvailable(fmt) }.is_ok();
        let mut out = Vec::new();
        if avail(CF_UNICODETEXT.0 as u32) {
            out.push(WIRE_TEXT.to_string());
        }
        if avail(self.fmt_html) {
            out.push(WIRE_HTML.to_string());
        }
        if avail(self.fmt_rtf) {
            out.push(WIRE_RTF.to_string());
        }
        if avail(self.fmt_png) {
            out.push(WIRE_PNG.to_string());
        }
        out
    }

    /// `WM_APP_CMD`: run every queued coordinator command on this thread. Drained into a `Vec` first so
    /// the `cmd_rx` borrow is released before any command runs (defensive against re-entry).
    fn drain_commands(&self, hwnd: HWND) {
        let mut cmds = Vec::new();
        {
            let mut rx = self.cmd_rx.borrow_mut();
            while let Ok(c) = rx.try_recv() {
                cmds.push(c);
            }
        }
        for c in cmds {
            match c {
                Cmd::SetOffer(wire) => self.apply_offer(hwnd, &wire),
                Cmd::Clear => self.clear(hwnd),
                Cmd::Read { wire, resp } => {
                    let _ = resp.send(self.read(&wire));
                }
                Cmd::Shutdown => {
                    // Drop our offer first so no WM_RENDERALLFORMATS fires as the window dies (we do
                    // NOT want the client's content to outlive the session on the host clipboard).
                    self.clear(hwnd);
                    // SAFETY: our own live window; triggers WM_DESTROY → PostQuitMessage → pump exit.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return;
                }
            }
        }
    }

    /// Install the client's offer as a delayed-render host selection.
    fn apply_offer(&self, hwnd: HWND, wire: &[String]) {
        let fmts = self.formats_for_offer(wire);
        if fmts.is_empty() {
            self.clear(hwnd);
            return;
        }
        if open_clipboard_retry(hwnd).is_err() {
            tracing::debug!("clipboard: OpenClipboard for set_offer failed");
            return;
        }
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it); EmptyClipboard makes us the owner,
        // then each SetClipboardData(_, None) registers a delayed-render promise for that format.
        unsafe {
            let _ = EmptyClipboard();
            for &f in &fmts {
                let _ = SetClipboardData(f, None);
            }
        }
        *self.offered.borrow_mut() = fmts;
    }

    /// Drop the selection we own (empty the clipboard iff we're still its owner).
    fn clear(&self, hwnd: HWND) {
        let had = {
            let mut o = self.offered.borrow_mut();
            let was = !o.is_empty();
            o.clear();
            was
        };
        if !had {
            return;
        }
        // SAFETY: GetClipboardOwner has no preconditions.
        let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
        if owner.0 != hwnd.0 {
            return; // someone else took the clipboard already
        }
        if open_clipboard_retry(hwnd).is_err() {
            return;
        }
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it); empty it to drop our promises.
        unsafe {
            let _ = EmptyClipboard();
        }
    }

    /// Read one wire format of the current host selection (a client fetch).
    fn read(&self, wire: &str) -> anyhow::Result<Vec<u8>> {
        let fmt = self
            .format_for_wire(wire)
            .context("unsupported wire MIME")?;
        // If we own the clipboard, its content is our own delayed-render offer (the client's copy),
        // not a host selection — declining avoids GetClipboardData re-entering our own WM_RENDERFORMAT.
        // SAFETY: GetClipboardOwner has no preconditions.
        if unsafe { GetClipboardOwner() }.unwrap_or_default().0 == self.own_hwnd.0 {
            anyhow::bail!("clipboard currently held by our own offer");
        }
        open_clipboard_retry(self.own_hwnd)?;
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it). GetClipboardData hands back a
        // clipboard-owned HGLOBAL (we must NOT free it); GlobalLock/Size/Unlock are balanced and we
        // copy exactly GlobalSize bytes out before the lock is released.
        let raw = unsafe {
            let handle = GetClipboardData(fmt).context("GetClipboardData")?;
            let hg = HGLOBAL(handle.0);
            let p = GlobalLock(hg);
            if p.is_null() {
                anyhow::bail!("GlobalLock failed");
            }
            let n = GlobalSize(hg);
            let mut buf = vec![0u8; n];
            std::ptr::copy_nonoverlapping(p as *const u8, buf.as_mut_ptr(), n);
            let _ = GlobalUnlock(hg);
            buf
        };
        Ok(convert_from_win(wire, &raw))
    }

    /// `WM_RENDERFORMAT`: a host app is pasting a format we promised. Fetch the bytes from the client
    /// (blocking this thread, bounded) and `SetClipboardData` them for the paster.
    fn on_render_format(&self, fmt: u32) {
        let Some(wire) = self.wire_for_format(fmt) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let ev = ClipEvent::Paste {
            mime: wire.to_string(),
            responder: PasteResponder::Sync(tx),
        };
        if self.clip_tx.send(ev).is_err() {
            return; // coordinator gone
        }
        let bytes = match rx.recv_timeout(RENDER_TIMEOUT) {
            Ok(b) => b,
            Err(_) => return, // timeout / dropped → leave the format unrendered (empty paste)
        };
        let win_bytes = convert_to_win(wire, &bytes);
        let Ok(hg) = alloc_hglobal(&win_bytes) else {
            return;
        };
        // Do NOT OpenClipboard here — the pasting app already holds it open across WM_RENDERFORMAT.
        // SAFETY: `hg` is a freshly-filled moveable HGLOBAL. On success the clipboard takes ownership
        // (we must not free it); on failure ownership stays with us, so we free it.
        unsafe {
            if SetClipboardData(fmt, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
            }
        }
    }

    /// The Win32 clipboard format id for a wire MIME (`None` = unsupported).
    fn format_for_wire(&self, wire: &str) -> Option<u32> {
        match wire {
            WIRE_TEXT => Some(CF_UNICODETEXT.0 as u32),
            WIRE_HTML => Some(self.fmt_html),
            WIRE_RTF => Some(self.fmt_rtf),
            WIRE_PNG => Some(self.fmt_png),
            _ => None,
        }
    }

    /// The wire MIME for a Win32 clipboard format id (`None` = one we don't offer).
    fn wire_for_format(&self, fmt: u32) -> Option<&'static str> {
        if fmt == CF_UNICODETEXT.0 as u32 {
            Some(WIRE_TEXT)
        } else if fmt == self.fmt_html {
            Some(WIRE_HTML)
        } else if fmt == self.fmt_rtf {
            Some(WIRE_RTF)
        } else if fmt == self.fmt_png {
            Some(WIRE_PNG)
        } else {
            None
        }
    }

    /// The clipboard format ids to promise for a client offer (dedup, 1:1 with the wire MIMEs — the OS
    /// auto-synthesizes CF_TEXT/CF_OEMTEXT from CF_UNICODETEXT, so no manual text fan-out is needed).
    fn formats_for_offer(&self, wire: &[String]) -> Vec<u32> {
        let mut out = Vec::new();
        for w in wire {
            if let Some(f) = self.format_for_wire(w) {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        out
    }
}

/// The active Windows clipboard backend handle held by [`super::HostClipboard`]. All Win32 work runs
/// on the message-loop thread; this is just the async-side control surface.
pub struct WindowsClipboard {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    /// The message window's `HWND` as an `isize` (so the handle stays `Send`/`Sync`); rebuilt for the
    /// `PostMessage` wakeups, which are documented thread-safe.
    hwnd: isize,
    current_wire: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WindowsClipboard {
    /// Spin up the message-loop thread + hidden window and return once it has bound (or failed).
    pub async fn open() -> anyhow::Result<(
        WindowsClipboard,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<ClipEvent>();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let current_wire = Arc::new(Mutex::new(Vec::new()));

        // Register the three custom formats up front — process-global and thread-agnostic, so this is
        // fine off the message thread and lets bring-up fail cleanly if the atoms can't be created.
        let fmt_html = register_format(w!("HTML Format"))?;
        let fmt_rtf = register_format(w!("Rich Text Format"))?;
        let fmt_png = register_format(w!("PNG"))?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<isize>>();
        let cw = Arc::clone(&current_wire);
        let join = std::thread::Builder::new()
            .name("punktfunk-clipboard-win".into())
            .spawn(move || pump_thread(clip_tx, cmd_rx, cw, fmt_html, fmt_rtf, fmt_png, ready_tx))
            .context("spawn windows clipboard thread")?;

        let hwnd = match tokio::time::timeout(Duration::from_secs(3), ready_rx).await {
            Ok(Ok(Ok(h))) => h,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => anyhow::bail!("windows clipboard thread exited during bring-up"),
            Err(_) => anyhow::bail!("windows clipboard bring-up timed out"),
        };

        Ok((
            WindowsClipboard {
                cmd_tx,
                hwnd,
                current_wire,
                join: Some(join),
            },
            clip_rx,
        ))
    }

    /// The current host selection's wire MIMEs (empty = nothing to offer).
    pub fn current_wire_mimes(&self) -> Vec<String> {
        self.current_wire.lock().unwrap().clone()
    }

    /// Install a client's offered formats as the host selection (fire-and-forget onto the thread).
    pub fn set_offer(&self, wire_mimes: &[String]) {
        let _ = self.cmd_tx.send(Cmd::SetOffer(wire_mimes.to_vec()));
        self.wake();
    }

    /// Drop the host selection we own (fire-and-forget onto the thread).
    pub fn clear_offer(&self) {
        let _ = self.cmd_tx.send(Cmd::Clear);
        self.wake();
    }

    /// Read one wire format of the current host selection (a client's fetch).
    pub async fn read_current(&self, wire_mime: &str) -> anyhow::Result<Vec<u8>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(Cmd::Read {
                wire: wire_mime.to_string(),
                resp: tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard thread gone"))?;
        self.wake();
        rx.await
            .map_err(|_| anyhow::anyhow!("clipboard read dropped"))?
    }

    /// Poke the message loop so it drains the command channel.
    fn wake(&self) {
        // SAFETY: PostMessageW is documented thread-safe; `hwnd` is our message window (or already
        // destroyed, in which case the post harmlessly fails and is ignored).
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut core::ffi::c_void)),
                WM_APP_CMD,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        self.wake();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// RAII `CloseClipboard` guard — pairs with a successful `open_clipboard_retry`, closing on scope exit
/// (including early `?`/`bail!` returns).
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful OpenClipboard on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Register (or resolve the existing id of) a custom clipboard format.
fn register_format(name: PCWSTR) -> anyhow::Result<u32> {
    // SAFETY: RegisterClipboardFormatW is thread-agnostic and process-global; `name` is a static
    // NUL-terminated wide literal.
    let id = unsafe { RegisterClipboardFormatW(name) };
    if id == 0 {
        anyhow::bail!("RegisterClipboardFormatW failed");
    }
    Ok(id)
}

/// Allocate a moveable HGLOBAL holding `bytes` (zero-init so an empty payload is still a valid, locked
/// buffer). Ownership is transferred to the clipboard by a following `SetClipboardData`.
fn alloc_hglobal(bytes: &[u8]) -> anyhow::Result<HGLOBAL> {
    // SAFETY: allocate at least one byte (GlobalLock of a 0-size block is unreliable), lock it, copy
    // the payload in, unlock. Alloc/lock/unlock are balanced; on lock failure we free before erroring.
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))
            .context("GlobalAlloc")?;
        let p = GlobalLock(hg);
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            anyhow::bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        let _ = GlobalUnlock(hg);
        Ok(hg)
    }
}

/// `OpenClipboard(hwnd)` with a brief retry loop (another process often holds it transiently).
fn open_clipboard_retry(hwnd: HWND) -> anyhow::Result<()> {
    for _ in 0..OPEN_RETRIES {
        // SAFETY: OpenClipboard with our window as owner; balanced by ClipboardGuard/CloseClipboard.
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Ok(());
        }
        std::thread::sleep(OPEN_RETRY_DELAY);
    }
    anyhow::bail!("OpenClipboard failed after retries")
}

/// Convert a Win32 clipboard payload to wire bytes.
fn convert_from_win(wire: &str, raw: &[u8]) -> Vec<u8> {
    match wire {
        WIRE_TEXT => winfmt::text_from_utf16(raw),
        WIRE_HTML => winfmt::html_from_cf(raw),
        WIRE_RTF => winfmt::rtf_from_cf(raw),
        _ => raw.to_vec(), // PNG + anything else: verbatim
    }
}

/// Convert wire bytes to a Win32 clipboard payload.
fn convert_to_win(wire: &str, wire_bytes: &[u8]) -> Vec<u8> {
    match wire {
        WIRE_TEXT => winfmt::text_to_utf16(wire_bytes),
        WIRE_HTML => winfmt::html_to_cf(wire_bytes),
        _ => wire_bytes.to_vec(), // RTF + PNG + anything else: verbatim
    }
}

/// Create the hidden message-only window (registering the class once, process-wide).
fn create_window() -> anyhow::Result<HWND> {
    // SAFETY: standard window-class registration + message-only window creation; every argument is a
    // valid handle / static literal, and `wndproc` matches the WNDPROC ABI.
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(PCWSTR::null())
            .context("GetModuleHandleW")?
            .into();
        let class_name = w!("PunktfunkClipboardWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let code = GetLastError();
            if code.0 != ERROR_CLASS_ALREADY_EXISTS {
                anyhow::bail!("RegisterClassW failed: {code:?}");
            }
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
        .context("CreateWindowExW")?;
        Ok(hwnd)
    }
}

/// The message-loop thread body: build the window, wire up state, then pump until `WM_QUIT`.
fn pump_thread(
    clip_tx: ClipTx,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    current_wire: Arc<Mutex<Vec<String>>>,
    fmt_html: u32,
    fmt_rtf: u32,
    fmt_png: u32,
    ready_tx: tokio::sync::oneshot::Sender<anyhow::Result<isize>>,
) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // A clone that outlives the boxed state, so we can announce Closed after the pump ends.
    let closed_tx = clip_tx.clone();

    let state = Box::new(WinClip {
        clip_tx,
        current_wire,
        cmd_rx: RefCell::new(cmd_rx),
        offered: RefCell::new(Vec::new()),
        fmt_html,
        fmt_rtf,
        fmt_png,
        own_hwnd: hwnd,
    });
    let ptr = Box::into_raw(state);
    // SAFETY: stash the state pointer for the WndProc; the window was created on this thread and the
    // pointer stays valid until we reclaim the Box after the pump exits.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
    }

    // Snapshot whatever is already on the host clipboard, so the first client `enable` announces it
    // (AddClipboardFormatListener only delivers *subsequent* changes).
    {
        // SAFETY: `ptr` is the live state we just stored; only this thread dereferences it.
        let st = unsafe { &*ptr };
        *st.current_wire.lock().unwrap() = st.available_wire_mimes();
    }

    // SAFETY: `hwnd` is our live window; start receiving WM_CLIPBOARDUPDATE.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        // SAFETY: tear down the half-built window and reclaim the leaked state box.
        unsafe {
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(ptr));
        }
        let _ = ready_tx.send(Err(
            anyhow::Error::new(e).context("AddClipboardFormatListener")
        ));
        return;
    }

    let _ = ready_tx.send(Ok(hwnd.0 as isize));

    // SAFETY: the standard Win32 message pump. GetMessageW returns >0 for a message, 0 for WM_QUIT,
    // and -1 on error — `.0 > 0` exits on both 0 and -1.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Pump exited (window destroyed): reclaim the leaked state box. No WndProc runs after this point.
    // SAFETY: `ptr` came from Box::into_raw above, is dereferenced only on this thread, and the
    // message loop has ended so no further access occurs.
    unsafe {
        drop(Box::from_raw(ptr));
    }
    let _ = closed_tx.send(ClipEvent::Closed);
}

/// The window procedure. Reaches per-window state through `GWLP_USERDATA`; runs only on the message
/// thread. Registered as the class `WNDPROC` (a safe fn coerces to the `unsafe extern "system"` ABI).
extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the `*const WinClip` stored right after window creation (0/null for
    // the WM_(NC)CREATE messages that fire before that — handled by the null check below).
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WinClip;
    if ptr.is_null() {
        // SAFETY: default processing before our state pointer is attached.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `ptr` is the live Box<WinClip> leaked in pump_thread, owned by this (the only) message
    // thread and freed only after the pump exits; the WndProc is not re-entered for this window, so
    // `&*ptr` is a valid shared borrow.
    let st = unsafe { &*ptr };
    match msg {
        WM_CLIPBOARDUPDATE => {
            st.on_clipboard_update(hwnd);
            LRESULT(0)
        }
        WM_RENDERFORMAT => {
            st.on_render_format(wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_CMD => {
            st.drain_commands(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends the GetMessageW pump by posting WM_QUIT to this thread's queue.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: default handling for every other message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
