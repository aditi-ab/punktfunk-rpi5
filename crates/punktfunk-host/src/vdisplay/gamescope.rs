//! gamescope virtual-display backend.
//!
//! Unlike KWin/Mutter (which create a virtual output at runtime via a protocol), gamescope is a
//! micro-compositor we *spawn*: `gamescope --backend headless -W w -H h -r hz -- <app>`. It runs
//! the app nested, composites at the requested size/refresh (so the source rate is the client's
//! rate natively — no separate refresh step), and exports a built-in PipeWire node named
//! `gamescope` (media.class `Video/Source`, BGRx/NV12, dmabuf or shm) on the user's PipeWire
//! daemon. We discover that node and capture it like any other; the gamescope *process* is the
//! keepalive — dropping the [`VirtualOutput`] kills it (tearing the output down).
//!
//! Requirements: gamescope built with PipeWire + libei input emulation (distro packages are);
//! a usable Vulkan device (the NVIDIA render node). Headless capture on the proprietary NVIDIA
//! driver is plausible-by-architecture but not a well-trodden path — validate empirically.
//! Input uses gamescope's own libei/EIS socket (`LIBEI_SOCKET`), relayed to the libei backend (see
//! `inject/libei.rs`) — wired and live-validated.

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The gamescope virtual-display driver. Three modes by env, in precedence order:
/// * `PUNKTFUNK_GAMESCOPE_SESSION=<client>` — host-MANAGE a `gamescope-session-plus` session
///   (full Steam-Deck-UI polish) headless at the CLIENT's mode; relaunch it when the mode changes.
/// * `PUNKTFUNK_GAMESCOPE_NODE=<id|auto>` — ATTACH to an already-running gamescope (capture +
///   inject, no lifecycle ownership).
/// * else — SPAWN a bare headless gamescope sized to the mode, running `PUNKTFUNK_GAMESCOPE_APP`.
pub struct GamescopeDisplay;

/// A running host-managed session (its transient systemd --user unit) + the mode it was launched at.
struct SessionState {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

/// The host-managed `gamescope-session-plus` session, tracked at **host lifetime** (NOT per
/// `GamescopeDisplay`, which is recreated per client session and would otherwise cold-start Steam on
/// every reconnect). A same-mode reconnect reuses the running session (no Steam restart); a
/// different mode relaunches it. Cleared/relaunched by `launch_session`; survives across client
/// connections; on host restart the next launch stops the leftover unit by name and starts fresh.
static MANAGED_SESSION: std::sync::Mutex<Option<SessionState>> = std::sync::Mutex::new(None);

/// Autologin gaming-mode `gamescope-session-plus@*` units we stopped on connect to free Steam
/// (single-instance), so [`schedule_restore_tv_session`] can restart them when the client disconnects.
static STOPPED_AUTOLOGIN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A pending debounced TV-session restore: the instant [`do_restore_tv_session`] should fire after
/// the last client disconnect. A reconnect inside the window clears it (and reuses the still-warm
/// managed session), so we never stop+relaunch gamescope per connect — that per-connect teardown is
/// what leaked NVIDIA GPU context on F44 (the black-screen reconnect). Driven by the host-lifetime
/// [`start_restore_worker`] thread.
static PENDING_RESTORE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// How long to wait after the last disconnect before restoring the TV's autologin gaming session —
/// long enough that a quick reconnect (e.g. a controller hiccup) reuses the warm managed session
/// instead of triggering a stop/relaunch.
const RESTORE_DEBOUNCE: Duration = Duration::from_secs(5);

/// systemd --user transient unit name for the host-managed gamescope-session-plus session.
const SESSION_UNIT: &str = "punktfunk-gamescope";
/// The gamescope-session-plus launcher script (Bazzite / SteamOS-like hosts).
const SESSION_PLUS_BIN: &str = "/usr/share/gamescope-session-plus/gamescope-session-plus";

impl GamescopeDisplay {
    pub fn new() -> Result<Self> {
        Ok(GamescopeDisplay)
    }
}

impl VirtualDisplay for GamescopeDisplay {
    fn name(&self) -> &'static str {
        "gamescope"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Host-managed gamescope-session-plus at the CLIENT's mode (the Bazzite path): launch the
        // full Steam-Deck-UI session headless at the client's resolution + refresh — so games SEE
        // them (via the injected --nested-refresh + generated CVT modes, not the box's TV EDID) —
        // and relaunch it when the client's mode changes. Reuses the node + EIS discovery below.
        if let Ok(client) = std::env::var("PUNKTFUNK_GAMESCOPE_SESSION") {
            return create_managed_session(&client, mode);
        }
        // Attach to an already-running gamescope (a foreign / externally-launched session) instead
        // of spawning our own: capture its node AND inject into its EIS socket.
        // PUNKTFUNK_GAMESCOPE_NODE=<id|auto>; "auto" discovers the gamescope `Video/Source` node.
        if let Ok(id) = std::env::var("PUNKTFUNK_GAMESCOPE_NODE") {
            let node_id: u32 = if id.trim().eq_ignore_ascii_case("auto") {
                find_gamescope_node().ok_or_else(|| {
                    anyhow!(
                        "PUNKTFUNK_GAMESCOPE_NODE=auto but no running gamescope Video/Source node \
                         was found — is the headless gamescope/Steam session up?"
                    )
                })?
            } else {
                id.parse()
                    .context("PUNKTFUNK_GAMESCOPE_NODE must be a node id or 'auto'")?
            };
            point_injector_at_eis();
            tracing::info!(node_id, "gamescope: attaching to existing PipeWire node");
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
                keepalive: Box::new(()),
            });
        }
        check_gamescope_version(); // diagnostic only — warns on known-deadlock-prone versions
        let proc = GamescopeProc(spawn(mode.width, mode.height, mode.refresh_hz.max(1))?);
        // gamescope creates its PipeWire node a moment after start; poll for it (the proc is held
        // alive meanwhile, and killed if we give up).
        let node_id = wait_for_node(Duration::from_secs(15)).ok_or_else(|| {
            anyhow!(
                "gamescope PipeWire node did not appear within 15s — gamescope may have failed to \
                 start or headless capture is unsupported on this GPU/driver (see /tmp/punktfunk-gamescope.log)"
            )
        })?;
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "gamescope virtual output ready"
        );
        Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(proc),
        })
    }
}

