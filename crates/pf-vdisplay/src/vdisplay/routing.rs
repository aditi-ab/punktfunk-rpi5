//! Gamescope sub-mode, injector id, dedicated-session, and TV-restore routing.
//!
//! Decisions travel as [`GamescopeRoute`] values on the backend instance — never
//! through process env, or a concurrent session retargets this one. Operator
//! knobs (`PUNKTFUNK_GAMESCOPE_*`) are sampled once. Pin: tests below.
//! Design: `design/gamemode-and-dedicated-sessions.md`.

use super::*;

/// Resolved gamescope sub-mode plus the payload `GamescopeDisplay::create` needs.
///
/// Carried on the backend instance (`VirtualDisplay::set_gamescope_route`), never
/// through process env — two sessions connecting at once would otherwise clobber
/// each other. Operator knobs stay operator overrides, sampled once
/// (`operator_gamescope`); nothing writes them.
///
/// Defined on every platform: the host's `SessionContext` carries it beside
/// `compositor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GamescopeRoute {
    /// Host-managed session-plus / SteamOS at the client's mode. `client` is the
    /// flavour (`steam`), from `PUNKTFUNK_GAMESCOPE_SESSION` if set.
    Managed { client: String },
    /// Capture an already-running gamescope. `node` is a PipeWire id, or `auto` to
    /// discover the box's own session (`PUNKTFUNK_GAMESCOPE_NODE` if set).
    Attach { node: String },
    /// Bare-spawn a headless gamescope, nesting this session's launch command.
    Spawn,
}

/// Ladder verdict before the [`GamescopeRoute`] payload is attached.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamescopeMode {
    Managed,
    /// Capture + inject; no lifecycle ownership.
    Attach,
    Spawn,
}

/// Pure ladder. Inputs are parameters so tests can pin it.
///
/// Managed only when session-plus/SteamOS is actually on the box — an
/// unconditional managed default kills the session on a plain distro.
/// Dedicated spawn is a body rung, below the operator MANAGED/ATTACH/NODE
/// overrides.
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
        // Dedicated spawn outranks managed-infra / foreign-attach, not the
        // operator MANAGED/ATTACH/NODE overrides above (debug/CI).
        GamescopeMode::Spawn
    } else if session_env || managed_infra {
        GamescopeMode::Managed
    } else if foreign_gamescope {
        GamescopeMode::Attach
    } else {
        GamescopeMode::Spawn
    }
}

/// Operator gamescope knobs, sampled once at first use and never written back.
///
/// `node_env` sits above `dedicated_launch` in [`pick_gamescope_mode`]. A live
/// re-read of a published `_NODE=auto` would latch Attach for the host's life
/// and override `game_session=dedicated`. Sampling once means "the operator
/// set this before we ran".
///
/// Pin: `operator_overrides_do_not_see_our_own_writes`.
#[cfg(target_os = "linux")]
static OPERATOR_GAMESCOPE: std::sync::OnceLock<OperatorGamescope> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct OperatorGamescope {
    managed: bool,
    attach: bool,
    /// `PUNKTFUNK_GAMESCOPE_NODE` if set. The ladder needs presence; the route needs the value.
    node: Option<String>,
    /// `PUNKTFUNK_GAMESCOPE_SESSION` — the managed session flavour.
    session: Option<String>,
}

#[cfg(target_os = "linux")]
fn operator_gamescope() -> &'static OperatorGamescope {
    OPERATOR_GAMESCOPE.get_or_init(|| {
        // Presence is not the grammar: `PUNKTFUNK_GAMESCOPE_ATTACH=0` must be off,
        // matching every other host knob (`env_on`). This rung outranks dedicated
        // spawn, so a silent inversion costs the client its own display.
        let ov = with_env_lock(|| OperatorGamescope {
            managed: pf_host_config::env_on("PUNKTFUNK_GAMESCOPE_MANAGED").unwrap_or(false),
            attach: pf_host_config::env_on("PUNKTFUNK_GAMESCOPE_ATTACH").unwrap_or(false),
            node: std::env::var("PUNKTFUNK_GAMESCOPE_NODE")
                .ok()
                .filter(|v| !v.is_empty()),
            session: std::env::var("PUNKTFUNK_GAMESCOPE_SESSION")
                .ok()
                .filter(|v| !v.is_empty()),
        });
        if ov.managed || ov.attach || ov.node.is_some() || ov.session.is_some() {
            tracing::info!(
                ?ov,
                "gamescope: operator sub-mode overrides sampled from the environment"
            );
        }
        ov
    })
}

