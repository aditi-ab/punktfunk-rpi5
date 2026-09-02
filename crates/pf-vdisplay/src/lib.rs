//! Client-sized virtual outputs for capture; released on disconnect.
//!
//! [`VirtualDisplay`] is the compositor-agnostic driver. [`VirtualDisplay::create`]
//! returns a [`VirtualOutput`] (capture target + RAII keepalive). Physical-monitor
//! capture is [`DisplayOwnership::External`]: lifecycle policy must not alter the
//! display. Gamescope spawn/manage/attach is [`GamescopeRoute`].

// Linux keeps `dead_code` on. Windows/macOS allow it: `proc`/`session`/`routing`/
// `monitors`/`lifecycle` exist for the Linux backends and are unused there. The
// Windows backend (`vdisplay/windows/`) is not enforced; review has to catch orphans.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::Result;
pub use punktfunk_core::Mode;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

/// Registry create/release of a managed virtual display. The host registers
/// [`DISPLAY_EVENT_SINK`]; this crate does not own the SSE bus type.
pub enum DisplayEvent {
    Created {
        backend: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    Released {
        count: u32,
    },
}

/// Host SSE sink. Unset at startup: events before register are dropped.
pub static DISPLAY_EVENT_SINK: std::sync::OnceLock<Box<dyn Fn(DisplayEvent) + Send + Sync>> =
    std::sync::OnceLock::new();

pub(crate) fn emit_display_event(ev: DisplayEvent) {
    if let Some(sink) = DISPLAY_EVENT_SINK.get() {
        sink(ev);
    }
}

/// Backend contract: [`DisplayOwnership`], [`VirtualOutput`], [`VirtualDisplay`].
/// Re-exported so `crate::VirtualDisplay` stays the public name.
#[path = "vdisplay/backend.rs"]
pub(crate) mod backend;
pub use backend::{DisplayOwnership, SessionIsolation, VirtualDisplay, VirtualOutput};
/// Negotiated ScreenCast cursor mode of a portal-backed output
/// ([`VirtualDisplay::last_portal_cursor_mode`]). The picker stays private; the verdict is the caller's.
pub use portal_cursor::Mode as PortalCursorMode;

/// Time-bounded child-process helpers. Compositor queries shell out; an unbounded wait wedges the session thread.
#[path = "vdisplay/proc.rs"]
pub(crate) mod proc;

#[path = "vdisplay/session.rs"]
pub(crate) mod session;
#[cfg(target_os = "linux")]
pub use session::session_epoch;
pub use session::{
    apply_session_env, compositor_for_kind, detect_active_session, observe_session_instance,
    settle_desktop_portal, try_recover_session, ActiveKind, ActiveSession, SessionEnv,
};

#[path = "vdisplay/routing.rs"]
pub(crate) mod routing;
pub use routing::{
    cancel_pending_tv_restore, input_backend_id, managed_session_available,
    preflight_takeover_privilege, release_autologin_mask, resolve_gamescope_route,
    restore_managed_session, restore_takeover_now, restore_takeover_on_startup,
    start_restore_worker, takeover_privilege_verdict, wants_dedicated_game_session, GamescopeRoute,
    TakeoverInapplicable, TakeoverVerdict,
};
#[cfg(target_os = "linux")]
pub use routing::{
    dedicated_game_exited, focus_streamed_output, gamescope_xwayland_cursor_targets,
    launch_into_gamescope_session, launch_is_nested, steam_appid_from_launch,
    watch_steam_game_exit,
};

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
    /// Hyprland — `hyprctl output create` + xdph ScreenCast. Distinct from
    /// [`Wlroots`]: it kept the wlr client protocols after dropping wlroots, so
    /// virtual-input is shared but IPC and portal are not. `design/hyprland-support.md`.
    Hyprland,
}

