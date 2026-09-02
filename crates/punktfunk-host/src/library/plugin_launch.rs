//! Resolve a plugin-owned library entry to a command at launch time over the
//! plugin's registered loopback UI surface.
//!
//! Entries persist only an opaque key. The owning plugin returns the current
//! command; the host runs it so the process enters the captured session.
//! `None` if the plugin is down, disowns the key, or answers something unusable.
//!
//! The per-boot UI secret authenticates the registered listener, not plugin
//! ownership — see `mgmt::plugins`. Pin: [`ask_plugin_launch`], tests below.

use super::*;
use std::time::Duration;

/// 3 s covers a healthy local lookup; a wedged plugin cannot hold the launch
/// (or the GameStream data-plane thread that calls this) longer than that.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);

/// 4 KiB: one command line (`flatpak run … --core=… "/very/long/rom path"`), not a script.
const MAX_COMMAND: usize = 4096;

/// 64 KiB ceiling for `{command, cwd}`; typical answers are two short strings.
const MAX_BODY: usize = 64 * 1024;

/// Optional `cwd`: emulators resolve cores/configs relative to their install dir.
pub struct PluginLaunch {
    pub command: String,
    pub cwd: Option<PathBuf>,
}

/// `POST /__launch` body: `{command, cwd}`.
#[derive(Deserialize)]
struct LaunchReply {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// Opaque plugin key. Echoed as JSON and in logs: cap 512, no control chars.
pub fn valid_plugin_entry_key(v: &str) -> bool {
    !v.is_empty() && v.len() <= 512 && !v.chars().any(char::is_control)
}

/// Loopback `POST /__launch` for `plugin`'s entry `key`. `None` if unregistered,
/// disowned, or unusable; every arm logs so the operator can tell which.
///
/// Blocking (`ureq`). Callers sit on a blocking thread: `resolve_launch` hops
/// through `spawn_blocking`. Handshake probes use [`super::launch_is_resolvable`],
/// which never asks.
pub fn ask_plugin_launch(plugin: &str, key: &str) -> Option<PluginLaunch> {
    if !valid_plugin_entry_key(key) {
        tracing::warn!(
            plugin,
            "plugin launch: entry key failed validation — ignoring"
        );
        return None;
    }
    let Some(cred) = crate::mgmt::ui_credential(plugin) else {
        tracing::warn!(
            plugin,
            entry = key,
            "plugin launch: no live plugin registered under that provider id (is it running?) — \
             nothing to launch"
        );
        return None;
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(ASK_TIMEOUT))
        .build()
        .into();
    // Registration stores a port, never an address: this always dials 127.0.0.1.
    // `send` + Content-Type, not `send_json` — that needs ureq's `json` feature.
    let body = serde_json::json!({ "entry": key }).to_string();
    let resp = match agent
        .post(&format!("http://127.0.0.1:{}/__launch", cred.port))
        .header("Authorization", &format!("Bearer {}", cred.secret))
        .header("Content-Type", "application/json")
        .send(&body)
    {
        Ok(r) => r,
        // 404 = the plugin disowns the key (forged rows included).
        Err(ureq::Error::StatusCode(404)) => {
            tracing::warn!(
                plugin,
                entry = key,
                "plugin launch: the plugin does not own an entry with that key — nothing to launch"
            );
            return None;
        }
        Err(ureq::Error::StatusCode(code)) => {
            tracing::warn!(
                plugin,
                entry = key,
                code,
                "plugin launch: the plugin refused to resolve the entry"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                plugin,
                entry = key,
                error = %e,
                "plugin launch: could not reach the plugin's launch surface"
            );
            return None;
        }
    };
    let mut resp = resp;
    // cap+1 so an over-cap answer is caught by the length check below rather than truncated.
    let buf = match resp
        .body_mut()
        .with_config()
        .limit((MAX_BODY + 1) as u64)
        .read_to_vec()
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(plugin, entry = key, error = %e, "plugin launch: reading the answer failed");
            return None;
        }
    };
    if buf.len() > MAX_BODY {
        tracing::warn!(
            plugin,
            entry = key,
            "plugin launch: answer exceeds the {MAX_BODY}-byte cap"
        );
        return None;
    }
    let reply: LaunchReply = match serde_json::from_slice(&buf) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(plugin, entry = key, error = %e, "plugin launch: answer was not {{command, cwd}}");
            return None;
        }
    };
    validate_reply(plugin, key, reply)
}

