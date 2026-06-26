//! Launch a process into the interactive user session from the SYSTEM host.
//!
//! The Windows host runs as a LocalSystem SCM service. To *launch* a game/launcher so it renders onto
//! the captured desktop — and so the user's protocol handlers (`HKCU\Software\Classes`), UWP/appx
//! activation, and each store's auth/entitlement context resolve — the process must run in the
//! interactive session under the **logged-in user's** token, not SYSTEM and not session 0.
//!
//! This is the same `WTSGetActiveConsoleSessionId → WTSQueryUserToken → DuplicateTokenEx →
//! CreateProcessAsUserW(winsta0\\default)` primitive the WGC helper relay uses
//! ([`crate::capture::wgc_relay`]), factored out for the library launch path
//! ([`crate::library::launch_title`]).
//!
//! IMPORTANT — use the **user** token (`WTSQueryUserToken`), NOT a session-retargeted SYSTEM token
//! (the host-spawn in [`crate::service`] duplicates the SYSTEM token and only changes its session id;
//! that is correct for launching *our own* streamer, but a store launcher needs the real user's token
//! for activation + auth). The host process itself stays SYSTEM.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

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

/// Spawn `cmdline` in the active console session, under the logged-in user's token, on the
/// interactive desktop (`winsta0\default`). Returns the new process id.
///
/// Fire-and-forget: the launched game/launcher outlives this call, so the host does not track the
/// child — its handles are closed before returning (the process keeps running). The environment is
/// the user's block merged with the host's `PUNKTFUNK_*`/`RUST_LOG` (same merge the WGC helper uses),
/// so `host.env` settings propagate.
///
/// Requires the host to run as SYSTEM (`WTSQueryUserToken` needs `SE_TCB`). Fails when no interactive
/// user is logged on (a pre-login / freshly-booted box can stream the login desktop but cannot
/// auto-launch a store title until someone signs in).
pub fn spawn_in_active_session(cmdline: &str, workdir: Option<&Path>) -> Result<u32> {
    // SAFETY: `spawn_inner` is unsafe only for its Win32 FFI; it has no caller-side preconditions — it
    // validates the session/token itself and owns every handle it opens — so calling it is always sound.
    unsafe { spawn_inner(cmdline, workdir) }
}

unsafe fn spawn_inner(cmdline: &str, workdir: Option<&Path>) -> Result<u32> {
    // The user token of the active console session (requires the host to be SYSTEM).
    let session = WTSGetActiveConsoleSessionId();
    if session == 0xFFFF_FFFF {
        bail!("no active console session (no interactive user is logged on)");
    }
    let mut user_token = HANDLE::default();
    WTSQueryUserToken(session, &mut user_token)
        .context("WTSQueryUserToken (host must be SYSTEM; needs a logged-on interactive user)")?;

    // A primary token for CreateProcessAsUserW.
    let mut primary = HANDLE::default();
    let dup = DuplicateTokenEx(
        user_token,
        TOKEN_ALL_ACCESS,
        None,
        SecurityImpersonation,
        TokenPrimary,
        &mut primary,
    );
    let _ = CloseHandle(user_token);
    dup.context("DuplicateTokenEx(TokenPrimary)")?;

    // The user's environment block (PATH/USERPROFILE/SystemRoot for handler + DLL resolution), MERGED
    // with the host's PUNKTFUNK_*/RUST_LOG vars — same shared helper the WGC helper + service spawns use.
    let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
    let _ = CreateEnvironmentBlock(&mut env_block, Some(primary), false);
    let merged_env = crate::capture::wgc_relay::merged_env_block(env_block as *const u16);
    if !env_block.is_null() {
        let _ = DestroyEnvironmentBlock(env_block);
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
    let created = CreateProcessAsUserW(
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
    );
    let _ = CloseHandle(primary);
    created.context("CreateProcessAsUserW (interactive-session launch)")?;

    let pid = pi.dwProcessId;
    // We don't supervise the child (it owns its own window/lifetime) — close the handles the API gave us.
    let _ = CloseHandle(pi.hProcess);
    let _ = CloseHandle(pi.hThread);
    Ok(pid)
}
