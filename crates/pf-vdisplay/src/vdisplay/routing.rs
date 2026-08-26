//! Gamescope-session routing (plan §W3 — carved out of [`super`]): mode selection
//! ([`pick_gamescope_mode`]), injector-backend selection ([`input_backend_id`]), dedicated-game-session
//! decisions/launch ([`wants_dedicated_game_session`], [`launch_into_gamescope_session`]), and the
//! managed-session restore workers.

use super::*;

/// The RESOLVED gamescope sub-mode for one session, with the payload `GamescopeDisplay::create`
/// needs.
///
/// This is what [`resolve_gamescope_route`] hands back, and it is carried on the backend INSTANCE
/// (`VirtualDisplay::set_gamescope_route`) exactly as `set_launch_command` carries the launch — not
/// through process env. The env knobs used to BE the channel: `resolve_gamescope_route` wrote
/// `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION` and `create` read them back, but the lock was released in
/// between, and the whole GameStream plane plus the mid-session switch watcher re-run the writer —
/// so session B's decision could overwrite session A's before A's `create` ever read it. They
/// remain *operator overrides*, sampled once (see `operator_gamescope`); nothing writes them now.
///
/// Defined on every platform because the host's `SessionContext` carries it beside `compositor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GamescopeRoute {
    /// Host-managed `gamescope-session-plus` / SteamOS session at the client's mode. `client` is
    /// the session flavour (`steam`), from `PUNKTFUNK_GAMESCOPE_SESSION` if the operator set it.
    Managed { client: String },
    /// Attach to an already-running gamescope. `node` is a PipeWire node id, or `auto` to discover
    /// the box's own game-mode session (from `PUNKTFUNK_GAMESCOPE_NODE` if the operator set it).
    Attach { node: String },
    /// Bare-spawn a headless gamescope for this session, nesting its launch command.
    Spawn,
}

/// How a gamescope-backed session is realized — the pure ladder's verdict, before the payload is
/// attached (see [`GamescopeRoute`]).
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

/// The operator's gamescope overrides, sampled ONCE — at first use, and never written back.
///
/// `resolve_gamescope_route` used to both WRITE `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION` (to publish the
/// sub-mode it chose) and READ them as operator overrides. Reading them live therefore fed the
/// ladder its own previous output: the Attach arm set `_NODE=auto`, and `node_env` sits at rung 2 of
/// [`pick_gamescope_mode`] — ABOVE `dedicated_launch` at rung 3 — so one Attach decision latched
/// Attach for the rest of the host's life and silently overrode `game_session=dedicated`. Only rung
/// 1 (`_MANAGED`) could escape, because the Spawn arm that would clear the keys sits below the rung
/// that by then always fired.
///
/// Sampling at first use keeps the override's actual meaning — "the operator set this before we
/// ran". Nothing publishes these keys any more (see [`resolve_gamescope_route`]): the resolved
/// decision travels as a [`GamescopeRoute`] VALUE carried on the backend instance, and every
/// consumer takes it that way — [`launch_is_nested`] by parameter, gamescope's `poolable_now` off
/// `self.route`, `crate::gamescope_hdr_available` by re-resolving the ladder. A change that
/// "restores" the write to serve some reader would restore the latch with it.
#[cfg(target_os = "linux")]
static OPERATOR_GAMESCOPE: std::sync::OnceLock<OperatorGamescope> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct OperatorGamescope {
    managed: bool,
    attach: bool,
    /// The operator's `PUNKTFUNK_GAMESCOPE_NODE` VALUE, if set — the ladder needs its presence and
    /// `resolve_gamescope_route` needs its content to build the route.
    node: Option<String>,
    /// Likewise `PUNKTFUNK_GAMESCOPE_SESSION` — the managed session flavour.
    session: Option<String>,
}

