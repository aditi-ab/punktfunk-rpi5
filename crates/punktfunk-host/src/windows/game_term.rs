//! Win32 half of [`crate::gamelease`]: the termination ladder (`WM_CLOSE`, then `TerminateProcess`),
//! and whether the game's window is on the input desktop yet ([`visible_window`]).
//!
//! Kept out of `procscan` (read-only) and `gamelease` (platform-neutral).
//!
//! `EnumWindows` sees only the calling thread's desktop. The host is SYSTEM in the interactive
//! session ([`super::service`]) but not on the user's desktop, so without
//! `OpenInputDesktop`/`SetThreadDesktop` the polite pass is empty and every game is killed.
//! A UAC prompt, lock, or Ctrl-Alt-Del swaps that desktop out from under us — same bind
//! `pf-inject`'s `sendinput.rs` uses for `SendInput`.
//!
//! Pin: [`request_close`], [`kill`]. Evidence: [`crate::gamelease`].

use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, EnumDesktopWindows, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, HDESK,
};
use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, GW_OWNER, WM_CLOSE,
};

/// Non-zero so a reader can tell a kill from a clean quit.
const KILLED_EXIT_CODE: u32 = 1;

/// `GENERIC_ALL`. The `windows` crate's desktop-rights type does not export generic rights, so
/// spell it (same constant `pf-inject`'s `sendinput.rs` uses).
const DESKTOP_GENERIC_ALL: u32 = 0x1000_0000;

/// The `pf1-gameterm` thread is dedicated and dies after this; previous desktop is not restored,
/// only the handle is closed.
struct InputDesktop(HDESK);

impl InputDesktop {
    /// `None` if the input desktop cannot be opened or bound (unprivileged, or a secure desktop).
    /// Callers skip the polite pass; the kill pass still works.
    fn attach() -> Option<Self> {
        // SAFETY: FFI by-value args only. `OpenInputDesktop` yields an owned `HDESK` only on `Ok`;
        // it is installed on this thread and owned by the returned guard (closed once in `Drop`),
        // or closed here on failure. `SetThreadDesktop` rebinds only the calling thread, which
        // owns no windows or hooks (fresh termination thread).
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

/// `EnumWindows` `LPARAM`. Owns the pid list: the value round-trips through a raw pointer, and a
/// borrowed lifetime there has no upside for one small allocation per termination.
struct CloseCtx {
    pids: Vec<u32>,
    posted: usize,
}

/// The window X, so the game can save. Zero is ordinary (no window yet, or off the input
/// desktop). The caller waits, then kills; it must not treat zero as success.
pub fn request_close(pids: &[u32]) -> usize {
    if pids.is_empty() {
        return 0;
    }
    // Held for the whole enumeration. Failure skips the polite pass (session 0 has no game windows).
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
    // SAFETY: `EnumWindows` calls `enum_close` synchronously and returns before this frame exits,
    // so the `&mut ctx` in `LPARAM` stays valid and unaliased for the whole call.
    unsafe {
        let _ = EnumWindows(Some(enum_close), LPARAM(&mut ctx as *mut CloseCtx as isize));
    }
    ctx.posted
}

/// Visible top-level windows only: message-only and tool windows swallow `WM_CLOSE` and would
/// inflate the count.
unsafe extern "system" fn enum_close(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lparam` is the `&mut CloseCtx` from `request_close`, valid for the enumeration;
    // the callback runs synchronously on the same thread, so this is the only live reference.
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

/// `EnumDesktopWindows` `LPARAM` for [`visible_window`].
struct FindCtx {
    pids: Vec<u32>,
    title: Option<String>,
}

/// Title of a visible, unowned, non-empty top-level window of one of `pids` on the input desktop —
/// the one the player sees. `None` when there is none yet, or that desktop cannot be read.
///
/// Enumerates the desktop by handle instead of binding this thread to it: the lease watcher asks
/// every second, and a desktop a thread is bound to cannot be closed.
pub fn visible_window(pids: &[u32]) -> Option<String> {
    if pids.is_empty() {
        return None;
    }
    // SAFETY: `OpenInputDesktop` yields an owned `HDESK` only on `Ok`, closed once below. The
    // enumeration calls `enum_find` synchronously, so `&mut ctx` in `LPARAM` stays valid and
    // unaliased for the whole call.
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
        let mut ctx = FindCtx {
            pids: pids.to_vec(),
            title: None,
        };
        let _ = EnumDesktopWindows(
            Some(desk),
            Some(enum_find),
            LPARAM(&mut ctx as *mut FindCtx as isize),
        );
        let _ = CloseDesktop(desk);
        ctx.title
    }
}

/// Stops at the first match. Owned windows (dialogs, splash tool windows) and zero-size ones are
/// not the game's main window.
unsafe extern "system" fn enum_find(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lparam` is the `&mut FindCtx` from `visible_window`, valid for the enumeration;
    // the callback runs synchronously on the same thread, so this is the only live reference.
    let ctx = unsafe { &mut *(lparam.0 as *mut FindCtx) };
    let mut pid = 0u32;
    let mut rect = RECT::default();
    // SAFETY: `hwnd` is the window the enumeration handed us; `pid`, `rect` and `buf` are live
    // locals we own, and `GetWindowTextW` writes at most `buf.len()` units.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let candidate = ctx.pids.contains(&pid)
            && IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_err()
            && GetWindowRect(hwnd, &mut rect).is_ok()
            && rect.right > rect.left
            && rect.bottom > rect.top;
        if !candidate {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        ctx.title = Some(String::from_utf16_lossy(&buf[..len]));
    }
    false.into()
}

/// Whether `pid` runs in this process's session. A pid a plugin reported may name anything;
/// SYSTEM must never terminate a service or another session's process on its word.
pub fn in_our_session(pid: u32) -> bool {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut ours = 0u32;
    let mut theirs = 0u32;
    // SAFETY: both out-params are live locals; the calls read a session id by pid and touch
    // no memory of ours. A failed lookup (pid gone, no access) leaves the default and fails.
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut ours).is_ok()
            && ProcessIdToSessionId(pid, &mut theirs).is_ok()
            && ours == theirs
    }
}

/// Caller must have re-verified start time ([`crate::procscan::Scanner::alive`]) immediately
/// before: Windows recycles pids.
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
