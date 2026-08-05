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

#[path = "gamescope/discovery.rs"]
mod discovery;
#[path = "gamescope/heads.rs"]
mod heads;
#[path = "gamescope/splash.rs"]
mod splash;
use discovery::{
    check_gamescope_version, find_gamescope_eis_socket, find_gamescope_node, gamescope_bin,
    gamescope_node_present, poll_managed_node, wait_for_node,
};
pub(crate) use discovery::{
    game_session_exited, gamescope_can_composite_cursor, gamescope_hdr_capable, is_available,
    note_spawn_flags_lost, steam_appid_from_launch, wait_for_steam_game_exit, SteamGameWatch,
};
pub(crate) use heads::list_monitors;
pub(crate) use splash::run as splash_run;

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
    /// This session negotiated HDR (10-bit BT.2020 PQ) — set via [`VirtualDisplay::set_hdr`]
    /// before `create`. Spawns gamescope with `--hdr-enabled --hdr-debug-force-support` so the
    /// WSI layer advertises HDR10/scRGB surfaces to nested games, and the composite gamescope
    /// hands us can be negotiated as a 10-bit PQ stream (`packaging/gamescope`).
    hdr: bool,
    /// This session's resolved sub-mode (set via [`VirtualDisplay::set_gamescope_route`]). Same
    /// per-instance discipline as `cmd`, and for the same reason: it used to arrive through
    /// `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION`, which a concurrent connect could overwrite between
    /// the decision and this session's `create`. `None` = nothing resolved it (a caller that never
    /// ran `apply_input_env`); `create` then falls through to the bare spawn, the safe default.
    route: Option<crate::GamescopeRoute>,
}

/// A running host-managed session (its transient systemd --user unit) + the mode it was launched at.
struct SessionState {
    width: u32,
    height: u32,
    refresh_hz: u32,
    /// Whether the session was launched with the HDR flags. Part of the reuse key for the same
    /// reason the registry's is: gamescope cannot turn HDR on live, so an SDR session cannot be
    /// handed to an HDR client (the game would get no HDR surfaces while the stream negotiated
    /// PQ) — that needs a relaunch, exactly like a mode change.
    hdr: bool,
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

/// The display-manager unit we stopped for the takeover (any DM that drove a LIVE gaming session
/// is stopped for the stream — see [`dm_plan`]), so the restore brings the box back via
/// `reset-failed` + `restart` of the DM instead of a `--user start` of the gamescope unit (which
/// cannot work on a mask-fragile flavor: without a DM login session there is no seat, so gamescope
/// never gets DRM master — live-proven on the Nobara repro VM 2026-07-24).
static STOPPED_DM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// mtime of the `steamos-session-select` sentinel as of the takeover — the baseline the in-stream
/// "Switch to Desktop" detector compares against. Steam's session-select script writes
/// `~/.config/steamos-session-select` unconditionally in its USER pass, before any of its
/// display-manager checks — so it advances even under a DM-stop takeover, where the script's
/// config-rewrite tail is a silent no-op (every write branch is gated on the DM *running*;
/// diagnosed live on the Nobara repro VM 2026-07-24). An advanced mtime after a capture loss is
/// therefore the one durable trace of the user's switch request.
///
/// Two levels of `Option`, because "no baseline" and "no sentinel" mean opposite things:
/// * **outer `None`** — never baselined (no takeover this host lifetime). Nothing can read as an
///   in-stream request: the sentinel is a permanent file, so any box whose user has EVER switched
///   sessions has one, and comparing against a missing baseline made that ancient write look like
///   a live "Switch to Desktop".
/// * **`Some(None)`** — baselined while no sentinel existed yet; a later one was created inside the
///   session, which IS a request.
/// * **`Some(Some(t))`** — baselined at mtime `t`; anything newer is a request.
static SESSION_SELECT_BASELINE: std::sync::Mutex<Option<Option<std::time::SystemTime>>> =
    std::sync::Mutex::new(None);

/// When [`honor_session_select_switch`] last ran. While recent, a managed (re)launch is refused —
/// the rebuild loop would otherwise race the booting desktop back into game mode (gamescope+Steam
/// come up faster than KWin, and a delivering managed pipeline ends the rebuild's re-detection).
static SWITCH_HONORED_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// How long after honoring an in-stream desktop switch the managed path refuses to relaunch,
/// giving the DM's desktop session time to come up so re-detection follows it instead.
const SWITCH_HONOR_GRACE: Duration = Duration::from_secs(120);

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
    let base = crate::session::runtime_dir();
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
    /// The display-manager unit we stopped on a mask-fragile DM flavor (restore = `reset-failed` +
    /// `restart` of the DM). `default` so takeover files from older hosts still parse.
    #[serde(default)]
    stopped_dm: Option<String>,
}

/// Path of the persisted [`TakeoverState`], under `$XDG_RUNTIME_DIR` (per-user, 0700, tmpfs — cleared
/// on reboot, which is correct: a reboot restarts the autologin itself).
fn takeover_state_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
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
        stopped_dm: STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    };
    if state.stopped_autologin.is_empty() && !state.steamos && state.stopped_dm.is_none() {
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
    if state.stopped_autologin.is_empty() && !state.steamos && state.stopped_dm.is_none() {
        clear_takeover();
        return;
    }
    tracing::warn!(
        units = ?state.stopped_autologin,
        steamos = state.steamos,
        stopped_dm = ?state.stopped_dm,
        "gamescope: found a stranded takeover from a previous host instance — scheduling TV restore"
    );
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_autologin;
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = state.steamos;
    *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_dm;
    // Re-baseline the session-select sentinel: after a crash-restore the launch-time baseline is
    // gone, and a long-existing sentinel file must not read as a fresh in-stream switch request.
    record_session_select_baseline();
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

    fn set_hdr(&mut self, on: bool) {
        self.hdr = on;
    }

    fn hdr(&self) -> bool {
        // The registry keys keep-alive reuse on it too: a kept SDR gamescope was spawned WITHOUT
        // the HDR flags, so handing it to an HDR session would give the game no HDR surfaces and
        // negotiate a PQ stream over an SDR composite — wrong, and not obviously broken.
        self.hdr
    }

    fn set_gamescope_route(&mut self, route: Option<crate::GamescopeRoute>) {
        self.route = route;
    }

    fn poolable_now(&self) -> bool {
        // Only a bare SPAWN is registry-poolable (its `create` reports `Owned`); managed
        // (`PUNKTFUNK_GAMESCOPE_SESSION`) and attach (`PUNKTFUNK_GAMESCOPE_NODE`) report
        // `SessionManaged`/`External`, so the registry must not reuse a kept spawn for them (same
        // backend name). Mirrors [`crate::launch_is_nested`]; read under the env lock the
        // sub-mode ladder writes these keys under.
        matches!(self.route, None | Some(crate::GamescopeRoute::Spawn))
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
        // THIS session's resolved sub-mode, handed over by the host from `apply_input_env`. It
        // used to be read out of the process env here, which meant a second connect could retarget
        // this one between the decision and this line.
        let (session_env, node_env) = match self.route.clone() {
            Some(crate::GamescopeRoute::Managed { client }) => (Some(client), None),
            Some(crate::GamescopeRoute::Attach { node }) => (None, Some(node)),
            Some(crate::GamescopeRoute::Spawn) => (None, None),
            // Nobody resolved a route (no `apply_input_env` on this path): bare spawn, which is
            // also what the ladder's own default arm picks.
            None => (None, None),
        };
        if let Some(client) = session_env {
            return create_managed_session(&client, mode, self.hdr);
        }
        // Attach to an already-running gamescope (a foreign / externally-launched session) instead
        // of spawning our own: capture its node AND inject into its EIS socket.
        // PUNKTFUNK_GAMESCOPE_NODE=<id|auto>; "auto" discovers the gamescope `Video/Source` node.
        if let Some(id) = node_env {
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
                expect_exact_dims: false,
            });
        }
        check_gamescope_version(); // diagnostic only — warns on known-deadlock-prone versions
                                   // B1: a dedicated STEAM launch needs Steam's single instance free. If the box autologged into
                                   // game mode (Bazzite) its Steam holds the instance, and a nested second Steam would see the
                                   // first and exit (crashing the spawn) — so free the autologin session first. Its restore is the
                                   // A3 takeover machinery (recorded in STOPPED_AUTOLOGIN + persisted; restarted on session end via
                                   // schedule_restore_tv_session). Non-Steam launches don't conflict, so they skip this.
        if self.cmd.as_deref().is_some_and(is_steam_launch) {
            // A dedicated launch NEEDS Steam's single instance — no attach degrade exists here, so
            // a mask-fragile-DM box without takeover privilege fails with the actionable error.
            stop_autologin_sessions()
                .context("dedicated Steam launch needs the box's gaming session freed")?;
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
            self.hdr,
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
fn create_managed_session(client: &str, mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    // A (re)connect cancels any pending debounced TV-restore: we're about to (re)use the managed
    // session, so the autologin must stay stopped and the warm session stays up (no stop/relaunch).
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // SteamOS (the real Steam Deck) has no session-plus: take over its `gamescope-session.target`
    // headless at the client's mode instead of launching a separate managed session.
    if steamos_session_present() {
        return create_managed_session_steamos(mode, hdr);
    }
    // In-stream "Switch to Desktop" under a DM-stop takeover: the user's session-select inside
    // the streamed game mode advanced the sentinel, but its config rewrite was a silent no-op
    // (every write branch needs the DM running, and the takeover stopped it) — so without this,
    // the capture loss it caused would just relaunch game mode ("thrown back in", field-tested
    // 2026-07-24). Honor the request instead: restore the DM and replay the switch.
    let dm_takeover = STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(dm) = dm_takeover {
        if session_select_requested() {
            *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()) = None;
            honor_session_select_switch(dm);
            return Err(anyhow!(
                "the user switched the box to the desktop session — display manager restored; \
                 re-detection follows the desktop compositor as it comes up"
            ));
        }
    }
    // Post-honor grace: while the selected desktop boots, a managed relaunch would win the race
    // (gamescope+Steam start faster than KWin) and a delivering pipeline ends the rebuild's
    // re-detection — right back in game mode. A live box-owned game-mode unit supersedes the
    // grace: the user already switched back, so managed may proceed.
    let honor_pending = SWITCH_HONORED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| t.elapsed() < SWITCH_HONOR_GRACE);
    if honor_pending {
        if running_autologin_gamescope_unit().is_some() {
            *SWITCH_HONORED_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        } else {
            return Err(anyhow!(
                "waiting for the desktop session the user selected — refusing to relaunch game \
                 mode (re-detection follows the desktop once it's up)"
            ));
        }
    }
    // Attach-only rebuild probe: reuse a live same-mode session, but NEVER stop/relaunch box
    // sessions — right after a capture loss the caller's session detection can be stale, and a
    // destructive rebuild here would fight the session the user just switched to.
    if crate::rebuild_probe_active() {
        let guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let same_mode = guard.as_ref().is_some_and(|s| {
            s.width == mode.width
                && s.height == mode.height
                && s.refresh_hz == mode.refresh_hz
                && s.hdr == hdr
        });
        if same_mode {
            if let Some(node_id) = find_gamescope_node() {
                point_injector_at_eis();
                tracing::info!(
                    node_id,
                    "gamescope session: attach-only probe reusing live node"
                );
                return Ok(managed_output(node_id, mode));
            }
        }
        return Err(anyhow!(
            "gamescope session has no attachable live node — attach-only rebuild probe refuses \
             to stop/relaunch box sessions (re-detection follows the live session)"
        ));
    }
    // Steam is single-instance: if the box autologged into gaming mode on a physical display (the
    // Bazzite default — `gamescope-session-plus@ogui-steam` on the TV), that session holds Steam and
    // renders to the TV's native mode, which we'd capture instead of the client's. Free Steam by
    // stopping it; [`schedule_restore_tv_session`] (on disconnect) brings it back after a debounce.
    // On a mask-fragile-DM box without the privilege to stop the DM, the takeover would destabilize
    // the seat — degrade to ATTACH instead: mirror the box's own live game-mode session (capture +
    // inject, no lifecycle ownership), which needs no takeover at all.
    if let Err(e) = stop_autologin_sessions() {
        tracing::warn!(
            error = %format!("{e:#}"),
            "gamescope: managed takeover unavailable — degrading to ATTACH (mirroring the box's \
             own game-mode session)"
        );
        let node_id = ensure_box_gamescope_mode(mode)?;
        point_injector_at_eis();
        return Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(()),
            ownership: DisplayOwnership::External,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
        });
    }
    // B1b: a desktop-session Steam (outside any gamescope unit) also holds the single instance and
    // would make the managed session's own Steam exit at birth. The managed session's Steam itself
    // is exempt (it lives in the SESSION_UNIT cgroup), so the same-mode reuse below is unaffected.
    free_desktop_steam()?;
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width
            && s.height == mode.height
            && s.refresh_hz == mode.refresh_hz
            && s.hdr == hdr
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
    let node_id = match launch_session(client, SESSION_UNIT, mode, hdr) {
        Ok(id) => id,
        Err(e) => {
            // The takeover already happened (autologin units stopped, possibly the DM down) — arm
            // the restore now, or a failed launch strands the box sessionless until a host
            // restart. Policy-timed; a quick client retry cancels it and relaunches warm.
            // MANAGED_SESSION must be released first: the scheduler reads it (orphan detection).
            drop(guard);
            schedule_restore_tv_session();
            return Err(e);
        }
    };
    // Baseline the session-select sentinel NOW: only a write from INSIDE this session (the user's
    // "Switch to Desktop") should read as a switch request, not the one that led here.
    record_session_select_baseline();
    point_injector_at_eis();
    *guard = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
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
        expect_exact_dims: false,
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
/// ([`crate::apply_input_env`]) only defaults to managed when this is true; a plain
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
    let uid = crate::proc::current_uid();
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
        Some((x11, wayland, _xauth)) => {
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

/// EVERY nested Xwayland the running gamescope session exposes, as `(DISPLAY, XAUTHORITY)` pairs
/// for the XFixes cursor source (remote-desktop-sweep Phase C). gamescope can run several
/// (`--xwayland-count N` — Steam Gaming Mode uses 2: one for Big Picture, one for the game), and
/// the pointer lives on whichever is FOCUSED — so the source connects to all and follows the one
/// whose pointer moves. The host is not a gamescope child, so gamescope's auth cookie rides along
/// when a process exposes it. Empty when no gamescope session is running / none exposes a `DISPLAY`.
#[cfg(target_os = "linux")]
pub(crate) fn xwayland_cursor_targets() -> Vec<(String, Option<String>)> {
    let uid = crate::proc::current_uid();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
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
        let Ok(raw) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        let (mut display, mut is_gamescope, mut xauth) = (None, false, None);
        for kv in raw.split(|&b| b == 0) {
            let kv = String::from_utf8_lossy(kv);
            if kv.starts_with("GAMESCOPE_WAYLAND_DISPLAY=") {
                is_gamescope = true;
            } else if let Some(v) = kv.strip_prefix("DISPLAY=") {
                if !v.is_empty() {
                    display = Some(v.to_string());
                }
            } else if let Some(v) = kv.strip_prefix("XAUTHORITY=") {
                if !v.is_empty() {
                    xauth = Some(v.to_string());
                }
            }
        }
        if let (true, Some(d)) = (is_gamescope, display) {
            // Distinct DISPLAY only; prefer the first non-empty XAUTHORITY seen for it.
            match out.iter_mut().find(|(dd, _)| *dd == d) {
                Some((_, xa)) if xa.is_none() => *xa = xauth,
                Some(_) => {}
                None => out.push((d, xauth)),
            }
        }
    }
    out
}