#[cfg(target_os = "linux")]
fn operator_gamescope() -> &'static OperatorGamescope {
    OPERATOR_GAMESCOPE.get_or_init(|| {
        // Explicit-off grammar, NOT a presence test. These two used to be `var_os(..).is_some()`,
        // which read `PUNKTFUNK_GAMESCOPE_ATTACH=0` as ATTACH ON — the exact opposite of what the
        // line says, and of every other knob on this host (`env_on` is shared for that reason).
        // Anyone turning a shipped `=1` off does it the way the rest of the file works, and the
        // rung this feeds outranks a dedicated game session, so a silent inversion here costs the
        // client its own display for the whole stream.
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

/// The injector backend that matches a video backend — the two must not diverge, so this is the one
/// place that decides. `pf_inject::set_backend_id` takes the answer; the caller publishes it
/// alongside [`resolve_gamescope_route`] whenever it routes a session.
///
/// A `&'static str` because pf-vdisplay never depends on pf-inject (see the crate manifest's note),
/// and these four ids are already the `PUNKTFUNK_INPUT_BACKEND` vocabulary the injector parses.
///
/// This used to be an `unsafe { std::env::set_var("PUNKTFUNK_INPUT_BACKEND", ..) }` inside
/// `apply_input_env`, read back by `pf_inject::default_backend` — which runs once per input batch on
/// the injector service thread. A `getenv` on that hot path racing this per-session `setenv` is the
/// `environ` data race, on a live streaming host, reachable by nothing more exotic than a client
/// reconnect (security-review 2026-08-25). Handing back a value removes the write entirely; the
/// operator's own `PUNKTFUNK_INPUT_BACKEND` is still READ by the injector, and is no longer
/// overwritten by us.
pub fn input_backend_id(chosen: Compositor) -> &'static str {
    match chosen {
        Compositor::Gamescope => "gamescope",
        // KWin: org_kde_kwin_fake_input — direct injection, no RemoteDesktop portal / approval
        // dialog (headless, the krdpserver path), authorized by the host's shipped .desktop.
        Compositor::Kwin => "kwin",
        // GNOME has neither fake_input nor the wlr protocols → RemoteDesktop portal via libei.
        Compositor::Mutter => "libei",
        // Hyprland kept `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` (D4) — same wlr
        // injector as sway/river, no code change.
        Compositor::Wlroots | Compositor::Hyprland => "wlr",
    }
}

/// The gamescope sub-mode ladder: **managed** (a host-managed session at the client's mode — tears
/// the TV's autologin down on connect, restored on a debounced idle; only where session-plus/SteamOS
/// actually exists), **attach** (mirror a running gamescope at its own mode; explicit via
/// `PUNKTFUNK_GAMESCOPE_ATTACH`/`PUNKTFUNK_GAMESCOPE_NODE`, or the fallback for a foreign gamescope
/// on an infra-less box), or **bare spawn** (a per-session headless gamescope nesting the session's
/// launch command — the plain-distro default). `PUNKTFUNK_GAMESCOPE_MANAGED` forces managed over all
/// of it.
///
/// Returns the resolved [`GamescopeRoute`] when `chosen` is gamescope — the caller must carry it to
/// the backend instance via `VirtualDisplay::set_gamescope_route`. It is a RETURN VALUE and not an
/// env write precisely because two sessions connecting at once would otherwise clobber each other's
/// decision through the process env.
///
/// Nothing here touches the injector: a caller routing a session pairs this with
/// [`input_backend_id`], while the operator-pinned (`PUNKTFUNK_COMPOSITOR`) path calls this ALONE —
/// it deliberately leaves input routing to the operator's own knob, but still needs a route, or
/// `create` would fall through to a bare spawn on a box pinned to the managed session.
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
        // Sampled inside — `operator_gamescope` takes ENV_LOCK itself, and no caller holds it (the
        // mutex is not reentrant). Nothing on this path writes the env at all any more.
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
        // Nothing is written back to the two knobs: they are the OPERATOR's inputs, sampled once
        // above, and the decision travels out as a value. An earlier revision published it here,
        // which is how one Attach latched Attach for the host's lifetime (the ladder re-read its
        // own output) and how a second session could retarget a first session's `create`.
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

/// Should a game-launching session get a **dedicated** headless gamescope (`game_session=dedicated`
/// policy, `design/gamemode-and-dedicated-sessions.md` B0)? True only when the session carries a
/// launch, the policy selects `dedicated`, AND gamescope is actually available (else it degrades to
/// `auto` honestly). Computed at the handshake and threaded into [`resolve_gamescope_route`] /
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
/// into the session — it would start twice. Takes the session's own resolved
/// [`GamescopeRoute`] (from [`resolve_gamescope_route`]) rather than re-reading process env, so a
/// concurrent session's routing decision cannot change this session's answer.
#[cfg(target_os = "linux")]
pub fn launch_is_nested(compositor: Compositor, route: Option<&GamescopeRoute>) -> bool {
    compositor == Compositor::Gamescope && matches!(route, Some(GamescopeRoute::Spawn))
}

/// Launch `cmd` into the live gamescope session (managed/attach — see
/// [`gamescope::launch_into_session`]). Split out so `library.rs` doesn't reach into the private
/// backend module.
#[cfg(target_os = "linux")]
pub fn launch_into_gamescope_session(cmd: &str) -> Result<std::process::Child> {
    gamescope::launch_into_session(cmd)
}

/// Put the compositor's focus on the streamed head `name`, so a window mapping **now** lands where
/// the client is looking. Split out for `library.rs` like [`launch_into_gamescope_session`].
///
/// Only the two EXTEND backends need (or can act on) this. The streamed head sits *beside* the
/// operator's there, and a new window goes to whatever monitor holds focus — so a session that never
/// claims focus launches every app onto a screen the client cannot see. The other backends are
/// no-ops by construction, not by omission:
///
/// * **KWin / Mutter** — the virtual output is promoted *primary* (`apply_virtual_primary` /
///   `make_virtual_primary`), which is already the compositor-level answer to "where do new windows
///   go".
/// * **gamescope** — the app is a child of the nested compositor, so placement is structural.
/// * **mirror pins** — the streamed head IS a physical head the operator is using; stealing its
///   focus mid-session would be a change they did not ask for.
///
/// `name` is checked against the backend's own minting scheme first, which is what keeps the
/// mirror-pin case out: there `name` is a physical connector, and only a head we created ourselves is
/// ours to focus.
///
/// Returns whether focus was actually asserted, so the caller can log the distinction rather than
/// guess. Best-effort throughout: failing to focus costs window placement, never the session.
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
        // Exhaustive on purpose (no `_` arm): a backend added later must come here and decide,
        // rather than inheriting "no focus" silently — the failure mode is invisible in a log and
        // only shows up as a game on the wrong screen.
        Compositor::Hyprland
        | Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope => false,
    }
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

