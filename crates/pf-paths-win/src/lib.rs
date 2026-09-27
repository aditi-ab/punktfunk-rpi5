//! Windows file-owner and DACL checks, asked before a directory or secret is trusted.
//!
//! `%ProgramData%` grants Users add-subdirectory + CREATOR OWNER, so anything created there
//! before the first elevated run belongs to whoever created it. These readers answer who owns
//! a path and whether anyone unprivileged can write to a directory; the callers decide what a
//! plant means for them. Ask BEFORE `pf_paths::create_private_dir`: its first pass re-owns the
//! contents and erases the evidence.

#![cfg(windows)]

use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, std::io::Error>;

fn other(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

/// `path` exists and an unprivileged account owns it. The two rename-aside sites that adopt
/// an Administrators-owned file from a prior install go through this.
pub fn planted_by_non_admin(path: &Path) -> bool {
    path.exists() && is_admin_owned(path) == Some(false)
}

/// Rename a planted file to `<path>.untrusted`, replacing an earlier one. Best-effort by
/// contract: the caller's guarantee is what it does when the file is still there.
pub fn rename_aside(path: &Path) -> Result<PathBuf> {
    let mut aside = path.to_path_buf().into_os_string();
    aside.push(".untrusted");
    let aside = PathBuf::from(aside);
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(path, &aside)?;
    Ok(aside)
}

/// Refuse a staging directory anyone but SYSTEM/Administrators/TrustedInstaller can write.
///
/// Both: the owner is in that set (an owner always retains `WRITE_DAC` and can restore their
/// own ACE), and no allow ACE grants a write-shaped right to anyone else. `CREATOR OWNER` is
/// outside — a non-admin who created the dir under `%ProgramData%` keeps control through it.
///
/// Reads the security descriptor; `icacls` output uses localized account names.
pub fn ensure_admin_only_source(dir: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Owned, PCWSTR};
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, GetAce, IsValidSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    /// Rights that replace what we are about to trust and execute. GENERIC_ALL/WRITE are checked
    /// unmapped in case an ACE stores them without mapping.
    const WRITE_MASK: u32 = 0x0000_0002 // FILE_WRITE_DATA / FILE_ADD_FILE
        | 0x0000_0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
        | 0x0000_0010 // FILE_WRITE_EA
        | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
        | 0x0000_0040 // FILE_DELETE_CHILD
        | 0x0001_0000 // DELETE
        | 0x0004_0000 // WRITE_DAC
        | 0x0008_0000 // WRITE_OWNER
        | 0x1000_0000 // GENERIC_ALL
        | 0x4000_0000; // GENERIC_WRITE

    if !dir.is_dir() {
        return Err(other(format!("{} is not a directory", dir.display())));
    }
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are live locals.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    // SAFETY: `sd` is null or the single LocalAlloc'd descriptor just returned, which `owner` and
    // `dacl` point into; `Owned` frees it once, after their last use.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    rc.ok()
        .map_err(|e| other(format!("GetNamedSecurityInfoW(owner + DACL): {e}")))?;
    let privileged = privileged_sids()?;
    let is_privileged = |sid: PSID| -> bool {
        // SAFETY: every `sid` handed in points into the descriptor returned above (or at an
        // ACE inside it) and is valid for this scope; IsValidSid is itself the probe.
        if sid.is_invalid() || !unsafe { IsValidSid(sid) }.as_bool() {
            return false;
        }
        privileged
            .iter()
            // SAFETY: `sid` passed IsValidSid above; `p` is an owned, length-exact SID copy.
            .any(|p| unsafe { EqualSid(sid, PSID(p.as_ptr().cast_mut().cast())) }.is_ok())
    };

    if !is_privileged(owner) {
        return Err(other(
            "the directory is owned by a non-administrative account, which retains WRITE_DAC \
             and can restore its own access at any time"
                .into(),
        ));
    }
    // A NULL DACL grants everyone everything; an absent one is not "no access".
    if dacl.is_null() {
        return Err(other(
            "the directory has a NULL DACL (everyone has full control)".into(),
        ));
    }
    // SAFETY: `dacl` is a valid ACL inside the descriptor; AceCount bounds the GetAce index.
    let count = unsafe { (*dacl).AceCount };
    for i in 0..count as u32 {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: i < AceCount, and `ace` is a live out-param.
        unsafe { GetAce(dacl, i, &mut ace) }.map_err(|e| other(format!("GetAce: {e}")))?;
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { *(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue; // deny ACEs only ever subtract; audit ACEs grant nothing
        }
        // SAFETY: an allow ACE is an ACCESS_ALLOWED_ACE, whose SidStart begins the trustee SID.
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        if allowed.Mask & WRITE_MASK == 0 {
            continue; // no write-shaped right
        }
        let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut core::ffi::c_void);
        if !is_privileged(sid) {
            return Err(other(format!(
                "a non-administrative trustee has write access (ACE {i}, mask {:#010x}) — \
                 anything staged here can be replaced before it is trusted or executed",
                allowed.Mask
            )));
        }
    }
    Ok(())
}

