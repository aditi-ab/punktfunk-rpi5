//! Virtual display orchestration (plan §6 / §W6) — the project's differentiator.
//!
//! A [`VirtualDisplay`] creates a *client-sized* output on demand, rendered natively and
//! headless (no scaling), to be captured and streamed, then torn down on disconnect. There is
//! no cross-compositor Wayland protocol for this, so each compositor has its own backend behind
//! this trait:
//!
//! * **KWin** — privileged `zkde_screencast_unstable_v1::stream_virtual_output` ([`kwin`]).
//! * **wlroots/Sway** — `swaymsg create_output` + `output mode --custom` ([`wlroots`]).
//! * **Mutter/GNOME** — D-Bus `RemoteDesktop` + `ScreenCast.RecordVirtual` ([`mutter`]).
//! * **Hyprland** — `hyprctl output create headless` + the xdg-desktop-portal-hyprland ScreenCast
//!   portal. Its own backend, not a wlroots dialect (`design/hyprland-support.md` D1).
//! * **gamescope** — three sub-modes behind one backend ([`GamescopeRoute`]): bare
//!   **spawn** of a nested headless session, host-**managed** `gamescope-session-plus`/SteamOS
//!   takeover, and **attach** to a session somebody else started. By far the largest backend here,
//!   because it owns session lifecycle rather than just minting an output.
//! * **monitor mirror** — no virtual display at all: stream a PHYSICAL head the compositor already
//!   has (the `PUNKTFUNK_CAPTURE_MONITOR` pin), reporting [`DisplayOwnership::External`] so none of
//!   the lifecycle policy is applied to someone else's screen.
//! * **Windows pf-vdisplay** — the all-Rust IddCx driver + its `manager`, the sole Windows backend.
//!
//! No list of file sizes here: it rots. The rule instead — the Linux backends plus the Windows
//! manager are the bulk of this crate, and the platform-neutral half (`policy`, `registry`,
//! `lifecycle`, `layout`, `identity`, `admission`, `monitors`, `session`, `routing`, `proc`,
//! `portal_config`) is the minority that every platform's CI actually compiles and tests.
//!
//! [`VirtualDisplay::create`] returns a [`VirtualOutput`]: the PipeWire node to capture plus an
//! owned keepalive whose `Drop` releases the output (RAII — no explicit `destroy`). Capture
//! consumes the node via the host `capture::capture_virtual_output`.

// `dead_code` is ENFORCED on Linux, where the clear majority of this crate lives — every compositor
// backend under `vdisplay/linux/` plus everything only they consume, which is roughly half the crate
// on its own and the half that carries the session-lifecycle risk. Off elsewhere for one structural
// reason: `proc`, `session`, `routing`, `monitors` and `lifecycle` are declared unconditionally but
// exist to serve the Linux backends, so on Windows/macOS most of their surface is legitimately
// unreferenced. Note what that waives: the Windows backend (`vdisplay/windows/`, itself thousands of
// lines) gets NO dead-code enforcement, so an orphaned Windows path has to be found by review.
// Scoping it this way rather than crate-wide still keeps the platform that owns most of the code
// honest. (Was a bare crate-wide allow whose "scaffold, defined ahead of the target that uses them"
// rationale had stopped being true.)
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::Result;
pub use punktfunk_core::Mode;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

/// A display-lifecycle event the registry emits when it creates or releases a managed virtual
/// display. The host wires [`DISPLAY_EVENT_SINK`] to translate these into its SSE event bus
/// (`crate::events` in the orchestrator), so this crate emits lifecycle signals without owning the
/// bus type — the one reach into the orchestrator's event module, inverted to a leaf hook (plan §W6).
pub enum DisplayEvent {
    /// A virtual display was created on `backend` at `width`×`height`@`refresh_hz`.
    Created {
        backend: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    /// `count` managed displays were released.
    Released { count: u32 },
}

/// The host-registered sink that forwards [`DisplayEvent`]s to the SSE bus. Set once at startup; a
/// display event before it is set is silently dropped (no subscriber yet).
pub static DISPLAY_EVENT_SINK: std::sync::OnceLock<Box<dyn Fn(DisplayEvent) + Send + Sync>> =
    std::sync::OnceLock::new();

/// Emit a [`DisplayEvent`] to the host sink, if registered.
pub(crate) fn emit_display_event(ev: DisplayEvent) {
    if let Some(sink) = DISPLAY_EVENT_SINK.get() {
        sink(ev);
    }
}

/// The virtual-display backend contract — [`DisplayOwnership`], [`VirtualOutput`], and the
/// [`VirtualDisplay`] trait (plan §W3). Re-exported so `crate::VirtualDisplay` etc. stay
/// stable for the ~30 external call sites.
#[path = "vdisplay/backend.rs"]
pub(crate) mod backend;
pub use backend::{DisplayOwnership, VirtualDisplay, VirtualOutput};
/// The NEGOTIATED ScreenCast cursor mode of a portal-backed output, reported per session by
/// [`VirtualDisplay::last_portal_cursor_mode`]. (The module itself stays private — the ladder that
/// picks the mode is this crate's business; the verdict is the caller's.)
pub use portal_cursor::Mode as PortalCursorMode;

/// Time-bounded child-process helpers — every compositor query shells out, and an unbounded one
/// can wedge the calling (session) thread forever.
#[path = "vdisplay/proc.rs"]
pub(crate) mod proc;

/// Live-session detection + session-epoch + env retargeting (plan §W3).
#[path = "vdisplay/session.rs"]
pub(crate) mod session;
pub use session::{
    apply_session_env, compositor_for_kind, detect_active_session, observe_session_instance,
    settle_desktop_portal, ActiveKind, ActiveSession, SessionEnv,
};
#[cfg(target_os = "linux")]
pub use session::{session_epoch, try_recover_session};

/// Gamescope-session routing (plan §W3).
#[path = "vdisplay/routing.rs"]
pub(crate) mod routing;
pub use routing::{
    apply_input_env, managed_session_available, preflight_takeover_privilege,
    release_autologin_mask, resolve_gamescope_route, restore_managed_session, restore_takeover_now,
    restore_takeover_on_startup, start_restore_worker, takeover_privilege_verdict,
    wants_dedicated_game_session, GamescopeRoute, TakeoverInapplicable, TakeoverVerdict,
};
#[cfg(target_os = "linux")]
pub use routing::{
    cancel_pending_tv_restore, dedicated_game_exited, gamescope_xwayland_cursor_targets,
    launch_into_gamescope_session, launch_is_nested, steam_appid_from_launch,
    watch_steam_game_exit,
};

/// Compositors punktfunk knows how to drive (plan §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    /// KWin / Plasma 6 — `zkde_screencast` virtual output.
    Kwin,
    /// wlroots proper (Sway / River) — headless `swaymsg create_output`.
    Wlroots,
    /// Mutter / GNOME — headless backend + Mutter DBus `RecordVirtual`.
    Mutter,
    /// gamescope — spawned headless at the client's size/refresh; capture its PipeWire node.
    Gamescope,
    /// Hyprland — headless `hyprctl output create` + the xdg-desktop-portal-hyprland (xdph)
    /// ScreenCast portal. A distinct backend from [`Wlroots`](Compositor::Wlroots): Hyprland
    /// dropped wlroots in v0.42 but kept the client-facing wlr protocols, so it shares the wlr
    /// virtual-input path yet needs its own IPC (`hyprctl`) and portal (xdph) — see
    /// `design/hyprland-support.md`.
    Hyprland,
}

