//! Resolve a plugin-owned library entry to a command at launch time over the plugin's
//! registered loopback UI surface.
//!
//! Entries persist only an opaque key. Asking the owning plugin avoids storing executable
//! content, rejects entries the plugin disowns, and picks up current emulator configuration.
//! The host executes the answer so the process enters the captured session and game lifetime.
//!
//! This is not a boundary against a stolen `plugin-token`: the runner shares one credential,
//! so its holder can replace any registration and control the command returned for that id.
//! The per-boot UI secret authenticates the registered listener, not plugin ownership. Closing
//! that principal gap requires runner isolation or executing as the runner, not validation here.

use super::*;
use std::time::Duration;

/// The whole ask, end to end. A plugin resolving one of its own entries is a local lookup against
/// state it already holds, so this is generous for a healthy plugin and short enough that a wedged
/// one cannot hold a launch — or, on the GameStream plane, the data-plane thread that calls this —
/// for longer than a player would keep staring at a tile that did nothing.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);

/// A command LINE, not a script. Generous for `flatpak run … --core=… "/very/long/rom path"`,
/// bounded so a malformed answer cannot land a megabyte in the logs or in a shell argument.
const MAX_COMMAND: usize = 4096;

/// Cap the whole response body — the shape is two short strings.
const MAX_BODY: usize = 64 * 1024;

/// What a plugin answered: the command line to run, and optionally the directory to run it in
/// (emulators that resolve cores or configs relative to their install dir need one).
pub struct PluginLaunch {
    pub command: String,
    pub cwd: Option<PathBuf>,
}

/// The wire shape of `POST /__launch`'s response.
#[derive(Deserialize)]
struct LaunchReply {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// The opaque per-entry key a `plugin` launch carries. It is echoed to the owning plugin as JSON and
/// lands in log lines, so bound it and keep control characters out; everything else is the plugin's
/// own namespace (rom-manager uses its `<platform>/<relpath>` external id).
pub fn valid_plugin_entry_key(v: &str) -> bool {
    !v.is_empty() && v.len() <= 512 && !v.chars().any(char::is_control)
}

/// Ask `plugin` what to run for its entry `key`.
///
/// `None` — the plugin is not registered/live, has no UI surface, disowns the entry, or answered
/// something unusable. Every arm logs, because from a player's seat all of them look like "the tile
/// did nothing", and the difference is exactly what an operator needs to fix it.
///
/// **Blocking** (`ureq`, the host's existing off-runtime HTTP client): callers run on a blocking
/// thread. `resolve_launch`'s async callers hop through `spawn_blocking`, and the handshake's
/// "is this launchable at all" probe uses [`super::launch_is_resolvable`], which never asks.
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
    // Loopback + the plugin's own per-boot secret, exactly what the console proxy presents. The
    // registration stores a PORT, never an address (mgmt::plugins D5), so this can only ever dial
    // this machine.
    // `send` with an explicit content type rather than `send_json`: that one needs ureq's `json`
    // feature, and the body is one field.
    let body = serde_json::json!({ "entry": key }).to_string();
    let resp = match agent
        .post(&format!("http://127.0.0.1:{}/__launch", cred.port))
        .header("Authorization", &format!("Bearer {}", cred.secret))
        .header("Content-Type", "application/json")
        .send(&body)
    {
        Ok(r) => r,
        // A plugin that does not know the entry says so with a 404 — the answer a FORGED entry gets,
        // and the reason planting one is not enough to make the host run anything.
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

/// The checks on what came back, split out so they can be tested without a plugin on a port.
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
    // Hygiene rather than a security boundary — a plugin that wanted two commands could always write
    // `a; b`, and composing the line is its job. But a launch command is ONE line: keeping control
    // characters out is what makes the logged line the line that ran, and what stops a stray `\r`
    // from mangling the Windows `cmd.exe /c` form.
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
            // Relative to WHAT? The host's cwd is not the plugin's, and a launch that silently ran
            // somewhere unintended is worse than one that says why it did not.
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

    /// A one-shot HTTP/1.1 stub on an ephemeral loopback port. Returns the port and a handle that
    /// yields the raw request text — so the assertions about what the HOST sent (method, path,
    /// bearer, body) live in the test thread, where a failure reads as a failure.
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
        // The cwd has to be absolute FOR THE HOST PLATFORM: `/opt/emu` has no drive letter, so
        // `Path::is_absolute` is false on Windows and `validate_reply` refuses the recipe — this
        // test could never pass there. Same split as `a_working_directory_must_be_absolute`.
        // (The `\\` is JSON escaping; the decoded value is `C:\emu`.)
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
        // The plugin's own per-boot secret, the same credential the console proxy presents.
        assert!(
            req.contains("Bearer s3cr3t"),
            "the ask must authenticate: {req:?}"
        );
        // The entry key is what the plugin resolves against its own state — it must be on the wire.
        assert!(
            req.contains(r#""entry":"snes/smw.sfc""#),
            "body was {req:?}"
        );
    }

    #[test]
    fn a_404_means_the_plugin_disowns_the_entry() {
        // The cross-provider case: a row planted under someone else's provider launches nothing,
        // because that provider's plugin is the one asked and it never published the key.
        let (port, server) = stub_plugin(404, r#"{"error":"no launchable entry \"forged\""}"#);
        crate::mgmt::register_ui_for_test("stub-disowner", port, "s");

        assert!(ask_plugin_launch("stub-disowner", "forged").is_none());
        server.join().expect("stub thread");
    }

    #[test]
    fn an_unregistered_provider_resolves_to_nothing() {
        // No live plugin, no port to dial, no launch — and no panic.
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
        // The interesting one: a second line smuggled into what the host logs as a single command.
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