impl Compositor {
    /// Wire / management id; matches [`punktfunk_core::CompositorPref::as_str`].
    pub fn id(self) -> &'static str {
        match self {
            Compositor::Kwin => "kwin",
            Compositor::Wlroots => "wlroots",
            Compositor::Mutter => "mutter",
            Compositor::Gamescope => "gamescope",
            Compositor::Hyprland => "hyprland",
        }
    }

    /// True unless this backend can stand a session up from nothing ([`Compositor::Gamescope`]).
    /// Desktop backends attach over IPC; with no compositor running, `create` can only fail.
    pub fn needs_live_session(self) -> bool {
        !matches!(self, Compositor::Gamescope)
    }

    pub fn label(self) -> &'static str {
        match self {
            Compositor::Kwin => "KWin / KDE Plasma",
            Compositor::Wlroots => "wlroots (Sway / River)",
            Compositor::Mutter => "Mutter / GNOME",
            Compositor::Gamescope => "gamescope",
            Compositor::Hyprland => "Hyprland",
        }
    }

    pub fn as_pref(self) -> punktfunk_core::CompositorPref {
        use punktfunk_core::CompositorPref as P;
        match self {
            Compositor::Kwin => P::Kwin,
            Compositor::Wlroots => P::Wlroots,
            Compositor::Mutter => P::Mutter,
            Compositor::Gamescope => P::Gamescope,
            // No distinct wire byte: Hyprland shares the wlroots-family `Wlroots` pref.
            // `pick_compositor` (host `native`) picks whichever of the two is live.
            Compositor::Hyprland => P::Wlroots,
        }
    }

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

    /// Every backend, in stable UI/enumeration order.
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

/// Backends usable now: gamescope if its binary exists, plus the live or pinned
/// session's compositor. **Not cheap, not memoized** — re-walks `/proc`, and
/// unexempted probes fork `gamescope --version` or do a Wayland/D-Bus roundtrip.
/// Cache if this is a hot path; do not block the async runtime.
///
/// Live/pinned session wins over each backend's env-reading probe. Those probes
/// miss a host started outside the session; env is retargeted only on connect
/// ([`apply_session_env`]). Listing a live-but-ungranted KWin and failing at
/// `create` beats "no usable compositor" on a box that is running KDE.
pub fn available() -> Vec<Compositor> {
    #[cfg(target_os = "linux")]
    {
        let live = compositor_for_kind(detect_active_session().kind);
        // Operator pin is what `detect` returns and what the host drives.
        let pinned = pf_host_config::config()
            .compositor
            .as_deref()
            .and_then(compositor_from_pin);
        Compositor::all()
            .into_iter()
            .filter(|&c| {
                // Live or pinned ⇒ usable without the env-reading probe. KWin's probe also
                // checks the `zkde_screencast` grant; listing an ungranted KWin and failing
                // at create beats "no usable compositor" on a running KDE box.
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

/// Backend named by an explicit `PUNKTFUNK_COMPOSITOR` pin, aliases included.
/// [`detect`] errors on `None`; [`available`] ignores a typo.
fn compositor_from_pin(v: &str) -> Option<Compositor> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "kwin" | "kde" | "plasma" => Compositor::Kwin,
        // `hyprland` is its own backend; `wlroots`/`sway`/`wlr` stay wlroots-proper.
        "hyprland" | "hypr" => Compositor::Hyprland,
        "wlroots" | "sway" | "wlr" | "river" => Compositor::Wlroots,
        "mutter" | "gnome" => Compositor::Mutter,
        "gamescope" => Compositor::Gamescope,
        _ => return None,
    })
}

/// Serializes this crate's process-env reads/writes on the per-session setup
/// path so two concurrent `spawn_blocking` sessions cannot interleave
/// retarget with each other's reads.
///
/// Does **not** make `set_var`/`remove_var` sound. `setenv(3)` mutates
/// `environ`; any concurrent `getenv` in the process is a data race. glibc,
/// zbus, wayland-client, and Mesa do not take this lock.
///
/// Remaining writes are [`apply_session_env`]'s four desktop variables,
/// which foreign loaders read from `environ` only. The lock orders our
/// own readers and keeps those writes from racing each other — discipline,
/// never proof.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with [`ENV_LOCK`] held. Wrap any session-setup `set_var`/`remove_var`.
pub fn with_env_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Compositor to drive: `PUNKTFUNK_COMPOSITOR` pin, else the live session
/// ([`detect_active_session`]), else a last-resort `XDG_CURRENT_DESKTOP` read.
pub fn detect() -> Result<Compositor> {
    // Variants are Linux backends. Off Linux, do not fall through to the XDG
    // sniff: `mgmt/display.rs` puts that error verbatim on `/display/monitors`.
    // The operator pin is gated with it — a Wayland name cannot be honoured here.
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
        // [`ENV_LOCK`]: another session's `apply_session_env` may `set_var`/`remove_var`
        // this key. glibc `getenv` concurrent with `setenv` is UB even on a different
        // variable. Read-then-drop: only the read needs serializing.
        let desktop = with_env_lock(|| std::env::var("XDG_CURRENT_DESKTOP"))
            .unwrap_or_default()
            .to_ascii_uppercase();
        compositor_from_xdg(&desktop)
    }
}

