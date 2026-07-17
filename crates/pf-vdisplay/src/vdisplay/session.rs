//! Live graphical-session detection + session-epoch + process-env retargeting (plan §W3 — the
//! self-contained subsystem carved out of [`super`]). Detects the active compositor/session
//! ([`detect_active_session`]), tracks the session epoch so pooled displays never outlive their
//! compositor instance, and retargets the process env at the live session ([`apply_session_env`],
//! [`settle_desktop_portal`]) under `super::ENV_LOCK`.

use super::*;

/// The **session epoch** — bumped whenever session detection observes a different compositor
/// *instance*: an [`ActiveKind`] change, **or** a new compositor PID for the same kind (the
/// Desktop→Game→Desktop bounce that brings up a fresh KWin/gamescope with an unrelated node-id space).
/// Pooled displays stamp the epoch at creation; the registry only reuses an entry whose epoch still
/// matches, and its linger timer reaps entries from dead epochs — so a switch can never hand back a
/// node id that now means nothing (`design/gamemode-and-dedicated-sessions.md` A4).
static SESSION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The current [session epoch](SESSION_EPOCH). Read by the registry at acquire (to stamp new entries
/// and gate reuse) and by its linger timer (to reap dead-epoch zombies).
pub fn session_epoch() -> u64 {
    SESSION_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Bump the [session epoch](SESSION_EPOCH) — call when session detection sees a new compositor
/// instance (kind change, or same-kind new PID). Returns the new value.
pub fn bump_session_epoch() -> u64 {
    SESSION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// The last-observed compositor instance `(kind, pid)`, so [`observe_session_instance`] can tell a
/// genuine instance change from a stable re-detect.
static LAST_INSTANCE: std::sync::Mutex<Option<(ActiveKind, Option<u32>)>> =
    std::sync::Mutex::new(None);

/// Observe the freshly-[detected](detect_active_session) live session and, if the compositor
/// *instance* changed since the last observation — a different [`ActiveKind`], **or** the same kind
/// with a new PID (a compositor restart / Desktop→Game→Desktop bounce) — bump the [session
/// epoch](SESSION_EPOCH) and [invalidate](registry::invalidate_backend) the previous backend's kept
/// displays, so a reconnect can never reuse a node id from the dead instance (A4). Idempotent per
/// instance; the first observation just records the baseline. Cheap on the steady state (one mutex
/// read); the registry lock is taken only on an actual change. Call from every site that detects the
/// session (the per-connect resolve, the mid-stream watcher, the capture-loss re-detect).
pub fn observe_session_instance(active: &ActiveSession) {
    let cur = (active.kind, active.compositor_pid);
    let mut last = LAST_INSTANCE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = *last {
        // Only a **desktop** compositor (KWin / Mutter / wlroots) instance change bumps the epoch +
        // invalidates its kept displays — its PipeWire node dies with the compositor. A **gamescope**
        // session (`ActiveKind::Gaming`) is NOT the epoch's subject: the box's game-mode / managed
        // gamescope isn't pooled, and dedicated **spawns** are independent nested sessions whose nodes
        // outlive any active-session change. So a game-mode gamescope restart, a Gaming↔Gaming winning-PID
        // flap (e.g. B1 stopping the autologin before a dedicated spawn), or a coexisting-gamescope set
        // change must NOT bump/invalidate — that would tear down a live/kept dedicated session (review
        // findings #6/#7/#10). Gate the whole action on a desktop kind being involved.
        if prev != cur && (is_desktop_kind(prev.0) || is_desktop_kind(cur.0)) {
            // Invalidate only the OLD backend, and only if it was a desktop compositor (never gamescope).
            if is_desktop_kind(prev.0) {
                if let Some(old) = compositor_for_kind(prev.0) {
                    registry::invalidate_backend(old.id());
                }
            }
            let epoch = bump_session_epoch();
            tracing::info!(
                from = ?prev.0,
                to = ?cur.0,
                epoch,
                "desktop compositor instance changed — session epoch bumped"
            );
        }
    }
    *last = Some(cur);
}

/// Is `kind` a **desktop** compositor (KWin / Mutter / wlroots) — one whose kept PipeWire outputs die
/// with the compositor instance, so the session epoch tracks it? `Gaming` (gamescope) and `None` are
/// not (gamescope spawns are independent nested sessions — see [`observe_session_instance`]).
fn is_desktop_kind(kind: ActiveKind) -> bool {
    matches!(
        kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
    )
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
    /// A wlroots-proper (Sway / River) desktop is live.
    DesktopWlroots,
    /// A Hyprland desktop is live (distinct from [`DesktopWlroots`](ActiveKind::DesktopWlroots):
    /// its own `hyprctl` IPC + xdph portal, though it shares the wlr virtual-input path).
    DesktopHyprland,
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
    /// `XDG_CURRENT_DESKTOP` to advertise (KDE/GNOME/sway/Hyprland/gamescope) — drives portal/EIS
    /// routing (xdph keys its Hyprland-specific behavior off `Hyprland`).
    pub xdg_current_desktop: Option<String>,
    /// `HYPRLAND_INSTANCE_SIGNATURE` of the live Hyprland instance (`Some` only for
    /// [`ActiveKind::DesktopHyprland`]). `hyprctl` needs it to reach the right instance socket;
    /// [`apply_session_env`] exports it so the systemd-`--user` host works without inheriting the
    /// session env (unlike sway's `SWAYSOCK`). `None` for every other compositor.
    pub hyprland_signature: Option<String>,
}

/// The live session: its [`ActiveKind`] plus the [`SessionEnv`] to target it.
pub struct ActiveSession {
    pub kind: ActiveKind,
    pub env: SessionEnv,
    /// PID of the winning compositor process (`None` when nothing live). The session watcher compares
    /// it across polls so a **same-kind** compositor restart (Desktop→Game→Desktop) bumps the session
    /// epoch — a fresh instance's node-id space is unrelated to the old one's (A4).
    pub compositor_pid: Option<u32>,
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
            compositor_pid: None,
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
        ActiveKind::DesktopHyprland => Some(Compositor::Hyprland),
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
    // The winning compositor's PID — kept so a same-kind compositor RESTART (a new PID) bumps the
    // session epoch (A4), not just a kind change.
    let mut winning_pid: Option<u32> = None;
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
                // Hyprland is its own backend (hyprctl + xdph) — split it out of the sway/river
                // wlroots-proper family (design/hyprland-support.md D1).
                "Hyprland" | "hyprland" => (ActiveKind::DesktopHyprland, 4),
                "sway" | "river" => (ActiveKind::DesktopWlroots, 4),
                _ => continue,
            };
            let pid = name.parse::<u32>().ok();
            if prio > best {
                best = prio;
                kind = k;
                winning_pid = pid;
            } else if prio == best {
                // Deterministic tie-break among same-top-priority processes: keep the LOWEST pid, so a
                // duplicate same-kind compositor (two `kwin_wayland`) can't make `winning_pid` flap with
                // `/proc` enumeration order — which `observe_session_instance` would misread as a
                // compositor restart and tear a live display down (re-review low-severity note).
                if let (Some(p), Some(w)) = (pid, winning_pid) {
                    if p < w {
                        kind = k;
                        winning_pid = Some(p);
                    }
                }
            }
        }
    }

    // Wayland-protocol backends (KWin, wlroots, Hyprland) need the live socket for input (the wlr
    // virtual pointer/keyboard client connects to it); Gaming-attach and Mutter are node/D-Bus
    // driven and don't.
    let wayland_display = match kind {
        ActiveKind::DesktopKde | ActiveKind::DesktopWlroots | ActiveKind::DesktopHyprland => {
            find_wayland_socket(&xdg_runtime_dir, uid)
        }
        _ => None,
    };
    let xdg_current_desktop = match kind {
        ActiveKind::DesktopKde => Some("KDE".to_string()),
        ActiveKind::DesktopGnome => Some("GNOME".to_string()),
        ActiveKind::DesktopWlroots => Some("sway".to_string()),
        // G4: advertise the real desktop so portal routing (portals.conf `[Hyprland]`) and xdph's
        // own Hyprland checks work — NOT the old blanket `sway`.
        ActiveKind::DesktopHyprland => Some("Hyprland".to_string()),
        ActiveKind::Gaming => Some("gamescope".to_string()),
        ActiveKind::None => None,
    };
    // Discover the Hyprland instance signature so `hyprctl` can reach the compositor even when the
    // host runs as a systemd `--user` service that never inherited the session env.
    let hyprland_signature = match kind {
        ActiveKind::DesktopHyprland => find_hypr_signature(&xdg_runtime_dir, uid),
        _ => None,
    };
    ActiveSession {
        kind,
        env: SessionEnv {
            wayland_display,
            xdg_runtime_dir,
            dbus_session_bus_address: dbus,
            xdg_current_desktop,
            hyprland_signature,
        },
        compositor_pid: winning_pid,
    }
}