/// Host-managed `gamescope-session-plus` at the client's mode (state in [`MANAGED_SESSION`], so it
/// persists across client connections — a reconnect at the same mode reuses it instantly). REUSE
/// the running session if the mode is unchanged and its node is still live (no Steam restart);
/// otherwise stop the old transient unit and RELAUNCH at the new mode (gamescope can't change output
/// mode live). Then discover the node + point the injector, exactly as the attach path does.
fn create_managed_session(client: &str, mode: Mode) -> Result<VirtualOutput> {
    // A (re)connect cancels any pending debounced TV-restore: we're about to (re)use the managed
    // session, so the autologin must stay stopped and the warm session stays up (no stop/relaunch).
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // Steam is single-instance: if the box autologged into gaming mode on a physical display (the
    // Bazzite default — `gamescope-session-plus@ogui-steam` on the TV), that session holds Steam and
    // renders to the TV's native mode, which we'd capture instead of the client's. Free Steam by
    // stopping it; [`schedule_restore_tv_session`] (on disconnect) brings it back after a debounce.
    stop_autologin_sessions();
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width && s.height == mode.height && s.refresh_hz == mode.refresh_hz
    });
    if same_mode {
        if let Some(node_id) = find_gamescope_node() {
            point_injector_at_eis();
            tracing::info!(
                node_id,
                w = mode.width,
                h = mode.height,
                hz = mode.refresh_hz,
                "gamescope session: reusing the running session (same mode — no Steam restart)"
            );
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
                keepalive: Box::new(()),
            });
        }
        tracing::warn!("gamescope session: tracked session has no live node — relaunching");
        *guard = None;
    }
    // (Re)launch at the new mode. `launch_session` stops the old unit by name first, so there is
    // exactly one gamescope `Video/Source` node for discovery.
    let node_id = launch_session(client, SESSION_UNIT, mode)?;
    point_injector_at_eis();
    *guard = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
    });
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope session: launched gamescope-session-plus at the client's mode"
    );
    Ok(VirtualOutput {
        node_id,
        remote_fd: None,
        preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
        keepalive: Box::new(()),
    })
}

