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
//! Input uses gamescope's own libei/EIS socket (`LIBEI_SOCKET`), relayed to the libei backend (see
//! `inject/libei.rs`) — wired and live-validated.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The gamescope virtual-display driver. Three modes by env, in precedence order:
/// * `PUNKTFUNK_GAMESCOPE_SESSION=<client>` — host-MANAGE a `gamescope-session-plus` session
///   (full Steam-Deck-UI polish) headless at the CLIENT's mode; relaunch it when the mode changes.
/// * `PUNKTFUNK_GAMESCOPE_NODE=<id|auto>` — ATTACH to an already-running gamescope (capture +
///   inject, no lifecycle ownership).
/// * else — SPAWN a bare headless gamescope sized to the mode, running `PUNKTFUNK_GAMESCOPE_APP`.
#[derive(Default)]
pub struct GamescopeDisplay {
    /// The resolved per-session launch command (set via [`VirtualDisplay::set_launch_command`]); the
    /// bare-spawn path runs it instead of reading the process-global `PUNKTFUNK_GAMESCOPE_APP`.
    cmd: Option<String>,
}

/// A running host-managed session (its transient systemd --user unit) + the mode it was launched at.
struct SessionState {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

/// The host-managed `gamescope-session-plus` session, tracked at **host lifetime** (NOT per
/// `GamescopeDisplay`, which is recreated per client session and would otherwise cold-start Steam on
/// every reconnect). A same-mode reconnect reuses the running session (no Steam restart); a
/// different mode relaunches it. Cleared/relaunched by `launch_session`; survives across client
/// connections; on host restart the next launch stops the leftover unit by name and starts fresh.
static MANAGED_SESSION: std::sync::Mutex<Option<SessionState>> = std::sync::Mutex::new(None);

/// Autologin gaming-mode `gamescope-session-plus@*` units we stopped on connect to free Steam
/// (single-instance), so [`schedule_restore_tv_session`] can restart them when the client disconnects.
static STOPPED_AUTOLOGIN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A pending debounced TV-session restore: the instant [`do_restore_tv_session`] should fire after
/// the last client disconnect. A reconnect inside the window clears it (and reuses the still-warm
/// managed session), so we never stop+relaunch gamescope per connect — that per-connect teardown is
/// what leaked NVIDIA GPU context on F44 (the black-screen reconnect). Driven by the host-lifetime
/// [`start_restore_worker`] thread.
static PENDING_RESTORE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// How long to wait after the last disconnect before restoring the TV's autologin gaming session —
/// long enough that a quick reconnect (e.g. a controller hiccup) reuses the warm managed session
/// instead of triggering a stop/relaunch.
const RESTORE_DEBOUNCE: Duration = Duration::from_secs(5);

/// Per-spawn instance counter (A5): each bare-spawn gets a unique id addressing its own log so two
/// coexisting gamescopes (a kept lingering spawn + a fresh one) never parse each other's node id.
static SPAWN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// This spawn instance's log path, under `$XDG_RUNTIME_DIR` (per-user, tmpfs; falls back to `/tmp`
/// only if unset). Replaces the shared `/tmp/punktfunk-gamescope.log` so concurrent spawns don't
/// clobber each other's `stream available on node ID:` line.
fn spawn_log_path(inst: u64) -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&base).join(format!("punktfunk-gamescope-{inst}.log"))
}

/// systemd --user transient unit name for the host-managed gamescope-session-plus session.
const SESSION_UNIT: &str = "punktfunk-gamescope";
/// The gamescope-session-plus launcher script (Bazzite / SteamOS-like hosts).
const SESSION_PLUS_BIN: &str = "/usr/share/gamescope-session-plus/gamescope-session-plus";

/// The ACTUAL Steam Deck (SteamOS) ships its OWN session — NOT Bazzite's session-plus. It's the
/// systemd-user `gamescope-session.target`, whose `gamescope-session.service` runs this script, which
/// `exec gamescope`s with HARDCODED physical-panel args (`-w 1280 -h 800 -O '*',eDP-1`) and launches
/// Steam via a SEPARATE `steam-launcher.service`. To honor the client's mode we (a) drop a `gamescope`
/// PATH-shim that rewrites those args to `--backend headless -W <client> …`, and (b) write a transient
/// user drop-in pointing the service's PATH at the shim + the mode, then restart the whole target —
/// so `steam-launcher.service` brings Steam up IN the headless gamescope at the client's resolution.
const STEAMOS_SESSION_BIN: &str = "/usr/lib/steamos/gamescope-session";
const STEAMOS_SESSION_TARGET: &str = "gamescope-session.target";

/// Set once we've reconfigured SteamOS's `gamescope-session.target` headless for a stream — the
/// SteamOS analogue of [`STOPPED_AUTOLOGIN`], so the restore path knows to remove the drop-in and
/// restart the physical session.
static STEAMOS_TOOK_OVER: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Persisted takeover state (`design/gamemode-and-dedicated-sessions.md` A3): the takeover mechanics
/// ([`STOPPED_AUTOLOGIN`] / [`STEAMOS_TOOK_OVER`]) are process memory, so a host **crash** mid-stream
/// would strand the box out of gaming mode with no restore. Mirroring the statics to a file lets
/// [`restore_takeover_on_startup`] put the TV back after a restart.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct TakeoverState {
    /// Autologin `gamescope-session-plus@*.service` units we stopped (to restart on restore).
    stopped_autologin: Vec<String>,
    /// Whether we took over SteamOS's `gamescope-session.target` (restore = remove drop-in + restart).
    steamos: bool,
}

/// Path of the persisted [`TakeoverState`], under `$XDG_RUNTIME_DIR` (per-user, 0700, tmpfs — cleared
/// on reboot, which is correct: a reboot restarts the autologin itself).
fn takeover_state_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&base).join("punktfunk-session-takeover.json")
}

/// Persist the current takeover mechanics so a host crash doesn't strand the box out of gaming mode.
/// Best-effort (a write failure just loses crash-restore, not correctness).
fn persist_takeover() {
    let state = TakeoverState {
        stopped_autologin: STOPPED_AUTOLOGIN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        steamos: *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()),
    };
    if state.stopped_autologin.is_empty() && !state.steamos {
        clear_takeover();
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = std::fs::write(takeover_state_path(), bytes);
    }
}

/// Remove the persisted takeover file (after a completed restore, or when there's nothing to restore).
fn clear_takeover() {
    let _ = std::fs::remove_file(takeover_state_path());
}

/// On host startup, restore the TV's gaming session if a previous host instance took it over and
/// crashed before restoring (`design/gamemode-and-dedicated-sessions.md` A3). Loads the persisted
/// [`TakeoverState`] into the statics and schedules a restore after a short reconnect grace (so a
/// client reconnecting right after the restart keeps the streamed session instead of bouncing the
/// box back to gaming mode). No-op when no takeover file exists (a clean start). Call once from
/// `serve` alongside [`start_restore_worker`].
pub fn restore_takeover_on_startup() {
    let Ok(bytes) = std::fs::read(takeover_state_path()) else {
        return; // no takeover file — clean start
    };
    let Ok(state) = serde_json::from_slice::<TakeoverState>(&bytes) else {
        clear_takeover();
        return;
    };
    if state.stopped_autologin.is_empty() && !state.steamos {
        clear_takeover();
        return;
    }
    tracing::warn!(
        units = ?state.stopped_autologin,
        steamos = state.steamos,
        "gamescope: found a stranded takeover from a previous host instance — scheduling TV restore"
    );
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_autologin;
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = state.steamos;
    // A generous grace so a client reconnecting right after the restart cancels it (create_managed_session
    // clears PENDING_RESTORE) and keeps the streamed session rather than bouncing to gaming mode.
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Instant::now() + Duration::from_secs(15));
}

