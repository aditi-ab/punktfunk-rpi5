//! Cross-process single-instance guard for pf-vdisplay management.
//!
//! A named mutex in `Global\` makes a second host process fail its vdisplay
//! open loudly instead of firing `IOCTL_CLEAR_ALL` and razing the live host's
//! monitors mid-stream. Claimed eagerly on the serve path; held for the
//! process lifetime (the OS reclaims the name on exit). Failed claims are
//! not memoized.
//!
//! DACL is SYSTEM + Administrators only. Tests pin which owner SIDs count
//! as a sibling host versus a squat.

use super::*;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// Process-global held mutex (`None` until claimed). Not per-manager: the
/// serve path claims it at startup, before any session opens the backend.
/// First-comer wins; a lazy service would otherwise lose the driver to a
/// stray second host. A failed claim is not memoized.
static INSTANCE: Mutex<Option<OwnedHandle>> = Mutex::new(None);

pub(super) fn claim_instance() -> Result<()> {
    let mut g = INSTANCE.lock().unwrap();
    if g.is_none() {
        *g = Some(acquire_single_instance()?);
    }
    Ok(())
}

/// Failure is a warning, not fatal — sessions then fail with the same
/// in-use error until the other instance exits.
pub fn claim_instance_eagerly() {
    if let Err(e) = claim_instance() {
        tracing::warn!("pf-vdisplay single-instance claim failed at startup: {e:#}");
    }
}

/// Hold the named mutex for the process lifetime. The OS reclaims it (and
/// frees the name) on any exit. A second process fails its vdisplay open
/// instead of `IOCTL_CLEAR_ALL` razing the live host's monitors.
fn acquire_single_instance() -> Result<OwnedHandle> {
    const IN_USE: &str = "another punktfunk-host process is already managing pf-vdisplay on this \
         machine — refusing to touch the driver (a second manager's startup CLEAR_ALL would raze \
         the live host's monitors mid-stream). Stop the other instance (e.g. `punktfunk-host \
         service stop`) first.";
    // `Global\` is creatable by any SeCreateGlobalPrivilege holder (includes LocalService).
    // Default DACL from the creating token lets a squatter deny SYSTEM and look like
    // "another instance". Explicit DACL so lesser principals cannot open ours; check
    // OWNER of an existing name so a squat is reported as a squat.
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
            // ACCESS_DENIED has three causes the handle cannot tell apart: live SCM
            // instance whose DACL denies this token OPEN; a squat with the same shape;
            // or no SeCreateGlobalPrivilege (ordinary interactive user). Name all three.
            Err(e) if e.code().0 == 0x8007_0005u32 as i32 => anyhow::bail!(
                "{IN_USE}\n\nIf no other punktfunk-host is running, either this process cannot \
                 create a `Global\\` kernel object at all (it needs SeCreateGlobalPrivilege — run \
                 the host ELEVATED or as the installed service account; an ordinary interactive \
                 user does not hold it), or the name `Global\\punktfunk-vdisplay-manager` has been \
                 SQUATTED by another process — any account with that privilege can create it first \
                 and deny us access, which disables virtual-display streaming until that process \
                 exits. Sysinternals `handle.exe -a punktfunk-vdisplay-manager` tells the two \
                 apart: a holder means a squat, NOTHING means the privilege."
            ),
            Err(e) => {
                return Err(e).context("CreateMutexW(punktfunk-vdisplay single-instance guard)");
            }
        };
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = OwnedHandle::from_raw_handle(h.0 as _);
        if already {
            // DACL let us in, which says nothing about who created it. Owner
            // outside SYSTEM/Administrators is a squat, not a sibling host.
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

/// Protected DACL (`D:P`): Full to SYSTEM and BUILTIN\Administrators only.
/// A LocalService plugin runner is neither, so it cannot open our object.
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

/// Owner SID of a kernel object as SDDL. `None` when unreadable (handle
/// lacks READ_CONTROL) — treated as unknown, never as fine.
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

/// SYSTEM, BUILTIN\Administrators, or an NT SERVICE SID (`S-1-5-80-…`).
///
/// Narrow on purpose: this decides sibling host vs squat. A `S-1-5-32-`
/// prefix or any `S-1-5-21-…` domain account reclassifies a non-admin
/// squatter as one of ours. LocalService (`S-1-5-19`) and NetworkService
/// (`S-1-5-20`) are excluded: the plugin runner is forced to LocalService,
/// so a name owned by it is a plugin, not a host.
fn is_privileged_sid(sid: &str) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544") || sid.starts_with("S-1-5-80-")
}

#[cfg(test)]
mod tests {
    use super::is_privileged_sid;

    /// Widening is silent: a squat starts reading as a sibling host.
    #[test]
    fn is_privileged_sid_accepts_system_admins_and_service_sids_only() {
        assert!(is_privileged_sid("S-1-5-18"), "SYSTEM");
        assert!(is_privileged_sid("S-1-5-32-544"), "BUILTIN\\Administrators");
        assert!(
            is_privileged_sid("S-1-5-80-3139157870-2983391045-3678747466-658725712-1809340420"),
            "an NT SERVICE\\… per-service SID"
        );

        assert!(!is_privileged_sid("S-1-5-32-545"), "BUILTIN\\Users");
        assert!(
            !is_privileged_sid("S-1-5-21-1004336348-1177238915-682003330-1001"),
            "a local/domain user account"
        );
        assert!(!is_privileged_sid("S-1-5-19"), "LocalService");
        assert!(!is_privileged_sid("S-1-5-20"), "NetworkService");
        assert!(
            !is_privileged_sid(""),
            "an unreadable owner is never 'fine'"
        );
        // `S-1-5-80` without the trailing dash is a different SID string;
        // `S-1-5-8` (Proxy) must not slip in under a loosened prefix.
        assert!(!is_privileged_sid("S-1-5-8"), "Proxy");
        assert!(!is_privileged_sid("S-1-5-800-1"), "not a service SID");
    }
}
