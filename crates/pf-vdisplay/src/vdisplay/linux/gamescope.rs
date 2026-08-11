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
    gamescope_can_composite_external_overlay, gamescope_can_offer_refresh_rates,
    gamescope_node_present, poll_managed_node, wait_for_node,
};
pub(crate) use discovery::{
    game_session_exited, gamescope_can_composite_cursor, gamescope_hdr_capable, is_available,
    note_spawn_flags_lost, steam_appid_from_launch, wait_for_steam_game_exit, SteamGameWatch,
};
pub(crate) use heads::list_monitors;
pub(crate) use splash::run as splash_run;

/// The gamescope virtual-display driver. Three sub-modes, resolved **per session** by
/// [`crate::resolve_gamescope_route`] and handed over through
/// [`VirtualDisplay::set_gamescope_route`] — never read out of the process env here, because a
/// second connect resolving its own route would otherwise retarget this session between the
/// decision and its `create` (Phase 2.3):
/// * **Managed** — host-MANAGE a `gamescope-session-plus`/SteamOS session (full Steam-Deck-UI
///   polish) headless at the CLIENT's mode; relaunch it when the mode changes.
/// * **Attach** — attach to an already-running gamescope (capture + inject, no lifecycle
///   ownership).
/// * **Spawn** — a bare headless gamescope sized to the mode, running the per-session launch
///   command ([`VirtualDisplay::set_launch_command`]) and falling back to
///   `PUNKTFUNK_GAMESCOPE_APP` only when that is empty.
///
/// `PUNKTFUNK_GAMESCOPE_{MANAGED,ATTACH,NODE,SESSION}` still exist, but purely as **operator
/// overrides** feeding that ladder — sampled once at first use in `routing::operator_gamescope`,
/// never republished.
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

/// Serialises the managed-session LAUNCH in [`create_managed_session`] — and nothing else.
///
/// [`MANAGED_SESSION`] used to be held from the same-mode test all the way across `launch_session`,
/// which gave two things at once: the tracked state, and mutual exclusion between two connects. That
/// wide hold is exactly what stranded a box on shutdown (`do_restore_tv_session` needs the same
/// mutex and could sit behind a ~90 s launch until `native.rs`'s 20 s grace expired and `exit(0)`
/// skipped the display-manager restore), so the decision was narrowed to a scope of its own — but
/// the mutual exclusion went with it. Two clients resolving the Managed route inside one launch
/// window then BOTH relaunched, and `launch_session`'s first act is `stop_session(SESSION_UNIT)`:
/// the second connect tore down the session the first was still polling for.
///
/// This lock restores only the exclusion. LOCK ORDER: `MANAGED_LAUNCH` → [`MANAGED_SESSION`], never
/// the reverse, and **no restore path may ever take it** — that is what keeps the shutdown restore
/// off the launch's critical path, which is the whole point of the narrowing.
static MANAGED_LAUNCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Autologin gaming-mode `gamescope-session-plus@*` units we stopped on connect to free Steam
/// (single-instance), so [`schedule_restore_tv_session`] can restart them when the client disconnects.
static STOPPED_AUTOLOGIN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The display-manager unit we stopped for the takeover (any DM that drove a LIVE gaming session
/// is stopped for the stream — see [`dm_plan`]), so the restore brings the box back via
/// `reset-failed` + `restart` of the DM instead of a `--user start` of the gamescope unit (which
/// cannot work on a mask-fragile flavor: without a DM login session there is no seat, so gamescope
/// never gets DRM master — live-proven on the Nobara repro VM 2026-07-24).
static STOPPED_DM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Whether this takeover runtime-masked the [`STOPPED_AUTOLOGIN`] units ([`mask_unit`]) — i.e.
/// whether there is a mask left to lift. Process memory, like the rest of the takeover mechanics.
/// [`restore_takeover_on_startup`] sets it for a stranded takeover it adopts: unmasking a unit we
/// never masked is a no-op, while missing one that IS masked leaves the box unable to enter its
/// own Game Mode until reboot.
static AUTOLOGIN_MASKED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

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

/// This spawn instance's log path, under the resolved per-user runtime dir
/// ([`crate::session::runtime_dir`] — `$XDG_RUNTIME_DIR` when set and non-empty, else
/// `/run/user/<uid>`; **never** `/tmp`, which Phase 4.2 removed from every one of these paths as a
/// world-writable-directory defect). Replaces the shared `/tmp/punktfunk-gamescope.log` so
/// concurrent spawns don't clobber each other's `stream available on node ID:` line.
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

/// Set once this host has written the `gamescope-session-plus@` bind drop-in over the BOX's own
/// autologin template ([`write_session_plus_dropin`] from [`ensure_box_gamescope_mode`]) — i.e.
/// whether there is a drop-in left to take away.
///
/// It needs its own flag because that path steals nothing: no autologin unit is stopped, no DM, no
/// SteamOS target, no managed session — so every one of the takeover statics stays empty and
/// [`takeover_live`] used to answer `false`. The drop-in then outlived the stream AND the host:
/// nothing in-process ever removed it, and from that point the operator's own Game Mode ran
/// punktfunk's patched gamescope inside our mount+user namespace, with `/tmp/.X11-unix` replaced
/// and the last client's HDR/refresh flags applied. Only the next host START swept it
/// ([`restore_takeover_on_startup`]), which is why it read as an anomaly "from a previous host
/// instance" — it was this one.
static SESSION_DROPIN_ARMED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Set once this host has pushed `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` into the
/// `systemd --user` manager's environment to re-mode the box's OWN game-mode session
/// ([`ensure_box_gamescope_mode`]). Those are process-global to the user manager and survive every
/// unit restart, so a stream that ended left the box's own Game Mode pinned at the last client's
/// resolution for the rest of the login — the restore has to `unset-environment` them, and may
/// only do so for values it put there itself (an operator's own `set-environment` is theirs).
static FORCED_SESSION_SCREEN_ENV: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

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
    /// Whether a host-managed [`SESSION_UNIT`] was running. It steals nothing — it is the managed
    /// session started BESIDE a live desktop (a client gamescope pin on a KDE box) — but it is
    /// still ours to end, and [`takeover_live`] already counts it for exactly that reason. Without
    /// it here, a host that crashed with such a session left the transient unit running with no
    /// record anywhere: the next start read the three stealing fields as empty and cleared the
    /// file, so nothing ever stopped it ("closing the app does not end the session" survives a host
    /// restart). Restored as an impossible-mode marker, never as a reusable session — see
    /// [`restore_takeover_on_startup`].
    #[serde(default)]
    managed_session: bool,
    /// Whether this host forced `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` into the
    /// `systemd --user` manager to re-mode the box's own game-mode session
    /// ([`FORCED_SESSION_SCREEN_ENV`]). Unlike [`SESSION_DROPIN_ARMED`] — runtime-dir state the next
    /// host start sweeps unconditionally — these live in the MANAGER, which outlives this process
    /// and every unit restart for the rest of the login. Nothing sweeps them and nothing may sweep
    /// them blind (`unset-environment` is indiscriminate; an operator's own `set-environment` is
    /// theirs), so this flag is the only way a host that was SIGKILLed/OOM-killed/updated in place
    /// can know they are ours to take back. Without it the operator's Game Mode came up at the last
    /// client's resolution for the remainder of that login, with no record and no code path that
    /// would ever undo it. `default` so takeover files from older hosts still parse.
    #[serde(default)]
    forced_screen_env: bool,
}

/// Path of the persisted [`TakeoverState`], under `$XDG_RUNTIME_DIR` (per-user, 0700, tmpfs — cleared
/// on reboot, which is correct: a reboot restarts the autologin itself).
fn takeover_state_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-session-takeover.json")
}

