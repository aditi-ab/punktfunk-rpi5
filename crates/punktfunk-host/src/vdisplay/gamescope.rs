//! gamescope virtual-display backend.
//!
//! Unlike KWin/Mutter (which create a virtual output at runtime via a protocol), gamescope is a
//! micro-compositor we *spawn*: `gamescope --backend headless -W w -H h -r hz -- <app>`. It runs
//! the app nested, composites at the requested size/refresh (so the source rate is the client's
//! rate natively — no separate refresh step), and exports a built-in PipeWire node named
//! `gamescope` (media.class `Video/Source`, BGRx/NV12, dmabuf or shm) on the user's PipeWire
//! daemon. We discover that node and capture it like any other; the gamescope *process* is the
//! keepalive — dropping the [`VirtualOutput`] kills it (tearing the output down).
//!
//! Requirements: gamescope built with PipeWire + libei input emulation (distro packages are);
//! a usable Vulkan device (the NVIDIA render node). Headless capture on the proprietary NVIDIA
//! driver is plausible-by-architecture but not a well-trodden path — validate empirically.
//! Input is a gamescope-specific libei/EIS socket (`LIBEI_SOCKET`), wired separately (TODO).

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The gamescope virtual-display driver. Each [`create`](VirtualDisplay::create) spawns one
/// headless gamescope process sized to the requested mode.
pub struct GamescopeDisplay;

impl GamescopeDisplay {
    pub fn new() -> Result<Self> {
        Ok(GamescopeDisplay)
    }
}

impl VirtualDisplay for GamescopeDisplay {
    fn name(&self) -> &'static str {
        "gamescope"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Attach to an already-running gamescope (e.g. a headless `gamescope-session-plus` Steam
        // session, or a debug/Steam-launched one) instead of spawning our own: capture its node
        // AND inject into its EIS socket. PUNKTFUNK_GAMESCOPE_NODE=<id|auto>; "auto" discovers the
        // gamescope `Video/Source` node so nothing has to be hand-wired. This is the Bazzite path:
        // a persistent headless Steam-Deck-UI session (full gamescope-session-plus polish, at the
        // client's resolution) that punktfunk streams + drives, instead of nesting a second Steam.
        if let Ok(id) = std::env::var("PUNKTFUNK_GAMESCOPE_NODE") {
            let node_id: u32 = if id.trim().eq_ignore_ascii_case("auto") {
                find_gamescope_node().ok_or_else(|| {
                    anyhow!(
                        "PUNKTFUNK_GAMESCOPE_NODE=auto but no running gamescope Video/Source node \
                         was found — is the headless gamescope/Steam session up?"
                    )
                })?
            } else {
                id.parse()
                    .context("PUNKTFUNK_GAMESCOPE_NODE must be a node id or 'auto'")?
            };
            // Point the libei injector at the running gamescope's EIS socket: it reads the relay
            // file [`EI_SOCKET_FILE`], so write the live socket's name there. Best-effort — video
            // still works without it (input just won't reach the attached session).
            match find_gamescope_eis_socket() {
                Some(sock) => match std::fs::write(EI_SOCKET_FILE, &sock) {
                    Ok(()) => tracing::info!(
                        socket = %sock,
                        "gamescope attach: pointed injector at the running session's EIS socket"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "gamescope attach: could not write the EIS relay file — input may not reach the session"
                    ),
                },
                None => tracing::warn!(
                    "gamescope attach: no connectable gamescope EIS socket found — input injection \
                     will not reach the attached session"
                ),
            }
            tracing::info!(node_id, "gamescope: attaching to existing PipeWire node");
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
                keepalive: Box::new(()),
            });
        }
        check_gamescope_version(); // diagnostic only — warns on known-deadlock-prone versions
        let proc = GamescopeProc(spawn(mode.width, mode.height, mode.refresh_hz.max(1))?);
        // gamescope creates its PipeWire node a moment after start; poll for it (the proc is held
        // alive meanwhile, and killed if we give up).
        let node_id = wait_for_node(Duration::from_secs(15)).ok_or_else(|| {
            anyhow!(
                "gamescope PipeWire node did not appear within 15s — gamescope may have failed to \
                 start or headless capture is unsupported on this GPU/driver (see /tmp/punktfunk-gamescope.log)"
            )
        })?;
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "gamescope virtual output ready"
        );
        Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(proc),
        })
    }
}

