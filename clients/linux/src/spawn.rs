//! The shell↔session handoff (punktfunk-planning `linux-client-rearchitecture.md`,
//! Phase 3): desktop connects spawn the `punktfunk-session` Vulkan binary instead of
//! streaming in-process, and this module bridges its stdout contract back into the
//! shell's UI — spinner until `{"ready":true}`, banner from the `{"error"|"ended": …}`
//! JSON line, exit code 3 routed to the re-pair PIN ceremony.
//!
//! The in-process GTK presenter stays for: `PUNKTFUNK_LEGACY_PRESENTER=1`, Gaming-Mode /
//! `--fullscreen` launches and `--browse` (a second toplevel under gamescope is a focus
//! fight — the console path moves wholesale into the session binary in Phase 4), the
//! request-access flow (its "waiting for approval" dialog drives a parked in-process
//! connect), and connects with no fingerprint in hand (the session binary never TOFUs).

use crate::app::App;
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use gtk::glib;
use std::io::BufRead as _;
use std::process::{Command, Stdio};
use std::rc::Rc;

/// One parsed stdout line / lifecycle event from the session child.
enum ChildEvent {
    /// `{"ready":true}` — first frame presented; the connect spinner can stop.
    Ready,
    /// `{"error": msg, "trust_rejected": bool}` — connect failed (the exit follows).
    Error { msg: String, trust_rejected: bool },
    /// `{"ended": msg}` — the session ran and ended abnormally (host ended, transport…).
    Ended(String),
    /// The child exited (code; -1 = killed by signal). Always the final event.
    Exited(i32),
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
fn session_binary() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("punktfunk-session");
        if sibling.exists() {
            return sibling;
        }
    }
    "punktfunk-session".into()
}

/// Spawn the session binary for a connect with `fp_hex` pinned and drive its lifecycle
/// into the UI. `tofu` = the fingerprint came from the host's advert rather than the
/// store — persist it once the child reports ready (the child connects pinned to it, so
/// ready proves the host really holds that identity; stricter than the in-process TOFU,
/// which pins whatever it observes).
///
/// The caller has already taken `busy`; it is released when the child exits. `Err` =
/// the spawn itself failed (binary missing?) — the caller falls back to the in-process
/// presenter and `busy` stays with it.
pub fn spawn_session(
    app: Rc<App>,
    req: ConnectRequest,
    fp_hex: String,
    tofu: bool,
) -> Result<(), ()> {
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
    if app.settings.borrow().fullscreen_on_stream {
        cmd.arg("--fullscreen");
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, binary = %session_binary().display(),
                "punktfunk-session spawn failed");
            return Err(());
        }
    };
    tracing::info!(host = %req.addr, port = req.port, "session binary spawned");

    if let Some(h) = app.hosts_ui() {
        h.clear_error();
        h.set_connecting(Some(req.card_key()));
    }

    // Reader thread: stdout lines → events, then reap the child and send the final exit.
    let (tx, rx) = async_channel::unbounded::<ChildEvent>();
    let stdout = child.stdout.take().expect("piped stdout");
    std::thread::Builder::new()
        .name("punktfunk-session-io".into())
        .spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(ev) = parse_line(&line) {
                    let _ = tx.send_blocking(ev);
                }
            }
            // EOF — the child is exiting (or crashed); reap it so nothing zombies.
            let code = child
                .wait()
                .map(|s| s.code().unwrap_or(-1))
                .unwrap_or(-1);
            let _ = tx.send_blocking(ChildEvent::Exited(code));
        })
        .map_err(|e| {
            tracing::warn!(error = %e, "session reader thread failed to start");
        })?;

    // Main-loop side: spinner/banner/re-pair per event; busy releases on exit.
    glib::spawn_future_local(async move {
        let mut error: Option<(String, bool)> = None;
        let mut ended: Option<String> = None;
        while let Ok(ev) = rx.recv().await {
            match ev {
                ChildEvent::Ready => {
                    if let Some(h) = app.hosts_ui() {
                        h.set_connecting(None);
                    }
                    if tofu {
                        // The advertised fingerprint just proved itself on a real
                        // connect — pin it (unpaired), like the in-process TOFU.
                        trust::persist_host(&req.name, &req.addr, req.port, &fp_hex, false);
                        app.toast(&format!(
                            "Trusted on first use — fingerprint {}…",
                            &fp_hex[..16]
                        ));
                    }
                }
                ChildEvent::Error { msg, trust_rejected } => error = Some((msg, trust_rejected)),
                ChildEvent::Ended(msg) => ended = Some(msg),
                ChildEvent::Exited(code) => {
                    tracing::info!(code, "session binary exited");
                    app.busy.set(false);
                    if let Some(h) = app.hosts_ui() {
                        h.set_connecting(None);
                    }
                    match (code, error.take(), ended.take()) {
                        (0, _, None) => {} // clean end — back on the hosts page, no noise
                        (0, _, Some(reason)) => app.connect_error(&reason),
                        (_, Some((_, true)), _) if !tofu => {
                            // The stored pin no longer matches (rotated cert or
                            // impostor) — route to re-pairing, like the in-process path.
                            app.toast("Host fingerprint changed — re-pair with a PIN to continue");
                            crate::ui_trust::pin_dialog(app.clone(), req.clone());
                        }
                        (_, Some((msg, _)), _) => app.connect_error(&format!("Couldn't connect — {msg}")),
                        (code, None, _) => app.connect_error(&format!(
                            "Stream session failed (punktfunk-session exit {code})"
                        )),
                    }
                    break;
                }
            }
        }
    });
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
