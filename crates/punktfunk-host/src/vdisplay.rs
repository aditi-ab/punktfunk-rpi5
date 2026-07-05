//! Virtual display orchestration (plan §6) — the project's differentiator.
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
//! consumes the node via [`crate::capture::capture_virtual_output`].

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;
pub use punktfunk_core::Mode;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

/// A created virtual output: a PipeWire source to capture, plus an owned keepalive whose drop
/// tears the output down (releases the compositor-side resource).
///
/// Allowed dead on non-Linux: the backends that construct it are all `cfg(target_os = "linux")`.
#[allow(dead_code)]
pub struct VirtualOutput {
    /// PipeWire node id of the output's screencast stream.
    pub node_id: u32,
    /// Portal/remote PipeWire fd when the node lives on a sandboxed remote (e.g. Mutter's
    /// RemoteDesktop+ScreenCast). `None` means the node is on the user's default PipeWire daemon
    /// (KWin `zkde_screencast`), captured by connecting to that daemon directly.
    #[cfg(target_os = "linux")]
    pub remote_fd: Option<OwnedFd>,
    /// `(width, height, refresh_hz)` to prefer in the PipeWire format negotiation. KWin and
    /// gamescope outputs are created at the exact size, so this just confirms it; **Mutter sizes
    /// its virtual monitor FROM the negotiation**, so here it's what makes the client's mode real.
    pub preferred_mode: Option<(u32, u32, u32)>,
    /// Windows capture identity (DXGI adapter LUID + GDI output name) for the SudoVDA backend —
    /// what [`crate::capture::capture_virtual_output`] needs to duplicate the right output.
    #[cfg(target_os = "windows")]
    pub win_capture: Option<crate::capture::dxgi::WinCaptureTarget>,
    /// Keeps the output — and whatever connection/thread backs it — alive; dropped on teardown.
    pub keepalive: Box<dyn Send>,
}

/// Pluggable virtual-output creation, per compositor.
pub trait VirtualDisplay: Send {
    /// Human-readable backend name (e.g. `"kwin"`, `"wlroots"`, `"mutter"`).
    fn name(&self) -> &'static str;
    /// Create a virtual output of the given mode. Teardown is RAII: drop the returned
    /// [`VirtualOutput`]'s `keepalive`.
    fn create(&mut self, mode: Mode) -> Result<VirtualOutput>;
    /// Set the per-session command this display should launch into its nested output (the resolved
    /// app/game). Carried on the backend instance — NOT a process-global env var — so concurrent
    /// sessions can't stomp each other's launch target. Default: no-op (backends that attach to an
    /// existing session / don't spawn a nested command ignore it; only gamescope's spawn path uses it).
    fn set_launch_command(&mut self, _cmd: Option<String>) {}
    /// Set the connecting client's cert fingerprint so the backend can give that client a STABLE virtual
    /// monitor identity across reconnects — Windows then reapplies the client's saved per-monitor config
    /// (notably DPI scaling). Carried on the backend instance; set once before [`create`](Self::create).
    /// Default: no-op — only the Windows pf-vdisplay backend uses it (Linux compositors own their virtual
    /// output identity). `None` = anonymous/unpaired/GameStream → the backend's auto (slot-based) identity.
    fn set_client_identity(&mut self, _fingerprint: Option<[u8; 32]>) {}
    /// The stable identity slot the backend resolved for the most recent [`create`](Self::create) —
    /// the per-client id the identity policy assigned (`Some`), or `None` for shared/anonymous. The
    /// registry reads it right after `create` to key the display's group **arrangement** (manual
    /// per-slot positions) and to label the mgmt `/display/state` slot. Default `None`: a backend
    /// with no per-client identity (Mutter/wlroots/gamescope) always auto-rows. Only KWin (per-slot
    /// output naming) reports a real slot on Linux.
    fn last_identity_slot(&self) -> Option<u32> {
        None
    }
    /// Place the most-recently-[created](Self::create) output at `(x, y)` in the desktop coordinate
    /// space (design `display-management.md` §6.2 — layout). The registry, which owns the display
    /// **group**, computes the position from the whole group (auto-row or the console's manual
    /// arrangement) and calls this right after `create`. Default no-op: only backends that can position
    /// an output (KWin) implement it; the registry never calls it for the desktop origin `(0, 0)`, so a
    /// single-display / first-of-group session issues no positioning at all. Best-effort — a failure
    /// leaves the compositor's default placement.
    fn apply_position(&mut self, _x: i32, _y: i32) {}
    /// Take the topology **restore** action this [`create`](Self::create) prepared — the work that
    /// un-does an `exclusive`/`primary` topology change (e.g. re-enable the physical outputs KWin
    /// disabled). The registry lifts it into the display **group** so it runs **once, when the group's
    /// last display is torn down** (design §6.1 — per-group restore), not when this one session's
    /// display drops: a sibling `exclusive` session must not have the physical re-enabled under it.
    /// Called right after `create`; the backend must not also run it itself. Default `None` — a backend
    /// whose topology auto-reverts (Mutter `APPLY_TEMPORARY`) or that changes nothing has nothing to
    /// hand off.
    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
    /// Tell the backend whether this create will be the **first** display in its group — i.e. no
    /// sibling of the same backend is already live (design §6.1). A backend that *establishes* the
    /// group's topology (Mutter's sole-monitor `exclusive` `ApplyMonitorsConfig`) applies it only when
    /// first; a later sibling **extends** into the already-exclusive desktop instead of re-clobbering it
    /// (a fresh sole-monitor config would disable the first session's virtual output). Set by the
    /// registry right before [`create`](Self::create). Default no-op: KWin recognises siblings at
    /// runtime by output name (first-slot-wins + a group-aware disable filter), and single-display
    /// backends never have a sibling.
    fn set_first_in_group(&mut self, _first: bool) {}
}