/// Injector id that matches this video backend — the one place the pair is
/// chosen, so they cannot diverge. Caller publishes it via
/// `pf_inject::set_backend_id` next to [`resolve_gamescope_route`].
///
/// `&'static str` because this crate must not depend on pf-inject; the four
/// ids are the injector's `PUNKTFUNK_INPUT_BACKEND` vocabulary.
///
/// A return value, not a `setenv`: `pf_inject::default_backend` `getenv`s on
/// the injector thread, and a per-session write races `environ`. The operator
/// knob is still read there; this function does not write it.
///
/// Pin: `every_compositor_names_the_injector_backend_that_matches_it`;
/// `pf-inject`'s matching table is the other half.
pub fn input_backend_id(chosen: Compositor) -> &'static str {
    match chosen {
        Compositor::Gamescope => "gamescope",
        // org_kde_kwin_fake_input — no RemoteDesktop portal. Headless krdpserver
        // path; the shipped .desktop authorizes it.
        Compositor::Kwin => "kwin",
        // Neither fake_input nor wlr virtual-input → RemoteDesktop portal via libei.
        Compositor::Mutter => "libei",
        // Hyprland still speaks `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`
        // — same wlr injector as sway/river.
        Compositor::Wlroots | Compositor::Hyprland => "wlr",
    }
}

/// Resolve the gamescope route when `chosen` is gamescope.
///
/// Return value, not an env write: two sessions connecting at once would
/// otherwise clobber each other. Caller must put it on the backend via
/// `VirtualDisplay::set_gamescope_route`.
///
/// The operator-pinned (`PUNKTFUNK_COMPOSITOR`) path calls this alone — it
/// leaves input routing to the operator's knob, but `create` still needs a
/// route or it falls through to a bare spawn on a box pinned to managed.
#[cfg(target_os = "linux")]
#[must_use = "the resolved gamescope route must reach the backend instance (set_gamescope_route)"]
pub fn resolve_gamescope_route(
    chosen: Compositor,
    dedicated_launch: bool,
) -> Option<GamescopeRoute> {
    if chosen != Compositor::Gamescope {
        return None;
    }
    {
        // `operator_gamescope` takes ENV_LOCK itself; the mutex is not reentrant.
        // Nothing on this path writes the env.
        let ov = operator_gamescope();
        let mode = pick_gamescope_mode(
            dedicated_launch,
            ov.managed,
            ov.attach,
            ov.node.is_some(),
            ov.session.is_some(),
            gamescope::managed_session_available(),
            gamescope::foreign_gamescope_running(),
        );
        tracing::info!(?mode, "gamescope sub-mode");
        // Operator knobs stay inputs. Publishing `_NODE=auto` here latches Attach
        // on the next sample (`node_env` outranks dedicated).
        Some(match mode {
            GamescopeMode::Attach => GamescopeRoute::Attach {
                node: ov.node.clone().unwrap_or_else(|| "auto".to_string()),
            },
            GamescopeMode::Managed => GamescopeRoute::Managed {
                client: ov.session.clone().unwrap_or_else(|| "steam".to_string()),
            },
            GamescopeMode::Spawn => GamescopeRoute::Spawn,
        })
    }
}

#[cfg(not(target_os = "linux"))]
pub fn resolve_gamescope_route(
    _chosen: Compositor,
    _dedicated_launch: bool,
) -> Option<GamescopeRoute> {
    None
}

/// Dedicated headless gamescope for this launch (`game_session=dedicated`).
///
/// True only with a launch, dedicated policy, and gamescope actually
/// available — else it degrades to `auto`. Handshake value, threaded into
/// [`resolve_gamescope_route`] / [`resolve_compositor`]; no new env knob.
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
        false // one desktop; no gamescope
    }
}

