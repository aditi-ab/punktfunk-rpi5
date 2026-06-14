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
/// is validated against this set before it's honored (see the m3 handshake's resolution).
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

/// Write a detected session's [`SessionEnv`] into the process env so every backend (video capture
/// and input alike) that reads `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` /
/// `XDG_CURRENT_DESKTOP` at open time targets the live session. The host serves one session at a
/// time, so a process-global write is sound; the next connect re-detects and re-applies. Same
/// `set_var` discipline already used for `PUNKTFUNK_GAMESCOPE_APP` on the launch path.
#[cfg(target_os = "linux")]
pub fn apply_session_env(active: &ActiveSession) {
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
}
#[cfg(not(target_os = "linux"))]
pub fn apply_session_env(_active: &ActiveSession) {}

/// Route input to match the chosen video backend (they must not diverge), via the highest-priority
/// `PUNKTFUNK_INPUT_BACKEND` knob the injector honors. For gamescope, the **default is a managed
/// session at the client's mode** (tears the TV's autologin down on connect; restored on a debounced
/// idle) — so the client gets ITS resolution (capture == encode == client mode), not the TV's, and a
/// quick reconnect reuses the warm session (no churn). Opt out to **attach** (mirror the running TV
/// session at its own mode, gaming stays live on the panel, no Steam restart) with
/// `PUNKTFUNK_GAMESCOPE_ATTACH`; an explicit `PUNKTFUNK_GAMESCOPE_NODE` also implies attach, and
/// `PUNKTFUNK_GAMESCOPE_MANAGED` forces managed over either.
#[cfg(target_os = "linux")]
pub fn apply_input_env(chosen: Compositor) {
    let backend = match chosen {
        Compositor::Gamescope => "gamescope",
        Compositor::Kwin | Compositor::Mutter => "libei",
        Compositor::Wlroots => "wlr",
    };
    std::env::set_var("PUNKTFUNK_INPUT_BACKEND", backend);
    if chosen == Compositor::Gamescope {
        let force_managed = std::env::var_os("PUNKTFUNK_GAMESCOPE_MANAGED").is_some();
        let attach = !force_managed
            && (std::env::var_os("PUNKTFUNK_GAMESCOPE_ATTACH").is_some()
                || std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_some());
        if attach {
            std::env::remove_var("PUNKTFUNK_GAMESCOPE_SESSION");
            if std::env::var_os("PUNKTFUNK_GAMESCOPE_NODE").is_none() {
                std::env::set_var("PUNKTFUNK_GAMESCOPE_NODE", "auto");
            }
        } else {
            if std::env::var_os("PUNKTFUNK_GAMESCOPE_SESSION").is_none() {
                std::env::set_var("PUNKTFUNK_GAMESCOPE_SESSION", "steam");
            }
            std::env::remove_var("PUNKTFUNK_GAMESCOPE_NODE");
        }
    }
}
#[cfg(not(target_os = "linux"))]
pub fn apply_input_env(_chosen: Compositor) {}

/// Detect the compositor to drive: explicit `PUNKTFUNK_COMPOSITOR` override (legacy / CI / forcing
/// a backend for a test), else the **live session** ([`detect_active_session`] — so a Bazzite box
/// follows Gaming↔Desktop switches), else a last-resort `XDG_CURRENT_DESKTOP` read.
pub fn detect() -> Result<Compositor> {
    if let Ok(v) = std::env::var("PUNKTFUNK_COMPOSITOR") {
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux (Wayland compositor)")
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux (Wayland compositor)")
    }
}

/// Path of the file where the gamescope backend relays the nested session's `LIBEI_SOCKET`
/// (gamescope's EIS server) for the input injector.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file() -> &'static str {
    gamescope::EI_SOCKET_FILE
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

#[cfg(target_os = "linux")]
mod gamescope;
#[cfg(target_os = "linux")]
mod kwin;
#[cfg(target_os = "linux")]
mod mutter;
#[cfg(target_os = "linux")]
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

    #[test]
    fn detect_active_session_is_side_effect_free_and_terminates() {
        // A pure probe of /proc + the runtime dir: it must not panic and must return promptly on
        // any box (CI has no graphical session → ActiveKind::None, with the runtime-dir anchor).
        let a = detect_active_session();
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