impl GamescopeDisplay {
    pub fn new() -> Result<Self> {
        Ok(GamescopeDisplay::default())
    }
}

impl VirtualDisplay for GamescopeDisplay {
    fn name(&self) -> &'static str {
        "gamescope"
    }

    fn set_launch_command(&mut self, cmd: Option<String>) {
        self.cmd = cmd;
    }

    fn poolable_now(&self) -> bool {
        // Only a bare SPAWN is registry-poolable (its `create` reports `Owned`); managed
        // (`PUNKTFUNK_GAMESCOPE_SESSION`) and attach (`PUNKTFUNK_GAMESCOPE_NODE`) report
        // `SessionManaged`/`External`, so the registry must not reuse a kept spawn for them (same
        // backend name). Mirrors [`crate::vdisplay::launch_is_nested`]; read under the env lock the
        // sub-mode ladder writes these keys under.
        crate::vdisplay::with_env_lock(|| {
            std::env::var_os("PUNKTFUNK_GAMESCOPE_SESSION").is_none()
                && std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_none()
        })
    }

    fn launch_command(&self) -> Option<String> {
        // The registry keys keep-alive reuse on (backend, mode, launch): a kept bare-spawn running
        // game A must never be reused for a session launching game B (A2).
        self.cmd.clone()
    }

    fn kept_display_alive(&mut self, node_id: u32) -> bool {
        // The nested gamescope dies when its game exits (independently of any compositor), leaving a
        // dead pooled node. Before the registry reuses that node on a reconnect, confirm it still
        // exists on the daemon; a `false` makes the registry recreate instead of handing back a corpse
        // (which would then burn a ~10 s first-frame retry before `mark_failed` recovered it).
        gamescope_node_present(node_id)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Host-managed gamescope-session-plus at the CLIENT's mode (the Bazzite path): launch the
        // full Steam-Deck-UI session headless at the client's resolution + refresh — so games SEE
        // them (via the injected --nested-refresh + generated CVT modes, not the box's TV EDID) —
        // and relaunch it when the client's mode changes. Reuses the node + EIS discovery below.
        if let Ok(client) = std::env::var("PUNKTFUNK_GAMESCOPE_SESSION") {
            return create_managed_session(&client, mode);
        }
        // Attach to an already-running gamescope (a foreign / externally-launched session) instead
        // of spawning our own: capture its node AND inject into its EIS socket.
        // PUNKTFUNK_GAMESCOPE_NODE=<id|auto>; "auto" discovers the gamescope `Video/Source` node.
        if let Ok(id) = std::env::var("PUNKTFUNK_GAMESCOPE_NODE") {
            let node_id: u32 = if id.trim().eq_ignore_ascii_case("auto") {
                // Attach to the box-owned game-mode session, but FIRST make it run at the connecting
                // client's resolution (the box is headless, so its game-mode mode is ours to set).
                // Reuse if it already matches (fast, no restart); otherwise relaunch the box's own
                // session at the client mode. Without this the client gets the box's default mode.
                ensure_box_gamescope_mode(mode)?
            } else {
                id.parse()
                    .context("PUNKTFUNK_GAMESCOPE_NODE must be a node id or 'auto'")?
            };
            point_injector_at_eis();
            tracing::info!(node_id, "gamescope: attaching to existing PipeWire node");
            // ATTACH = mirror a foreign gamescope we don't own → External (no keep-alive/reuse).
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
                keepalive: Box::new(()),
                ownership: DisplayOwnership::External,
                reused_gen: None,
                pool_gen: None,
            });
        }
        check_gamescope_version(); // diagnostic only — warns on known-deadlock-prone versions
                                   // B1: a dedicated STEAM launch needs Steam's single instance free. If the box autologged into
                                   // game mode (Bazzite) its Steam holds the instance, and a nested second Steam would see the
                                   // first and exit (crashing the spawn) — so free the autologin session first. Its restore is the
                                   // A3 takeover machinery (recorded in STOPPED_AUTOLOGIN + persisted; restarted on session end via
                                   // schedule_restore_tv_session). Non-Steam launches don't conflict, so they skip this.
        if self.cmd.as_deref().is_some_and(is_steam_launch) {
            stop_autologin_sessions();
            // B1b: a Steam running in a plain DESKTOP session (GNOME/KDE) holds the instance just
            // the same, and the autologin stop above can't see it — free it too, or fail loudly.
            free_desktop_steam()?;
        }
        // A5: a per-spawn instance id addresses this spawn's log + node discovery, so two coexisting
        // bare-spawns (a kept lingering one + a fresh one) never parse each other's node id from a
        // shared log. The nested-command's LIBEI relay stays on the global path (per-instance input
        // isolation is `design/gamescope-multiuser.md` scope, not addressed here).
        let inst = SPAWN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log = spawn_log_path(inst);
        let child = spawn(
            mode.width,
            mode.height,
            mode.refresh_hz.max(1),
            self.cmd.as_deref(),
            &log,
        )?;
        let child_pid = child.id();
        let proc = GamescopeProc {
            child,
            log: log.clone(),
        };
        // gamescope creates its PipeWire node a moment after start; poll for it (the proc is held
        // alive meanwhile, and killed if we give up). Discovery reads THIS spawn's log, and the
        // fallback is scoped to this spawn's process tree.
        let node_id = wait_for_node(Duration::from_secs(15), &log, child_pid).ok_or_else(|| {
            anyhow!(
                "gamescope PipeWire node did not appear within 15s — gamescope may have failed to \
                 start or headless capture is unsupported on this GPU/driver (see {})",
                log.display()
            )
        })?;
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "gamescope virtual output ready"
        );
        // Bare SPAWN: we own the nested gamescope process → registry-poolable (keep-alive-able).
        Ok(VirtualOutput::owned(
            node_id,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(proc),
        ))
    }
}

/// Host-managed `gamescope-session-plus` at the client's mode (state in [`MANAGED_SESSION`], so it
/// persists across client connections — a reconnect at the same mode reuses it instantly). REUSE
/// the running session if the mode is unchanged and its node is still live (no Steam restart);
/// otherwise stop the old transient unit and RELAUNCH at the new mode (gamescope can't change output
/// mode live). Then discover the node + point the injector, exactly as the attach path does.
fn create_managed_session(client: &str, mode: Mode) -> Result<VirtualOutput> {
    // A (re)connect cancels any pending debounced TV-restore: we're about to (re)use the managed
    // session, so the autologin must stay stopped and the warm session stays up (no stop/relaunch).
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // SteamOS (the real Steam Deck) has no session-plus: take over its `gamescope-session.target`
    // headless at the client's mode instead of launching a separate managed session.
    if steamos_session_present() {
        return create_managed_session_steamos(mode);
    }
    // Steam is single-instance: if the box autologged into gaming mode on a physical display (the
    // Bazzite default — `gamescope-session-plus@ogui-steam` on the TV), that session holds Steam and
    // renders to the TV's native mode, which we'd capture instead of the client's. Free Steam by
    // stopping it; [`schedule_restore_tv_session`] (on disconnect) brings it back after a debounce.
    stop_autologin_sessions();
    // B1b: a desktop-session Steam (outside any gamescope unit) also holds the single instance and
    // would make the managed session's own Steam exit at birth. The managed session's Steam itself
    // is exempt (it lives in the SESSION_UNIT cgroup), so the same-mode reuse below is unaffected.
    free_desktop_steam()?;
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width && s.height == mode.height && s.refresh_hz == mode.refresh_hz
    });
    if same_mode {
        if let Some(node_id) = find_gamescope_node() {
            point_injector_at_eis();
            tracing::info!(
                node_id,
                w = mode.width,
                h = mode.height,
                hz = mode.refresh_hz,
                "gamescope session: reusing the running session (same mode — no Steam restart)"
            );
            return Ok(managed_output(node_id, mode));
        }
        tracing::warn!("gamescope session: tracked session has no live node — relaunching");
        *guard = None;
    }
    // (Re)launch at the new mode. `launch_session` stops the old unit by name first, so there is
    // exactly one gamescope `Video/Source` node for discovery.
    let node_id = launch_session(client, SESSION_UNIT, mode)?;
    point_injector_at_eis();
    *guard = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
    });
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope session: launched gamescope-session-plus at the client's mode"
    );
    Ok(managed_output(node_id, mode))
}