/// Persist the current takeover mechanics so a host crash doesn't strand the box out of gaming mode.
/// Best-effort (a write failure just loses crash-restore, not correctness).
/// ⚠ Never call this while holding any of the four statics it samples — [`MANAGED_SESSION`]
/// included, which is new here. Each is taken and released in turn (one lock at a time, as
/// [`takeover_live`] does), and a caller holding one would deadlock on itself: `std::sync::Mutex`
/// is not reentrant.
fn persist_takeover() {
    let state = TakeoverState {
        stopped_autologin: STOPPED_AUTOLOGIN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        steamos: *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()),
        stopped_dm: STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        managed_session: MANAGED_SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some(),
        forced_screen_env: *FORCED_SESSION_SCREEN_ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    };
    if !takeover_state_is_live(&state) {
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

/// Does this [`TakeoverState`] describe anything a restore would have to undo? The file-side twin
/// of [`takeover_live`], and split out pure because the same test used to be written longhand in
/// two places and drifted from the in-memory one: `persist_takeover` and
/// `restore_takeover_on_startup` both tested only the three *stealing* fields, so a state whose
/// sole content was a managed session was written as "nothing to restore" and deleted on the spot.
///
/// It is narrower than [`takeover_live`] by exactly ONE flag, and the reason is which side of the
/// process boundary the state lives on. [`SESSION_DROPIN_ARMED`] is a file in the runtime dir that
/// [`restore_takeover_on_startup`] removes unconditionally in its prologue, so persisting it would
/// only make the file exist for a restore with nothing left to do. The forced `SCREEN_*`
/// ([`FORCED_SESSION_SCREEN_ENV`]) are NOT that: they live in the `systemd --user` manager, nothing
/// sweeps them, and the earlier version of this comment claimed otherwise — which is what licensed
/// leaving them out of the file, so the one mechanism built to survive a crash excluded the one
/// piece of state that most needs it. They are persisted and counted here now.
fn takeover_state_is_live(state: &TakeoverState) -> bool {
    !state.stopped_autologin.is_empty()
        || state.steamos
        || state.stopped_dm.is_some()
        || state.managed_session
        || state.forced_screen_env
}

/// On host startup, restore the TV's gaming session if a previous host instance took it over and
/// crashed before restoring (`design/gamemode-and-dedicated-sessions.md` A3). Loads the persisted
/// [`TakeoverState`] into the statics and schedules a restore after a short reconnect grace (so a
/// client reconnecting right after the restart keeps the streamed session instead of bouncing the
/// box back to gaming mode). No-op when no takeover file exists (a clean start). Call once from
/// `serve` alongside [`start_restore_worker`].
pub fn restore_takeover_on_startup() {
    // BEFORE anything else, and unconditionally — this one is not about a takeover.
    //
    // The `gamescope-session-plus@` bind drop-in ([`session_plus_dropin_path`]) applies to the
    // TEMPLATE, so it also reaches the unit the box autologs into at boot, and every path it names
    // lives in tmpfs. A copy left behind by a host that died — or by 0.26.0-canary, which wrote it
    // under `$HOME` where a reboot could not clear it — therefore asks the box's own Game Mode for
    // a mount namespace whose bind sources no longer exist. That is a box that cannot enter Game
    // Mode at all, with no punktfunk running to explain it. Clearing it here is the upgrade path
    // for every box that already ran that build.
    if remove_session_plus_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover gamescope-session-plus bind drop-in from a previous \
             host instance — it asks the box's OWN Game Mode session for a mount namespace, and \
             everything it binds lives in tmpfs, so after a reboot that unit could not start at all"
        );
        systemctl_user(&["daemon-reload"]);
    }
    let Ok(bytes) = std::fs::read(takeover_state_path()) else {
        return; // no takeover file — clean start
    };
    let Ok(state) = serde_json::from_slice::<TakeoverState>(&bytes) else {
        clear_takeover();
        return;
    };
    if !takeover_state_is_live(&state) {
        clear_takeover();
        return;
    }
    tracing::warn!(
        units = ?state.stopped_autologin,
        steamos = state.steamos,
        stopped_dm = ?state.stopped_dm,
        managed_session = state.managed_session,
        forced_screen_env = state.forced_screen_env,
        "gamescope: found a stranded takeover from a previous host instance — scheduling TV restore"
    );
    // Assume the adopted takeover carries our runtime mask whenever it stopped units: whether it did
    // is not persisted (it follows from the DM flavor, which can only have stayed the same), and the
    // two errors are not symmetric — unmasking a unit we never masked is a no-op, while skipping one
    // that IS masked bars the box from its own game mode until reboot.
    *AUTOLOGIN_MASKED.lock().unwrap_or_else(|e| e.into_inner()) =
        !state.stopped_autologin.is_empty();
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_autologin;
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = state.steamos;
    *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_dm;
    // [`SESSION_DROPIN_ARMED`] is deliberately NOT persisted and not adopted here: the sweep at the
    // top of this function already removed the drop-in (from both of its homes) and reloaded
    // systemd, unconditionally and before anything else — so there is nothing left for a restored
    // flag to owe.
    //
    // The forced SCREEN_* are the opposite case and ARE adopted: the previous host put them in the
    // user manager, which this process did not restart and cannot sweep blind, so only this flag can
    // authorise the `unset-environment` the scheduled restore owes ([`unset_forced_session_screen_env`]
    // no-ops without it). Adopting a `false` is also correct — it means the crashed host never forced
    // them, and unsetting an operator's own values would be a fresh bug.
    *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = state.forced_screen_env;
    if state.managed_session {
        // An ADOPTED managed session is something to STOP, never something to reuse: the mode it
        // was launched at is not persisted (and a wrong one would hand the next client a session at
        // someone else's resolution while reporting a same-mode reuse). The impossible 0x0/0 Hz
        // marker can never satisfy `create_managed_session`'s equality test, so every route through
        // it relaunches — while `takeover_live` and `do_restore_tv_session`'s nothing-was-stolen arm
        // both see a session that owes the box a `stop`.
        *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
            width: 0,
            height: 0,
            refresh_hz: 0,
            hdr: false,
        });
    }
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
        // Only a bare SPAWN is registry-poolable (its `create` reports `Owned`); Managed and
        // Attach report `SessionManaged`/`External`, so the registry must not reuse a kept spawn
        // for them (same backend name). `None` is poolable because `create`'s own `None` arm falls
        // through to the bare spawn — the invariant to keep is "agrees with what `create` does with
        // the same route", NOT "agrees with [`crate::launch_is_nested`]", which answers `false` for
        // `None`. Reads THIS session's resolved route (handed over by `set_gamescope_route`): no
        // env read, and therefore no env lock — Phase 2.3 deleted both the read and the writer that
        // used to publish `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION` back into the process env.
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
                ensure_box_gamescope_mode(mode, self.hdr)?
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

        // Resolve the app ONCE, HERE — before the gate, and hand the answer to [`spawn`]. The gate
        // used to test `self.cmd` alone while `spawn` resolved the `PUNKTFUNK_GAMESCOPE_APP`
        // fallback afterwards and recognised it as Steam (`steam_mode`), so the documented
        // `PUNKTFUNK_GAMESCOPE_APP="steam -gamepadui"` route got gamescope's `--steam` mode with
        // NO instance free — and then collided with the box's own autologin/desktop Steam, which
        // is precisely the collision this block exists to prevent.
        let app = resolved_spawn_app(self.cmd.as_deref());
        if app.as_deref().is_some_and(is_steam_launch) {
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
            app,
            &log,
            self.hdr,
        )?;
        let mut proc = GamescopeProc {
            child,
            log: log.clone(),
        };
        // gamescope creates its PipeWire node a moment after start; poll for it (the proc is held
        // alive meanwhile, and killed if we give up). Discovery reads THIS spawn's log, the
        // fallback is scoped to this spawn's process tree, and it gives up early if the process is
        // already gone — a `vkCreateDevice` failure exits in under a second, and the 15 s the wait
        // used to spend on its corpse produced a timeout error that blamed the GPU instead.
        let node_id =
            wait_for_node(Duration::from_secs(15), &log, &mut proc.child).ok_or_else(|| {
                anyhow!(
                    "gamescope published no PipeWire node within 15s (or exited first) — it may \
                     have failed to start, or headless capture may be unsupported on this \
                     GPU/driver; its own log says which (see {})",
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
        // Read the tracked mode in a scope of its own: everything below is a `pw-dump` and a file
        // write, and holding MANAGED_SESSION across either puts a probe on the restore worker's
        // critical path for no gain.
        let same_mode = {
            let guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().is_some_and(|s| {
                s.width == mode.width
                    && s.height == mode.height
                    && s.refresh_hz == mode.refresh_hz
                    && s.hdr == hdr
            })
        };
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
        let node_id = ensure_box_gamescope_mode(mode, hdr)?;
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
    // DECIDE under the lock, ACT outside it — the shape `session::observe_session_instance` and the
    // registry already use, and the shape `create_managed_session_steamos` adopted for exactly this
    // reason (see its LOCK ORDER note). The guard used to be held across `find_gamescope_node` (an
    // unbounded `pw-dump`), `point_injector_at_eis` and — the long one — `launch_session`, whose two
    // 45 s node-poll deadlines plus `systemd-run` round trips make ~90 s an ordinary duration.
    // Meanwhile `do_restore_tv_session` needs the same mutex, and reaches it only AFTER it has
    // stopped our unit and consumed the takeover: on a `systemctl --user restart punktfunk-host`
    // during a launch, the shutdown restore blocked there while `native.rs`'s 20 s
    // SHUTDOWN_RESTORE_GRACE ran out and `exit(0)` skipped the display-manager restore entirely —
    // a box with its DM stopped, no graphical session, and nothing left on disk to heal it.
    //
    // The exclusion the wide hold ALSO provided is not free, though, and dropping it silently was
    // this shape's own regression: two connects at the same mode inside one launch window both saw
    // no tracked session, both relaunched, and the second `stop_session(SESSION_UNIT)`'d the unit the
    // first was polling for. [`MANAGED_LAUNCH`] takes that duty back on its own — held from BEFORE
    // the decision (so the second arrival re-tests it only once the first has recorded its result,
    // and then reuses) to the end of the function, and touched by no restore path.
    let _launching = MANAGED_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = {
        let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let same = guard.as_ref().is_some_and(|s| {
            s.width == mode.width
                && s.height == mode.height
                && s.refresh_hz == mode.refresh_hz
                && s.hdr == hdr
        });
        // Clear it up front on a mode change: from here on the tracked session is about to be torn
        // down by `launch_session` anyway, and a concurrent restore must not read it as live.
        //
        // Residual, and deliberately preferred to the block it replaces: for the duration of the
        // launch a managed session that stole NOTHING (started beside a live desktop) is invisible
        // to `takeover_live`, so a restore firing inside that window returns early instead of
        // waiting for the launch and then stopping the unit. The launch's own failure arm arms a
        // restore, and a success records the session for the next disconnect — whereas the held
        // guard could strand a whole box with its display manager down.
        if !same {
            *guard = None;
        }
        same
    };
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
        *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    // (Re)launch at the new mode, holding NOTHING. `launch_session` stops the old unit by name
    // first, so there is exactly one gamescope `Video/Source` node for discovery.
    let node_id = match launch_session(client, SESSION_UNIT, mode, hdr) {
        Ok(id) => id,
        Err(e) => {
            // The takeover already happened (autologin units stopped, possibly the DM down) — arm
            // the restore now, or a failed launch strands the box sessionless until a host
            // restart. Policy-timed; a quick client retry cancels it and relaunches warm.
            // MANAGED_SESSION is already released (the scheduler reads it for orphan detection).
            schedule_restore_tv_session();
            return Err(e);
        }
    };
    // Baseline the session-select sentinel NOW: only a write from INSIDE this session (the user's
    // "Switch to Desktop") should read as a switch request, not the one that led here.
    record_session_select_baseline();
    point_injector_at_eis();
    // Re-acquire only to record the result — the same shape `create_managed_session_steamos` uses.
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
    // A3, and the reason `TakeoverState` carries a `managed_session` flag at all: on a box where
    // this session stole NOTHING (a client gamescope pin beside a live desktop) the only earlier
    // `persist_takeover` — `stop_autologin_sessions`' — wrote an all-empty state and DELETED the
    // file. A host that then crashed left the transient unit running with no record of it anywhere.
    // Called after the guard above has dropped: `persist_takeover` samples the same mutex.
    persist_takeover();
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
        // Resolved, not a raw `comm` read: nixpkgs wraps gamescope too, so on NixOS the kernel
        // reports `.gamescope-wrap` and this probe saw no foreign session at all.
        let Some(comm) = crate::proc::match_name(&e.path()) else {
            continue;
        };
        if !matches!(comm.as_str(), "gamescope" | "gamescope-wl") {
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

/// Budget for a cheap unit STATE query (`systemctl is-active`). These answer from the manager's
/// in-memory state, so tens of milliseconds is the normal cost. Several sit inside 10–45 s poll
/// loops, where an unbounded one blows the whole deadline in a single tick.
///
/// ⚠ Only for callers whose timeout answer is the SAFE one. Both current callers time out into
/// "assume active"/"keep looping", so a miss costs a poll tick. A caller whose timeout would invert
/// the answer into the refusing direction must NOT use this bound — `loginctl show-user -p Linger`
/// was given it and became a hard connect failure on a correctly-configured box (see
/// [`linger_enabled`], now on [`UNIT_QUERY_BUDGET`]): 300 ms is an in-memory-read budget, and
/// anything that spawns a process and makes a D-Bus round trip is not that.
const UNIT_STATE_BUDGET: Duration = Duration::from_millis(300);

/// Budget for an enumeration or a `loginctl` write (`systemctl list-units`, `enable-linger`) —
/// heavier than a state query (it walks every loaded unit, and lingering goes through polkit) but
/// still a local round trip.
const UNIT_QUERY_BUDGET: Duration = Duration::from_secs(5);

/// Budget for a unit LIFECYCLE verb on the user manager: `start`/`stop`/`restart`/`kill`,
/// `daemon-reload`, `mask`/`unmask`, `reset-failed`, `set-environment`, `systemd-run --user`.
/// Generous, because a stop job legitimately waits on the unit's own teardown — but bounded,
/// because every one of these runs on a stream thread or the shutdown restore, and
/// [`crate::proc`]'s module doc is about precisely that: a hung helper pins the thread whose only
/// way to end a session is to return.
const UNIT_VERB_BUDGET: Duration = Duration::from_secs(10);

/// Budget for a SYSTEM-bus display-manager verb ([`systemctl_system`]) and for the distro's own
/// session-switch helper. Much larger than [`UNIT_VERB_BUDGET`]: stopping a DM tears down a whole
/// seat's worth of sessions, and a premature kill here would abandon a takeover half-done. The
/// bound still matters — the callers fall through to the pkexec helper on failure, so the worst a
/// timeout costs is one redundant helper call, against a wedge that would otherwise be permanent.
const DM_VERB_BUDGET: Duration = Duration::from_secs(30);

/// Run a `systemctl --user` subcommand best-effort — a failure just means the session won't change,
/// which the caller's node-wait surfaces.
///
/// Status-blind by design (every caller here treats it as fire-and-forget), but NOT time-blind: it
/// is the shared chokepoint behind daemon-reload/restart/stop/set-environment across this file, so
/// one wedged user manager used to pin the stream thread through any of them.
fn systemctl_user(args: &[&str]) {
    let _ = crate::proc::status_within(
        Command::new("systemctl").arg("--user").args(args),
        UNIT_VERB_BUDGET,
    );
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

/// Drop-in for the box's OWN autologin `gamescope-session-plus@*.service`.
///
/// The transient-unit path ([`launch_session`]) can pass `BindReadOnlyPaths` straight to
/// `systemd-run`, but a box that owns an autologin session is RESTARTED in place instead — no
/// `systemd-run`, so the bind has to arrive as a drop-in or Nobara's hardcoded
/// `/usr/bin/gamescope` wins there too (see [`DISTRO_GAMESCOPE_PATH`]).
///
/// **In `$XDG_RUNTIME_DIR`, not `$HOME`**, and that placement is load-bearing. This drop-in
/// applies to the whole `gamescope-session-plus@` TEMPLATE, so it also reaches the unit the box
/// autologs into at every boot — and both paths it names live in tmpfs. A copy left in
/// `~/.config/systemd/user/` therefore survives a reboot that deletes everything it points at, and
/// a `BindReadOnlyPaths=` whose source is gone fails the unit outright: the box can no longer
/// enter Game Mode at all, with no punktfunk running and nothing to say why.
/// `$XDG_RUNTIME_DIR/systemd/user/` is on systemd's user unit search path and dies with the login
/// session, so a stale one cannot outlive the host that wrote it. (Drop-ins are collected from
/// every directory on that path, so the lower search priority of the runtime directory costs
/// nothing — and if a systemd ever ignored it, the session would simply run the distro's stock
/// gamescope, which is the safe direction.)
///
/// `zz-` so it sorts last, matching the SteamOS drop-in convention above.
fn session_plus_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// Where 0.26.0-canary wrote the same drop-in: under `$HOME`, where it outlives the runtime paths
/// it names. Every box that ran that build has one on disk right now, and it is what leaves the
/// box unable to start Game Mode after a reboot — so this host removes it wherever it finds it
/// ([`remove_session_plus_dropin`]), including once at startup before anything else runs.
fn legacy_session_plus_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// Write the box-session drop-in carrying the same two fixes the transient path gets: the bind, and
/// the WSI opt-out when the box's layer was built for a different gamescope. `PF_HZ`/`PF_HDR_ARGS`
/// ride along because the wrapper reads them (without `PF_HZ` it falls back to 60).
///
/// When there is no bind to arm ([`arm_session_bind`] said no — the usual answer on every distro
/// whose session script honours `GAMESCOPE_BIN`) it REMOVES any drop-in instead of writing an
/// inert one, and reports `Ok(false)`. That is not tidiness: everything else in this file is an
/// `Environment=`, and the whole cost of the mechanism — the mount namespace, and the user
/// namespace that comes with it — is bought by the bind alone. A drop-in that kept only the env
/// lines would be harmless; one that kept the bind while the host had decided against it would be
/// the crash-loop this backstop exists to prevent.
fn write_session_plus_dropin(
    wrapper: &std::path::Path,
    mode: Mode,
    hdr: bool,
    wsi_ok: bool,
) -> Result<bool> {
    let Some(bind) = arm_session_bind(wrapper) else {
        remove_session_plus_dropin();
        return Ok(false);
    };
    let path = session_plus_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "[Service]\n\
         {binds}\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {wsi}",
        binds = bind.unit_lines(),
        hz = game_hz(mode.refresh_hz),
        hdr_args = hdr_args(hdr)
            .into_iter()
            .chain(cursor_args())
            .collect::<Vec<_>>()
            .join(" "),
        wsi = if wsi_ok {
            String::new()
        } else {
            wsi_off_unit_lines()
        },
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))?;
    Ok(true)
}

/// Remove the box-session drop-in from BOTH homes it has had — the runtime one this host writes
/// and the `$HOME` one 0.26.0-canary wrote (see [`legacy_session_plus_dropin_path`]). Best-effort,
/// mirroring [`remove_steamos_dropin`]; reports whether anything was actually removed, because the
/// caller then owes systemd a `daemon-reload` (a removal that isn't reloaded still applies at the
/// unit's next start, which for the box's own session unit means its next BOOT).
fn remove_session_plus_dropin() -> bool {
    // A loop, not an `any`/`all`: BOTH paths must be tried every time. Short-circuiting on the
    // first success would leave the other one — and the one it would leave is the `$HOME` copy that
    // outlives a reboot.
    let mut removed = false;
    for path in [
        session_plus_dropin_path(),
        legacy_session_plus_dropin_path(),
    ] {
        removed |= std::fs::remove_file(&path).is_ok();
    }
    removed
}

/// [`remove_session_plus_dropin`] **plus** clearing [`SESSION_DROPIN_ARMED`] and the
/// `daemon-reload` the removal owes systemd — the one call every hand-back path should make, so
/// that "the box's own unit template is clean again" and "we no longer think we owe it a removal"
/// can never disagree. A removal systemd has not reloaded still applies at the unit's next start,
/// which for the box's own autologin unit means its next BOOT.
fn disarm_session_plus_dropin() {
    let removed = remove_session_plus_dropin();
    *SESSION_DROPIN_ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
}

/// Take back the `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` this host forced into the
/// user manager for a re-moded box session ([`FORCED_SESSION_SCREEN_ENV`]). No-op unless we set
/// them — `unset-environment` is indiscriminate, and an operator who set their own must keep it.
/// `session.rs`'s `scrub_desktop_manager_env` does exactly this for `WAYLAND_DISPLAY`/`DISPLAY`;
/// these three had no such counterpart at all, which is why they outlived every stream.
fn unset_forced_session_screen_env() {
    let mut forced = FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !*forced {
        return;
    }
    *forced = false;
    drop(forced);
    systemctl_user(&[
        "unset-environment",
        "SCREEN_WIDTH",
        "SCREEN_HEIGHT",
        "CUSTOM_REFRESH_RATES",
    ]);
    tracing::info!(
        "gamescope: unset the forced SCREEN_WIDTH/SCREEN_HEIGHT/CUSTOM_REFRESH_RATES — the box's \
         own game mode is back on its own resolution"
    );
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

    // BOTH failure exits below arm the restore before returning, exactly as
    // `create_managed_session` does at its own post-takeover failure (see its `Err` arm's comment,
    // verbatim: "The takeover already happened … or a failed launch strands the box sessionless
    // until a host restart"). The takeover here has ALREADY happened — `gamescope-session.target`
    // is restarted headless and STEAMOS_TOOK_OVER is set — so a bare `?` left the Deck in a
    // headless session with its panel dark, PENDING_RESTORE unset, and no in-process path that
    // would ever run `do_restore_tv_session`: the GameStream plane calls `cancel_pending_restore`
    // on connect and never `restore_managed_session`, so it stayed dark until the host exited or
    // restarted, with each of the 8 pipeline retries restarting the target again.
    // MANAGED_SESSION is already released (the `drop(guard)` above), which is what the scheduler's
    // orphan-detection read needs.
    let node_id = match poll_managed_node(Duration::from_secs(30)) {
        Some(id) => id,
        None => {
            schedule_restore_tv_session();
            bail!(
                "SteamOS headless gamescope node did not appear within 30s after restarting \
                 {STEAMOS_SESSION_TARGET} — check `journalctl --user -u gamescope-session.service`"
            );
        }
    };
    // The shim is only a PATH entry — confirm the session actually took it before we trust the
    // capabilities the plan was already built on (a stock gamescope here means no HDR and, worse,
    // a silently pointerless stream). Leaves the tracked state unset on failure, so the retry does
    // a clean restart rather than a same-mode reuse of a session we just rejected.
    if let Err(e) = verify_managed_spawn_flags(hdr) {
        schedule_restore_tv_session();
        return Err(e);
    }
    point_injector_at_eis();
    // Re-acquire to record the tracked session — the same shape `create_managed_session` uses.
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
    // Re-persist now that the tracked session exists: the write above the node poll recorded the
    // takeover, this one records that a session came out of it.
    persist_takeover();
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
fn ensure_box_gamescope_mode(mode: Mode, hdr: bool) -> Result<u32> {
    let target = (mode.width, mode.height);
    // Sampled ONCE and read as a three-state answer ([`BoxOutputSize`]) — never as `== Some(target)`,
    // which made "cannot tell" mean "restart the box's session".
    let size = box_output_size();
    // Fast path: already at the client's resolution — just attach to the live node.
    if size == BoxOutputSize::Known(target) {
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
    //
    // The guard is on the DECISION, not on finding a node: a momentarily absent node (the session
    // restarting, a `pw-dump` that timed out) used to fall straight THROUGH this `if` into
    // `set-environment SCREEN_WIDTH/HEIGHT` + a unit restart — doing on a panel exactly what the
    // paragraph above forbids, and leaving the forced SCREEN_* behind in the user manager
    // afterwards. Refuse instead, the same shape the attach-only rebuild probe above uses: the
    // caller's retry re-asks once the node is back.
    if physical_display_connected() {
        let node = find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "the box drives a physical display, so its game-mode session is mirrored at its \
                 OWN mode — and it publishes no gamescope Video/Source node right now. Refusing to \
                 re-mode it to {}x{}: that would flip the screen someone is looking at and, on a \
                 DM-driven box, bounce the login session with it",
                mode.width,
                mode.height
            )
        })?;
        tracing::info!(
            node,
            client_w = mode.width,
            client_h = mode.height,
            "gamescope: box drives a physical display — attaching at its own mode (no re-mode)"
        );
        return Ok(node);
    }
    // AMBIGUOUS — two coexisting gamescopes at different sizes, so we cannot say which one the box's
    // session unit owns. Everything below this line is destructive (`set-environment` + a restart of
    // the box's OWN autologin unit), and the commonest way to reach ambiguity is the most expensive
    // one to be wrong about: a game running in a nested per-title gamescope (`gamescope -W … --
    // %command%`) inside a session that may well ALREADY be at the client's resolution. Restarting
    // there kills the user's running game on every connect.
    //
    // So mirror the live node at whatever mode it is at — the same non-destructive degrade the
    // physical-display arm above takes, and the same outcome the pre-`Option` code produced here
    // whenever its arbitrary first-`/proc`-entry pick happened to be the session compositor. Refusing
    // instead (the reviewer's first suggestion) would fail EVERY connect for as long as the nested
    // gamescope lives, i.e. for the whole game — a retry cannot converge on a state that only ends
    // when the user quits the game.
    if size == BoxOutputSize::Ambiguous {
        let node = find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "two gamescopes are running at different output sizes and neither publishes a \
                 Video/Source node right now — refusing to re-mode the box's session to {}x{} \
                 without knowing which one it is (re-detection follows the live session)",
                mode.width,
                mode.height
            )
        })?;
        tracing::warn!(
            node,
            client_w = mode.width,
            client_h = mode.height,
            "gamescope: two coexisting gamescopes disagree on the output size (a game nested in the \
             session is the usual cause) — attaching at the live node's own mode instead of \
             restarting the box's session under it"
        );
        return Ok(node);
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
        from = ?size,
        to_w = mode.width,
        to_h = mode.height,
        hz = mode.refresh_hz,
        %unit,
        "gamescope: relaunching the box game-mode session at the client's resolution"
    );
    // The session reads SCREEN_WIDTH/HEIGHT (+ CUSTOM_REFRESH_RATES) from the user-manager
    // environment; set them and restart the box's own unit. Record that we did: the manager keeps
    // them for the rest of the login, so the restore owes them an `unset-environment`
    // ([`unset_forced_session_screen_env`]) or the box's own Game Mode stays at the client's
    // resolution long after the stream ended.
    *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = true;
    systemctl_user(&[
        "set-environment",
        &format!("SCREEN_WIDTH={}", mode.width),
        &format!("SCREEN_HEIGHT={}", mode.height),
        &format!("CUSTOM_REFRESH_RATES={}", mode.refresh_hz.max(1)),
    ]);
    // …and persist that, which is new: this path steals nothing, so it used to write no takeover
    // file at all, and the three variables above outlive this process. A host SIGKILLed / OOM-killed
    // / updated in place left the operator's own Game Mode pinned at the last client's resolution
    // for the rest of the login with nothing on disk saying who did it. Called with no takeover
    // static held (the guard above dropped at its semicolon) — `persist_takeover` samples them all.
    persist_takeover();
    // Same two fixes the transient path gets, but this unit is the BOX's own — they have to arrive
    // as a drop-in, and `daemon-reload` before the restart or systemd runs the old unit.
    let mut bound = match write_gamescope_bin_wrapper()
        .and_then(|w| write_session_plus_dropin(&w, mode, hdr, wsi_layer_matches_our_gamescope()))
    {
        Ok(true) => {
            // Record it BEFORE the restart, and persist it: from this instant the box's OWN
            // autologin template carries our bind, and only a restore takes it back. Nothing else
            // on this path sets a takeover static, so without this flag `takeover_live()` answered
            // `false` and neither the disconnect restore nor the shutdown one ever ran — the
            // drop-in outlived the stream and the host process both.
            *SESSION_DROPIN_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = true;
            tracing::info!(
                bin = %gamescope_bin(),
                %unit,
                "gamescope: dropped in a bind over {DISTRO_GAMESCOPE_PATH} for the box's own \
                 session unit — a session script that hardcodes that path (Nobara) gets the \
                 patched build on this restart too"
            );
            true
        }
        Ok(false) => {
            // `write_session_plus_dropin` REMOVES rather than writes when there is no bind to arm,
            // so this arm is also the disarm — the flag must follow it or the restore would owe a
            // removal for a drop-in that is already gone.
            *SESSION_DROPIN_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = false;
            false
        }
        // Best-effort: a box whose session already runs our binary loses nothing, and a failure
        // here must not block a restart that would otherwise work.
        Err(e) => {
            tracing::warn!(error = %e, "gamescope: could not write the box-session drop-in");
            false
        }
    };
    // Unconditional, unlike the old "only when we wrote one": `write_session_plus_dropin` also
    // REMOVES a drop-in (a stale one from a build that wrote it to `$HOME`, or one this process
    // disarmed), and a removal systemd has not reloaded still applies at the unit's next start.
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", &unit]);
    // Wait for the relaunched session to come up at the new size and publish its capture node. The
    // node appears when gamescope is up (well before Steam finishes booting); the caller's
    // first-frame retry absorbs Steam's cold start.
    let mut deadline = Instant::now() + Duration::from_secs(45);
    loop {
        // "Did what we asked for come up", NOT "is the box unanimous" — see [`any_output_size_is`].
        if any_output_size_is(&gamescope_argvs(), target) {
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
            // RUNTIME BACKSTOP — and this is the unit it matters most for. This is the BOX's own
            // session: if the bind keeps killing it, `gamescope-session-plus`'s short-session
            // tracker eventually stops trying and hands the seat to the desktop session, which is
            // how a streaming attempt ends with the user on plasma and unable to get back. Drop the
            // drop-in, reload, and give it one more restart without it.
            if bound {
                note_bind_hazard(&unit);
                disarm_session_plus_dropin(); // also clears the flag: there is nothing left to undo
                systemctl_user(&["restart", &unit]);
                bound = false;
                deadline = Instant::now() + Duration::from_secs(45);
                continue;
            }
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

/// Output (capture) resolution `-W <w> -H <h>` of ONE gamescope, from its argv. `None` if the
/// flags aren't both present — which is also the final filter that separates a compositor from
/// anything else [`gamescope_argvs`] let by. Pure + unit-tested.
fn gamescope_output_size(argv: &[String]) -> Option<(u32, u32)> {
    match (
        argv_u32(argv, &["-W", "--output-width"]),
        argv_u32(argv, &["-H", "--output-height"]),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

/// What "the gamescope on this box" is producing — a **three-state** answer, because on the one
/// deployment that matters the question genuinely has three answers and collapsing two of them is
/// what makes a caller do the wrong thing.
///
/// This started as `find_map` over `/proc` read_dir order: the first gamescope with a `-W`/`-H`,
/// which is arbitrary. A Deck or Bazzite box in Game Mode routinely runs TWO — the session
/// compositor and a nested one for the game (a per-title `gamescope -W 1920 -H 1080 -- %command%`
/// launch option produces exactly that, and `heads.rs`'s own test encodes the shape as normal) —
/// plus any headless one this crate spawned. A wrong-but-confident answer scaled the libei
/// injector's absolute coordinates to the wrong picture and made [`ensure_box_gamescope_mode`]
/// compare its target against a compositor it was not restarting.
///
/// It then became `Option`, and *that* is what forced this enum: at both of
/// [`ensure_box_gamescope_mode`]'s `== Some(target)` sites, "cannot tell" was indistinguishable
/// from "a different resolution" — i.e. it meant DO THE DESTRUCTIVE THING. On a headless Game Mode
/// box with a game nested inside the session (the normal shape) a connect at the session's own
/// resolution stopped taking the fast path and instead `set-environment`ed + restarted the box's
/// own unit, killing the running game. Three states keep "unknown" answerable as "unknown".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxOutputSize {
    /// Every gamescope that carries a `-W`/`-H` carries THIS one — the only state a caller may act
    /// on, in either direction (reuse when it matches, re-mode when it does not).
    Known((u32, u32)),
    /// Nothing on the box reports an output size: no gamescope running, or one launched without
    /// those flags. Not ambiguity — there is no second opinion to be wrong about — so the re-mode
    /// path treats it as the pre-`Option` code did and proceeds.
    Unreported,
    /// Two coexisting gamescopes report DIFFERENT sizes and nothing here can say which one owns the
    /// session. Callers must pick their NON-destructive branch: identifying the session compositor
    /// (rather than declining to guess) needs a `/proc` ppid map to reject the nested ones, which is
    /// deliberately not built here.
    Ambiguous,
}

/// [`BoxOutputSize`] for the gamescopes running right now.
fn box_output_size() -> BoxOutputSize {
    classify_output_size(&gamescope_argvs())
}

/// The single size every gamescope carrying `-W`/`-H` agrees on. `None` for BOTH of the other two
/// states, which is sound only because this reader's one consumer — the libei injector's coordinate
/// hint — treats "unknown" as "omit the hint and use raw client pixels". Anything that would ACT on
/// the difference must call [`box_output_size`] instead.
fn current_gamescope_output_size() -> Option<(u32, u32)> {
    match box_output_size() {
        BoxOutputSize::Known(size) => Some(size),
        BoxOutputSize::Unreported | BoxOutputSize::Ambiguous => None,
    }
}

/// [`box_output_size`]'s decision over a supplied argv set. Pure + unit-tested — the three-way rule
/// is the whole point of the function, and it is not observable from the `/proc`-reading half.
fn classify_output_size(argvs: &[Vec<String>]) -> BoxOutputSize {
    let mut agreed: Option<(u32, u32)> = None;
    for argv in argvs {
        let Some(size) = gamescope_output_size(argv) else {
            continue;
        };
        match agreed {
            None => agreed = Some(size),
            Some(seen) if seen == size => {}
            Some(seen) => {
                tracing::debug!(
                    ?seen,
                    ?size,
                    "gamescope: two coexisting gamescopes report different output sizes — \
                     answering 'ambiguous' rather than picking one of them"
                );
                return BoxOutputSize::Ambiguous;
            }
        }
    }
    match agreed {
        Some(size) => BoxOutputSize::Known(size),
        None => BoxOutputSize::Unreported,
    }
}

/// Is ANY gamescope on the box producing exactly `target`? The right question after we restarted the
/// box's own session at `target`: we are not asking "what is this box's size" (the unanimity
/// question) but "did the thing we asked for come up". Unanimity is the wrong test there — a single
/// unrelated gamescope at another size (a kept bare spawn of ours for a different client, which does
/// NOT die with the restarted unit) would hold the answer at `Ambiguous` forever, spending the whole
/// 45 s wait, the bind-hazard backstop's second restart, and another 45 s before failing a connect
/// whose session had in fact come up. Pure + unit-tested.
fn any_output_size_is(argvs: &[Vec<String>], target: (u32, u32)) -> bool {
    argvs
        .iter()
        .any(|argv| gamescope_output_size(argv) == Some(target))
}

/// The numeric value following the first of `names` present in `argv`. Pure + unit-tested — it is
/// the shared reader behind both the output-size probe above and the mode verification below.
fn argv_u32(argv: &[String], names: &[&str]) -> Option<u32> {
    argv.iter().enumerate().find_map(|(i, a)| {
        names
            .contains(&a.as_str())
            .then(|| argv.get(i + 1).and_then(|v| v.parse().ok()))
            .flatten()
    })
}

/// Did the MODE we asked an indirectly-spawned session for actually reach its gamescope?
///
/// [`verify_managed_spawn_flags`] answers the same question for the capability flags and REFUSES
/// the session when they are missing, because the retry then resolves a different (correct) plan.
/// The mode has no such recovery: relaunching would hand the session the exact same environment and
/// lose it the same way, so refusing would only loop. It is not silent either, though — and it used
/// to be, in the way that costs the most:
///
/// `--nested-refresh` is the ONLY refresh a headless gamescope has. `CHeadlessBackend::Init`
/// assigns `g_nOutputRefresh = g_nNestedRefresh`, defaulting to **60 Hz** when the flag is absent,
/// and that one number is what the session composites at, what `vblankmanager` paces to, and what
/// Steam and every game are told the display runs at. It reaches a `gamescope-session-plus` only
/// through the `GAMESCOPE_BIN` wrapper — which the session script is free to lose (a `sessions.d`
/// file sourced with `set -a` can reassign `GAMESCOPE_BIN`; one that sets `GAMESCOPECMD` outright
/// skips the whole builder). When that happened the stream still ran, still looked right, and still
/// showed the client's own fps counter at the negotiated rate — because the encode loop repeats the
/// held frame — while the game underneath was capped to 60. Field report 2026-08-08.
///
/// So: warn, name the numbers, and carry on. Same "any running gamescope carrying it" rule as the
/// flag check, and the same silence when `/proc` cannot be read.
fn warn_if_mode_lost(mode: Mode, want_hz: u32) {
    let argvs = gamescope_argvs();
    let lost = mode_mismatch(mode.width, mode.height, want_hz, &argvs);
    if lost.is_empty() {
        return;
    }
    tracing::warn!(
        lost = %lost.join(", "),
        "gamescope: the session did not start at the mode we asked for — the session script \
         dropped GAMESCOPE_BIN / SCREEN_WIDTH / SCREEN_HEIGHT. A headless gamescope reports \
         `--nested-refresh` as its ONE refresh rate (60 Hz when the flag never arrives), so games \
         and Steam will believe the display runs at that rate however fast the stream is. Install \
         punktfunk-gamescope, or check /etc/gamescope-session-plus/sessions.d/ for a file that \
         overrides GAMESCOPE_BIN or sets GAMESCOPECMD"
    );
}

/// Which parts of the requested mode no running gamescope was started with, as human-readable
/// `asked=…, got=…` fragments. Empty when it matches — or when there is nothing to compare against,
/// which is the same fail-open rule [`missing_flags`] has and for the same reason. Pure +
/// unit-tested.
fn mode_mismatch(want_w: u32, want_h: u32, want_hz: u32, argvs: &[Vec<String>]) -> Vec<String> {
    if argvs.is_empty() {
        return Vec::new();
    }
    let mut lost = Vec::new();
    let sizes: Vec<(u32, u32)> = argvs
        .iter()
        .filter_map(|a| {
            Some((
                argv_u32(a, &["-W", "--output-width"])?,
                argv_u32(a, &["-H", "--output-height"])?,
            ))
        })
        .collect();
    // No gamescope carries an output size at all → we cannot tell ours apart from a nested one;
    // stay quiet rather than warn on every box that runs a second gamescope.
    if !sizes.is_empty() && !sizes.contains(&(want_w, want_h)) {
        lost.push(format!(
            "resolution asked={want_w}x{want_h}, got={}",
            sizes
                .iter()
                .map(|(w, h)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    let rates: Vec<u32> = argvs
        .iter()
        .filter_map(|a| argv_u32(a, &["-r", "--nested-refresh"]))
        .collect();
    if !rates.contains(&want_hz) {
        lost.push(match rates.as_slice() {
            // The flag is absent everywhere — the exact shape that silently yields 60 Hz.
            [] => format!(
                "refresh asked={want_hz}Hz, got=no --nested-refresh at all (gamescope defaults to \
                 60Hz headless)"
            ),
            got => format!(
                "refresh asked={want_hz}Hz, got={}Hz",
                got.iter().map(u32::to_string).collect::<Vec<_>>().join("/")
            ),
        });
    }
    lost
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
    let out = crate::proc::output_within(
        Command::new("systemctl").args([
            "--user",
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ]),
        UNIT_QUERY_BUDGET,
    )
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
    // All three budgeted: this runs on the disconnect restore and on the shutdown path, where the
    // whole sequence has ~20 s before `native.rs` gives up and exits — three unbounded `systemctl`
    // calls against a busy user manager were enough on their own to spend it.
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "kill", "--signal=SIGKILL", unit]),
        UNIT_VERB_BUDGET,
    );
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "stop", unit]),
        UNIT_VERB_BUDGET,
    );
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "reset-failed", unit]),
        UNIT_VERB_BUDGET,
    );
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
/// ⚠ The mask stops the UNIT from starting — it does NOT stop the relogin loop that keeps trying.
/// On images whose SDDM session helper execs the session script directly (`/etc/sddm/wayland-session
/// gamescope-session-plus steam`, f43 bazzite-deck — live-diagnosed on the .41 VM 2026-07-31) SDDM
/// relogins ~3×/s regardless, each a full `bash --login` session start — 328 forks/s, load 6+, 1481
/// logind sessions in 8 minutes, the journal flooded past its own rotation. The stream itself
/// survives, but the storm starves the game and the encoder ("atrocious, unplayable 240fps"). The
/// real defense against the storm is stopping the DM ([`dm_plan`]); the mask stays as belt-and-braces
/// for the window before the stop lands, and as the degraded takeover when the stop is impossible.
///
/// ⚠⚠ The mask DOES bite on that image, which is easy to miss and was the 2026-08-10 field bug: the
/// session script's last act is `systemctl --user --wait start gamescope-session-plus@$1.service`, so
/// a masked unit makes every entry into game mode — including the user's own deliberate "Return to
/// Gaming Mode" — fail instantly, with Steam left sitting on its "Switch to Desktop…" modal forever.
/// The mask is therefore only sound while our managed session actually holds the box: the moment the
/// box leaves it (a mid-stream switch to a desktop session), [`lift_autologin_mask`] must lift it, or
/// the way back is barred until reboot (`--runtime` lives in tmpfs — which is exactly why "it works
/// again after a reboot").
fn mask_unit(unit: &str) {
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "mask", "--runtime", unit]),
        UNIT_VERB_BUDGET,
    );
}

