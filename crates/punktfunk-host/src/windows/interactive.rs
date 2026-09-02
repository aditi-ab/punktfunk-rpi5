//! Launch a process into the interactive user session from the SYSTEM host.
//!
//! The host is a LocalSystem SCM service. A store launcher needs the logged-in
//! user's token so `HKCU\Software\Classes` handlers, UWP/appx activation, and
//! store auth resolve against that user — not SYSTEM, not session 0.
//!
//! Used by [`crate::library::launch_title`]. Token is `WTSQueryUserToken` of
//! the console session, then `CreateProcessAsUserW` on `winsta0\default`.
//! Do not reuse the session-retargeted SYSTEM token in [`crate::service`];
//! that token is for launching our own streamer. The host stays SYSTEM.

use anyhow::{bail, Context, Result};
use std::path::Path;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_BREAKAWAY_FROM_JOB, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTUPINFOW,
};

/// `Some((own_session, console_session))` when this process is not in the
/// active console session. Display writes then fail `ERROR_ACCESS_DENIED`,
/// GDI describes the wrong session, and `SendInput` goes nowhere.
///
/// `None` when already on the console, or when the query cannot answer
/// (boot, session transition, or the session call failed): unknown stays
/// quiet; this only names the known-bad state.
pub fn console_session_mismatch() -> Option<(u32, u32)> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut own: u32 = 0;
    // SAFETY: `own` is a live local out-param for this synchronous call; no pointer escapes it.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut own) }.is_err() {
        return None;
    }
    // SAFETY: takes no arguments and returns the console session id by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    (console != 0xFFFF_FFFF && own != console).then_some((own, console))
}

/// Spawn `cmdline` in the console session under the logged-in user's token
/// on `winsta0\default`. Returns the new process id.
///
/// Fire-and-forget: child handles close before return; the process keeps
/// running. Environment is the user's block plus this process's
/// `PUNKTFUNK_*` / `RUST_LOG` (see [`merged_env_block`]).
///
/// Needs SYSTEM (`WTSQueryUserToken` requires `SE_TCB`). Fails when no
/// interactive user is logged on.
pub fn spawn_in_active_session(cmdline: &str, workdir: Option<&Path>) -> Result<u32> {
    // SAFETY: takes no arguments and returns the console session id by value.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == 0xFFFF_FFFF {
        bail!("no active console session (no interactive user is logged on)");
    }
    let mut user_token = HANDLE::default();
    // SAFETY: `session` is a plain id and `user_token` a live local out-param; on `Ok` the call
    // yields an owned token handle, closed exactly once below.
    unsafe { WTSQueryUserToken(session, &mut user_token) }
        .context("WTSQueryUserToken (host must be SYSTEM; needs a logged-on interactive user)")?;

    let mut primary = HANDLE::default();
    // SAFETY: `user_token` is the live token just opened; `primary` is a live local out-param that
    // receives a second owned handle on `Ok`. Both are closed exactly once, below.
    let dup = unsafe {
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    // SAFETY: `user_token` is live and owned here, and is not used again after this close.
    let _ = unsafe { CloseHandle(user_token) };
    dup.context("DuplicateTokenEx(TokenPrimary)")?;

    let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `env_block` is a live local out-param and `primary` the live token above; on success
    // the call stores an owned block pointer, destroyed exactly once below.
    let _ = unsafe { CreateEnvironmentBlock(&mut env_block, Some(primary), false) };
    // SAFETY: `env_block` is either still null (the call above failed) or the double-null-terminated
    // UTF-16 block `CreateEnvironmentBlock` just wrote — exactly the two states the helper accepts.
    let merged_env = unsafe { merged_env_block(env_block as *const u16) };
    if !env_block.is_null() {
        // SAFETY: `env_block` is the live block from the call above, destroyed exactly once and not
        // read after — `merged_env` owns its own copy of the parsed entries.
        let _ = unsafe { DestroyEnvironmentBlock(env_block) };
    }

    // Captured interactive desktop, not the caller's (session 0 has no UI).
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };

    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let workdir_w: Option<Vec<u16>> = workdir.map(|d| {
        d.as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    });
    let cwd = match &workdir_w {
        Some(w) => PCWSTR(w.as_ptr()),
        None => PCWSTR::null(),
    };

    let mut pi = PROCESS_INFORMATION::default();
    // The caller in `crate::service` sits in a kill-on-close job with
    // BREAKAWAY_OK. Without this flag the child joins that job and dies when
    // the service stops. Retry without the flag if the job forbids breakaway
    // (ACCESS_DENIED); launching inside that job beats not launching.
    let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_BREAKAWAY_FROM_JOB;
    let created = loop {
        // SAFETY: `primary` is the live primary token; `cmd`, `desktop` (via `si.lpDesktop`),
        // `workdir_w` (via `cwd`) and `merged_env` are locals that outlive the call, each
        // NUL-terminated as the API requires — `merged_env` doubly so, per `merged_env_block`.
        // `pi` is a live local out-param, and the API retains none of these pointers.
        let r = unsafe {
            CreateProcessAsUserW(
                Some(primary),
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false, // no inherit: fire-and-forget; no stdio relay
                flags,
                Some(merged_env.as_ptr() as *const core::ffi::c_void),
                cwd,
                &si,
                &mut pi,
            )
        };
        if r.is_ok() || !flags.contains(CREATE_BREAKAWAY_FROM_JOB) {
            break r;
        }
        tracing::debug!("breakaway launch refused ({r:?}) — retrying inside the job");
        flags &= !CREATE_BREAKAWAY_FROM_JOB;
    };
    // SAFETY: `primary` is live and owned here, closed exactly once and not used after.
    let _ = unsafe { CloseHandle(primary) };
    created.context("CreateProcessAsUserW (interactive-session launch)")?;

    let pid = pi.dwProcessId;
    // SAFETY: `created` was `Ok`, so `pi` holds two owned handles; each is closed exactly once here
    // and never used after. Closing them does not terminate the child, which owns its own lifetime.
    unsafe {
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
    }
    Ok(pid)
}

