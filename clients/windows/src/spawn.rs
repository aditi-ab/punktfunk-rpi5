//! The shell↔session handoff: streams run in the spawned `punktfunk-session` Vulkan
//! binary (session-always, mirroring the GTK shell's `clients/linux/src/spawn.rs`). This
//! module owns the child's lifecycle plumbing — spawned with CREATE_NO_WINDOW (the
//! session keeps the console subsystem for its stdout contract; without the flag a GUI
//! parent would pop a console window), its stdout contract parsed into typed
//! [`SpawnEvent`]s a reader thread hands to the app's navigation closure: spinner until
//! `{"ready":true}`, banner from the `{"error"|"ended": …}` line, `trust_rejected`
//! routed to the re-pair PIN ceremony, `stats:` lines to the session status page.
//!
//! The legacy in-process D3D11VA presenter remains reachable via the
//! `PUNKTFUNK_BUILTIN_STREAM=1` env override (`app::use_builtin_stream`) — the
//! developer A/B baseline until its deletion.

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// One parsed event from the session child.
pub(crate) enum SpawnEvent {
    /// The child presented its first frame (its window is up and streaming).
    Ready,
    /// One `stats:` line, already human-formatted by the session (per 1 s window).
    Stats(String),
    /// The child exited (stdout EOF + reap; a kill lands here too). `error`/`ended`
    /// carry the contract lines seen on the way out, when any (the exit code is logged
    /// by the reader; routing keys off the lines, which say strictly more).
    Exited {
        error: Option<(String, bool)>,
        ended: Option<String>,
    },
}

/// Kills the spawned session child (the Disconnect button, request-access Cancel). Safe
/// to call any time; a child that already exited is a no-op. A FRESH handle is installed
/// per spawn (`Shared::session`) so a stale handle can never kill a newer session.
#[derive(Clone, Default)]
pub(crate) struct SessionChild(Arc<Mutex<Option<Child>>>);

