//! The shell↔session handoff: every stream runs in the spawned `punktfunk-session`
//! Vulkan binary (the legacy in-process presenter is gone — phase 5 of
//! punktfunk-planning `linux-client-rearchitecture.md`). This module owns the child's
//! lifecycle plumbing — its stdout contract parsed into typed [`AppMsg`]s the relm4 app
//! consumes: spinner until `{"ready":true}`, banner from the `{"error"|"ended": …}`
//! line, exit code 3 + `trust_rejected` routed to the re-pair PIN ceremony.

use crate::app::AppMsg;
use crate::ui_hosts::ConnectRequest;
use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// Spawn tunables beyond a plain connect.
#[derive(Debug, Default)]
pub struct SpawnOpts {
    /// Handshake budget override (`--connect-timeout`) — the request-access flow passes
    /// ~185 s because the host PARKS the connection until the operator approves.
    pub connect_timeout_secs: Option<u64>,
    /// Persist the host as *paired* once the child reports ready (request-access: the
    /// operator's approval IS the pairing). Plain TOFU persists unpaired.
    pub persist_paired: bool,
    /// A cancel handle to arm (request-access's waiting dialog): killing the child is
    /// the only abort a parked connect has.
    pub cancel: Option<CancelHandle>,
}

/// Kills the spawned session child (the request-access Cancel button). Safe to call
/// any time; a child that already exited is a no-op.
#[derive(Clone, Debug, Default)]
pub struct CancelHandle(Arc<Mutex<Option<Child>>>);

impl CancelHandle {
    pub fn kill(&self) {
        if let Some(child) = self.0.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// One parsed stdout line from the session child's contract.
enum ChildEvent {
    Ready,
    Error { msg: String, trust_rejected: bool },
    Ended(String),
}

/// Parse one stdout line of the session contract; `None` for anything else (stats…).
fn parse_line(line: &str) -> Option<ChildEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("ready").and_then(|r| r.as_bool()) == Some(true) {
        return Some(ChildEvent::Ready);
    }
    if let Some(msg) = v.get("error").and_then(|m| m.as_str()) {
        return Some(ChildEvent::Error {
            msg: msg.to_string(),
            trust_rejected: v.get("trust_rejected").and_then(|t| t.as_bool()) == Some(true),
        });
    }
    if let Some(msg) = v.get("ended").and_then(|m| m.as_str()) {
        return Some(ChildEvent::Ended(msg.to_string()));
    }
    None
}

/// The session binary: installed next to the shell, else `$PATH` (dev runs from
/// `target/…` land on the sibling).
pub fn session_binary() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("punktfunk-session");
        if sibling.exists() {
            return sibling;
        }
    }
    "punktfunk-session".into()
}

/// Spawn the session binary for a connect with `fp_hex` pinned and translate its
/// lifecycle into [`AppMsg`]s. `tofu` = the fingerprint came from the host's advert
/// rather than the store — the app persists it once the child reports ready (the child
/// connects pinned to it, so ready proves the host really holds that identity).
///
/// The caller has already taken `busy`; [`AppMsg::SessionExited`] releases it. `Err` =
/// the spawn itself failed (binary missing?) — surfaced as a connect error.
pub fn spawn_session(
    sender: relm4::Sender<AppMsg>,
    req: ConnectRequest,
    fp_hex: String,
    tofu: bool,
    fullscreen_on_stream: bool,
    opts: SpawnOpts,
) -> Result<(), String> {
    let mut cmd = Command::new(session_binary());
    cmd.arg("--connect")
        .arg(format!("{}:{}", req.addr, req.port))
        .arg("--fp")
        .arg(&fp_hex)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()); // session logs interleave with the shell's
    if let Some((id, _)) = &req.launch {
        cmd.arg("--launch").arg(id);
    }
    if let Some(secs) = opts.connect_timeout_secs {
        cmd.arg("--connect-timeout").arg(secs.to_string());
    }
    if fullscreen_on_stream {
        cmd.arg("--fullscreen");
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start punktfunk-session: {e}"))?;
    tracing::info!(host = %req.addr, port = req.port, "session binary spawned");

    let stdout = child.stdout.take().expect("piped stdout");
    // Park the child where the cancel handle (and the reader, for the final reap) can
    // reach it.
    let slot = opts
        .cancel
        .clone()
        .unwrap_or_default();
    *slot.0.lock().unwrap() = Some(child);

    let persist_paired = opts.persist_paired;
    std::thread::Builder::new()
        .name("punktfunk-session-io".into())
        .spawn(move || {
            let mut error: Option<(String, bool)> = None;
            let mut ended: Option<String> = None;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match parse_line(&line) {
                    Some(ChildEvent::Ready) => {
                        let _ = sender.send(AppMsg::SessionReady {
                            req: req.clone(),
                            fp_hex: fp_hex.clone(),
                            tofu,
                            persist_paired,
                        });
                    }
                    Some(ChildEvent::Error { msg, trust_rejected }) => {
                        error = Some((msg, trust_rejected));
                    }
                    Some(ChildEvent::Ended(msg)) => ended = Some(msg),
                    None => {}
                }
            }
            // EOF — reap the child (killed-by-cancel lands here too; -1 = signal).
            let code = slot
                .0
                .lock()
                .unwrap()
                .take()
                .and_then(|mut c| c.wait().ok())
                .and_then(|s| s.code())
                .unwrap_or(-1);
            tracing::info!(code, "session binary exited");
            let _ = sender.send(AppMsg::SessionExited {
                req,
                code,
                error,
                ended,
                tofu,
            });
        })
        .map_err(|e| format!("session reader thread: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_stdout_contract() {
        assert!(matches!(parse_line("{\"ready\":true}"), Some(ChildEvent::Ready)));
        match parse_line("{\"error\":\"no route\",\"trust_rejected\":false}") {
            Some(ChildEvent::Error { msg, trust_rejected }) => {
                assert_eq!(msg, "no route");
                assert!(!trust_rejected);
            }
            _ => panic!("error line"),
        }
        match parse_line("{\"error\":\"pin\",\"trust_rejected\":true}") {
            Some(ChildEvent::Error { trust_rejected, .. }) => assert!(trust_rejected),
            _ => panic!("trust line"),
        }
        match parse_line("{\"ended\":\"Host ended the session\"}") {
            Some(ChildEvent::Ended(m)) => assert_eq!(m, "Host ended the session"),
            _ => panic!("ended line"),
        }
        // Stats and stray output never become events.
        assert!(parse_line("stats: 1280×800@60 · 60 fps").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("{\"other\":1}").is_none());
    }
}