/// Undo [`mask_unit`] — every restore path must unmask before (or regardless of) restarting, or the
/// box's own return-to-gaming-mode stays broken until reboot.
fn unmask_unit(unit: &str) {
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "unmask", "--runtime", unit]),
        UNIT_VERB_BUDGET,
    );
}

/// Lift the takeover's runtime mask ([`mask_unit`]) on the box's own autologin units, so the box can
/// enter its own game mode again. Idempotent and cheap once lifted (the flag short-circuits), so
/// every path that hands the box back may call it unconditionally.
///
/// Deliberately does NOT consume [`STOPPED_AUTOLOGIN`]: the mask's lifetime is shorter than the
/// takeover's. Lifting it mid-stream only says "the box may start its own gaming session again"; the
/// restore still owes that list a `start` if the box is still ours at disconnect
/// ([`do_restore_tv_session`]).
fn lift_autologin_mask() {
    let mut masked = AUTOLOGIN_MASKED.lock().unwrap_or_else(|e| e.into_inner());
    if !*masked {
        return;
    }
    *masked = false;
    let units = STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner());
    for unit in units.iter() {
        unmask_unit(unit);
    }
    tracing::info!(
        units = ?*units,
        "gamescope: lifted the takeover's runtime mask — the box can enter its own game mode again"
    );
}

/// Does a mid-stream session switch TO `kind` end the window in which the takeover's mask is sound?
/// Only a **desktop** session does: it means the box left our managed game session for one of its
/// own, so nothing is left for the mask to defend and the user's next move — "Return to Gaming
/// Mode" — needs the unit ([`mask_unit`]).
///
/// [`ActiveKind::Gaming`] must not lift by itself (a takeover's own managed session reads as Gaming,
/// and lifting there would void the mask for the whole stream, in exactly the storm window it exists
/// for), and neither must [`ActiveKind::None`] — a managed session momentarily down between
/// relaunches reads as `None`, and that is mid-takeover, not the end of one.
fn switch_ends_mask_window(kind: super::ActiveKind) -> bool {
    use super::ActiveKind;
    matches!(
        kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
    )
}

/// The host's mid-stream session watcher calls this on every switch it confirms; see
/// [`switch_ends_mask_window`] for which ones actually lift the mask.
pub fn release_autologin_mask(switched_to: super::ActiveKind) {
    if switch_ends_mask_window(switched_to) {
        lift_autologin_mask();
    }
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

/// The group `pf-dm-helper` authorizes on. The polkit action has to stay `allow_any` (the host
/// commonly runs as a LINGERING user unit, which has no logind session for polkit to classify), so
/// the helper makes the real decision itself: only a member of this group may run the verbs. Every
/// package CREATES the group and adds NOBODY to it — writing the usbip `attach` node it also gates
/// materialises arbitrary emulated USB hardware, so joining stays a deliberate act.
const DM_HELPER_GROUP: &str = "punktfunk";

/// The packaged helper on this box, if any (see [`DM_HELPER_PATHS`]).
fn installed_dm_helper() -> Option<&'static str> {
    DM_HELPER_PATHS
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
}

/// Why the packaged DM helper did not perform a verb.
///
/// This used to be a bare `false`, and that is the whole reason a Nobara box spent a release
/// telling its owner to "reinstall the punktfunk package, or install the display-manager polkit
/// rule from the docs" while the true cause was **group membership** — which neither remedy
/// touches (field, Nobara 44 / 0.27.0-0.ci12635, 2026-08-09). The polkit action was installed,
/// `allow_any`, annotated at the right path, and pkexec DID run the helper; the helper then
/// printed the exact fix on stderr and `.status()` threw it away, leaving the caller to guess —
/// and guess wrong, silently, on every connect (the takeover degrades to attach, so nothing fails
/// loudly).
///
/// The four shapes are kept apart because they need four different fixes, and because a helper
/// that could not even be EXECUTED must never masquerade as one that ran and refused.
enum DmHelperError {
    /// Nothing to run: no packaged helper on this box (a tarball/source install, or a package
    /// older than the helper). The polkit-rule route from the docs applies; the group does not.
    NotInstalled,
    /// The helper is installed but `pkexec` could not be spawned at all (no polkit on this box, or
    /// the spawn failed). Nothing evaluated the request, so nothing refused it.
    NotExecutable { helper: &'static str, io: String },
    /// `pkexec` itself refused before the helper's own gate: the action is missing/overridden, or
    /// the authorization could not be obtained. 126/127 are pkexec's OWN exit codes — the helper
    /// only ever exits 0, 1 or 2 — so this is distinguishable from a refusal.
    Denied {
        helper: &'static str,
        code: i32,
        stderr: String,
    },
    /// The helper RAN and failed, and said why on stderr. That text is the answer: it names the
    /// user, the group, and the `usermod` line that fixes it. Pass it through verbatim rather than
    /// inventing a diagnosis on top of it.
    Refused {
        helper: &'static str,
        code: Option<i32>,
        stderr: String,
    },
}

impl std::fmt::Display for DmHelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(
                f,
                "no packaged pf-dm-helper on this box (looked in {}) — install the punktfunk \
                 package, or add a display-manager polkit rule for your user (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)",
                DM_HELPER_PATHS.join(" and ")
            ),
            Self::NotExecutable { helper, io } => write!(
                f,
                "{helper} is installed but could not be run via pkexec ({io}) — this box appears \
                 to have no polkit; add a display-manager polkit rule for your user instead (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)"
            ),
            Self::Denied {
                helper,
                code,
                stderr,
            } => write!(
                f,
                "pkexec never ran {helper} (exit {code}{}) — polkit did not authorize \
                 io.unom.punktfunk.dm-helper, so the action is missing or overridden; reinstall \
                 the punktfunk package, or add a display-manager polkit rule for your user (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)",
                suffix(stderr)
            ),
            Self::Refused {
                helper,
                code,
                stderr,
            } if stderr.is_empty() => write!(
                f,
                "{helper} ran and failed (exit {}) without printing a reason",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ),
            Self::Refused { helper, stderr, .. } => {
                write!(f, "{helper} ran and refused: {stderr}")
            }
        }
    }
}