impl Compositor {
    /// Stable lowercase id used on the wire / management API (matches
    /// [`punktfunk_core::CompositorPref::as_str`]).
    pub fn id(self) -> &'static str {
        match self {
            Compositor::Kwin => "kwin",
            Compositor::Wlroots => "wlroots",
            Compositor::Mutter => "mutter",
            Compositor::Gamescope => "gamescope",
            Compositor::Hyprland => "hyprland",
        }
    }

    /// Does this backend need a compositor that is ALREADY RUNNING for this uid?
    ///
    /// Every desktop backend attaches to a live session — it asks Mutter/KWin/sway/Hyprland to mint
    /// a virtual output over their IPC, so with nothing running there is no one to ask and `create`
    /// can only fail (on GNOME: `RemoteDesktop.CreateSession:
    /// org.freedesktop.DBus.Error.ServiceUnknown`). [`Compositor::Gamescope`] is the exception: it
    /// stands its own session up from nothing (bare headless spawn / managed takeover), which is
    /// exactly why a headless box pins to it.
    ///
    /// Callers use this to tell "the session is up" from "the session is a corpse" BEFORE marching a
    /// client into a doomed bring-up — the state a compositor crash leaves behind (gnome-shell
    /// SIGSEGV → GDM greeter, whose auto-login is once-per-boot, so it never returns on its own).
    pub fn needs_live_session(self) -> bool {
        !matches!(self, Compositor::Gamescope)
    }

    /// Human label for UIs.
    pub fn label(self) -> &'static str {
        match self {
            Compositor::Kwin => "KWin / KDE Plasma",
            Compositor::Wlroots => "wlroots (Sway / River)",
            Compositor::Mutter => "Mutter / GNOME",
            Compositor::Gamescope => "gamescope",
            Compositor::Hyprland => "Hyprland",
        }
    }

    /// The protocol [`punktfunk_core::CompositorPref`] naming this backend.
    pub fn as_pref(self) -> punktfunk_core::CompositorPref {
        use punktfunk_core::CompositorPref as P;
        match self {
            Compositor::Kwin => P::Kwin,
            Compositor::Wlroots => P::Wlroots,
            Compositor::Mutter => P::Mutter,
            Compositor::Gamescope => P::Gamescope,
            // D2: no distinct wire byte for Hyprland — it shares the wlroots-family `Wlroots` pref.
            // A client asking for `wlroots`/`hyprland` gets whichever of the two is the live session
            // (`pick_compositor` (host `native`) resolves the family).
            Compositor::Hyprland => P::Wlroots,
        }
    }

    /// The concrete backend a [`punktfunk_core::CompositorPref`] names, or `None` for `Auto`.
    pub fn from_pref(p: punktfunk_core::CompositorPref) -> Option<Compositor> {
        use punktfunk_core::CompositorPref as P;
        Some(match p {
            P::Auto => return None,
            P::Kwin => Compositor::Kwin,
            P::Wlroots => Compositor::Wlroots,
            P::Mutter => Compositor::Mutter,
            P::Gamescope => Compositor::Gamescope,
        })
    }

    /// Every backend, in a stable display order (for enumeration / UIs).
    pub fn all() -> [Compositor; 5] {
        [
            Compositor::Kwin,
            Compositor::Gamescope,
            Compositor::Mutter,
            Compositor::Wlroots,
            Compositor::Hyprland,
        ]
    }
}

