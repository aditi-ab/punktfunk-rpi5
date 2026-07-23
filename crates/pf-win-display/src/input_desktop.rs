//! Make display-config writes follow the INPUT desktop.
//!
//! Windows refuses `ChangeDisplaySettingsEx` / `SetDisplayConfig` issued from a thread that is not
//! on the desktop currently receiving input. While a UAC consent prompt (or the lock / logon
//! screen) is up, the input desktop is `Winlogon`; a host thread sitting on `WinSta0\Default` — the
//! desktop the service explicitly launches it onto — then gets `DISP_CHANGE_FAILED` /
//! `ERROR_ACCESS_DENIED` for EVERY write. A session starting in that window can never set its
//! virtual display's mode, so the capturer sizes its ring to a stale mode, every composed frame is
//! dropped for a size mismatch, and the client sits on a black screen until bring-up runs out of
//! retries. Field-reported 2026-07-23: a consent prompt left up on an unattended host made the box
//! unreachable until someone walked over to it.
//!
//! Measured on-glass that same day (RTX 4090 box, SYSTEM host in console session 2, a real
//! interactive consent prompt up, virtual display `\\.\DISPLAY42` active):
//!
//! ```text
//! INPUT desktop = Winlogon
//! UNBOUND  CDS_TEST      -> -1 (DISP_CHANGE_FAILED)      <- the field failure, reproduced
//! UNBOUND  SDC_VALIDATE  -> 0x5 (ERROR_ACCESS_DENIED)    <- the field failure, reproduced
//! bound thread desktop -> Winlogon
//! BOUND    CDS_TEST      ->  0 (DISP_CHANGE_SUCCESSFUL)
//! BOUND    SDC_VALIDATE  -> 0x0 (ERROR_SUCCESS)
//! ```
//!
//! The retry model mirrors `pf-inject`'s `sendinput.rs`: do NOT pay
//! `OpenInputDesktop`/`SetThreadDesktop` on every write — issue the write, and only when it fails
//! the way a wrong-desktop write fails do we rebind and retry once. That keeps the normal path
//! byte-for-byte as it was (a working write is never touched) and makes this strictly additive.
//!
//! Unlike sendinput's injector — a dedicated thread that KEEPS its binding — these helpers run on
//! shared/task threads, so the binding here is SCOPED: [`InputDesktopBinding`] restores the
//! thread's original desktop on drop. A thread left bound to a `Winlogon` desktop that is later
//! destroyed (prompt dismissed) would fail every subsequent display write for the life of the
//! process — the exact wedge this module exists to remove, so it must not be introduced here.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// `GENERIC_ALL` for the desktop open. The `windows` crate models desktop access as its own flag
/// type and doesn't export the generic rights, so spell it out (same constant `sendinput.rs` and
/// the cursor poller use).
const GENERIC_ALL: u32 = 0x1000_0000;

/// This thread's binding to the input desktop, restored on drop.
///
/// `GetThreadDesktop` returns a BORROWED handle (documented: it creates no new handle and must not
/// be closed), so only the handle `OpenInputDesktop` produced is closed here — and only after the
/// thread has been moved back off it.
pub(crate) struct InputDesktopBinding {
    previous: HDESK,
    input: HDESK,
    /// `UOI_NAME` of the desktop bound to, for the "this write only landed because we rebound"
    /// log. Read once at bind time (the failure path only), never in steady state.
    name: String,
}

impl InputDesktopBinding {
    /// Bind the calling thread to the desktop currently receiving input. `None` when there is no
    /// reachable input desktop (not privileged for `Winlogon` — a host that is not SYSTEM) or the
    /// rebind is refused, in which case the caller keeps whatever result it already had rather than
    /// changing behaviour.
    pub(crate) fn bind() -> Option<Self> {
        // SAFETY: all four are FFI calls taking by-value args only. `GetThreadDesktop` yields a
        // borrowed handle for THIS thread (never closed here). `OpenInputDesktop` yields an owned
        // `HDESK` only on `Ok`; it is either installed by `SetThreadDesktop` (and then owned by the
        // returned guard, which closes it exactly once in `Drop`) or closed right here on failure —
        // so it is closed exactly once on every path and never used after close. `SetThreadDesktop`
        // rebinds only the calling thread, which owns no windows or hooks (these are display-config
        // worker threads), so it cannot fail on that account.
        unsafe {
            let previous = GetThreadDesktop(GetCurrentThreadId()).ok()?;
            let input = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            )
            .ok()?;
            let name = desktop_name(input).unwrap_or_else(|| "<unnamed>".into());
            if SetThreadDesktop(input).is_err() {
                let _ = CloseDesktop(input);
                return None;
            }
            Some(Self {
                previous,
                input,
                name,
            })
        }
    }
}

