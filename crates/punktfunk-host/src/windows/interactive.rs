//! Launch a process into the interactive user session from the SYSTEM host.
//!
//! The Windows host runs as a LocalSystem SCM service. To *launch* a game/launcher so it renders onto
//! the captured desktop — and so the user's protocol handlers (`HKCU\Software\Classes`), UWP/appx
//! activation, and each store's auth/entitlement context resolve — the process must run in the
//! interactive session under the **logged-in user's** token, not SYSTEM and not session 0.
//!
//! This is the standard `WTSGetActiveConsoleSessionId → WTSQueryUserToken → DuplicateTokenEx →
//! CreateProcessAsUserW(winsta0\\default)` primitive, used for the library launch path
//! ([`crate::library::launch_title`]).
//!
//! IMPORTANT — use the **user** token (`WTSQueryUserToken`), NOT a session-retargeted SYSTEM token
//! (the host-spawn in [`crate::service`] duplicates the SYSTEM token and only changes its session id;
//! that is correct for launching *our own* streamer, but a store launcher needs the real user's token
//! for activation + auth). The host process itself stays SYSTEM.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]
// …and the proofs only cover the whole file once an `unsafe fn` body needs its own blocks: the
// workspace sets `unsafe_op_in_unsafe_fn` to `warn`, which is a ratchet, not a floor. This module is
// at zero, so hold it there — `merged_env_block`'s pointer walk is the one real contract here, and
// it must not silently re-absorb the FFI calls around it.
#![deny(unsafe_op_in_unsafe_fn)]

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
    CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

/// `Some((own_session, console_session))` when this process is NOT in the active console session —
/// the state in which every display write (`SetDisplayConfig` / `ChangeDisplaySettingsExW`) fails
/// `ERROR_ACCESS_DENIED`, GDI reads describe the wrong session's displays, and `SendInput` compose
/// kicks go nowhere. Field signature: a hand-launched host whose session later went remote (an RDP
/// round-trip) — the installed service relaunches the host when the console session moves, a
/// hand-launched one stays trapped. `None` in the console session, and `None` when the check
/// itself can't answer (no console session attached — boot / session transition — or the session
/// query failed): the guard exists to NAME a known-bad state, so an unknown state stays quiet.
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

/// Spawn `cmdline` in the active console session, under the logged-in user's token, on the
/// interactive desktop (`winsta0\default`). Returns the new process id.
///
/// Fire-and-forget: the launched game/launcher outlives this call, so the host does not track the
/// child — its handles are closed before returning (the process keeps running). The environment is
/// the user's block merged with the host's `PUNKTFUNK_*`/`RUST_LOG` (see [`merged_env_block`]),
/// so `host.env` settings propagate.
///
/// Requires the host to run as SYSTEM (`WTSQueryUserToken` needs `SE_TCB`). Fails when no interactive
/// user is logged on (a pre-login / freshly-booted box can stream the login desktop but cannot
/// auto-launch a store title until someone signs in).
/// Safe: `cmdline`/`workdir` are borrowed Rust data, every FFI argument is built locally from them,
/// and the function owns each handle it opens (closing all of them before it returns) — there is no
/// precondition a caller could violate, so the `unsafe` marks only the Win32 calls below.
pub fn spawn_in_active_session(cmdline: &str, workdir: Option<&Path>) -> Result<u32> {
    // The user token of the active console session (requires the host to be SYSTEM).
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

    // A primary token for CreateProcessAsUserW.
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

    // The user's environment block (PATH/USERPROFILE/SystemRoot for handler + DLL resolution), MERGED
    // with the host's PUNKTFUNK_*/RUST_LOG vars — same shared helper the WGC helper + service spawns use.
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

    // The game/launcher must appear on the interactive desktop the host is capturing.
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
    // SAFETY: `primary` is the live primary token; `cmd`, `desktop` (via `si.lpDesktop`), `workdir_w`
    // (via `cwd`) and `merged_env` are locals that outlive the call, each NUL-terminated as the API
    // requires — `merged_env` doubly so, per `merged_env_block`. `pi` is a live local out-param, and
    // the API retains none of these pointers.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(primary),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false, // no handle inheritance — fire-and-forget GUI launch, no stdio relay
            CREATE_UNICODE_ENVIRONMENT,
            Some(merged_env.as_ptr() as *const core::ffi::c_void),
            cwd,
            &si,
            &mut pi,
        )
    };
    // SAFETY: `primary` is live and owned here, closed exactly once and not used after.
    let _ = unsafe { CloseHandle(primary) };
    created.context("CreateProcessAsUserW (interactive-session launch)")?;

    let pid = pi.dwProcessId;
    // We don't supervise the child (it owns its own window/lifetime) — close the handles the API gave us.
    // SAFETY: `created` was `Ok`, so `pi` holds two owned handles; each is closed exactly once here
    // and never used after. Closing them does not terminate the child, which owns its own lifetime.
    unsafe {
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
    }
    Ok(pid)
}

/// Build the environment block for a process launched into the interactive session: the target
/// session's block (`user_block`, from `CreateEnvironmentBlock`) with this process's `PUNKTFUNK_*`
/// vars overlaid, so the child runs with the SAME settings this process has
/// (`PUNKTFUNK_ENCODER=nvenc`, `PUNKTFUNK_ZEROCOPY`, …) instead of the target shell's. Returns a
/// UTF-16, double-null-terminated block suitable for `CREATE_UNICODE_ENVIRONMENT`. Shared by the
/// interactive library launch (here) and the Windows service launching the host into the active
/// session ([`crate::service`]).
///
/// # Safety
/// `user_block` must be either null or a valid pointer to a UTF-16, double-null-terminated
/// environment block (the `CreateEnvironmentBlock` output), readable for its whole length.
pub(crate) unsafe fn merged_env_block(user_block: *const u16) -> Vec<u16> {
    // Parse the user block ("VAR=VALUE\0" … "\0") into entries.
    let mut entries: Vec<String> = Vec::new();
    if !user_block.is_null() {
        let mut p = user_block;
        loop {
            let mut len = 0isize;
            // SAFETY: per this fn's contract `p` points into a double-null-terminated block that is
            // readable for its whole length. `len` only advances over units this loop has already
            // read as non-NUL, so `p.offset(len)` stays inside the current entry — the scan stops at
            // that entry's terminator, and the empty entry stops the outer loop before `p` can pass
            // the block's end.
            while unsafe { *p.offset(len) } != 0 {
                len += 1;
            }
            if len == 0 {
                break; // the trailing empty string = end of block
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
    // Overlay "our" settings — PUNKTFUNK_* and RUST_LOG — dropping whatever the target block had.
    let is_ours = |k: &str| k.starts_with("PUNKTFUNK_") || k == "RUST_LOG";
    entries.retain(|e| !is_ours(e.split('=').next().unwrap_or("")));
    for (k, v) in std::env::vars().filter(|(k, _)| is_ours(k)) {
        entries.push(format!("{k}={v}"));
    }
    // Serialize back to a UTF-16 double-null-terminated block.
    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend(e.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}