/// Stop every running autologin gaming-mode session (`gamescope-session-plus@*.service`) so its
/// single-instance Steam is free for our own host-managed session. Records the units so
/// [`schedule_restore_tv_session`] can restart them on disconnect. Our own session is the transient
/// `punktfunk-gamescope` unit (not a `@`-instance), so it's never matched here. No-op when nothing
/// is autologged in (e.g. a box that boots headless).
fn stop_autologin_sessions() {
    let Ok(out) = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ])
        .output()
    else {
        return;
    };
    let mut stopped = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(unit) = line.split_whitespace().next() {
            if unit.starts_with("gamescope-session-plus@") && unit.ends_with(".service") {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", unit])
                    .status();
                tracing::info!(
                    unit,
                    "freed Steam: stopped the autologin gaming session for this stream"
                );
                stopped.push(unit.to_string());
            }
        }
    }
    if !stopped.is_empty() {
        *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = stopped;
    }
}

/// Client disconnected: **schedule** a debounced restore of the TV's autologin gaming session(s) we
/// stopped on connect — the actual restore fires [`RESTORE_DEBOUNCE`] later (via [`start_restore_worker`])
/// unless a client reconnects first, which cancels it and reuses the warm managed session. Debouncing
/// means at most one gamescope stop/relaunch per quiet period instead of one per disconnect — the
/// per-connect churn is what leaked GPU context on F44. No-op when nothing was stolen (non-Bazzite /
/// headless box). Idempotent / safe to call on every session end.
pub fn schedule_restore_tv_session() {
    if STOPPED_AUTOLOGIN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
    {
        return; // nothing was stolen → nothing to restore (also the non-Bazzite path)
    }
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Instant::now() + RESTORE_DEBOUNCE);
    tracing::info!(
        secs = RESTORE_DEBOUNCE.as_secs(),
        "gamescope: scheduled debounced TV-session restore (cancelled if a client reconnects)"
    );
}

/// Tear down our host-managed session (freeing Steam) and restart the autologin gaming session(s)
/// we stopped on connect — so the TV returns to gaming mode when no one is streaming. Invoked by
/// [`start_restore_worker`] once the debounce deadline passes; takes the stopped-unit list so a
/// cancelled+reconnected window keeps the list for a later real restore.
fn do_restore_tv_session() {
    let units = std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    if units.is_empty() {
        return; // nothing was stolen → nothing to restore (also the non-Bazzite path)
    }
    stop_session(SESSION_UNIT); // our gamescope/Steam session, so Steam is free for the autologin
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    for unit in units {
        let _ = Command::new("systemctl")
            .args(["--user", "start", &unit])
            .status();
        tracing::info!(
            unit,
            "restored the TV's autologin gaming session (debounce elapsed, no client)"
        );
    }
}

/// Host-lifetime worker that fires a pending [`schedule_restore_tv_session`] once its debounce
/// deadline passes. Returns a keepalive handle — drop it (host shutdown) to stop the worker. Cheap:
/// a 100 ms tick that does nothing until a restore is actually pending.
pub fn start_restore_worker() -> std::sync::Arc<()> {
    let handle = std::sync::Arc::new(());
    let weak = std::sync::Arc::downgrade(&handle);
    if let Err(e) = std::thread::Builder::new()
        .name("punktfunk-restore-worker".into())
        .spawn(move || {
            while weak.upgrade().is_some() {
                std::thread::sleep(Duration::from_millis(100));
                let due = {
                    let mut g = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
                    match *g {
                        Some(deadline) if Instant::now() >= deadline => {
                            *g = None;
                            true
                        }
                        _ => false,
                    }
                };
                if due {
                    do_restore_tv_session();
                }
            }
        })
    {
        tracing::error!(error = %e, "restore-worker spawn failed — TV session won't auto-restore on idle");
    }
    handle
}

/// Point the libei injector at the running gamescope's EIS socket (it reads the relay file
/// [`EI_SOCKET_FILE`]). Best-effort — video still works without it (input just won't reach the
/// session). Shared by the attach and host-managed-session paths.
fn point_injector_at_eis() {
    match find_gamescope_eis_socket() {
        Some(sock) => match std::fs::write(EI_SOCKET_FILE, &sock) {
            Ok(()) => {
                tracing::info!(socket = %sock, "gamescope: pointed injector at the session's EIS socket")
            }
            Err(e) => tracing::warn!(
                error = %e,
                "gamescope: could not write the EIS relay file — input may not reach the session"
            ),
        },
        None => tracing::warn!(
            "gamescope: no connectable gamescope EIS socket found — input won't reach the session"
        ),
    }
}

