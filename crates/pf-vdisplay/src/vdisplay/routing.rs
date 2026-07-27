//! Gamescope-session routing (plan §W3 — carved out of [`super`]): mode selection
//! ([`pick_gamescope_mode`]), input-env routing ([`apply_input_env`]), dedicated-game-session
//! decisions/launch ([`wants_dedicated_game_session`], [`launch_into_gamescope_session`]), and the
//! managed-session restore workers.

use super::*;

/// How a gamescope-backed session is realized. Chosen per connect by [`pick_gamescope_mode`],
/// written into the env knobs `GamescopeDisplay::create` dispatches on.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamescopeMode {
    /// Host-managed `gamescope-session-plus` / SteamOS session at the client's mode.
    Managed,
    /// Attach to an already-running gamescope (capture + inject, no lifecycle ownership).
    Attach,
    /// Bare-spawn a headless gamescope per session, nesting the session's launch command.
    Spawn,
}

/// Pure sub-mode ladder for gamescope (unit-testable — the env/probe inputs are parameters):
/// explicit `PUNKTFUNK_GAMESCOPE_MANAGED` forces managed; explicit ATTACH/NODE forces attach; an
/// operator-set `PUNKTFUNK_GAMESCOPE_SESSION` keeps managed; otherwise managed only **when the box
/// actually has the session infrastructure** (gamescope-session-plus / SteamOS — the old code
/// defaulted to managed unconditionally and then bailed on a plain distro, killing the session);
/// a foreign (not host-spawned) gamescope on an infra-less box is attached to; and the final
/// default is a per-session bare spawn — the path that nests the client's launch command.
#[cfg(target_os = "linux")]
fn pick_gamescope_mode(
    dedicated_launch: bool,
    force_managed: bool,
    attach_env: bool,
    node_env: bool,
    session_env: bool,
    managed_infra: bool,
    foreign_gamescope: bool,
) -> GamescopeMode {
    if force_managed {
        GamescopeMode::Managed
    } else if attach_env || node_env {
        GamescopeMode::Attach
    } else if dedicated_launch {
        // A dedicated game session always spawns its own headless gamescope at the client's mode,
        // nesting just the game — outranking managed-infra / foreign-attach, but not the explicit
        // operator MANAGED/ATTACH/NODE overrides above (debug/CI). (design/gamemode-and-dedicated-sessions.md §5.3)
        GamescopeMode::Spawn
    } else if session_env || managed_infra {
        GamescopeMode::Managed
    } else if foreign_gamescope {
        GamescopeMode::Attach
    } else {
        GamescopeMode::Spawn
    }
}

