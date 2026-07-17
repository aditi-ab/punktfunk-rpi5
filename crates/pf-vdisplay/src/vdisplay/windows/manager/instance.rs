//! The cross-process single-instance guard for pf-vdisplay management (plan §W3, carved out of the
//! manager). A named mutex makes a SECOND host process fail its vdisplay open loudly instead of firing
//! `IOCTL_CLEAR_ALL` and razing the live host's monitors mid-stream.

use super::*;

/// The held single-instance mutex (`None` until claimed). Process-global — not per-manager — so the
/// serve path can claim it EAGERLY at startup, before any session opens the backend: the claim is
/// first-comer-wins, and a lazily-claiming service could otherwise lose its own machine's driver to
/// a stray second host started while the service sat idle (observed on-glass). A failed claim is NOT
/// memoized: once the other instance exits, the next attempt succeeds.
static INSTANCE: Mutex<Option<OwnedHandle>> = Mutex::new(None);

/// Claim (or re-verify) the cross-process single-instance guard. Idempotent; retries after failure.
pub(super) fn claim_instance() -> Result<()> {
    let mut g = INSTANCE.lock().unwrap();
    if g.is_none() {
        *g = Some(acquire_single_instance()?);
    }
    Ok(())
}

/// Eager startup claim for the serve/service path (Windows): reserves this process as THE
/// pf-vdisplay manager before any client connects. Failure is a loud warning, not fatal — sessions
/// then fail with the same clear in-use error until the other instance exits.
pub fn claim_instance_eagerly() {
    if let Err(e) = claim_instance() {
        tracing::warn!("pf-vdisplay single-instance claim failed at startup: {e:#}");
    }
}

/// The cross-process single-instance guard for pf-vdisplay management. A SECOND host process's
/// first device open used to fire `IOCTL_CLEAR_ALL` and raze the live host's monitors mid-stream —
/// an admin footgun (run `punktfunk-host serve` while the SCM service streams), masked afterwards
/// because both processes' pings satisfy the shared driver watchdog. The named mutex makes the
/// second process fail its vdisplay open LOUDLY instead. Held, never released, for the process
/// lifetime; the OS reclaims it (and frees the name) when the process exits, however it exits.
fn acquire_single_instance() -> Result<OwnedHandle> {
    const IN_USE: &str = "another punktfunk-host process is already managing pf-vdisplay on this \
         machine — refusing to touch the driver (a second manager's startup CLEAR_ALL would raze \
         the live host's monitors mid-stream). Stop the other instance (e.g. `punktfunk-host \
         service stop`) first.";
    // SAFETY: plain FFI create of a named mutex; the returned handle (checked) is solely owned by
    // the `OwnedHandle`, and `GetLastError` is read immediately after the create — the documented
    // ERROR_ALREADY_EXISTS protocol for pre-existing named objects.
    unsafe {
        let h = match CreateMutexW(None, false, w!("Global\\punktfunk-vdisplay-manager")) {
            Ok(h) => h,
            // The name exists but its creator's DACL denies this token the implicit OPEN (the SCM
            // service creates it as SYSTEM; a second elevated-admin host lands here instead of in
            // the ALREADY_EXISTS branch — validated on-glass). Same meaning: an instance is live.
            Err(e) if e.code().0 == 0x8007_0005u32 as i32 => anyhow::bail!("{IN_USE}"),
            Err(e) => {
                return Err(e).context("CreateMutexW(punktfunk-vdisplay single-instance guard)");
            }
        };
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = OwnedHandle::from_raw_handle(h.0 as _);
        if already {
            anyhow::bail!("{IN_USE}");
        }
        Ok(owned)
    }
}
