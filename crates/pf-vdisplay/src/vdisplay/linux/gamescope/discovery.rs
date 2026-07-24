//! gamescope **discovery + probes** (plan §W3, carved out of the backend): finding the compositor's
//! PipeWire node (log line first, then a scoped `pw-dump` fallback), locating its live EIS/libei
//! socket, the version gate, and the dedicated-session game-exit check. Pure read-side plumbing — it
//! observes gamescope, never spawns or tears it down (that stays in [`super`]).

use super::*;

/// Wait for gamescope to report its PipeWire node. Authoritative source: gamescope's own log
/// line `stream available on node ID: N` (its node carries `node.name=gamescope` on TWO objects
/// — the adapter and the inner stream — and only the advertised id is the correct capture
/// target). Falls back to `pw-dump` discovery if the log line doesn't show.
/// B2 (game-exit detection): confirm a **dedicated** gamescope session's game has exited. gamescope is
/// a single-app compositor — it exits when its nested app exits — so once capture is lost, THIS
/// session's `node_id` not reappearing within a short confirmation window means the game quit (vs. a
/// transient PipeWire hiccup). Scoped to the session's own `node_id` (via [`gamescope_node_present`]),
/// so a **coexisting** gamescope (a second dedicated session, or the box's game-mode gamescope beside a
/// non-Steam dedicated launch) doesn't mask the exit (review findings #4/#8). Returns `true` when the
/// node stays absent across the window.
pub(crate) fn game_session_exited(node_id: u32) -> bool {
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        if gamescope_node_present(node_id) {
            return false; // OUR node is (still) present → not an exit (transient loss)
        }
        if Instant::now() >= deadline {
            return true; // our node stayed gone across the window → the game exited
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Outcome of watching a dedicated Steam launch's game lifetime ([`wait_for_steam_game_exit`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SteamGameWatch {
    /// The game was seen running and has now exited — the session should end (APP_EXITED).
    Exited,
    /// The watch was cancelled (the session ended for another reason) or the game never started
    /// within the startup grace — leave the session as-is.
    Cancelled,
}

/// Parse the Steam appid a dedicated launch targets from its resolved command
/// (`steam [-silent] steam://rungameid/<appid>`). `None` unless the first token is `steam` and a
/// `steam://rungameid/<digits>` URI is present — the trailing digits are the appid, which is exactly
/// what Steam's launch reaper carries as `AppId=<appid>` (gameid == appid for a plain library title,
/// the only kind the host ever resolves to this shape).
pub(crate) fn steam_appid_from_launch(cmd: &str) -> Option<u32> {
    if cmd.split_whitespace().next() != Some("steam") {
        return None;
    }
    const MARKER: &str = "steam://rungameid/";
    let tail = &cmd[cmd.find(MARKER)? + MARKER.len()..];
    let digits: String = tail
        .chars()
        .take_while(|c: &char| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Block until the dedicated Steam game `appid` has started and then exited, `cancel` is set, or the
/// game never appears within the startup grace. Same-uid `/proc` scan keyed on Steam's launch reaper
/// (`SteamLaunch AppId=<appid>`), whose lifetime is exactly the game's. Returns
/// [`SteamGameWatch::Exited`] only after the game was actually seen running and then stayed gone
/// across a short confirmation window — so a cold Steam boot / shader precompile (game not up yet) or
/// a transient scan miss can't end the stream early. Runs on the host's per-session watch thread.
pub(crate) fn wait_for_steam_game_exit(
    appid: u32,
    cancel: &std::sync::atomic::AtomicBool,
) -> SteamGameWatch {
    use std::sync::atomic::Ordering;
    // Cold Steam boot + first-launch shader precompile can delay the game window by minutes; give it a
    // generous window to appear. A game that never starts leaves the session up (the Steam client is
    // still streamed, and the node-death path still covers the Steam client itself dying).
    const START_GRACE: Duration = Duration::from_secs(300);
    const POLL: Duration = Duration::from_secs(1);
    // Require the reaper gone across this window (a few polls) so a brief process swap can't fire early.
    const EXIT_CONFIRM: Duration = Duration::from_secs(3);

    let start_deadline = Instant::now() + START_GRACE;
    // Phase 1: wait for the game's reaper to appear.
    while !steam_game_running(appid) {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= start_deadline {
            return SteamGameWatch::Cancelled;
        }
        std::thread::sleep(POLL);
    }
    // Phase 2: the game is up — wait for its reaper to disappear (confirmed across the window).
    let mut gone_since: Option<Instant> = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return SteamGameWatch::Cancelled;
        }
        if steam_game_running(appid) {
            gone_since = None;
        } else if gone_since.get_or_insert_with(Instant::now).elapsed() >= EXIT_CONFIRM {
            return SteamGameWatch::Exited;
        }
        std::thread::sleep(POLL);
    }
}

/// Is Steam's launch reaper for appid `appid` alive right now (same uid as the host)? Steam wraps
/// every game launch — native or Proton — in `…/reaper SteamLaunch AppId=<appid> -- <game>`, and the
/// reaper lives for the game's whole lifetime, so its presence is a precise "the game is running"
/// signal. Matched on the `SteamLaunch` + `AppId=<appid>` argv tokens together (exact-match, so
/// `AppId=57` never matches appid 570) — specific to the game reaper, so Steam's own shader-precompile
/// step (not reaper-wrapped) can't be mistaken for the game.
fn steam_game_running(appid: u32) -> bool {
    // SAFETY: `getuid()` is a parameterless POSIX call that always succeeds and touches no memory.
    let uid = unsafe { libc::getuid() };
    let appid_tok = format!("AppId={appid}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(md) = std::fs::metadata(e.path()) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid {
            continue;
        }
        let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let (mut launch, mut appid_match) = (false, false);
        for arg in cmdline.split(|&b| b == 0) {
            if arg == b"SteamLaunch" {
                launch = true;
            } else if arg == appid_tok.as_bytes() {
                appid_match = true;
            }
        }
        if launch && appid_match {
            return true;
        }
    }
    false
}