/// Warn ONCE, at startup, when this box will need the managed gamescope takeover but its user is
/// not in the `punktfunk` group the packaged privilege helper gates on — the one takeover
/// prerequisite that fails silently mid-stream instead of at setup time. Gated so a box that will
/// never attempt a takeover stays quiet; see [`gamescope::preflight_takeover_privilege`] for the
/// exact conditions. Call once at `serve` startup, alongside [`restore_takeover_on_startup`].
#[cfg(target_os = "linux")]
pub fn preflight_takeover_privilege() {
    gamescope::preflight_takeover_privilege();
}

#[cfg(not(target_os = "linux"))]
pub fn preflight_takeover_privilege() {}

/// Why the managed takeover's `punktfunk`-group prerequisite does not apply to this box. Each of
/// these alone makes the group irrelevant, so a box in any of these states must not be nagged —
/// but the reason is kept so a troubleshooting UI can say *which* one, instead of hiding the row
/// and leaving "why isn't this listed?" unanswerable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverInapplicable {
    /// Running as root: the plain system-bus `systemctl` verbs succeed, so the helper — and its
    /// group check — is never reached.
    Root,
    /// No display manager drives this box's logins (getty autologin / an enabled user unit).
    NoDisplayManager,
    /// No `gamescope-session-plus`/SteamOS session infrastructure ⇒ no autologin gaming session to
    /// free ⇒ no takeover.
    NoManagedSession,
    /// A tarball/source/Nix install: neither the packaged helper nor the group exists, and the
    /// hand-written polkit rule from the docs is the route instead.
    NoPackagedHelper,
    /// The user's login name could not be resolved, so no usable `usermod` line could be produced.
    UnknownUser,
    /// The managed takeover is a Linux path.
    NotLinux,
}

