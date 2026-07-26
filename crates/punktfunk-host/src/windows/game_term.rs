//! Asking a game to close on Windows, and killing it if it won't.
//!
//! The two halves of [`crate::gamelease`]'s termination ladder that need Win32: post `WM_CLOSE` to the
//! game's windows, then `TerminateProcess` whatever ignored it. Kept out of `procscan` (read-only by
//! construction) and out of `gamelease` (platform-neutral orchestration).
//!
//! ### Why the desktop dance
//!
//! The polite half is the reason this module is not three lines. `EnumWindows` enumerates the windows
//! of the **calling thread's desktop** — and the host runs as SYSTEM in session 0, whose desktop has
//! none of the interactive user's windows on it. Without first binding the thread to the input desktop
//! the enumeration comes back empty and the ladder silently degrades to killing every game outright,
//! which is exactly the unsaved-progress outcome the grace window exists to avoid.
//!
//! Same `OpenInputDesktop`/`SetThreadDesktop` pattern the input injector uses for `SendInput`
//! (`pf-inject`'s `sendinput.rs`), for the same reason: a UAC prompt, the lock screen or
//! Ctrl-Alt-Del swaps the input desktop out from under us.

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    HDESK,
};
use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};

/// Exit code reported for a game the host killed. Arbitrary but non-zero, so anything reading it can
/// tell it apart from a clean quit.
const KILLED_EXIT_CODE: u32 = 1;

/// `GENERIC_ALL` for a desktop open. The `windows` crate models desktop rights as their own type, so
/// the generic right isn't reachable through it — the same local const the input backends define
/// (`pf-inject`'s `sendinput.rs` / `pointer_windows.rs`).
const DESKTOP_GENERIC_ALL: u32 = 0x1000_0000;

/// Binds the calling thread to the desktop currently receiving input, for as long as it is held.
///
/// The thread this runs on is dedicated and short-lived (`pf1-gameterm`), so the previous desktop is
/// not restored — only the handle is released.
struct InputDesktop(HDESK);

impl InputDesktop {
    /// `None` when the input desktop can't be opened or bound (an unprivileged host, or a secure
    /// desktop we aren't allowed onto). Callers degrade rather than fail: without it the polite pass
    /// finds no windows, and the kill pass still works.
    fn attach() -> Option<Self> {
        // SAFETY: FFI calls taking by-value args only (constant desktop flags, a bool, an access
        // mask). `OpenInputDesktop` yields an owned `HDESK` only on `Ok`; it is then either installed
        // on this thread and owned by the returned guard (closed exactly once in `Drop`), or closed
        // here on the failure path. `SetThreadDesktop` rebinds only the calling thread — which owns
        // this guard — and succeeds only when that thread has no windows or hooks, true of the fresh
        // termination thread.
        unsafe {
            let h = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(DESKTOP_GENERIC_ALL),
            )
            .ok()?;
            if SetThreadDesktop(h).is_ok() {
                Some(Self(h))
            } else {
                let _ = CloseDesktop(h);
                None
            }
        }
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle this guard owns and has not closed; `CloseDesktop` runs once
        // here with no later use.
        unsafe {
            let _ = CloseDesktop(self.0);
        }
    }
}

/// What the enumeration callback needs, passed through `LPARAM`.
///
/// Owns its pid list rather than borrowing: the value round-trips through a raw pointer, and a
/// borrowed lifetime there is a subtlety with no upside for one small allocation per termination.
struct CloseCtx {
    pids: Vec<u32>,
    posted: usize,
}

/// Ask every visible top-level window belonging to `pids` to close, the way clicking its X would —
/// so a game runs its own shutdown path and gets the chance to save. Returns how many windows were
/// asked.
///
/// Zero is a perfectly ordinary answer: a game may be mid-load with no window yet, or the host may not
/// have reached the input desktop. The caller's job is to wait and then insist, not to treat this as
/// success.
pub fn request_close(pids: &[u32]) -> usize {
    if pids.is_empty() {
        return 0;
    }
    // Without the input desktop this enumerates session 0 and finds nothing (see the module docs), so
    // failing to attach means skipping the polite pass rather than doing it uselessly. Held for the
    // whole enumeration.
    let Some(_desktop) = InputDesktop::attach() else {
        tracing::debug!(
            "could not bind to the input desktop — skipping the polite close and going straight to \
             the kill pass"
        );
        return 0;
    };
    let mut ctx = CloseCtx {
        pids: pids.to_vec(),
        posted: 0,
    };
    // SAFETY: `EnumWindows` invokes `enum_close` synchronously for each top-level window on this
    // thread's desktop and returns before this frame exits, so the `&mut ctx` pointer it carries in
    // the `LPARAM` stays valid for the whole call and is not aliased (nothing else touches `ctx`
    // while the enumeration runs).
    unsafe {
        let _ = EnumWindows(Some(enum_close), LPARAM(&mut ctx as *mut CloseCtx as isize));
    }
    ctx.posted
}

/// `EnumWindows` callback: post `WM_CLOSE` to each visible window owned by one of the target pids.
///
/// Visible top-level windows only — a game's message-only and tool windows would swallow the close, and
/// posting to them achieves nothing while making the count meaningless.
unsafe extern "system" fn enum_close(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lparam` is the `&mut CloseCtx` `request_close` passed in, valid for the whole
    // enumeration; the callback is invoked synchronously on the same thread, so this is the only live
    // reference to it.
    let ctx = unsafe { &mut *(lparam.0 as *mut CloseCtx) };
    let mut pid = 0u32;
    // SAFETY: `hwnd` is the window the enumeration handed us; `pid` is a live local we own.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if ctx.pids.contains(&pid) && IsWindowVisible(hwnd).as_bool() {
            // Posted, not sent: a `SendMessage` would block this thread on a hung game's message pump.
            if PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
                ctx.posted += 1;
            }
        }
    }
    true.into() // keep enumerating — a game can own several windows
}

/// Kill `pids` outright. Returns how many were terminated.
///
/// The caller must have re-verified each pid against its recorded start time immediately before
/// calling this ([`crate::procscan::Scanner::alive`]) — Windows recycles pids briskly, and this call is
/// unforgiving about being pointed at the wrong one.
pub fn kill(pids: &[u32]) -> usize {
    let mut killed = 0;
    for &pid in pids {
        // SAFETY: `OpenProcess` yields an owned handle only on `Ok`, which is closed exactly once
        // below; `TerminateProcess` takes it by value plus a plain exit code.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                if TerminateProcess(h, KILLED_EXIT_CODE).is_ok() {
                    killed += 1;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    killed
}
