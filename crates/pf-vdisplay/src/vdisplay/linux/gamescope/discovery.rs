//! gamescope **discovery + probes** (plan §W3, carved out of the backend): finding the compositor's
//! PipeWire node (log line first, then a scoped `pw-dump` fallback), locating its live EIS/libei
//! socket, the version gate, and the dedicated-session game-exit check. Pure read-side plumbing — it
//! observes gamescope, never spawns or tears it down (that stays in [`super`]).

use super::*;

/// Budget for a `pw-dump` snapshot. Two facts make an unbounded one the worst call in this file:
/// it is polled every 300–500 ms from three separate 45 s loops, and it talks to the very daemon
/// this module documents gamescope as head-blocking below [`MIN_GAMESCOPE`] — so the failure mode
/// is not "slow", it is "never returns", on the session's own stream thread. Two seconds is far
/// above a populated graph's real cost; every caller already has a "couldn't ask" path.
const PW_DUMP_BUDGET: Duration = Duration::from_secs(2);

/// Budget for a `gamescope --version` probe. It loads the binary and prints a banner — no Vulkan
/// device, no daemon — so anything approaching this bound is a binary that cannot run at all,
/// which is exactly what a `None`/`false` answer means to each caller.
const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(2);

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
    let uid = crate::proc::current_uid();
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

