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
//!
//! [`VirtualDisplay::create`] returns a [`VirtualOutput`]: the PipeWire node to capture plus an
//! owned keepalive whose `Drop` releases the output (RAII — no explicit `destroy`). Capture
//! consumes the node via the host `capture::capture_virtual_output`.

// Scaffold: some backend paths + Stage-3 identity are defined ahead of the target that uses them.
#![allow(dead_code)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

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
    apply_input_env, managed_session_available, restore_managed_session,
    restore_takeover_on_startup, start_restore_worker, wants_dedicated_game_session,
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
/// session's own compositor (KWin / Mutter / wlroots) when the host runs inside it. Cheap,
/// side-effect-free probes — safe to call per management request. A concrete client preference
/// is validated against this set before it's honored (see the punktfunk/1 handshake's resolution).
pub fn available() -> Vec<Compositor> {
    #[cfg(target_os = "linux")]
    {
        let mut v = Vec::new();
        if kwin::is_available() {
            v.push(Compositor::Kwin);
        }
        if gamescope::is_available() {
            v.push(Compositor::Gamescope);
        }
        if mutter::is_available() {
            v.push(Compositor::Mutter);
        }
        if wlroots::is_available() {
            v.push(Compositor::Wlroots);
        }
        if hyprland::is_available() {
            v.push(Compositor::Hyprland);
        }
        v
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
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
    if let Some(v) = pf_host_config::config().compositor.as_deref() {
        return match v.trim().to_ascii_lowercase().as_str() {
            "kwin" | "kde" | "plasma" => Ok(Compositor::Kwin),
            // `hyprland` names the distinct backend (D1); `wlroots`/`sway`/`wlr` stay wlroots-proper.
            "hyprland" | "hypr" => Ok(Compositor::Hyprland),
            "wlroots" | "sway" | "wlr" | "river" => Ok(Compositor::Wlroots),
            "mutter" | "gnome" => Ok(Compositor::Mutter),
            "gamescope" => Ok(Compositor::Gamescope),
            other => {
                anyhow::bail!(
                    "unknown PUNKTFUNK_COMPOSITOR '{other}' (kwin|wlroots|hyprland|mutter|gamescope)"
                )
            }
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(c) = compositor_for_kind(detect_active_session().kind) {
        return Ok(c);
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    if desktop.contains("KDE") {
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

/// Open the virtual-display driver for `compositor`.
pub fn open(compositor: Compositor) -> Result<Box<dyn VirtualDisplay>> {
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
        // `ensure_available` self-heals the hostless-zombie state a WUDFHost crash leaves (adapter
        // devnode present, interface gone): one device cycle + re-probe before giving up.
        anyhow::ensure!(
            driver::ensure_available(),
            "pf-vdisplay driver interface not found — the pf-vdisplay IddCx driver is not installed or \
             not loaded (the host installer bundles it; reinstall or check the driver state)"
        );
        Ok(Box::new(driver::PfVdisplayDisplay::new()?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux or Windows")
    }
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
    // most one of these is set), else the Auto default.
    let legacy = [
        "PUNKTFUNK_KWIN_VIRTUAL_PRIMARY",
        "PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok());
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

// Platform-neutral per-client stable display-id map (Stage 3): Windows seeds the monitor EDID +
// ConnectorIndex from the id; KWin names its output from it. `allow(dead_code)` because only Windows
// consumes it in non-test code today — the KWin wiring is the next Stage-3 step.
#[allow(dead_code)]
#[path = "vdisplay/identity.rs"]
pub(crate) mod identity;

// Platform-neutral mode-conflict admission (Stage 4): the separate/join/steal/reject decision + the
// live-session registry, wired into the punktfunk/1 handshake.
#[path = "vdisplay/admission.rs"]
pub mod admission;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/hyprland.rs"]
mod hyprland;

#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin.rs"]
mod kwin;

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