/// `": <text>"`, or nothing at all when there is no text — so a message never ends in a dangling
/// colon on a helper that said nothing.
fn suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new() // no dangling ": " when the child was silent
    } else {
        format!(": {stderr}")
    }
}

/// Run the packaged DM helper (`stop` | `restore` | `linger`) via pkexec.
///
/// **Captures stderr and keeps the exit code** ([`DmHelperError`]): the helper's own message is
/// the only place the actionable cause exists (group membership, a missing
/// `display-manager.service` alias), and every caller here turns a failure into text an operator
/// reads. `output()` rather than `status()` also gives the child a NULL stdin, so a pkexec that
/// decides to prompt gets EOF immediately instead of blocking a stream thread on a tty read it can
/// never satisfy.
///
/// Deliberately unbounded in time: the `stop`/`restore` verbs shell out to `systemctl`, whose stop
/// job legitimately takes seconds on a busy seat, and a budget here would kill the takeover
/// mid-flight rather than diagnose it.
fn dm_helper(verb: &str) -> std::result::Result<(), DmHelperError> {
    let Some(helper) = installed_dm_helper() else {
        return Err(DmHelperError::NotInstalled);
    };
    let out = Command::new("pkexec")
        .arg(helper)
        .arg(verb)
        .output()
        .map_err(|e| DmHelperError::NotExecutable {
            helper,
            io: e.to_string(),
        })?;
    if out.status.success() {
        return Ok(());
    }
    // One line: these land in a `tracing` field, and the helper's two-line refusal (reason +
    // `Grant it with: …`) has to survive the trip intact.
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match out.status.code() {
        // pkexec's own codes: 127 "not authorized / could not execute the program", 126
        // "authentication dialog dismissed". The helper only ever exits 0, 1 or 2, so either of
        // these means the request never reached its group gate.
        Some(c @ (126 | 127)) => Err(DmHelperError::Denied {
            helper,
            code: c,
            stderr,
        }),
        // `None` = killed by a signal, and [`DmHelperError::Refused`] renders that as "signal"
        // rather than inventing a plausible-looking exit code.
        code => Err(DmHelperError::Refused {
            helper,
            code,
            stderr,
        }),
    }
}

/// Startup preflight for the takeover's one prerequisite nothing can automate: this user being in
/// the `punktfunk` group.
///
/// The failure it catches is **silent by construction**. Missing membership doesn't fail a unit,
/// doesn't fail the connect and doesn't fail the stream: the takeover simply degrades to ATTACH
/// (or, under SDDM, to mask-only), which on a box whose panel is off reads as "a black screen on
/// every connect" and nothing else. It also only ever surfaces mid-stream, in a warn line nobody
/// is watching while they're trying to play. So say it once, at startup, where an operator
/// actually reads the log — and say it with the command that fixes it.
///
/// Deliberately **not** unconditional; a box that will never attempt a takeover must not be
/// nagged. Four gates, each of which alone makes the group irrelevant:
/// * **root** — the plain system-bus `systemctl` verbs succeed, so the helper is never reached;
/// * **no display manager** — [`dm_plan`] only stops a DM that exists, and a getty-autologin /
///   enabled-user-unit box has none;
/// * **no gamescope session infrastructure** ([`managed_session_available`]) — no
///   `gamescope-session-plus`/SteamOS means no autologin gaming session to free, so
///   [`stop_autologin_sessions`] returns before it looks at the DM at all;
/// * **no packaged helper** — a tarball/source/Nix install has neither the helper nor the group,
///   and its route is the hand-written polkit rule from the docs, which this group has no part in.
///
/// Membership is read from the **user database**, not from our own `getgroups()`, because that is
/// what the gate we are predicting reads: `pf-dm-helper` runs as root and resolves the caller's
/// groups with `id -nG <user>`. A `usermod -aG` therefore satisfies the helper immediately — but
/// the remedy still says to log back in, because the same group gates the usbip nodes the virtual
/// Steam Deck pad attaches through, and THAT is a credential check against this process, whose
/// supplementary groups were fixed when its `systemd --user` manager started.
pub fn preflight_takeover_privilege() {
    if crate::proc::current_uid() == 0 {
        return; // root: `systemctl stop <dm>` succeeds outright, the helper is never consulted
    }
    let Some(dm) = display_manager_unit() else {
        return; // no DM drives this box's logins — nothing for the takeover to stop
    };
    if !managed_session_available() {
        return; // no session-plus/SteamOS ⇒ no autologin gaming session ⇒ no takeover
    }
    let Some(helper) = installed_dm_helper() else {
        return; // unpackaged install: no helper, no group, the polkit-rule route applies instead
    };
    let Some(user) = current_user_name() else {
        return; // cannot name the user ⇒ cannot give a usable `usermod` line; stay quiet
    };
    let group = DM_HELPER_GROUP;
    if user_in_group(&user, group) {
        return;
    }
    tracing::warn!(
        %user,
        %dm,
        helper,
        group,
        "gamescope: the managed takeover on this box has to stop {dm} for a stream, which runs \
         through {helper} — and that helper only serves members of the '{group}' group, which \
         '{user}' is not in. Every takeover will degrade silently: the stream mirrors the box's \
         own session instead, which with the panel off looks like a black screen on every \
         connect. Fix it once with `sudo usermod -aG {group} {user}`, then log out and back in — \
         a `systemd --user` session keeps the group set it started with, and the same group gates \
         the virtual Steam Deck pad's usbip nodes. It can present arbitrary emulated USB devices, \
         so join it only on a machine you trust."
    );
}