/// UTF-16, double-null-terminated block for `CREATE_UNICODE_ENVIRONMENT`:
/// the target session's `user_block` (`CreateEnvironmentBlock`) with this
/// process's `PUNKTFUNK_*` and `RUST_LOG` overlaid, so the child inherits
/// host settings rather than the target shell's. Shared with
/// [`crate::service`].
///
/// # Safety
/// `user_block` must be null or a valid pointer to a UTF-16,
/// double-null-terminated environment block, readable for its whole length.
pub(crate) unsafe fn merged_env_block(user_block: *const u16) -> Vec<u16> {
    let mut entries: Vec<String> = Vec::new();
    if !user_block.is_null() {
        let mut p = user_block;
        loop {
            let mut len = 0isize;
            // SAFETY: per this fn's contract `p` is in a readable double-null-terminated
            // block. `len` only advances over non-NUL units already read, so
            // `p.offset(len)` stays in the current entry. The empty entry stops the
            // outer loop before `p` can pass the block's end.
            while unsafe { *p.offset(len) } != 0 {
                len += 1;
            }
            if len == 0 {
                break; // empty entry: end of block
            }
            // SAFETY: `p` is readable for `len` non-NUL UTF-16 units, just scanned above, and the
            // slice is consumed before `p` moves.
            let slice = unsafe { std::slice::from_raw_parts(p, len as usize) };
            entries.push(String::from_utf16_lossy(slice));
            // SAFETY: `len` is the entry length and unit `len` is its NUL, so this lands on the next
            // entry — at worst the trailing empty one, which is still inside the block.
            p = unsafe { p.offset(len + 1) };
        }
    }
    let is_ours = |k: &str| k.starts_with("PUNKTFUNK_") || k == "RUST_LOG";
    entries.retain(|e| !is_ours(e.split('=').next().unwrap_or("")));
    for (k, v) in std::env::vars().filter(|(k, _)| is_ours(k)) {
        entries.push(format!("{k}={v}"));
    }
    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend(e.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}