/// Find the live Hyprland instance signature (`HYPRLAND_INSTANCE_SIGNATURE`) for our uid. Trust a
/// valid inherited value first (the host launched inside the session); otherwise pick the
/// newest-mtime instance directory under `$XDG_RUNTIME_DIR/hypr/` that we own and that still has a
/// live `.socket.sock` — the same "newest wins" heuristic as [`find_wayland_socket`]. A desktop
/// normally exposes exactly one. (Phase-2 refinement: match the instance to `compositor_pid` via
/// `hyprctl instances` when several coexist — `design/hyprland-support.md` §Phase-1.1.)
#[cfg(target_os = "linux")]
fn find_hypr_signature(runtime: &str, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let hypr = std::path::Path::new(runtime).join("hypr");
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if !sig.is_empty() && hypr.join(&sig).join(".socket.sock").exists() {
            return Some(sig);
        }
    }
    let mut cands: Vec<(std::time::SystemTime, String)> = Vec::new();
    for e in std::fs::read_dir(&hypr).ok()?.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_dir() || md.uid() != uid {
            continue;
        }
        if !e.path().join(".socket.sock").exists() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
        cands.push((mtime, name));
    }
    cands.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    cands.into_iter().next().map(|(_, n)| n)
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
    // Hyprland: export the discovered instance signature so `hyprctl` reaches the live compositor
    // (fixes G4 for the systemd `--user` host, which never inherited it). Only set when detection
    // found a Hyprland session; a stale value from a previous connect is cleared otherwise so a
    // Hyprland→sway switch can't leave `hyprctl` pointed at a dead instance.
    match &e.hyprland_signature {
        Some(sig) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", sig),
        None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
    }
    // NOTHING live ⇒ every session-scoped var still in the env is a leftover from a previous
    // connect's retarget, and the availability probes read them: after a gnome-shell crash
    // (observed 2026-07-10: SIGSEGV → GDM greeter) a stale `XDG_CURRENT_DESKTOP=GNOME` kept
    // `mutter::is_available()` true, so a client's explicit backend request routed into the dead
    // session — 45 s create timeouts and a libei error loop instead of the crisp "no live
    // graphical session" handshake error. Clear them so `available()` reports the truth and the
    // client fails fast (and, when configured, `try_recover_session` can bring the desktop back).
    if active.kind == ActiveKind::None {
        std::env::remove_var("XDG_CURRENT_DESKTOP");
        std::env::remove_var("WAYLAND_DISPLAY");
    }
    // Topology (Stage 2): the per-compositor backends (KWin/Mutter) now read
    // [`effective_topology`] directly at create time — the console policy, else the legacy
    // `PUNKTFUNK_{KWIN,MUTTER}_VIRTUAL_PRIMARY` env, else the Auto default (exclusive on the
    // auto-desktop path). So this connect-path no longer writes that env (one fewer process-env
    // mutation on the `ENV_LOCK` surface); `effective_topology()` computes the identical result.
}