/// Path of the host-written `GAMESCOPE_BIN` wrapper (per-user, in tmpfs).
fn gamescope_bin_wrapper_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::Path::new(&base).join("punktfunk-gamescope-bin")
}

/// Write the `GAMESCOPE_BIN` wrapper that injects `--nested-refresh $PF_HZ` — the flag
/// gamescope-session-plus does NOT expose, and the one that makes games see the client's refresh
/// instead of ~60 Hz. The body is constant (the rate comes from the `PF_HZ` env per launch), so the
/// write is idempotent. Returns its path.
fn write_gamescope_bin_wrapper() -> Result<std::path::PathBuf> {
    let path = gamescope_bin_wrapper_path();
    std::fs::write(
        &path,
        "#!/bin/sh\nexec /usr/bin/gamescope --nested-refresh \"${PF_HZ:-60}\" \"$@\"\n",
    )
    .with_context(|| format!("write GAMESCOPE_BIN wrapper {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod the GAMESCOPE_BIN wrapper {}", path.display()))?;
    Ok(path)
}

/// Launch `gamescope-session-plus <client>` headless at `mode` as a transient `systemd --user`
/// unit (clean cgroup teardown of the whole Steam tree on stop). Injects `--nested-refresh` (via
/// the wrapper) + `--generate-drm-mode cvt` so games see exactly `mode` (resolution + refresh) and
/// not the box's physical-display EDID. Blocks until the gamescope `Video/Source` node appears
/// (Steam Big Picture cold-start is slow), returning its id; on timeout it stops the unit and errors.
fn launch_session(client: &str, unit_name: &str, mode: Mode) -> Result<u32> {
    if !std::path::Path::new(SESSION_PLUS_BIN).exists() {
        anyhow::bail!(
            "PUNKTFUNK_GAMESCOPE_SESSION is set but {SESSION_PLUS_BIN} is missing — the host-managed \
             session needs gamescope-session-plus (a Bazzite / SteamOS-like host)"
        );
    }
    let wrapper = write_gamescope_bin_wrapper()?;
    stop_session(unit_name); // clear any stale unit + relay so a relaunch is clean
    let hz = mode.refresh_hz.max(1);
    let status = Command::new("systemd-run")
        .args(["--user", "--collect", &format!("--unit={unit_name}")])
        .arg("--setenv=BACKEND=headless")
        .arg(format!("--setenv=SCREEN_WIDTH={}", mode.width))
        .arg(format!("--setenv=SCREEN_HEIGHT={}", mode.height))
        .arg(format!("--setenv=PF_HZ={hz}"))
        .arg(format!("--setenv=GAMESCOPE_BIN={}", wrapper.display()))
        .arg("--setenv=DRM_MODE=cvt")
        .arg(format!("--setenv=CUSTOM_REFRESH_RATES={hz}"))
        .arg("--")
        .arg(SESSION_PLUS_BIN)
        .arg(client)
        .status()
        .context(
            "launch gamescope-session-plus via `systemd-run --user` (is the user systemd manager \
             up with XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS set?)",
        )?;
    if !status.success() {
        anyhow::bail!("`systemd-run --user` failed to start the gamescope session (exit {status})");
    }
    // Steam Big Picture cold-start is far slower than a bare app — poll the node for up to 45s.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(id) = find_gamescope_node() {
            return Ok(id);
        }
        if Instant::now() >= deadline {
            stop_session(unit_name);
            anyhow::bail!(
                "gamescope-session-plus '{client}' did not publish a Video/Source node within 45s \
                 (Steam failed to start? — `journalctl --user -u {unit_name}`)"
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Stop the host-managed session's transient unit (best-effort) and clear the EIS relay so a dead
/// session's socket name can't be reconnected.
fn stop_session(unit_name: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", unit_name])
        .status();
    let _ = std::fs::remove_file(EI_SOCKET_FILE);
}

/// File where the wrapper below writes gamescope's `LIBEI_SOCKET` (its EIS server socket),
/// read by the libei injector to drive input into the nested app. See [`crate::inject`].
pub const EI_SOCKET_FILE: &str = "/tmp/punktfunk-gamescope-ei";

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. The app comes from
/// `PUNKTFUNK_GAMESCOPE_APP` (default a no-op that just keeps gamescope alive — set it to a real
/// game/GL app for actual content, e.g. `steam -gamepadui` for the SteamOS-like session).
/// stdout/stderr go to `/tmp/punktfunk-gamescope.log`. The app is launched through a tiny shell
/// wrapper that relays gamescope's `LIBEI_SOCKET` (set for its children) to [`EI_SOCKET_FILE`]
/// so the input injector can connect to gamescope's EIS server from outside.
fn spawn(w: u32, h: u32, hz: u32) -> Result<Child> {
    let app =
        std::env::var("PUNKTFUNK_GAMESCOPE_APP").unwrap_or_else(|_| "sleep infinity".to_string());
    let _ = std::fs::remove_file(EI_SOCKET_FILE); // stale socket path from a previous session
    let mut cmd = Command::new("gamescope");
    cmd.args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &hz.to_string()])
        .args(["--xwayland-count", "1", "--"])
        .args([
            "sh",
            "-c",
            &format!("printf %s \"$LIBEI_SOCKET\" > {EI_SOCKET_FILE}; exec \"$@\""),
            "sh",
        ])
        .args(app.split_whitespace())
        // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
        .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia");
    if let Ok(log) = std::fs::File::create("/tmp/punktfunk-gamescope.log") {
        if let Ok(log2) = log.try_clone() {
            cmd.stdout(Stdio::from(log)).stderr(Stdio::from(log2));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    tracing::info!(w, h, hz, %app, "spawning gamescope (headless)");
    cmd.spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")
}

/// Wait for gamescope to report its PipeWire node. Authoritative source: gamescope's own log
/// line `stream available on node ID: N` (its node carries `node.name=gamescope` on TWO objects
/// — the adapter and the inner stream — and only the advertised id is the correct capture
/// target). Falls back to `pw-dump` discovery if the log line doesn't show.
fn wait_for_node(timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = node_from_log() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return find_gamescope_node(); // last-resort fallback
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Parse `stream available on node ID: N` from the spawned gamescope's log (ANSI-colored).
fn node_from_log() -> Option<u32> {
    let log = std::fs::read_to_string("/tmp/punktfunk-gamescope.log").ok()?;
    for line in log.lines().rev() {
        if let Some(pos) = line.find("stream available on node ID:") {
            let tail = &line[pos + "stream available on node ID:".len()..];
            let digits: String = tail.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// Find the `gamescope` `Video/Source` node id in a `pw-dump` snapshot of the default daemon.
///
/// `node.name=gamescope` appears on TWO objects (the adapter *and* the inner stream node); only
/// the one whose `media.class` is `Video/Source` is a valid capture target — connecting to the
/// other wedges the link. So we require `Video/Source` first and fall back to a bare name match
/// only if no class-tagged node is present (older gamescope that doesn't set media.class).
fn find_gamescope_node() -> Option<u32> {
    let out = Command::new("pw-dump").output().ok()?;
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let nodes = dump.as_array()?;
    let node_props = |obj: &serde_json::Value| -> Option<(u32, String, String)> {
        if obj.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            return None;
        }
        let id = obj.get("id").and_then(|i| i.as_u64())? as u32;
        let props = obj.get("info").and_then(|i| i.get("props"));
        let name = props
            .and_then(|p| p.get("node.name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let class = props
            .and_then(|p| p.get("media.class"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        Some((id, name, class))
    };
    // Preferred: a Video/Source node named (or containing) "gamescope".
    for obj in nodes {
        if let Some((id, name, class)) = node_props(obj) {
            if class == "Video/Source" && (name == "gamescope" || name.contains("gamescope")) {
                return Some(id);
            }
        }
    }
    // Fallback: a node literally named "gamescope" with no usable class tag.
    for obj in nodes {
        if let Some((id, name, _)) = node_props(obj) {
            if name == "gamescope" {
                tracing::warn!(
                    node_id = id,
                    "gamescope node has no media.class=Video/Source tag — capturing it anyway"
                );
                return Some(id);
            }
        }
    }
    None
}

/// Find the live gamescope EIS (libei) socket to inject into when ATTACHING to an existing
/// session (the spawn path instead relays the nested gamescope's `LIBEI_SOCKET` through a file).
///
/// gamescope names its EIS socket `gamescope-<display>-ei` in `XDG_RUNTIME_DIR` (alongside the
/// `gamescope-<display>` wayland socket). Stale sockets from dead sessions linger, so we don't
/// trust the name — we `connect()` each candidate and keep the connectable ones, returning the
/// most recently created (the live session). Returns the bare socket *name* (the injector
/// resolves it against `XDG_RUNTIME_DIR`, matching libei's own `LIBEI_SOCKET` semantics).
fn find_gamescope_eis_socket() -> Option<String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut live: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in std::fs::read_dir(&runtime).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The EIS socket itself, not its `.lock` sidecar or the bare wayland socket.
        if !(name.starts_with("gamescope-") && name.ends_with("-ei")) {
            continue;
        }
        // Connectable == a live listener is behind it (a dead session's socket refuses).
        if std::os::unix::net::UnixStream::connect(entry.path()).is_err() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        live.push((mtime, name));
    }
    live.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    live.into_iter().next().map(|(_, n)| n)
}

/// gamescope is usable wherever its binary runs — it spawns its own nested session, so it does
/// not require any particular desktop to be running. Quiet (no version warning — that's for the
/// create path); just checks the binary executes.
pub fn is_available() -> bool {
    std::process::Command::new("gamescope")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimum gamescope that captures reliably: below 3.16.22, headless PipeWire capture deadlocks
/// against PipeWire ≥ 1.6 (a loop-lock bug) and a stuck link head-blocks the whole daemon.
const MIN_GAMESCOPE: (u32, u32, u32) = (3, 16, 22);

/// Best-effort: warn loudly if the installed gamescope is older than [`MIN_GAMESCOPE`]. Parsing
/// failures are silent (don't block a possibly-fine custom build) — this is a diagnostic, not a
/// gate. Returns the parsed version when it could read one.
fn check_gamescope_version() -> Option<(u32, u32, u32)> {
    let out = Command::new("gamescope").arg("--version").output().ok()?;
    // gamescope prints the version banner to stderr on some builds, stdout on others.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ver = parse_version(&text)?;
    if ver < MIN_GAMESCOPE {
        tracing::warn!(
            found = %format!("{}.{}.{}", ver.0, ver.1, ver.2),
            min = %format!("{}.{}.{}", MIN_GAMESCOPE.0, MIN_GAMESCOPE.1, MIN_GAMESCOPE.2),
            "gamescope is older than the minimum for reliable headless capture — expect a \
             capture deadlock against PipeWire ≥ 1.6 (a wedged link head-blocks the daemon); \
             upgrade gamescope or use PUNKTFUNK_COMPOSITOR=kwin|mutter"
        );
    }
    Some(ver)
}

/// Extract the first `X.Y.Z` version triple from arbitrary text (e.g. `gamescope version 3.16.22`).
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let mut parts = token.split('.');
        let (a, b, c) = (parts.next()?, parts.next(), parts.next());
        let (Some(b), Some(c)) = (b, c) else { continue };
        if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), c.parse()) {
            return Some((a, b, c));
        }
    }
    None
}

/// Owns the spawned gamescope process; killing it tears the virtual output down.
struct GamescopeProc(Child);

impl Drop for GamescopeProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        // Clear the relayed EIS socket name so the host-lifetime injector can't reconnect to this
        // now-dead session's socket between sessions (the stale path is the "Connection refused").
        let _ = std::fs::remove_file(EI_SOCKET_FILE);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_version, MIN_GAMESCOPE};

    #[test]
    fn parses_version_banner() {
        assert_eq!(
            parse_version("gamescope version 3.16.22"),
            Some((3, 16, 22))
        );
        assert_eq!(
            parse_version("gamescope: version v3.15.9 (no PipeWire)"),
            Some((3, 15, 9))
        );
        assert_eq!(parse_version("3.16.20-1.fc41"), Some((3, 16, 20)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("only 3.16 here"), None); // needs a full triple
    }

    #[test]
    fn flags_known_bad_versions() {
        // The 26.04-shipped 3.16.20 is below the minimum (PipeWire 1.6 deadlock).
        assert!(parse_version("gamescope version 3.16.20").unwrap() < MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.16.22").unwrap() >= MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.17.0").unwrap() >= MIN_GAMESCOPE);
    }
}