/// Find the live gamescope session's `(DISPLAY, WAYLAND_DISPLAY, XAUTHORITY)` by scanning same-uid
/// processes for one whose environment carries `GAMESCOPE_WAYLAND_DISPLAY` (gamescope sets it for
/// everything it runs — Steam, the game, our own nested `sh`). The Wayland value returned is that
/// gamescope socket; `DISPLAY` is the nested Xwayland; `XAUTHORITY` is its auth file (for X
/// clients that aren't gamescope children). Any one can be individually absent.
fn discover_session_display_env() -> Option<(Option<String>, Option<String>, Option<String>)> {
    let uid = crate::proc::current_uid();
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
        let mut xauth = None;
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
            } else if let Some(v) = kv.strip_prefix("XAUTHORITY=") {
                if !v.is_empty() {
                    xauth = Some(v.to_string());
                }
            }
        }
        // Only a process INSIDE a gamescope session (it has the marker var) is a valid source.
        if gs_wayland.is_some() {
            return Some((display, gs_wayland, xauth));
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
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gsbin")
}

/// The rate to give gamescope as its nested refresh — the client's, capped by the host's frame
/// limiter (`PUNKTFUNK_MAX_FPS`, unset by default).
///
/// gamescope's nested refresh is the rate the game is clamped to, which is what makes it the right
/// and only place for this knob. The session is untouched: the client still negotiates and receives
/// its full rate, because the encode loop re-encodes the held frame whenever the compositor has
/// produced no new one — a repeat of an unchanged picture, which costs an almost-empty P-frame.
/// So a 60-capped game on a 120 Hz session still puts 120 frames a second on the wire, and the GPU
/// time the game is no longer spending goes to capture and encode instead (see
/// `design/…` and issue #9's contention notes).
///
/// The cap is the nested output's rate, so everything gamescope composites moves at it, not just
/// the game — under gamescope there is only the one output. That is the trade the knob is: it is
/// off by default, and its whole purpose is to stop the game rendering flat out.
fn game_hz(session_hz: u32) -> u32 {
    pf_host_config::config().game_fps(session_hz).max(1)
}