/// Poll [`find_gamescope_node`] (unscoped) up to `timeout` — for the managed / SteamOS session, which
/// logs to journald (no per-spawn file) and is single-session (no scoping needed).
pub(super) fn poll_managed_node(timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = find_gamescope_node() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

pub(super) fn wait_for_node(
    timeout: Duration,
    log: &std::path::Path,
    child_pid: u32,
) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = node_from_log(log) {
            return Some(id);
        }
        if Instant::now() >= deadline {
            // Last-resort fallback scoped to THIS spawn's process tree (A5), so a coexisting gamescope's
            // node isn't picked by mistake.
            return find_gamescope_node_scoped(Some(child_pid));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Parse `stream available on node ID: N` from a spawned gamescope's per-instance log (ANSI-colored).
fn node_from_log(log: &std::path::Path) -> Option<u32> {
    let log = std::fs::read_to_string(log).ok()?;
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

/// Is a PipeWire node with exactly `node_id` present on the default daemon right now? Used by the
/// keep-alive reuse liveness probe ([`GamescopeDisplay::kept_display_alive`]): a kept gamescope node
/// vanishes when its nested game exits, so a missing id means "recreate, don't reuse the corpse".
pub(super) fn gamescope_node_present(node_id: u32) -> bool {
    let Ok(out) = Command::new("pw-dump").arg(node_id.to_string()).output() else {
        // pw-dump unavailable → don't block reuse (mark_failed is the backstop on a genuinely dead node).
        return true;
    };
    let Ok(dump) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return true;
    };
    dump.as_array()
        .map(|objs| {
            objs.iter().any(|o| {
                o.get("id").and_then(|i| i.as_u64()) == Some(node_id as u64)
                    && o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Node")
            })
        })
        .unwrap_or(true)
}

/// Find the `gamescope` `Video/Source` node id in a `pw-dump` snapshot of the default daemon.
///
/// `node.name=gamescope` appears on TWO objects (the adapter *and* the inner stream node); only
/// the one whose `media.class` is `Video/Source` is a valid capture target — connecting to the
/// other wedges the link. So we require `Video/Source` first and fall back to a bare name match
/// only if no class-tagged node is present (older gamescope that doesn't set media.class).
pub(super) fn find_gamescope_node() -> Option<u32> {
    find_gamescope_node_scoped(None)
}

/// Like [`find_gamescope_node`], but when `scope` is `Some(pid)` only a node whose owning process
/// (`application.process.id`) is `pid` or a descendant of it qualifies (A5 — a spawn's node must
/// belong to OUR gamescope's process tree, so a coexisting foreign / other-session gamescope node is
/// never mistaken for ours). `None` = any gamescope node (the managed/attach paths, single-session).
fn find_gamescope_node_scoped(scope: Option<u32>) -> Option<u32> {
    let out = Command::new("pw-dump").output().ok()?;
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let nodes = dump.as_array()?;
    let node_props = |obj: &serde_json::Value| -> Option<(u32, String, String, Option<u32>)> {
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
        // PipeWire records the owning process id as a string or an int depending on version.
        let pid = props
            .and_then(|p| p.get("application.process.id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .map(|n| n as u32)
            });
        Some((id, name, class, pid))
    };
    // A node is in-scope when no scope is asked, or its owning pid descends from the scope pid. When
    // the pid prop is absent (older gamescope / PipeWire) we DON'T exclude it — falling back to the
    // per-instance log is the primary addressing (design §7 risk note).
    let in_scope = |pid: Option<u32>| -> bool {
        match scope {
            None => true,
            Some(root) => pid.map(|p| descends_from(p, root)).unwrap_or(true),
        }
    };
    // Preferred: a Video/Source node named (or containing) "gamescope", in scope.
    for obj in nodes {
        if let Some((id, name, class, pid)) = node_props(obj) {
            if class == "Video/Source"
                && (name == "gamescope" || name.contains("gamescope"))
                && in_scope(pid)
            {
                return Some(id);
            }
        }
    }
    // Fallback: a node literally named "gamescope" with no usable class tag, in scope.
    for obj in nodes {
        if let Some((id, name, _, pid)) = node_props(obj) {
            if name == "gamescope" && in_scope(pid) {
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
pub(super) fn find_gamescope_eis_socket() -> Option<String> {
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
pub(crate) fn is_available() -> bool {
    std::process::Command::new("gamescope")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimum gamescope that captures reliably: below 3.16.22, headless PipeWire capture deadlocks
/// against PipeWire ≥ 1.6 (a loop-lock bug) and a stuck link head-blocks the whole daemon.
const MIN_GAMESCOPE: (u32, u32, u32) = (3, 16, 22);

/// First gamescope that paints the Steam overlay (Shift+Tab / Quick Access Menu) into its built-in
/// PipeWire node. `paint_pipewire()` is a *separate, reduced* composite from the display scanout;
/// the overlay-window paint (gated on the consumer negotiating `gamescope_focus_appid == 0`, which
/// we do by never advertising that property — see the capturer's EnumFormat builders) first ships
/// in 3.16.23 (gamescope commits `ccd62074` + `f8b33d38`). Below this the overlay is *never* in the
/// node, so it cannot appear in the stream no matter what the host does. The cursor and
/// external-overlay / notification layers are excluded on *every* version (handled host-side).
const MIN_GAMESCOPE_OVERLAY: (u32, u32, u32) = (3, 16, 23);

/// Best-effort: warn if the installed gamescope is older than [`MIN_GAMESCOPE`] (capture is
/// unreliable) or than [`MIN_GAMESCOPE_OVERLAY`] (capture works but the Steam overlay can't reach
/// the stream). Parsing failures are silent (don't block a possibly-fine custom build) — this is a
/// diagnostic, not a gate. Returns the parsed version when it could read one.
pub(super) fn check_gamescope_version() -> Option<(u32, u32, u32)> {
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
    } else if ver < MIN_GAMESCOPE_OVERLAY {
        // Capture is fine; the Steam overlay just won't be in the frame gamescope hands us.
        tracing::warn!(
            found = %format!("{}.{}.{}", ver.0, ver.1, ver.2),
            min = %format!(
                "{}.{}.{}",
                MIN_GAMESCOPE_OVERLAY.0, MIN_GAMESCOPE_OVERLAY.1, MIN_GAMESCOPE_OVERLAY.2
            ),
            "gamescope is older than the first version that paints the Steam overlay (Shift+Tab / \
             Quick Access Menu) into its PipeWire node — the overlay will be absent from the \
             stream until you upgrade gamescope (the cursor is composited host-side regardless)"
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

#[cfg(test)]
mod tests {
    use super::{parse_version, steam_appid_from_launch, MIN_GAMESCOPE, MIN_GAMESCOPE_OVERLAY};

    #[test]
    fn parses_steam_appid_from_launch() {
        // The resolved dedicated-launch command (pre- or post-`-silent` shaping) → the appid.
        assert_eq!(
            steam_appid_from_launch("steam steam://rungameid/570"),
            Some(570)
        );
        assert_eq!(
            steam_appid_from_launch("steam -silent steam://rungameid/1091500"),
            Some(1091500)
        );
        // Non-Steam launches / bare Steam with no rungameid URI → no appid (no game-exit watch).
        assert_eq!(steam_appid_from_launch("lutris lutris:rungameid/42"), None);
        assert_eq!(steam_appid_from_launch("steam -gamepadui"), None);
        assert_eq!(steam_appid_from_launch("vkcube"), None);
        // A steam:// URI that isn't the first `steam` token (a custom command) is not treated as one.
        assert_eq!(
            steam_appid_from_launch("firefox steam://rungameid/570"),
            None
        );
    }

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

    #[test]
    fn overlay_threshold_brackets_the_fix() {
        // 3.16.22 captures fine but predates the overlay-in-pipewire paint — it sits in the
        // "capture works, overlay absent" window `[MIN_GAMESCOPE, MIN_GAMESCOPE_OVERLAY)`, which is
        // exactly the `else if` warn arm; 3.16.23 is the first to include the overlay.
        assert!(parse_version("gamescope version 3.16.22").unwrap() >= MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.16.22").unwrap() < MIN_GAMESCOPE_OVERLAY);
        assert!(parse_version("gamescope version 3.16.23").unwrap() >= MIN_GAMESCOPE_OVERLAY);
        assert!(parse_version("gamescope version 3.16.25").unwrap() >= MIN_GAMESCOPE_OVERLAY);
    }
}