/// The [`VirtualOutput`] for a managed / SteamOS-takeover session: a box-level session whose restore
/// lifecycle is (at Part A1) the gamescope module's own machinery (`schedule_restore_tv_session`), so
/// it is [`DisplayOwnership::SessionManaged`] — the registry passes it through (no pooling), and the
/// capturer's unit keepalive tears nothing down on drop. (Part A3 replaces the unit keepalive with a
/// real `ManagedSessionHandle` and flips this to `Owned`.)
fn managed_output(node_id: u32, mode: Mode) -> VirtualOutput {
    VirtualOutput {
        node_id,
        remote_fd: None,
        preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
        keepalive: Box::new(()),
        ownership: DisplayOwnership::SessionManaged,
        reused_gen: None,
        pool_gen: None,
    }
}

/// SteamOS detection: its session launcher is present and Bazzite's session-plus is NOT (so the
/// drop-in / PATH-shim takeover applies rather than launching a separate session-plus unit).
fn steamos_session_present() -> bool {
    std::path::Path::new(STEAMOS_SESSION_BIN).exists()
        && !std::path::Path::new(SESSION_PLUS_BIN).exists()
}

/// Does this box have the infrastructure the MANAGED gamescope mode drives — Bazzite's
/// `gamescope-session-plus` or SteamOS's `gamescope-session`? The sub-mode ladder
/// ([`crate::vdisplay::apply_input_env`]) only defaults to managed when this is true; a plain
/// distro (neither present) falls through to the bare-spawn path instead of the old behaviour of
/// defaulting to managed and then bailing on the missing session script.
pub fn managed_session_available() -> bool {
    std::path::Path::new(SESSION_PLUS_BIN).exists()
        || std::path::Path::new(STEAMOS_SESSION_BIN).exists()
}

/// Is a gamescope WE DIDN'T SPAWN running for our uid right now? Used by the sub-mode ladder to
/// pick ATTACH (mirror the foreign session) over a bare spawn on a box without managed-session
/// infra. Our own per-session bare-spawn gamescopes are children of this host process — excluded by
/// walking each candidate's ppid chain — so one client's nested gamescope never makes the next
/// client attach to it.
pub fn foreign_gamescope_running() -> bool {
    // SAFETY: `getuid()` is a parameterless POSIX call that always succeeds and touches no memory.
    let uid = unsafe { libc::getuid() };
    let our_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let Ok(md) = std::fs::metadata(e.path()) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(e.path().join("comm")) else {
            continue;
        };
        if !matches!(comm.trim(), "gamescope" | "gamescope-wl") {
            continue;
        }
        if !descends_from(pid, our_pid) {
            return true;
        }
    }
    false
}

/// Is `pid` a descendant of (or equal to) `ancestor`? Walks the ppid chain via `/proc/<pid>/stat`
/// with a hop cap so a racing/exiting process can't loop us.
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Field 4 (ppid) follows the parenthesized comm — split after the LAST ')' since comm can
        // itself contain parentheses.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            return false;
        };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) else {
            return false;
        };
        pid = ppid;
    }
    false
}

/// Launch `cmd` INTO the live gamescope session (the managed / SteamOS / attach modes, where the
/// session already exists and [`spawn`]'s nesting doesn't apply). The child gets the session's own
/// `DISPLAY` (gamescope's Xwayland) and Wayland socket, discovered from a process already inside the
/// session — so X11 and Wayland clients alike land on the streamed gamescope output. Discovery is
/// best-effort: without it we still spawn with the host env and warn (a `steam steam://…` launch
/// still works there — the running Steam instance picks the URI up over its own pipe, no display
/// env needed).
pub fn launch_into_session(cmd: &str) -> Result<std::process::Child> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    match discover_session_display_env() {
        Some((x11, wayland)) => {
            tracing::info!(
                command = %cmd,
                x11_display = x11.as_deref().unwrap_or("-"),
                wayland = wayland.as_deref().unwrap_or("-"),
                "gamescope: launching into the live session"
            );
            if let Some(d) = x11 {
                c.env("DISPLAY", d);
            }
            if let Some(w) = wayland {
                c.env("WAYLAND_DISPLAY", w);
            }
        }
        None => tracing::warn!(
            command = %cmd,
            "gamescope: could not discover the session's display env — spawning with the host env \
             (a `steam steam://…` launch still reaches the running Steam; other apps may not land \
             in the session)"
        ),
    }
    c.spawn()
        .context("spawn launch command into gamescope session")
}

/// Find the live gamescope session's `(DISPLAY, WAYLAND_DISPLAY)` by scanning same-uid processes
/// for one whose environment carries `GAMESCOPE_WAYLAND_DISPLAY` (gamescope sets it for everything
/// it runs — Steam, the game, our own nested `sh`). The Wayland value returned is that gamescope
/// socket; `DISPLAY` is the nested Xwayland. Either can be individually absent.
fn discover_session_display_env() -> Option<(Option<String>, Option<String>)> {
    // SAFETY: `getuid()` is a parameterless POSIX call that always succeeds and touches no memory.
    let uid = unsafe { libc::getuid() };
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
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
        let Ok(raw) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        let mut display = None;
        let mut gs_wayland = None;
        for kv in raw.split(|&b| b == 0) {
            let kv = String::from_utf8_lossy(kv);
            if let Some(v) = kv.strip_prefix("GAMESCOPE_WAYLAND_DISPLAY=") {
                if !v.is_empty() {
                    gs_wayland = Some(v.to_string());
                }
            } else if let Some(v) = kv.strip_prefix("DISPLAY=") {
                if !v.is_empty() {
                    display = Some(v.to_string());
                }
            }
        }
        // Only a process INSIDE a gamescope session (it has the marker var) is a valid source.
        if gs_wayland.is_some() {
            return Some((display, gs_wayland));
        }
    }
    None
}

/// Run a `systemctl --user` subcommand best-effort — a failure just means the session won't change,
/// which the caller's node-wait surfaces.
fn systemctl_user(args: &[&str]) {
    let _ = Command::new("systemctl").arg("--user").args(args).status();
}

/// Directory holding the per-user `gamescope` PATH-shim (tmpfs under `XDG_RUNTIME_DIR`).
fn headless_shim_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&base).join("punktfunk-gsbin")
}

/// The gamescope arg-rewriting shim. SteamOS hardcodes physical-panel args, so we intercept the
/// session's `exec gamescope` (via PATH) and rewrite to a headless output at the client's mode (read
/// from `PF_W`/`PF_H`/`PF_HZ`), dropping the physical flags. Idempotent; returns the shim's directory.
fn write_headless_shim() -> Result<std::path::PathBuf> {
    const SHIM_BODY: &str = r#"#!/bin/bash
W="${PF_W:-1920}"; H="${PF_H:-1080}"; HZ="${PF_HZ:-60}"
keep=()
while [ $# -gt 0 ]; do
  case "$1" in
    --generate-drm-mode|-w|-h|-W|-H|-O|--prefer-output) shift 2;;
    *) keep+=("$1"); shift;;
  esac