/// The gamescope arg-rewriting shim. SteamOS hardcodes physical-panel args, so we intercept the
/// session's `exec gamescope` (via PATH) and rewrite to a headless output at the client's mode (read
/// from `PF_W`/`PF_H`/`PF_HZ`), dropping the physical flags. Idempotent; returns the shim's directory.
///
/// `PF_HZ` is the frame-limited rate ([`game_hz`]) — the shim spends it on `-r` alone, so it caps
/// the game without touching the resolution the client negotiated (`PF_W`/`PF_H`).
fn write_headless_shim() -> Result<std::path::PathBuf> {
    // `$PF_HDR_ARGS` is unquoted for the same reason as in the GAMESCOPE_BIN wrapper: it is our
    // own flag list ([`hdr_args`]) and must word-split into separate argv entries.
    let shim_body = format!(
        r#"#!/bin/bash
W="${{PF_W:-1920}}"; H="${{PF_H:-1080}}"; HZ="${{PF_HZ:-60}}"
keep=()
while [ $# -gt 0 ]; do
  case "$1" in
    --generate-drm-mode|-w|-h|-W|-H|-O|--prefer-output) shift 2;;
    *) keep+=("$1"); shift;;
  esac
done
exec {bin} --backend headless -W "$W" -H "$H" -w "$W" -h "$H" -r "$HZ" ${{PF_HDR_ARGS}} "${{keep[@]}}"
"#,
        bin = gamescope_bin()
    );
    let dir = headless_shim_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let shim = dir.join("gamescope");
    std::fs::write(&shim, &shim_body).with_context(|| format!("write shim {}", shim.display()))?;
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
fn write_steamos_dropin(shim_dir: &std::path::Path, mode: Mode, hdr: bool) -> Result<()> {
    let path = steamos_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    // UnsetEnvironment: the same headless-must-not-attach armor `launch_session` gives its
    // transient unit — the manager env can carry a stale desktop DISPLAY/WAYLAND_DISPLAY (from a
    // portal settle), and gamescope would abort trying to attach to it instead of becoming the
    // display server. Unit-scoped belt-and-suspenders on top of the observe_session_instance scrub.
    let body = format!(
        "[Service]\n\
         Environment=PATH={shim}:/usr/bin:/bin:/usr/local/bin\n\
         Environment=PF_W={w}\n\
         Environment=PF_H={h}\n\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         UnsetEnvironment=DISPLAY WAYLAND_DISPLAY\n",
        shim = shim_dir.display(),
        w = mode.width,
        h = mode.height,
        hz = game_hz(mode.refresh_hz),
        // Read (unquoted) by the PATH shim — empty for an SDR session. Quoted HERE because a
        // systemd `Environment=` value with spaces must be, or only the first flag survives.
        hdr_args = hdr_args(hdr)
            .into_iter()
            .chain(cursor_args())
            .collect::<Vec<_>>()
            .join(" "),
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
fn create_managed_session_steamos(mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width
            && s.height == mode.height
            && s.refresh_hz == mode.refresh_hz
            && s.hdr == hdr
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
    // Attach-only rebuild probe: the reuse path above may attach, but a restart of the session
    // target is out of bounds — observed live on a Deck: a stale post-capture-loss detection made
    // this restart steal the seat back from the KDE session the user had just switched to.
    if crate::rebuild_probe_active() {
        return Err(anyhow!(
            "gamescope has no live node and this is an attach-only rebuild probe — refusing to \
             restart {STEAMOS_SESSION_TARGET} (the box may be mid-switch to another session; \
             re-detection follows it)"
        ));
    }
    let shim_dir = write_headless_shim()?;
    write_steamos_dropin(&shim_dir, mode, hdr)?;
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", STEAMOS_SESSION_TARGET]);
    // LOCK ORDER. Everything below this line must run WITHOUT `MANAGED_SESSION` held: the restore
    // path takes STEAMOS_TOOK_OVER first and MANAGED_SESSION second (`do_restore_tv_session`), and
    // `takeover_live` reads the whole set — so taking them in the other order here is an AB/BA
    // deadlock between a connect and the restore worker, which genuinely run concurrently
    // (`registry.rs` calls `vd.create` off the registry lock; the worker is its own thread).
    // Nothing between here and the re-acquire reads the tracked session.
    drop(guard);
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
    // The shim is only a PATH entry — confirm the session actually took it before we trust the
    // capabilities the plan was already built on (a stock gamescope here means no HDR and, worse,
    // a silently pointerless stream). Leaves the tracked state unset on failure, so the retry does
    // a clean restart rather than a same-mode reuse of a session we just rejected.
    verify_managed_spawn_flags(hdr)?;
    point_injector_at_eis();
    // Re-acquire to record the tracked session — the same shape `create_managed_session` uses.
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
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
    // Attach-only rebuild probe (parity with both managed paths — this gap was the attach-path
    // stale-detection hazard): right after a capture loss the caller's session detection can be
    // stale, and a set-environment + unit restart here would fight the session the user just
    // switched to. Mirror whatever live node exists at its own mode; refuse otherwise.
    if crate::rebuild_probe_active() {
        if let Some(node) = find_gamescope_node() {
            tracing::info!(
                node,
                "gamescope: attach-only rebuild probe — mirroring the live node at its own mode"
            );
            return Ok(node);
        }
        return Err(anyhow!(
            "no live gamescope node — attach-only rebuild probe refuses to restart the box's \
             session (re-detection follows the live session)"
        ));
    }
    // A box driving a PHYSICAL display is mirrored at its own mode, never re-moded: the re-mode
    // restart is the headless-box model (no panel ⇒ the game-mode resolution is ours to set);
    // on-glass it would flip the user's own screen to the client's resolution — and on a
    // DM-session-driven box (Nobara) the unit restart bounces the login session with it.
    if physical_display_connected() {
        if let Some(node) = find_gamescope_node() {
            tracing::info!(
                node,
                client_w = mode.width,
                client_h = mode.height,
                "gamescope: box drives a physical display — attaching at its own mode (no re-mode)"
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

/// argv of every gamescope COMPOSITOR running on this box, read from `/proc/<pid>/cmdline`.
///
/// Match by argv[0]'s basename — NOT `/proc/<pid>/exe`, which is commonly unreadable for the
/// gamescope process (returns empty). `ends_with` rather than `==` because the binary the host
/// resolves is frequently our own `punktfunk-gamescope` ([`gamescope_bin`]); it still excludes
/// `gamescopectl`, `gamescopereaper` and the `gamescope-session-plus` shell wrapper, none of which
/// END in the name.
fn gamescope_argvs() -> Vec<Vec<String>> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in dir.flatten() {
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
        if args
            .first()
            .is_some_and(|a0| a0.rsplit('/').next().unwrap_or(a0).ends_with("gamescope"))
        {
            found.push(args);
        }
    }
    found
}

/// Output (capture) resolution `-W <w> -H <h>` of the running gamescope, parsed from its
/// `/proc/<pid>/cmdline`. `None` if no gamescope is running or the flags aren't present — which is
/// also the final filter that separates a compositor from anything else [`gamescope_argvs`] let by.
fn current_gamescope_output_size() -> Option<(u32, u32)> {
    gamescope_argvs().into_iter().find_map(|args| {
        let flag = |names: &[&str]| -> Option<u32> {
            args.iter().enumerate().find_map(|(i, a)| {
                names
                    .contains(&a.as_str())
                    .then(|| args.get(i + 1).and_then(|v| v.parse().ok()))
                    .flatten()
            })
        };
        match (
            flag(&["-W", "--output-width"]),
            flag(&["-H", "--output-height"]),
        ) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }
    })
}

/// Did the flags we passed an INDIRECTLY-spawned session actually reach its gamescope?
///
/// The bare spawn builds argv itself and cannot lose them. The two managed modes can: a
/// `gamescope-session-plus` gets them through `GAMESCOPE_BIN` + `PF_HDR_ARGS`, SteamOS through a
/// PATH shim, and a session that ignores either execs the distro's gamescope with none of them.
/// The HDR half of that failure is loud on its own (capture negotiation times out and the session
/// dies on the bit-depth promise) — but a lost `--pipewire-composite-cursor` is SILENT: the host
/// was told the compositor would paint the pointer, so it didn't, and nobody did. The stream is
/// fine except that it has no cursor.
///
/// So check the running compositor and refuse the session when a flag is missing. The plan is
/// already fixed by this point (`cursor_blend` feeds the encoder open, which precedes the display),
/// so correcting THIS session isn't possible — instead latch the capability off
/// ([`note_spawn_flags_lost`]) and fail, and the retry plans a correct host-composited SDR session.
///
/// Fail OPEN in every ambiguous direction: no expected flags, or no readable gamescope process at
/// all, is silence. Only a compositor we can see, that is missing a flag we can name, fails.
///
/// It accepts ANY running gamescope carrying the flags, which is deliberate — a box commonly has a
/// second one (observed on the Nobara test box: its own game-mode
/// `/usr/bin/gamescope --prefer-output *,eDP-1 … --steam` running beside ours), and demanding that
/// EVERY gamescope carry them would reject a perfectly good session. The direction that error can
/// go is a false PASS, and the flag that matters is immune to it: `--pipewire-composite-cursor`
/// exists only in our patch set, so no foreign gamescope can be carrying it. `--hdr-enabled`
/// predates us and could in principle be borrowed from a neighbour, but its failure mode is the
/// loud one this check is not for.
fn verify_managed_spawn_flags(hdr: bool) -> Result<()> {
    let expected: Vec<String> = hdr_args(hdr)
        .into_iter()
        .chain(cursor_args())
        .filter(|a| a.starts_with("--")) // flag names only — their values are bare words
        .collect();
    if expected.is_empty() {
        return Ok(());
    }
    let missing = missing_flags(&expected, &gamescope_argvs());
    if missing.is_empty() {
        tracing::debug!(flags = ?expected, "gamescope: the session's compositor carries our flags");
        return Ok(());
    }
    note_spawn_flags_lost();
    // Warn as well as erroring: the latch is a process-wide capability change, and whichever
    // caller consumes this error decides on its own how loudly to report it.
    tracing::warn!(
        missing = %missing.join(" "),
        "gamescope: the session ignored GAMESCOPE_BIN / the PATH shim and ran a stock gamescope — \
         HDR and the in-node cursor are now off for this host process"
    );
    Err(anyhow!(
        "the gamescope session started without {} — it ignored GAMESCOPE_BIN / the PATH shim and \
         ran a stock gamescope. Refusing it rather than streaming a session whose shape was \
         planned around flags that never arrived (a missing cursor flag has no symptom but an \
         absent pointer). Those capabilities are off for this host now; reconnect for a plain SDR \
         session, or install punktfunk-gamescope as the box's `gamescope`",
        missing.join(" ")
    ))
}

/// Which of `expected` no running gamescope carries. Split out pure because both of its empty
/// answers are load-bearing and mean opposite things: no `argvs` is "we could not look" (silence),
/// no missing flag is "we looked and it is fine" — and getting the first one wrong would fail every
/// session on a box whose `/proc` we cannot read.
fn missing_flags<'a>(expected: &'a [String], argvs: &[Vec<String>]) -> Vec<&'a str> {
    if argvs.is_empty() {
        return Vec::new();
    }
    expected
        .iter()
        .filter(|f| !argvs.iter().any(|argv| argv.iter().any(|a| a == *f)))
        .map(String::as_str)
        .collect()
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
///
/// ⚠ The mask only covers the UNIT path — it is NOT what stops the relogin loop itself. On images
/// whose SDDM session helper execs the session script directly (`/etc/sddm/wayland-session
/// gamescope-session-plus steam`, f43 bazzite-deck — live-diagnosed on the .41 VM 2026-07-31) the
/// relogin never touches the unit, so the mask blocks nothing: SDDM relogins ~3×/s, each a full
/// `bash --login` session start that fails against the managed instance — 328 forks/s, load 6+,
/// 1481 logind sessions in 8 minutes, the journal flooded past its own rotation. The stream itself
/// survives, but the storm starves the game and the encoder ("atrocious, unplayable 240fps"). The
/// real defense is stopping the DM ([`dm_plan`]); the mask stays as belt-and-braces for the window
/// before the stop lands, for images that DO route the relogin through the unit, and as the
/// degraded takeover when the stop is impossible.
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

/// The unit name of the display manager driving this box's graphical logins, from the
/// `display-manager.service` alias symlink (the Fedora/Arch/openSUSE convention every
/// gamescope-session distro follows). `None` when no DM is installed (a box that boots straight
/// into a user session — getty autologin / an enabled user unit).
fn display_manager_unit() -> Option<String> {
    display_manager_unit_under(std::path::Path::new("/etc/systemd/system"))
}

/// [`display_manager_unit`] against an arbitrary root (the unit-testable core).
fn display_manager_unit_under(base: &std::path::Path) -> Option<String> {
    let target = std::fs::read_link(base.join("display-manager.service")).ok()?;
    target.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Does this display manager's autologin loop SURVIVE the gamescope unit being masked? This does
/// NOT decide whether the DM keeps running — any DM relogin-loops against a killed live gaming
/// session, so [`dm_plan`] stops the DM on every flavor — it decides whether masking is safe at
/// all, and with it the DEGRADED takeover when the DM can't be stopped (no lingering / no
/// privilege):
/// * **SDDM** survives (a failing autologin leaves sddm itself running — .181 2026-07-07), so the
///   degraded takeover is mask-only: Steam stays protected, and the cost is SDDM's relogin churn
///   for the stream's duration — anything from logind/ACL flapping (.181, the audio-flap
///   pathology) to a full fork storm on images whose sddm helper bypasses the unit (.41
///   2026-07-31, see [`mask_unit`]).
/// * Nobara's `plasmalogin` (KDE's SDDM successor) is proven FATAL: against a masked unit
///   its session Exec fails instantly, `Relogin=true` retries, and `plasmalogin.service` trips
///   systemd's start limit within ~1 s — the DM dies and the box is a permanent black screen that
///   only a root `reset-failed` + `restart` recovers (live-proven on the Nobara repro VM
///   2026-07-24). Unknown DMs are treated as fragile: the fragile path degrades gracefully, a wrong
///   "safe" kills the seat.
fn dm_survives_masked_unit(dm: &str) -> bool {
    dm == "sddm.service"
}

/// The takeover's display-manager decision, derived purely from the DM flavor and whether any
/// autologin gaming instance is LIVE (unit-tested; the runtime guards — lingering, privilege —
/// stay with [`stop_autologin_sessions`]).
///
/// Killing a live autologin session starts its DM's `Relogin=true` loop, and no flavor tolerates
/// that loop well: SDDM's churns logind sessions up to a fork storm ([`mask_unit`]), plasmalogin's
/// start-limit-kills the DM. So whenever a DM drove a LIVE gaming session, the DM itself is
/// stopped for the stream's duration; the restore ([`do_restore_tv_session`]) brings it back and
/// its autologin restores gaming mode. The flavors differ only in masking and in the degraded
/// mode ([`dm_survives_masked_unit`]).
struct DmPlan {
    /// Touch nothing at all: a mask-fragile DM with no live gaming instance — killing
    /// loaded-but-inactive leftovers frees nothing, and stopping the DM would kill the user's
    /// live desktop for it.
    skip: bool,
    /// Mask the units before killing them (safe only where the DM survives a masked unit; also
    /// the whole of the degraded takeover when the DM can't be stopped).
    mask: bool,
    /// Stop the DM for the stream's duration (only a live instance justifies it).
    stop_dm: bool,
}

/// See [`DmPlan`].
fn dm_plan(dm: Option<&str>, any_live: bool) -> DmPlan {
    let mask = dm.is_none_or(dm_survives_masked_unit);
    DmPlan {
        skip: !mask && !any_live,
        mask,
        stop_dm: dm.is_some() && any_live,
    }
}

/// The packaged privileged fallback for the display-manager takeover verbs: a root helper behind
/// its own polkit action (`io.unom.punktfunk.dm-helper`, `allow_any` — the mechanism these
/// distros use for their own session switcher, e.g. Nobara's `os-session-select`), so the managed
/// takeover works out of the box on mask-fragile DM flavors with no hand-installed polkit rule.
/// The helper derives the DM unit from the `display-manager.service` symlink itself, so this
/// process never gets to name an arbitrary unit across the privilege boundary. Two layouts: the
/// rpm/deb `libexec` path (what the shipped policy annotates) and Arch's `/usr/lib/<pkg>` (its
/// PKGBUILD rewrites the annotation to match).
const DM_HELPER_PATHS: &[&str] = &[
    "/usr/libexec/punktfunk/pf-dm-helper",
    "/usr/lib/punktfunk/pf-dm-helper",
];

/// Run the packaged DM helper (`stop` | `restore` | `linger`) via pkexec. `false` when the helper
/// isn't installed (tarball/old package), pkexec is missing, or polkit denies the action.
fn dm_helper(verb: &str) -> bool {
    let Some(helper) = DM_HELPER_PATHS
        .iter()
        .find(|p| std::path::Path::new(p).exists())
    else {
        return false;
    };
    Command::new("pkexec")
        .arg(helper)
        .arg(verb)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `systemctl` on the SYSTEM bus, **never interactively**. Every privileged verb below runs on the
/// stream's own thread — the capture-loss rebuild, or the restore worker — with no way to answer a
/// question. Without `--no-ask-password`, systemctl asks polkit for interactive authorization, and
/// on a box whose desktop session is still alive (the host is a `--user` unit inside it) polkit
/// hands that to the session's agent: a password dialog on the box's OWN screen, which during a
/// managed takeover is off or mid-switch. Nobody sees it, nobody answers it, and the call blocks
/// while the rebuild budget burns — the takeover then lands after the session it was for already
/// ended. `--no-ask-password` turns that into the immediate "interactive authentication required"
/// failure the callers are written for, so the pkexec helper (`allow_any`, no agent needed) takes
/// over instead of a dialog. Field-suspect in the 0.20.0 Nobara report (intermittent disconnect +
/// a screen that never comes back), where the timing is a race against the KDE agent's own death.
fn systemctl_system(args: &[&str]) -> bool {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--no-ask-password").args(args);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Would stopping the display manager also stop US? A packaged host runs as a `systemd --user`
/// unit, so its lifetime hangs off the user manager — and the DM stop ends the user's last login
/// session. logind then stops `user@<uid>.service` once `UserStopDelaySec` (10 s by default)
/// elapses, taking the host with it: the stream dies mid-takeover, and nothing is left to restart
/// the display manager, so the box stays dark until someone reaches a VT. **Field-proven on 0.20.0**
/// (Nobara, 2026-07-27): DM stopped at 12:34:18.9, the user manager stopped the host at 12:34:29.0
/// — 10.1 s, textbook `UserStopDelaySec`. It never showed on the repro VM because lingering was
/// enabled there for the sessionless tests.
///
/// Lingering (`loginctl enable-linger` — which the KDE/GNOME/Arch setup docs already ask for) is
/// what breaks the dependency: logind keeps the user manager up with no session at all. So ensure
/// it BEFORE touching the DM, and refuse the takeover when it can't be ensured — the caller then
/// degrades to attach, which mirrors the box's own session and never stops the DM.
fn ensure_host_survives_dm_stop() -> bool {
    if !host_is_under_user_manager() {
        return true; // root / a system unit — the DM stop cannot reach us
    }
    if linger_enabled() {
        return true;
    }
    // `set-self-linger` is `allow_active` in logind's own policy, so a host started inside the
    // user's session can do this itself; a sessionless one (the packaged unit) goes through the
    // helper, whose grant is scoped to the calling uid.
    let uid = uid_string();
    let _ = Command::new("loginctl")
        .args(["--no-ask-password", "enable-linger", &uid])
        .status();
    if linger_enabled() || (dm_helper("linger") && linger_enabled()) {
        tracing::info!(
            uid,
            "enabled lingering for this user — the managed takeover stops the display manager, \
             which ends this login session, and without lingering logind would stop the host \
             along with it (`loginctl disable-linger` reverts it)"
        );
        return true;
    }
    false
}

/// Is this process's lifetime tied to a `systemd --user` manager (i.e. would logind's user-manager
/// stop take us down)? Read from our own cgroup path.
fn host_is_under_user_manager() -> bool {
    std::fs::read_to_string("/proc/self/cgroup")
        .as_deref()
        .map(cgroup_under_user_manager)
        .unwrap_or(false)
}

/// [`host_is_under_user_manager`]'s test: does this `/proc/self/cgroup` content sit under a
/// `user@<uid>.service` manager? Pure + unit-tested. A system unit
/// (`/system.slice/punktfunk-host.service`) does not, and neither does a bare process started from
/// a login shell (`/user.slice/user-1000.slice/session-2.scope`) — logind's user-manager stop only
/// reaches units the user manager owns.
fn cgroup_under_user_manager(cgroup: &str) -> bool {
    cgroup.contains("user@")
}

/// Our uid as a string — what `loginctl` wants for a user argument.
fn uid_string() -> String {
    crate::proc::current_uid().to_string()
}

/// Is lingering on for this user (logind keeps the `--user` manager alive with no session)?
fn linger_enabled() -> bool {
    Command::new("loginctl")
        .args(["show-user", &uid_string(), "-p", "Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

/// Stop the display manager for a takeover on a mask-fragile DM flavor. Plain `systemctl stop` on
/// the SYSTEM bus first — succeeds as root or under an operator polkit rule scoped to the DM unit
/// (see docs); fails cleanly otherwise ("interactive authentication required") — then the
/// packaged pkexec helper. `false` means no privilege path exists and the caller degrades to
/// attach.
fn try_stop_display_manager(dm: &str) -> bool {
    systemctl_system(&["stop", dm]) || dm_helper("stop")
}

/// Restore the display manager: `reset-failed` (a relogin loop may have tripped the unit's start
/// limit, and a plain restart is refused until the accounting clears) + `restart` — its autologin
/// session Exec brings the box's own session back up. Plain system-bus verbs first (root / an
/// operator polkit rule), then the packaged pkexec helper, whose `restore` verb performs the same
/// two steps as root.
fn restore_display_manager(dm: &str) -> bool {
    let _ = systemctl_system(&["reset-failed", dm]);
    systemctl_system(&["restart", dm]) || dm_helper("restore")
}

/// The distro's session-switch helper (ChimeraOS/Nobara layout). Its USER pass records the
/// sentinel + self-pkexecs (authorized `allow_any` by the distro's own polkit action policy, so
/// it works from our sessionless context); its ROOT pass rewrites the DM autologin config — but
/// only while the DM is RUNNING, which is why the takeover must restart the DM before calling it.
const OS_SESSION_SELECT: &str = "/usr/libexec/os-session-select";

/// The sentinel Steam's `steamos-session-select` writes in its user pass
/// (`~/.config/steamos-session-select`) — see [`SESSION_SELECT_BASELINE`].
fn session_select_sentinel() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        std::path::Path::new(&home)
            .join(".config")
            .join("steamos-session-select"),
    )
}

/// Current mtime of the session-select sentinel (`None` when it doesn't exist yet).
fn session_select_mtime() -> Option<std::time::SystemTime> {
    let path = session_select_sentinel()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Record the sentinel baseline, so a LATER write (the user's in-stream "Switch to Desktop") is
/// distinguishable from the switch that led into this session. Taken at **takeover** (the moment
/// [`STOPPED_DM`] is set, which is what arms the honor gate) and again at a successful launch: the
/// switch INTO game mode writes the sentinel on its way in, and that write must never read as a
/// request to go back out. Baselining only at launch left the window in between — a takeover whose
/// launch failed, then a client retry inside the restore debounce — reading a months-old sentinel
/// as a live request and pushing the box to the desktop the user never asked for.
fn record_session_select_baseline() {
    *SESSION_SELECT_BASELINE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(session_select_mtime());
}

/// Did a session-select run inside the managed session since the baseline? Inside a managed game
/// session the only switch Steam offers is TO the desktop, so an advanced sentinel reads as that
/// request.
fn session_select_requested() -> bool {
    let baseline = *SESSION_SELECT_BASELINE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    sentinel_advanced(baseline, session_select_mtime())
}

/// [`session_select_requested`]'s decision, as a pure function of the two readings (the
/// unit-testable core). No baseline ⇒ no request: see [`SESSION_SELECT_BASELINE`] for why a
/// missing baseline must not be read as "the sentinel appeared during the session".
fn sentinel_advanced(
    baseline: Option<Option<std::time::SystemTime>>,
    now: Option<std::time::SystemTime>,
) -> bool {
    match (baseline, now) {
        (Some(Some(base)), Some(now)) => now > base,
        (Some(None), Some(_)) => true, // no sentinel at baseline — created during the session
        _ => false,
    }
}

/// Honor the user's in-stream "Switch to Desktop" under a DM-stop takeover. The OS flow was a
/// silent no-op (the switch script's config rewrite requires a running DM, which the takeover
/// stopped), so replay it with the DM up — every verb live-validated on the Nobara repro VM:
/// 1. consume the takeover and start the DM (its autologin heads back into game mode briefly —
///    the config still names it);
/// 2. run the distro's own `os-session-select desktop` as the user (its internal pkexec is
///    `allow_any`-authorized), which rewrites the DM autologin config to the desktop session;
/// 3. stop the autologin gamescope unit — the login session exits, and `Relogin=true` relogs
///    into the now-selected desktop.
///
/// The caller then refuses managed relaunches for [`SWITCH_HONOR_GRACE`] so the capture-loss
/// re-detection follows the desktop compositor once it's up instead of racing it.
fn honor_session_select_switch(dm: String) {
    tracing::info!(
        %dm,
        "gamescope: in-stream session-select detected — restoring the display manager and \
         switching the box to the desktop session"
    );
    // Consume the takeover state up front: from here on the box is the DM's again.
    std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    clear_takeover();
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    stop_session(SESSION_UNIT); // dead already (the switch shut its Steam down) — clear the unit
    if !restore_display_manager(&dm) {
        tracing::warn!(
            %dm,
            "gamescope: display-manager start was denied — the desktop switch may need a manual \
             `systemctl restart` of the DM"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let active = Command::new("systemctl")
            .args(["is-active", &dm])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
            .unwrap_or(false);
        if active {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Rewrite the autologin session via the distro's own switch helper (needs the DM running).
    // Absent/failing helper degrades to a plain DM restore — the box lands back in game mode on
    // glass and the stream follows that instead (no black screen either way).
    if std::path::Path::new(OS_SESSION_SELECT).exists() {
        match Command::new(OS_SESSION_SELECT).arg("desktop").status() {
            Ok(s) if s.success() => {
                // The relogin only fires when the CURRENT (game-mode) login session exits: wait
                // for its autologin unit to come up, then stop it. Never mask here — the mask is
                // what start-limit-kills this DM flavor.
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if let Some(unit) = running_autologin_gamescope_unit() {
                        systemctl_user(&["stop", &unit]);
                        tracing::info!(
                            %unit,
                            "gamescope: desktop selected — stopped the game-mode session so the \
                             DM relogs into the desktop"
                        );
                        break;
                    }
                    if Instant::now() >= deadline {
                        tracing::warn!(
                            "gamescope: game-mode session never appeared after the DM restart — \
                             the desktop switch may need a manual session exit"
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            other => tracing::warn!(
                status = ?other,
                "gamescope: os-session-select failed — leaving the box in its configured session"
            ),
        }
    } else {
        tracing::warn!(
            "gamescope: no {OS_SESSION_SELECT} on this box — restored the DM into its configured \
             session instead of switching to the desktop"
        );
    }
    record_session_select_baseline();
    *SWITCH_HONORED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// Stop every autologin gaming-mode session (`gamescope-session-plus@*.service`) so its
/// single-instance Steam is free for our own host-managed session. Records the units so
/// [`schedule_restore_tv_session`] can restart them on disconnect. Our own session is the transient
/// `punktfunk-gamescope` unit (not a `@`-instance), so it's never matched here. No-op when nothing
/// is autologged in (e.g. a box that boots headless).
///
/// When a display manager drove a LIVE gaming session, it is **stopped for the stream** on every
/// flavor ([`dm_plan`]): killing the session otherwise starts the DM's `Relogin=true` loop, which
/// at best churns logind sessions/ACLs and at worst is a full fork storm — f43 bazzite-deck's sddm
/// helper execs the session script directly, so the masked unit never enters the picture (328
/// forks/s, load 6+, live-diagnosed on the .41 VM 2026-07-31 — see [`mask_unit`]). The units
/// themselves are torn down with **SIGKILL** ([`kill_unit`]) to avoid the F44 GPU-context leak
/// that the autologin's SIGTERM stop triggers. The flavors differ in masking and in the degraded
/// mode when the DM can't be stopped (no lingering / no privilege):
/// * **SDDM / no DM**: each unit is **masked first** ([`mask_unit`] — belt-and-braces under a
///   stopped DM, and the whole defense on images that DO route the relogin through the unit).
///   Matches every loaded instance, not just `running` ones — under a relogin churn the unit
///   flaps through `activating`/`failed` between cycles, and an unmasked flapping unit re-enters
///   the fight the moment the supervisor restarts it. A failed DM stop **degrades to mask-only**
///   with a warning, never to attach: the mask still protects Steam, at the storm-tax price.
/// * **Mask-fragile DM** (Nobara's `plasmalogin`, unknown DMs): masking start-limit-kills the DM
///   itself (permanent black screen), so the units are killed unmasked, and a failed DM stop
///   **fails the takeover** — the error tells the caller to degrade to ATTACH (mirror the box's
///   own session) rather than destabilize the seat.
fn stop_autologin_sessions() -> Result<()> {
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
        return Ok(());
    };
    // `(unit, ACTIVE state)` — the `--plain` columns are UNIT LOAD ACTIVE SUB DESCRIPTION.
    let listed: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let unit = cols.next()?;
            let active = cols.nth(1).unwrap_or("");
            (unit.starts_with("gamescope-session-plus@") && unit.ends_with(".service"))
                .then(|| (unit.to_string(), active.to_string()))
        })
        .collect();
    if listed.is_empty() {
        return Ok(()); // nothing autologged in — Steam is already free
    }
    let dm = display_manager_unit();
    // Only a LIVE instance holds Steam / justifies touching the DM. A loaded-but-inactive
    // leftover (the box switched back to the desktop earlier) must not stop the DM — that
    // would kill the user's live desktop to free nothing.
    let any_live = listed
        .iter()
        .any(|(_, active)| matches!(active.as_str(), "active" | "activating"));
    let plan = dm_plan(dm.as_deref(), any_live);
    if plan.skip {
        return Ok(());
    }
    if plan.stop_dm {
        let dm = dm.expect("stop_dm ⇒ Some");
        // The DM stop ends this user's last login session. If our own lifetime hangs off the user
        // manager and lingering can't be turned on, that stop kills the host ~10s later — with the
        // box's display manager down and nobody left to bring it back. On a mask-fragile flavor,
        // degrading to attach is strictly better than a black screen that needs a VT to recover;
        // where masking is safe, mask-only (the storm tax) is strictly better than attach.
        let dm_stopped = if !ensure_host_survives_dm_stop() {
            if !plan.mask {
                bail!(
                    "stopping {dm} ends this user's last login session, and without lingering \
                     logind would stop the user manager — and this host with it — about 10s \
                     later, leaving the box with no display manager and nothing to restore it; \
                     enabling lingering failed, so the managed takeover is unavailable (run \
                     `sudo loginctl enable-linger $USER` once, as the setup docs ask, then \
                     reconnect)"
                );
            }
            tracing::warn!(
                %dm,
                "cannot stop the display manager for this stream (lingering could not be \
                 enabled, and without it the DM stop would take this host down ~10s later) — \
                 leaving it running: its autologin Relogin loop will churn logind sessions for \
                 the whole stream, up to a fork storm that starves the game and encoder; run \
                 `sudo loginctl enable-linger $USER` once, as the setup docs ask"
            );
            false
        } else if !try_stop_display_manager(&dm) {
            if !plan.mask {
                bail!(
                    "the box's gaming session is driven by {dm}, which does not survive a masked \
                     session unit, and stopping it needs privilege — the packaged pf-dm-helper \
                     polkit action is missing or was denied (reinstall the punktfunk package, or \
                     install the display-manager polkit rule from the docs) so the managed \
                     takeover is unavailable"
                );
            }
            tracing::warn!(
                %dm,
                "stopping the display manager for this stream needs privilege — the packaged \
                 pf-dm-helper polkit action is missing or was denied — leaving it running: its \
                 autologin Relogin loop will churn logind sessions for the whole stream, up to a \
                 fork storm that starves the game and encoder (reinstall the punktfunk package, \
                 or install the display-manager polkit rule from the docs)"
            );
            false
        } else {
            true
        };
        if dm_stopped {
            tracing::info!(
                %dm,
                "freed Steam: stopped the display manager for this stream (its autologin \
                 Relogin loop would otherwise churn against the takeover)"
            );
            // Baseline the switch sentinel HERE, not just at a successful launch: setting
            // STOPPED_DM is what arms the honor gate, so from this instant an unbaselined
            // sentinel would read as an in-stream "Switch to Desktop" — including the write from
            // the switch that just brought the box INTO game mode. A successful launch
            // re-baselines (tighter still).
            record_session_select_baseline();
            *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()) = Some(dm);
        }
    }
    let units: Vec<String> = listed.into_iter().map(|(u, _)| u).collect();
    let mut stopped = Vec::new();
    for unit in units {
        if plan.mask {
            mask_unit(&unit); // belt-and-braces under a stopped DM; the whole defense otherwise
        }
        kill_unit(&unit); // SIGKILL teardown — avoid the F44 GPU-context leak
        tracing::info!(
            %unit,
            masked = plan.mask,
            "freed Steam: stopped the autologin gaming session for this stream"
        );
        stopped.push(unit);
    }
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = stopped;
    persist_takeover(); // A3: survive a host crash mid-stream
    Ok(())
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
    use crate::policy::{self, Linger};
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
    if !takeover_live() {
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

/// Is anything of the box's own session ours right now — an autologin unit we stopped, a stopped
/// display manager, a SteamOS target we re-pointed, or a managed session we launched beside a live
/// desktop? The precondition for every restore path.
fn takeover_live() -> bool {
    // ONE lock at a time. In a `||` chain every `.lock()` temporary lives to the end of the
    // statement, so this used to hold all four simultaneously — putting it in the lock-order graph
    // for no reason, since each is only read. Scoped bindings drop each guard before the next.
    let autologin = !STOPPED_AUTOLOGIN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    let steamos = *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner());
    let dm = STOPPED_DM
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    autologin
        || steamos
        || dm
        // A managed session that took nothing over (started beside a live desktop — e.g. a client
        // gamescope pin on a KDE box) still owns the transient SESSION_UNIT: without this arm it
        // was ORPHANED forever after disconnect ("closing the app does not end the session",
        // field report 2026-07-24) — the restore stops it even with no autologin to bring back.
        || MANAGED_SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
}

/// Give the box its own session back **now**, synchronously — the host is going away (SIGTERM from
/// `systemctl --user stop`/`restart`, a package update, Ctrl-C) and a live takeover must not
/// outlive it. On a DM-flavor takeover the display manager is STOPPED: nothing else on the box
/// will ever restart it, and the persisted crash-restore state lives in `$XDG_RUNTIME_DIR`, which
/// logind removes along with the user manager — so not even the next host start can heal it. The
/// box would stay dark until someone reached a VT.
///
/// Deliberately ignores the keep-alive policy that [`schedule_restore_tv_session`] honors:
/// `keep_alive=forever` pins a session for the NEXT client, which is meaningless once the host
/// that would serve them is exiting. No-op when nothing was taken over.
pub fn restore_takeover_now() {
    if !takeover_live() {
        return;
    }
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None; // doing it right here
    tracing::info!("gamescope: host is shutting down — restoring the box's own session first");
    do_restore_tv_session();
}

/// Does any DRM connector report a physically `connected` display? Scans
/// `/sys/class/drm/*/status` — only connector nodes (`card0-eDP-1`, `card0-HDMI-A-1`, …) have a
/// `status` file, so the bare `cardN` device dirs and `renderD*` nodes filter themselves out. A
/// headless box (VM, panel-less mini PC) has none — in which case a "restore to the physical
/// panel" can only fail, gamescope having no output to drive. Errors (no DRM at all, sysfs
/// unreadable) read as headless: the safe direction is keeping the working session.
fn physical_display_connected() -> bool {
    connected_connector_under(std::path::Path::new("/sys/class/drm"))
}

/// [`physical_display_connected`] against an arbitrary sysfs root (the unit-testable core).
fn connected_connector_under(base: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(base) else {
        return false;
    };
    entries.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected")
    })
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
            // A box with no physically connected display (a VM, a panel-less mini PC) has no
            // "physical gaming session" to restore TO: removing the drop-in and restarting the
            // target just crash-loops gamescope (no output to drive) and strands every later
            // connect on "no usable compositor". Keep the headless session — and the takeover
            // state, so a same-mode reconnect reuses it warm — instead. Checked at restore time
            // (not connect time) so plugging a panel in later restores normally.
            if !physical_display_connected() {
                tracing::info!(
                    "gamescope (SteamOS): no physical display connected — keeping the headless \
                     session (nothing to restore to)"
                );
                return;
            }
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
    let dm = std::mem::take(&mut *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()));
    if units.is_empty() && dm.is_none() {
        // Nothing was stolen — but a managed session that started BESIDE a live desktop (client
        // gamescope pin on a KDE box) still owns the transient unit; stop it so it doesn't run
        // orphaned forever after the disconnect. No-op when the unit isn't running.
        if MANAGED_SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some()
        {
            stop_session(SESSION_UNIT);
            tracing::info!(
                "gamescope: stopped the idle managed session (nothing was taken over — no box \
                 session to restore)"
            );
        }
        return;
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
    // user switched to a desktop session (KDE/GNOME/wlroots/Hyprland) in the meantime, don't yank
    // them back to gaming — leave the desktop alone. (We still stopped our idle managed session
    // above.)
    use super::ActiveKind;
    if matches!(
        super::detect_active_session().kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
    ) {
        tracing::info!(
            "gamescope: a desktop session is active — not restoring the TV gaming session"
        );
        return;
    }
    // DM-stop takeover ([`dm_plan`] — every flavor stops a DM that drove a live gaming session):
    // restore the DM (`reset-failed` clears any relogin start-limit accounting, then `restart`);
    // its autologin session Exec brings gaming mode back itself. The unit is NOT `--user start`ed
    // here: on a mask-fragile flavor that cannot work — without a DM login session there is no
    // seat, so gamescope never gets DRM master (unit goes `failed`, screen stays black —
    // live-proven on the Nobara repro VM) — and under SDDM the relogin makes it redundant.
    if let Some(dm) = dm {
        let restart = restore_display_manager(&dm);
        if restart {
            tracing::info!(%dm, "restored the display manager (its autologin brings gaming mode back)");
        } else if crate::try_recover_session() {
            tracing::warn!(
                %dm,
                "display-manager restart lost its privilege — fired PUNKTFUNK_RECOVER_SESSION_CMD \
                 to bring the session back"
            );
        } else {
            tracing::error!(
                %dm,
                "could not restart the display manager and no PUNKTFUNK_RECOVER_SESSION_CMD is \
                 configured — the box has no graphical session until someone runs \
                 `systemctl reset-failed {dm} && systemctl restart {dm}` as root"
            );
        }
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
        Some(sock) => {
            // Relay format: line 1 = socket, optional line 2 = the session's CURRENT output
            // size as "WxH". gamescope's EIS advertises only a degenerate INT32_MAX region, so
            // the injector can't learn the output geometry from the protocol — the hint lets
            // it scale normalized client positions correctly even when the client streams at
            // a different resolution than the session runs (foreign attach, supersample).
            let size = current_gamescope_output_size();
            let body = match size {
                Some((w, h)) => format!("{sock}\n{w}x{h}"),
                None => sock.clone(),
            };
            match std::fs::write(ei_socket_file(), body) {
                Ok(()) => {
                    tracing::info!(
                        socket = %sock,
                        output = ?size,
                        "gamescope: pointed injector at the session's EIS socket"
                    )
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "gamescope: could not write the EIS relay file — input may not reach the session"
                ),
            }
        }
        None => tracing::warn!(
            "gamescope: no connectable gamescope EIS socket found — input won't reach the session"
        ),
    }
}

/// Mirror the physical head this gamescope session is driving (`vdisplay::mirror`'s gamescope arm).
///
/// The other backends mirror a head by *starting a new cast* addressed by connector name. gamescope
/// has no such call and needs none: it composites its one head into a PipeWire node it already
/// publishes, so mirroring is an ATTACH to that node — the same node the `PUNKTFUNK_GAMESCOPE_NODE`
/// route uses, reached here without the sub-mode ladder because a monitor pin has already decided
/// the question the ladder exists to answer.
///
/// **What makes this the mirror rather than the takeover**: nothing is stopped, nothing is
/// relaunched, and no mode is imposed. The MANAGED route would tear the TV's autologin session down
/// and rebuild it headless at the client's mode — correct when the operator wants the box's screen
/// to go dark and the client to drive it, and exactly wrong when they asked to stream the panel
/// they are looking at. `keepalive` is therefore `()`: we did not create this session and must not
/// end it (`DisplayOwnership::External`, applied by the caller).
///
/// `hw_cursor` is inert here, and deliberately so rather than silently dropped: whether gamescope's
/// pointer reaches the stream is fixed by the SPAWN flags of the session that is already running
/// (`--pipewire-composite-cursor`, see `gamescope_can_composite_cursor`), not negotiable per cast.
/// The session plan reads that same fact and arranges the XFixes reconstruction when it is absent.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let node_id = find_gamescope_node().ok_or_else(|| {
        anyhow!(
            "gamescope is driving {connector:?} but publishes no PipeWire Video/Source node — the \
             session may still be starting, or this gamescope was built without PipeWire support"
        )
    })?;
    // Absolute input lands through gamescope's own EIS socket, as it does on every other gamescope
    // route. The host's `capture_monitor` anchor is a no-op for this backend (gamescope advertises
    // a degenerate INT32_MAX region), so the output-size hint written here is what actually scales
    // the client's normalized positions onto the head.
    point_injector_at_eis();
    tracing::info!(
        connector,
        node_id,
        hw_cursor,
        "gamescope: mirroring the session's own head (attach — the gaming session is untouched)"
    );
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: None,
        keepalive: Box::new(()),
    })
}

/// Path of the host-written `GAMESCOPE_BIN` wrapper (per-user, in tmpfs).
fn gamescope_bin_wrapper_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gamescope-bin")
}

/// Write the `GAMESCOPE_BIN` wrapper that injects `--nested-refresh $PF_HZ` — the flag
/// gamescope-session-plus does NOT expose, and the one that makes games see the client's refresh
/// instead of ~60 Hz — plus `$PF_HDR_ARGS` for an HDR session. The body is constant (rate and HDR
/// flags come from the env per launch), so the write is idempotent. Returns its path.
///
/// `$PF_HDR_ARGS` is deliberately UNQUOTED: it is either empty or a short list of gamescope flags
/// this host built itself ([`hdr_args`]), never operator input, and it has to word-split into
/// separate argv entries.
fn write_gamescope_bin_wrapper() -> Result<std::path::PathBuf> {
    let path = gamescope_bin_wrapper_path();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec {} --nested-refresh \"${{PF_HZ:-60}}\" ${{PF_HDR_ARGS}} \"$@\"\n",
            gamescope_bin()
        ),
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
fn launch_session(client: &str, unit_name: &str, mode: Mode, hdr: bool) -> Result<u32> {
    if !std::path::Path::new(SESSION_PLUS_BIN).exists() {
        anyhow::bail!(
            "PUNKTFUNK_GAMESCOPE_SESSION is set but {SESSION_PLUS_BIN} is missing — the host-managed \
             session needs gamescope-session-plus (a Bazzite / SteamOS-like host)"
        );
    }
    let wrapper = write_gamescope_bin_wrapper()?;
    stop_session(unit_name); // clear any stale unit + relay so a relaunch is clean
    let hz = mode.refresh_hz.max(1);
    // The two rates are deliberately different when the frame limiter is set. CUSTOM_REFRESH_RATES
    // generates the mode the session ADVERTISES, which must stay the client's — that is what makes
    // games see the real refresh instead of the box's EDID. PF_HZ becomes `--nested-refresh`, the
    // rate the game is clamped to, and is the only one the limiter touches. Identical when it's
    // unset, which is the default.
    let game = game_hz(mode.refresh_hz);
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
            .arg(format!("--setenv=PF_HZ={game}"))
            // Read (unquoted) by the GAMESCOPE_BIN wrapper — empty for a stock-gamescope SDR
            // session, and carrying the cursor flag whenever the binary supports it.
            .arg(format!(
                "--setenv=PF_HDR_ARGS={}",
                hdr_args(hdr)
                    .into_iter()
                    .chain(cursor_args())
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
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
            // `GAMESCOPE_BIN` is a session-plus convention, not a guarantee — confirm the session
            // honoured it before we trust the capabilities the plan was already built on. Stop the
            // unit on rejection so the retry relaunches instead of reusing what we just refused.
            if let Err(e) = verify_managed_spawn_flags(hdr) {
                stop_session(unit_name);
                return Err(e);
            }
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
/// the libei injector to drive input into the nested app. See the `pf-inject` crate.
///
/// Placed under `$XDG_RUNTIME_DIR` (a per-user, 0700 directory) — NOT a world-writable `/tmp` —
/// so a second unprivileged local user can neither read the relayed socket path nor pre-plant the
/// file to redirect the host's injector to a rogue EIS server (which would let them keylog or deny
/// the remote session's keyboard/mouse input; security-review 2026-06-28 #6). Falls back to `/tmp`
/// only if `XDG_RUNTIME_DIR` is unset (gamescope itself requires it, so this is rare); the reader
/// (the `pf-inject` crate) additionally rejects a symlinked relay file as defense-in-depth.
pub fn ei_socket_file() -> std::path::PathBuf {
    // The path itself is the shared `pf_paths::gamescope_ei_socket_file` contract (also read by the
    // libei injector). Compute it under the session env lock so a concurrent session handshake's
    // `apply_session_env` XDG_RUNTIME_DIR retarget can't race this producer-side read.
    crate::with_env_lock(pf_paths::gamescope_ei_socket_file)
}

/// Does this resolved launch command start the Steam **client**? Such a launch needs Steam's single
/// instance free before a dedicated spawn (B1), and wants gamescope's `--steam` integration on.
/// Pure + unit-tested.
///
/// The test is the first token, NOT the presence of a `steam://` URI. A `steam_ui` launcher entry
/// (design D4) resolves to a bare `steam -gamepadui` / `steam` with no URI at all, and it is *more*
/// exposed to the single-instance problem than a game launch is, not less: on a box that autologged
/// into game mode, the nested second Steam would see the first and exit, taking the spawn down with
/// it. A URI-gated check would silently skip both the instance free and `--steam` for exactly the
/// launch that most needs them.
fn is_steam_launch(cmd: &str) -> bool {
    cmd.split_whitespace().next() == Some("steam")
}

/// Shape a resolved launch command for a bare-spawn gamescope session. A Steam URI launch
/// (`steam steam://rungameid/<id>`, produced by `library::command_for`) gets `-gamepadui` inserted
/// so the nested Steam is Big Picture — the identity gamescope's `--steam` integration is built
/// around (it's what SteamOS/Bazzite game mode runs): the boot shows the gamepad UI instead of the
/// desktop Steam client window (field report 2026-07-27: the desktop UI flashing through the
/// stream "looks bad"), and gamescope's focus rules already prefer the game window over the Steam
/// UI appid, which is what the previous `-silent` shaping was working around. Operator-typed
/// custom commands and non-Steam launches are returned unchanged. Idempotent (never
/// double-inserts). Pure + unit-tested.
fn shape_dedicated_command(app: &str) -> String {
    let mut it = app.split_whitespace();
    if it.next() == Some("steam") {
        let rest: Vec<&str> = it.collect();
        if !rest.contains(&"-gamepadui") && rest.iter().any(|t| t.starts_with("steam://")) {
            return format!("steam -gamepadui {}", rest.join(" "));
        }
    }
    app.to_string()
}

/// Add the compositor-side arguments shared by every bare gamescope spawn. `steam_mode` belongs
/// before the `--` terminator; [`PUNKTFUNK_GAMESCOPE_APP`](spawn) configures the nested command
/// after it and therefore cannot enable gamescope's Steam integration itself.
///
/// `-r` is the rate the GAME sees and is clamped to, which is why the frame limiter lives here
/// (see [`game_hz`]) and nowhere near the session: capping it makes the game stop rendering
/// frames nobody asked for, while capture and the wire keep running at the client's own rate.
fn add_bare_gamescope_args(
    command: &mut Command,
    w: u32,
    h: u32,
    hz: u32,
    steam_mode: bool,
    grab_cursor: bool,
    hdr: bool,
) {
    command
        .args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &game_hz(hz).to_string()]);
    if steam_mode {
        command.arg("--steam");
    }
    if grab_cursor {
        command.arg("--force-grab-cursor");
    }
    for arg in hdr_args(hdr).into_iter().chain(cursor_args()) {
        command.arg(arg);
    }
    command.args(["--xwayland-count", "1", "--"]);
}

/// The gamescope flags that make an HDR session HDR — shared by all three spawn sub-modes (bare
/// spawn, the `GAMESCOPE_BIN` wrapper, the SteamOS PATH shim), which is the point: a kept display
/// is keyed on `hdr`, so the flags must not be able to drift between the paths that produce it.
///
/// * `--hdr-enabled` sets gamescope's `cv_hdr_enabled` convar.
/// * `--hdr-debug-force-support` is what makes it work HEADLESS: the headless connector hardcodes
///   `SupportsHDR() == false`, and this flag is the documented bypass. Without it steamcompmgr
///   never pushes the `gamescopeHDROutputFeedback` root atom, so the WSI layer advertises no
///   HDR10/scRGB surfaces and nested games render SDR — which would look exactly like a capture
///   negotiation failure while actually being a spawn-flag bug. (A first-class `--headless-hdr`
///   is the upstream-friendly replacement; we pin the gamescope we ship, so the debug flag is
///   fine meanwhile.)
/// * `--hdr-sdr-content-nits` maps SDR content into the PQ container. Everything that is not an
///   HDR game — the desktop, the Steam overlay, an SDR title — rides through it, so it decides
///   how bright "white" lands on the client's panel. Only passed when the operator set the knob;
///   otherwise gamescope's own default (400) applies.
fn hdr_args(hdr: bool) -> Vec<String> {
    if !hdr {
        return Vec::new();
    }
    let mut args = vec![
        "--hdr-enabled".to_string(),
        "--hdr-debug-force-support".to_string(),
    ];
    if let Some(nits) = pf_host_config::config().gamescope_sdr_nits {
        args.push("--hdr-sdr-content-nits".to_string());
        args.push(nits.to_string());
    }
    args
}

/// `--pipewire-composite-cursor` when the resolved gamescope has it (patch level 2+). Paired with
/// [`crate::gamescope_composites_cursor`], which is what tells the host to STOP compositing the
/// pointer itself — the two must agree, so both read the same probe.
///
/// Passed on every session, HDR or not: a cursor in the node is strictly better than one blended
/// host-side (it costs the host a full-frame pass, and on the zero-CSC encode source it cannot be
/// done at all). Empty on a stock gamescope, which is exactly the old behaviour.
fn cursor_args() -> Vec<String> {
    if gamescope_can_composite_cursor() {
        vec!["--pipewire-composite-cursor".to_string()]
    } else {
        Vec::new()
    }
}

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. The app comes from
/// `PUNKTFUNK_GAMESCOPE_APP` (default a no-op that just keeps gamescope alive — set it to a real
/// game/GL app for actual content, e.g. `steam -gamepadui` for the SteamOS-like session).
/// stdout/stderr go to `log` (this spawn's per-instance log, A5). The app is launched through a tiny
/// shell wrapper that relays gamescope's `LIBEI_SOCKET` (set for its children) to [`ei_socket_file`]
/// so the input injector can connect to gamescope's EIS server from outside — and (unless
/// `PUNKTFUNK_GAMESCOPE_SPLASH=0`) backgrounds the host's splash client first, so the fresh
/// compositor has a painting window from the first second: gamescope pushes capture buffers only
/// when it composites, and a nested Steam bootstrap paints nothing until the gamepad UI's first
/// frame — far longer than any first-frame budget (see `gamescope/splash.rs`).
fn spawn(
    w: u32,
    h: u32,
    hz: u32,
    cmd: Option<&str>,
    log: &std::path::Path,
    hdr: bool,
) -> Result<Child> {
    // A non-empty per-session command (set via `set_launch_command`) wins; else the
    // `PUNKTFUNK_GAMESCOPE_APP` env var (the documented manual fallback); else a no-op that keeps
    // gamescope alive. Each level is taken only if non-empty, so a blank per-session cmd transparently
    // falls through to the env (matching the pre-fix behaviour).
    let app = cmd
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        // Read the env fallback under the shared env lock so it can't race a concurrent session's
        // `set_var` of the same key (security-review 2026-06-28 #7).
        .or_else(|| crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_APP").ok()))
        .filter(|s| !s.trim().is_empty());
    // A real app was requested (vs. the `sleep infinity` keep-alive) — used to scope the game-only
    // cursor-grab flag below.
    let game_launch = app.is_some();
    let app = app.unwrap_or_else(|| "sleep infinity".to_string());
    // Dedicated-launch command shaping (Part B): a Steam URI runs with `-gamepadui` so the nested
    // Steam is Big Picture — the identity gamescope's `--steam` mode is built around — instead of
    // the desktop client window.
    let app = shape_dedicated_command(&app);
    let relay = ei_socket_file();
    let _ = std::fs::remove_file(&relay); // stale socket path from a previous session
                                          // Enable gamescope's Steam integration (`--steam`: in-game overlay, Steam+X shortcuts, gamepad-UI
                                          // navigation) whenever we're launching Steam — the operator no longer has to set the global
                                          // PUNKTFUNK_GAMESCOPE_STEAM knob for a Steam title. The knob still forces it on for every spawn.
    let steam_mode = pf_host_config::config().gamescope_steam || is_steam_launch(&app);
    // Opt-in relative-mouse capture for a nested game (`PUNKTFUNK_GAMESCOPE_GRAB_CURSOR`): the client
    // already sends relative motion, but gamescope only enters relative mode when the app hides the
    // cursor, which some FPS titles never signal over the injected pointer — grabbing fixes mouselook.
    // Default OFF (it forces relative mode, which would break absolute-pointer games/menus).
    let grab_cursor = game_launch && pf_host_config::config().gamescope_grab_cursor;
    // The splash client (see `gamescope/splash.rs`): without a painting client gamescope pushes NO
    // capture buffers, and a nested Steam bootstrap paints nothing for far longer than any
    // first-frame budget — so every bare spawn backgrounds the host's own splash beside the nested
    // app. Skipped only via the PUNKTFUNK_GAMESCOPE_SPLASH=0 escape hatch (or if the host can't
    // name its own executable, where the old starve-prone behaviour is still better than no spawn).
    let splash_exe = pf_host_config::config()
        .gamescope_splash
        .then(std::env::current_exe)
        .and_then(|r| {
            r.map_err(|e| tracing::warn!(error = %e, "gamescope: current_exe failed — no splash"))
                .ok()
        });
    let mut cmd = Command::new(gamescope_bin());
    add_bare_gamescope_args(&mut cmd, w, h, hz, steam_mode, grab_cursor, hdr);
    let script = nested_wrapper_script(&relay, splash_exe.is_some());
    cmd.args(["sh", "-c", &script, "sh"]);
    if let Some(exe) = &splash_exe {
        cmd.arg(exe);
    }
    cmd.args(app.split_whitespace())
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
    tracing::info!(
        w, h, hz, steam_mode, hdr,
        bin = %gamescope_bin(),
        splash = splash_exe.is_some(),
        %app,
        log = %log.display(),
        "spawning gamescope (headless)"
    );
    cmd.spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")
}

/// The nested-command wrapper script for a bare spawn: relay gamescope's `LIBEI_SOCKET` to the
/// injector's file, optionally background the splash client (`"$1"` is the host executable — passed
/// as an argv so its path never needs shell-escaping), then exec the real app. Pure + unit-tested.
fn nested_wrapper_script(relay: &std::path::Path, with_splash: bool) -> String {
    if with_splash {
        format!(
            "printf %s \"$LIBEI_SOCKET\" > '{}'; \"$1\" gamescope-splash & shift; exec \"$@\"",
            relay.display()
        )
    } else {
        format!(
            "printf %s \"$LIBEI_SOCKET\" > '{}'; exec \"$@\"",
            relay.display()
        )
    }
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
        cgroup_is_punktfunk_owned, cgroup_under_user_manager, connected_connector_under,
        display_manager_unit_under, dm_plan, dm_survives_masked_unit, game_hz, hdr_args,
        is_steam_launch, missing_flags, nested_wrapper_script, sentinel_advanced,
        shape_dedicated_command,
    };

    /// The HDR spawn flags are what make a nested game render HDR at all — and their absence is
    /// indistinguishable, on-glass, from a capture negotiation failure. Both flags are required:
    /// `--hdr-enabled` alone does nothing on the HEADLESS backend, whose connector hardcodes
    /// `SupportsHDR() == false`.
    #[test]
    fn hdr_spawn_flags_are_both_present_and_absent_for_sdr() {
        assert!(
            hdr_args(false).is_empty(),
            "an SDR spawn takes no HDR flags"
        );
        let args = hdr_args(true);
        assert!(args.iter().any(|a| a == "--hdr-enabled"));
        assert!(
            args.iter().any(|a| a == "--hdr-debug-force-support"),
            "without the force flag the headless connector reports no HDR support, so the WSI \
             layer advertises no HDR surfaces and games render SDR"
        );
    }

    #[test]
    fn user_manager_lifetime_detection() {
        // The packaged host: a `--user` unit, so logind's user-manager stop takes it down with the
        // login session the DM stop ends — this is the case that needs lingering.
        assert!(cgroup_under_user_manager(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-host.service\n"
        ));
        assert!(cgroup_under_user_manager(
            "0::/user.slice/user-1000.slice/user@1000.service/session.slice/punktfunk-gamescope.service\n"
        ));
        // A system unit outlives every session — the DM stop cannot reach it.
        assert!(!cgroup_under_user_manager(
            "0::/system.slice/punktfunk-host.service\n"
        ));
        // Started from a login shell: owned by the session scope, not the user manager.
        assert!(!cgroup_under_user_manager(
            "0::/user.slice/user-1000.slice/session-2.scope\n"
        ));
    }

    #[test]
    fn session_select_sentinel_needs_a_baseline() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let t1 = t0 + std::time::Duration::from_secs(1);
        // Never baselined: the sentinel is a permanent file, so a box whose user EVER switched
        // sessions has one — that ancient write is not a live "Switch to Desktop" request. This is
        // the case that pushed a Nobara box to the desktop after a failed managed launch.
        assert!(!sentinel_advanced(None, Some(t0)));
        assert!(!sentinel_advanced(None, None));
        // Baselined with no sentinel yet, then one appeared inside the session: a real request.
        assert!(sentinel_advanced(Some(None), Some(t0)));
        assert!(!sentinel_advanced(Some(None), None));
        // Baselined at an mtime: only a NEWER one is the user's in-stream switch. The write that
        // brought the box into game mode is the baseline itself, so it reads as no request.
        assert!(sentinel_advanced(Some(Some(t0)), Some(t1)));
        assert!(!sentinel_advanced(Some(Some(t0)), Some(t0)));
        assert!(!sentinel_advanced(Some(Some(t1)), Some(t0)));
        assert!(!sentinel_advanced(Some(Some(t0)), None));
    }

    #[test]
    fn nested_wrapper_script_shapes() {
        let relay = std::path::Path::new("/run/user/1000/pf-ei");
        // Plain: relay + exec, no splash machinery.
        let plain = nested_wrapper_script(relay, false);
        assert!(plain.contains("/run/user/1000/pf-ei"));
        assert!(plain.ends_with("exec \"$@\""));
        assert!(!plain.contains("gamescope-splash"));
        // Splash: `"$1"` is the host exe (an argv, never shell-interpolated), backgrounded and
        // shifted away so `exec "$@"` still runs the untouched app tokens.
        let splash = nested_wrapper_script(relay, true);
        assert!(splash.contains("\"$1\" gamescope-splash &"));
        assert!(splash.contains("shift; exec \"$@\""));
    }

    #[test]
    fn display_manager_flavor_detection() {
        let base = std::env::temp_dir().join(format!("pf-dm-scan-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // No alias symlink (no DM installed — getty autologin boxes) → None.
        assert_eq!(display_manager_unit_under(&base), None);
        // The Fedora-style alias symlink resolves to its target's basename (read_link, not
        // canonicalize — the target needn't exist on the build box).
        std::os::unix::fs::symlink(
            "/usr/lib/systemd/system/plasmalogin.service",
            base.join("display-manager.service"),
        )
        .unwrap();
        assert_eq!(
            display_manager_unit_under(&base).as_deref(),
            Some("plasmalogin.service")
        );
        // Only SDDM is proven to survive a masked session unit; plasmalogin start-limit-kills
        // itself (live-proven), and unknown DMs default to fragile.
        assert!(dm_survives_masked_unit("sddm.service"));
        assert!(!dm_survives_masked_unit("plasmalogin.service"));
        assert!(!dm_survives_masked_unit("gdm.service"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dm_plan_stops_any_dm_that_drove_a_live_session() {
        // SDDM, live gaming session: mask (belt-and-braces) AND stop the DM — the mask alone
        // does not stop the relogin loop on images whose sddm helper execs the session script
        // directly, bypassing the unit (fork storm, .41 VM 2026-07-31).
        let p = dm_plan(Some("sddm.service"), true);
        assert!(!p.skip && p.mask && p.stop_dm);
        // SDDM, only inactive leftovers: nothing live justifies touching the DM — mask+kill only.
        let p = dm_plan(Some("sddm.service"), false);
        assert!(!p.skip && p.mask && !p.stop_dm);
        // Mask-fragile flavor, live: stop the DM, never mask (masking start-limit-kills the DM).
        let p = dm_plan(Some("plasmalogin.service"), true);
        assert!(!p.skip && !p.mask && p.stop_dm);
        // Mask-fragile flavor, nothing live: hands off entirely — stopping the DM here would
        // kill the user's live desktop to free nothing.
        assert!(dm_plan(Some("plasmalogin.service"), false).skip);
        // No DM at all (getty autologin): mask+kill, nothing to stop.
        let p = dm_plan(None, true);
        assert!(!p.skip && p.mask && !p.stop_dm);
    }

    #[test]
    fn connector_status_scan() {
        let base = std::env::temp_dir().join(format!("pf-drm-scan-{}", std::process::id()));
        let mk = |name: &str, status: Option<&str>| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(s) = status {
                std::fs::write(dir.join("status"), s).unwrap();
            }
        };
        // Headless layout: device + render nodes only (no status files) → not connected.
        mk("card0", None);
        mk("renderD128", None);
        assert!(!connected_connector_under(&base));
        // Connectors present but nothing plugged in → still not connected.
        mk("card0-HDMI-A-1", Some("disconnected\n"));
        assert!(!connected_connector_under(&base));
        // A live panel → connected.
        mk("card0-eDP-1", Some("connected\n"));
        assert!(connected_connector_under(&base));
        // A missing base dir (no DRM at all) reads as headless.
        assert!(!connected_connector_under(&base.join("nope")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn steam_launch_detection() {
        assert!(is_steam_launch("steam steam://rungameid/570"));
        assert!(is_steam_launch("steam -silent steam://rungameid/570"));
        assert!(!is_steam_launch("vkcube"));
        assert!(!is_steam_launch("lutris lutris:rungameid/42"));
        // A `steam_ui` LAUNCHER entry (design D4) carries no URI, and must still count: it needs the
        // single instance freed (B1) and gamescope's `--steam` mode on. Gating on `steam://` would
        // have skipped both for the one launch that is Big Picture itself.
        assert!(is_steam_launch("steam -gamepadui"));
        assert!(is_steam_launch("steam"));
        // A command that merely mentions steam elsewhere is not a Steam client launch.
        assert!(!is_steam_launch("mygame --steam-overlay"));
    }

    #[test]
    fn dedicated_command_shaping() {
        // Steam URI → -gamepadui inserted so the nested Steam is Big Picture (not the desktop UI).
        assert_eq!(
            shape_dedicated_command("steam steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
        );
        // Idempotent: an already-gamepadui command is left alone.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
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
        // The `steam_ui` launcher entries (design D4) pass through untouched — the shaping only ever
        // fires on a `steam://` game launch, so there is no way to end up with `-gamepadui` twice.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui"),
            "steam -gamepadui"
        );
        assert_eq!(shape_dedicated_command("steam"), "steam");
    }

    #[test]
    fn game_hz_is_the_session_rate_until_the_limiter_is_set() {
        // The env is process-wide and `config()` is parsed once, so this asserts the DEFAULT
        // (nothing set) — which is the case that matters most: every existing host must keep
        // handing gamescope the client's own rate, untouched. `game_fps`'s own unit test in
        // pf-host-config covers the capping arithmetic without needing the env.
        if pf_host_config::config().max_fps.is_none() {
            for hz in [30, 60, 120, 144, 240] {
                assert_eq!(game_hz(hz), hz);
            }
        }
        // Never zero, whatever the inputs: gamescope would reject `-r 0`.
        assert!(game_hz(0) >= 1);
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

    /// The silent-cursor guard: a managed session that ignored `GAMESCOPE_BIN` / the PATH shim runs
    /// a stock gamescope, and the host — already told the compositor would paint the pointer —
    /// paints none either. Only a compositor we can SEE, missing a flag we can NAME, may fail.
    #[test]
    fn spawn_flag_verification_fails_closed_only_on_evidence() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };
        let want: Vec<String> = ["--hdr-enabled", "--pipewire-composite-cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // The flags arrived: nothing to report.
        assert!(missing_flags(
            &want,
            &[argv(
                "/usr/bin/punktfunk-gamescope --backend headless -W 1920 -H 1080 \
                 --hdr-enabled --hdr-debug-force-support --pipewire-composite-cursor"
            )]
        )
        .is_empty());

        // The session execed the distro binary — BOTH flags lost. This is the case that used to
        // stream a pointerless picture without a word.
        assert_eq!(
            missing_flags(
                &want,
                &[argv(
                    "/usr/bin/gamescope --backend headless -W 1920 -H 1080"
                )]
            ),
            vec!["--hdr-enabled", "--pipewire-composite-cursor"]
        );

        // A stock gamescope can take `--hdr-enabled` (it predates our patches) — so the HDR flag
        // alone proves nothing, and the cursor flag must be checked on its own.
        assert_eq!(
            missing_flags(
                &want,
                &[argv("/usr/bin/gamescope --hdr-enabled -W 1920 -H 1080")]
            ),
            vec!["--pipewire-composite-cursor"]
        );

        // Fail OPEN when we could not look: an unreadable `/proc` is not evidence of anything, and
        // treating it as a miss would fail every managed session on a hardened box.
        assert!(missing_flags(&want, &[]).is_empty());

        // Several gamescopes running (a nested game under the session): the flags need only be on
        // ONE of them — the session compositor.
        assert!(missing_flags(
            &want,
            &[
                argv("/usr/bin/gamescope -W 800 -H 600"),
                argv("/usr/bin/punktfunk-gamescope --hdr-enabled --pipewire-composite-cursor"),
            ]
        )
        .is_empty());
    }
}
