//! `punktfunk-console.exe` — the couch/HTPC entry point.
//!
//! Exists because an MSIX `<Application>` cannot pass ARGUMENTS to a full-trust executable:
//! a second Start-menu tile therefore cannot simply be "punktfunk-client.exe --console", it
//! needs its own executable. This is that executable, and it is deliberately nothing but a
//! hand-off — it starts the session binary's `--browse` mode (the complete controller-driven
//! client: host list, discovery, PIN pairing, settings, Wake-on-LAN, library) fullscreen and
//! mirrors its exit code, so whatever supervises this process sees the real result.
//!
//! `--windowed` keeps it in a window; everything else is the session binary's own business.

// No console window: this is launched from a Start-menu tile / shortcut, and a flashing
// console behind the couch UI looks like a crash.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    // The session binary ships beside us in the package; fall back to PATH for a dev run.
    let session = std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("punktfunk-session.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "punktfunk-session".into());

    let mut cmd = std::process::Command::new(session);
    cmd.arg("--browse");
    if !std::env::args().any(|a| a == "--windowed") {
        cmd.arg("--fullscreen");
    }
    match cmd.status() {
        Ok(st) => std::process::exit(st.code().unwrap_or(0)),
        Err(_) => std::process::exit(1),
    }
}

/// The workspace builds on Linux/macOS too; there is nothing to launch there.
#[cfg(not(windows))]
fn main() {}