done
exec /usr/bin/gamescope --backend headless -W "$W" -H "$H" -w "$W" -h "$H" -r "$HZ" "${keep[@]}"
"#;
    let dir = headless_shim_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let shim = dir.join("gamescope");
    std::fs::write(&shim, SHIM_BODY).with_context(|| format!("write shim {}", shim.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod shim {}", shim.display()))?;
    Ok(dir)
}

/// Path of the transient user drop-in that points `gamescope-session.service` at the shim + mode.
/// `zz-` so it sorts last (overrides any distro drop-in).
fn steamos_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session.service.d/zz-punktfunk-headless.conf")
}

/// Write the drop-in: prepend the shim dir to the service's PATH + pass the client's mode via `PF_*`.
/// A subsequent `daemon-reload` + target restart applies it.
fn write_steamos_dropin(shim_dir: &std::path::Path, mode: Mode) -> Result<()> {
    let path = steamos_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "[Service]\n\
         Environment=PATH={shim}:/usr/bin:/bin:/usr/local/bin\n\
         Environment=PF_W={w}\n\
         Environment=PF_H={h}\n\
         Environment=PF_HZ={hz}\n",
        shim = shim_dir.display(),
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz.max(1),
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))
}

/// Remove the headless drop-in (restore-on-disconnect). Best-effort.
fn remove_steamos_dropin() {
    let _ = std::fs::remove_file(steamos_dropin_path());
}

/// Take over SteamOS's `gamescope-session.target` headless at the CLIENT's mode: write the shim + a
/// drop-in carrying the mode, `daemon-reload`, then RESTART the target so `steam-launcher.service`
/// brings Steam up in the fresh headless gamescope — and attach to its node. A same-mode reconnect
/// reuses the running session (no Steam restart); a different mode rewrites the drop-in + restarts.
/// The restart kills any prior gamescope, so there's exactly one node to discover (no stale attach).
fn create_managed_session_steamos(mode: Mode) -> Result<VirtualOutput> {
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width && s.height == mode.height && s.refresh_hz == mode.refresh_hz
    });
    if same_mode {
        if let Some(node_id) = find_gamescope_node() {
            point_injector_at_eis();
            tracing::info!(
                node_id,
                w = mode.width,
                h = mode.height,
                hz = mode.refresh_hz,
                "gamescope (SteamOS): reusing the headless session (same mode — no Steam restart)"
            );
            return Ok(managed_output(node_id, mode));
        }
        *guard = None; // tracked session lost its node — fall through to a clean restart
    }
    let shim_dir = write_headless_shim()?;
    write_steamos_dropin(&shim_dir, mode)?;
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", STEAMOS_SESSION_TARGET]);
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = true;
    persist_takeover(); // A3: survive a host crash mid-stream
                        // gamescope's node appears within a few seconds of the restart; Steam's first FRAME is slower
                        // (Big Picture cold start) and is awaited by the caller's first-frame retry loop. The managed
                        // session logs to journald (not a per-spawn file), so poll `find_gamescope_node` directly.
    let node_id = poll_managed_node(Duration::from_secs(30)).ok_or_else(|| {
        anyhow!(
            "SteamOS headless gamescope node did not appear within 30s after restarting \
             {STEAMOS_SESSION_TARGET} — check `journalctl --user -u gamescope-session.service`"
        )
    })?;
    point_injector_at_eis();
    *guard = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
    });
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope (SteamOS): took over gamescope-session.target headless at the client's mode"
    );
    Ok(managed_output(node_id, mode))
}

/// ATTACH at the CLIENT's resolution: ensure the box's own game-mode session is running at `mode`'s
/// output size, then return its capture node. Reuses the running session if it already matches (no
/// restart — the rock-solid fast path a stable client always hits); otherwise reconfigures + restarts
/// the box's OWN autologin `gamescope-session-plus@<client>` unit at the client mode. Restarting the
/// box's own unit (rather than spawning a competing one) avoids the autologin-respawn fight the old
/// MANAGED path hit. A headless box has no physical panel, so its game-mode resolution is ours to set;
/// Steam restarts only on an actual resolution CHANGE.
fn ensure_box_gamescope_mode(mode: Mode) -> Result<u32> {
    let target = (mode.width, mode.height);
    // Fast path: already at the client's resolution — just attach to the live node.
    if current_gamescope_output_size() == Some(target) {
        if let Some(node) = find_gamescope_node() {
            tracing::info!(
                w = mode.width,
                h = mode.height,
                node,
                "gamescope: box game-mode session already at the client's resolution — reusing"
            );
            return Ok(node);
        }
    }
    let Some(unit) = running_autologin_gamescope_unit() else {
        // No box-owned autologin session to reconfigure (a bare/foreign gamescope): attach to
        // whatever node exists, accepting its resolution.
        return find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "no running gamescope Video/Source node — is the headless game mode up? \
                 (put the box into Steam Game Mode)"
            )
        });
    };
    tracing::info!(
        from = ?current_gamescope_output_size(),
        to_w = mode.width,
        to_h = mode.height,
        hz = mode.refresh_hz,
        %unit,
        "gamescope: relaunching the box game-mode session at the client's resolution"
    );
    // The session reads SCREEN_WIDTH/HEIGHT (+ CUSTOM_REFRESH_RATES) from the user-manager
    // environment; set them and restart the box's own unit.
    systemctl_user(&[
        "set-environment",
        &format!("SCREEN_WIDTH={}", mode.width),
        &format!("SCREEN_HEIGHT={}", mode.height),
        &format!("CUSTOM_REFRESH_RATES={}", mode.refresh_hz.max(1)),
    ]);
    systemctl_user(&["restart", &unit]);
    // Wait for the relaunched session to come up at the new size and publish its capture node. The
    // node appears when gamescope is up (well before Steam finishes booting); the caller's
    // first-frame retry absorbs Steam's cold start.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if current_gamescope_output_size() == Some(target) {
            if let Some(node) = find_gamescope_node() {
                tracing::info!(
                    node,
                    w = mode.width,
                    h = mode.height,
                    "gamescope: box game-mode session relaunched at the client's resolution"
                );
                return Ok(node);
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "box game-mode session did not come up at {}x{} within 45s after relaunch \
                 (Steam may still be booting)",
                mode.width,
                mode.height
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Output (capture) resolution `-W <w> -H <h>` of the running `gamescope` binary, parsed from its
/// `/proc/<pid>/cmdline`. `None` if no gamescope is running or the flags aren't present.
fn current_gamescope_output_size() -> Option<(u32, u32)> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        // Match the gamescope BINARY by argv[0]'s basename — NOT /proc/<pid>/exe, which is commonly
        // unreadable for the gamescope process (returns empty). The session wrapper scripts run as
        // bash/sh (argv[0] != gamescope), so they're excluded; the -W/-H presence check below is the
        // final filter.
        let is_gamescope = args
            .first()
            .map(|a0| a0.rsplit('/').next().unwrap_or(a0) == "gamescope")
            .unwrap_or(false);
        if !is_gamescope {
            continue;
        }
        let flag = |names: &[&str]| -> Option<u32> {
            args.iter().enumerate().find_map(|(i, a)| {
                names
                    .contains(&a.as_str())
                    .then(|| args.get(i + 1).and_then(|v| v.parse().ok()))
                    .flatten()
            })
        };
        if let (Some(w), Some(h)) = (
            flag(&["-W", "--output-width"]),
            flag(&["-H", "--output-height"]),
        ) {
            return Some((w, h));
        }
    }
    None
}

/// The running autologin gaming-mode unit (`gamescope-session-plus@<client>.service`), if any — the
/// box's own game-mode session, which [`ensure_box_gamescope_mode`] reconfigures + restarts.
fn running_autologin_gamescope_unit() -> Option<String> {
    let out = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|u| u.starts_with("gamescope-session-plus@") && u.ends_with(".service"))
        .map(|u| u.to_string())
}

