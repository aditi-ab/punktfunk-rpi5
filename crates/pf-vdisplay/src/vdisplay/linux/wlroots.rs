//! wlroots/Sway virtual-output backend via sway IPC + the xdg ScreenCast portal
//! (xdg-desktop-portal-wlr):
//!
//! 1. `swaymsg create_output` adds a headless output (`HEADLESS-N` — sway must run the
//!    headless backend, or have it co-loaded; the name is found by diffing
//!    `swaymsg -t get_outputs` before/after).
//! 2. `swaymsg output <NAME> mode --custom WxH@HzHz` sets the client's exact mode — a fresh
//!    headless output also *needs* a real mode for a refresh clock, or it produces no frames.
//! 3. The ScreenCast portal yields the output's PipeWire node. There is no GUI to pick an
//!    output headlessly, so xdpw is steered through its chooser hook: a managed config
//!    (`~/.config/xdg-desktop-portal-wlr/config`, written once + portal restarted on change)
//!    sets `chooser_type=simple` with a `chooser_cmd` that cats the chooser file, which we
//!    write per session (`Monitor: <NAME>` — xdpw 0.8 parses that prefix strictly).
//! 4. Teardown is RAII: drop stops the portal thread (its zbus connection ends the cast) and
//!    runs `swaymsg output <NAME> unplug` (headless outputs support unplug since sway 1.8).
//!
//! Requirements: the host runs inside the sway session's environment (`SWAYSOCK` for swaymsg,
//! and the portal activation env — `WAYLAND_DISPLAY`/`XDG_CURRENT_DESKTOP=sway` imported into
//! `systemctl --user`, see `scripts/headless/prepare-session.sh`), with the ScreenCast
//! interface routed to xdpw (`scripts/headless/portals.conf`).

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// File the xdpw output chooser reads the selected output from (see [`xdpw_config`]); we
/// write `Monitor: <NAME>\n` here right before the portal handshake selects sources. Lives
/// under `$XDG_RUNTIME_DIR` (per-user, mode 0700) — NOT a fixed world-writable /tmp path,
/// where another local user could pre-create it (DoS) or rewrite it between our write and
/// xdpw's read (steer capture at a different output).
fn chooser_file() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/punktfunk-xdpw-output")
}

/// The managed xdpw config: per-session output selection with no GUI. The `|| echo` fallback
/// keeps plain portal capture (`--source portal` flow) working when no session has written
/// the chooser file. xdpw runs `chooser_cmd` via `/bin/sh -c`, reads stdout.
fn xdpw_config() -> String {
    format!(
        "# managed by punktfunk (vdisplay/wlroots.rs) — per-session output selection.\n\
[screencast]\n\
chooser_type=simple\n\
chooser_cmd=cat {} 2>/dev/null || echo 'Monitor: HEADLESS-1'\n",
        chooser_file()
    )
}

/// The wlroots/Sway virtual-display driver. Stateless — each [`create`](VirtualDisplay::create)
/// adds one headless output and spins up a portal thread owning the cast on it.
pub struct WlrootsDisplay;

impl WlrootsDisplay {
    pub fn new() -> Result<Self> {
        Ok(WlrootsDisplay)
    }
}

/// wlroots/Sway is usable when the host runs inside a Sway session — signalled by `SWAYSOCK`
/// (the IPC socket `swaymsg create_output` needs). Cheap env check for the enumeration path.
pub fn is_available() -> bool {
    std::env::var_os("SWAYSOCK").is_some()
}

impl VirtualDisplay for WlrootsDisplay {
    fn name(&self) -> &'static str {
        "wlroots"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let before = output_names()
            .context("swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)")?;
        swaymsg(&["create_output"])
            .context("swaymsg create_output (sway needs the headless backend loaded)")?;
        // The output appears synchronously in practice; poll briefly to be safe, and own it
        // from here on so error unwinding unplugs it.
        let output = OutputGuard(wait_new_output(&before, Duration::from_secs(5))?);
        let name = output.0.clone();

        // The client's exact mode (also the refresh clock that makes the output produce frames).
        let m = format!(
            "{}x{}@{}Hz",
            mode.width,
            mode.height,
            mode.refresh_hz.max(1)
        );
        swaymsg(&["output", &name, "mode", "--custom", &m])
            .with_context(|| format!("swaymsg output {name} mode --custom {m}"))?;
        swaymsg(&["output", &name, "enable"])
            .with_context(|| format!("swaymsg output {name} enable"))?;

        // Steer xdpw's headless output chooser at our new output, then run the portal
        // handshake on its own thread (it parks to keep the cast alive, like the other backends).
        ensure_xdpw_config()?;
        let chooser = chooser_file();
        std::fs::write(&chooser, format!("Monitor: {name}\n"))
            .with_context(|| format!("write {chooser}"))?;
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        thread::Builder::new()
            .name("punktfunk-wlr-vout".into())
            .spawn(move || portal_thread(setup_tx, stop_thread))
            .context("spawn wlroots portal thread")?;