/// This process's login name, for a `usermod` line the operator can paste. From `id -un <uid>`
/// rather than `$USER`: a `systemd --user` unit's environment is whatever the manager was started
/// with, and the uid is the thing pkexec will actually resolve.
fn current_user_name() -> Option<String> {
    let out = crate::proc::output_within(
        Command::new("id").args(["-un", &uid_string()]),
        Duration::from_secs(5),
    )
    .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Is `user` in `group` **according to the user database** — the same question, asked the same
/// way, that `pf-dm-helper` answers as root before it will do anything. Budgeted: `id` resolves
/// through NSS, which on a box with a remote directory can block, and this runs on the startup
/// path.
fn user_in_group(user: &str, group: &str) -> bool {
    let Ok(out) = crate::proc::output_within(
        Command::new("id").args(["-nG", user]),
        Duration::from_secs(5),
    ) else {
        return true; // couldn't ask ⇒ don't accuse: a false alarm here sends people down a wrong path
    };
    if !out.status.success() {
        return true;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .any(|g| g == group)
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
///
/// Bounded by [`DM_VERB_BUDGET`] on top of that: `--no-ask-password` removes the *dialog*, not the
/// wait — a system manager mid-shutdown can still take the request and never answer. A timeout
/// reads as `false`, which is the same answer an unauthorized call already gives, so every caller
/// falls through to the pkexec helper exactly as it does today.
fn systemctl_system(args: &[&str]) -> bool {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--no-ask-password").args(args);
    crate::proc::status_within(&mut cmd, DM_VERB_BUDGET)
        .map(|s| s.success())
        .unwrap_or(false)
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
///
/// `Err` carries **why** it could not be ensured, because the helper path is reached here first:
/// on a sessionless host the `linger` verb goes through the same [`dm_helper`] gate the `stop`
/// verb does, so a user outside the `punktfunk` group fails at THIS step and never reaches the
/// DM-stop one. Dropping the reason here would just move the misdiagnosis one message earlier.
fn ensure_host_survives_dm_stop() -> std::result::Result<(), String> {
    if !host_is_under_user_manager() {
        return Ok(()); // root / a system unit — the DM stop cannot reach us
    }
    if linger_enabled() {
        return Ok(());
    }
    // `set-self-linger` is `allow_active` in logind's own policy, so a host started inside the
    // user's session can do this itself; a sessionless one (the packaged unit) goes through the
    // helper, whose grant is scoped to the calling uid.
    let uid = uid_string();
    let _ = crate::proc::status_within(
        Command::new("loginctl").args(["--no-ask-password", "enable-linger", &uid]),
        UNIT_QUERY_BUDGET,
    );
    let helper = if linger_enabled() {
        Ok(()) // the plain verb was enough — the helper was never needed
    } else {
        dm_helper("linger").map_err(|e| e.to_string())
    };
    match helper {
        Ok(()) if linger_enabled() => {
            tracing::info!(
                uid,
                "enabled lingering for this user — the managed takeover stops the display manager, \
                 which ends this login session, and without lingering logind would stop the host \
                 along with it (`loginctl disable-linger` reverts it)"
            );
            Ok(())
        }
        // The verb reported success and `loginctl` still says no: not a privilege problem, so say
        // that instead of blaming the grant the operator would then go and re-check.
        Ok(()) => Err(format!(
            "`loginctl enable-linger {uid}` reported success but lingering is still off"
        )),
        Err(why) => Err(why),
    }
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

/// Is lingering on for this user (logind keeps the `--user` manager alive with no session)? An
/// unanswered one reads as "not lingering", which refuses the takeover rather than risking the DM
/// stop taking the host down with it.
///
/// [`UNIT_QUERY_BUDGET`], not [`UNIT_STATE_BUDGET`], and the failure DIRECTION is why. The 300 ms
/// bound is documented as "anything near it means the manager is wedged — the case each caller's
/// failure path already covers", and that holds for the other two callers, whose timeout answers
/// `true`/keep-looping (benign). Here a timeout INVERTS the answer to `false`, and `false` is the
/// refusing direction: `ensure_host_survives_dm_stop` then reports "`enable-linger` reported success
/// but lingering is still off" and the bare-spawn Steam path fails a connect that would have worked,
/// blaming a lingering configuration that is in fact correct. And this is not an in-memory read the
/// way `systemctl is-active` is: it is a process spawn plus libsystemd's dynamic link plus a logind
/// D-Bus round trip, sampled at the busiest moment on the box (a takeover, with Steam and a
/// compositor being torn down). Only a genuinely wedged logind exceeds 5 s.
fn linger_enabled() -> bool {
    crate::proc::output_within(
        Command::new("loginctl").args(["show-user", &uid_string(), "-p", "Linger", "--value"]),
        UNIT_QUERY_BUDGET,
    )
    .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
    .unwrap_or(false)
}

/// Stop the display manager for a takeover on a mask-fragile DM flavor. Plain `systemctl stop` on
/// the SYSTEM bus first — succeeds as root or under an operator polkit rule scoped to the DM unit
/// (see docs); fails cleanly otherwise ("interactive authentication required") — then the
/// packaged pkexec helper. The `Err` is the HELPER's reason (the plain verb's failure is expected
/// and carries no information: an unprivileged host is meant to fail it), and the caller puts it
/// in front of the operator instead of guessing.
fn try_stop_display_manager(dm: &str) -> std::result::Result<(), DmHelperError> {
    if systemctl_system(&["stop", dm]) {
        return Ok(());
    }
    dm_helper("stop")
}

/// Restore the display manager: `reset-failed` (a relogin loop may have tripped the unit's start
/// limit, and a plain restart is refused until the accounting clears) + `restart` — its autologin
/// session Exec brings the box's own session back up. Plain system-bus verbs first (root / an
/// operator polkit rule), then the packaged pkexec helper, whose `restore` verb performs the same
/// two steps as root. The `Err` carries the helper's own reason: this is the failure that leaves a
/// box with **no graphical session at all**, so the log line it produces has to be the one that
/// solves it.
fn restore_display_manager(dm: &str) -> std::result::Result<(), DmHelperError> {
    let _ = systemctl_system(&["reset-failed", dm]);
    if systemctl_system(&["restart", dm]) {
        return Ok(());
    }
    dm_helper("restore")
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
    // Consume the takeover state up front: from here on the box is the DM's again. The mask goes
    // FIRST and while the unit list still exists — this path discards that list, and it is the only
    // record of what carries a mask. Without this the mask outlived not just the stream but the
    // boot: the disconnect restore (the only other unmask) would find an empty list and lift
    // nothing. It is also what lets step 1 below work at all, since the DM's autologin heads back
    // into game mode through exactly this unit.
    lift_autologin_mask();
    std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    clear_takeover();
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    stop_session(SESSION_UNIT); // dead already (the switch shut its Steam down) — clear the unit
    if let Err(e) = restore_display_manager(&dm) {
        tracing::warn!(
            %dm,
            reason = %e,
            "gamescope: display-manager start was denied — the desktop switch may need a manual \
             `systemctl restart` of the DM"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // Budgeted: this is a 10 s loop, and a single unbounded `is-active` against a system
        // manager that is itself mid-restart would consume the whole window in one tick.
        let active = crate::proc::output_within(
            Command::new("systemctl").args(["is-active", &dm]),
            UNIT_STATE_BUDGET,
        )
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
        // [`DM_VERB_BUDGET`] rather than a unit budget: the helper self-pkexecs and rewrites the
        // DM's autologin config, which is DM-verb-shaped work — and it runs on the stream thread
        // that is honoring the user's in-stream switch.
        match crate::proc::status_within(
            Command::new(OS_SESSION_SELECT).arg("desktop"),
            DM_VERB_BUDGET,
        ) {
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
    let Ok(out) = crate::proc::output_within(
        Command::new("systemctl").args([
            "--user",
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ]),
        UNIT_QUERY_BUDGET,
    ) else {
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
        //
        // Both failure arms below quote the REASON they were handed rather than describing one.
        // 0.26.0/0.27.0 described one — "the packaged pf-dm-helper polkit action is missing or was
        // denied (reinstall the punktfunk package, or install the display-manager polkit rule from
        // the docs)" — and on the box that produced it the action was installed, permissive,
        // correctly annotated, and pkexec had already RUN the helper; the helper's refusal ("user
        // 'x' is not in the 'punktfunk' group") was thrown away with its stderr. Both suggested
        // remedies were dead ends: neither a reinstall nor a polkit rule adds anyone to a group.
        let dm_stopped = if let Err(why) = ensure_host_survives_dm_stop() {
            if !plan.mask {
                // The reason goes LAST in both bails: the helper's own refusal ends in a command
                // to paste, and burying that mid-sentence is how it stops being read.
                bail!(
                    "stopping {dm} ends this user's last login session, and without lingering \
                     logind would stop the user manager — and this host with it — about 10s \
                     later, leaving the box with no display manager and nothing to restore it; \
                     lingering could not be enabled, so the managed takeover is unavailable. \
                     Either run `sudo loginctl enable-linger $USER` once, as the setup docs ask, \
                     and reconnect — or fix the privileged path: {why}"
                );
            }
            tracing::warn!(
                %dm,
                reason = %why,
                "cannot stop the display manager for this stream (lingering could not be \
                 enabled, and without it the DM stop would take this host down ~10s later) — \
                 leaving it running: its autologin Relogin loop will churn logind sessions for \
                 the whole stream, up to a fork storm that starves the game and encoder; run \
                 `sudo loginctl enable-linger $USER` once, as the setup docs ask"
            );
            false
        } else if let Err(why) = try_stop_display_manager(&dm) {
            if !plan.mask {
                bail!(
                    "the box's gaming session is driven by {dm}, which does not survive a masked \
                     session unit, and stopping it needs privilege, so the managed takeover is \
                     unavailable — {why}"
                );
            }
            tracing::warn!(
                %dm,
                reason = %why,
                "stopping the display manager for this stream needs privilege and the privileged \
                 path failed — leaving it running: its autologin Relogin loop will churn logind \
                 sessions for the whole stream, up to a fork storm that starves the game and \
                 encoder"
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
    if plan.mask {
        // Record that a mask is outstanding BEFORE laying it: every hand-back path lifts it off this
        // flag, and one that ran between the mask and an unrecorded flag would leave it on forever.
        *AUTOLOGIN_MASKED.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }
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

/// Budget for the `steam -shutdown` HELPER itself — derived from [`STEAM_SHUTDOWN_WAIT`], because
/// the cause may not be given less time than we are willing to wait for the effect.
///
/// It was [`UNIT_QUERY_BUDGET`] (5 s) on the premise that `/usr/bin/steam` is a thin IPC forwarder.
/// It is not: on every mainstream distro it is the `steam.sh` bootstrap — env setup, runtime and
/// bootstrap checks, library-path shimming — which only then runs the client binary that forwards
/// `-shutdown`. And [`crate::proc::status_within`] does not merely stop waiting at the budget: it
/// `killpg`s the whole process group, so an expiry KILLS the forwarder before the request leaves,
/// after which the 20 s wait below can only ever time out and tell the operator to close by hand the
/// Steam we just silently shot at. Half the effect budget leaves the wait its own room while still
/// covering a cold `steam.sh` on a loaded box.
const STEAM_SHUTDOWN_SEND_BUDGET: Duration = Duration::from_secs(STEAM_SHUTDOWN_WAIT.as_secs() / 2);

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
    // REAPED, not fire-and-forget. `let _ = …spawn()` dropped the `Child` immediately, and
    // `Child`'s Drop explicitly does not wait — so this helper became a zombie held for the host's
    // whole lifetime. Nothing downstream collected it: the loop below polls the TARGET Steam's pid,
    // and `pid_running` documents the very state this call site was manufacturing ("a zombie keeps
    // its /proc entry but has already released the Steam instance"). The budget is
    // [`STEAM_SHUTDOWN_SEND_BUDGET`], NOT a query-sized one: this helper is `steam.sh`'s bootstrap
    // before it is an IPC send, and the budget kills its whole process group — so too short a bound
    // does not merely stop waiting, it destroys the request. The rest of the waiting is the
    // `STEAM_SHUTDOWN_WAIT` loop below, which is about the OTHER process.
    let _ = crate::proc::status_within(
        Command::new("steam")
            .arg("-shutdown")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        STEAM_SHUTDOWN_SEND_BUDGET,
    );
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
/// display manager, a SteamOS target we re-pointed, a bind drop-in we armed over the box's own
/// session template, a resolution we pinned into its user manager, or a managed session we
/// launched beside a live desktop? The precondition for every restore path.
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
    // The ATTACH re-mode path steals nothing, so none of the three above records it — but it DID
    // rewrite the box's own `gamescope-session-plus@` template and pin the user manager's
    // SCREEN_*/CUSTOM_REFRESH_RATES at the client's mode, and only a restore takes those back.
    let dropin = *SESSION_DROPIN_ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let screen_env = *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    autologin
        || steamos
        || dm
        || dropin
        || screen_env
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

/// What a bounded `systemctl --user` lifecycle verb on the RESTORE path actually did. Three states,
/// because the log line these produce is the only thing an operator ever sees about a box that may
/// or may not have its screen back.
enum RestoreVerb {
    /// The job completed and systemd reported success.
    Done,
    /// We stopped WAITING at the budget — the job itself is untouched. `status_within` kills the
    /// systemctl CLIENT; a queued systemd job is not cancelled by that, so the unit still starts.
    StillRunning,
    /// systemd said no (masked unit, tripped start limit, missing unit), or the helper could not be
    /// spawned at all. The only outcome an operator has to act on.
    Failed(String),
}

/// Issue `systemctl --user <args>` on the restore path, bounded, and classify the outcome
/// ([`RestoreVerb`]).
///
/// The classification is the point. The sweep made these two call sites status-CHECKED (they used to
/// log an unconditional success over a discarded exit status, which is worse) but routed
/// [`std::io::ErrorKind::TimedOut`] into the same arm as a real failure — and for lifecycle verbs on
/// a restore that arm is the loud "the panel stays dark, run this by hand" error. Exceeding
/// [`UNIT_VERB_BUDGET`] is the ORDINARY case for these two: a `restart` of
/// `gamescope-session.target` blocks on `steam-launcher.service` stopping, i.e. on Steam tearing a
/// running game down. So the box came back exactly as intended while the highest-visibility
/// diagnostic on it said it had not — a false alarm on every SteamOS restore with a game loaded.
///
/// The bound itself stays: this runs on the shutdown path, where `native.rs`'s 20 s
/// `SHUTDOWN_RESTORE_GRACE` then `exit(0)` means an unbounded verb costs the display-manager restore
/// that follows it.
fn issue_restore_verb(args: &[&str]) -> RestoreVerb {
    match crate::proc::status_within(
        Command::new("systemctl").arg("--user").args(args),
        UNIT_VERB_BUDGET,
    ) {
        Ok(s) if s.success() => RestoreVerb::Done,
        Ok(s) => RestoreVerb::Failed(format!("systemctl exited with {s}")),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => RestoreVerb::StillRunning,
        Err(e) => RestoreVerb::Failed(format!("could not run systemctl: {e}")),
    }
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
                clear_takeover(); // A3: takeover undone — and only now that it actually is
                return;
            }
            // Checked, not `systemctl_user`'d: that helper is status-blind (`let _ = …status()`),
            // so this used to log an unconditional success over a restart that may never have
            // happened — on the one box whose panel goes dark until it does.
            match issue_restore_verb(&["restart", STEAMOS_SESSION_TARGET]) {
                RestoreVerb::Done => tracing::info!(
                    "gamescope (SteamOS): restored the physical gaming session (removed headless \
                     override)"
                ),
                // NOT an error, and this is the ordinary outcome on a Deck with a game loaded: a
                // `restart` of the target blocks on `steam-launcher.service`'s stop, i.e. on Steam
                // tearing a title (Proton/wineserver included) down — the very teardown this file
                // budgets 20 s for elsewhere and calls "generous". systemd has the job queued and
                // will finish it; all we stopped was our own wait.
                RestoreVerb::StillRunning => tracing::info!(
                    "gamescope (SteamOS): the {STEAMOS_SESSION_TARGET} restart is still running \
                     after {}s (Steam closing a game is the usual reason) — systemd owns the job \
                     from here; the panel comes back when it completes",
                    UNIT_VERB_BUDGET.as_secs()
                ),
                RestoreVerb::Failed(why) => tracing::error!(
                    status = %why,
                    "gamescope (SteamOS): could not restart {STEAMOS_SESSION_TARGET} — the Deck's \
                     panel stays dark until someone runs \
                     `systemctl --user restart {STEAMOS_SESSION_TARGET}` (the headless override is \
                     already removed, so that restart is all it needs)"
                ),
            }
            clear_takeover(); // A3: consumed — after the restart, not before it
            return;
        }
    }
    // Unmask BEFORE taking the list (it reads that list) and unconditionally — before the
    // desktop-active early return below, and before the restart loop: a unit left masked would break
    // the user's own return to gaming mode until reboot. Usually already lifted by then (the
    // mid-stream switch that ends the mask's window does it — [`release_autologin_mask`]), in which
    // case this is a no-op.
    lift_autologin_mask();
    let units = std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    let dm = std::mem::take(&mut *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()));
    // Consume the managed-session record HERE, in a scope of its own, and never hold that lock
    // across the work below. `create_managed_session` can be inside `launch_session` holding it for
    // up to ~90 s (two 45 s node-poll deadlines plus `systemd-run` round trips), and this function
    // is the shutdown path: blocking on it AFTER `stop_session` had already killed the unit meant
    // the display-manager restore below was never reached before `native.rs`'s 20 s
    // `SHUTDOWN_RESTORE_GRACE` expired and the process `exit(0)`ed — a box with its DM stopped, no
    // graphical session, and (with `clear_takeover` where it used to be) nothing on disk to heal it.
    let managed_was_running = MANAGED_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some();
    if units.is_empty() && dm.is_none() {
        // Nothing was stolen — but a managed session that started BESIDE a live desktop (client
        // gamescope pin on a KDE box) still owns the transient unit; stop it so it doesn't run
        // orphaned forever after the disconnect. No-op when the unit isn't running.
        if managed_was_running {
            stop_session(SESSION_UNIT);
            tracing::info!(
                "gamescope: stopped the idle managed session (nothing was taken over — no box \
                 session to restore)"
            );
        }
        // …and the ATTACH re-mode path steals nothing either, yet leaves the two most durable
        // marks this backend can make on a box: a bind drop-in over its OWN autologin template and
        // a user manager pinned at the client's resolution. Both used to survive the stream, the
        // host process, and (for the `$HOME` drop-in) the reboot. Undoing them does NOT restart
        // the box's session: the live one keeps the client's mode until it next starts, which is
        // the non-destructive direction — bouncing the user's own Game Mode on a disconnect is the
        // very thing the physical-display guard in `ensure_box_gamescope_mode` refuses to do.
        disarm_session_plus_dropin();
        unset_forced_session_screen_env();
        clear_takeover();
        return;
    }
    stop_session(SESSION_UNIT); // our gamescope/Steam session, so Steam is free for the autologin

    // Hand the box back its OWN gamescope and its own resolution BEFORE any of the branches below,
    // every one of which can return: our bind drop-in exists to serve a punktfunk stream, and
    // leaving it would silently put the patched build (plus our HDR/cursor flags) under the user's
    // ordinary game mode — exactly the "sits beside the distro package" rule this whole design
    // rests on. It used to sit after the desktop-active and DM returns, so those two paths leaked
    // it.
    disarm_session_plus_dropin();
    unset_forced_session_screen_env();
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
        clear_takeover(); // A3: consumed — the units/DM records are already drained into locals
        return;
    }
    // DM-stop takeover ([`dm_plan`] — every flavor stops a DM that drove a live gaming session):
    // restore the DM (`reset-failed` clears any relogin start-limit accounting, then `restart`);
    // its autologin session Exec brings gaming mode back itself. The unit is NOT `--user start`ed
    // here: on a mask-fragile flavor that cannot work — without a DM login session there is no
    // seat, so gamescope never gets DRM master (unit goes `failed`, screen stays black —
    // live-proven on the Nobara repro VM) — and under SDDM the relogin makes it redundant.
    if let Some(dm) = dm {
        match restore_display_manager(&dm) {
            Ok(()) => {
                tracing::info!(%dm, "restored the display manager (its autologin brings gaming mode back)")
            }
            Err(why) if crate::try_recover_session() => tracing::warn!(
                %dm,
                reason = %why,
                "display-manager restart lost its privilege — fired PUNKTFUNK_RECOVER_SESSION_CMD \
                 to bring the session back"
            ),
            // The worst state this code can produce: a box with no graphical session at all. The
            // helper's own reason rides along, because "run these two commands as root" fixes the
            // symptom once and the reason is what stops it happening again.
            Err(why) => tracing::error!(
                %dm,
                reason = %why,
                "could not restart the display manager and no PUNKTFUNK_RECOVER_SESSION_CMD is \
                 configured — the box has no graphical session until someone runs \
                 `systemctl reset-failed {dm} && systemctl restart {dm}` as root"
            ),
        }
        // LAST, not first. The persisted marker is the ONLY thing that heals a box whose DM is
        // down after this process dies, and every step above it is unbounded work on a shutdown
        // path with a 20 s grace (`native.rs`'s `SHUTDOWN_RESTORE_GRACE`, then `exit(0)` — which
        // runs no destructors). Deleting it before the restart had been issued meant an expiry
        // anywhere in between left the box dark with nothing on disk saying so.
        clear_takeover();
        return;
    }
    for unit in units {
        // Checked, not discarded: this call and the SteamOS `restart` above were the two places
        // that logged an unconditional success over a thrown-away exit status. A `--user start`
        // fails for reasons an operator can act on (the unit is masked, its start limit tripped),
        // and the DM branch thirty lines up already shows the shape — say what happened.
        match issue_restore_verb(&["start", &unit]) {
            RestoreVerb::Done => tracing::info!(
                unit,
                "restored the TV's autologin gaming session (debounce elapsed, no client)"
            ),
            // A `--user start` of a gamescope-session-plus unit waits for its Exec to signal, and on
            // a cold box that is Steam's own start — routinely longer than the bound. Queued is not
            // failed; only a status we actually READ can say the box is out of game mode.
            RestoreVerb::StillRunning => tracing::info!(
                unit,
                "the TV's autologin gaming session is still starting after {}s — systemd owns the \
                 job from here",
                UNIT_VERB_BUDGET.as_secs()
            ),
            RestoreVerb::Failed(why) => tracing::error!(
                unit,
                status = %why,
                "could not restart the TV's autologin gaming session — the box is left out of \
                 game mode until someone runs `systemctl --user start {unit}` (a masked unit or a \
                 tripped start limit are the usual causes: \
                 `systemctl --user unmask --runtime {unit} && systemctl --user reset-failed {unit}`)"
            ),
        }
    }
    clear_takeover(); // A3: consumed — and only now, with the restarts actually issued
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
            //
            // The two halves of this file are resolved from different sources — the socket is the
            // newest CONNECTABLE `gamescope-*-ei`, the size comes off `/proc` — so on a box running
            // two gamescopes they could describe two different compositors. That is why the size
            // probe now answers `None` unless every gamescope agrees: omitting the hint makes
            // `pf-inject` fall back to raw client pixels, whereas a hint taken from the nested
            // game's `-W 1280 -H 800` while the socket belongs to the 1920x1080 session put the
            // pointer's reachable area at two thirds of the picture.
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

/// The absolute path a session script may hardcode instead of honouring `GAMESCOPE_BIN`.
///
/// Nobara's `gamescope-session-plus` builds its command as `GAMESCOPECMD="/usr/bin/gamescope …"`
/// and reads `GAMESCOPE_BIN` NOWHERE, so all three of our spawn levers miss at once: the env var
/// is ignored, and an absolute path cannot be redirected by a PATH shim. The session then runs a
/// stock gamescope, the capability probe rejects it, and every session dies with
/// "pipeline build failed (out of retries)".
const DISTRO_GAMESCOPE_PATH: &str = "/usr/bin/gamescope";

/// The socket directory every Xwayland — and so every gamescope — insists on before it will open a
/// display. See [`SessionBind`] for why the host has to care about a path it never reads itself.
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// Does the box's `gamescope-session-plus` script HARDCODE [`DISTRO_GAMESCOPE_PATH`] — i.e. is the
/// bind the only lever left, or does the ordinary `GAMESCOPE_BIN` env var already reach gamescope?
///
/// Pure over the script text so the answer is testable: mentions `GAMESCOPE_BIN` anywhere ⇒ the
/// documented lever exists and the bind is unnecessary; never mentions it but does name the
/// absolute path ⇒ Nobara's shape, where the env var, the PATH shim and `PUNKTFUNK_GAMESCOPE_BIN`
/// all miss at once. Anything else ⇒ we cannot say the bind would help, so we don't arm it.
///
/// It reads the MAIN script only. A `sessions.d` fragment that presets `GAMESCOPECMD` with its own
/// absolute path defeats `GAMESCOPE_BIN` just as thoroughly and is invisible here — that case still
/// lands, as it always did, in [`verify_managed_spawn_flags`].
fn script_hardcodes_gamescope(script: &str) -> bool {
    !script.contains("GAMESCOPE_BIN") && names_the_distro_binary(script)
}

/// Does `script` name [`DISTRO_GAMESCOPE_PATH`] as a COMPLETE path rather than as the prefix of a
/// longer one? A plain `contains` also matches `/usr/bin/gamescopectl` and
/// `/usr/bin/gamescope-session-plus` — both of which really do appear in these scripts, and neither
/// of which is the binary the redirect is about. Getting this wrong arms a mount namespace on a box
/// that has no use for one.
fn names_the_distro_binary(script: &str) -> bool {
    script.match_indices(DISTRO_GAMESCOPE_PATH).any(|(at, _)| {
        let after = &script[at + DISTRO_GAMESCOPE_PATH.len()..];
        !after
            .starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    })
}

/// The session script's text, read once per process. `None` when it cannot be read at all, which
/// [`plan_bind`] treats as "do not arm" — a box we cannot inspect keeps the behaviour that works.
fn session_script() -> Option<&'static str> {
    static SCRIPT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            // Bounded read: this is a shell script (~30 KiB on every distro that ships one), and
            // an unbounded read of an arbitrary path a package could replace is not a thing to do
            // on the connect path.
            let bytes = std::fs::read(SESSION_PLUS_BIN).ok()?;
            if bytes.len() > 1 << 20 {
                return None;
            }
            Some(String::from_utf8_lossy(&bytes).into_owned())
        })
        .as_deref()
}

/// Why the host is NOT redirecting [`DISTRO_GAMESCOPE_PATH`] for this launch. Each arm is a
/// different sentence to an operator reading the log, which is the whole reason it is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOff {
    /// The resolved binary IS the distro path — a bind over itself redirects nothing.
    SameBinary,
    /// The resolved gamescope is a bare NAME, not an absolute path, so the wrapper would resolve it
    /// through `PATH` inside the unit — onto the path we just bound the wrapper over.
    UnresolvedBinary,
    /// The session script honours `GAMESCOPE_BIN` (or never names the absolute path): the ordinary
    /// env lever already reaches gamescope, so the namespace would be cost without benefit.
    EnvLeverSuffices,
    /// The session script could not be read — fail closed rather than arm a mechanism we cannot
    /// show is needed.
    ScriptUnreadable,
    /// `PUNKTFUNK_GAMESCOPE_BIND=0`.
    OperatorOff,
    /// [`note_bind_hazard`] fired: a session launched with the bind armed never came up.
    Disarmed,
}

/// What the session unit's `/usr/bin/gamescope` redirect should be for this launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindPlan {
    /// No redirect, and therefore NO mount namespace for the unit at all.
    Off(BindOff),
    /// Redirect. `x11` additionally binds a user-owned socket directory over [`X11_SOCKET_DIR`],
    /// which is what stops the namespace from killing the session (see [`SessionBind`]).
    Arm { x11: bool },
}

/// The decision, pure. Ordered so the answers that are about CORRECTNESS come first (a redirect
/// that would do nothing, or that would make the wrapper re-exec itself), then the operator's
/// knob, then the backstop, then what the box actually needs.
///
/// `enabled` is `PUNKTFUNK_GAMESCOPE_BIND`: `None` = auto, `Some(false)` = never, `Some(true)` =
/// force (skip the script probe — for a `GAMESCOPE_BIN` defeated somewhere the host cannot read,
/// e.g. a `sessions.d` fragment presetting `GAMESCOPECMD`). Force deliberately does NOT outrank
/// `disarmed`: the backstop is the safety net, and a forced box that crash-loops must still stop.
///
/// `x11_dir_uid` is the owner of [`X11_SOCKET_DIR`] as the HOST sees it (`None` = absent).
fn plan_bind(
    resolved_bin: &str,
    script: Option<&str>,
    enabled: Option<bool>,
    disarmed: bool,
    x11_dir_uid: Option<u32>,
    our_uid: u32,
) -> BindPlan {
    if resolved_bin == DISTRO_GAMESCOPE_PATH {
        return BindPlan::Off(BindOff::SameBinary);
    }
    // A bare name is not safe to bind AROUND. The wrapper's body is `exec <resolved> …`, so a
    // relative name is resolved through `PATH` *inside* the unit — where it lands on the very path
    // we bound the wrapper over, and the wrapper re-execs itself until the box gives up.
    // [`gamescope_bin`] falls back to a bare name when its `PATH` walk finds nothing, so this is
    // reachable, not theoretical.
    if !resolved_bin.starts_with('/') {
        return BindPlan::Off(BindOff::UnresolvedBinary);
    }
    if enabled == Some(false) {
        return BindPlan::Off(BindOff::OperatorOff);
    }
    if disarmed {
        return BindPlan::Off(BindOff::Disarmed);
    }
    if enabled != Some(true) {
        let Some(script) = script else {
            return BindPlan::Off(BindOff::ScriptUnreadable);
        };
        if !script_hardcodes_gamescope(script) {
            return BindPlan::Off(BindOff::EnvLeverSuffices);
        }
    }
    // The socket directory only needs replacing when it is owned by somebody else (root, on every
    // normal box) — that is exactly the ownership the user namespace cannot map. A directory that
    // is already ours reads as ours inside too, and an ABSENT one is no hazard either: gamescope
    // creates it itself, inside the namespace, owned by us.
    BindPlan::Arm {
        x11: x11_dir_uid.is_some_and(|uid| uid != our_uid),
    }
}

/// An armed redirect, resolved to concrete paths — the single place that knows the systemd
/// spelling, so the transient unit ([`launch_session`]) and the box's own unit
/// ([`write_session_plus_dropin`]) cannot drift apart.
///
/// **Why there are two binds and not one.** The redirect itself is
/// `BindReadOnlyPaths=<wrapper>:/usr/bin/gamescope`: a bind rather than a replacement because
/// `punktfunk-gamescope` ships under its own name precisely so it sits BESIDE the distro package
/// (see packaging/gamescope/README.md), and it is scoped to the unit, so nothing outside sees it
/// and nothing is written to `/usr`.
///
/// But asking a systemd **user** unit for a mount namespace also gets you a **user** namespace —
/// an unprivileged manager cannot create the first without the second — and systemd maps exactly
/// one id into it (`uid_map` = `1000 1000 1`). Every root-owned path the session then inspects
/// reads as `65534`, and gamescope's Xwayland inspects one:
///
/// ```text
/// on disk (no namespace):       drwxrwxrwt 2 0 0      /tmp/.X11-unix
/// inside a user unit, no bind:  drwxrwxrwt 2 0 0      /tmp/.X11-unix
/// inside a user unit WITH bind: drwxrwxrwt 2 65534 65534  /tmp/.X11-unix   ← nobody
/// ```
///
/// wlroots' `check_socket_dir` wants that directory owned by root **or us**, sees `nobody`, and
/// refuses every display number:
///
/// ```text
/// Error wlserver: [xwayland/sockets.c:100] /tmp/.X11-unix not owned by root or us   (×11)
/// Error wlserver: [xwayland/sockets.c:217] No display available in the first 33
/// → SIGSEGV in run_pipewire
/// ```
///
/// Three of those in a row feed `gamescope-session-plus`'s short-session tracker, which gives up,
/// runs `steamos-session-select`, and rewrites the user's session to **plasma** — so the symptom a
/// user reports is "I got thrown onto KDE and can't get back", two removes from the cause, and it
/// survives the reboot they would naturally try. Measured on home-nobara-1 (fc44), canary
/// g13179011, 2026-08-09.
///
/// So the fix is to give the namespace a socket directory it CAN accept: a 0700 directory under
/// `$XDG_RUNTIME_DIR` that we own, bound read-WRITE (Xwayland has to create the socket in it).
/// Inside the namespace the check reads "us" and Xwayland starts.
///
/// **What that costs.** The session's Xwayland socket FILE is then only on the unit's `/tmp`, so a
/// process outside the namespace cannot reach it by path. The one host-side X client is the XFixes
/// cursor reader (`pf-capture`'s `xfixes_cursor`), and it is not in play whenever this bind is:
/// the bind only arms for a resolved `punktfunk-gamescope`, whose patch level 2+ paints the pointer
/// into the capture node itself, so `SessionPlan::gamescope_cursor` is false and the reader is
/// never spawned (`session_plan::gamescope_needs_host_cursor`). On the ATTACH route, where the
/// reader IS spawned, it reaches the display over the ABSTRACT socket `@/tmp/.X11-unix/X<n>` —
/// x11rb tries that before the filesystem path, and an abstract socket lives in the network
/// namespace, which this unit does not get one of. If that ever fails, the reader logs and retries
/// forever; the stream runs without a composited pointer. Nothing else host-side opens an X
/// connection: capture is PipeWire, injection is libei/EIS, clipboard is Wayland.
struct SessionBind {
    wrapper: std::path::PathBuf,
    /// The user-owned directory bound over [`X11_SOCKET_DIR`], or `None` when the real one is
    /// already ours and needs no replacing.
    x11_dir: Option<std::path::PathBuf>,
}

impl SessionBind {
    /// The `[Service]` settings this bind is, one per line — the shared spelling behind both
    /// renderers below.
    fn properties(&self) -> Vec<String> {
        let mut props = vec![format!(
            "BindReadOnlyPaths={}:{DISTRO_GAMESCOPE_PATH}",
            self.wrapper.display()
        )];
        if let Some(dir) = &self.x11_dir {
            // Read-WRITE: Xwayland creates its socket in here.
            props.push(format!("BindPaths={}:{X11_SOCKET_DIR}", dir.display()));
        }
        props
    }

    /// `systemd-run --property=` arguments (the transient unit).
    fn run_args(&self) -> Vec<String> {
        self.properties()
            .into_iter()
            .map(|p| format!("--property={p}"))
            .collect()
    }

    /// Drop-in body lines (the box's own unit).
    fn unit_lines(&self) -> String {
        self.properties()
            .into_iter()
            .map(|p| format!("{p}\n"))
            .collect()
    }
}

/// Owner of [`X11_SOCKET_DIR`] as the host sees it. `lstat`, matching wlroots' own check — a
/// symlink there is not a directory it will accept either.
fn x11_socket_dir_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(X11_SOCKET_DIR)
        .ok()
        .map(|md| md.uid())
}

/// The user-owned socket directory bound over [`X11_SOCKET_DIR`]: per-user, in tmpfs, **0700**.
///
/// 0700 is not decoration. wlroots accepts the directory if it is owned by root or us AND either
/// carries the sticky bit or is not group/other-writable; 0700 satisfies the second under either
/// reading of that rule, and it is the same "not a world-writable /tmp path" discipline every
/// other host-written runtime path in this module follows.
fn session_x11_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-x11")
}

/// Create (or re-assert) [`session_x11_dir`] and drop dead sockets from it.
fn ensure_session_x11_dir() -> Result<std::path::PathBuf> {
    let dir = session_x11_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    prune_stale_x11_sockets(&dir);
    Ok(dir)
}

/// Remove `X<n>` sockets nothing is listening on.
///
/// A session we tore down with SIGKILL ([`kill_unit`], which is how the F44 GPU-context leak is
/// avoided) never unlinks its Xwayland socket. wlroots walks display numbers 0..32 and gives up
/// after the last one, so left alone these corpses would silently cost one display number per
/// relaunch until a long-lived host could no longer start a session at all. On the real
/// `/tmp/.X11-unix` a boot-time tmpfiles clean papers over that; our directory has no such helper.
///
/// The liveness test is a connect: a listening X server accepts instantly, a corpse refuses. So a
/// live session's socket can never be the one removed — including a socket belonging to some other
/// gamescope that happens to share this directory.
fn prune_stale_x11_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::symlink_metadata(&path).is_ok_and(|md| md.file_type().is_socket()) {
            continue;
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            continue; // somebody is listening — not ours to remove
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Prove, on THIS box, that the bind set we are about to arm leaves gamescope a socket directory it
/// will accept — before handing it to a unit whose failure costs the user their session.
///
/// It is the field reproduction, run by the host:
///
/// ```text
/// systemd-run --user --wait --collect --pipe \
///   --property=BindReadOnlyPaths=/etc/hostname:/etc/hostname -- /usr/bin/ls -ldn /tmp/.X11-unix
///   → 65534 65534          # any mount-namespace property, no compensation
/// systemd-run --user --wait --collect --pipe -- /usr/bin/ls -ldn /tmp/.X11-unix
///   → 0 0                  # no namespace at all
/// ```
///
/// Same properties, a throwaway unit, and `stat` reads back the owner of [`X11_SOCKET_DIR`] from
/// INSIDE the namespace. Our uid ⇒ wlroots' "owned by root or us" will pass and the session can
/// start. Anything else — including a unit that would not start, a systemd that rejects one of the
/// properties, or no user manager at all — is a refusal, because every one of those is also a
/// reason the real session would have failed.
///
/// Cached for the process: both bound paths are fixed for a host's lifetime, and the answer is a
/// property of this systemd and this kernel, not of the session. The probe runs no session script,
/// so it cannot feed `gamescope-session-plus`'s short-session tracker.
///
/// Budgeted, because this sits on the CONNECT path: `systemd-run --wait` against a wedged user
/// manager would otherwise block a client's handshake forever, and a probe that cannot answer is
/// a refusal anyway.
fn bind_survives_namespace(bind: &SessionBind) -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--wait", "--collect", "--pipe", "--quiet"]);
        for arg in bind.run_args() {
            cmd.arg(arg);
        }
        // Absolute, because the probe unit's PATH is the user manager's, not ours — and on the
        // very box this matters for, a bare name could resolve through the bind we just made.
        cmd.args(["--", "/usr/bin/stat", "-c", "%u", X11_SOCKET_DIR]);
        let out = crate::proc::output_within(&mut cmd, BIND_PROBE_BUDGET);
        let owner = match &out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok(),
            _ => None,
        };
        let ours = crate::proc::current_uid();
        match owner {
            Some(uid) if uid == ours => {
                tracing::debug!(
                    uid,
                    "gamescope: probed the session unit's namespace — {X11_SOCKET_DIR} reads as \
                     ours inside it, so gamescope's Xwayland will accept it"
                );
                true
            }
            Some(uid) => {
                tracing::warn!(
                    uid,
                    ours,
                    "gamescope: probed the session unit's namespace and {X11_SOCKET_DIR} reads as \
                     uid {uid} inside it, not ours — gamescope's Xwayland would refuse it and the \
                     session would never start. NOT redirecting {DISTRO_GAMESCOPE_PATH}; the \
                     session runs the distro's stock gamescope instead (no HDR, no in-node cursor, \
                     games see 60 Hz) but it starts."
                );
                false
            }
            None => {
                tracing::warn!(
                    error = ?out.as_ref().err(),
                    status = ?out.as_ref().ok().map(|o| o.status),
                    "gamescope: could not probe the session unit's namespace (systemd-run --user \
                     failed, or it rejected one of the bind properties) — NOT redirecting \
                     {DISTRO_GAMESCOPE_PATH}, since whatever stopped the probe would have stopped \
                     the session too"
                );
                false
            }
        }
    })
}