impl SessionChild {
    pub(crate) fn kill(&self) {
        if let Some(child) = self.0.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// One parsed stdout line of the session contract; `None` for anything unrecognized.
enum ChildLine {
    Ready,
    Error { msg: String, trust_rejected: bool },
    Ended(String),
    Stats(String),
}

fn parse_line(line: &str) -> Option<ChildLine> {
    if let Some(stats) = line.strip_prefix("stats: ") {
        return Some(ChildLine::Stats(stats.to_string()));
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("ready").and_then(|r| r.as_bool()) == Some(true) {
        return Some(ChildLine::Ready);
    }
    if let Some(msg) = v.get("error").and_then(|m| m.as_str()) {
        return Some(ChildLine::Error {
            msg: msg.to_string(),
            trust_rejected: v.get("trust_rejected").and_then(|t| t.as_bool()) == Some(true),
        });
    }
    if let Some(msg) = v.get("ended").and_then(|m| m.as_str()) {
        return Some(ChildLine::Ended(msg.to_string()));
    }
    None
}

/// The session binary: installed next to the shell (the MSIX layout and dev
/// `target\…` runs both land on the sibling), else `PATH`.
pub(crate) fn session_binary() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("punktfunk-session.exe");
        if sibling.exists() {
            return sibling;
        }
    }
    "punktfunk-session".into()
}

/// Spawn the session binary for a connect with `fp_hex` pinned and feed its lifecycle to
/// `on_event` from a reader thread. The child is parked in `slot` so Disconnect/Cancel
/// can kill it. `fullscreen` starts the stream window fullscreen (the Settings "Start
/// streams fullscreen" toggle); `launch` carries a library title id for the host to
/// launch during the handshake. `Err` = the spawn itself failed (binary missing?) —
/// surfaced as a connect error by the caller.
pub(crate) fn spawn_session(
    addr: &str,
    port: u16,
    fp_hex: &str,
    connect_timeout_secs: u64,
    fullscreen: bool,
    launch: Option<&str>,
    slot: SessionChild,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let mut cmd = Command::new(session_binary());
    cmd.arg("--connect")
        .arg(format!("{addr}:{port}"))
        .arg("--fp")
        .arg(fp_hex)
        .arg("--connect-timeout")
        .arg(connect_timeout_secs.to_string());
    if fullscreen {
        cmd.arg("--fullscreen");
    }
    if let Some(id) = launch {
        cmd.arg("--launch").arg(id);
    }
    spawn_with(cmd, &format!("{addr}:{port}"), slot, on_event)
}

/// Spawn the session binary in `--browse` mode: the console (gamepad) library for a
/// PAIRED host, in the session window — launches run as streams in that same window.
/// The same stdout contract as a connect (`--json-status`): `ready` when the library
/// window presents, `error` on a failed start, EOF on quit.
pub(crate) fn spawn_browse(
    addr: &str,
    port: u16,
    fullscreen: bool,
    slot: SessionChild,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let mut cmd = Command::new(session_binary());
    cmd.arg("--browse")
        .arg(format!("{addr}:{port}"))
        .arg("--json-status");
    if fullscreen {
        cmd.arg("--fullscreen");
    }
    spawn_with(cmd, &format!("{addr}:{port}"), slot, on_event)
}

/// The shared spawn + stdout-contract reader behind [`spawn_session`]/[`spawn_browse`].
fn spawn_with(
    mut cmd: Command,
    host_label: &str,
    slot: SessionChild,
    mut on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // session logs interleave with the shell's (dev runs)
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start punktfunk-session: {e}"))?;
    tracing::info!(host = %host_label, "session binary spawned");

    let stdout = child.stdout.take().expect("piped stdout");
    // Park the child where the kill handle (and the reader, for the final reap) reach it.
    *slot.0.lock().unwrap() = Some(child);

    std::thread::Builder::new()
        .name("punktfunk-session-io".into())
        .spawn(move || {
            let mut error: Option<(String, bool)> = None;
            let mut ended: Option<String> = None;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match parse_line(&line) {
                    Some(ChildLine::Ready) => on_event(SpawnEvent::Ready),
                    Some(ChildLine::Stats(s)) => on_event(SpawnEvent::Stats(s)),
                    Some(ChildLine::Error {
                        msg,
                        trust_rejected,
                    }) => error = Some((msg, trust_rejected)),
                    Some(ChildLine::Ended(msg)) => ended = Some(msg),
                    None => {}
                }
            }
            // EOF — reap the child (killed-by-Disconnect lands here too; -1 = no code).
            let code = slot
                .0
                .lock()
                .unwrap()
                .take()
                .and_then(|mut c| c.wait().ok())
                .and_then(|s| s.code())
                .unwrap_or(-1);
            tracing::info!(code, "session binary exited");
            on_event(SpawnEvent::Exited { error, ended });
        })
        .map_err(|e| format!("session reader thread: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_stdout_contract() {
        assert!(matches!(
            parse_line("{\"ready\":true}"),
            Some(ChildLine::Ready)
        ));
        match parse_line("{\"error\":\"no route\",\"trust_rejected\":false}") {
            Some(ChildLine::Error {
                msg,
                trust_rejected,
            }) => {
                assert_eq!(msg, "no route");
                assert!(!trust_rejected);
            }
            _ => panic!("error line"),
        }
        match parse_line("{\"error\":\"pin\",\"trust_rejected\":true}") {
            Some(ChildLine::Error { trust_rejected, .. }) => assert!(trust_rejected),
            _ => panic!("trust line"),
        }
        match parse_line("{\"ended\":\"Host ended the session\"}") {
            Some(ChildLine::Ended(m)) => assert_eq!(m, "Host ended the session"),
            _ => panic!("ended line"),
        }
        // Stats lines become Stats events; stray output never becomes an event.
        match parse_line("stats: 1280\u{00D7}800@60 \u{00B7} 60 fps") {
            Some(ChildLine::Stats(s)) => assert!(s.starts_with("1280")),
            _ => panic!("stats line"),
        }
        assert!(parse_line("").is_none());
        assert!(parse_line("{\"other\":1}").is_none());
    }
}