/// True when `vd.create` nests the launch (gamescope bare-spawn). The session
/// must not also spawn it — the game would start twice.
///
/// Takes this session's [`GamescopeRoute`], not process env, so a concurrent
/// session cannot change the answer.
#[cfg(target_os = "linux")]
pub fn launch_is_nested(compositor: Compositor, route: Option<&GamescopeRoute>) -> bool {
    compositor == Compositor::Gamescope && matches!(route, Some(GamescopeRoute::Spawn))
}

/// Launch `cmd` into a live managed/attach session. Spawn nests instead
/// ([`launch_is_nested`]).
#[cfg(target_os = "linux")]
pub fn launch_into_gamescope_session(cmd: &str) -> Result<std::process::Child> {
    gamescope::launch_into_session(cmd)
}

/// Put compositor focus on streamed head `name` so a window mapping now lands
/// where the client is looking.
///
/// Only EXTEND backends act: the streamed head sits beside the operator's, and
/// a new window goes to whichever monitor holds focus. Other backends are
/// no-ops by construction — KWin/Mutter promote the virtual output primary;
/// gamescope nests the app; a mirror pin *is* a physical head, and stealing
/// its focus mid-session is unasked.
///
/// `name` is checked against the backend's mint first: a physical connector
/// is not ours to focus.
///
/// Returns whether focus was asserted (for logs). Best-effort: a miss costs
/// placement, never the session.
#[cfg(target_os = "linux")]
pub fn focus_streamed_output(compositor: Compositor, name: &str) -> bool {
    match compositor {
        Compositor::Hyprland if hyprland::is_managed_output(name) => {
            hyprland::focus_output(name);
            true
        }
        Compositor::Wlroots if wlroots::is_managed_output(name) => {
            wlroots::focus_output(name);
            true
        }
        // No `_` arm: a new backend must decide here. Silent "no focus" only
        // shows up as a game on the wrong screen.
        Compositor::Hyprland
        | Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope => false,
    }
}

/// Nested Xwayland `(DISPLAY, XAUTHORITY)` pairs for the XFixes cursor source.
/// Gamescope can run several; the pointer is on the focused one. Empty when
/// none are exposed — the host then leaves gamescope cursorless.
#[cfg(target_os = "linux")]
pub fn gamescope_xwayland_cursor_targets() -> Vec<(String, Option<String>)> {
    gamescope::xwayland_cursor_targets()
}

/// Dedicated game gone: `node_id` does not reappear shortly after capture loss.
/// `true` ends the session cleanly instead of a capture-loss rebuild. Scoped
/// to this session's node so a coexisting gamescope cannot mask the exit.
/// Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn dedicated_game_exited(node_id: u32) -> bool {
    gamescope::game_session_exited(node_id)
}

#[cfg(not(target_os = "linux"))]
pub fn dedicated_game_exited(_node_id: u32) -> bool {
    false
}

/// Steam appid a dedicated launch targets, for the exit watcher. `None` for a
/// non-Steam launch — those use [`dedicated_game_exited`]: gamescope's nested
/// child *is* the game.
#[cfg(target_os = "linux")]
pub fn steam_appid_from_launch(cmd: &str) -> Option<u32> {
    gamescope::steam_appid_from_launch(cmd)
}

/// Block until Steam `appid` has started and then exited.
///
/// `true` → end the session cleanly. `false` if `cancel` is set or the game
/// never started within the startup grace (leave the session up). Runs on
/// the per-session watch thread; `cancel` is the session stop flag.
#[cfg(target_os = "linux")]
pub fn watch_steam_game_exit(appid: u32, cancel: &std::sync::atomic::AtomicBool) -> bool {
    matches!(
        gamescope::wait_for_steam_game_exit(appid, cancel),
        gamescope::SteamGameWatch::Exited
    )
}

/// Cancel a pending TV-session restore: a client (re)connected. No-op off Linux.
#[cfg(target_os = "linux")]
pub fn cancel_pending_tv_restore() {
    gamescope::cancel_pending_restore();
}