/// How long [`bind_survives_namespace`] may take. Generous for what it is — one transient unit that
/// runs `stat` — and short enough that a wedged user manager costs a connect a moment, not the
/// session.
const BIND_PROBE_BUDGET: Duration = Duration::from_secs(10);

/// Has a session launched WITH the bind armed failed to come up? Same shape and the same reasoning
/// as [`note_spawn_flags_lost`]: a process-wide, one-way latch, because nothing we can observe
/// proves the next session would fare better — and the failure it guards against is the worst one
/// this backend has (a crash-loop that ends with the box's session rewritten to the desktop).
fn bind_disarmed() -> bool {
    BIND_DISARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Disarm the bind for the rest of this host process and say why, as loudly as the failure
/// deserves. `failed_unit` names the unit that did not come up, so the two callers — the transient
/// session and the box's own — read differently in a log.
fn note_bind_hazard(failed_unit: &str) {
    BIND_DISARMED.store(true, std::sync::atomic::Ordering::Relaxed);
    match session_log_refusal() {
        Some(marker) => tracing::error!(
            unit = failed_unit,
            evidence = marker,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed, \
             and its log carries the signature of the reason — a mount namespace in a systemd USER \
             unit is also a USER namespace, in which only this uid is mapped, so root-owned \
             {X11_SOCKET_DIR} reads as `nobody` and gamescope's Xwayland refuses to open a display. \
             Disarming the bind and relaunching without it: the session runs the distro's stock \
             gamescope (no HDR, no in-node cursor, games see 60 Hz) but it STARTS. The bind stays \
             off until this host process restarts."
        ),
        None => tracing::warn!(
            unit = failed_unit,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed. \
             Nothing in the session log names the known cause (the user namespace that comes with \
             the unit's mount namespace, which makes root-owned {X11_SOCKET_DIR} read as `nobody` \
             to gamescope's Xwayland), so this may be an unrelated failure — disarming and \
             relaunching without the bind anyway, because a session that will not start is worse \
             than one without our flags. The bind stays off until this host process restarts."
        ),
    }
}

static BIND_DISARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The lines wlroots prints when the socket-directory check fails, and the one it prints after it
/// has failed for every display number. Either is proof this is the namespace bug and not, say, a
/// Steam that would not start.
const XWAYLAND_REFUSAL_MARKERS: [&str; 2] = [
    "not owned by root or us",
    "No display available in the first",
];

/// Which refusal marker (if any) a session log carries. Pure so the matching is testable against
/// the exact field lines.
fn xwayland_refusal_marker(log: &str) -> Option<&'static str> {
    XWAYLAND_REFUSAL_MARKERS
        .into_iter()
        .find(|marker| log.contains(marker))
}

/// Scan the logs `gamescope-session-plus` writes for the Xwayland refusal — evidence that a failed
/// launch was THIS bug. Best-effort and bounded: a missing or unreadable log just means we cannot
/// confirm it, never that we assume the opposite.
fn session_log_refusal() -> Option<&'static str> {
    let home = std::env::var("HOME").ok()?;
    for name in [".gamescope-stderr.log", ".gamescope-stdout.log"] {
        let path = std::path::Path::new(&home).join(name);
        if let Some(marker) = tail_of(&path, 64 << 10)
            .as_deref()
            .and_then(xwayland_refusal_marker)
        {
            return Some(marker);
        }
    }
    None
}

