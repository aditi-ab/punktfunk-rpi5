//! The Windows side of take-turns profiles (`design/windows-take-turns.md`): read which account is
//! on the console and which can be switched in, and attach one with `tscon`.
//!
//! The decision is pure ([`crate::profiles::take_turns`]); this is the WTS enumeration and the
//! switch it feeds. The host runs as SYSTEM in the console session, so `tscon <id> /dest:console`
//! attaches a disconnected session with no password, and the service's console-change relaunch
//! brings the host up in it. A switch marker file carries the "Switching to …" line across that
//! relaunch, since the process that ran the switch does not survive it.

use crate::profiles::take_turns::{parse_local_accounts, AccountState, LocalAccount};
use std::path::PathBuf;
use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSQuerySessionInformationW, WTSUserName, WTS_CONNECTSTATE_CLASS, WTS_SESSION_INFOW,
};

/// One local interactive session, as the switch reads it.
struct Session {
    id: u32,
    user: String,
    state: WTS_CONNECTSTATE_CLASS,
    console: bool,
}

/// What Windows reports about `account` now: whether it holds the console, and the id of a
/// non-console session to attach if not. `exists` stays true — a bound account's deletion is
/// caught when a profile is made, not re-checked every list (a gone account simply never readies).
pub(crate) fn account_state(account: &str) -> AccountState {
    let sessions = enumerate();
    let on_console = sessions
        .iter()
        .any(|s| s.console && s.user.eq_ignore_ascii_case(account));
    let switchable_session = (!on_console)
        .then(|| {
            sessions
                .iter()
                .find(|s| {
                    !s.console
                        && s.user.eq_ignore_ascii_case(account)
                        && (s.state == WTSActive || s.state == WTSDisconnected)
                })
                .map(|s| s.id)
        })
        .flatten();
    AccountState {
        exists: true,
        on_console,
        switchable_session,
    }
}

/// Attach `account`'s `session_id` to the console. Writes the switch marker first so the card
/// reads "Switching to …" through the relaunch, then runs `tscon`. The service sees the console
/// change and relaunches the host into the session.
pub(crate) fn switch_to(account: &str, session_id: u32) -> anyhow::Result<()> {
    write_switch_marker(account);
    let tscon = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join(r"System32\tscon.exe");
    let status = std::process::Command::new(tscon)
        .arg(session_id.to_string())
        .arg("/dest:console")
        .status()
        .map_err(|e| anyhow::anyhow!("run tscon: {e}"))?;
    if !status.success() {
        anyhow::bail!("tscon attach session {session_id} to console: {status}");
    }
    Ok(())
}

/// The switch marker path, beside the rest of the host config.
fn marker_path() -> PathBuf {
    pf_paths::config_dir().join("take-turns-switch")
}

/// Record a switch to `account` as in flight, now.
fn write_switch_marker(account: &str) {
    let now = crate::clock::unix_secs();
    if let Err(e) = std::fs::write(marker_path(), format!("{account}\n{now}\n")) {
        tracing::warn!(error = %e, "take-turns switch marker not written — the card won't show the wait");
    }
}

/// The marker (account, written-unix), or `None` when there is none or it doesn't parse.
pub(crate) fn read_switch_marker() -> Option<(String, i64)> {
    let text = std::fs::read_to_string(marker_path()).ok()?;
    let mut lines = text.lines();
    let account = lines.next()?.trim().to_string();
    let at = lines.next()?.trim().parse().ok()?;
    (!account.is_empty()).then_some((account, at))
}

/// Every local interactive session: its id, user, state, and whether it is the console. A session
/// that ends mid-enumeration is dropped (its user query fails).
fn enumerate() -> Vec<Session> {
    // SAFETY: no pointers in; returns the console session id by value.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let mut buffer: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: null server is the local machine; both out-pointers stay live for the call.
    if unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }.is_err()
        || buffer.is_null()
    {
        return Vec::new();
    }
    // SAFETY: WTS returned `count` consecutive records in the live allocation.
    let ids: Vec<(u32, WTS_CONNECTSTATE_CLASS)> =
        unsafe { std::slice::from_raw_parts(buffer, count as usize) }
            .iter()
            .map(|s| (s.SessionId, s.State))
            .collect();
    // SAFETY: the allocation WTSEnumerateSessionsW returned, freed once.
    unsafe { WTSFreeMemory(buffer.cast()) };
    ids.into_iter()
        .filter_map(|(id, state)| {
            let user = session_user(id)?;
            Some(Session {
                id,
                user,
                state,
                console: id == console,
            })
        })
        .collect()
}

/// The account name on session `id`, or `None` if it has none / the query fails.
fn session_user(id: u32) -> Option<String> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: both out-params stay live; WTS owns the returned allocation until the free below.
    unsafe { WTSQuerySessionInformationW(None, id, WTSUserName, &mut buffer, &mut bytes) }.ok()?;
    if buffer.is_null() {
        return None;
    }
    let units = bytes as usize / 2;
    // SAFETY: WTS reports `bytes` readable bytes at `buffer`; the allocation is live.
    let raw = unsafe { std::slice::from_raw_parts(buffer.0, units) };
    let end = raw.iter().position(|u| *u == 0).unwrap_or(raw.len());
    let name = String::from_utf16_lossy(&raw[..end]);
    // SAFETY: the allocation WTSQuerySessionInformationW returned, freed once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    (!name.trim().is_empty()).then(|| name.trim().to_string())
}

/// The enabled local Windows accounts, each with its SID; `take_turns::bindable` picks the ones a
/// profile may bind. A console-only convenience, so it shells out to `Get-LocalUser` rather than
/// carry a second account-enumeration FFI.
pub(crate) fn list_local_accounts() -> Vec<LocalAccount> {
    // Force UTF-8 on stdout: to a pipe PowerShell writes the console OEM codepage, so an account
    // name with a non-ASCII character (an umlaut, say) arrives as mojibake and never matches the
    // WTS console user. `OutputEncoding` makes `Get-LocalUser` emit UTF-8 this reads back whole.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; Get-LocalUser | Where-Object { $_.Enabled } | ForEach-Object { $_.Name + [char]9 + $_.SID.Value }",
        ])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    parse_local_accounts(&String::from_utf8_lossy(&out.stdout))
}