/// File where the wrapper below writes gamescope's `LIBEI_SOCKET` (its EIS server socket),
/// read by the libei injector to drive input into the nested app. See [`crate::inject`].
pub const EI_SOCKET_FILE: &str = "/tmp/punktfunk-gamescope-ei";

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. The app comes from
/// `PUNKTFUNK_GAMESCOPE_APP` (default a no-op that just keeps gamescope alive — set it to a real
/// game/GL app for actual content, e.g. `steam -gamepadui` for the SteamOS-like session).
/// stdout/stderr go to `/tmp/punktfunk-gamescope.log`. The app is launched through a tiny shell
/// wrapper that relays gamescope's `LIBEI_SOCKET` (set for its children) to [`EI_SOCKET_FILE`]
/// so the input injector can connect to gamescope's EIS server from outside.
fn spawn(w: u32, h: u32, hz: u32) -> Result<Child> {
    let app =
        std::env::var("PUNKTFUNK_GAMESCOPE_APP").unwrap_or_else(|_| "sleep infinity".to_string());
    let _ = std::fs::remove_file(EI_SOCKET_FILE); // stale socket path from a previous session
    let mut cmd = Command::new("gamescope");
    cmd.args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &hz.to_string()])
        .args(["--xwayland-count", "1", "--"])
        .args([
            "sh",
            "-c",
            &format!("printf %s \"$LIBEI_SOCKET\" > {EI_SOCKET_FILE}; exec \"$@\""),
            "sh",
        ])
        .args(app.split_whitespace())
        // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
        .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia");
    if let Ok(log) = std::fs::File::create("/tmp/punktfunk-gamescope.log") {
        if let Ok(log2) = log.try_clone() {
            cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log2));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    tracing::info!(w, h, hz, %app, "spawning gamescope (headless)");
    cmd.spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")
}

