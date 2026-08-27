//! The couch/HTPC hand-off: run the session binary's `--browse` mode (the complete
//! controller-driven client — host list, discovery, PIN pairing, settings, Wake-on-LAN,
//! library) and mirror its exit code.
//!
//! Shared by BOTH couch entry points — `punktfunk-console.exe`, which needs its own
//! executable because an MSIX `<Application>` cannot pass arguments, and this shell's
//! `--console` flag — so the spawn flags below are stated once. They were stated in neither
//! until 2026-08-27, which is why both Start-menu tiles opened a black console window that
//! then sat behind the couch UI for the whole session.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The session binary: installed next to us (the MSIX layout and dev `target\…` runs both
/// land on the sibling), else `PATH`.
pub(crate) fn session_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("punktfunk-session.exe");
        if sibling.exists() {
            return sibling;
        }
    }
    "punktfunk-session".into()
}

/// Run `punktfunk-session --browse` (fullscreen unless `--windowed`) and exit with the
/// child's code, so whatever supervises this process sees the real result. Never returns.
pub(crate) fn run_browse() -> ! {
    use std::os::windows::process::CommandExt as _;
    // `punktfunk-session` keeps the CONSOLE subsystem for its stdout contract, and both couch
    // entry points are GUI processes with no console to lend it — so without this flag Windows
    // mints one, and the couch UI comes up in front of a black terminal window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = Command::new(session_binary());
    cmd.arg("--browse");
    // A couch UI is fullscreen unless explicitly told otherwise.
    if !std::env::args().any(|a| a == "--windowed") {
        cmd.arg("--fullscreen");
    }
    cmd.stdin(Stdio::null())
        // Nothing here parses the stdout contract (no `--json-status`), but `match_window`
        // reports the settled window size on stdout REGARDLESS — and with no console the
        // handle it would inherit is invalid, which panics the child mid-stream on the first
        // report. A sink that goes nowhere is the difference between quiet and a crash.
        .stdout(Stdio::null())
        // Piped through the log tee: a couch launch (Start-menu tile, Steam shortcut) has no
        // console either, so the session's whole receive/decode/present log would otherwise
        // evaporate exactly when a user hits something worth reporting.
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);

    // Spawn (not `status()`) so the stderr pipe can be drained into the client log.
    let run = cmd.spawn().and_then(|mut child| {
        if let Some(stderr) = child.stderr.take() {
            crate::logfile::forward_child_stderr(stderr);
        }
        child.wait()
    });
    match run {
        Ok(st) => std::process::exit(st.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("could not start the console UI: {e}");
            std::process::exit(1);
        }
    }
}
