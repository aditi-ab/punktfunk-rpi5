//! Tray lifecycle: find, start, stop, check, and supervise `punktfunk-tray.exe`.
//! Shared by the `tray` CLI and by [`supervise`], which the host service runs
//! for its whole lifetime.
//!
//! The tray is a per-user, per-session GUI with no recovery of its own. HKLM
//! `Run` fires at sign-in only; [`supervise`] restarts a tray that dies after.
//!
//! [`start`] tries the session-crossing path first, then a plain spawn:
//!
//! * **From the host service (SYSTEM)** — land in the active console session
//!   under the logged-in user's token via
//!   [`crate::interactive::spawn_in_active_session`] (`WTSQueryUserToken` +
//!   `CreateProcessAsUserW`). Needs `SE_TCB`, which only SYSTEM holds.
//! * **From an interactive shell** — the caller is already the user in the
//!   right session, so `WTSQueryUserToken` fails and a plain spawn is correct.
//!
//! Trying the privileged path first discriminates the two without a token
//! inspection and without adding `unsafe` to this crate.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub const TRAY_EXE: &str = "punktfunk-tray.exe";

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => {
            let (pid, how) = start()?;
            match pid {
                Some(pid) => println!("status tray started (pid {pid}, {how})"),
                None => println!("status tray is already running"),
            }
            Ok(())
        }
        Some("stop") => {
            if stop() {
                println!("status tray stopped");
            } else {
                println!("no status tray was running");
            }
            Ok(())
        }
        Some("status") => {
            let path = tray_exe();
            println!(
                "status tray: {}",
                match (&path, is_running()) {
                    (None, _) => "not installed".to_string(),
                    (Some(_), true) => "running".to_string(),
                    (Some(_), false) => "not running".to_string(),
                }
            );
            if let Some(p) = path {
                println!("executable:  {}", p.display());
            }
            Ok(())
        }
        _ => bail!("usage: punktfunk-host tray <start|stop|status>"),
    }
}

/// `punktfunk-tray.exe` next to this executable. The `trayicon` task is optional, so absence is not an error.
pub fn tray_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(TRAY_EXE)))
        .filter(|p| p.exists())
}

/// Any session. Best-effort: a failed snapshot reads as not running — a hint, never proof for a kill.
pub fn is_running() -> bool {
    let stem = TRAY_EXE.trim_end_matches(".exe");
    crate::detect::running_process_names()
        .iter()
        .any(|n| n == stem)
}

/// Two misses restart the tray, so this is half the grace window.
const WATCH_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// HKLM `Run` `PunktfunkTray` is the only "this box wants an icon" signal. The exe is always on disk.
fn wanted() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run")
        .and_then(|k| k.get_value::<String, _>("PunktfunkTray"))
        .is_ok()
}

/// Keep a status tray alive while the host runs. Spawned once from `mgmt::run`.
///
/// First check is immediate. Later ticks need two consecutive misses before
/// [`ensure`]: one miss is the tray's own Exit, which stops this service a few
/// seconds later.
pub fn supervise() {
    std::thread::spawn(|| {
        if !wanted() {
            tracing::debug!("no HKLM Run entry for the status tray — not supervising it");
            return;
        }
        ensure();
        let mut missed = false;
        loop {
            std::thread::sleep(WATCH_TICK);
            let absent = !is_running();
            if absent && missed {
                ensure();
            }
            missed = absent;
        }
    });
}

/// Best-effort: a miss is usually "nobody signed in yet", so this stays at `debug`/`info`.
fn ensure() {
    match start() {
        Ok((Some(pid), how)) => tracing::info!(pid, how, "status tray started"),
        Ok((None, _)) => tracing::trace!("status tray is already running"),
        Err(e) => tracing::debug!(error = %e, "could not start the status tray"),
    }
}

/// Start the tray if it is not already up.
///
/// `Ok(None)` = already running. The tray also holds `Local\PunktfunkTray`, so a lost race just
/// exits the second instance. The `&'static str` names the launch path.
///
/// An elevated interactive spawn inherits this token; UIPI then blocks a medium-integrity `--quit`.
/// Prefer `tray start` from a normal shell, or let the host service do it.
pub fn start() -> Result<(Option<u32>, &'static str)> {
    let Some(exe) = tray_exe() else {
        bail!("{TRAY_EXE} is not installed next to this executable");
    };
    if is_running() {
        return Ok((None, "already running"));
    }
    // Quoted: the install directory is operator-chosen and routinely contains spaces.
    let quoted = format!("\"{}\"", exe.display());
    let session_err = match crate::interactive::spawn_in_active_session(&quoted, None) {
        Ok(pid) => return Ok((Some(pid), "into the active console session")),
        Err(e) => e,
    };
    // Only SYSTEM holds SE_TCB. A fallback spawn is correct only in the console session; from
    // ssh/RDP it would land in a session nobody is looking at and still report success.
    if let Some((own, console)) = crate::interactive::console_session_mismatch() {
        bail!(
            "cannot place the tray in session {console} from session {own}: {session_err}\n\
             crossing sessions needs SE_TCB, which only SYSTEM holds — run this from the console \
             session itself, or let the host service do it"
        );
    }
    let child = std::process::Command::new(&exe)
        .spawn()
        .with_context(|| format!("spawn {}", exe.display()))?;
    Ok((Some(child.id()), "in this session"))
}

/// Stop every tray instance. Returns whether one was running.
///
/// [`supervise`] puts one back within a minute while this host runs — a diagnostic, not an off
/// switch. Clearing the HKLM `Run` value (see [`wanted`]) is the off switch.
///
/// `--quit` posts WM_CLOSE so the tray can `NIM_DELETE` its icon; skip that and the shell keeps a
/// ghost. `--quit` only reaches this session, so `taskkill` reaps any instance in another.
pub fn stop() -> bool {
    let was_running = is_running();
    if !was_running {
        return false;
    }
    if let Some(exe) = tray_exe() {
        if let Ok(mut child) = std::process::Command::new(&exe).arg("--quit").spawn() {
            let _ = child.wait();
        }
        for _ in 0..8 {
            if !is_running() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", TRAY_EXE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    true
}