/// The takeover's one un-automatable prerequisite, as data.
///
/// Defined on every platform (like [`GamescopeRoute`]) because the host maps it into a wire check
/// regardless of target — off Linux it is always `Inapplicable { why: NotLinux }`. Membership is
/// the **user database's** answer, matching what `pf-dm-helper` itself asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TakeoverVerdict {
    /// A gate excluded this box; `why` names which one.
    Inapplicable { why: TakeoverInapplicable },
    /// The takeover applies here and the user is already a member.
    Ok { user: String, group: &'static str },
    /// The takeover applies here and the user is **not** a member — every takeover will degrade
    /// silently to mirroring the box's own session.
    MissingMembership {
        user: String,
        dm: String,
        helper: &'static str,
        group: &'static str,
    },
}

/// The gated verdict behind [`preflight_takeover_privilege`], for callers that want to render it
/// rather than log it (the host's diagnostics registry). Computing it does not log.
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

/// Tell the takeover that the box switched to `switched_to` mid-stream. A managed takeover
/// runtime-masks the box's own autologin gaming unit so its session supervisor cannot restart it
/// underneath us — but that mask is only sound while our managed session actually holds the box.
/// Once the user has switched the box to a desktop session mid-stream, the mask defends nothing and
/// bars the way back: the distro's session script starts that very unit, so "Return to Gaming Mode"
/// fails instantly and Steam sits on its "Switch to Desktop…" modal until a reboot clears the mask.
/// Call from the mid-stream session watcher on every switch it confirms; no-op when no takeover
/// masked anything, and when the switch does not end the mask's window.
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

    /// The injector backend is a RETURN VALUE the caller hands to `pf_inject::set_backend_id`, never
    /// a `set_var` of `PUNKTFUNK_INPUT_BACKEND` — that write raced `pf_inject::default_backend`'s
    /// `getenv`, which runs once per input batch on the injector service thread
    /// (security-review 2026-08-25). Pinning the whole table because pf-vdisplay cannot depend on
    /// pf-inject: these four ids are one half of a cross-crate contract, and `pf-inject`'s
    /// `every_id_the_video_side_emits_maps_to_a_backend` pins the other.
    #[test]
    fn every_compositor_names_the_injector_backend_that_matches_it() {
        assert_eq!(input_backend_id(Compositor::Gamescope), "gamescope");
        assert_eq!(input_backend_id(Compositor::Kwin), "kwin");
        assert_eq!(input_backend_id(Compositor::Mutter), "libei");
        // Hyprland shares sway's wlr virtual-input protocols (D4) — same injector, on purpose.
        assert_eq!(input_backend_id(Compositor::Wlroots), "wlr");
        assert_eq!(input_backend_id(Compositor::Hyprland), "wlr");
    }

    /// The ladder must not be able to read back its own output. `resolve_gamescope_route`'s Attach arm used
    /// to write `PUNKTFUNK_GAMESCOPE_NODE=auto`, and `node_env` outranks `dedicated_launch` — so
    /// while the override was read live, one Attach latched Attach for the host's lifetime and
    /// silently overrode `game_session=dedicated`. Sampling once is what breaks the loop, and it is
    /// what makes restoring the write a non-event rather than a relapse; this pins that the sample
    /// does not move when the key is written afterwards.
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
