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
//! Input is a gamescope-specific libei/EIS socket (`LIBEI_SOCKET`), wired separately (TODO).

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The gamescope virtual-display driver. Each [`create`](VirtualDisplay::create) spawns one
/// headless gamescope process sized to the requested mode.
pub struct GamescopeDisplay;

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
        // Attach to an already-running gamescope (debug / Steam-launched session) instead of
        // spawning one: LUMEN_GAMESCOPE_NODE=<pipewire node id>.
        if let Ok(id) = std::env::var("LUMEN_GAMESCOPE_NODE") {
            let node_id: u32 = id
                .parse()
                .context("LUMEN_GAMESCOPE_NODE must be a node id")?;
            tracing::info!(node_id, "gamescope: attaching to existing PipeWire node");
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                keepalive: Box::new(()),
            });
        }
        let proc = GamescopeProc(spawn(mode.width, mode.height, mode.refresh_hz.max(1))?);
        // gamescope creates its PipeWire node a moment after start; poll for it (the proc is held
        // alive meanwhile, and killed if we give up).
        let node_id = wait_for_node(Duration::from_secs(15)).ok_or_else(|| {
            anyhow!(
                "gamescope PipeWire node did not appear within 15s — gamescope may have failed to \
                 start or headless capture is unsupported on this GPU/driver (see /tmp/lumen-gamescope.log)"
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
            keepalive: Box::new(proc),
        })
    }
}

/// Spawn `gamescope --backend headless -W w -H h -r hz -- <app>`. The app comes from
/// `LUMEN_GAMESCOPE_APP` (default a no-op that just keeps gamescope alive — set it to a real
/// game/GL app for actual content). stdout/stderr go to `/tmp/lumen-gamescope.log`.
fn spawn(w: u32, h: u32, hz: u32) -> Result<Child> {
    let app = std::env::var("LUMEN_GAMESCOPE_APP").unwrap_or_else(|_| "sleep infinity".to_string());
    let mut cmd = Command::new("gamescope");
    cmd.args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &hz.to_string()])
        .args(["--xwayland-count", "1", "--"])
        .args(app.split_whitespace())
        // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
        .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia");
    if let Ok(log) = std::fs::File::create("/tmp/lumen-gamescope.log") {
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
    let log = std::fs::read_to_string("/tmp/lumen-gamescope.log").ok()?;
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
fn find_gamescope_node() -> Option<u32> {
    let out = Command::new("pw-dump").output().ok()?;
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    for obj in dump.as_array()? {
        if obj.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let props = obj.get("info").and_then(|i| i.get("props"));
        let name = props
            .and_then(|p| p.get("node.name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let class = props
            .and_then(|p| p.get("media.class"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        if name == "gamescope" || (class == "Video/Source" && name.contains("gamescope")) {
            return obj.get("id").and_then(|i| i.as_u64()).map(|x| x as u32);
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
    }
}