/// Reply checks, factored so tests need no listening plugin.
fn validate_reply(plugin: &str, key: &str, reply: LaunchReply) -> Option<PluginLaunch> {
    let command = reply.command.trim().to_string();
    if command.is_empty() {
        tracing::warn!(
            plugin,
            entry = key,
            "plugin launch: answered an empty command"
        );
        return None;
    }
    if command.len() > MAX_COMMAND {
        tracing::warn!(
            plugin,
            entry = key,
            "plugin launch: command exceeds the {MAX_COMMAND}-byte cap"
        );
        return None;
    }
    // One line so the log is the line that ran. `\r` would mangle Windows `cmd.exe /c`.
    if command.chars().any(char::is_control) {
        tracing::warn!(
            plugin,
            entry = key,
            "plugin launch: command contains control characters — refusing it"
        );
        return None;
    }
    let cwd = match reply
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        None => None,
        Some(dir) => {
            let path = PathBuf::from(dir);
            // Host cwd ≠ plugin cwd; a relative path would run somewhere unintended.
            if !path.is_absolute() {
                tracing::warn!(
                    plugin,
                    entry = key,
                    cwd = dir,
                    "plugin launch: working directory must be absolute — refusing it"
                );
                return None;
            }
            Some(path)
        }
    };
    Some(PluginLaunch { command, cwd })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// One-shot loopback stub. The join handle is the raw request so HOST-side
    /// asserts fail in this thread.
    fn stub_plugin(status: u16, body: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read until the body named by Content-Length has arrived (ureq always sends one here).
            loop {
                let n = sock.read(&mut chunk).expect("read request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status} STATUS\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            let _ = sock.flush();
            String::from_utf8_lossy(&buf).to_string()
        });
        (port, handle)
    }

    #[test]
    fn asks_the_registered_plugin_and_takes_its_answer() {
        // Absolute for this OS: `/opt/emu` is relative on Windows. `\\` is JSON for `C:\emu`.
        let (answer, cwd) = if cfg!(windows) {
            (
                r#"{"command":"retroarch 'smw.sfc'","cwd":"C:\\emu"}"#,
                r"C:\emu",
            )
        } else {
            (
                r#"{"command":"retroarch 'smw.sfc'","cwd":"/opt/emu"}"#,
                "/opt/emu",
            )
        };
        let (port, server) = stub_plugin(200, answer);
        crate::mgmt::register_ui_for_test("stub-launcher", port, "s3cr3t");

        let got = ask_plugin_launch("stub-launcher", "snes/smw.sfc").expect("a recipe");
        assert_eq!(got.command, "retroarch 'smw.sfc'");
        assert_eq!(got.cwd.as_deref(), Some(std::path::Path::new(cwd)));

        let req = server.join().expect("stub thread");
        assert!(req.starts_with("POST /__launch "), "request was {req:?}");
        assert!(
            req.contains("Bearer s3cr3t"),
            "the ask must authenticate: {req:?}"
        );
        assert!(
            req.contains(r#""entry":"snes/smw.sfc""#),
            "body was {req:?}"
        );
    }

    #[test]
    fn a_404_means_the_plugin_disowns_the_entry() {
        let (port, server) = stub_plugin(404, r#"{"error":"no launchable entry \"forged\""}"#);
        crate::mgmt::register_ui_for_test("stub-disowner", port, "s");

        assert!(ask_plugin_launch("stub-disowner", "forged").is_none());
        server.join().expect("stub thread");
    }

    #[test]
    fn an_unregistered_provider_resolves_to_nothing() {
        assert!(ask_plugin_launch("no-such-plugin-is-registered", "k").is_none());
    }

    fn reply(command: &str, cwd: Option<&str>) -> LaunchReply {
        LaunchReply {
            command: command.into(),
            cwd: cwd.map(str::to_string),
        }
    }

    #[test]
    fn entry_keys_are_bounded_and_printable() {
        assert!(valid_plugin_entry_key("snes/Super Mario World.sfc"));
        assert!(!valid_plugin_entry_key(""));
        assert!(!valid_plugin_entry_key("with\nnewline"));
        assert!(!valid_plugin_entry_key("with\0nul"));
        assert!(!valid_plugin_entry_key(&"x".repeat(513)));
    }

    #[test]
    fn a_usable_answer_passes_through_trimmed() {
        let got = validate_reply(
            "rom-manager",
            "snes/smw",
            reply("  retroarch 'smw.sfc' \n", None),
        )
        .expect("usable");
        assert_eq!(got.command, "retroarch 'smw.sfc'");
        assert!(got.cwd.is_none());
    }

    #[test]
    fn empty_and_oversized_and_control_char_commands_are_refused() {
        assert!(validate_reply("p", "k", reply("   ", None)).is_none());
        assert!(validate_reply("p", "k", reply(&"x".repeat(MAX_COMMAND + 1), None)).is_none());
        // Newline: two lines in what the host logs as one command.
        assert!(validate_reply("p", "k", reply("retroarch rom\nrm -rf ~", None)).is_none());
    }

    #[test]
    fn a_working_directory_must_be_absolute() {
        let abs = if cfg!(windows) { r"C:\emu" } else { "/opt/emu" };
        let got = validate_reply("p", "k", reply("run", Some(abs))).expect("absolute cwd is fine");
        assert_eq!(got.cwd.as_deref(), Some(std::path::Path::new(abs)));
        assert!(validate_reply("p", "k", reply("run", Some("emu/cores"))).is_none());
        // An empty/whitespace cwd is "no preference", not a refusal.
        assert!(validate_reply("p", "k", reply("run", Some("  ")))
            .expect("blank cwd is tolerated")
            .cwd
            .is_none());
    }
}