/// The compositor backends usable on this host *right now*: gamescope wherever its binary is
/// installed (it spawns a nested session — independent of the running desktop), plus the live
/// session's own compositor (KWin / Mutter / wlroots / Hyprland) when the host runs inside it.
/// Side-effect-free, but **not cheap, and not memoized**: every call re-walks `/proc`
/// ([`detect_active_session`]), and each backend probe that the live/pinned short-circuit below does
/// not exempt does real work — `gamescope::is_available` FORKS `gamescope --version`,
/// `kwin::is_available` does a Wayland registry roundtrip, `wlroots`/`hyprland` read a socket path
/// and `mutter` a D-Bus name. So a console polling `/host/compositors` on a KDE box still forks a
/// gamescope per poll, on a thread the caller must therefore not assume is cheap to block (mgmt
/// calls it inline on the async runtime). Callers wanting a hot path should cache the answer;
/// treating this as free is what the "cheap, safe per management request" claim this doc used to
/// make invited. A concrete client preference is validated against this set before it's honored
/// (see the punktfunk/1 handshake's resolution).
///
/// The **live session is the primary signal**, ahead of each backend's own probe. Those probes read
/// the process env (`XDG_CURRENT_DESKTOP` for Mutter, `WAYLAND_DISPLAY` for KWin's registry
/// handshake, `SWAYSOCK` for sway) — env a host started *outside* the session (a `systemd --user`
/// unit, a TTY, ssh) never inherited. It is only retargeted at the live session on the connect path
/// ([`apply_session_env`]), so enumerating before the first client connect reported "unavailable"
/// for the very desktop the operator was sitting in — while [`detect`], which scans `/proc`, marked
/// that same backend the default. The management API showed both badges on one row, and the answer
/// flipped depending on whether anyone had connected yet. Basing both on the same `/proc` scan makes
/// the two agree, and makes the answer independent of how the host was launched.
pub fn available() -> Vec<Compositor> {
    #[cfg(target_os = "linux")]
    {
        let live = compositor_for_kind(detect_active_session().kind);
        // An explicit operator pin counts too: it's what `detect` returns as the default and what
        // the host will actually drive, so listing it "unavailable" was the same contradiction.
        let pinned = pf_host_config::config()
            .compositor
            .as_deref()
            .and_then(compositor_from_pin);
        Compositor::all()
            .into_iter()
            .filter(|&c| {
                // Running (or pinned) ⇒ usable, without consulting the env-reading probe. KWin is
                // the one backend whose probe checks a real capability beyond "is it up" (the
                // privileged `zkde_screencast` grant); a live-but-ungranted KWin now surfaces as
                // available and fails at create with that probe's precise message, which beats
                // "no usable compositor" on a box that is visibly running KDE.
                live == Some(c)
                    || pinned == Some(c)
                    || match c {
                        Compositor::Kwin => kwin::is_available(),
                        Compositor::Gamescope => gamescope::is_available(),
                        Compositor::Mutter => mutter::is_available(),
                        Compositor::Wlroots => wlroots::is_available(),
                        Compositor::Hyprland => hyprland::is_available(),
                    }
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// The backend an explicit `PUNKTFUNK_COMPOSITOR` value names (aliases included), or `None` for an
/// unrecognized value. Shared by [`detect`] (which turns `None` into an error naming the accepted
/// values) and [`available`] (which just ignores a typo'd pin).
fn compositor_from_pin(v: &str) -> Option<Compositor> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "kwin" | "kde" | "plasma" => Compositor::Kwin,
        // `hyprland` names the distinct backend (D1); `wlroots`/`sway`/`wlr` stay wlroots-proper.
        "hyprland" | "hypr" => Compositor::Hyprland,
        "wlroots" | "sway" | "wlr" | "river" => Compositor::Wlroots,
        "mutter" | "gnome" => Compositor::Mutter,
        "gamescope" => Compositor::Gamescope,
        _ => return None,
    })
}

/// Serializes ALL process-global env mutation on the per-session setup path. `std::env::set_var`
/// concurrent with another thread's `set_var` (glibc `environ` realloc) is a data race = UB. With
/// the default concurrent native sessions each running `resolve_compositor` in its own
/// `spawn_blocking`, the per-session env retargeting would otherwise race and could crash the host
/// (security-review 2026-06-28 #7). Every env write on the setup path takes this lock; steady-state
/// streaming reads cached config, not env. This removes the memory-unsafety; the launch command is
/// additionally threaded per-session (`SessionContext.launch` → `set_launch_command`) so it never
/// rides the process env at all — the remaining knobs here (session retarget, gamescope sub-mode)
/// still carry a cross-session *value* confusion window inherent to a process-global env.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with [`ENV_LOCK`] held. Use around any `set_var`/`remove_var` on the session-setup path.
pub fn with_env_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Detect the compositor to drive: explicit `PUNKTFUNK_COMPOSITOR` override (legacy / CI / forcing
/// a backend for a test), else the **live session** ([`detect_active_session`] — so a Bazzite box
/// follows Gaming↔Desktop switches), else a last-resort `XDG_CURRENT_DESKTOP` read.
pub fn detect() -> Result<Compositor> {
    // Compositor detection is a Linux question — the variants ARE the Linux backends. Asked
    // anywhere else this used to fall through to the XDG sniff below and fail with advice about
    // `XDG_CURRENT_DESKTOP` and `PUNKTFUNK_COMPOSITOR`, which `mgmt/display.rs` puts VERBATIM into
    // the `/display/monitors` response — so on a Windows host the console's only explanation for an
    // empty monitor picker was Linux troubleshooting (sweep §13.17). The operator pin is gated with
    // it: naming a Wayland compositor on Windows cannot be honoured either.
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!(
            "compositor detection is Linux-only; on {} the host enumerates displays through the OS \
             display API instead (`vdisplay::monitors::list_windows`)",
            std::env::consts::OS
        )
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(v) = pf_host_config::config().compositor.as_deref() {
            return compositor_from_pin(v).ok_or_else(|| unknown_pin_error(v));
        }
        if let Some(c) = compositor_for_kind(detect_active_session().kind) {
            return Ok(c);
        }
        // Under [`ENV_LOCK`]: `apply_session_env` `set_var`s — and, for a dead session,
        // `remove_var`s — this very key from another session's `spawn_blocking`, and a glibc
        // `getenv` concurrent with a `setenv` is the `environ` realloc data race ENV_LOCK exists
        // for (it is UB regardless of which key each side touches, so "different variable" is no
        // defence). Read-then-drop: only the read needs serializing.
        let desktop = with_env_lock(|| std::env::var("XDG_CURRENT_DESKTOP"))
            .unwrap_or_default()
            .to_ascii_uppercase();
        compositor_from_xdg(&desktop)
    }
}

/// The error for a `PUNKTFUNK_COMPOSITOR` value that names no backend.
///
/// `cinnamon`/`muffin` get their own answer rather than the bare list: it is the value a Mint or
/// LMDE user reaches for first, and the plain list invites them to try the next-closest name
/// (`mutter` — Muffin *is* a Mutter fork), which starts a session that then fails deep inside a
/// `org.gnome.Mutter.ScreenCast` call Muffin does not serve. There is no working value; say so, and
/// name the route that does work.
#[cfg(target_os = "linux")]
fn unknown_pin_error(v: &str) -> anyhow::Error {
    const ACCEPTED: &str = "kwin|wlroots|hyprland|mutter|gamescope";
    if matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "cinnamon" | "muffin"
    ) {
        return anyhow::anyhow!(
            "PUNKTFUNK_COMPOSITOR='{v}' is not a backend and cannot become one: Cinnamon's \
             compositor Muffin has no virtual-output API (no `RecordVirtual`), so it cannot make a \
             screen for a client. Do NOT substitute 'mutter' — Muffin is a Mutter fork but serves \
             none of that interface. Use PUNKTFUNK_COMPOSITOR=gamescope to stream games through a \
             headless gamescope, which needs no desktop compositor. See \
             https://docs.punktfunk.unom.io/docs/debian#cinnamon-linux-mint-and-lmde"
        );
    }
    anyhow::anyhow!("unknown PUNKTFUNK_COMPOSITOR '{v}' ({ACCEPTED})")
}