/// Tear a gamescope `systemd --user` unit down with **SIGKILL** rather than the default SIGTERM stop
/// (`design/gamemode-and-dedicated-sessions.md` A3 / `session-aware-host-followups.md` #1): the
/// hypothesis — validated as the fix on the F44 repro box `.181` — is that gamescope's SIGTERM
/// teardown handler (the one that SIGSEGVs, exit 139) LEAKS the NVIDIA GPU context, after which every
/// subsequent gamescope fails `vkCreateDevice` with `VK_ERROR_INITIALIZATION_FAILED` (-3) until a
/// reboot. SIGKILL skips that handler so the driver reclaims the context cleanly via normal process
/// exit. Follow with `stop` + `reset-failed` to clear the unit's state so a relaunch is clean.
fn kill_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "kill", "--signal=SIGKILL", unit])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "stop", unit])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed", unit])
        .status();
}

/// Runtime-mask `unit` so the box's session supervisor cannot restart it underneath the takeover.
/// Bazzite/SteamOS autologin runs under SDDM with `Relogin=true` (`/etc/sddm.conf.d/steamos.conf`):
/// the moment the autologin session dies — including our own deliberate stop — SDDM logs back in and
/// starts the unit again within the same second. A merely-stopped unit then fights our host-managed
/// session over the Steam single instance and the GPU for the whole stream (the restarted wrapper
/// relaunches gamescope every ~7 s; the contention SIGSEGVs gamescopes and eventually kills the
/// streaming one — the "stream dies after 30 s–5 min" field reports, diagnosed live on .181
/// 2026-07-07). `--runtime` keeps the mask in tmpfs so a reboot clears it even if the host dies
/// without restoring (the same semantics as the persisted takeover file).
fn mask_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "mask", "--runtime", unit])
        .status();
}

/// Undo [`mask_unit`] — every restore path must unmask before (or regardless of) restarting, or the
/// box's own return-to-gaming-mode stays broken until reboot.
fn unmask_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "unmask", "--runtime", unit])
        .status();
}

/// Stop every autologin gaming-mode session (`gamescope-session-plus@*.service`) so its
/// single-instance Steam is free for our own host-managed session. Records the units so
/// [`schedule_restore_tv_session`] can restart them on disconnect. Our own session is the transient
/// `punktfunk-gamescope` unit (not a `@`-instance), so it's never matched here. No-op when nothing
/// is autologged in (e.g. a box that boots headless). Each unit is **masked first** ([`mask_unit`] —
/// SDDM's `Relogin=true` would otherwise restart it instantly), then torn down with **SIGKILL**
/// ([`kill_unit`]) to avoid the F44 GPU-context leak that the autologin's SIGTERM stop triggers.
/// Matches every loaded instance, not just `running` ones — under the SDDM relogin churn the unit
/// flaps through `activating`/`failed` between cycles, and an unmasked flapping unit re-enters the
/// fight the moment the supervisor restarts it.
fn stop_autologin_sessions() {
    let Ok(out) = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ])
        .output()
    else {
        return;
    };
    let mut stopped = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(unit) = line.split_whitespace().next() {
            if unit.starts_with("gamescope-session-plus@") && unit.ends_with(".service") {
                mask_unit(unit); // block the SDDM relogin loop from restarting it mid-stream
                kill_unit(unit); // SIGKILL teardown — avoid the F44 GPU-context leak
                tracing::info!(
                    unit,
                    "freed Steam: masked + SIGKILL-stopped the autologin gaming session for this stream"
                );
                stopped.push(unit.to_string());
            }
        }
    }
    if !stopped.is_empty() {
        *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = stopped;
        persist_takeover(); // A3: survive a host crash mid-stream
    }
}