impl Drop for InputDesktopBinding {
    fn drop(&mut self) {
        // SAFETY: `self.previous` is the borrowed desktop this thread started on and `self.input`
        // is the handle this guard uniquely owns. The thread is moved back FIRST, so the handle is
        // no longer the thread's desktop when it is closed — closed exactly once, never used after.
        unsafe {
            let _ = SetThreadDesktop(self.previous);
            let _ = CloseDesktop(self.input);
        }
    }
}

/// `UOI_NAME` of the current input desktop — `Some("Winlogon")` while a UAC consent prompt, the
/// lock screen or the logon screen owns input, `Some("Default")` in normal operation, `None` when
/// it cannot be opened at all.
pub(crate) fn input_desktop_name() -> Option<String> {
    // SAFETY: `OpenInputDesktop` yields an owned handle only on `Ok`, which is closed exactly once
    // below and not used after.
    unsafe {
        let h = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
        )
        .ok()?;
        let name = desktop_name(h);
        let _ = CloseDesktop(h);
        name
    }
}

/// `UOI_NAME` of an already-open desktop handle. Borrows `h` — closing it stays the caller's job.
///
/// # Safety
/// `h` must be a live desktop handle for the duration of the call.
unsafe fn desktop_name(h: HDESK) -> Option<String> {
    let mut name = [0u16; 64]; // "Default"/"Winlogon"/"Screen-saver" all fit with room to spare
    let mut needed = 0u32;
    // SAFETY: `h` is live per this fn's contract; `name`/`needed` are live out-params and the call
    // writes at most `nlength` bytes, exactly the size passed.
    GetUserObjectInformationW(
        HANDLE(h.0),
        UOI_NAME,
        Some(name.as_mut_ptr().cast()),
        (name.len() * 2) as u32,
        Some(&mut needed),
    )
    .ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

/// `true` when the input desktop is a SECURE one (`UOI_NAME` != `Default`: `Winlogon` for UAC
/// consent / lock / logon, or a screen-saver desktop) — i.e. when display writes from the host's
/// own desktop are being refused for that reason. `false` when it is normal OR unreadable: this
/// only ever phrases a diagnostic, so an unknown desktop must not claim the secure one is up.
pub(crate) fn input_desktop_is_secure() -> bool {
    input_desktop_name().is_some_and(|n| !n.eq_ignore_ascii_case("Default"))
}

/// `ERROR_ACCESS_DENIED` — exactly how Windows refuses a `SetDisplayConfig` issued from a thread
/// that is not on the input desktop (measured, see the module header). Narrower than "any error" on
/// purpose: the CCD path also returns `ERROR_INVALID_PARAMETER` (0x57) for the unrelated
/// exclusive-mode topology bug, and re-issuing THAT on another desktop would only confuse its
/// diagnosis.
const SDC_ACCESS_DENIED: i32 = 5;

/// [`retry_on_input_desktop`] specialised for the CCD writes, which all return a Win32 error code.
pub(crate) fn retry_set_display_config(write: impl FnMut() -> i32) -> i32 {
    retry_on_input_desktop(|rc| *rc == SDC_ACCESS_DENIED, write)
}

/// Run a display-config write; if it comes back the way a write from the wrong desktop comes back,
/// bind this thread to the CURRENT input desktop and run it exactly once more.
///
/// `denied` decides which results are worth a retry — keep it NARROW (the specific
/// wrong-desktop verdict), so a genuinely bad mode or a driver-level refusal is not re-issued and
/// mis-attributed. The binding is dropped before returning, so the calling thread always leaves on
/// the desktop it arrived on.
pub(crate) fn retry_on_input_desktop<T>(
    denied: impl Fn(&T) -> bool,
    mut write: impl FnMut() -> T,
) -> T {
    let first = write();
    if !denied(&first) {
        return first;
    }
    // Only worth the rebind when the input desktop is genuinely elsewhere; on a normal desktop the
    // refusal means something else entirely and a retry would just repeat it.
    let Some(binding) = InputDesktopBinding::bind() else {
        return first;
    };
    let second = write();
    if !denied(&second) {
        // Say so. A silent save is indistinguishable in a log from a write that never needed
        // saving, which makes "did the fix fire?" unanswerable in the field — and made the first
        // on-glass verification of this very change inconclusive.
        tracing::info!(
            desktop = %binding.name,
            "display write was refused off the input desktop — retried bound to it and it applied \
             (a UAC prompt / lock screen owns input; the session continues normally)"
        );
    }
    second
}