/// The last-resort `XDG_CURRENT_DESKTOP` sniff, as a **pure function of the (uppercased) value** so
/// its branches — including the two that only ever produce an error — are testable without mutating
/// process-global env. Called only by [`detect`], after both the operator pin and live-session
/// detection have come up empty.
#[cfg(target_os = "linux")]
fn compositor_from_xdg(desktop: &str) -> Result<Compositor> {
    // CINNAMON is tested FIRST, ahead of GNOME, and the order is load-bearing rather than
    // stylistic: Cinnamon is a GNOME derivative, so a session that advertises both (`X-Cinnamon`
    // alongside a GNOME-compatibility token) would otherwise match the GNOME arm and be handed the
    // Mutter backend — which then fails deep in a `org.gnome.Mutter.ScreenCast` call that Muffin
    // does not serve, i.e. an obscure D-Bus error instead of the explanation below. The more
    // specific desktop wins.
    if desktop.contains("CINNAMON") {
        // Linux Mint / LMDE report `X-Cinnamon`. Cinnamon is NOT a missing backend we could add —
        // its compositor (Muffin) exposes no virtual-output API at all: the fork base is Mutter
        // 3.36, and `org.cinnamon.Muffin.ScreenCast` carries only `RecordMonitor` / `RecordWindow`,
        // never Mutter 42+'s `RecordVirtual`. Its portal backend (xdg-desktop-portal-xapp)
        // implements no ScreenCast either, so the sway/Hyprland portal route is closed too. The
        // generic message below would send a Cinnamon user hunting for the setting that turns it
        // on; there isn't one. Name the ONE route that does work on that box — a headless
        // gamescope, which needs no desktop compositor at all — instead of a dead end.
        anyhow::bail!(
            "Cinnamon (XDG_CURRENT_DESKTOP='{desktop}') cannot host a virtual display: its \
             compositor Muffin has no virtual-output API, so Punktfunk cannot create a screen \
             for a client on it. Stream games instead by setting PUNKTFUNK_COMPOSITOR=gamescope \
             in host.env — the host then spawns its own headless gamescope per connect and needs \
             no desktop session. See \
             https://docs.punktfunk.unom.io/docs/debian#cinnamon-linux-mint-and-lmde"
        )
    } else if desktop.contains("KDE") {
        Ok(Compositor::Kwin)
    } else if desktop.contains("GNOME") {
        Ok(Compositor::Mutter)
    } else if desktop.contains("HYPRLAND") {
        Ok(Compositor::Hyprland)
    } else if desktop.contains("SWAY") || desktop.contains("WLROOTS") {
        Ok(Compositor::Wlroots)
    } else {
        anyhow::bail!(
            "could not detect compositor: no live graphical session for this uid and \
             XDG_CURRENT_DESKTOP='{desktop}'; set PUNKTFUNK_COMPOSITOR"
        )
    }
}

/// Attach-only probes: while any scope is held, backend `create` paths must not stop, relaunch,
/// or take over box sessions — they may only attach to an already-live output, and fail fast
/// otherwise. The capture-loss rebuild holds one for its first seconds: right after a capture
/// loss the active-session detection can be STALE (a Game→Desktop switch observed live: the
/// probe's gamescope re-acquire restarted `gamescope-session.target` and yanked the user out of
/// the KDE session they had just switched to). A counter, so overlapping scopes compose.
static REBUILD_PROBES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// RAII scope marking pipeline builds as attach-only probes (see [`rebuild_probe_active`]).
pub struct RebuildProbeScope(());

pub fn rebuild_probe_scope() -> RebuildProbeScope {
    REBUILD_PROBES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    RebuildProbeScope(())
}