#[cfg(not(target_os = "linux"))]
pub fn cancel_pending_tv_restore() {}

/// Whether managed gamescope can stand a session up from nothing (SteamOS
/// `gamescope-session` or Bazzite `gamescope-session-plus`). Lets connect
/// route a box with no live graphical session to takeover instead of failing.
/// Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn managed_session_available() -> bool {
    gamescope::managed_session_available()
}

#[cfg(not(target_os = "linux"))]
pub fn managed_session_available() -> bool {
    false
}

/// Schedule a debounced TV restore after a managed takeover session ends.
/// A reconnect inside the window reuses the warm session (no per-connect
/// gamescope stop/relaunch). No-op when nothing was taken. Needs
/// [`start_restore_worker`] running to fire.
#[cfg(target_os = "linux")]
pub fn restore_managed_session() {
    gamescope::schedule_restore_tv_session();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_managed_session() {}

/// Host-lifetime worker for debounced [`restore_managed_session`]. Hold the
/// returned handle for the host's life; dropping it stops the worker. Call
/// once from `serve()`.
#[cfg(target_os = "linux")]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    gamescope::start_restore_worker()
}

#[cfg(not(target_os = "linux"))]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    std::sync::Arc::new(())
}

/// Recover a stranded TV takeover from a crashed previous host. Call once at
/// `serve` startup, beside [`start_restore_worker`]. No-op when nothing was
/// persisted.
#[cfg(target_os = "linux")]
pub fn restore_takeover_on_startup() {
    gamescope::restore_takeover_on_startup();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_on_startup() {}

/// Warn once at startup when this box will need managed takeover but the user
/// is not in the `punktfunk` group the helper gates on — the one prerequisite
/// that fails silently mid-stream. Gated so a box that never attempts takeover
/// stays quiet. Call once at `serve` startup.
#[cfg(target_os = "linux")]
pub fn preflight_takeover_privilege() {
    gamescope::preflight_takeover_privilege();
}

#[cfg(not(target_os = "linux"))]
pub fn preflight_takeover_privilege() {}

/// Why the `punktfunk`-group prerequisite does not apply. Each variant alone
/// makes the group irrelevant — do not nag — but keep `why` so a diagnostics
/// UI can name which gate, not hide the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverInapplicable {
    /// Root: plain system-bus `systemctl` succeeds, so the helper is never reached.
    Root,
    /// No DM drives logins (getty autologin / an enabled user unit).
    NoDisplayManager,
    /// No session-plus/SteamOS infra ⇒ no autologin gaming session to free.
    NoManagedSession,
    /// Tarball/source/Nix: no packaged helper or group; the docs' polkit rule is the route.
    NoPackagedHelper,
    /// Login name unresolved, so no usable `usermod` line.
    UnknownUser,
    /// Managed takeover is a Linux path.
    NotLinux,
}

/// The takeover's one un-automatable prerequisite, as data.
///
/// Defined on every platform (the host maps it to a wire check). Off Linux
/// it is always `Inapplicable { why: NotLinux }`. Membership is the user
/// database's answer, matching `pf-dm-helper`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TakeoverVerdict {
    Inapplicable { why: TakeoverInapplicable },
    Ok { user: String, group: &'static str },
    /// Takeover applies and the user is **not** a member — every takeover
    /// degrades silently to mirroring the box's own session.
    MissingMembership {
        user: String,
        dm: String,
        helper: &'static str,
        group: &'static str,
    },
}

/// Gated verdict behind [`preflight_takeover_privilege`], for a diagnostics
/// UI. Computing it does not log.
#[cfg(target_os = "linux")]
pub fn takeover_privilege_verdict() -> TakeoverVerdict {
    gamescope::takeover_privilege_verdict()
}

#[cfg(not(target_os = "linux"))]
pub fn takeover_privilege_verdict() -> TakeoverVerdict {
    TakeoverVerdict::Inapplicable {
        why: TakeoverInapplicable::NotLinux,
    }
}

