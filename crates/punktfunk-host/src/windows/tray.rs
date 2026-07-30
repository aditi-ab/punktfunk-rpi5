//! Tray lifecycle: the one place that knows how to find, start, stop and check the per-user status
//! tray. Shared by the `tray` CLI subcommand and by post-update reconciliation
//! (`update::windows::relaunch_tray`).
//!
//! Why this exists at all: `punktfunk-tray.exe` is a per-USER, per-SESSION GUI process with no
//! recovery path of its own. The HKLM `Run` value only fires at sign-in, and nothing in the product
//! restarts a tray that died — so anything that kills one (an upgrade's `StopTrays`, a crash) left
//! the operator without an icon until they signed out and back in.
//!
//! The launch has to cross a privilege boundary in one direction but not the other, so [`start`]
//! tries the session-crossing path first and falls back to a plain spawn:
//!
//! * **From the host service (SYSTEM)** — the tray must land in the active console session under
//!   the *logged-in user's* token, not SYSTEM's. That is
//!   [`crate::interactive::spawn_in_active_session`] (`WTSQueryUserToken` +
//!   `CreateProcessAsUserW`), and it needs `SE_TCB`, which only SYSTEM holds.
//! * **From an interactive shell** — the caller IS the user in the right session already, so
//!   `WTSQueryUserToken` fails (an administrator does not hold `SE_TCB` either) and a plain spawn
//!   is both sufficient and correct.
//!
//! Trying the privileged path and falling back on failure discriminates the two without a token
//! inspection, and without adding any `unsafe` to this crate.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// The tray's image name, as the installer lays it down next to `punktfunk-host.exe`.
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

/// `punktfunk-tray.exe` next to this executable, when it is actually installed (the `trayicon`
/// task is optional, so its absence is a legitimate state, not an error).
pub fn tray_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(TRAY_EXE)))
        .filter(|p| p.exists())
}

/// Is a tray running in ANY session? Reuses the conflicting-host scan's Toolhelp snapshot rather
/// than opening a second one. Best-effort: a failed snapshot reads as "not running", so callers
/// must treat this as a hint — never as proof for a destructive decision.
pub fn is_running() -> bool {
    let stem = TRAY_EXE.trim_end_matches(".exe");
    crate::detect::running_process_names()
        .iter()
        .any(|n| n == stem)
}

/// Start the tray if it is not already up.
///
/// `Ok(None)` = one was already running (idempotent by design: the tray also guards itself with a
/// `Local\PunktfunkTray` mutex, so even a lost race merely makes the second instance exit). The
/// `&'static str` names which mechanism launched it, for an honest CLI message.
///
/// Note for an ELEVATED interactive caller: the fallback spawn inherits this process's token, so
/// the tray then runs elevated. It works, but UIPI stops a medium-integrity `--quit` from closing
/// it later. Prefer `tray start` from a normal shell, or let the host service do it.
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
    // Only SYSTEM holds SE_TCB, so reaching here means an ordinary (possibly elevated) user — which
    // is fine ONLY if we are already sitting in the session the icon has to appear in. Refuse
    // loudly otherwise: a plain spawn from an ssh/RDP session would put a tray in a session nobody
    // is looking at, report success, and leave the operator staring at an empty notification area.
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
/// Graceful first, mirroring the uninstaller's own order (`[UninstallRun]`): `--quit` posts
/// WM_CLOSE to the tray window, which lets it remove its icon via `NIM_DELETE` instead of leaving a
/// ghost in the notification area until the shell next sweeps it. `--quit` only reaches the session
/// it runs in, so a force-kill reaps any instance in another session.
pub fn stop() -> bool {
    let was_running = is_running();
    if !was_running {
        return false;
    }
    if let Some(exe) = tray_exe() {
        // Best-effort and short: if the graceful close does not land we force it below anyway.
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