/// How long a desktop Steam gets to honor `steam -shutdown` before the spawn fails. Steam tears
/// down a running game (Proton/wineserver included) on the way out, so this is generous.
const STEAM_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// B1b: free Steam held by a plain **desktop** session (GNOME/KDE — e.g. a Steam the user opened
/// while streaming the desktop). [`stop_autologin_sessions`] only frees `gamescope-session-plus@*`
/// autologin units, so a desktop Steam still holds the single instance — a dedicated launch's
/// nested `steam` would just forward its URI to it and exit, gamescope would follow its child
/// down, and the client would see a black screen while the game launches invisibly on the desktop
/// (observed 2026-07-14 on a GNOME host: session-recovery restarted GDM for a desktop stream, the
/// user opened Steam there, and the next game-library launch black-screened through all 8 pipeline
/// retries). Asks that Steam to quit via `steam -shutdown` (the single-instance IPC, graceful) and
/// waits for it to exit; on timeout the spawn fails with an operator-actionable error instead of
/// the misleading no-frames retry loop. Steam instances punktfunk owns are exempt — URI forwarding
/// into a reused/kept session is the designed path, and another session's live Steam must never be
/// torn down from here.
fn free_desktop_steam() -> Result<()> {
    let Some(pid) = desktop_steam_pid() else {
        return Ok(());
    };
    tracing::info!(
        pid,
        "freeing Steam: a desktop-session Steam holds the single instance — sending `steam -shutdown`"
    );
    let _ = Command::new("steam")
        .arg("-shutdown")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let deadline = Instant::now() + STEAM_SHUTDOWN_WAIT;
    while Instant::now() < deadline {
        if !pid_running(pid) {
            tracing::info!(pid, "desktop Steam exited — single instance free");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!(
        "Steam is already running in the host's desktop session (pid {pid}) and did not exit \
         within {}s of `steam -shutdown` — close Steam on the host, then launch again",
        STEAM_SHUTDOWN_WAIT.as_secs()
    )
}

/// Pid of a live Steam instance running OUTSIDE anything punktfunk owns (i.e. a desktop-session
/// Steam), found via `~/.steam/steam.pid` — Steam's own single-instance marker, kept current by
/// every fresh instance. `None` when Steam isn't running, the pidfile is stale (pid dead, zombie,
/// or recycled by a non-Steam process), or the instance is punktfunk's own: a descendant of this
/// host process (a dedicated spawn's nested Steam) or inside the managed [`SESSION_UNIT`] cgroup.
fn desktop_steam_pid() -> Option<u32> {
    let home = std::env::var("HOME").ok()?;
    let pid = std::fs::read_to_string(format!("{home}/.steam/steam.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    // Steam's own processes report comm `steam` (the ubuntu12_32 binary) or `steam.sh`; anything
    // else means the pid was recycled since Steam last ran.
    if !matches!(comm.trim(), "steam" | "steam.sh") || !pid_running(pid) {
        return None;
    }
    if descends_from(pid, std::process::id()) {
        return None; // our own dedicated spawn's Steam
    }
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    if cgroup_is_punktfunk_owned(&cgroup) {
        return None; // the host service's tree or the managed session unit
    }
    Some(pid)
}

/// Does this `/proc/<pid>/cgroup` content place the process in a punktfunk-owned unit — the host
/// service itself or the host-managed gamescope session? Desktop Steams live in desktop app scopes
/// (e.g. `app-gnome-steam-<pid>.scope`) instead. Pure + unit-tested.
fn cgroup_is_punktfunk_owned(cgroup: &str) -> bool {
    cgroup.contains("punktfunk-host.service") || cgroup.contains(&format!("{SESSION_UNIT}.service"))
}

/// Is `pid` alive and not a zombie? (A zombie keeps its `/proc` entry but has already released the
/// Steam instance, so waiting on it would spin the full shutdown deadline for nothing.)
fn pid_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Field 3 (state) follows the parenthesized comm — split after the LAST ')' since comm can
    // itself contain parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

/// Cancel any pending TV-session restore — a client has (re)connected, so the box must stay in the
/// streamed session, not bounce back to gaming mode. This covers the **keep-alive reuse** reconnect
/// path (a kept dedicated / managed gamescope), which never calls `create_managed_session` (where the
/// managed path already clears `PENDING_RESTORE`) — so without this, a dedicated Steam reconnect within
/// the linger window would restart the autologin *underneath* the live session (review finding #3).
/// Called from the connect path (native `resolve_compositor`, GameStream `open_gs_virtual_source`).
/// No-op when nothing is pending; the stopped-unit list stays armed for a later real disconnect.
pub fn cancel_pending_restore() {
    let mut g = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_some() {
        *g = None;
        tracing::info!(
            "gamescope: client (re)connected — cancelled the pending TV-session restore"
        );
    }
}

/// The delay before restoring the TV's autologin session after the last client disconnects — the
/// display-management **keep-alive policy**, replacing the hardcoded [`RESTORE_DEBOUNCE`]
/// (`design/gamemode-and-dedicated-sessions.md` A3). The managed gamescope session is a single
/// box-level singleton (not a registry pool entry — A1), so its keep-alive lives here rather than in
/// the registry, but reads the same policy the pooled backends do:
///   * `off` → restore immediately (0 s);
///   * `duration(s)` → restore after `s`;
///   * `forever` → **`None`**: never auto-restore — the managed session is HELD until host stop or a
///     manual return to gaming mode (the `gaming-rig` "the TV model" story, now truthful on gamescope);
///   * unconfigured → the historical 5 s [`RESTORE_DEBOUNCE`] (bit-for-bit today's behavior).
fn restore_delay() -> Option<Duration> {
    use crate::vdisplay::policy::{self, Linger};
    match policy::prefs()
        .configured_effective()
        .map(|e| e.keep_alive.linger())
    {
        Some(Linger::Immediate) => Some(Duration::from_secs(0)),
        Some(Linger::For(d)) => Some(d),
        Some(Linger::Forever) => None,
        None => Some(RESTORE_DEBOUNCE),
    }
}

/// Client disconnected: **schedule** a policy-timed restore of the TV's autologin gaming session(s) we
/// stopped on connect ([`restore_delay`], via [`start_restore_worker`]) — unless a client reconnects
/// first, which cancels it and reuses the warm managed session. Debouncing means at most one gamescope
/// stop/relaunch per quiet period instead of one per disconnect — the per-connect churn is what leaked
/// GPU context on F44. Under `keep_alive=forever` ([`restore_delay`] `None`) NO restore is scheduled:
/// the managed session is pinned (gaming-rig). No-op when nothing was stolen (non-Bazzite / headless
/// box). Idempotent / safe to call on every session end.
pub fn schedule_restore_tv_session() {
    let nothing_to_restore = STOPPED_AUTOLOGIN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
        && !*STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner());
    if nothing_to_restore {
        return; // nothing was taken over → nothing to restore (also the non-managed path)
    }
    match restore_delay() {
        None => {
            // keep_alive=forever → pin the managed session; leave PENDING_RESTORE unset.
            *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            tracing::info!(
                "gamescope: keep-alive=forever — managed session held (no TV-restore scheduled; \
                 return to gaming mode or restart the host to free it)"
            );
        }
        Some(delay) => {
            *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + delay);
            tracing::info!(
                secs = delay.as_secs(),
                "gamescope: scheduled TV-session restore (keep-alive policy; cancelled on reconnect)"
            );
        }
    }
}

/// Tear down our host-managed session (freeing Steam) and restart the autologin gaming session(s)
/// we stopped on connect — so the TV returns to gaming mode when no one is streaming. Invoked by
/// [`start_restore_worker`] once the debounce deadline passes; takes the stopped-unit list so a
/// cancelled+reconnected window keeps the list for a later real restore.
fn do_restore_tv_session() {
    // SteamOS: we reconfigured `gamescope-session.target` headless via a drop-in. Restore = remove
    // the drop-in + restart the target (back to the physical panel) — unless the user switched to a
    // desktop session meanwhile, in which case drop the override and leave the desktop alone.
    {
        let mut took = STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner());
        if *took {
            *took = false;
            clear_takeover(); // A3: takeover undone — drop the persisted crash-restore marker
            *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
            remove_steamos_dropin();
            systemctl_user(&["daemon-reload"]);
            use super::ActiveKind;
            if matches!(
                super::detect_active_session().kind,
                ActiveKind::DesktopKde
                    | ActiveKind::DesktopGnome
                    | ActiveKind::DesktopWlroots
                    | ActiveKind::DesktopHyprland
            ) {
                tracing::info!(
                    "gamescope (SteamOS): a desktop session is active — removed the headless \
                     override, not restarting the gaming session"
                );
                return;
            }
            systemctl_user(&["restart", STEAMOS_SESSION_TARGET]);
            tracing::info!(
                "gamescope (SteamOS): restored the physical gaming session (removed headless override)"
            );
            return;
        }
    }
    let units = std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    if units.is_empty() {
        return; // nothing was stolen → nothing to restore (also the non-Bazzite path)
    }
    clear_takeover(); // A3: takeover consumed — drop the persisted crash-restore marker
    stop_session(SESSION_UNIT); // our gamescope/Steam session, so Steam is free for the autologin
                                // Unmask UNCONDITIONALLY (before the desktop-active early return below): a unit left masked
                                // would break the user's own return to gaming mode until reboot.
    for unit in &units {
        unmask_unit(unit);
    }
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // Only bring the gaming autologin BACK if the box is still meant to be in gaming mode. If the
    // user switched to a desktop session (KDE/GNOME/wlroots) in the meantime, don't yank them back
    // to gaming — leave the desktop alone. (We still stopped our idle managed session above.)
    use super::ActiveKind;
    if matches!(
        super::detect_active_session().kind,
        ActiveKind::DesktopKde | ActiveKind::DesktopGnome | ActiveKind::DesktopWlroots
    ) {
        tracing::info!(
            "gamescope: a desktop session is active — not restoring the TV gaming session"
        );
        return;
    }
    for unit in units {
        let _ = Command::new("systemctl")
            .args(["--user", "start", &unit])
            .status();
        tracing::info!(
            unit,
            "restored the TV's autologin gaming session (debounce elapsed, no client)"
        );
    }
}

/// Host-lifetime worker that fires a pending [`schedule_restore_tv_session`] once its debounce
/// deadline passes. Returns a keepalive handle — drop it (host shutdown) to stop the worker. Cheap:
/// a 100 ms tick that does nothing until a restore is actually pending.
pub fn start_restore_worker() -> std::sync::Arc<()> {
    let handle = std::sync::Arc::new(());
    let weak = std::sync::Arc::downgrade(&handle);
    if let Err(e) = std::thread::Builder::new()
        .name("punktfunk-restore-worker".into())
        .spawn(move || {
            while weak.upgrade().is_some() {
                std::thread::sleep(Duration::from_millis(100));
                let due = {
                    let mut g = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
                    match *g {
                        Some(deadline) if Instant::now() >= deadline => {
                            *g = None;
                            true
                        }
                        _ => false,
                    }
                };
                if due {
                    do_restore_tv_session();
                }
            }
        })
    {
        tracing::error!(error = %e, "restore-worker spawn failed — TV session won't auto-restore on idle");
    }
    handle
}

/// Point the libei injector at the running gamescope's EIS socket (it reads the relay file
/// [`ei_socket_file`]). Best-effort — video still works without it (input just won't reach the
/// session). Shared by the attach and host-managed-session paths.
fn point_injector_at_eis() {
    match find_gamescope_eis_socket() {
        Some(sock) => match std::fs::write(ei_socket_file(), &sock) {
            Ok(()) => {
                tracing::info!(socket = %sock, "gamescope: pointed injector at the session's EIS socket")
            }
            Err(e) => tracing::warn!(
                error = %e,
                "gamescope: could not write the EIS relay file — input may not reach the session"
            ),
        },
        None => tracing::warn!(
            "gamescope: no connectable gamescope EIS socket found — input won't reach the session"
        ),
    }
}

/// Path of the host-written `GAMESCOPE_BIN` wrapper (per-user, in tmpfs).
fn gamescope_bin_wrapper_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&base).join("punktfunk-gamescope-bin")
}