/// Error for a `PUNKTFUNK_COMPOSITOR` value that names no backend.
///
/// `cinnamon`/`muffin` get their own answer: the bare list invites `mutter`
/// (Muffin is a Mutter fork), which then fails inside `org.gnome.Mutter.ScreenCast`.
/// There is no working desktop value; name the gamescope route instead.
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

/// Last-resort `XDG_CURRENT_DESKTOP` sniff as a pure function of the (uppercased)
/// value so the error branches are testable without mutating process env.
/// Called only by [`detect`] after pin and live-session detection are empty.
#[cfg(target_os = "linux")]
fn compositor_from_xdg(desktop: &str) -> Result<Compositor> {
    // CINNAMON before GNOME is load-bearing: a session advertising both would
    // match GNOME, get Mutter, and fail inside `org.gnome.Mutter.ScreenCast`.
    // The more specific desktop wins.
    if desktop.contains("CINNAMON") {
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

/// Attach-only probes: while held, `create` may only attach to a live output
/// and must not stop, relaunch, or take over a box session. Capture-loss rebuild
/// holds one while active-session detection can still be stale. A counter so
/// overlapping scopes compose.
static REBUILD_PROBES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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

/// True while any [`rebuild_probe_scope`] is live. Skip stop/relaunch/takeover-restart.
pub fn rebuild_probe_active() -> bool {
    REBUILD_PROBES.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// Monitor to mirror instead of creating a virtual display, or `None`.
///
/// **`PUNKTFUNK_CAPTURE_MONITOR` wins over stored policy** — an appliance pin
/// in `host.env` must not be overridden by a console click. Unset, the
/// persisted picker applies (the env is snapshotted at startup, so a UI
/// cannot write it). Read per `open`, not cached, so a console change takes
/// effect on the next session.
pub fn capture_monitor() -> Option<String> {
    if let Some(env) = pf_host_config::config().capture_monitor.as_deref() {
        return Some(env.to_string());
    }
    policy::prefs().get().capture_monitor
}

/// Open the virtual-display driver for `compositor`.
///
/// A [`capture_monitor`] pin routes to the mirror backend (physical head, no
/// virtual display). Resolved here — the one place every session opens a
/// display — so the pin is host-wide. `design/per-monitor-portal-capture.md`.
pub fn open(compositor: Compositor) -> Result<Box<dyn VirtualDisplay>> {
    #[cfg(target_os = "linux")]
    if let Some(connector) = capture_monitor() {
        // Empty heads (nested/headless) degrade to virtual-display so a persisted
        // pin cannot refuse to stream. A miss among existing heads stays a hard
        // error; an enumeration error stays on the mirror path.
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
        // Sole backend is the IddCx driver; `compositor` is unused on Windows.
        let _ = compositor;
        // `ensure_available` waits out a D0 re-register (wake-from-sleep) and
        // reloads a hostless-zombie adapter (devnode present, interface gone).
        // `.context` appends; a replacement "not installed" hid mid-resume.
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

/// Mirror backend for an explicit head, bypassing [`open`]'s
/// `PUNKTFUNK_CAPTURE_MONITOR` pin. `pf_host_config` snapshots env at
/// startup, so a tool cannot set the pin for itself.
#[cfg(target_os = "linux")]
pub fn open_mirror(compositor: Compositor, connector: &str) -> Result<Box<dyn VirtualDisplay>> {
    Ok(Box::new(mirror::MirrorDisplay::new(
        compositor,
        connector.to_string(),
    )?))
}

/// Is `compositor` up and able to create a virtual output right now?
/// KWin checks the privileged `zkde_screencast` global. The others have no
/// equivalent pre-flight; `Ok(())` means try `create`.
pub fn probe(compositor: Compositor) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        match compositor {
            Compositor::Kwin => kwin::probe(),
            // `hyprctl` must reach the compositor, plus the permission-system warning.
            Compositor::Hyprland => hyprland::probe(),
            // Spawn / D-Bus / output-on-demand: nothing to pre-check beyond "Linux".
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

// Management policy (keep-alive / topology / conflict / identity / layout).
// Platform-neutral — mgmt API and both host paths read it — so no cfg gate.
// `design/display-management.md`.
#[path = "vdisplay/policy.rs"]
pub mod policy;

// Physical heads the compositor already has — not ours. Platform-neutral
// facade; per-backend reads live beside each dialect.
// `design/per-monitor-portal-capture.md`.
#[path = "vdisplay/monitors.rs"]
pub mod monitors;

// Stream a head the compositor already has. `VirtualDisplay` so session
// machinery is unchanged; `DisplayOwnership::External` so lifecycle policy
// is not applied to someone else's monitor.
#[cfg(target_os = "linux")]
#[path = "vdisplay/mirror.rs"]
mod mirror;

// Pure lifecycle state machine (refcount + linger + pin). Registry executes the transitions.
#[path = "vdisplay/lifecycle.rs"]
pub(crate) mod lifecycle;

// Snapshot/release facade over per-OS lifecycle owners for /display/state and /display/release.
#[path = "vdisplay/registry.rs"]
pub mod registry;

// Arrangement engine (auto-row / manual → positions). Registry readout and KWin apply consume it.
#[path = "vdisplay/layout.rs"]
pub(crate) mod layout;

/// Concrete topology (never [`policy::Topology::Auto`]). `Auto` is **extend**
/// under a `PUNKTFUNK_COMPOSITOR` pin (host is not the sole desktop), else
/// **exclusive** (promote the virtual output to sole).
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

/// Topology applied at create. Precedence: configured console policy, else
/// legacy `PUNKTFUNK_{KWIN,MUTTER}_VIRTUAL_PRIMARY` (`1`→exclusive, `0`→extend),
/// else [`resolve_topology`] of `Auto`. Always concrete.
pub fn effective_topology() -> policy::Topology {
    if let Some(e) = policy::prefs().configured_effective() {
        return resolve_topology(e.topology);
    }
    // Legacy env if present, else Auto. [`ENV_LOCK`]: this runs inside `create`,
    // concurrent with another session's `apply_session_env`; racing `getenv` is
    // UB even though nobody writes these keys.
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

// Per-compositor backends under `vdisplay/linux/` and `vdisplay/windows/`.
// `#[path]` keeps the `crate::*` module names flat.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/gamescope.rs"]
mod gamescope;

/// Hidden `gamescope-splash` subcommand: X11 client backgrounded beside the
/// nested app so the compositor composites (and PipeWire delivers) before
/// Steam paints. Blocks for the session; gamescope's reaper tears it down.
/// `vdisplay/linux/gamescope/splash.rs`.
#[cfg(target_os = "linux")]
pub fn gamescope_splash_client() -> anyhow::Result<()> {
    gamescope::splash_run()
}

/// Can a gamescope session on this host stream 10-bit BT.2020 PQ?
///
/// Settled **before spawn** — punktfunk/1 Welcome fixes bit depth and cannot
/// take it back. Both terms must hold: the resolved binary offers 10-bit PQ
/// capture (`--version` banner, once per process), and this host **spawns**
/// gamescope rather than attaching (`PUNKTFUNK_GAMESCOPE_NODE` inherits
/// someone else's flags). A host-managed `gamescope-session-plus` / SteamOS
/// session counts as a spawn: we own `GAMESCOPE_BIN`.
pub fn gamescope_hdr_available() -> bool {
    gamescope_ours_and(
        #[cfg(target_os = "linux")]
        gamescope::gamescope_hdr_capable,
    )
}

/// Will gamescope paint the pointer into its PipeWire node (no XFixes blend)?
///
/// The blend forces encode onto the compute colour-conversion arm — the
/// zero-copy RGB-direct front end has no blend stage. Settled before
/// `SessionPlan::cursor_blend` opens the encoder. Same two terms as
/// [`gamescope_hdr_available`]: patched binary, and this host spawns it.
pub fn gamescope_composites_cursor() -> bool {
    gamescope_ours_and(
        #[cfg(target_os = "linux")]
        gamescope::gamescope_can_composite_cursor,
    )
}

/// Shared half of the HDR/cursor answers: this host must **spawn** the
/// session (an attach inherits someone else's flags), then `probe` the binary.
///
/// Ask the resolved **route**, never `PUNKTFUNK_GAMESCOPE_NODE` — that key is
/// an operator override, not the published decision. [`GamescopeRoute::Attach`]
/// and the monitor-pin mirror would otherwise answer "ours".
///
/// The ladder is re-run with `dedicated_launch = false` (no session context),
/// and `create_managed_session` can still degrade `Managed` to Attach after
/// this answer is due. Do not guess `dedicated_launch = true`: over-promising
/// 10-bit PQ / a composited cursor is unrecoverable. Under-promise plus
/// `gamescope::cursor_args` (binary probe, ungated) can double-draw the
/// pointer; that is the cheaper failure. Close both gaps by taking the
/// session's own [`GamescopeRoute`].
fn gamescope_ours_and(#[cfg(target_os = "linux")] probe: fn() -> bool) -> bool {
    #[cfg(target_os = "linux")]
    {
        // `probe` first: memoized `--version`. Route resolution walks `/proc`;
        // a stock gamescope is already `false` and skips the walk.
        probe()
            && !session_is_a_foreign_gamescope(
                capture_monitor().is_some(),
                resolve_gamescope_route(Compositor::Gamescope, false).as_ref(),
            )
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Is the gamescope this session will use one somebody else started?
///
/// Foreign if `mirror_pinned` ([`open`] attaches to the running session's
/// node without the sub-mode ladder) or [`GamescopeRoute::Attach`].
/// [`GamescopeRoute::Managed`] is not: takeover starts through our
/// `GAMESCOPE_BIN`, so the flags are ours.
///
/// `mirror_pinned` is the pin, not whether mirror actually took — [`open`]
/// degrades a pin with no heads to a bare spawn, and that session is still
/// called foreign here. Fail-closed: do not enumerate heads from a
/// capability query that must answer before anything exists to ask.
fn session_is_a_foreign_gamescope(mirror_pinned: bool, route: Option<&GamescopeRoute>) -> bool {
    mirror_pinned || matches!(route, Some(GamescopeRoute::Attach { .. }))
}

// Per-client stable display-id map (EDID serial / KWin output name / Mutter ScaleMap).
// `allow(dead_code)` covers helpers no current backend reaches; re-test without
// it on Linux, where dead_code is enforced.
#[allow(dead_code)]
#[path = "vdisplay/identity.rs"]
pub(crate) mod identity;

// Mode-conflict admission (separate/join/steal/reject) plus the live-session registry.
#[path = "vdisplay/admission.rs"]
pub mod admission;

/// In-place edit of the user's xdg-desktop-portal config (wlr-family needs
/// one key in a file the user owns). Unconditional so merge tests run on
/// every platform's CI, not only where the Linux callers compile.
#[path = "vdisplay/linux/portal_config.rs"]
mod portal_config;

/// ScreenCast cursor mode to request, negotiated against `AvailableCursorModes`.
/// A mode the backend does not advertise closes the session. Unconditional
/// so the ladder's tests run without a compositor, on every CI.
#[path = "vdisplay/linux/portal_cursor.rs"]
mod portal_cursor;

/// Line fed to xdph's custom picker to select an output headlessly.
/// Unconditional: wire format has no schema and no error report; parser
/// tests are the only place a malformed line is visible.
#[path = "vdisplay/linux/portal_picker.rs"]
mod portal_picker;

/// Never-dropped tokio runtime for portal handshakes. Outlives ashpd's
/// process-global cached D-Bus connection; only Linux backends speak to it.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/portal_rt.rs"]
mod portal_rt;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/hyprland.rs"]
mod hyprland;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin.rs"]
mod kwin;

// In-process `kde_output_management_v2` topology. Avoids a stuck
// libkscreen/kscreen-KDED `kscreen-doctor`. `kwin` consumes it, kscreen fallback.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin_output_mgmt.rs"]
mod kwin_output_mgmt;

// DPMS of the box's own panels: gamescope honors `Topology::Exclusive` with
// no desktop output. GNOME cannot (Mutter has no client DPMS). Outputs cannot
// be disabled (KWin refuses zero enabled); DPMS-off is the translation.
// Refcounted; `gamescope` consumes it, best-effort.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/panel_dpms.rs"]
mod panel_dpms;

// Direct DRM CRTC-off when there is no desktop to ask (Game Mode). Reached
// from `panel_dpms`'s non-KDE arm, which owns the refcount and the hold.
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/drm_dpms.rs"]
mod drm_dpms;

#[cfg(target_os = "windows")]
#[path = "vdisplay/windows/manager.rs"]
pub mod manager;

// DDC/CI panel power: Windows manager blanks/wakes the box's real panels around a session.
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

    /// `mgmt/display.rs` puts this error verbatim on `/display/monitors`;
    /// the wording is a user-facing surface.
    #[cfg(target_os = "linux")]
    #[test]
    fn xdg_sniff_maps_known_desktops() {
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

    /// Muffin has no virtual-output API: there is no `PUNKTFUNK_COMPOSITOR`
    /// that makes Cinnamon host a virtual display. The error must name gamescope.
    #[cfg(target_os = "linux")]
    #[test]
    fn cinnamon_is_told_to_use_gamescope_not_to_pick_a_backend() {
        // Mint/LMDE set `X-Cinnamon`; the GNOME-compatibility token must not win.
        for v in ["X-CINNAMON", "CINNAMON", "X-CINNAMON:GNOME-FLASHBACK"] {
            let err = compositor_from_xdg(v)
                .expect_err("Cinnamon cannot host a virtual display")
                .to_string();
            assert!(err.contains("gamescope"), "no gamescope route named: {err}");
            assert!(err.contains("Muffin"), "does not say why: {err}");
        }
    }

    /// An explicit `cinnamon`/`muffin` pin must not list `mutter` (Muffin is a
    /// Mutter fork). A typo still gets the ordinary accepted-values list.
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

    /// Unknown desktop keeps the generic advice; the Cinnamon arm must not swallow it.
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
        // No live session → no backend; the caller maps this to handshake error / fallback.
        assert_eq!(compositor_for_kind(ActiveKind::None), None);
    }

    /// Spawn-vs-attach for [`gamescope_hdr_available`] /
    /// [`gamescope_composites_cursor`]. Both answers are irrevocable once
    /// punktfunk/1 Welcome has gone out; an over-promise is not recoverable.
    #[test]
    fn only_a_session_we_start_can_promise_gamescope_capabilities() {
        // Attach is somebody else's session, whatever rung reached it.
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
        // Spawn and Managed start through our GAMESCOPE_BIN; flags are ours.
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
        // No route = not a gamescope session; the binary probe alone decides.
        assert!(!session_is_a_foreign_gamescope(false, None));
        // Monitor pin bypasses the ladder (mirror attaches to the running node).
        assert!(session_is_a_foreign_gamescope(true, None));
        assert!(session_is_a_foreign_gamescope(
            true,
            Some(&GamescopeRoute::Spawn)
        ));
    }

    #[test]
    fn detect_active_session_is_side_effect_free_and_terminates() {
        // /proc + runtime-dir probe: must not panic; CI has no session → ActiveKind::None.
        let a = detect_active_session();
        // Runtime-dir anchor is XDG; Windows has no equivalent.
        #[cfg(target_os = "linux")]
        assert!(!a.env.xdg_runtime_dir.is_empty());
        // Wayland sockets are resolved only for Wayland-protocol desktops.
        if matches!(
            a.kind,
            ActiveKind::Gaming | ActiveKind::DesktopGnome | ActiveKind::None
        ) {
            assert!(a.env.wayland_display.is_none());
        }
    }
}