        let (fd, node_id) = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => bail!("ScreenCast portal on {name} failed: {e}"),
            Err(_) => bail!("timed out waiting for the ScreenCast portal on {name}"),
        };
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "sway headless output ready"
        );
        Ok(VirtualOutput {
            node_id,
            remote_fd: Some(fd),
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(Keepalive {
                _stop: StopGuard(stop),
                _output: output,
            }),
            // Owned (the compositor output is ours to tear down), but not registry-poolable: the
            // portal fd can't be re-opened per attach, so the registry passes it through on
            // `remote_fd.is_some()` (keep-alive stays off for wlroots until fresh-portal re-attach).
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
        })
    }
}

/// Drop order matters: stop the portal thread first (zbus connection drop ends the cast),
/// then unplug the output (fields drop in declaration order).
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// Dropping this ends the portal keepalive thread, closing its zbus connection — the portal
/// then tears the screencast session down.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Owns the created headless output; dropping it unplugs it from sway.
struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match swaymsg(&["output", &self.0, "unplug"]) {
            Ok(_) => tracing::info!(output = %self.0, "sway headless output unplugged"),
            Err(e) => tracing::warn!(output = %self.0, error = %format!("{e:#}"), "unplug failed"),
        }
    }
}

/// Run `swaymsg -- <args>`, returning stdout (`--` so command tokens like `--custom` reach
/// sway instead of swaymsg's own getopt). swaymsg exits non-zero (with the error on stderr/
/// stdout) when the command fails, so checking the status covers `{"success": false}` too.
fn swaymsg(args: &[&str]) -> Result<String> {
    let out = Command::new("swaymsg")
        .arg("--")
        .args(args)
        .output()
        .context("run swaymsg (is sway installed?)")?;
    if !out.status.success() {
        bail!(
            "swaymsg {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Current output names from `swaymsg -t get_outputs` (JSON).
fn output_names() -> Result<Vec<String>> {
    let out = Command::new("swaymsg")
        .args(["-t", "get_outputs", "--raw"])
        .output()
        .context("run swaymsg (is sway installed?)")?;
    if !out.status.success() {
        bail!(
            "swaymsg -t get_outputs failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    let outputs: serde_json::Value = serde_json::from_str(&raw).context("parse get_outputs")?;
    Ok(outputs
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| o.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect())
}

/// Wait for the output `create_output` added (the name not in `before` — HEADLESS-N).
fn wait_new_output(before: &[String], timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(name) = output_names()?
            .into_iter()
            .find(|n| !before.iter().any(|b| b == n))
        {
            return Ok(name);
        }
        if Instant::now() >= deadline {
            bail!("create_output succeeded but no new output appeared");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Make sure xdpw uses our output chooser. xdpw reads its config only at startup, so on a
/// change restart it if running (`try-restart`; if it isn't, D-Bus activation will start it
/// with the new config). The config itself is static — the *selection* is the chooser file.
fn ensure_xdpw_config() -> Result<()> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
    let dir = base.join("xdg-desktop-portal-wlr");
    let path = dir.join("config");
    let cfg = xdpw_config();
    if std::fs::read_to_string(&path).is_ok_and(|c| c == cfg) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    std::fs::write(&path, &cfg).with_context(|| format!("write {}", path.display()))?;
    tracing::info!(path = %path.display(), "wrote managed xdg-desktop-portal-wlr config");
    let _ = Command::new("systemctl")
        .args(["--user", "try-restart", "xdg-desktop-portal-wlr.service"])
        .status();
    Ok(())
}

/// The ScreenCast portal handshake (same shape as the capture module's portal thread, but it
/// reports the fd + node id and parks until stopped — the zbus connection is the cast's
/// lifetime). xdpw answers the source selection via the chooser, no dialog.
fn portal_thread(setup_tx: Sender<Result<(OwnedFd, u32), String>>, stop: Arc<AtomicBool>) {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Multi-thread runtime: the zbus background reader must be pumped across the
    // create_session → select_sources → start handshake (see capture/linux.rs).
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    let err_tx = setup_tx.clone();

    rt.block_on(async move {
        let result: Result<()> = async {
            let proxy = Screencast::new().await.context(
                "connect ScreenCast portal (is xdg-desktop-portal running with the wlr backend?)",
            )?;
            let session = proxy
                .create_session(Default::default())
                .await
                .context("create_session")?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        // xdpw offers MONITOR only; the chooser picks our output.
                        .set_sources(BitFlags::from_flag(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .context("select_sources")?
                .response()
                .context("select_sources rejected")?;
            let streams = proxy
                .start(&session, None, Default::default())
                .await
                .context("start cast")?
                .response()
                .context("start response (chooser declined? check the xdpw config/chooser file)")?;
            let stream = streams
                .streams()
                .first()
                .context("portal returned no streams")?
                .clone();
            let node_id = stream.pipe_wire_node_id();
            let fd = proxy
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .context("open_pipe_wire_remote")?;

            setup_tx
                .send(Ok((fd, node_id)))
                .map_err(|_| anyhow!("virtual-output opener went away"))?;

            // Park, keeping `proxy` + `session` (the zbus connection) alive until stopped —
            // the cast is torn down when the connection drops.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
}