/// Restore the box's own session now — host is exiting. Blocks on `systemctl`,
/// so call off the async runtime. A takeover that outlives the host leaves
/// the box with no display manager and nobody to restart it. No-op when
/// nothing was taken.
#[cfg(target_os = "linux")]
pub fn restore_takeover_now() {
    gamescope::restore_takeover_now();
    // xdph picker is the other hold a host can outlive. Not restored per cast:
    // rewriting the config restarts xdph and orphans the portal's cached D-Bus
    // (`hyprland::StopGuard::drop`) — a stream that never delivers a buffer.
    // Shutdown: no live cast, restart is free. No-op if we never took it.
    hyprland::restore_picker_on_shutdown();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_now() {}

/// Mid-stream switch: drop the autologin-unit mask if it no longer applies.
///
/// Takeover masks the box's autologin gaming unit so the supervisor cannot
/// restart it under us. That is only sound while our managed session holds
/// the box. After a switch to desktop the mask bars "Return to Gaming Mode"
/// until reboot. Call from the session watcher on every confirmed switch;
/// no-op when nothing was masked or the switch does not end the mask window.
#[cfg(target_os = "linux")]
pub fn release_autologin_mask(switched_to: crate::ActiveKind) {
    gamescope::release_autologin_mask(switched_to);
}

#[cfg(not(target_os = "linux"))]
pub fn release_autologin_mask(_switched_to: crate::ActiveKind) {}

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
        // Session infra present: managed.
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
        // Dedicated launch forces Spawn, outranking managed-infra + foreign-attach…
        assert_eq!(pick(true, false, false, false, false, true, true), Spawn);
        // …but the explicit operator overrides still win over dedicated.
        assert_eq!(pick(true, true, false, false, false, true, false), Managed);
        assert_eq!(pick(true, false, true, false, false, false, false), Attach);
        assert_eq!(pick(true, false, false, true, false, false, false), Attach);
    }

    /// Injector id is a return value, never a `PUNKTFUNK_INPUT_BACKEND` `set_var`
    /// (that raced `pf_inject::default_backend`'s `getenv` on the injector
    /// thread). Whole table: this crate cannot depend on pf-inject; the four
    /// ids are one half of the contract (`every_id_the_video_side_emits_maps_to_a_backend`
    /// is the other).
    #[test]
    fn every_compositor_names_the_injector_backend_that_matches_it() {
        assert_eq!(input_backend_id(Compositor::Gamescope), "gamescope");
        assert_eq!(input_backend_id(Compositor::Kwin), "kwin");
        assert_eq!(input_backend_id(Compositor::Mutter), "libei");
        // Hyprland shares sway's wlr virtual-input protocols — same injector on purpose.
        assert_eq!(input_backend_id(Compositor::Wlroots), "wlr");
        assert_eq!(input_backend_id(Compositor::Hyprland), "wlr");
    }

    /// Sample must not move when `PUNKTFUNK_GAMESCOPE_NODE` is written afterwards.
    /// `node_env` outranks `dedicated_launch`; a live re-read of a published
    /// `=auto` would latch Attach for the host's life.
    #[test]
    #[cfg(target_os = "linux")]
    fn operator_overrides_do_not_see_our_own_writes() {
        use super::operator_gamescope;
        let first = operator_gamescope();
        let restore = crate::with_env_lock(|| std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE"));
        // SAFETY: both mutations run under `with_env_lock` — the crate's env-writer
        // serialization (ENV_LOCK, lib.rs); the readers under test sample once at startup.
        crate::with_env_lock(|| unsafe { std::env::set_var("PUNKTFUNK_GAMESCOPE_NODE", "auto") });
        let second = operator_gamescope();
        crate::with_env_lock(|| match &restore {
            // SAFETY: as above — the restore also runs under the same env-writer lock.
            Some(v) => unsafe { std::env::set_var("PUNKTFUNK_GAMESCOPE_NODE", v) },
            // SAFETY: as above.
            None => unsafe { std::env::remove_var("PUNKTFUNK_GAMESCOPE_NODE") },
        });
        assert_eq!(
            second.node, first.node,
            "writing the key we publish must not turn into an operator override"
        );
    }
}