/// Write the `GAMESCOPE_BIN` wrapper that injects `--nested-refresh $PF_HZ` — the flag
/// gamescope-session-plus does NOT expose, and the one that makes games see the client's refresh
/// instead of ~60 Hz. The body is constant (the rate comes from the `PF_HZ` env per launch), so the
/// write is idempotent. Returns its path.
fn write_gamescope_bin_wrapper() -> Result<std::path::PathBuf> {
    let path = gamescope_bin_wrapper_path();
    std::fs::write(
        &path,
        "#!/bin/sh\nexec /usr/bin/gamescope --nested-refresh \"${PF_HZ:-60}\" \"$@\"\n",
    )
    .with_context(|| format!("write GAMESCOPE_BIN wrapper {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod the GAMESCOPE_BIN wrapper {}", path.display()))?;
    Ok(path)
}

/// Launch `gamescope-session-plus <client>` headless at `mode` as a transient `systemd --user`
/// unit (clean cgroup teardown of the whole Steam tree on stop). Injects `--nested-refresh` (via
/// the wrapper) + `--generate-drm-mode cvt` so games see exactly `mode` (resolution + refresh) and
/// not the box's physical-display EDID. Blocks until the gamescope `Video/Source` node appears
/// (Steam Big Picture cold-start is slow), returning its id; on timeout it stops the unit and errors.
fn launch_session(client: &str, unit_name: &str, mode: Mode) -> Result<u32> {
    if !std::path::Path::new(SESSION_PLUS_BIN).exists() {
        anyhow::bail!(
            "PUNKTFUNK_GAMESCOPE_SESSION is set but {SESSION_PLUS_BIN} is missing — the host-managed \
             session needs gamescope-session-plus (a Bazzite / SteamOS-like host)"
        );
    }
    let wrapper = write_gamescope_bin_wrapper()?;
    stop_session(unit_name); // clear any stale unit + relay so a relaunch is clean
    let hz = mode.refresh_hz.max(1);
    let start_unit = || -> Result<()> {
        let status = Command::new("systemd-run")
            .args(["--user", "--collect", &format!("--unit={unit_name}")])
            // Same headless-must-not-attach rule as [`spawn`]: the transient unit inherits the
            // user manager env, which can carry a (possibly stale) desktop DISPLAY/WAYLAND_DISPLAY
            // that would abort gamescope at startup.
            .arg("--property=UnsetEnvironment=DISPLAY WAYLAND_DISPLAY")
            .arg("--setenv=BACKEND=headless")
            .arg(format!("--setenv=SCREEN_WIDTH={}", mode.width))
            .arg(format!("--setenv=SCREEN_HEIGHT={}", mode.height))
            .arg(format!("--setenv=PF_HZ={hz}"))
            .arg(format!("--setenv=GAMESCOPE_BIN={}", wrapper.display()))
            .arg("--setenv=DRM_MODE=cvt")
            .arg(format!("--setenv=CUSTOM_REFRESH_RATES={hz}"))
            .arg("--")
            .arg(SESSION_PLUS_BIN)
            .arg(client)
            .status()
            .context(
                "launch gamescope-session-plus via `systemd-run --user` (is the user systemd \
                 manager up with XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS set?)",
            )?;
        if !status.success() {
            anyhow::bail!(
                "`systemd-run --user` failed to start the gamescope session (exit {status})"
            );
        }
        Ok(())
    };
    start_unit()?;
    // Steam Big Picture cold-start is far slower than a bare app — poll the node for up to 45s.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(id) = find_gamescope_node() {
            return Ok(id);
        }
        if Instant::now() >= deadline {
            stop_session(unit_name);
            anyhow::bail!(
                "gamescope-session-plus '{client}' did not publish a Video/Source node within 45s \
                 (Steam failed to start? — `journalctl --user -u {unit_name}`)"
            );
        }
        // The session-plus wrapper hard-kills a gamescope that missed its 5 s readiness handshake
        // and exits 1 (a slow NVIDIA cold start routinely needs 5-15 s — the .181 storm 2026-07-07),
        // and the transient unit has no Restart= — without supervision the rest of this poll would
        // wait on a corpse. Re-run the unit so every readiness attempt inside the deadline is used.
        if !unit_starting_or_active(unit_name) {
            tracing::warn!(
                unit = unit_name,
                "gamescope session: transient unit died (missed the wrapper's 5 s gamescope \
                 readiness window?) — relaunching"
            );
            // Brief cooldown before the relaunch: the wrapper SIGKILLed a gamescope mid-Vulkan-init,
            // and the NVIDIA driver reclaims that context asynchronously — an instant relaunch pays
            // the reclaim serialization on top of device init and misses the 5 s window again.
            std::thread::sleep(Duration::from_millis(1500));
            let _ = Command::new("systemctl")
                .args(["--user", "reset-failed", unit_name])
                .status();
            start_unit()?;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Is the unit currently starting or up (`activating` / `active` — also `deactivating`: let a stop
/// finish; the next poll tick sees the settled state)? Unknown/unreachable states report `true` so a
/// systemctl hiccup can't trigger a relaunch storm.
fn unit_starting_or_active(unit: &str) -> bool {
    let Ok(out) = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output()
    else {
        return true;
    };
    matches!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "active" | "activating" | "reloading" | "deactivating"
    )
}

/// Stop the host-managed session's transient unit ([`kill_unit`] — SIGKILL teardown to avoid the F44
/// GPU-context leak) and clear the EIS relay so a dead session's socket name can't be reconnected.
fn stop_session(unit_name: &str) {
    kill_unit(unit_name);
    let _ = std::fs::remove_file(ei_socket_file());
}

/// File where the wrapper below writes gamescope's `LIBEI_SOCKET` (its EIS server socket), read by
/// the libei injector to drive input into the nested app. See [`crate::inject`].
///
/// Placed under `$XDG_RUNTIME_DIR` (a per-user, 0700 directory) — NOT a world-writable `/tmp` —
/// so a second unprivileged local user can neither read the relayed socket path nor pre-plant the
/// file to redirect the host's injector to a rogue EIS server (which would let them keylog or deny
/// the remote session's keyboard/mouse input; security-review 2026-06-28 #6). Falls back to `/tmp`
/// only if `XDG_RUNTIME_DIR` is unset (gamescope itself requires it, so this is rare); the reader
/// ([`crate::inject`]) additionally rejects a symlinked relay file as defense-in-depth.
pub fn ei_socket_file() -> std::path::PathBuf {
    let runtime = crate::vdisplay::with_env_lock(|| std::env::var_os("XDG_RUNTIME_DIR"));
    match runtime {
        Some(rt) if !rt.is_empty() => std::path::PathBuf::from(rt).join("punktfunk-gamescope-ei"),
        _ => std::path::PathBuf::from("/tmp/punktfunk-gamescope-ei"),
    }
}

/// Shape a resolved launch command for a bare-spawn gamescope session. A Steam URI launch
/// (`steam steam://rungameid/<id>`, produced by `library::command_for`) gets `-silent` inserted so
/// the game is the gamescope focus with no Steam client window to navigate
/// (`design/gamemode-and-dedicated-sessions.md` §5.3). Operator-typed custom commands and non-Steam
/// launches are returned unchanged. Idempotent (never double-inserts `-silent`). Pure + unit-tested.
/// Does this resolved launch command start Steam (`steam … steam://…`)? Such a launch needs Steam's
/// single instance free before a dedicated spawn (B1). Pure + unit-tested.
fn is_steam_launch(cmd: &str) -> bool {
    let mut it = cmd.split_whitespace();
    it.next() == Some("steam") && cmd.contains("steam://")
}

fn shape_dedicated_command(app: &str) -> String {
    let mut it = app.split_whitespace();
    if it.next() == Some("steam") {
        let rest: Vec<&str> = it.collect();
        if !rest.contains(&"-silent") && rest.iter().any(|t| t.starts_with("steam://")) {
            return format!("steam -silent {}", rest.join(" "));
        }
    }
    app.to_string()
}

/// Add the compositor-side arguments shared by every bare gamescope spawn. `steam_mode` belongs
/// before the `--` terminator; [`PUNKTFUNK_GAMESCOPE_APP`](spawn) configures the nested command
/// after it and therefore cannot enable gamescope's Steam integration itself.
fn add_bare_gamescope_args(command: &mut Command, w: u32, h: u32, hz: u32, steam_mode: bool) {
    command
        .args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &hz.to_string()]);
    if steam_mode {
        command.arg("--steam");
    }
    command.args(["--xwayland-count", "1", "--"]);
}

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. The app comes from
/// `PUNKTFUNK_GAMESCOPE_APP` (default a no-op that just keeps gamescope alive — set it to a real
/// game/GL app for actual content, e.g. `steam -gamepadui` for the SteamOS-like session).
/// stdout/stderr go to `log` (this spawn's per-instance log, A5). The app is launched through a tiny
/// shell wrapper that relays gamescope's `LIBEI_SOCKET` (set for its children) to [`ei_socket_file`]
/// so the input injector can connect to gamescope's EIS server from outside.
fn spawn(w: u32, h: u32, hz: u32, cmd: Option<&str>, log: &std::path::Path) -> Result<Child> {
    // A non-empty per-session command (set via `set_launch_command`) wins; else the
    // `PUNKTFUNK_GAMESCOPE_APP` env var (the documented manual fallback); else a no-op that keeps
    // gamescope alive. Each level is taken only if non-empty, so a blank per-session cmd transparently
    // falls through to the env (matching the pre-fix behaviour).
    let app = cmd
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        // Read the env fallback under the shared env lock so it can't race a concurrent session's
        // `set_var` of the same key (security-review 2026-06-28 #7).
        .or_else(|| {
            crate::vdisplay::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_APP").ok())
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "sleep infinity".to_string());
    // Dedicated-launch command shaping (Part B): a Steam URI runs with `-silent` so the game is the
    // gamescope focus with no Steam client window to navigate.
    let app = shape_dedicated_command(&app);
    let relay = ei_socket_file();
    let _ = std::fs::remove_file(&relay); // stale socket path from a previous session
    let steam_mode = crate::config::config().gamescope_steam;
    let mut cmd = Command::new("gamescope");
    add_bare_gamescope_args(&mut cmd, w, h, hz, steam_mode);
    cmd.args([
        "sh",
        "-c",
        &format!(
            "printf %s \"$LIBEI_SOCKET\" > '{}'; exec \"$@\"",
            relay.display()
        ),
        "sh",
    ])
    .args(app.split_whitespace())
    // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
    .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia")
    // A HEADLESS gamescope must never attach to a parent compositor. A host (re)started after
    // a desktop login inherits the user manager's DISPLAY/WAYLAND_DISPLAY — and a stale
    // WAYLAND_DISPLAY (e.g. a leftover `wayland-kde` in the manager env from a past session)
    // makes gamescope 3.16 exit at startup with "Failed to connect to wayland socket" before
    // its PipeWire node ever appears (observed 2026-07-14; the boot-started host never saw the
    // bug because it predates any login's env import). gamescope exports its own DISPLAY /
    // GAMESCOPE_WAYLAND_DISPLAY to the nested app, so the child loses nothing.
    .env_remove("DISPLAY")
    .env_remove("WAYLAND_DISPLAY");
    if let Ok(logf) = std::fs::File::create(log) {
        if let Ok(log2) = logf.try_clone() {
            cmd.stdout(Stdio::from(logf)).stderr(Stdio::from(log2));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    tracing::info!(w, h, hz, steam_mode, %app, log = %log.display(), "spawning gamescope (headless)");
    cmd.spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")
}

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
pub fn game_session_exited(node_id: u32) -> bool {
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

/// Poll [`find_gamescope_node`] (unscoped) up to `timeout` — for the managed / SteamOS session, which
/// logs to journald (no per-spawn file) and is single-session (no scoping needed).
fn poll_managed_node(timeout: Duration) -> Option<u32> {
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

fn wait_for_node(timeout: Duration, log: &std::path::Path, child_pid: u32) -> Option<u32> {
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
fn gamescope_node_present(node_id: u32) -> bool {
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
fn find_gamescope_node() -> Option<u32> {
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

/// Owns the spawned gamescope process (and its per-instance log, A5); killing it tears the virtual
/// output down.
struct GamescopeProc {
    child: Child,
    log: std::path::PathBuf,
}

impl Drop for GamescopeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Clear the relayed EIS socket name so the host-lifetime injector can't reconnect to this
        // now-dead session's socket between sessions (the stale path is the "Connection refused").
        let _ = std::fs::remove_file(ei_socket_file());
        // Drop this spawn's per-instance log (A5) so `$XDG_RUNTIME_DIR` doesn't accumulate them.
        let _ = std::fs::remove_file(&self.log);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cgroup_is_punktfunk_owned, is_steam_launch, parse_version, shape_dedicated_command,
        MIN_GAMESCOPE,
    };

    #[test]
    fn steam_launch_detection() {
        assert!(is_steam_launch("steam steam://rungameid/570"));
        assert!(is_steam_launch("steam -silent steam://rungameid/570"));
        assert!(!is_steam_launch("vkcube"));
        assert!(!is_steam_launch("lutris lutris:rungameid/42"));
        assert!(!is_steam_launch("steam -bigpicture")); // no URI = not a game launch
    }

    #[test]
    fn dedicated_command_shaping() {
        // Steam URI → -silent inserted so the game is the gamescope focus.
        assert_eq!(
            shape_dedicated_command("steam steam://rungameid/570"),
            "steam -silent steam://rungameid/570"
        );
        // Idempotent: an already-silent command is left alone.
        assert_eq!(
            shape_dedicated_command("steam -silent steam://rungameid/570"),
            "steam -silent steam://rungameid/570"
        );
        // Non-Steam launches and operator custom commands are untouched.
        assert_eq!(shape_dedicated_command("vkcube"), "vkcube");
        assert_eq!(
            shape_dedicated_command("lutris lutris:rungameid/42"),
            "lutris lutris:rungameid/42"
        );
        // A bare `steam` with no URI is left alone (not a game launch).
        assert_eq!(
            shape_dedicated_command("steam -bigpicture"),
            "steam -bigpicture"
        );
    }

    #[test]
    fn desktop_steam_cgroup_ownership() {
        // A desktop-launched Steam (the B1b conflict case, as observed on a GNOME host).
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-steam-48605.scope"
        ));
        // KDE spawns app scopes too; still foreign.
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@0f3a.service"
        ));
        // Our own dedicated spawn tree (Steam nested under the host service).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-host.service"
        ));
        // The host-managed gamescope session unit (SESSION_UNIT).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-gamescope.service"
        ));
        assert!(!cgroup_is_punktfunk_owned(""));
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
}