/// SYSTEM, `BUILTIN\Administrators`, and TrustedInstaller (owns `%ProgramFiles%`).
fn privileged_sids() -> Result<Vec<Vec<u8>>> {
    use windows::core::{Owned, PCWSTR};
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{GetLengthSid, PSID};

    [
        "S-1-5-18",
        "S-1-5-32-544",
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    ]
    .iter()
    .map(|s| {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid = PSID::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; psid is a live out-param.
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid) }
            .map_err(|e| other(format!("ConvertStringSidToSidW({s}): {e}")))?;
        // SAFETY: ConvertStringSidToSidW allocates with LocalAlloc; `Owned` frees it once.
        let _psid = unsafe { Owned::new(HLOCAL(psid.0)) };
        // SAFETY: psid is a valid SID; copy it out so the caller owns plain bytes.
        let len = unsafe { GetLengthSid(psid) } as usize;
        // SAFETY: GetLengthSid just measured exactly `len` readable bytes at `psid`.
        Ok(unsafe { std::slice::from_raw_parts(psid.0 as *const u8, len) }.to_vec())
    })
    .collect()
}

/// `Some(true)` SYSTEM/Administrators/TrustedInstaller, `Some(false)` any other owner, `None`
/// if the owner cannot be read. Call before `create_private_dir` re-owns the file and erases
/// the signal. Distrusts a `host.env` / `web-password` a non-admin planted under `%ProgramData%`.
pub fn is_admin_owned(path: &Path) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Owned, PCWSTR};
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are live locals.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    // SAFETY: `sd` is null or the single LocalAlloc'd descriptor just returned, which `owner`
    // points into; `Owned` frees it once, after the last use of `owner`.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    rc.ok().ok()?;
    let privileged = privileged_sids().ok()?;
    // SAFETY: `owner` points into the descriptor returned above; IsValidSid is the probe.
    if owner.is_invalid() || !unsafe { IsValidSid(owner) }.as_bool() {
        return None;
    }
    let admin = privileged.iter().any(|p| {
        // SAFETY: `owner` passed IsValidSid; `p` is an owned, length-exact SID copy.
        unsafe { EqualSid(owner, PSID(p.as_ptr().cast_mut().cast())) }.is_ok()
    });
    Some(admin)
}

/// SID of the account this process runs as, as plain bytes.
///
/// A per-user install (`PUNKTFUNK_CONFIG_DIR` into a profile) legitimately owns its own
/// secrets; only a *foreign* unprivileged owner is a plant.
fn current_user_sid() -> Option<Vec<u8>> {
    use windows::core::Owned;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle needing no close; `token` is a live
    // out-param.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: the open succeeded, so `token` is a handle this frame alone owns.
    let token = unsafe { Owned::new(token) };
    let mut len = 0u32;
    // Sizing call: fails with ERROR_INSUFFICIENT_BUFFER and sets `len`.
    // SAFETY: the null buffer with len 0 is the documented sizing form.
    unsafe { GetTokenInformation(*token, TokenUser, None, 0, &mut len) }.ok();
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` holds exactly the `len` bytes the sizing call asked for.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: on success `buf` starts with a TOKEN_USER whose Sid points inside it. A
    // `Vec<u8>` is only byte-aligned, so the struct is read unaligned.
    let sid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
        .User
        .Sid;
    if sid.is_invalid() {
        return None;
    }
    // SAFETY: `sid` is valid; GetLengthSid measures exactly the readable bytes, which live
    // until `buf` drops.
    let n = unsafe { GetLengthSid(sid) } as usize;
    // SAFETY: `n` bytes at `sid` are readable and copied out before `buf` drops.
    Some(unsafe { std::slice::from_raw_parts(sid.0 as *const u8, n) }.to_vec())
}

/// True when `path`'s owner is the account this process runs as.
pub fn owned_by_current_user(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Owned, PCWSTR};
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let Some(me) = current_user_sid() else {
        return false;
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are live locals.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    // SAFETY: `sd` is null or the single LocalAlloc'd descriptor just returned, which `owner`
    // points into; `Owned` frees it once, after the last use of `owner`.
    let _sd = unsafe { Owned::new(HLOCAL(sd.0)) };
    rc.is_ok()
        && !owner.is_invalid()
        // SAFETY: `owner` points into the descriptor returned above; IsValidSid is the probe.
        && unsafe { IsValidSid(owner) }.as_bool()
        // SAFETY: both SIDs passed IsValidSid / GetLengthSid; `me` is an owned copy.
        && unsafe { EqualSid(owner, PSID(me.as_ptr().cast_mut().cast())) }.is_ok()
}

/// True when `path` holds a secret an unprivileged account owns, and renames it aside.
///
/// `%ProgramData%` grants Users add-subdirectory + CREATOR OWNER, so a token, key or cert
/// dropped there before the first elevated run belongs to whoever dropped it. Adopting one
/// hands that account the host's own credential.
///
/// Call BEFORE `pf_paths::create_private_dir`: its first pass re-owns the contents
/// (`icacls /setowner /T`) and erases the only evidence. A `true` verdict must make the
/// caller mint a fresh secret through `write_secret_file`, which unlinks and creates new:
/// a planted file the planter still holds open fails that mint instead of receiving it.
/// An unreadable owner counts as planted.
pub fn quarantine_planted_secret(path: &Path) -> bool {
    if !path.exists() || is_admin_owned(path) == Some(true) || owned_by_current_user(path) {
        return false;
    }
    match rename_aside(path) {
        Ok(aside) => tracing::warn!(
            path = %path.display(), aside = %aside.display(),
            "secret was owned by a non-admin account (planted before install) — renamed aside; minting a fresh one"
        ),
        Err(e) => tracing::error!(
            error = %e, path = %path.display(),
            "secret is non-admin-owned and could not be renamed aside — refusing to adopt it"
        ),
    }
    true
}
