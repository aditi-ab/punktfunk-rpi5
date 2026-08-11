//! Live graphical-session detection + session-epoch + process-env retargeting (plan §W3 — the
//! self-contained subsystem carved out of [`super`]). Detects the active compositor/session
//! ([`detect_active_session`]), tracks the session epoch so pooled displays never outlive their
//! compositor instance, and retargets the process env at the live session ([`apply_session_env`],
//! [`settle_desktop_portal`]) under `super::ENV_LOCK`.

use super::*;

/// Budget for one `systemctl --user` / `dbus-update-activation-environment` call.
///
/// These talk to the session bus, and a bus that is itself restarting or wedged answers nothing —
/// unbounded, that pinned the caller (on the host, the session's stream thread) forever. A restart
/// of the portal units is the slowest legitimate case, hence the generous window; missing it just
/// means the portal env settles late, which the callers already treat as best-effort.
#[cfg(target_os = "linux")]
const SYSTEMD_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

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
    // DECIDE under the lock, ACT outside it. The action below drops keep-alive displays through the
    // registry lock and shells out to `systemctl` on a 10 s budget; holding a process-wide mutex
    // across that blocks every other detector — and this runs from the per-connect resolve, the
    // mid-stream switch watcher AND the capture-loss re-detect. The baseline is advanced inside the
    // lock too, so a concurrent observer sees the new instance and cannot run the action twice.
    let changed = {
        let mut last = LAST_INSTANCE.lock().unwrap_or_else(|e| e.into_inner());
        let prev = *last;
        // A `None` scan result is NOT an observation (see [`classify_instance_change`]), so it must
        // not become the baseline either: recording it would make the NEXT poll — the one that sees
        // the still-running desktop again — read as `None → DesktopKde`, i.e. a fresh instance, and
        // bump the epoch out from under every pooled display. Leave the baseline on the last REAL
        // instance and a transient miss is fully inert, in both directions.
        if cur.0 != ActiveKind::None {
            *last = Some(cur);
        }
        prev
    };
    if let Some(prev) = changed {
        if let InstanceChange::NewInstance { invalidate } = classify_instance_change(prev, cur) {
            // Invalidate only the OLD backend, and only if it was a desktop compositor (never gamescope).
            if let Some(old_kind) = invalidate {
                if let Some(old) = compositor_for_kind(old_kind) {
                    registry::invalidate_backend(old.id());
                }
                // The dead desktop's socket vars may still sit in the systemd --user manager env
                // ([`settle_desktop_portal`]'s import-environment) — scrub them NOW, or the next
                // `gamescope-session.target` start inherits a stale WAYLAND_DISPLAY and gamescope
                // runs NESTED against the dead desktop socket instead of becoming the display
                // server ("Failed to connect to wayland socket: wayland-0" — kept a Deck's Game
                // Mode from starting at all, observed live 2026-07-21).
                scrub_desktop_manager_env();
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
}

/// What a `prev` → `cur` observation means for the session epoch — the pure core of
/// [`observe_session_instance`], so the (surprisingly load-bearing) rules below are unit-tested
/// without the process-global baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceChange {
    /// The same instance, or a change the epoch does not track — do nothing.
    Nothing,
    /// A new compositor instance: bump the epoch. `invalidate` names the OUTGOING desktop
    /// compositor whose kept displays must be dropped (its PipeWire nodes died with it); `None`
    /// when the outgoing session was gamescope / nothing, which owns no pooled displays.
    NewInstance { invalidate: Option<ActiveKind> },
}

/// The epoch's rules, in one place:
///
/// * A `cur` of [`ActiveKind::None`] is **never** a change. `detect_active_session` answers `None`
///   both for "no graphical session is running" and for a scan that simply saw nothing — its whole
///   probe hangs off `if let Ok(entries) = std::fs::read_dir("/proc")`, and every per-PID rung
///   (`metadata`, `match_name`) can lose a race with a re-exec. Treating that as "the desktop
///   changed" ran `registry::invalidate_backend`, which removes pool entries in ANY lifecycle state
///   — Active ones included — so one unlucky `/proc` read tore down displays that were mid-stream,
///   and scrubbed the live session's socket vars out of the systemd `--user` manager on the way.
///   A real logout is picked up by the NEXT real observation (a different kind, or the same kind at
///   a new PID), which is the evidence-carrying end of the same transition.
/// * Only a **desktop** compositor (KWin / Mutter / wlroots) instance change counts. A **gamescope**
///   session ([`ActiveKind::Gaming`]) is not the epoch's subject: the box's game-mode / managed
///   gamescope isn't pooled, and dedicated **spawns** are independent nested sessions whose nodes
///   outlive any active-session change. So a game-mode gamescope restart, a Gaming↔Gaming
///   winning-PID flap (e.g. B1 stopping the autologin before a dedicated spawn), or a
///   coexisting-gamescope set change must NOT bump/invalidate — that would tear down a live/kept
///   dedicated session (review findings #6/#7/#10).
/// * A same-kind PID change IS a change: a fresh KWin's node-id space is unrelated to the dead
///   one's (A4).
fn classify_instance_change(
    prev: (ActiveKind, Option<u32>),
    cur: (ActiveKind, Option<u32>),
) -> InstanceChange {
    if cur.0 == ActiveKind::None
        || prev == cur
        || !(is_desktop_kind(prev.0) || is_desktop_kind(cur.0))
    {
        return InstanceChange::Nothing;
    }
    InstanceChange::NewInstance {
        invalidate: is_desktop_kind(prev.0).then_some(prev.0),
    }
}

/// Counterpart to [`settle_desktop_portal`]'s `import-environment`: drop the desktop session's
/// socket vars from the systemd `--user` manager env once that desktop instance is GONE. They
/// persist in the manager otherwise, and every later user unit inherits them — including
/// `gamescope-session.target`, whose gamescope then aborts trying to attach to the dead desktop
/// socket. Best-effort; the D-Bus activation env has no unset op, but gamescope-session is
/// systemd-started, so the manager scrub is the one that matters. (A desktop restart re-imports
/// via the next [`settle_desktop_portal`], so scrubbing on a bounce is harmless.)
#[cfg(target_os = "linux")]
fn scrub_desktop_manager_env() {
    let _ = crate::proc::status_within(
        std::process::Command::new("systemctl").args([
            "--user",
            "unset-environment",
            "WAYLAND_DISPLAY",
            "DISPLAY",
        ]),
        SYSTEMD_BUDGET,
    );
}

#[cfg(not(target_os = "linux"))]
fn scrub_desktop_manager_env() {}

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
    /// session env. `None` for every other compositor.
    pub hyprland_signature: Option<String>,
    /// `SWAYSOCK` of the live sway instance (`Some` only for a sway [`ActiveKind::DesktopWlroots`]).
    /// `swaymsg` needs it, and it was the LAST session variable the host could not derive: a
    /// `systemd --user` host that never inherited the login shell's environment had no sway IPC at
    /// all, so output enumeration and the chooser both failed. Derived from the detected compositor
    /// PID like the Hyprland signature above. `None` on river (wlroots, but no sway IPC) and every
    /// other compositor.
    pub sway_socket: Option<String>,
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
    // Only the non-Linux `detect_active_session` calls this (below); Linux always has a real
    // session to describe.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn none() -> ActiveSession {
        let probe = EnvProbe::sample();
        ActiveSession {
            kind: ActiveKind::None,
            env: SessionEnv {
                xdg_runtime_dir: default_runtime_dir(&probe),
                dbus_session_bus_address: default_bus(&probe, &default_runtime_dir(&probe)),
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

/// The session-scoped variables detection reads, sampled ONCE under [`ENV_LOCK`].
///
/// Detection used to call `std::env::var` at five points spread across a `/proc` scan, none of them
/// holding the lock its own writers take — the getenv/setenv data race [`crate::ENV_LOCK`]'s doc
/// describes as UB that "could crash the host" (glibc `setenv` can realloc `environ` and free the
/// old value string under a concurrent reader). Sampling up front closes that.
///
/// Sampling rather than simply taking the lock for the whole of [`detect_active_session`] is
/// deliberate: that function runs every second from the host's session watcher and scans `/proc`,
/// and holding a process-wide lock across a directory walk trades one problem for another. The
/// snapshot costs one acquisition and five reads with no syscalls in between.
///
/// It also makes the readers below pure functions of their inputs, which is what lets the tests
/// exercise them without mutating process-global state.
#[derive(Clone, Debug, Default)]
struct EnvProbe {
    xdg_runtime_dir: Option<String>,
    dbus_session_bus_address: Option<String>,
    wayland_display: Option<String>,
    hyprland_signature: Option<String>,
    swaysock: Option<String>,
}

impl EnvProbe {
    /// Every var is `filter`ed non-empty: `Ok("")` is not a usable runtime dir or socket path, and
    /// treating it as one is how an empty `XDG_RUNTIME_DIR` used to yield a *relative* path.
    fn sample() -> EnvProbe {
        fn v(k: &str) -> Option<String> {
            std::env::var(k).ok().filter(|s| !s.is_empty())
        }
        crate::with_env_lock(|| EnvProbe {
            xdg_runtime_dir: v("XDG_RUNTIME_DIR"),
            dbus_session_bus_address: v("DBUS_SESSION_BUS_ADDRESS"),
            wayland_display: v("WAYLAND_DISPLAY"),
            hyprland_signature: v("HYPRLAND_INSTANCE_SIGNATURE"),
            swaysock: v("SWAYSOCK"),
        })
    }
}

/// The per-user runtime directory, resolved ONCE for callers outside detection.
///
/// The rule is the one [`default_runtime_dir`] applies, and the point is what it is NOT: several
/// callers used to spell it `std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())`,
/// while their own doc comments promised "per-user, 0700 — NOT a world-writable /tmp path another
/// local user could pre-create or rewrite". One of those paths is an executable that
/// xdg-desktop-portal-hyprland then RUNS. `Ok("")` was the second half of the same defect: it
/// yielded a path relative to the process's CWD rather than any runtime dir at all.
#[cfg(target_os = "linux")]
pub(crate) fn runtime_dir() -> String {
    default_runtime_dir(&EnvProbe::sample())
}

#[cfg(target_os = "linux")]
fn default_runtime_dir(env: &EnvProbe) -> String {
    env.xdg_runtime_dir.clone().unwrap_or_else(|| {
        let uid = crate::proc::current_uid();
        format!("/run/user/{uid}")
    })
}

#[cfg(not(target_os = "linux"))]
fn default_runtime_dir(env: &EnvProbe) -> String {
    env.xdg_runtime_dir.clone().unwrap_or_default()
}

fn default_bus(env: &EnvProbe, runtime: &str) -> String {
    env.dbus_session_bus_address
        .clone()
        .unwrap_or_else(|| format!("unix:path={runtime}/bus"))
}

/// Detect the graphical session live for our uid right now (cheap, side-effect-free: a `/proc`
/// scan plus a runtime-dir socket scan — well under the handshake timeout). The authority is the
/// running compositor process; a desktop compositor outranks a lingering gamescope. Used to route
/// each connect to the correct backend, and to derive the [`SessionEnv`] that targets it.
#[cfg(target_os = "linux")]
pub fn detect_active_session() -> ActiveSession {
    use std::os::unix::fs::MetadataExt;
    let uid = crate::proc::current_uid();
    // ONE sample of the session-scoped env, before any scanning — see [`EnvProbe`]. Everything
    // below reads this snapshot, never the process env.
    let env = EnvProbe::sample();
    let xdg_runtime_dir = default_runtime_dir(&env);
    let dbus = default_bus(&env, &xdg_runtime_dir);

    // Process probe: the running graphical compositor of THIS uid decides the kind. Priority lets
    // a real desktop (kwin/gnome/sway) win over a leftover gamescope child. Names are matched
    // exactly, `pkill -x` style — but resolved through [`crate::proc::match_name`], NOT a raw
    // `comm` read: on NixOS every one of these binaries is a nixpkgs wrapper whose real ELF is
    // `.<name>-wrapped`, so a raw `comm` says `.kwin_wayland-w` and this whole probe answered
    // `None` on a running KDE desktop.
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
            let Some(comm) = crate::proc::match_name(&pid_path) else {
                continue;
            };
            let (k, prio) = match comm.as_str() {
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
            find_wayland_socket(&env, &xdg_runtime_dir, uid)
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
        ActiveKind::DesktopHyprland => find_hypr_signature(&env, &xdg_runtime_dir, uid),
        _ => None,
    };
    // Same idea for sway's IPC socket: without it `swaymsg` has nothing to talk to, and a
    // `systemd --user` host never inherited it.
    let sway_socket = match kind {
        ActiveKind::DesktopWlroots => find_sway_socket(&env, &xdg_runtime_dir, uid, winning_pid),
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
            sway_socket,
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
fn find_hypr_signature(env: &EnvProbe, runtime: &str, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let hypr = std::path::Path::new(runtime).join("hypr");
    if let Some(sig) = &env.hyprland_signature {
        if hypr.join(sig).join(".socket.sock").exists() {
            return Some(sig.clone());
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

/// Find the live sway IPC socket (`SWAYSOCK`) for our uid. Trust a valid inherited value first (the
/// host launched inside the session); then the exact `sway-ipc.<uid>.<pid>.sock` for the compositor
/// PID detection already picked — a name sway builds from those two numbers, so it is an identity
/// match rather than a guess; then the newest-mtime `sway-ipc.<uid>.*.sock` we own, for the case
/// where the socket name does not match the PID we saw (sway re-exec, a wrapper process).
///
/// `None` on river: it is the other [`ActiveKind::DesktopWlroots`] compositor and has no sway IPC —
/// which is the honest answer, since the wlroots backend drives sway through `swaymsg`.
#[cfg(target_os = "linux")]
fn find_sway_socket(env: &EnvProbe, runtime: &str, uid: u32, pid: Option<u32>) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if let Some(s) = &env.swaysock {
        if std::path::Path::new(s).exists() {
            return Some(s.clone());
        }
    }
    if let Some(pid) = pid {
        let exact = std::path::Path::new(runtime).join(format!("sway-ipc.{uid}.{pid}.sock"));
        if exact.exists() {
            return Some(exact.to_string_lossy().into_owned());
        }
    }
    let prefix = format!("sway-ipc.{uid}.");
    let mut cands: Vec<(std::time::SystemTime, String)> = Vec::new();
    for e in std::fs::read_dir(runtime).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !name.ends_with(".sock") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.uid() != uid {
            continue;
        }
        let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
        cands.push((mtime, e.path().to_string_lossy().into_owned()));
    }
    cands.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    cands.into_iter().next().map(|(_, p)| p)
}

#[cfg(not(target_os = "linux"))]
pub fn detect_active_session() -> ActiveSession {
    ActiveSession::none()
}

/// Find the live `wayland-*` socket in `runtime` for our uid (skipping `.lock` sidecars). Trust a
/// valid inherited `WAYLAND_DISPLAY` first; otherwise take the newest-mtime socket we own (a
/// desktop session normally exposes exactly one).
#[cfg(target_os = "linux")]
fn find_wayland_socket(env: &EnvProbe, runtime: &str, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if let Some(w) = env.wayland_display.clone() {
        {
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
    // sway: same treatment, and for the same reason — `swaymsg` (output enumeration, the capture
    // chooser) is unreachable without it, so a systemd `--user` host that never inherited the login
    // environment had no sway backend at all. Cleared when nothing sway-shaped is live, so a
    // sway→Hyprland switch can't leave `swaymsg` aimed at a dead socket. `wlroots::is_available()`
    // keys off this variable, so setting it here is also what makes the backend visible at all.
    match &e.sway_socket {
        Some(sock) => std::env::set_var("SWAYSOCK", sock),
        None => std::env::remove_var("SWAYSOCK"),
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
    let _ = crate::proc::status_within(
        std::process::Command::new("systemctl")
            .args(["--user", "import-environment"])
            .args(VARS),
        SYSTEMD_BUDGET,
    );
    let _ = crate::proc::status_within(
        std::process::Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .args(VARS),
        SYSTEMD_BUDGET,
    );
    // KWin input goes through the xdg RemoteDesktop portal; the frontend routes RemoteDesktop to a
    // backend by its OWN startup XDG_CURRENT_DESKTOP, so restart it (+ the KDE backend) to re-read
    // the now-live session, then let it settle before the injector reopens against it.
    if chosen == Compositor::Kwin {
        let _ = crate::proc::status_within(
            std::process::Command::new("systemctl").args([
                "--user",
                "try-restart",
                "xdg-desktop-portal-kde.service",
                "xdg-desktop-portal.service",
            ]),
            SYSTEMD_BUDGET,
        );
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    // Hyprland capture rides the xdg ScreenCast portal serviced by xdph (G5): on a mid-stream switch
    // xdph may still hold the old session's Wayland/instance env, so restart it (+ the frontend) to
    // re-read the now-live session, mirroring the KWin settling above.
    if chosen == Compositor::Hyprland {
        let _ = crate::proc::status_within(
            std::process::Command::new("systemctl").args([
                "--user",
                "try-restart",
                "xdg-desktop-portal-hyprland.service",
                "xdg-desktop-portal.service",
            ]),
            SYSTEMD_BUDGET,
        );
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    tracing::info!(
        compositor = chosen.id(),
        "settled desktop portal env for the switched-to session"
    );
}

#[cfg(not(target_os = "linux"))]
pub fn settle_desktop_portal(_chosen: Compositor) {}

/// The epoch rules are platform-neutral (they are pure over [`ActiveKind`] + PID), so — unlike the
/// `/proc`-and-socket tests below — these run on every host this crate builds on.
#[cfg(test)]
mod instance_change_tests {
    use super::*;

    /// The 10.9 regression: a scan that answered `None` while KDE was in fact still up used to
    /// satisfy `is_desktop_kind(prev)` and run the full invalidate — which drops pool entries in
    /// ANY state, live streaming ones included.
    #[test]
    fn a_none_observation_is_never_a_change() {
        for prev in [
            (ActiveKind::DesktopKde, Some(42)),
            (ActiveKind::DesktopGnome, Some(7)),
            (ActiveKind::Gaming, Some(9)),
            (ActiveKind::None, None),
        ] {
            assert_eq!(
                classify_instance_change(prev, (ActiveKind::None, None)),
                InstanceChange::Nothing,
                "a None scan result must not invalidate {prev:?}"
            );
        }
    }

    #[test]
    fn a_desktop_swap_invalidates_the_outgoing_desktop() {
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopGnome, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        // Desktop → gamescope (Game Mode): the dead KWin's kept displays go with it.
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::Gaming, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        // gamescope → desktop: a new epoch, but gamescope owns no pooled entries to invalidate.
        assert_eq!(
            classify_instance_change(
                (ActiveKind::Gaming, Some(1)),
                (ActiveKind::DesktopKde, Some(2))
            ),
            InstanceChange::NewInstance { invalidate: None }
        );
    }

    #[test]
    fn a_same_kind_restart_is_a_new_instance_but_a_gamescope_flap_is_not() {
        // A fresh KWin (new PID) has an unrelated node-id space — A4.
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopKde, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        // The same instance re-detected: inert.
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopKde, Some(1))
            ),
            InstanceChange::Nothing
        );
        // Gaming↔Gaming winning-PID flap: never the epoch's business (findings #6/#7/#10).
        assert_eq!(
            classify_instance_change((ActiveKind::Gaming, Some(1)), (ActiveKind::Gaming, Some(2))),
            InstanceChange::Nothing
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A scratch runtime dir with the sway-ipc sockets named in `pids`, plus the uid the names are
    /// built from. Removed on drop.
    struct FakeRuntime {
        dir: std::path::PathBuf,
        uid: u32,
    }

    impl FakeRuntime {
        fn new(tag: &str, pids: &[u32]) -> FakeRuntime {
            let uid = crate::proc::current_uid();
            let dir =
                std::env::temp_dir().join(format!("pf-swaysock-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for pid in pids {
                std::fs::write(dir.join(format!("sway-ipc.{uid}.{pid}.sock")), b"").unwrap();
            }
            FakeRuntime { dir, uid }
        }
        fn path(&self) -> &str {
            self.dir.to_str().unwrap()
        }
    }

    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A probe with nothing inherited, so the "trust what we inherited" rung can't decide the test.
    /// The readers take this by argument, so — unlike the `set_var`/`remove_var` dance this replaced
    /// — the tests no longer mutate process-global state to steer them.
    fn no_inherited_env() -> EnvProbe {
        EnvProbe::default()
    }

    /// The point of deriving it: the socket that belongs to the compositor detection actually found,
    /// not merely *a* sway socket. A stale socket from a previous sway (crash, re-login) sitting in
    /// the same runtime dir must not win.
    #[test]
    fn the_socket_matching_the_detected_pid_wins() {
        let rt = FakeRuntime::new("exact", &[4242, 9999]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(4242));
        assert_eq!(
            got,
            Some(format!("{}/sway-ipc.{}.4242.sock", rt.path(), rt.uid))
        );
    }

    /// sway re-exec (or a wrapper) can leave the socket named for a PID we didn't see. One socket in
    /// the dir is still unambiguous — better to hand `swaymsg` the real thing than nothing.
    #[test]
    fn an_unmatched_pid_falls_back_to_the_socket_that_is_there() {
        let rt = FakeRuntime::new("fallback", &[777]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(12345));
        assert_eq!(
            got,
            Some(format!("{}/sway-ipc.{}.777.sock", rt.path(), rt.uid))
        );
    }

    /// river is the other wlroots desktop and ships no sway IPC. Reporting `None` is what keeps
    /// `apply_session_env` from exporting a `SWAYSOCK` that points at nothing — an exported lie
    /// would make `wlroots::is_available()` claim a backend that cannot answer.
    #[test]
    fn no_sway_ipc_socket_reports_none() {
        let rt = FakeRuntime::new("none", &[]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(1));
        assert_eq!(got, None);
    }

    /// A socket NAMED for another uid is not ours to talk to. This covers the filename filter
    /// (`sway-ipc.<uid>.` prefix) and nothing more — it was previously called
    /// `another_uids_socket_is_ignored`, which claimed the ownership guard below it. It never
    /// reached that guard: the prefix test `continue`s first, so the assertion held even with
    /// `md.uid() != uid` deleted. See `another_uids_owned_socket_is_ignored` for the real leg.
    #[test]
    fn another_uids_socket_name_is_ignored() {
        let rt = FakeRuntime::new("otheruid", &[]);
        let other = rt.uid.wrapping_add(1);
        std::fs::write(rt.dir.join(format!("sway-ipc.{other}.500.sock")), b"").unwrap();
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(500));
        assert_eq!(got, None);
    }

    /// The ownership guard proper: a socket named as ours but OWNED by someone else must be
    /// rejected on its metadata. That is the case that matters — a hostile local user can pick the
    /// filename, so the name is not evidence.
    ///
    /// Ignored by default because it needs to `chown` a file to another uid, i.e. root. Run it
    /// where that is true (a container, or CI as root):
    ///     cargo test -p pf-vdisplay -- --ignored another_uids_owned
    #[test]
    #[ignore = "needs root to chown the socket to another uid"]
    fn another_uids_owned_socket_is_ignored() {
        use std::os::unix::fs::MetadataExt;
        let rt = FakeRuntime::new("ownedbyother", &[]);
        // Named exactly as one of OURS, so the prefix filter admits it and the metadata check is
        // the only thing that can reject it.
        let path = rt.dir.join(format!("sway-ipc.{}.500.sock", rt.uid));
        std::fs::write(&path, b"").unwrap();
        let target_uid = rt.uid.wrapping_add(1);
        let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: `c_path` is a live NUL-terminated CString borrowed for the duration of the call,
        // and `chown` reads it without retaining it. `u32::MAX` is the unsigned spelling of the
        // `-1` gid that means "leave the group unchanged".
        let rc = unsafe { libc::chown(c_path.as_ptr(), target_uid, u32::MAX) };
        assert_eq!(rc, 0, "chown failed — this test needs root");
        assert_eq!(std::fs::metadata(&path).unwrap().uid(), target_uid);

        // Query with a pid that does NOT match the filename: the exact-path shortcut earlier in
        // `find_sway_socket` returns before the ownership guard, so hitting it would make this
        // test vacuous in the same way the one above was.
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(999));
        assert_eq!(got, None, "a socket owned by another uid must be rejected");
    }
}
