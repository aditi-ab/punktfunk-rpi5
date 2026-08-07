//! The cross-process single-instance guard for pf-vdisplay management (plan §W3, carved out of the
//! manager). A named mutex makes a SECOND host process fail its vdisplay open loudly instead of firing
//! `IOCTL_CLEAR_ALL` and razing the live host's monitors mid-stream.

use super::*;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

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
    // A name in `Global\` is creatable by ANY principal holding SeCreateGlobalPrivilege — which
    // includes the LocalService account the plugin runner is forced to (plugins.rs). With `None`
    // security attributes this object took the DACL from the creating token's default, and a
    // squatter who got there first (creating the name with a DACL that denies SYSTEM) permanently
    // and silently disabled every virtual-display session: the host lands in the ACCESS_DENIED arm
    // below and reports a perfectly reasonable "another instance is managing the driver", which
    // sends the operator hunting a process that does not exist (2026-08-05 review L-16).
    //
    // Two changes: create with an EXPLICIT DACL so lesser principals cannot open ours, and check
    // the OWNER of a name that already exists so a squat is reported as a squat.
    let sd = security_descriptor()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: plain FFI create of a named mutex; `sa` (and the descriptor it points at) outlives
    // the call, the returned handle (checked) is solely owned by the `OwnedHandle`, and
    // `GetLastError` is read immediately after the create — the documented ERROR_ALREADY_EXISTS
    // protocol for pre-existing named objects.
    unsafe {
        let h = match CreateMutexW(Some(&sa), false, w!("Global\\punktfunk-vdisplay-manager")) {
            Ok(h) => h,
            // The name exists but its creator's DACL denies this token the implicit OPEN (the SCM
            // service creates it as SYSTEM; a second elevated-admin host lands here instead of in
            // the ALREADY_EXISTS branch — validated on-glass). Legitimately that means an instance
            // is live; it is ALSO exactly what a squat looks like, so say both.
            Err(e) if e.code().0 == 0x8007_0005u32 as i32 => anyhow::bail!(
                "{IN_USE}\n\nIf no other punktfunk-host is running, the name \
                 `Global\\punktfunk-vdisplay-manager` has been SQUATTED by another process — any \
                 account with SeCreateGlobalPrivilege can create it first and deny us access, \
                 which disables virtual-display streaming until that process exits. Find the \
                 holder with Sysinternals `handle.exe -a punktfunk-vdisplay-manager`."
            ),
            Err(e) => {
                return Err(e).context("CreateMutexW(punktfunk-vdisplay single-instance guard)");
            }
        };
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = OwnedHandle::from_raw_handle(h.0 as _);
        if already {
            // We opened an existing object — so its DACL let us in, but that says nothing about
            // who created it. If the owner is not SYSTEM/Administrators it is not one of ours.
            if let Some(owner) = object_owner_sid(h) {
                if !is_privileged_sid(&owner) {
                    anyhow::bail!(
                        "the pf-vdisplay single-instance name is held by a NON-ADMINISTRATIVE \
                         process (owner SID {owner}) — this is not another punktfunk-host, it is a \
                         squat on `Global\\punktfunk-vdisplay-manager`, and it blocks all \
                         virtual-display streaming while it is held."
                    );
                }
            }
            anyhow::bail!("{IN_USE}");
        }
        Ok(owned)
    }
}

/// `D:P(A;;GA;;;SY)(A;;GA;;;BA)` — a protected DACL (no inheritance) granting Full to SYSTEM and
/// BUILTIN\Administrators, and to nobody else. Everything that legitimately manages pf-vdisplay is
/// one of those two; a LocalService plugin runner is neither, so it can no longer open our object.
fn security_descriptor() -> Result<LocalSd> {
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::Authorization::SDDL_REVISION_1;
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is NUL-terminated (`w!`), and `psd` is a live out-param whose
    // allocation is taken over by `LocalSd` below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)"),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .context("build the pf-vdisplay single-instance security descriptor")?;
    Ok(LocalSd(psd.0))
}

/// Owns a `LocalAlloc`'d security descriptor and frees it on drop.
struct LocalSd(*mut core::ffi::c_void);

impl Drop for LocalSd {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
            // which documents LocalFree as the matching deallocation.
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0),
                ));
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

/// The owner SID of a kernel object, as an SDDL string. `None` when it cannot be read (the handle
/// lacks READ_CONTROL) — treated as "unknown", never as "fine".
fn object_owner_sid(h: HANDLE) -> Option<String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSID};

    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `h` is the live mutex handle; the out-params are live locals; `sd` is the single
    // allocation and is LocalFree'd below.
    let rc = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    let out = if rc.is_ok() && !owner.is_invalid() {
        let mut sid_str = windows::core::PWSTR::null();
        // SAFETY: `owner` points into `sd` and is a valid SID; `sid_str` is a live out-param whose
        // LocalAlloc'd string is freed immediately below.
        unsafe {
            if ConvertSidToStringSidW(owner, &mut sid_str).is_ok() && !sid_str.is_null() {
                let text = sid_str.to_string().unwrap_or_default();
                let _ = LocalFree(Some(HLOCAL(sid_str.0 as _)));
                Some(text)
            } else {
                None
            }
        }
    } else {
        None
    };
    // SAFETY: `sd` is the LocalAlloc'd descriptor GetSecurityInfo returned (null when it failed,
    // which LocalFree tolerates).
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    out
}

/// SYSTEM, BUILTIN\Administrators, or a member of the Administrators-owned set — the principals a
/// legitimate pf-vdisplay manager runs as.
fn is_privileged_sid(sid: &str) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544") || sid.starts_with("S-1-5-80-") // service SIDs
}