/// The last `max` bytes of a file, lossily as text. Bounded because these logs are written by
/// someone else's script and a chatty gamescope can make them large.
fn tail_of(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Resolve [`plan_bind`] against the live box, log the outcome for an operator, and return the
/// armed bind (or `None` — in which case the unit gets NO mount namespace, which is the state
/// every box was in before the redirect existed and the state every non-hardcoding distro stays
/// in).
fn arm_session_bind(wrapper: &std::path::Path) -> Option<SessionBind> {
    let plan = plan_bind(
        gamescope_bin(),
        session_script(),
        pf_host_config::config().gamescope_bind,
        bind_disarmed(),
        x11_socket_dir_owner(),
        crate::proc::current_uid(),
    );
    let x11 = match plan {
        BindPlan::Arm { x11 } => x11,
        // Only the arms an operator could act on are worth a line each; `SameBinary` and
        // `EnvLeverSuffices` are the ordinary answer on almost every box.
        BindPlan::Off(BindOff::SameBinary | BindOff::EnvLeverSuffices) => {
            tracing::debug!(
                bin = %gamescope_bin(),
                "gamescope: no {DISTRO_GAMESCOPE_PATH} redirect needed for this session"
            );
            return None;
        }
        BindPlan::Off(BindOff::UnresolvedBinary) => {
            tracing::warn!(
                bin = %gamescope_bin(),
                "gamescope: the resolved gamescope is a bare name, not an absolute path — not \
                 redirecting {DISTRO_GAMESCOPE_PATH}, because the wrapper resolves that name \
                 through PATH inside the unit and would land back on the redirect itself. Put the \
                 binary on the host's PATH, or set PUNKTFUNK_GAMESCOPE_BIN to an absolute path."
            );
            return None;
        }
        BindPlan::Off(BindOff::ScriptUnreadable) => {
            tracing::debug!(
                script = SESSION_PLUS_BIN,
                "gamescope: cannot read the session script, so cannot show a {DISTRO_GAMESCOPE_PATH} \
                 redirect is needed — not arming one"
            );
            return None;
        }
        BindPlan::Off(BindOff::OperatorOff) => {
            tracing::info!(
                "gamescope: PUNKTFUNK_GAMESCOPE_BIND=0 — never redirecting {DISTRO_GAMESCOPE_PATH}. \
                 On a distro whose gamescope-session-plus hardcodes that path the session runs the \
                 distro's stock gamescope: no HDR, no in-node cursor, and games see gamescope's \
                 60 Hz headless default."
            );
            return None;
        }
        BindPlan::Off(BindOff::Disarmed) => {
            tracing::info!(
                "gamescope: the {DISTRO_GAMESCOPE_PATH} redirect is disarmed for this host process \
                 — an earlier session did not come up with it armed. Running the distro's stock \
                 gamescope (no HDR, no in-node cursor). Restart punktfunk-host to try it again."
            );
            return None;
        }
    };
    let x11_dir = if x11 {
        match ensure_session_x11_dir() {
            Ok(dir) => Some(dir),
            // No socket directory we own ⇒ no way to survive the user namespace the redirect
            // costs ⇒ do not arm it. Degraded beats a session that cannot start.
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "gamescope: could not prepare a user-owned {X11_SOCKET_DIR} for the session's \
                     mount namespace — NOT redirecting {DISTRO_GAMESCOPE_PATH}, because the \
                     namespace alone would stop gamescope's Xwayland from opening a display. The \
                     session runs the distro's stock gamescope instead."
                );
                return None;
            }
        }
    } else {
        None
    };
    let bind = SessionBind {
        wrapper: wrapper.to_path_buf(),
        x11_dir,
    };
    // Everything above is a decision about what SHOULD work. This is the one check that asks the
    // box, and it is the last gate before a unit whose failure costs the user their session.
    if !bind_survives_namespace(&bind) {
        return None;
    }
    match &bind.x11_dir {
        Some(dir) => tracing::info!(
            bin = %gamescope_bin(),
            x11_dir = %dir.display(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. A \
             user-owned {X11_SOCKET_DIR} rides along because the mount namespace that redirect \
             costs brings a USER namespace with it, in which the real (root-owned) socket directory \
             reads as `nobody` and gamescope's Xwayland refuses to open a display."
        ),
        None => tracing::info!(
            bin = %gamescope_bin(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. \
             {X11_SOCKET_DIR} is already ours, so the unit's user namespace maps it unchanged and \
             it needs no replacing."
        ),
    }
    Some(bind)
}

/// The environment that turns the distro's `VkLayer_FROG_gamescope_wsi` off for a whole session
/// tree — applied wherever we start one: [`launch_session`]'s transient unit and the box-session
/// drop-in ([`write_session_plus_dropin`]).
///
/// ⭐⭐⭐ **`DISABLE_GAMESCOPE_WSI` is the one that actually works, and it is not the obvious one.**
/// `gamescope-session-plus` does an unconditional `export ENABLE_GAMESCOPE_WSI=1` near the top of
/// the script, before it launches anything — so a `--setenv=ENABLE_GAMESCOPE_WSI=0` on the unit is
/// CLOBBERED for the script and every child it spawns: gamescope, Steam, and every game Steam
/// launches. The Vulkan loader resolves an implicit layer's two manifest knobs in a fixed order
/// (`loader.c`, `loader_implicit_layer_is_enabled`): `enable_environment` switches the layer on
/// only when the variable equals exactly `"1"`, and then `disable_environment` is consulted last —
/// *"has priority over everything else"* — where the mere PRESENCE of the variable, at any value,
/// forces the layer off. Nothing in the session script mentions `DISABLE_GAMESCOPE_WSI`, so it is
/// the only one of the two that survives the script.
///
/// ⚠️⚠️ What getting this wrong looks like in the field (Nobara 44, 2026-08-11): the layer stayed
/// on for games while this host's own log said it had been disabled. Neither Steam Big Picture nor
/// mangoapp is a Vulkan client, so both paint regardless and the session looks perfectly healthy —
/// right up until a game starts. Then the game runs with sound and input while its swapchain is
/// dead: a black screen, and not one line of error anywhere.
///
/// `ENABLE_GAMESCOPE_WSI=0` is kept alongside for a layer built without a `disable_environment`
/// (the loader warns about such a layer but honours its `enable_environment`), and because it is
/// what an operator reading the unit will look for.
const WSI_OFF_ENV: [(&str, &str); 2] = [
    ("DISABLE_GAMESCOPE_WSI", "1"),
    ("ENABLE_GAMESCOPE_WSI", "0"),
];

/// [`WSI_OFF_ENV`] as `systemd-run` arguments, for the transient unit.
fn wsi_off_setenv_args() -> Vec<String> {
    WSI_OFF_ENV
        .iter()
        .map(|(name, value)| format!("--setenv={name}={value}"))
        .collect()
}

/// [`WSI_OFF_ENV`] as unit-file lines, for the box-session drop-in. Trailing newline included, so
/// whatever the body puts after it still parses — same contract as [`SessionBind::unit_lines`].
fn wsi_off_unit_lines() -> String {
    WSI_OFF_ENV
        .iter()
        .map(|(name, value)| format!("Environment={name}={value}\n"))
        .collect()
}

/// Whether the box's `VkLayer_FROG_gamescope_wsi` can be trusted against the gamescope we run.
///
/// The layer ships with the DISTRO's gamescope and speaks its `gamescope_swapchain` protocol; we
/// run our own build. When the two disagree the compositor rejects the client's
/// `swapchain_feedback` ("message too short") and **kills every Vulkan client** — Steam never
/// paints and the stream is a black screen with no error anywhere else.
///
/// Measured on Nobara 44 (`vkcube` under each build, layer on):
/// distro 3.16.23.2 → 0 errors; our 3.16.25 → 1 rejected client. The upstream protocol XML is
/// byte-identical between those commits, so this is the distro PATCHING gamescope, not a version
/// bump — which is why the check is "do the version triples differ", not a floor.
///
/// Disabling it costs only the layer's extras (XWayland bypass, present-mode control, client HDR
/// metadata) — far cheaper than a client that cannot start.
///
/// ⚠️ **`ENABLE_GAMESCOPE_WSI=0` is NOT enough on its own**, which is what [`WSI_OFF_ENV`] is for.
fn wsi_layer_matches_our_gamescope() -> bool {
    let ours = discovery::gamescope_version_of(std::path::Path::new(gamescope_bin()));
    let distro = discovery::gamescope_version_of(std::path::Path::new(DISTRO_GAMESCOPE_PATH));
    match (ours, distro) {
        // Same upstream triple ⇒ the layer was built from the same protocol. Keep it.
        (Some(a), Some(b)) => a == b,
        // Either side unreadable: leave the layer alone rather than degrade a box that works.
        _ => true,
    }
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
    // ONE rate reaches gamescope, and it is `--nested-refresh` (via the wrapper's `PF_HZ`). On the
    // headless backend that flag IS the output refresh — `CHeadlessBackend::Init` assigns
    // `g_nOutputRefresh = g_nNestedRefresh` — so it is simultaneously the rate the session
    // composites at, the rate `vblankmanager` paces to, and the rate Steam and every game are told
    // the display runs at. When the frame limiter (`PUNKTFUNK_MAX_FPS`) is set they all drop
    // together; that is the trade the knob is, and it is off by default.
    //
    // `CUSTOM_REFRESH_RATES` below does NOT do this, whatever its name suggests: it is the *set* of
    // rates the session may offer, and `gamescope-session-plus` gates it on the binary having
    // `--custom-refresh-rates`, which no upstream gamescope has ever had. On a stock gamescope it
    // is inert (it was a silent no-op for years); on our `+pfhdr3` build it is what puts more than
    // one entry in Steam's refresh menu. Either way it cannot fix a wrong `--nested-refresh`.
    let game = game_hz(mode.refresh_hz);
    // The advertised SET, which always contains the rate we actually run at.
    let offered = {
        let mut r = pf_host_config::config().gamescope_refresh_rates.clone();
        if !r.contains(&hz) {
            r.push(hz);
        }
        r.sort_unstable();
        r.dedup();
        r.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
    };
    // Redirect a hardcoded `/usr/bin/gamescope` at our wrapper, for session scripts that never
    // read `GAMESCOPE_BIN` (Nobara). `mut` because the backstop below drops it and relaunches:
    // an armed bind is the one thing here that can stop the session coming up at all.
    let mut bind = arm_session_bind(&wrapper);
    // The distro's Vulkan WSI layer speaks the distro gamescope's protocol; ours may differ, and a
    // mismatch kills every Vulkan client with no error but a black screen. Steam Big Picture is not
    // one of them, so the casualty is the GAMES — see [`WSI_OFF_ENV`] for why both variables go.
    let wsi_ok = wsi_layer_matches_our_gamescope();
    if !wsi_ok {
        tracing::warn!(
            "gamescope: this box's VkLayer_FROG_gamescope_wsi was built for a different gamescope \
             than the one we run — disabling it for this session (DISABLE_GAMESCOPE_WSI=1, which \
             the session script cannot clobber the way it clobbers ENABLE_GAMESCOPE_WSI). Left \
             enabled it rejects the client's swapchain_feedback and every Vulkan client dies; \
             Steam's own UI is not one, so what you see is a game that runs with sound and input \
             on a black screen, with no other symptom."
        );
    }
    let start_unit = |bind: Option<&SessionBind>| -> Result<()> {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--collect", &format!("--unit={unit_name}")]);
        for arg in bind.map(SessionBind::run_args).unwrap_or_default() {
            cmd.arg(arg);
        }
        if !wsi_ok {
            for arg in wsi_off_setenv_args() {
                cmd.arg(arg);
            }
        }
        // Same headless-must-not-attach rule as [`spawn`]: the transient unit inherits the
        // user manager env, which can carry a (possibly stale) desktop DISPLAY/WAYLAND_DISPLAY
        // that would abort gamescope at startup.
        cmd.arg("--property=UnsetEnvironment=DISPLAY WAYLAND_DISPLAY")
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
            .arg(format!("--setenv=CUSTOM_REFRESH_RATES={offered}"))
            .arg("--")
            .arg(SESSION_PLUS_BIN)
            .arg(client);
        // Budgeted: without `--wait`, `systemd-run` returns as soon as the manager has accepted the
        // transient unit, so the only way this takes seconds is a user manager that is not
        // answering — the exact case the caller's error text is written for, and one that used to
        // pin the connecting client's thread indefinitely instead.
        let status = crate::proc::status_within(&mut cmd, UNIT_VERB_BUDGET).context(
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
    start_unit(bind.as_ref())?;
    // Steam Big Picture cold-start is far slower than a bare app — poll the node for up to 45s.
    let mut deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(id) = find_gamescope_node() {
            // `GAMESCOPE_BIN` is a session-plus convention, not a guarantee — confirm the session
            // honoured it before we trust the capabilities the plan was already built on. Stop the
            // unit on rejection so the retry relaunches instead of reusing what we just refused.
            if let Err(e) = verify_managed_spawn_flags(hdr) {
                stop_session(unit_name);
                return Err(e);
            }
            // Loud, but not fatal — see [`warn_if_mode_lost`] for why this one warns where the
            // capability flags above refuse.
            warn_if_mode_lost(mode, game);
            return Ok(id);
        }
        if Instant::now() >= deadline {
            stop_session(unit_name);
            // RUNTIME BACKSTOP. An armed bind is the only thing here that can make the session
            // fail to start *because of us*, and the way it fails is the expensive one: gamescope
            // dies over and over, the session-plus short-session tracker gives up, and the box's
            // session is rewritten to the desktop — a state that survives the reboot a user tries.
            // So spend the second 45 s without it rather than returning a failure: a stock-gamescope
            // session (no HDR, no in-node cursor, 60 Hz) is a working Game Mode, and the latch keeps
            // every later launch this process makes on that path.
            if bind.take().is_some() {
                note_bind_hazard(unit_name);
                start_unit(None)?;
                deadline = Instant::now() + Duration::from_secs(45);
                continue;
            }
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
            let _ = crate::proc::status_within(
                Command::new("systemctl").args(["--user", "reset-failed", unit_name]),
                UNIT_VERB_BUDGET,
            );
            start_unit(bind.as_ref())?;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Is the unit currently starting or up (`activating` / `active` — also `deactivating`: let a stop
/// finish; the next poll tick sees the settled state)? Unknown/unreachable states report `true` so a
/// systemctl hiccup can't trigger a relaunch storm.
///
/// Budgeted at [`UNIT_STATE_BUDGET`], and this is the site where that matters most: it is polled
/// every 500 ms inside `launch_session`'s 45 s node-wait, so one unbounded `is-active` against a
/// wedged user manager blew the entire deadline in a single tick — and a timeout reads as `true`,
/// which is already the documented "don't storm" answer.
fn unit_starting_or_active(unit: &str) -> bool {
    let Ok(out) = crate::proc::output_within(
        Command::new("systemctl").args(["--user", "is-active", unit]),
        UNIT_STATE_BUDGET,
    ) else {
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
    // `-r` above is what this headless session will REPORT as its refresh (the headless backend
    // assigns `g_nOutputRefresh = g_nNestedRefresh`), so it is already correct here. This adds the
    // rest of the SET the in-session UI may offer — the bare spawn passes it directly, with none of
    // the session-script indirection the managed path has to route it through.
    for arg in hdr_args(hdr)
        .into_iter()
        .chain(cursor_args())
        .chain(refresh_rate_args(hz))
    {
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
    let mut args = Vec::new();
    if gamescope_can_composite_cursor() {
        args.push("--pipewire-composite-cursor".to_string());
    }
    // The external overlay (mangoapp — the Deck UI's fps/frametime readout, patch level 4+). Unlike
    // the cursor there is no host-side fallback: the host cannot reconstruct another process's
    // overlay window, so without this the layer is simply absent from every gamescope stream.
    if gamescope_can_composite_external_overlay() {
        args.push("--pipewire-composite-external-overlay".to_string());
    }
    args
}

/// `--custom-refresh-rates <list>` when the resolved gamescope has it (patch level 3+): the rates a
/// HEADLESS session may offer its clients.
///
/// Without it a headless connector advertises exactly one rate, so Steam's in-session display
/// settings show a single entry and a game reads the display as that one number. `session_hz` is
/// always in the list — it is the mode the session actually runs at, and an advertised set that
/// excluded it would be a lie in the other direction.
///
/// The operator can widen the set (`PUNKTFUNK_GAMESCOPE_REFRESH_RATES=60,90,120`) so the in-session
/// UI offers real choices; unset, we advertise the one rate we run at, which is what the client
/// asked for.
fn refresh_rate_args(session_hz: u32) -> Vec<String> {
    if !gamescope_can_offer_refresh_rates() {
        return Vec::new();
    }
    let mut rates = pf_host_config::config().gamescope_refresh_rates.clone();
    if !rates.contains(&session_hz) {
        rates.push(session_hz);
    }
    rates.sort_unstable();
    rates.dedup();
    vec![
        "--custom-refresh-rates".to_string(),
        rates
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
    ]
}

/// What a bare SPAWN will actually run, resolved ONCE per `create`: the per-session launch command
/// ([`VirtualDisplay::set_launch_command`]) wins; else the process-global `PUNKTFUNK_GAMESCOPE_APP`
/// (the documented manual fallback); else `None`, meaning [`spawn`]'s no-op keep-alive. Each level
/// is taken only if non-empty, so a blank per-session command transparently falls through to the
/// env.
///
/// Split out of [`spawn`] because `create` has to gate the Steam single-instance free on the SAME
/// answer `spawn` later turns into `--steam`. It did not: it tested only the per-session command,
/// so the env fallback reached `spawn`, was recognised as Steam there, and got the integration
/// flag with none of the instance work — a nested second Steam that sees the box's and exits,
/// taking the spawn down with it. It also removes the second env read from the spawn path: one
/// [`crate::with_env_lock`] acquisition per create instead of two.
fn resolved_spawn_app(cmd: Option<&str>) -> Option<String> {
    cmd.map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        // Read the env fallback under the shared env lock so it can't race a concurrent session's
        // `set_var` of the same key (security-review 2026-06-28 #7).
        .or_else(|| crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_APP").ok()))
        .filter(|s| !s.trim().is_empty())
}

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. `app` is this session's resolved
/// launch command ([`resolved_spawn_app`] — the per-session `set_launch_command`, else the
/// `PUNKTFUNK_GAMESCOPE_APP` fallback); `None` runs a no-op that just keeps gamescope alive.
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
    app: Option<String>,
    log: &std::path::Path,
    hdr: bool,
) -> Result<Child> {
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
        any_output_size_is, cgroup_is_punktfunk_owned, cgroup_under_user_manager,
        classify_output_size, connected_connector_under, display_manager_unit_under, dm_plan,
        dm_survives_masked_unit, game_hz, gamescope_output_size, hdr_args, is_steam_launch,
        mask_unit, missing_flags, mode_mismatch, nested_wrapper_script, plan_bind,
        release_autologin_mask, script_hardcodes_gamescope, sentinel_advanced,
        shape_dedicated_command, switch_ends_mask_window, takeover_state_is_live, unmask_unit,
        wsi_off_setenv_args, wsi_off_unit_lines, xwayland_refusal_marker, BindOff, BindPlan,
        BoxOutputSize, DmHelperError, SessionBind, TakeoverState, AUTOLOGIN_MASKED,
        DISTRO_GAMESCOPE_PATH, STOPPED_AUTOLOGIN, WSI_OFF_ENV, X11_SOCKET_DIR,
    };

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// The output-size probe answers for the box, and three consumers read it as "this session's
    /// size" — the libei injector's coordinate scale among them. A box running two gamescopes (a
    /// Game Mode session plus a nested per-title one, the shape `heads.rs` pins as normal) has no
    /// single answer, and the original `find_map` returned whichever `/proc` `read_dir` yielded
    /// first. "Cannot tell" is a state every consumer can handle; a confident wrong number is not.
    #[test]
    fn the_output_size_probe_refuses_to_pick_between_disagreeing_gamescopes() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080 --prefer-output HDMI-A-1");
        let nested = argv("gamescope --backend wayland -W 1280 -H 800");
        // One compositor, or several that agree — a plain answer.
        assert_eq!(
            classify_output_size(std::slice::from_ref(&session)),
            BoxOutputSize::Known((1920, 1080))
        );
        assert_eq!(
            classify_output_size(&[session.clone(), session.clone()]),
            BoxOutputSize::Known((1920, 1080))
        );
        // Disagreement is AMBIGUOUS, not a coin flip — in either enumeration order.
        assert_eq!(
            classify_output_size(&[session.clone(), nested.clone()]),
            BoxOutputSize::Ambiguous
        );
        assert_eq!(
            classify_output_size(&[nested.clone(), session.clone()]),
            BoxOutputSize::Ambiguous
        );
        // Nothing to go on at all is its OWN state, and it must not be confused with the one above:
        // `ensure_box_gamescope_mode` proceeds to re-mode on `Unreported` (there is no second
        // opinion to be wrong about) and must never re-mode on `Ambiguous` (the second opinion is
        // very likely the session the user's game is running in).
        assert_eq!(classify_output_size(&[]), BoxOutputSize::Unreported);
        assert_eq!(
            classify_output_size(&[argv("gamescope --steam")]),
            BoxOutputSize::Unreported
        );
        assert_ne!(BoxOutputSize::Unreported, BoxOutputSize::Ambiguous);
    }

    /// After we restart the box's own unit at `target`, the question is "did what we asked for come
    /// up", NOT "is the box unanimous". Testing unanimity there let one unrelated gamescope at
    /// another size — a kept bare spawn of ours for a different client, which does NOT die with the
    /// restarted unit — hold the answer at `Ambiguous` for the full 45 s wait, the bind-hazard
    /// backstop's second restart and another 45 s, and then fail a connect whose session was up all
    /// along (having bounced the operator's Game Mode twice on the way).
    #[test]
    fn the_post_restart_wait_asks_whether_the_target_size_is_present() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080");
        let stray = argv("punktfunk-gamescope --backend headless -W 1280 -H 720");
        assert!(any_output_size_is(
            std::slice::from_ref(&session),
            (1920, 1080)
        ));
        // The stray one neither satisfies nor blocks the answer.
        assert!(any_output_size_is(
            &[stray.clone(), session.clone()],
            (1920, 1080)
        ));
        assert!(!any_output_size_is(
            std::slice::from_ref(&stray),
            (1920, 1080)
        ));
        assert!(!any_output_size_is(&[], (1920, 1080)));
        // …and unanimity WOULD have blocked it — the regression this predicate replaces.
        assert_eq!(
            classify_output_size(&[stray, session]),
            BoxOutputSize::Ambiguous
        );
    }

    /// `-W`/`-H` must be read as a pair off ONE argv, and the long spellings count: a half-answer
    /// would otherwise be published as a monitor row (`heads_under`) or a pointer scale.
    #[test]
    fn output_size_needs_both_flags_from_the_same_argv() {
        assert_eq!(
            gamescope_output_size(&argv("gamescope -W 2560 -H 1440")),
            Some((2560, 1440))
        );
        assert_eq!(
            gamescope_output_size(&argv("gamescope --output-width 800 --output-height 600")),
            Some((800, 600))
        );
        assert_eq!(gamescope_output_size(&argv("gamescope -W 2560")), None);
        assert_eq!(gamescope_output_size(&argv("gamescope -H 1440")), None);
        // The NESTED size (`-w`/`-h`) is a different thing and must never stand in for the output.
        assert_eq!(
            gamescope_output_size(&argv("gamescope -w 1280 -h 800")),
            None
        );
    }

    /// A persisted takeover exists so a host CRASH can heal the box. The file used to be written
    /// (and, on the next start, adopted) only when one of the three *stealing* fields was set — so
    /// a managed session that took nothing over, the one arm `takeover_live` grew specifically
    /// because such a session was "ORPHANED forever after disconnect", was recorded as nothing at
    /// all and its transient unit outlived the host.
    #[test]
    fn a_managed_session_alone_is_a_takeover_worth_persisting() {
        let nothing = TakeoverState::default();
        assert!(!takeover_state_is_live(&nothing));
        let managed = TakeoverState {
            managed_session: true,
            ..Default::default()
        };
        assert!(takeover_state_is_live(&managed));
        // …and the three original fields keep their meaning, each on its own.
        assert!(takeover_state_is_live(&TakeoverState {
            stopped_autologin: vec!["gamescope-session-plus@steam.service".into()],
            ..Default::default()
        }));
        assert!(takeover_state_is_live(&TakeoverState {
            steamos: true,
            ..Default::default()
        }));
        assert!(takeover_state_is_live(&TakeoverState {
            stopped_dm: Some("sddm.service".into()),
            ..Default::default()
        }));
    }

    /// The ATTACH re-mode path steals nothing, so all four fields above stay empty — but it pins
    /// `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` in the `systemd --user` MANAGER, which
    /// outlives this process and every unit restart for the rest of the login. Nothing sweeps them
    /// (`restore_takeover_on_startup`'s unconditional prologue removes the drop-in and nothing
    /// else), and nothing may sweep them blind, so the persisted flag is the only thing that lets a
    /// host which was SIGKILLed mid-stream know they are ours to take back. Written as "nothing to
    /// restore", the file was deleted on the spot and the operator's own Game Mode came up at the
    /// last client's resolution until they logged out.
    #[test]
    fn a_forced_session_resolution_alone_is_a_takeover_worth_persisting() {
        assert!(takeover_state_is_live(&TakeoverState {
            forced_screen_env: true,
            ..Default::default()
        }));
    }

    /// An older host's takeover file has neither new field; it must still parse (the box it
    /// describes is mid-takeover, and refusing the file is refusing the restore).
    #[test]
    fn an_older_takeover_file_still_parses() {
        let old =
            r#"{"stopped_autologin":["gamescope-session-plus@steam.service"],"steamos":false}"#;
        let state: TakeoverState = serde_json::from_str(old).expect("older file parses");
        assert_eq!(state.stopped_autologin.len(), 1);
        assert!(!state.managed_session, "absent field defaults to false");
        assert!(takeover_state_is_live(&state));
    }

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

    /// The whole point of [`DmHelperError`]: a failure has to say which of the four things went
    /// wrong, because they need four different fixes. Pins the two properties that were violated
    /// in the field — the helper's own words survive to the operator, and a helper that never RAN
    /// never reads as one that ran and refused.
    #[test]
    fn dm_helper_failures_stay_distinguishable() {
        let refusal = "pf-dm-helper: user 'nobara-user' is not in the 'punktfunk' group — \
                       refusing. Grant it with: sudo usermod -aG punktfunk nobara-user";
        let ran = DmHelperError::Refused {
            helper: "/usr/libexec/punktfunk/pf-dm-helper",
            code: Some(1),
            stderr: refusal.to_string(),
        }
        .to_string();
        // Verbatim: the helper already names the user, the group and the exact command.
        assert!(ran.contains(refusal), "{ran}");
        assert!(ran.contains("ran and refused"), "{ran}");

        // …and none of the three "it never got that far" shapes may claim a refusal, or the
        // operator goes looking for a group problem that isn't there.
        for e in [
            DmHelperError::NotInstalled,
            DmHelperError::NotExecutable {
                helper: "/usr/libexec/punktfunk/pf-dm-helper",
                io: "No such file or directory (os error 2)".into(),
            },
            DmHelperError::Denied {
                helper: "/usr/libexec/punktfunk/pf-dm-helper",
                code: 127,
                stderr: "Error executing command as another user: Not authorized".into(),
            },
        ] {
            let s = e.to_string();
            assert!(!s.contains("ran and refused"), "{s}");
            // …and none of them may send the operator after group membership, which is only ever
            // the answer when the helper actually evaluated it.
            assert!(!s.contains("group"), "{s}");
            // Every one of them still ends in something the operator can act on.
            assert!(s.contains("polkit") || s.contains("install"), "{s}");
        }

        // The remedies the old fixed guess offered — a reinstall and a polkit rule — must appear
        // ONLY where they can actually help, never on the path that ran and was refused.
        assert!(!ran.contains("reinstall"), "{ran}");
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
    fn only_a_desktop_switch_ends_the_mask_window() {
        use crate::ActiveKind;
        // The user switched the box to a desktop session mid-stream: our managed game session is
        // over, so the mask defends nothing — and the "Return to Gaming Mode" that follows has to
        // be able to start the unit (the distro session script starts exactly it).
        for kind in [
            ActiveKind::DesktopKde,
            ActiveKind::DesktopGnome,
            ActiveKind::DesktopWlroots,
            ActiveKind::DesktopHyprland,
        ] {
            assert!(switch_ends_mask_window(kind), "{kind:?}");
        }
        // A takeover's own managed session reads as Gaming, so lifting here would void the mask for
        // the whole stream — in exactly the SDDM-relogin window it exists for. (Coming BACK to
        // gaming needs no lift either: it evidently already started.)
        assert!(!switch_ends_mask_window(ActiveKind::Gaming));
        // A managed session momentarily down between relaunches reads as None. That is
        // mid-takeover, not the end of one.
        assert!(!switch_ends_mask_window(ActiveKind::None));
    }

    /// The same rule as above, but end-to-end against REAL systemd — that the decision is actually
    /// wired to the mask, that a lift leaves the restart list intact, and that `--runtime` is what
    /// comes off (a plain `unmask` does NOT clear a runtime mask, which is the whole reason the
    /// stranded mask survived every non-reboot remedy in the field).
    ///
    /// Ignored by default: it needs a live `systemd --user` manager. On a Linux box with a session:
    /// `cargo test -p pf-vdisplay -- --ignored the_mask_comes_off`. Uses a unit name nothing owns —
    /// `mask` works on a non-existent unit (it is just a symlink to `/dev/null`), so this never goes
    /// near the box's real gaming session.
    #[test]
    #[ignore = "needs a live systemd --user manager (run explicitly on a Linux box with a session)"]
    fn the_mask_comes_off_only_when_the_box_takes_itself_back() {
        const PROBE: &str = "punktfunk-mask-probe@lifetime-test.service";
        let is_enabled = || {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "is-enabled", PROBE])
                .output()
                .expect("systemctl --user");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        unmask_unit(PROBE); // a previous failed run must not decide this one

        // Lay the takeover's mask exactly as `stop_autologin_sessions` does.
        *STOPPED_AUTOLOGIN.lock().unwrap() = vec![PROBE.to_string()];
        *AUTOLOGIN_MASKED.lock().unwrap() = true;
        mask_unit(PROBE);
        assert_eq!(is_enabled(), "masked-runtime");

        // Mid-stream, with the box still ours: the mask is doing its job and must stay. `Gaming` is
        // what our own managed session reads as, and `None` is one momentarily down between
        // relaunches — lifting on either would void the mask for the whole stream.
        release_autologin_mask(crate::ActiveKind::Gaming);
        release_autologin_mask(crate::ActiveKind::None);
        assert_eq!(is_enabled(), "masked-runtime");

        // The user switched the box to its own desktop mid-stream: the window is over, and the way
        // back into game mode has to be clear before they ask for it.
        release_autologin_mask(crate::ActiveKind::DesktopKde);
        assert_ne!(is_enabled(), "masked-runtime");
        // The restart list SURVIVES the lift: the mask's lifetime is shorter than the takeover's,
        // and the disconnect restore still owes these units a `start`.
        assert_eq!(STOPPED_AUTOLOGIN.lock().unwrap().as_slice(), [PROBE]);
        // Idempotent — the watcher calls it on every switch it confirms.
        release_autologin_mask(crate::ActiveKind::DesktopGnome);
        assert_ne!(is_enabled(), "masked-runtime");

        unmask_unit(PROBE);
        STOPPED_AUTOLOGIN.lock().unwrap().clear();
        *AUTOLOGIN_MASKED.lock().unwrap() = false;
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

    /// The silent-60Hz guard. A headless gamescope reports `--nested-refresh` as its ONE refresh
    /// rate and falls back to 60 Hz when the flag never arrives, so a session that lost the
    /// `GAMESCOPE_BIN` wrapper streams at the client's rate while telling every game it is 60 —
    /// the exact shape of the 2026-08-08 field report, and invisible without this.
    #[test]
    fn mode_mismatch_names_what_the_session_actually_got() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };

        // The good case: our own managed spawn, carrying everything we asked for.
        let ok = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --nested-refresh 120 --steam",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &ok).is_empty());

        // THE field case: the wrapper was dropped, so there is no `--nested-refresh` anywhere and
        // gamescope silently ran its 60 Hz default. Size still landed (SCREEN_WIDTH survived).
        let lost = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --steam",
        )];
        let got = mode_mismatch(1920, 1080, 120, &lost);
        assert_eq!(got.len(), 1, "only the refresh is wrong: {got:?}");
        assert!(got[0].contains("asked=120Hz"), "{got:?}");
        assert!(got[0].contains("no --nested-refresh at all"), "{got:?}");

        // A wrong rate is reported with the number it actually got, not just "missing".
        let wrong = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh 60")];
        let got = mode_mismatch(1920, 1080, 120, &wrong);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("got=60Hz"), "{got:?}");

        // Resolution lost too (SCREEN_WIDTH/HEIGHT dropped as well) — both are named.
        let both = vec![argv("gamescope -W 1280 -H 720")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &both).len(), 2);

        // Fail OPEN, exactly like `missing_flags`: nothing to compare against says nothing. A box
        // with a second gamescope that carries no output size must not produce a false alarm.
        assert!(mode_mismatch(1920, 1080, 120, &[]).is_empty());

        // ANY running gamescope carrying the mode satisfies it — a Deck commonly runs a nested one
        // beside the session, and demanding that every gamescope match would reject a good session.
        let two = vec![
            argv("gamescope -W 1280 -H 800 --nested-refresh 60"),
            argv("gamescope -W 1920 -H 1080 --nested-refresh 120"),
        ];
        assert!(mode_mismatch(1920, 1080, 120, &two).is_empty());

        // The long spellings are read too.
        let long = vec![argv(
            "gamescope --output-width 1920 --output-height 1080 --nested-refresh 120",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &long).is_empty());

        // A flag with no value after it must not panic or read past the end.
        let truncated = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &truncated).len(), 1);
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

    /// Nobara's line 244 verbatim (the file that started this), against the upstream shape that
    /// honours the documented lever. Only the first needs a bind — and getting that backwards
    /// costs every OTHER distro a mount namespace it has no use for, which is what made 0.26.0-1
    /// unstartable on boxes that never had the hardcode at all.
    #[test]
    fn only_a_script_that_hardcodes_the_path_needs_the_bind() {
        let nobara = "if [ -z \"$GAMESCOPECMD\" ]; then\n    \
                      GAMESCOPECMD=\"/usr/bin/gamescope \\\n";
        assert!(script_hardcodes_gamescope(nobara));

        // Upstream / Bazzite: the env var is right there in the script.
        let upstream = "GAMESCOPE_BIN=${GAMESCOPE_BIN:-/usr/bin/gamescope}\n\
                        GAMESCOPECMD=\"$GAMESCOPE_BIN \\\n";
        assert!(
            !script_hardcodes_gamescope(upstream),
            "a script that reads GAMESCOPE_BIN needs no namespace to reach our binary"
        );

        // Mentions the var but not the path, and vice versa in the other direction: neither is the
        // shape the bind fixes, so neither arms it.
        assert!(!script_hardcodes_gamescope(
            "exec ${GAMESCOPE_BIN} \"$@\"\n"
        ));
        assert!(!script_hardcodes_gamescope("exec gamescope \"$@\"\n"));

        // The two longer paths that START with the one we redirect. A plain `contains` would read
        // either as a hardcode and buy the box a mount namespace for nothing.
        assert!(!script_hardcodes_gamescope(
            "/usr/bin/gamescopectl takescreenshot\n"
        ));
        assert!(!script_hardcodes_gamescope(
            "exec /usr/bin/gamescope-session-plus \"$@\"\n"
        ));
        // …while the real one still counts, whether a quote, a space or a newline follows it.
        assert!(script_hardcodes_gamescope("GS=\"/usr/bin/gamescope\"\n"));
        assert!(script_hardcodes_gamescope("exec /usr/bin/gamescope\n"));
        assert!(script_hardcodes_gamescope(
            "exec /usr/bin/gamescope -W 1920\n"
        ));
    }

    /// The decision matrix, in the order the arms are meant to win. Every `Off` arm here is a box
    /// that gets NO mount namespace — the state every box was in before the redirect existed.
    #[test]
    fn bind_is_armed_only_where_it_is_needed_and_survivable() {
        const OURS: &str = "/usr/bin/punktfunk-gamescope";
        const AUTO: Option<bool> = None;
        const OFF: Option<bool> = Some(false);
        const FORCE: Option<bool> = Some(true);
        let hardcoded = Some("GAMESCOPECMD=\"/usr/bin/gamescope -W\"");
        let honours = Some("GAMESCOPECMD=\"${GAMESCOPE_BIN} -W\"");

        // The case the mechanism exists for: hardcoding script, our binary, root-owned socket dir
        // ⇒ redirect AND replace the socket directory, or the namespace kills Xwayland.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );

        // A socket directory already owned by us maps to us inside the user namespace too, so
        // there is nothing to compensate for — and an ABSENT one is created, inside, by us.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(1000), 1000),
            BindPlan::Arm { x11: false }
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, None, 1000),
            BindPlan::Arm { x11: false }
        );

        // Nothing to redirect: the resolved binary IS the distro path. Checked before everything
        // else, because it is true regardless of what the script or the operator says.
        assert_eq!(
            plan_bind(
                DISTRO_GAMESCOPE_PATH,
                hardcoded,
                FORCE,
                false,
                Some(0),
                1000
            ),
            BindPlan::Off(BindOff::SameBinary)
        );

        // A bare name — `gamescope_bin`'s fallback when its PATH walk finds nothing. Binding
        // around it makes the wrapper `exec` a name that now resolves to the wrapper, so not even
        // a forcing operator gets it.
        assert_eq!(
            plan_bind("gamescope", hardcoded, FORCE, false, Some(0), 1000),
            BindPlan::Off(BindOff::UnresolvedBinary)
        );

        // The ordinary answer on Bazzite/SteamOS-likes: the env lever lands, so no namespace.
        assert_eq!(
            plan_bind(OURS, honours, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::EnvLeverSuffices)
        );

        // Fail closed on a box we cannot inspect.
        assert_eq!(
            plan_bind(OURS, None, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::ScriptUnreadable)
        );

        // Both retreats outrank the need: an operator's `=0`, and the runtime backstop's latch.
        assert_eq!(
            plan_bind(OURS, hardcoded, OFF, false, Some(0), 1000),
            BindPlan::Off(BindOff::OperatorOff)
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );

        // `=1` skips the script probe (for a GAMESCOPE_BIN defeated in a `sessions.d` fragment the
        // host cannot read) — but it does NOT outrank the backstop, or a forced box that
        // crash-loops would never stop.
        assert_eq!(
            plan_bind(OURS, honours, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, None, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, honours, FORCE, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );
    }

    /// The two renderers must spell the SAME settings — the transient unit takes them as
    /// `systemd-run --property=`, the box's own unit as drop-in lines, and a session that got only
    /// half of them is the crash this whole mechanism is guarding.
    #[test]
    fn both_bind_renderers_carry_the_socket_directory() {
        let bind = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: Some(std::path::PathBuf::from("/run/user/1000/punktfunk-x11")),
        };
        assert_eq!(
            bind.run_args(),
            vec![
                format!(
                    "--property=BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}"
                ),
                format!("--property=BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}"),
            ]
        );
        assert_eq!(
            bind.unit_lines(),
            format!(
                "BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}\n\
                 BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}\n"
            )
        );
        // The socket-directory bind must be read-WRITE (Xwayland creates its socket in there); a
        // read-only one would fail exactly as loudly as no bind at all.
        assert!(bind.unit_lines().contains("BindPaths="));

        // No compensation needed ⇒ exactly one setting, and the unit line ends in a newline so the
        // `Environment=` lines after it in the drop-in body still parse.
        let plain = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: None,
        };
        assert_eq!(plain.run_args().len(), 1);
        assert!(plain.unit_lines().ends_with('\n'));
        assert!(!plain.unit_lines().contains("BindPaths="));
    }

    /// The exact lines the failing box logged (home-nobara-1, canary g13179011). They are what
    /// turns the backstop's message from "something went wrong" into a named cause.
    #[test]
    fn the_xwayland_refusal_is_recognised_from_a_real_log() {
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:100] /tmp/.X11-unix not owned by root or us\n"
            ),
            Some("not owned by root or us")
        );
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:217] No display available in the first 33\n"
            ),
            Some("No display available in the first")
        );
        // A session that failed for some other reason must NOT be reported as this bug — the
        // backstop still disarms, but it says so differently.
        assert_eq!(
            xwayland_refusal_marker("steam.sh: line 1: pipewire: command not found\n"),
            None
        );
    }

    /// `gamescope-session-plus` runs `export ENABLE_GAMESCOPE_WSI=1` before it launches anything,
    /// so that variable alone cannot turn the layer off for Steam or for the games Steam launches
    /// — only `DISABLE_GAMESCOPE_WSI`, which the script never mentions and which the Vulkan loader
    /// treats as an unconditional force-off, survives. Dropping it would restore the field bug
    /// (a game with sound and input on a black screen) while the host's log still claimed the
    /// layer was disabled, which is what made it so expensive to find. See [`WSI_OFF_ENV`].
    #[test]
    fn the_wsi_opt_out_carries_the_variable_the_session_script_cannot_clobber() {
        assert!(
            WSI_OFF_ENV.contains(&("DISABLE_GAMESCOPE_WSI", "1")),
            "the clobber-proof variable is the whole point of the opt-out"
        );

        // Both spellings reach both launch paths, and neither may lose the other.
        let args = wsi_off_setenv_args();
        let lines = wsi_off_unit_lines();
        for (name, value) in WSI_OFF_ENV {
            assert!(args.contains(&format!("--setenv={name}={value}")), "{name}");
            assert!(
                lines.contains(&format!("Environment={name}={value}\n")),
                "{name}"
            );
        }

        // Trailing newline: the drop-in body appends nothing after this block today, but the bind
        // lines above it rely on the same contract and the order has changed before.
        assert!(lines.ends_with('\n'));
    }
}