/// Compositors punktfunk knows how to drive (plan §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    /// KWin / Plasma 6 — `zkde_screencast` virtual output.
    Kwin,
    /// wlroots (Sway/Hyprland) — headless `create_output`.
    Wlroots,
    /// Mutter / GNOME — headless backend + Mutter DBus `RecordVirtual`.
    Mutter,
    /// gamescope — spawned headless at the client's size/refresh; capture its PipeWire node.
    Gamescope,
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
        }
    }

    /// Human label for UIs.
    pub fn label(self) -> &'static str {
        match self {
            Compositor::Kwin => "KWin / KDE Plasma",
            Compositor::Wlroots => "wlroots (Sway / Hyprland)",
            Compositor::Mutter => "Mutter / GNOME",
            Compositor::Gamescope => "gamescope",
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
    pub fn all() -> [Compositor; 4] {
        [
            Compositor::Kwin,
            Compositor::Gamescope,
            Compositor::Mutter,
            Compositor::Wlroots,
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
        v
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// The kind of graphical session live for our uid *right now* — the basis for per-connect backend
/// selection on a box that flips between Steam Gaming Mode and a KDE/GNOME desktop (Bazzite,
/// SteamOS). Detected by probing which compositor process is actually running, not by a static
/// env var, so the host follows the box as the user switches sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveKind {
    /// A `gamescope` session is live (Steam Gaming Mode / `gamescope-session-plus`).
    Gaming,
    /// A KWin / Plasma desktop is live.
    DesktopKde,
    /// A GNOME / Mutter desktop is live.
    DesktopGnome,
    /// A wlroots (Sway / Hyprland) desktop is live.
    DesktopWlroots,
    /// No recognized graphical session is running for our uid.
    None,
}

/// The session environment that points a backend at the [detected](detect_active_session) active
/// session: the Wayland socket (for the Wayland-protocol backends), the runtime dir + session bus
/// (for PipeWire capture + D-Bus / portal input), and the desktop name (for portal routing). The
/// host serves one session at a time, so [`apply_session_env`] writes these into the process env
/// per connect and every backend that reads them then opens against the live session.
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` of the live compositor (`None` for Gaming-attach / Mutter, which are
    /// PipeWire-node / D-Bus driven and don't talk Wayland to us).
    pub wayland_display: Option<String>,
    /// `/run/user/<uid>` — the trustworthy anchor (the default PipeWire daemon + bus live here).
    pub xdg_runtime_dir: String,
    /// `DBUS_SESSION_BUS_ADDRESS` (defaults to `unix:path=<runtime>/bus`).
    pub dbus_session_bus_address: String,
    /// `XDG_CURRENT_DESKTOP` to advertise (KDE/GNOME/sway/gamescope) — drives portal/EIS routing.
    pub xdg_current_desktop: Option<String>,
}

/// The live session: its [`ActiveKind`] plus the [`SessionEnv`] to target it.
pub struct ActiveSession {
    pub kind: ActiveKind,
    pub env: SessionEnv,
}

impl ActiveSession {
    /// A "nothing live" result carrying just the runtime-dir anchor.
    fn none() -> ActiveSession {
        ActiveSession {
            kind: ActiveKind::None,
            env: SessionEnv {
                xdg_runtime_dir: default_runtime_dir(),
                dbus_session_bus_address: default_bus(&default_runtime_dir()),
                ..Default::default()
            },
        }
    }
}

/// The concrete backend that drives a given live-session kind. `None` for [`ActiveKind::None`].
pub fn compositor_for_kind(kind: ActiveKind) -> Option<Compositor> {
    match kind {
        ActiveKind::Gaming => Some(Compositor::Gamescope),
        ActiveKind::DesktopKde => Some(Compositor::Kwin),
        ActiveKind::DesktopGnome => Some(Compositor::Mutter),
        ActiveKind::DesktopWlroots => Some(Compositor::Wlroots),
        ActiveKind::None => None,
    }
}

#[cfg(target_os = "linux")]
fn default_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        // SAFETY: `getuid()` is a parameterless POSIX call that always succeeds and touches no
        // memory — it just returns the calling process's real uid. Nothing is aliased or freed.
        let uid = unsafe { libc::getuid() };
        format!("/run/user/{uid}")
    })
}
#[cfg(not(target_os = "linux"))]
fn default_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_default()
}

fn default_bus(runtime: &str) -> String {
    std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| format!("unix:path={runtime}/bus"))
}

/// Detect the graphical session live for our uid right now (cheap, side-effect-free: a `/proc`
/// scan plus a runtime-dir socket scan — well under the handshake timeout). The authority is the
/// running compositor process; a desktop compositor outranks a lingering gamescope. Used to route
/// each connect to the correct backend, and to derive the [`SessionEnv`] that targets it.
#[cfg(target_os = "linux")]
pub fn detect_active_session() -> ActiveSession {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `getuid()` is a parameterless POSIX call that always succeeds and touches no memory —
    // it just returns the calling process's real uid. Nothing is aliased or freed.
    let uid = unsafe { libc::getuid() };
    let xdg_runtime_dir = default_runtime_dir();
    let dbus = default_bus(&xdg_runtime_dir);

    // Process probe: the running graphical compositor of THIS uid decides the kind. Priority lets
    // a real desktop (kwin/gnome/sway) win over a leftover gamescope child. comm names mirror the
    // `pkill -x` discipline (exact, ≤15 chars so untruncated).
    let mut kind = ActiveKind::None;
    let mut best = 0u8;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let pid_path = e.path();
            let Ok(md) = std::fs::metadata(&pid_path) else {
                continue;
            };
            if md.uid() != uid {
                continue;
            }
            let Ok(comm) = std::fs::read_to_string(pid_path.join("comm")) else {
                continue;
            };
            let (k, prio) = match comm.trim() {
                "gamescope" | "gamescope-wl" => (ActiveKind::Gaming, 1),
                "kwin_wayland" => (ActiveKind::DesktopKde, 4),
                "gnome-shell" => (ActiveKind::DesktopGnome, 4),
                "sway" | "Hyprland" | "hyprland" | "river" => (ActiveKind::DesktopWlroots, 4),
                _ => continue,
            };
            if prio > best {
                best = prio;
                kind = k;
            }
        }
    }

    // Wayland-protocol backends (KWin, wlroots) need the live socket; Gaming-attach and Mutter are
    // node/D-Bus driven and don't.
    let wayland_display = match kind {
        ActiveKind::DesktopKde | ActiveKind::DesktopWlroots => {
            find_wayland_socket(&xdg_runtime_dir, uid)
        }
        _ => None,
    };
    let xdg_current_desktop = match kind {
        ActiveKind::DesktopKde => Some("KDE".to_string()),
        ActiveKind::DesktopGnome => Some("GNOME".to_string()),
        ActiveKind::DesktopWlroots => Some("sway".to_string()),
        ActiveKind::Gaming => Some("gamescope".to_string()),
        ActiveKind::None => None,
    };
    ActiveSession {
        kind,
        env: SessionEnv {
            wayland_display,
            xdg_runtime_dir,
            dbus_session_bus_address: dbus,
            xdg_current_desktop,
        },
    }
}

#[cfg(not(target_os = "linux"))]
pub fn detect_active_session() -> ActiveSession {
    ActiveSession::none()
}

/// Find the live `wayland-*` socket in `runtime` for our uid (skipping `.lock` sidecars). Trust a
/// valid inherited `WAYLAND_DISPLAY` first; otherwise take the newest-mtime socket we own (a
/// desktop session normally exposes exactly one).
#[cfg(target_os = "linux")]
fn find_wayland_socket(runtime: &str, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(w) = std::env::var("WAYLAND_DISPLAY") {
        if !w.is_empty() {
            let p = if w.starts_with('/') {
                std::path::PathBuf::from(&w)
            } else {
                std::path::Path::new(runtime).join(&w)
            };
            if p.exists() {
                return Some(w);
            }
        }
    }
    let mut cands: Vec<(std::time::SystemTime, String)> = Vec::new();
    for e in std::fs::read_dir(runtime).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("wayland-") || name.ends_with(".lock") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.uid() != uid {
            continue;
        }
        let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
        cands.push((mtime, name));
    }
    cands.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    cands.into_iter().next().map(|(_, n)| n)
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

/// Write a detected session's [`SessionEnv`] into the process env so every backend (video capture
/// and input alike) that reads `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` /
/// `XDG_CURRENT_DESKTOP` at open time targets the live session. Serialized via [`ENV_LOCK`] so
/// concurrent session handshakes can't race the `set_var`s; the next connect re-detects and
/// re-applies.
#[cfg(target_os = "linux")]
pub fn apply_session_env(active: &ActiveSession) {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let e = &active.env;
    std::env::set_var("XDG_RUNTIME_DIR", &e.xdg_runtime_dir);
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &e.dbus_session_bus_address);
    if let Some(w) = &e.wayland_display {
        std::env::set_var("WAYLAND_DISPLAY", w);
    }
    if let Some(d) = &e.xdg_current_desktop {
        std::env::set_var("XDG_CURRENT_DESKTOP", d);
    }
    // Mutter on NVIDIA has no working dmabuf capture sync — force SHM there; the KWin/gamescope
    // tiled/LINEAR paths keep zero-copy.
    if active.kind == ActiveKind::DesktopGnome {
        std::env::set_var("PUNKTFUNK_FORCE_SHM", "1");
    }
    // Topology (Stage 2): the per-compositor backends (KWin/Mutter) now read
    // [`effective_topology`] directly at create time — the console policy, else the legacy
    // `PUNKTFUNK_{KWIN,MUTTER}_VIRTUAL_PRIMARY` env, else the Auto default (exclusive on the
    // auto-desktop path). So this connect-path no longer writes that env (one fewer process-env
    // mutation on the `ENV_LOCK` surface); `effective_topology()` computes the identical result.
}
#[cfg(not(target_os = "linux"))]
pub fn apply_session_env(_active: &ActiveSession) {}

/// On a **mid-stream** switch to a desktop, the xdg-desktop-portal (D-Bus-activated) and the systemd
/// `--user` environment can still point at the OLD session, so the host's RemoteDesktop portal opens
/// against a half-stale env — it accepts events but they don't reach the compositor until a
/// reconnect. Push the live session env into the systemd/D-Bus activation environment and (for KWin,
/// whose input rides the xdg RemoteDesktop portal) restart the portal so it re-reads it — the same
/// settling a fresh desktop login does. Best-effort; mirrors the wlroots portal restart. GNOME uses
/// Mutter's *direct* EIS (no xdg portal), so it only needs the env push.
#[cfg(target_os = "linux")]
pub fn settle_desktop_portal(chosen: Compositor) {
    const VARS: &[&str] = &[
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
    ];
    // Push our (correct) env into the systemd --user manager + the D-Bus activation environment so a
    // re-activated portal/backend inherits the live session.
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "import-environment"])
        .args(VARS)
        .status();
    let _ = std::process::Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(VARS)
        .status();
    // KWin input goes through the xdg RemoteDesktop portal; the frontend routes RemoteDesktop to a
    // backend by its OWN startup XDG_CURRENT_DESKTOP, so restart it (+ the KDE backend) to re-read
    // the now-live session, then let it settle before the injector reopens against it.
    if chosen == Compositor::Kwin {
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "try-restart",
                "xdg-desktop-portal-kde.service",
                "xdg-desktop-portal.service",
            ])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    tracing::info!(
        compositor = chosen.id(),
        "settled desktop portal env for the switched-to session"
    );
}
#[cfg(not(target_os = "linux"))]
pub fn settle_desktop_portal(_chosen: Compositor) {}

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
pub fn apply_input_env(chosen: Compositor) {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = match chosen {
        Compositor::Gamescope => "gamescope",
        // KWin: org_kde_kwin_fake_input — direct injection, no RemoteDesktop portal / approval
        // dialog (headless, the krdpserver path), authorized by the host's shipped .desktop.
        Compositor::Kwin => "kwin",
        // GNOME has neither fake_input nor the wlr protocols → RemoteDesktop portal via libei.
        Compositor::Mutter => "libei",
        Compositor::Wlroots => "wlr",
    };
    std::env::set_var("PUNKTFUNK_INPUT_BACKEND", backend);
    if chosen == Compositor::Gamescope {
        let mode = pick_gamescope_mode(
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
pub fn apply_input_env(_chosen: Compositor) {}

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

/// Detect the compositor to drive: explicit `PUNKTFUNK_COMPOSITOR` override (legacy / CI / forcing
/// a backend for a test), else the **live session** ([`detect_active_session`] — so a Bazzite box
/// follows Gaming↔Desktop switches), else a last-resort `XDG_CURRENT_DESKTOP` read.
pub fn detect() -> Result<Compositor> {
    if let Some(v) = crate::config::config().compositor.as_deref() {
        return match v.trim().to_ascii_lowercase().as_str() {
            "kwin" | "kde" | "plasma" => Ok(Compositor::Kwin),
            "wlroots" | "sway" | "hyprland" | "wlr" => Ok(Compositor::Wlroots),
            "mutter" | "gnome" => Ok(Compositor::Mutter),
            "gamescope" => Ok(Compositor::Gamescope),
            other => {
                anyhow::bail!(
                    "unknown PUNKTFUNK_COMPOSITOR '{other}' (kwin|wlroots|mutter|gamescope)"
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
    } else if desktop.contains("SWAY")
        || desktop.contains("WLROOTS")
        || desktop.contains("HYPRLAND")
    {
        Ok(Compositor::Wlroots)
    } else {
        anyhow::bail!(
            "could not detect compositor: no live graphical session for this uid and \
             XDG_CURRENT_DESKTOP='{desktop}'; set PUNKTFUNK_COMPOSITOR"
        )
    }
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
            pf_vdisplay::ensure_available(),
            "pf-vdisplay driver interface not found — the pf-vdisplay IddCx driver is not installed or \
             not loaded (the host installer bundles it; reinstall or check the driver state)"
        );
        Ok(Box::new(pf_vdisplay::PfVdisplayDisplay::new()?))
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
            // gamescope spawns its own nested session per `create`; Mutter is D-Bus on demand;
            // wlroots creates the output on demand — nothing to pre-check beyond "Linux".
            Compositor::Gamescope | Compositor::Mutter | Compositor::Wlroots => Ok(()),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = compositor;
        pf_vdisplay::probe()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux or Windows")
    }
}

/// Path of the file where the gamescope backend relays the nested session's `LIBEI_SOCKET`
/// (gamescope's EIS server) for the input injector. Under `$XDG_RUNTIME_DIR` (per-user 0700).
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file() -> std::path::PathBuf {
    gamescope::ei_socket_file()
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

// The user-configurable management policy (keep-alive / topology / conflict / identity / layout),
// layered above the per-compositor backends — platform-neutral (the mgmt API + both host paths read
// it), so no cfg gate. See `design/display-management.md`.
#[path = "vdisplay/policy.rs"]
pub(crate) mod policy;

// The pure per-display lifecycle state machine (refcount + linger + pin), platform-neutral and
// property-tested; the registry executes the side effects its transitions dictate.
#[path = "vdisplay/lifecycle.rs"]
pub(crate) mod lifecycle;

// The neutral snapshot/release facade over the per-OS lifecycle owners (Windows manager; Linux pool
// later), for the management API's /display/state + /display/release.
#[path = "vdisplay/registry.rs"]
pub(crate) mod registry;

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
            if crate::config::config().compositor.is_some() {
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
// backends under `vdisplay/windows/`; `#[path]` keeps the `crate::vdisplay::*` module names flat.
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
pub(crate) mod admission;
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/kwin.rs"]
mod kwin;
#[cfg(target_os = "windows")]
#[path = "vdisplay/windows/manager.rs"]
pub(crate) mod manager;
#[cfg(target_os = "linux")]
#[path = "vdisplay/linux/mutter.rs"]
mod mutter;
#[cfg(target_os = "windows")]
#[path = "vdisplay/windows/pf_vdisplay.rs"]
pub(crate) mod pf_vdisplay;
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
        // No live session → no backend; the caller turns this into a handshake error / fallback.
        assert_eq!(compositor_for_kind(ActiveKind::None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gamescope_mode_ladder() {
        use GamescopeMode::*;
        let pick = pick_gamescope_mode;
        // (force_managed, attach_env, node_env, session_env, managed_infra, foreign_gamescope)
        // Plain distro, nothing running: bare spawn — the path that nests the launch command.
        assert_eq!(pick(false, false, false, false, false, false), Spawn);
        // Bazzite/SteamOS (session infra present): managed, as validated live.
        assert_eq!(pick(false, false, false, false, true, false), Managed);
        assert_eq!(pick(false, false, false, false, true, true), Managed);
        // Foreign gamescope on an infra-less box: attach and mirror it.
        assert_eq!(pick(false, false, false, false, false, true), Attach);
        // Operator-set PUNKTFUNK_GAMESCOPE_SESSION keeps managed even without detected infra.
        assert_eq!(pick(false, false, false, true, false, false), Managed);
        // Explicit attach/node wins over infra…
        assert_eq!(pick(false, true, false, false, true, false), Attach);
        assert_eq!(pick(false, false, true, true, true, false), Attach);
        // …and force-managed wins over everything.
        assert_eq!(pick(true, true, true, false, false, false), Managed);
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