/// Wait for gamescope to report its PipeWire node. Authoritative source: gamescope's own log
/// line `stream available on node ID: N` (its node carries `node.name=gamescope` on TWO objects
/// — the adapter and the inner stream — and only the advertised id is the correct capture
/// target). Falls back to `pw-dump` discovery if the log line doesn't show.
fn wait_for_node(timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = node_from_log() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return find_gamescope_node(); // last-resort fallback
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Parse `stream available on node ID: N` from the spawned gamescope's log (ANSI-colored).
fn node_from_log() -> Option<u32> {
    let log = std::fs::read_to_string("/tmp/punktfunk-gamescope.log").ok()?;
    for line in log.lines().rev() {
        if let Some(pos) = line.find("stream available on node ID:") {
            let tail = &line[pos + "stream available on node ID:".len()..];
            let digits: String = tail.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// Find the `gamescope` `Video/Source` node id in a `pw-dump` snapshot of the default daemon.
///
/// `node.name=gamescope` appears on TWO objects (the adapter *and* the inner stream node); only
/// the one whose `media.class` is `Video/Source` is a valid capture target — connecting to the
/// other wedges the link. So we require `Video/Source` first and fall back to a bare name match
/// only if no class-tagged node is present (older gamescope that doesn't set media.class).
fn find_gamescope_node() -> Option<u32> {
    let out = Command::new("pw-dump").output().ok()?;
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let nodes = dump.as_array()?;
    let node_props = |obj: &serde_json::Value| -> Option<(u32, String, String)> {
        if obj.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            return None;
        }
        let id = obj.get("id").and_then(|i| i.as_u64())? as u32;
        let props = obj.get("info").and_then(|i| i.get("props"));
        let name = props
            .and_then(|p| p.get("node.name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let class = props
            .and_then(|p| p.get("media.class"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        Some((id, name, class))
    };
    // Preferred: a Video/Source node named (or containing) "gamescope".
    for obj in nodes {
        if let Some((id, name, class)) = node_props(obj) {
            if class == "Video/Source" && (name == "gamescope" || name.contains("gamescope")) {
                return Some(id);
            }
        }
    }
    // Fallback: a node literally named "gamescope" with no usable class tag.
    for obj in nodes {
        if let Some((id, name, _)) = node_props(obj) {
            if name == "gamescope" {
                tracing::warn!(
                    node_id = id,
                    "gamescope node has no media.class=Video/Source tag — capturing it anyway"
                );
                return Some(id);
            }
        }
    }
    None
}

/// Find the live gamescope EIS (libei) socket to inject into when ATTACHING to an existing
/// session (the spawn path instead relays the nested gamescope's `LIBEI_SOCKET` through a file).
///
/// gamescope names its EIS socket `gamescope-<display>-ei` in `XDG_RUNTIME_DIR` (alongside the
/// `gamescope-<display>` wayland socket). Stale sockets from dead sessions linger, so we don't
/// trust the name — we `connect()` each candidate and keep the connectable ones, returning the
/// most recently created (the live session). Returns the bare socket *name* (the injector
/// resolves it against `XDG_RUNTIME_DIR`, matching libei's own `LIBEI_SOCKET` semantics).
fn find_gamescope_eis_socket() -> Option<String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut live: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in std::fs::read_dir(&runtime).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The EIS socket itself, not its `.lock` sidecar or the bare wayland socket.
        if !(name.starts_with("gamescope-") && name.ends_with("-ei")) {
            continue;
        }
        // Connectable == a live listener is behind it (a dead session's socket refuses).
        if std::os::unix::net::UnixStream::connect(entry.path()).is_err() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        live.push((mtime, name));
    }
    live.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    live.into_iter().next().map(|(_, n)| n)
}

/// gamescope is usable wherever its binary runs — it spawns its own nested session, so it does
/// not require any particular desktop to be running. Quiet (no version warning — that's for the
/// create path); just checks the binary executes.
pub fn is_available() -> bool {
    std::process::Command::new("gamescope")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimum gamescope that captures reliably: below 3.16.22, headless PipeWire capture deadlocks
/// against PipeWire ≥ 1.6 (a loop-lock bug) and a stuck link head-blocks the whole daemon.
const MIN_GAMESCOPE: (u32, u32, u32) = (3, 16, 22);

/// Best-effort: warn loudly if the installed gamescope is older than [`MIN_GAMESCOPE`]. Parsing
/// failures are silent (don't block a possibly-fine custom build) — this is a diagnostic, not a
/// gate. Returns the parsed version when it could read one.
fn check_gamescope_version() -> Option<(u32, u32, u32)> {
    let out = Command::new("gamescope").arg("--version").output().ok()?;
    // gamescope prints the version banner to stderr on some builds, stdout on others.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ver = parse_version(&text)?;
    if ver < MIN_GAMESCOPE {
        tracing::warn!(
            found = %format!("{}.{}.{}", ver.0, ver.1, ver.2),
            min = %format!("{}.{}.{}", MIN_GAMESCOPE.0, MIN_GAMESCOPE.1, MIN_GAMESCOPE.2),
            "gamescope is older than the minimum for reliable headless capture — expect a \
             capture deadlock against PipeWire ≥ 1.6 (a wedged link head-blocks the daemon); \
             upgrade gamescope or use PUNKTFUNK_COMPOSITOR=kwin|mutter"
        );
    }
    Some(ver)
}

/// Extract the first `X.Y.Z` version triple from arbitrary text (e.g. `gamescope version 3.16.22`).
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let mut parts = token.split('.');
        let (a, b, c) = (parts.next()?, parts.next(), parts.next());
        let (Some(b), Some(c)) = (b, c) else { continue };
        if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), c.parse()) {
            return Some((a, b, c));
        }
    }
    None
}

/// Owns the spawned gamescope process; killing it tears the virtual output down.
struct GamescopeProc(Child);

impl Drop for GamescopeProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        // Clear the relayed EIS socket name so the host-lifetime injector can't reconnect to this
        // now-dead session's socket between sessions (the stale path is the "Connection refused").
        let _ = std::fs::remove_file(EI_SOCKET_FILE);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_version, MIN_GAMESCOPE};

    #[test]
    fn parses_version_banner() {
        assert_eq!(
            parse_version("gamescope version 3.16.22"),
            Some((3, 16, 22))
        );
        assert_eq!(
            parse_version("gamescope: version v3.15.9 (no PipeWire)"),
            Some((3, 15, 9))
        );
        assert_eq!(parse_version("3.16.20-1.fc41"), Some((3, 16, 20)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("only 3.16 here"), None); // needs a full triple
    }

    #[test]
    fn flags_known_bad_versions() {
        // The 26.04-shipped 3.16.20 is below the minimum (PipeWire 1.6 deadlock).
        assert!(parse_version("gamescope version 3.16.20").unwrap() < MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.16.22").unwrap() >= MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.17.0").unwrap() >= MIN_GAMESCOPE);
    }
}
