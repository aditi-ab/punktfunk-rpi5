//! Tray lifecycle: find, start, stop, check, and supervise `punktfunk-tray.exe`.
//! Shared by the `tray` CLI and by [`supervise`], which the host service runs
//! for its whole lifetime.
//!
//! The tray is a per-user, per-session GUI with no recovery of its own. HKLM
//! `Run` fires at sign-in only; [`supervise`] restarts a tray that dies after.
//!
//! [`start`] tries the user-token path first, then a plain spawn:
//!
//! * **From a streaming host (SYSTEM)** — launch as the signed-in user of that
//!   host's WTS session via
//!   [`crate::interactive::spawn_as_current_session_user`] (`WTSQueryUserToken`
//!   + `CreateProcessAsUserW`). Only SYSTEM holds the required `SE_TCB`.
//! * **From an interactive shell** — the caller already has the right user and
//!   session token, so `WTSQueryUserToken` fails and a plain spawn is correct.
//!
//! Seat hosts do not supervise a tray: the seat manager is their control surface.
//! An explicit seat-local start still uses that session's user and mutex.

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

/// Whether this host belongs to an add-on-managed seat rather than the console.
fn seat_host() -> bool {
    std::env::var("PUNKTFUNK_SEAT_SESSION").as_deref() == Ok("1")
}

/// Reads the console install's HKLM `Run` opt-in for tray supervision.
fn wanted() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run")
        .and_then(|k| k.get_value::<String, _>("PunktfunkTray"))
        .is_ok()
}

/// Keeps the console status tray alive while the ordinary host runs.
///
/// Seat hosts return before spawning the watcher because their manager is the
/// control surface. On the console, two misses trigger [`ensure`]; one miss is
/// the tray's own Exit racing the service shutdown.
pub fn supervise() {
    if seat_host() {
        tracing::debug!("seat host: status-tray supervision disabled");
        return;
    }
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

/// Starts a tray for the caller's WTS session.
///
/// Console callers avoid a duplicate visible in the process snapshot. Seat
/// callers launch and let the tray's session-local mutex resolve duplicates;
/// a console tray must not suppress theirs. SYSTEM drops to the session user,
/// while an interactive caller keeps its token.
pub fn start() -> Result<(Option<u32>, &'static str)> {
    let Some(exe) = tray_exe() else {
        bail!("{TRAY_EXE} is not installed next to this executable");
    };
    if !seat_host() && is_running() {
        return Ok((None, "already running"));
    }
    // Quoting preserves an operator-chosen install path that contains spaces.
    let quoted = format!("\"{}\"", exe.display());
    if let Ok(pid) = crate::interactive::spawn_as_current_session_user(&quoted, None) {
        return Ok((Some(pid), "as this session's user"));
    }
    // WTSQueryUserToken is privileged; an interactive caller's plain spawn preserves its seat.
    let child = std::process::Command::new(&exe)
        .spawn()
        .with_context(|| format!("spawn {}", exe.display()))?;
    Ok((Some(child.id()), "in this session"))
}

/// Stops the tray in this session; the console path can reap stale peers.
///
/// `--quit` posts `WM_CLOSE` through the session-local tray mutex so the icon is
/// removed cleanly. A seat host never uses global `taskkill`, which would kill
/// other seats. Console supervision restarts its tray while the HKLM opt-in
/// remains present.
pub fn stop() -> bool {
    let was_running = is_running();
    if !was_running && !seat_host() {
        return false;
    }
    if let Some(exe) = tray_exe() {
        if let Ok(mut child) = std::process::Command::new(&exe).arg("--quit").spawn() {
            let _ = child.wait();
        }
        if seat_host() {
            return true;
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