/// Route input to match the chosen video backend (they must not diverge), via the highest-priority
/// `PUNKTFUNK_INPUT_BACKEND` knob the injector honors. For gamescope the sub-mode ladder
/// ([`pick_gamescope_mode`]) selects **managed** (a host-managed session at the client's mode —
/// tears the TV's autologin down on connect, restored on a debounced idle; only where
/// session-plus/SteamOS actually exists), **attach** (mirror a running gamescope at its own mode;
/// explicit via `PUNKTFUNK_GAMESCOPE_ATTACH`/`PUNKTFUNK_GAMESCOPE_NODE`, or the fallback for a
/// foreign gamescope on an infra-less box), or **bare spawn** (a per-session headless gamescope
/// nesting the session's launch command — the plain-distro default). `PUNKTFUNK_GAMESCOPE_MANAGED`
/// forces managed over all of it.
#[cfg(target_os = "linux")]
pub fn apply_input_env(chosen: Compositor, dedicated_launch: bool) {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = match chosen {
        Compositor::Gamescope => "gamescope",
        // KWin: org_kde_kwin_fake_input — direct injection, no RemoteDesktop portal / approval
        // dialog (headless, the krdpserver path), authorized by the host's shipped .desktop.
        Compositor::Kwin => "kwin",
        // GNOME has neither fake_input nor the wlr protocols → RemoteDesktop portal via libei.
        Compositor::Mutter => "libei",
        // Hyprland kept `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` (D4) — same wlr
        // injector as sway/river, no code change.
        Compositor::Wlroots | Compositor::Hyprland => "wlr",
    };
    std::env::set_var("PUNKTFUNK_INPUT_BACKEND", backend);
    if chosen == Compositor::Gamescope {
        let mode = pick_gamescope_mode(
            dedicated_launch,
            std::env::var_os("PUNKTFUNK_GAMESCOPE_MANAGED").is_some(),
            std::env::var_os("PUNKTFUNK_GAMESCOPE_ATTACH").is_some(),
            std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_some(),
            std::env::var_os("PUNKTFUNK_GAMESCOPE_SESSION").is_some(),
            gamescope::managed_session_available(),
            gamescope::foreign_gamescope_running(),
        );
        tracing::info!(?mode, "gamescope sub-mode");
        match mode {
            GamescopeMode::Attach => {
                std::env::remove_var("PUNKTFUNK_GAMESCOPE_SESSION");
                if std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_none() {
                    std::env::set_var("PUNKTFUNK_GAMESCOPE_NODE", "auto");
                }
            }
            GamescopeMode::Managed => {
                if std::env::var_os("PUNKTFUNK_GAMESCOPE_SESSION").is_none() {
                    std::env::set_var("PUNKTFUNK_GAMESCOPE_SESSION", "steam");
                }
                std::env::remove_var("PUNKTFUNK_GAMESCOPE_NODE");
            }
            GamescopeMode::Spawn => {
                // Bare spawn: `create` must fall through to the spawn path, so neither knob may
                // linger from an earlier connect's managed/attach selection.
                std::env::remove_var("PUNKTFUNK_GAMESCOPE_SESSION");
                std::env::remove_var("PUNKTFUNK_GAMESCOPE_NODE");
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_input_env(_chosen: Compositor, _dedicated_launch: bool) {}

/// Should a game-launching session get a **dedicated** headless gamescope (`game_session=dedicated`
/// policy, `design/gamemode-and-dedicated-sessions.md` B0)? True only when the session carries a
/// launch, the policy selects `dedicated`, AND gamescope is actually available (else it degrades to
/// `auto` honestly). Computed at the handshake and threaded into [`apply_input_env`] /
/// [`resolve_compositor`] as a value (no new env knob — the `ENV_LOCK` discipline).
pub fn wants_dedicated_game_session(has_launch: bool) -> bool {
    use policy::GameSession;
    if !has_launch || policy::prefs().game_session() != GameSession::Dedicated {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if gamescope::is_available() {
            true
        } else {
            tracing::warn!(
                "game_session=dedicated but gamescope is unavailable — falling back to auto routing"
            );
            false
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false // Windows: a launching session opens into the one desktop (no gamescope)
    }
}

/// Will `vd.create` on this backend NEST the session's launch command itself (gamescope's bare
/// spawn runs it inside the new gamescope)? When true the session must NOT also spawn the command
/// into the session — it would start twice. Read AFTER [`apply_input_env`] resolved the gamescope
/// sub-mode (the env knobs are that resolution's output).
#[cfg(target_os = "linux")]
pub fn launch_is_nested(compositor: Compositor) -> bool {
    compositor == Compositor::Gamescope
        && with_env_lock(|| {
            std::env::var_os("PUNKTFUNK_GAMESCOPE_SESSION").is_none()
                && std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_none()
        })
}

/// Launch `cmd` into the live gamescope session (managed/attach — see
/// [`gamescope::launch_into_session`]). Split out so `library.rs` doesn't reach into the private
/// backend module.
#[cfg(target_os = "linux")]
pub fn launch_into_gamescope_session(cmd: &str) -> Result<std::process::Child> {
    gamescope::launch_into_session(cmd)
}

/// Every nested Xwayland `(DISPLAY, XAUTHORITY)` of the running gamescope session for the XFixes
/// cursor source (remote-desktop-sweep Phase C) — gamescope can run several, and the pointer is on
/// whichever is focused. Empty when no gamescope session is running / it exposes no Xwayland (the
/// host then leaves gamescope cursorless, today's behaviour).
#[cfg(target_os = "linux")]
pub fn gamescope_xwayland_cursor_targets() -> Vec<(String, Option<String>)> {
    gamescope::xwayland_cursor_targets()
}

/// B2: has a **dedicated** gamescope game session's game exited (its `node_id` doesn't reappear within a
/// short window after capture loss)? The dedicated-spawn session ends cleanly on `true` instead of the
/// capture-loss rebuild. Scoped to the session's OWN node so a coexisting gamescope doesn't mask the
/// exit (review #4/#8). Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn dedicated_game_exited(node_id: u32) -> bool {
    gamescope::game_session_exited(node_id)
}

#[cfg(not(target_os = "linux"))]
pub fn dedicated_game_exited(_node_id: u32) -> bool {
    false
}

/// The Steam appid a dedicated launch targets (`steam … steam://rungameid/<appid>`), for the
/// game-exit watcher. `None` for a non-Steam launch (those are covered by the node-death path
/// [`dedicated_game_exited`] — gamescope's nested child IS the game). See
/// [`gamescope::steam_appid_from_launch`].
#[cfg(target_os = "linux")]
pub fn steam_appid_from_launch(cmd: &str) -> Option<u32> {
    gamescope::steam_appid_from_launch(cmd)
}

/// Block until the dedicated Steam game `appid` has started and then exited — returns `true` when the
/// session should end cleanly (APP_EXITED). Returns `false` if `cancel` is set (the session ended for
/// another reason) or the game never started within the startup grace (leave the session up). Runs on
/// the host's per-session watch thread; `cancel` is the session's stop flag. See
/// [`gamescope::wait_for_steam_game_exit`].
#[cfg(target_os = "linux")]
pub fn watch_steam_game_exit(appid: u32, cancel: &std::sync::atomic::AtomicBool) -> bool {
    matches!(
        gamescope::wait_for_steam_game_exit(appid, cancel),
        gamescope::SteamGameWatch::Exited
    )
}

/// Cancel any pending TV-session restore because a client (re)connected (review #3). No-op off Linux.
#[cfg(target_os = "linux")]
pub fn cancel_pending_tv_restore() {
    gamescope::cancel_pending_restore();
}

#[cfg(not(target_os = "linux"))]
pub fn cancel_pending_tv_restore() {}

/// Can the MANAGED gamescope path stand a session up from nothing on this box (SteamOS's
/// `gamescope-session` launcher or Bazzite's `gamescope-session-plus` present)? Lets the connect
/// path route a "no live graphical session" box to the gamescope takeover — which rebuilds the
/// session at the client's mode — instead of failing the connect. Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn managed_session_available() -> bool {
    gamescope::managed_session_available()
}

#[cfg(not(target_os = "linux"))]
pub fn managed_session_available() -> bool {
    false
}

/// Call when a client session ends: if the host-managed gamescope path took over a box's autologin
/// gaming session (stopped its single-instance Steam to stream at the client's mode), **schedule** a
/// debounced restore so the TV returns to gaming mode — unless a client reconnects within the window
/// (which reuses the warm session, avoiding the per-connect gamescope stop/relaunch that leaked GPU
/// context on F44). No-op on other compositors / when nothing was taken. Needs [`start_restore_worker`]
/// running to actually fire.
#[cfg(target_os = "linux")]
pub fn restore_managed_session() {
    gamescope::schedule_restore_tv_session();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_managed_session() {}

/// Start the host-lifetime worker that fires debounced [`restore_managed_session`] restores once a
/// client has been gone long enough. Hold the returned handle for the host's lifetime; dropping it
/// stops the worker. Call once from `serve()`.
#[cfg(target_os = "linux")]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    gamescope::start_restore_worker()
}

#[cfg(not(target_os = "linux"))]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    std::sync::Arc::new(())
}

/// Recover a stranded TV takeover from a crashed previous host instance
/// (`design/gamemode-and-dedicated-sessions.md` A3). Call once at `serve` startup, alongside
/// [`start_restore_worker`]. No-op when no takeover was persisted (a clean start).
#[cfg(target_os = "linux")]
pub fn restore_takeover_on_startup() {
    gamescope::restore_takeover_on_startup();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_on_startup() {}

/// Give the box its own session back **now**, synchronously, because the host is exiting. Blocks
/// (it shells out to `systemctl`), so call it off the async runtime. Call from the host's shutdown
/// path — a takeover that outlives the host leaves the box with no display manager and nobody left
/// to restart it. No-op when nothing was taken over.
#[cfg(target_os = "linux")]
pub fn restore_takeover_now() {
    gamescope::restore_takeover_now();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_now() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn gamescope_mode_ladder() {
        use GamescopeMode::*;
        let pick = pick_gamescope_mode;
        // (dedicated_launch, force_managed, attach_env, node_env, session_env, managed_infra, foreign_gamescope)
        // Plain distro, nothing running: bare spawn — the path that nests the launch command.
        assert_eq!(pick(false, false, false, false, false, false, false), Spawn);
        // Bazzite/SteamOS (session infra present): managed, as validated live.
        assert_eq!(
            pick(false, false, false, false, false, true, false),
            Managed
        );
        assert_eq!(pick(false, false, false, false, false, true, true), Managed);
        // Foreign gamescope on an infra-less box: attach and mirror it.
        assert_eq!(pick(false, false, false, false, false, false, true), Attach);
        // Operator-set PUNKTFUNK_GAMESCOPE_SESSION keeps managed even without detected infra.
        assert_eq!(
            pick(false, false, false, false, true, false, false),
            Managed
        );
        // Explicit attach/node wins over infra…
        assert_eq!(pick(false, false, true, false, false, true, false), Attach);
        assert_eq!(pick(false, false, false, true, true, true, false), Attach);
        // …and force-managed wins over everything.
        assert_eq!(pick(false, true, true, true, false, false, false), Managed);
        // A dedicated launch forces Spawn, outranking managed-infra + foreign-attach…
        assert_eq!(pick(true, false, false, false, false, true, true), Spawn);
        // …but the explicit operator overrides still win over dedicated.
        assert_eq!(pick(true, true, false, false, false, true, false), Managed);
        assert_eq!(pick(true, false, true, false, false, false, false), Attach);
        assert_eq!(pick(true, false, false, true, false, false, false), Attach);
    }
}