/// Wait for a freshly spawned gamescope to report its PipeWire node. Authoritative source:
/// gamescope's own log line `stream available on node ID: N` (its node carries
/// `node.name=gamescope` on TWO objects — the adapter and the inner stream — and only the
/// advertised id is the correct capture target). Falls back, at the deadline, to `pw-dump`
/// discovery SCOPED to this spawn's process tree (`child`'s pid, A5), so a coexisting gamescope's
/// node is never mistaken for ours.
///
/// Takes the `Child` rather than a bare pid so it can **stop early when gamescope is already
/// dead**. A gamescope that fails `vkCreateDevice` exits in under a second, and polling its corpse
/// for the full 15 s bought nothing except a caller error that blamed the wrong thing ("headless
/// capture is unsupported on this GPU/driver"). `try_wait` turns that into an immediate `None`
/// while the log — which the caller names in the same error — still holds the real reason.
pub(super) fn wait_for_node(
    timeout: Duration,
    log: &std::path::Path,
    child: &mut Child,
) -> Option<u32> {
    let child_pid = child.id();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = node_from_log(log) {
            return Some(id);
        }
        // Check for a node FIRST, then for death: a gamescope that published its node and then
        // exited in the same tick still gives us the id, and the caller's own liveness handling
        // (the keepalive `Child`, `kept_display_alive`) owns what happens next.
        match child.try_wait() {
            // Still running — keep waiting.
            Ok(None) => {}
            // Exited. One last scoped look (the node line may have been written between the two
            // reads above), then give up rather than poll a corpse to the deadline.
            Ok(Some(status)) => {
                tracing::warn!(
                    pid = child_pid,
                    %status,
                    log = %log.display(),
                    "gamescope: the spawned process exited before publishing a PipeWire node — \
                     not waiting out the rest of the budget"
                );
                return node_from_log(log).or_else(|| find_gamescope_node_scoped(Some(child_pid)));
            }
            // `try_wait` itself failed (the child was reaped elsewhere, ECHILD): fall back to the
            // old behaviour rather than inventing a death.
            Err(_) => {}
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
    let Ok(out) = crate::proc::output_within(
        Command::new("pw-dump").arg(node_id.to_string()),
        PW_DUMP_BUDGET,
    ) else {
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
    let out = crate::proc::output_within(&mut Command::new("pw-dump"), PW_DUMP_BUDGET).ok()?;
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
    // Under the shared env lock: `session::apply_session_env` `set_var`s XDG_RUNTIME_DIR from the
    // connect thread, and glibc's setenv/getenv pair is a data race the crate's own `lib.rs`
    // documents as UB. The lock is not reentrant, so this must stay a read taken HERE and not
    // hoisted into a caller — the only caller, `point_injector_at_eis`, holds nothing (its
    // `ei_socket_file()` takes and releases the same lock separately).
    let runtime = crate::with_env_lock(|| std::env::var("XDG_RUNTIME_DIR").ok())?;
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
    crate::proc::output_within(
        Command::new(gamescope_bin()).arg("--version"),
        VERSION_PROBE_BUDGET,
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// The gamescope binary this host spawns, resolved ONCE per process:
///
/// 1. `PUNKTFUNK_GAMESCOPE_BIN` — an absolute path override (an operator's own build),
/// 2. `punktfunk-gamescope` on `PATH` — our carried build (`packaging/gamescope`), which adds
///    10-bit BT.2020/PQ capture formats to gamescope's PipeWire node,
/// 3. `gamescope` — the distro's.
///
/// Resolved to an ABSOLUTE path whenever it can be (`which`-style `PATH` walk), because the
/// same answer has to be baked into the two indirect spawn paths — the `GAMESCOPE_BIN` wrapper
/// (gamescope-session-plus) and the SteamOS PATH shim — which run outside this process's `PATH`.
/// A resolution failure falls back to the bare name so a normal install still works.
pub(crate) fn gamescope_bin() -> &'static str {
    static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        // The env override is read under the shared env lock (a concurrent session's `set_var`
        // must not race it) — same discipline as every other env read in this crate.
        let over = crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_BIN").ok())
            .filter(|s| !s.trim().is_empty());
        if let Some(path) = over {
            tracing::info!(bin = %path, "gamescope: PUNKTFUNK_GAMESCOPE_BIN override");
            return path;
        }
        if let Some(path) = which_in_path("punktfunk-gamescope") {
            tracing::info!(
                bin = %path,
                "gamescope: using the punktfunk build (10-bit HDR capture available)"
            );
            return path;
        }
        which_in_path("gamescope").unwrap_or_else(|| "gamescope".to_string())
    })
    .as_str()
}

/// Minimal `which`: the first executable `<dir>/<name>` across `PATH`. No crate dependency for
/// three lines of `access(2)`-shaped logic, and it keeps [`gamescope_bin`] honest about whether
/// our build is actually installed (a bare name would "resolve" and then fail at spawn).
fn which_in_path(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::with_env_lock(|| std::env::var("PATH").ok())?;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let cand = std::path::Path::new(dir).join(name);
        let Ok(md) = std::fs::metadata(&cand) else {
            continue;
        };
        if md.is_file() && md.permissions().mode() & 0o111 != 0 {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// The punktfunk patch-set revision the resolved gamescope carries — the `+pfhdr<N>` marker our
/// build stamps into the `--version` banner (`packaging/gamescope/README.md`). `None` for a stock
/// gamescope.
///
/// **A static, binary-identity answer, cached for the process**, and it has to be: punktfunk
/// fixes a session's shape before the display exists — the bit depth in the Welcome, which is
/// irrevocable (PQ frames handed to an 8-bit encoder are a deliberate hard error), and whether
/// the host must composite the cursor itself, which is decided before the encoder is opened.
/// Optimistic "spawn it and see" would strand the session either way. So we ask the binary once
/// and believe it.
///
/// Monotonic, so one probe answers every capability:
/// * `1` — 10-bit BT.2020/PQ capture formats ([`gamescope_hdr_capable`]);
/// * `2` — …and `--pipewire-composite-cursor` ([`gamescope_can_composite_cursor`]);
/// * `3` — …and `--custom-refresh-rates` ([`gamescope_can_offer_refresh_rates`]);
/// * `4` — …and `--pipewire-composite-external-overlay`
///   ([`gamescope_can_composite_external_overlay`]).
///
/// When upstream takes the functional patches this becomes a plain version floor, exactly like
/// [`MIN_GAMESCOPE_OVERLAY`].
fn gamescope_patch_level() -> u32 {
    static LEVEL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        let Ok(out) = crate::proc::output_within(
            Command::new(gamescope_bin()).arg("--version"),
            VERSION_PROBE_BUDGET,
        ) else {
            return 0;
        };
        // The banner goes to stderr on some builds, stdout on others (same as the version gate).
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let level = parse_patch_level(&text);
        if level > 0 {
            tracing::info!(
                bin = %gamescope_bin(),
                level,
                "gamescope carries the punktfunk patch set — HDR capture, and (level 2+) the \
                 cursor composited into the capture stream"
            );
        } else {
            // INFO, not DEBUG: this is the whole reason a box streams SDR, and the branch above
            // announces the good news at INFO. A field report ("HDR stopped working after the
            // update") cost a deep dive because the handshake's `capture_supports_hdr=false` was
            // visible at INFO while the ONE line saying why sat a level below it. Fires once per
            // process — the answer is cached in `LEVEL`.
            tracing::info!(
                bin = %gamescope_bin(),
                "gamescope has no {PFHDR_MARKER} marker — sessions on this backend stay 8-bit SDR \
                 with a host-composited cursor (install punktfunk-gamescope for HDR)"
            );
        }
        level
    })
}

/// Does the resolved gamescope offer 10-bit BT.2020/PQ formats on its PipeWire node — i.e. can a
/// session on this host stream true HDR10 off a gamescope virtual output?
pub(crate) fn gamescope_hdr_capable() -> bool {
    gamescope_patch_level() >= 1 && !flags_lost()
}

/// Can the resolved gamescope paint the pointer INTO its PipeWire node
/// (`--pipewire-composite-cursor`)? When it can, the host stops reconstructing the cursor from
/// XFixes and blending it in — which is what frees the session to take the encoder's zero-CSC
/// RGB-direct source, since that front end has no blend stage.
pub(crate) fn gamescope_can_composite_cursor() -> bool {
    gamescope_patch_level() >= 2 && !flags_lost()
}

/// Does the resolved gamescope actually PUBLISH the keymap it compiles from `XKB_DEFAULT_*`?
///
/// Below this level it reads the five variables, builds the keymap, and then hands its clients
/// nothing: the seat carries a keymap-less stub keyboard that `wlserver_keyboardfocus()` re-binds
/// on every focus change, so Xwayland and every Wayland client keep their own built-in `us`. A
/// headless session has no libinput devices to ever put the real keymap on the seat, so it never
/// recovers. The keys punktfunk injects are US-POSITIONAL and the session's keymap is what turns
/// them into characters — so on a stock gamescope a German keyboard types its US neighbours
/// (`#`→`\`, `ä`→`'`, `-`→`/`) no matter how the box is configured.
pub(crate) fn gamescope_honours_xkb_env() -> bool {
    gamescope_patch_level() >= 8 && !flags_lost()
}

/// Does the resolved gamescope let us hand a headless session the list of refresh rates it may
/// offer (`--custom-refresh-rates`)?
///
/// Below this level a headless gamescope advertises **one** rate — whatever `--nested-refresh`
/// resolved to, or its own 60 Hz default — and no resolution list at all, because its connector
/// returns empty spans from `GetModes()`/`GetValidDynamicRefreshRates()` and reports an INTERNAL
/// screen (which makes `update_mode_atoms` delete the mode-list atom outright). So on a stock
/// gamescope, Steam's in-session display settings show exactly one refresh rate and no
/// resolutions, and games read the display as 60 Hz whatever the client negotiated.
///
/// `gamescope-session-plus` has probed for this flag for years (`CUSTOM_REFRESH_RATES` is gated on
/// `gamescope --help` mentioning it) — upstream simply never had it, so the env var it plumbs was
/// a no-op everywhere.
pub(crate) fn gamescope_can_offer_refresh_rates() -> bool {
    gamescope_patch_level() >= 3 && !flags_lost()
}

/// Can the resolved gamescope paint the EXTERNAL OVERLAY — mangoapp, the Deck-UI fps/frametime
/// readout — into its PipeWire node (`--pipewire-composite-external-overlay`)?
///
/// `paint_pipewire` has never referenced that layer on any upstream version, so a client whose
/// only view of the session is the node sees the overlay it just enabled simply not appear.
/// Unlike the cursor there is no host-side substitute: the host cannot reconstruct someone else's
/// overlay window.
pub(crate) fn gamescope_can_composite_external_overlay() -> bool {
    gamescope_patch_level() >= 4 && !flags_lost()
}

/// Does the resolved gamescope paint — and publish to its PipeWire node — on the game's COMMIT
/// when adaptive sync is requested, instead of sampling on its synthetic vblank tick?
///
/// Below this level `--adaptive-sync` is inert on a headless session (the connector advertises no
/// VRR), so the compose timer quantizes game presents onto the `-r` grid: a frame finished just
/// after the tick's sample waits out a full period, and the stream delivers measurably fewer
/// unique frames than the session rate — the shortfall `PUNKTFUNK_VDISPLAY_HZ_MULT` papers over
/// by making the game render every frame twice. Also below this level a `--framerate-limit`
/// equal to the refresh rate is skipped as "close enough" to the display's own pacing — under
/// VRR that limiter is the only pace the game has left, which is why the two flags only ever
/// travel together (the `adaptive_sync_args` builder).
pub(crate) fn gamescope_paints_on_commit() -> bool {
    gamescope_patch_level() >= 9 && !flags_lost()
}

/// Has a spawn been observed where our flags did NOT reach the gamescope process?
///
/// The binary probe above answers "can it", which is all the bare spawn needs — there we build
/// argv ourselves. The two INDIRECT modes can't promise that much: a host-managed
/// `gamescope-session-plus` gets the flags via `GAMESCOPE_BIN` + `PF_HDR_ARGS`, and SteamOS via a
/// PATH shim, and a session free to ignore either would exec the distro's gamescope instead. Then
/// the binary is still capable and the running compositor still has none of the flags.
///
/// So a capability that was only ever *probed* becomes one that has been *contradicted*, and this
/// latch is how the contradiction sticks: [`note_spawn_flags_lost`] sets it, and from then on both
/// answers above are `false` for the rest of the process. The observing spawn fails — the plan it
/// was created under is already wrong and cannot be edited mid-create — and the retry re-resolves
/// against the latched answers, landing on a correct SDR host-composited session. One failed
/// attempt per boot, then it converges.
fn flags_lost() -> bool {
    FLAGS_LOST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record that a spawned session's gamescope did not receive the flags we passed it — see
/// [`flags_lost`]. Idempotent and one-way: nothing ever clears it, because nothing we can observe
/// proves the next session would fare better.
pub(crate) fn note_spawn_flags_lost() {
    FLAGS_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
}

static FLAGS_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The marker `packaging/gamescope/patches/0003-*` stamps into the `--version` banner, followed by
/// the patch-set revision (`+pfhdr1`, `+pfhdr2`, …).
const PFHDR_MARKER: &str = "+pfhdr";

/// The `+pfhdr<N>` revision in a `--version` banner, or `0` for a stock gamescope. Split out pure
/// because everything downstream is a `>=` on it: read it too low and a capable box silently loses
/// HDR; too high and the host promises a cursor nobody paints.
fn parse_patch_level(banner: &str) -> u32 {
    banner
        .find(PFHDR_MARKER)
        .map(|i| &banner[i + PFHDR_MARKER.len()..])
        .map(|tail| {
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// The upstream `X.Y.Z` a specific gamescope binary reports, or `None` if it cannot be run/parsed.
///
/// Split from [`check_gamescope_version`] (which only ever probes the RESOLVED binary) because the
/// WSI-layer check has to compare TWO binaries — ours and the distro's — and a `None` there means
/// "leave the layer alone", not "assume old".
pub(super) fn gamescope_version_of(bin: &std::path::Path) -> Option<(u32, u32, u32)> {
    let out = crate::proc::output_within(Command::new(bin).arg("--version"), VERSION_PROBE_BUDGET)
        .ok()?;
    // Same stdout/stderr split as the version gate: builds disagree on where the banner goes.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_version(&text)
}

/// Minimum gamescope that captures reliably: below 3.16.22, headless PipeWire capture deadlocks
/// against PipeWire ≥ 1.6 (a loop-lock bug) and a stuck link head-blocks the whole daemon.
const MIN_GAMESCOPE: (u32, u32, u32) = (3, 16, 22);

/// First gamescope that paints the Steam overlay (Shift+Tab / Quick Access Menu) into its built-in
/// PipeWire node. `paint_pipewire()` is a *separate, reduced* composite from the display scanout;
/// the overlay-window paint (gated on the consumer negotiating `gamescope_focus_appid == 0`, which
/// we do by never advertising that property — see the capturer's EnumFormat builders) first ships
/// in 3.16.23 (gamescope commits `ccd62074` + `f8b33d38`). Below this the overlay is *never* in the
/// node, so it cannot appear in the stream no matter what the host does.
///
/// On a **stock** gamescope the cursor and external-overlay / notification layers are excluded from
/// `paint_pipewire` on every version, and the host handles the cursor itself. punktfunk's own build
/// puts both back: `--pipewire-composite-cursor` at patch level 2+
/// ([`gamescope_can_composite_cursor`], which is what suppresses the host-side blend) and
/// `--pipewire-composite-external-overlay` at 4+ ([`gamescope_can_composite_external_overlay`]) —
/// see [`gamescope_patch_level`]. So "the overlay is missing from the stream" is a question about
/// which flags reached the running compositor, not about host-side compositing.
const MIN_GAMESCOPE_OVERLAY: (u32, u32, u32) = (3, 16, 23);

/// Best-effort: warn if the installed gamescope is older than [`MIN_GAMESCOPE`] (capture is
/// unreliable) or than [`MIN_GAMESCOPE_OVERLAY`] (capture works but the Steam overlay can't reach
/// the stream). Parsing failures are silent (don't block a possibly-fine custom build) — this is a
/// diagnostic, not a gate. Returns the parsed version when it could read one.
pub(super) fn check_gamescope_version() -> Option<(u32, u32, u32)> {
    let out = crate::proc::output_within(
        Command::new(gamescope_bin()).arg("--version"),
        VERSION_PROBE_BUDGET,
    )
    .ok()?;
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
    use super::{
        parse_patch_level, parse_version, steam_appid_from_launch, MIN_GAMESCOPE,
        MIN_GAMESCOPE_OVERLAY,
    };

    /// The `+pfhdr<N>` probe decides, before anything is spawned, whether a session may negotiate
    /// HDR (level 1) and whether the host must composite the pointer itself (level 2). Both are
    /// irreversible once the session is planned, so the parse has to be exact in both directions:
    /// too low silently costs a capable box its HDR, too high promises a cursor nobody paints.
    #[test]
    fn patch_level_parses_the_marker_and_nothing_else() {
        // The real banner shape: `git describe` output, our marker, then the compiler.
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25-1-g8c676c3+pfhdr2 (gcc 15.2.0)"),
            2
        );
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25+pfhdr1 (clang 20)"),
            1
        );
        // A stock gamescope — every capability off.
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25 (gcc 15.2.0)"),
            0
        );
        assert_eq!(parse_patch_level(""), 0);
        // Multi-digit revisions must not truncate to their first digit.
        assert_eq!(parse_patch_level("3.16.25+pfhdr10 (gcc)"), 10);
        // A marker with no number is not a capability claim.
        assert_eq!(parse_patch_level("3.16.25+pfhdr (gcc)"), 0);
        // The version triple must never be mistaken for the level.
        assert_eq!(parse_patch_level("gamescope version 3.16.25"), 0);
    }

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