#[cfg(not(target_os = "linux"))]
pub fn apply_session_env(_active: &ActiveSession) {}

/// Fire the operator's session-recovery hook (`PUNKTFUNK_RECOVER_SESSION_CMD`) because a client
/// connected while NO graphical session is live for this uid — the state a compositor crash
/// leaves behind (gnome-shell SIGSEGV → GDM greeter, whose auto-login only fires once per boot,
/// so the box would otherwise sit headless until a walk-up login or a reboot). The command runs
/// detached via `sh -c` (typically a display-manager restart — see the config docs) and is
/// debounced to one launch per minute so a retrying client can't stack restarts. Returns whether
/// a recovery is underway (just launched, or launched within the debounce window), letting the
/// handshake error tell the client to simply retry.
#[cfg(target_os = "linux")]
pub fn try_recover_session() -> bool {
    let Some(cmd) = pf_host_config::config().recover_session_cmd.clone() else {
        return false;
    };
    static LAST_LAUNCH: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(60);
    let mut last = LAST_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < DEBOUNCE) {
        return true; // a launch is already in flight — the retry lands in the recovered session
    }
    match std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            *last = Some(std::time::Instant::now());
            tracing::warn!(cmd = %cmd,
                "no live graphical session — launched the operator's session-recovery command");
            // Reap off-thread so the finished child never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(e) => {
            tracing::error!(cmd = %cmd, error = %e,
                "session-recovery command failed to launch");
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn try_recover_session() -> bool {
    false
}

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
    // Hyprland capture rides the xdg ScreenCast portal serviced by xdph (G5): on a mid-stream switch
    // xdph may still hold the old session's Wayland/instance env, so restart it (+ the frontend) to
    // re-read the now-live session, mirroring the KWin settling above.
    if chosen == Compositor::Hyprland {
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "try-restart",
                "xdg-desktop-portal-hyprland.service",
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