impl Drop for RebuildProbeScope {
    fn drop(&mut self) {
        REBUILD_PROBES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Is any [`rebuild_probe_scope`] active? Destructive session operations (stop/relaunch/
/// takeover-restart) must be skipped while true.
pub fn rebuild_probe_active() -> bool {
    REBUILD_PROBES.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// The monitor this host mirrors instead of creating a virtual display, or `None` for the normal
/// virtual-display path.
///
/// Precedence: **`PUNKTFUNK_CAPTURE_MONITOR` wins over the stored policy.** An appliance pins in
/// `host.env` and must stay pinned there — a console click (or a stale settings file) should not be
/// able to re-aim a machine whose operator declared the answer in its unit's environment. With the
/// env unset, the console's persisted choice applies, which is what makes a picker possible at all:
/// the env is read once at startup, so it could never be what a UI writes.
///
/// Read per `open` rather than cached, so a console change takes effect on the next session instead
/// of at the next host restart.
pub fn capture_monitor() -> Option<String> {
    if let Some(env) = pf_host_config::config().capture_monitor.as_deref() {
        return Some(env.to_string());
    }
    policy::prefs().get().capture_monitor
}

/// Open the virtual-display driver for `compositor`.
///
/// A [`capture_monitor`] pin routes to the **mirror** backend instead: the host streams that
/// physical head and creates no virtual display at all. Deliberately resolved here, at the one place
/// every session opens a display, so the pin can't be honored on one plane and ignored on another —
/// it is a host-wide setting (`design/per-monitor-portal-capture.md` §5.3).
pub fn open(compositor: Compositor) -> Result<Box<dyn VirtualDisplay>> {
    #[cfg(target_os = "linux")]
    if let Some(connector) = capture_monitor() {
        // A pin is host-wide and PERSISTED, but the session it applies to is not: a box that boots
        // between a desktop (heads to mirror) and a nested/headless Game Mode (none) would carry the
        // pin into a session where `resolve` can only fail — and since this is the one place every
        // session opens a display, that failure is a host that refuses to stream at all rather than
        // one that streams the normal way. So a compositor reporting NO heads whatsoever degrades to
        // the virtual-display path with the reason logged.
        //
        // Narrow on purpose. A pin that misses among heads that DO exist stays the hard error
        // `monitors::resolve` makes it (design/per-monitor-portal-capture.md §5.2): showing someone
        // a different screen than they asked for is the failure worth refusing over, and "there are
        // no screens here" is not that. An enumeration ERROR also stays on the mirror path, so the
        // session fails with the real reason instead of quietly ignoring the operator's choice.
        match monitors::list(compositor) {
            Ok(heads) if heads.is_empty() => tracing::warn!(
                pinned = %connector,
                compositor = compositor.id(),
                "the streamed-screen pin names a monitor but this session has no physical heads to \
                 mirror (a nested or headless compositor) — creating a virtual display instead; the \
                 pin applies again as soon as a session with real heads runs"
            ),
            _ => return Ok(Box::new(mirror::MirrorDisplay::new(compositor, connector)?)),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match compositor {
            Compositor::Kwin => Ok(Box::new(kwin::KwinDisplay::new()?)),
            Compositor::Gamescope => Ok(Box::new(gamescope::GamescopeDisplay::new()?)),
            Compositor::Mutter => Ok(Box::new(mutter::MutterDisplay::new()?)),
            Compositor::Wlroots => Ok(Box::new(wlroots::WlrootsDisplay::new()?)),
            Compositor::Hyprland => Ok(Box::new(hyprland::HyprlandDisplay::new()?)),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // The pf-vdisplay all-Rust IddCx driver is the sole virtual-display backend (the legacy SudoVDA
        // fallback was removed — its driver is no longer shipped). The compositor arg is moot on Windows.
        let _ = compositor;
        // `ensure_available` waits out a devnode that is merely coming up (the wake-from-sleep case:
        // the adapter re-enters D0 and re-registers its interface while a reconnecting client is
        // already knocking) and self-heals the hostless-zombie state a WUDFHost crash leaves (adapter
        // devnode present, interface gone) by reloading the adapter.
        //
        // `context`, not a replacement message: it reports WHY — how long it waited, whether a reload
        // ran, how many interface instances were seen and in what state. A flat "the driver is not
        // installed" is what a field report carried from a box whose driver was installed, started,
        // and simply mid-resume, and it pointed every reader at the wrong problem.
        use anyhow::Context as _;
        driver::ensure_available().context(
            "pf-vdisplay driver interface not available — the pf-vdisplay IddCx driver is not \
             installed, not loaded, or did not finish coming back up (the host installer bundles \
             it; reinstall or check the driver state)",
        )?;
        Ok(Box::new(driver::PfVdisplayDisplay::new()?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux or Windows")
    }
}

/// Open the **mirror** backend for a specific monitor, bypassing the `PUNKTFUNK_CAPTURE_MONITOR`
/// pin that [`open`] consults. For tools that name the head explicitly (`punktfunk-host
/// mirror-test`) — the pin can't serve them, since `pf_host_config` parses the environment once at
/// startup, so a tool setting the variable for itself would be reading a snapshot taken before it.
#[cfg(target_os = "linux")]
pub fn open_mirror(compositor: Compositor, connector: &str) -> Result<Box<dyn VirtualDisplay>> {
    Ok(Box::new(mirror::MirrorDisplay::new(
        compositor,
        connector.to_string(),
    )?))
}

/// Readiness probe for `compositor`: is it up and able to create a virtual output *right
/// now*? A session-bringup script polls this (via `punktfunk-host probe-compositor`) to gate
/// on actual readiness instead of racing the compositor with a blind sleep.
///
/// KWin gets a real check (the privileged `zkde_screencast` global must be advertised). The
/// others are spawn/D-Bus/portal-based and have no equivalent pre-flight global, so a probe
/// just confirms the backend opens — `Ok(())` means "go ahead and try `create`".
pub fn probe(compositor: Compositor) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        match compositor {
            Compositor::Kwin => kwin::probe(),
            // Hyprland gets a real pre-flight: `hyprctl` must reach the compositor (else a clear
            // error instead of a create-time failure), plus the permission-system warning.
            Compositor::Hyprland => hyprland::probe(),
            // gamescope spawns its own nested session per `create`; Mutter is D-Bus on demand;
            // wlroots creates the output on demand — nothing to pre-check beyond "Linux".
            Compositor::Gamescope | Compositor::Mutter | Compositor::Wlroots => Ok(()),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = compositor;
        driver::probe()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux or Windows")
    }
}

// The user-configurable management policy (keep-alive / topology / conflict / identity / layout),
// layered above the per-compositor backends — platform-neutral (the mgmt API + both host paths read
// it), so no cfg gate. See `design/display-management.md`.
#[path = "vdisplay/policy.rs"]
pub mod policy;

// Read-only physical-monitor enumeration (the heads the compositor ALREADY has — not ours), for
// pinning capture at one of them + the console picker. Platform-neutral facade; the per-backend
// reads live beside the code that already speaks each dialect. See
// `design/per-monitor-portal-capture.md` §5.1.
#[path = "vdisplay/monitors.rs"]
pub mod monitors;

// The monitor-mirror backend: stream a head the compositor ALREADY has (the
// `PUNKTFUNK_CAPTURE_MONITOR` pin) instead of creating one. Implements `VirtualDisplay` so the
// session machinery is unchanged, but reports `DisplayOwnership::External` so none of the
// virtual-display lifecycle policy is applied to someone else's monitor.
#[cfg(target_os = "linux")]
#[path = "vdisplay/mirror.rs"]
mod mirror;

// The pure per-display lifecycle state machine (refcount + linger + pin), platform-neutral and
// property-tested; the registry executes the side effects its transitions dictate.
#[path = "vdisplay/lifecycle.rs"]
pub(crate) mod lifecycle;

// The neutral snapshot/release facade over the per-OS lifecycle owners (Windows manager; Linux pool
// later), for the management API's /display/state + /display/release.
#[path = "vdisplay/registry.rs"]
pub mod registry;

// The pure display-arrangement engine (auto-row / manual → per-member positions), platform-neutral
// and unit-tested; the registry (state readout) and the KWin position apply consume it.
#[path = "vdisplay/layout.rs"]
pub(crate) mod layout;

/// Resolve a [`policy::Topology`] to a concrete value (never [`policy::Topology::Auto`]). `Auto`
/// reproduces today's default: **extend** under an explicit `PUNKTFUNK_COMPOSITOR` pin (the CI/test
/// posture, where the host isn't the sole desktop), else **exclusive** (Windows + the auto-detected
/// Linux desktop path, where "stream this desktop" means promoting the virtual output to sole).
pub fn resolve_topology(t: policy::Topology) -> policy::Topology {
    match t {
        policy::Topology::Auto => {
            if pf_host_config::config().compositor.is_some() {
                policy::Topology::Extend
            } else {
                policy::Topology::Exclusive
            }
        }
        concrete => concrete,
    }
}

/// The concrete display topology for the current session — what the per-compositor backends (and the
/// Windows isolate gate) apply at create time. Precedence, mirroring the rest of the policy surface:
/// the **console policy** when configured, else the legacy **`PUNKTFUNK_{KWIN,MUTTER}_VIRTUAL_PRIMARY`**
/// env (an operator's explicit choice — `1`→exclusive, `0`→extend), else the **Auto** default
/// ([`resolve_topology`]: exclusive on the auto-detected desktop / Windows, extend under a compositor
/// pin). Always resolved (never [`policy::Topology::Auto`]). This is the Stage-2 replacement for the
/// `apply_session_env` boolean write — the backends read policy directly, so the `primary` level
/// (distinct from `exclusive`) becomes expressible and one process-env mutation drops off the connect
/// path.
pub fn effective_topology() -> policy::Topology {
    if let Some(e) = policy::prefs().configured_effective() {
        return resolve_topology(e.topology);
    }
    // Unconfigured: honor a legacy operator env if present (a host runs one desktop backend, so at
    // most one of these is set), else the Auto default. Read under [`ENV_LOCK`] like every other
    // env read on the session-setup path: this runs inside `create`, concurrent with another
    // session's `apply_session_env` `set_var`s, and glibc's `environ` realloc makes a racing
    // `getenv` UB no matter that these particular keys are ones nobody writes.
    let legacy = with_env_lock(|| {
        [
            "PUNKTFUNK_KWIN_VIRTUAL_PRIMARY",
            "PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY",
        ]
        .iter()
        .find_map(|k| std::env::var(k).ok())
    });
    match legacy.as_deref().map(str::trim) {
        Some("1" | "true" | "yes" | "on") => policy::Topology::Exclusive,
        Some("0" | "false" | "no" | "off") => policy::Topology::Extend,
        _ => resolve_topology(policy::Topology::Auto),
    }
}

// Goal-1 stage 6: per-compositor Linux backends under `vdisplay/linux/`, the Windows IddCx/SudoVDA
// backends under `vdisplay/windows/`; `#[path]` keeps the `crate::*` module names flat.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/gamescope.rs"]
mod gamescope;

/// Entry point for the hidden `gamescope-splash` host subcommand: the tiny X11 client every bare
/// gamescope spawn backgrounds beside its nested app, so the fresh compositor composites — and its
/// PipeWire node delivers frames — from the first second instead of starving the first-frame wait
/// while the nested Steam bootstrap paints nothing (see `vdisplay/linux/gamescope/splash.rs`).
/// Blocks for the session's lifetime; gamescope's reaper tears it down with the session.
#[cfg(target_os = "linux")]
pub fn gamescope_splash_client() -> anyhow::Result<()> {
    gamescope::splash_run()
}

/// Can a gamescope session on this host stream true HDR (10-bit BT.2020 PQ)?
///
/// **A static answer, resolved before anything is spawned** — the punktfunk/1 Welcome fixes a
/// session's bit depth before the display exists and cannot take it back. Two terms, both facts
/// about how the session will be brought up rather than about how it went:
///
/// 1. the resolved gamescope binary offers 10-bit PQ capture formats (our `pipewire-hdr` build —
///    probed once per process from the `--version` banner), and
/// 2. this host **spawns** gamescope rather than attaching to a foreign one
///    (`PUNKTFUNK_GAMESCOPE_NODE`): an attach inherits whatever flags someone else's session was
///    started with, so it can promise nothing. (Attach-mode HDR needs the distro's gamescope
///    patched — the upstream-PR follow-up.)
///
/// A host-managed `gamescope-session-plus` / SteamOS session counts as a spawn: we own its
/// `GAMESCOPE_BIN` wrapper (or PATH shim), so the flags are ours.
///
/// Always `false` off Linux.
pub fn gamescope_hdr_available() -> bool {
    gamescope_ours_and(
        #[cfg(target_os = "linux")]
        gamescope::gamescope_hdr_capable,
    )
}

/// Will a gamescope session paint the pointer INTO its PipeWire node, so the host does NOT have to
/// reconstruct it from XFixes and blend it into every frame?
///
/// That blend is not free: it forces the encode path onto its compute colour-conversion arm,
/// because the zero-copy RGB-direct source hands the captured buffer to a fixed-function front end
/// with no blend stage. So a `true` here is what lets a gamescope session skip a full-frame pass
/// per frame — and, like the HDR answer, it must be settled BEFORE the session is planned
/// (`SessionPlan::cursor_blend` feeds the encoder open).
///
/// Same two terms as [`gamescope_hdr_available`], for the same reasons: the resolved binary
/// carries the patch, and this host is the one spawning it.
pub fn gamescope_composites_cursor() -> bool {
    gamescope_ours_and(
        #[cfg(target_os = "linux")]
        gamescope::gamescope_can_composite_cursor,
    )
}

/// The shared half of the two capability answers above: the session must be one this host
/// **spawns** (an attach inherits whatever flags someone else's session was started with, so it
/// can promise nothing), and then `probe` — which asks the resolved binary — must agree.
///
/// A host-managed `gamescope-session-plus` / SteamOS session counts as a spawn: we own its
/// `GAMESCOPE_BIN` wrapper (or PATH shim), so the flags are ours.
///
/// **Ask the resolved ROUTE, never the env.** This used to test the spawn-vs-attach term by reading
/// `PUNKTFUNK_GAMESCOPE_NODE`, which worked only while `apply_input_env` PUBLISHED its decision into
/// that key. Phase 2.3 deleted the publication (routing.rs: "Nothing is written back to the two
/// knobs") and left the key as an operator override — rung 2 of a 6-rung ladder — so the session
/// that reaches [`GamescopeRoute::Attach`] at the ladder's rung 5 instead (a foreign gamescope on an
/// infra-less box), and the monitor-pin mirror that never consults the ladder at all, both answered
/// "ours". The two consequences were silent and unrecoverable: the punktfunk/1 Welcome fixed the
/// session at 10-bit BT.2020/PQ against a foreign 8-bit SDR composite, and the host skipped the
/// XFixes cursor reconstruction for a session whose gamescope was never given
/// `--pipewire-composite-cursor` — a stream with no pointer in it at all.
///
/// **Two residual gaps**, both of which need a route this crate cannot see from here:
///
/// * the ladder is re-run with `dedicated_launch = false`, since a capability query carries no
///   session context — so it cannot see the one input that would move a session from
///   Managed/Attach to Spawn. On a box with no session infrastructure AND a foreign gamescope
///   running, a `game_session=dedicated` launch really takes rung 3 (`Spawn`) while this re-run
///   takes rung 5 (`Attach`) and answers "foreign";
/// * `create_managed_session` can degrade a resolved `Managed` to an ATTACH at create time (a
///   mask-fragile DM it may not stop — it then mirrors the box's own game-mode session). That
///   happens after this answer is due, and the ladder re-run here still says `Managed`, so such a
///   session is still credited with flags it does not own.
///
/// The second over-promises. The first UNDER-promises, and `false` is the deliberate choice for an
/// input we cannot see, because the two directions do not cost the same: over-promising fixes the
/// punktfunk/1 Welcome at 10-bit PQ against an 8-bit SDR composite and leaves a stream with **no
/// pointer at all**, while under-promising costs HDR and draws the pointer twice. But do not read
/// that as "fails closed": it is not, for the cursor. `gamescope::cursor_args` adds
/// `--pipewire-composite-cursor` from the BINARY probe alone, ungated by this answer, so on the
/// bare spawn above gamescope paints the pointer into the node while the host's
/// `session_plan::gamescope_needs_host_cursor` (`gamescope && !gamescope_composites_cursor()`) also
/// blends the XFixes pointer on top — two pointers, plus the encoder pushed off its zero-copy arm.
/// Do not "fix" that by re-running the ladder with a guessed `dedicated_launch = true`: that trades
/// the mild failure for the severe one on every non-launching session. Both gaps close the same
/// way, and only that way: give these two functions the session's own [`GamescopeRoute`] (which
/// `SessionContext` already carries) and have the backend report the degrade — a change to two
/// public signatures and every host call site, i.e. work outside this crate.
fn gamescope_ours_and(#[cfg(target_os = "linux")] probe: fn() -> bool) -> bool {
    #[cfg(target_os = "linux")]
    {
        // `probe` first: it is memoized (the `--version` banner is parsed once per process), while
        // the route resolution walks `/proc` for a foreign gamescope. On a box with a stock
        // gamescope the answer is already `false` and the walk never happens.
        probe()
            && !session_is_a_foreign_gamescope(
                capture_monitor().is_some(),
                resolve_gamescope_route(Compositor::Gamescope, false).as_ref(),
            )
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Pure predicate behind [`gamescope_ours_and`]: is the gamescope this session will use one
/// SOMEBODY ELSE started, whose spawn flags we therefore cannot vouch for?
///
/// Two ways to land on a foreign session, and both must count:
///
/// * `mirror_pinned` — a `PUNKTFUNK_CAPTURE_MONITOR` pin routes [`open`] to the mirror backend,
///   whose gamescope arm attaches to the node the RUNNING session already publishes without
///   consulting the sub-mode ladder at all. On a Bazzite/SteamOS box that session is Game Mode's,
///   i.e. by definition not ours.
/// * a [`GamescopeRoute::Attach`] verdict — however the ladder reached it (operator override,
///   or the foreign-gamescope rung).
///
/// [`GamescopeRoute::Managed`] is NOT foreign: the managed takeover starts the session through our
/// own `GAMESCOPE_BIN` wrapper / PATH shim, so its flags are the ones we chose.
///
/// `mirror_pinned` is judged from the pin alone, not from whether the mirror actually took: [`open`]
/// degrades a pin to the virtual-display path when the session reports no physical heads, and a
/// pinned box that lands there is called foreign here although it will bare-spawn. That is the
/// fail-closed direction — a capability withheld from a session that could have had it — and the
/// alternative (enumerating heads from a capability query) would put a compositor roundtrip on a
/// path that must answer before anything exists to ask.
fn session_is_a_foreign_gamescope(mirror_pinned: bool, route: Option<&GamescopeRoute>) -> bool {
    mirror_pinned || matches!(route, Some(GamescopeRoute::Attach { .. }))
}

// Platform-neutral per-client stable display-id map: Windows seeds the monitor EDID serial +
// IddCx ConnectorIndex from the id; KWin names its output `Virtual-punktfunk-<id>` (kwin.rs's
// `resolve_slot` call); Mutter cannot carry the id into its virtual monitor at all, so it keys the
// host-persisted `ScaleMap` on the same identity key. All three are production call sites, so the
// `allow(dead_code)` below no longer stands for "unwired yet" (it did when only Windows consumed the
// map); it now covers whatever helpers no CURRENT backend reaches. Worth re-testing without it —
// that has to happen on a Linux build, since this is the platform where dead_code is enforced.
#[allow(dead_code)]
#[path = "vdisplay/identity.rs"]
pub(crate) mod identity;

// Platform-neutral mode-conflict admission (Stage 4): the separate/join/steal/reject decision + the
// live-session registry, wired into the punktfunk/1 handshake.
#[path = "vdisplay/admission.rs"]
pub mod admission;

/// Editing the user's xdg-desktop-portal configs in place — the wlr-family backends both need one
/// key set in a file the USER owns, and used to overwrite the whole thing.
///
/// Declared unconditionally although only the Linux backends call it: the merge is pure string
/// handling, so its tests — which are what make a merge safe to run against a user's real config —
/// should run on every platform's CI rather than only where the callers compile.
#[path = "vdisplay/linux/portal_config.rs"]
mod portal_config;

/// Which ScreenCast cursor mode to REQUEST — negotiated against `AvailableCursorModes` instead of
/// hardcoded, because a mode the backend does not advertise closes the session outright.
///
/// Declared unconditionally for the same reason as `portal_config` above: the ladder is pure
/// integer work whose tests are the only place its behaviour is observable without a compositor,
/// so they should run on every platform's CI rather than only where the callers compile.
#[path = "vdisplay/linux/portal_cursor.rs"]
mod portal_cursor;

/// The line fed to xdph's custom picker to select an output headlessly.
///
/// Declared unconditionally for the same reason again: it is a wire format with no schema and no
/// error report, so the transcribed-parser tests are the only place a malformed line is visible
/// without a compositor. That is not hypothetical — a missing separator shipped, and the one
/// assertion that existed for it passed throughout.
#[path = "vdisplay/linux/portal_picker.rs"]
mod portal_picker;

/// The single, never-dropped tokio runtime the portal handshakes run on. Linux-only: it exists to
/// outlive ashpd's process-global cached D-Bus connection, and only the Linux backends speak to it.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/portal_rt.rs"]
mod portal_rt;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/hyprland.rs"]
mod hyprland;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin.rs"]
mod kwin;

// In-process KDE output management (kde_output_management_v2) — the topology path that used to shell
// out to `kscreen-doctor`, driven over the compositor's own Wayland instead so it can't be wedged by
// a stuck libkscreen/kscreen-KDED backend. Consumed by `kwin` (best-effort, with kscreen fallback).
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin_output_mgmt.rs"]
mod kwin_output_mgmt;

// DPMS control of the box's live KDE desktop (org_kde_kwin_dpms) — how a bare-spawn gamescope
// session honors `Topology::Exclusive`: the spawn is its own headless compositor, so the desktop's
// physical outputs can't be *disabled* (KWin refuses zero enabled outputs and no output there is
// ours) — they are put to DPMS-off for the stream instead, refcounted across concurrent spawns.
// Consumed by `gamescope` (best-effort, with kscreen fallback).
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin_dpms.rs"]
mod kwin_dpms;

#[cfg(target_os = "windows")]
#[path = "vdisplay/windows/manager.rs"]
pub mod manager;

// DDC/CI panel power control (physical monitors), used only by the Windows manager to blank/wake the
// box's real panels around a virtual-display session — moved in with the subsystem (plan §W6).
#[cfg(target_os = "windows")]
#[path = "vdisplay/ddc.rs"]
mod ddc;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/mutter.rs"]
mod mutter;

#[cfg(target_os = "windows")]
#[path = "vdisplay/windows/pf_vdisplay.rs"]
pub mod driver;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/wlroots.rs"]
mod wlroots;

#[cfg(test)]
mod tests {
    use super::*;

    /// The XDG sniff is the last thing standing between an unrecognized desktop and a useless
    /// error, and `mgmt/display.rs` puts that error VERBATIM in the console's `/display/monitors`
    /// response — so its exact wording is a user-facing surface, tested as one.
    #[cfg(target_os = "linux")]
    #[test]
    fn xdg_sniff_maps_known_desktops() {
        // Real-world values, uppercased the way `detect` hands them over.
        assert_eq!(compositor_from_xdg("KDE").unwrap(), Compositor::Kwin);
        assert_eq!(compositor_from_xdg("GNOME").unwrap(), Compositor::Mutter);
        assert_eq!(
            compositor_from_xdg("UBUNTU:GNOME").unwrap(),
            Compositor::Mutter
        );
        assert_eq!(
            compositor_from_xdg("HYPRLAND").unwrap(),
            Compositor::Hyprland
        );
        assert_eq!(compositor_from_xdg("SWAY").unwrap(), Compositor::Wlroots);
    }

    /// Cinnamon must NOT fall into the generic "set PUNKTFUNK_COMPOSITOR" arm: Muffin has no
    /// virtual-output API, so there is no value of that variable which makes a Cinnamon desktop
    /// host a virtual display. The error has to name gamescope — the one route that works on an
    /// LMDE/Mint box — or the user is sent hunting for a setting that does not exist.
    #[cfg(target_os = "linux")]
    #[test]
    fn cinnamon_is_told_to_use_gamescope_not_to_pick_a_backend() {
        // `X-Cinnamon` is what Mint and LMDE actually set.
        for v in ["X-CINNAMON", "CINNAMON", "X-CINNAMON:GNOME-FLASHBACK"] {
            let err = compositor_from_xdg(v)
                .expect_err("Cinnamon cannot host a virtual display")
                .to_string();
            assert!(err.contains("gamescope"), "no gamescope route named: {err}");
            assert!(err.contains("Muffin"), "does not say why: {err}");
        }
    }

    /// Pinning `cinnamon` explicitly must not answer with the plain list of accepted values: the
    /// next thing a Mint user tries is `mutter` (Muffin is a Mutter fork), which fails much later
    /// and much less clearly. A typo'd pin still gets the ordinary list.
    #[cfg(target_os = "linux")]
    #[test]
    fn pinning_cinnamon_explains_instead_of_listing_backends() {
        for v in ["cinnamon", "Cinnamon", "muffin", " MUFFIN "] {
            let err = unknown_pin_error(v).to_string();
            assert!(err.contains("gamescope"), "no working route named: {err}");
            assert!(
                err.contains("Muffin"),
                "does not explain why it cannot work: {err}"
            );
        }
        let typo = unknown_pin_error("kwim").to_string();
        assert!(
            typo.contains("kwin|wlroots|hyprland|mutter|gamescope"),
            "{typo}"
        );
        assert!(!typo.contains("Muffin"), "{typo}");
    }

    /// An unknown desktop keeps the generic advice — the Cinnamon arm must not swallow it.
    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_desktop_keeps_the_generic_error() {
        let err = compositor_from_xdg("XFCE").unwrap_err().to_string();
        assert!(err.contains("PUNKTFUNK_COMPOSITOR"), "{err}");
        assert!(!err.contains("Muffin"), "{err}");
    }

    #[test]
    fn active_kind_maps_to_its_backend() {
        assert_eq!(
            compositor_for_kind(ActiveKind::Gaming),
            Some(Compositor::Gamescope)
        );
        assert_eq!(
            compositor_for_kind(ActiveKind::DesktopKde),
            Some(Compositor::Kwin)
        );
        assert_eq!(
            compositor_for_kind(ActiveKind::DesktopGnome),
            Some(Compositor::Mutter)
        );
        assert_eq!(
            compositor_for_kind(ActiveKind::DesktopWlroots),
            Some(Compositor::Wlroots)
        );
        assert_eq!(
            compositor_for_kind(ActiveKind::DesktopHyprland),
            Some(Compositor::Hyprland)
        );
        // No live session → no backend; the caller turns this into a handshake error / fallback.
        assert_eq!(compositor_for_kind(ActiveKind::None), None);
    }

    /// The spawn-vs-attach term behind [`gamescope_hdr_available`] /
    /// [`gamescope_composites_cursor`]. Both answers are IRREVOCABLE once the punktfunk/1 Welcome
    /// has gone out (bit depth is fixed there; the session plan's cursor decision feeds the encoder
    /// open), so an over-promise here is not recoverable at runtime — which is why the regression
    /// this pins mattered: the term used to be read off `PUNKTFUNK_GAMESCOPE_NODE`, a key nothing
    /// writes any more, so every foreign session answered "ours".
    #[test]
    fn only_a_session_we_start_can_promise_gamescope_capabilities() {
        // Attach — however the ladder got there — is somebody else's session: unknown spawn flags.
        assert!(session_is_a_foreign_gamescope(
            false,
            Some(&GamescopeRoute::Attach {
                node: "auto".into()
            })
        ));
        assert!(session_is_a_foreign_gamescope(
            false,
            Some(&GamescopeRoute::Attach { node: "42".into() })
        ));
        // A bare spawn is ours by definition; so is the managed takeover (it starts gamescope
        // through our own GAMESCOPE_BIN wrapper / PATH shim, so the flags are the ones we chose).
        assert!(!session_is_a_foreign_gamescope(
            false,
            Some(&GamescopeRoute::Spawn)
        ));
        assert!(!session_is_a_foreign_gamescope(
            false,
            Some(&GamescopeRoute::Managed {
                client: "steam".into()
            })
        ));
        // No route at all = not a gamescope session; the binary probe alone then decides.
        assert!(!session_is_a_foreign_gamescope(false, None));
        // A monitor pin bypasses the ladder entirely (mirror backend → attach to the node the
        // RUNNING session publishes), so it is foreign whatever the ladder would have said.
        assert!(session_is_a_foreign_gamescope(true, None));
        assert!(session_is_a_foreign_gamescope(
            true,
            Some(&GamescopeRoute::Spawn)
        ));
    }

    #[test]
    fn detect_active_session_is_side_effect_free_and_terminates() {
        // A pure probe of /proc + the runtime dir: it must not panic and must return promptly on
        // any box (CI has no graphical session → ActiveKind::None, with the runtime-dir anchor).
        let a = detect_active_session();
        // The runtime-dir anchor is a Linux (XDG) concept; Windows has no equivalent.
        #[cfg(target_os = "linux")]
        assert!(!a.env.xdg_runtime_dir.is_empty());
        // Wayland sockets are only resolved for the Wayland-protocol desktops.
        if matches!(
            a.kind,
            ActiveKind::Gaming | ActiveKind::DesktopGnome | ActiveKind::None
        ) {
            assert!(a.env.wayland_display.is_none());
        }
    }
}
