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
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdpw-output")
}

/// The chooser command xdpw runs via `/bin/sh -c`, reading stdout. The `|| echo` fallback keeps
/// plain portal capture (`--source portal`) working when no session has written the chooser file.
fn chooser_cmd() -> String {
    format!(
        "cat {} 2>/dev/null || echo 'Monitor: HEADLESS-1'",
        chooser_file()
    )
}

/// The wlroots/Sway virtual-display driver. Stateless — each [`create`](VirtualDisplay::create)
/// adds one headless output and spins up a portal thread owning the cast on it.
pub struct WlrootsDisplay {
    /// Out-of-band cursor request (`set_hw_cursor`, the negotiated cursor channel): portal
    /// `CursorMode::Metadata` — shapes/positions ride `SPA_META_Cursor` for the channel + the
    /// composite blend. Off (every non-channel session): `Embedded` — the compositor paints the
    /// pointer into frames, zero host-side cursor work (the pre-channel default this backend
    /// always had). ⚠️ Metadata is UNTESTED on-glass for this backend (Phase B wired it so the
    /// channel isn't silently dead here; KWin/Mutter are the validated legs).
    hw_cursor: bool,
}

impl WlrootsDisplay {
    pub fn new() -> Result<Self> {
        Ok(WlrootsDisplay { hw_cursor: false })
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

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
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

        // Steer xdpw's headless output chooser at our new output, then run the portal handshake on
        // its own thread (it parks to keep the cast alive, like the other backends). Serialized:
        // the chooser is one per-user file, so a concurrent session's write between ours and xdpw's
        // read would silently capture the wrong output (see `SELECTION_LOCK`).
        let (fd, node_id, stop) = {
            let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            select_and_cast(&name, self.hw_cursor)?
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
            expect_exact_dims: false,
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

/// Run a swaymsg **query** (`-t <kind> --raw`) and parse its JSON.
///
/// ⚠️ Deliberately NOT [`swaymsg`]: that helper inserts `--` so its arguments are read as a sway
/// *command*, which is right for `create_output` and wrong for a query — `-t` after `--` comes back
/// as `Unknown/invalid command '-t'` (caught on-glass writing the monitor enumeration).
fn swaymsg_query(kind: &str) -> Result<serde_json::Value> {
    let out = Command::new("swaymsg")
        .args(["-t", kind, "--raw"])
        .output()
        .context("run swaymsg (is sway installed?)")?;
    if !out.status.success() {
        bail!(
            "swaymsg -t {kind} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    serde_json::from_str(&raw).with_context(|| format!("parse {kind}"))
}

/// Current output names from `swaymsg -t get_outputs` (JSON).
fn output_names() -> Result<Vec<String>> {
    let outputs = swaymsg_query("get_outputs")?;
    Ok(outputs
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| o.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect())
}

/// Serializes **write-the-chooser → complete-the-handshake**, process-wide.
///
/// The chooser is a single per-user file: whoever writes last before xdpw reads wins. Two sessions
/// starting at once (or a mirror starting beside a virtual output) would otherwise race, and the
/// loser doesn't fail — it silently captures the *other* session's output. Held across the portal
/// handshake, not just the write, because the read happens inside it.
static SELECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point xdpw's chooser at `output` and run the ScreenCast handshake, returning the portal fd +
/// node id and the guard that stops the cast. The caller must hold [`SELECTION_LOCK`].
fn select_and_cast(output: &str, hw_cursor: bool) -> Result<(OwnedFd, u32, Arc<AtomicBool>)> {
    ensure_xdpw_config()?;
    let chooser = chooser_file();
    std::fs::write(&chooser, format!("Monitor: {output}\n"))
        .with_context(|| format!("write {chooser}"))?;
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-wlr-cast".into())
        .spawn(move || portal_thread(setup_tx, stop_thread, hw_cursor))
        .context("spawn wlroots portal thread")?;
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id))) => Ok((fd, node_id, stop)),
        Ok(Err(e)) => bail!("ScreenCast portal on {output} failed: {e}"),
        Err(_) => bail!("timed out waiting for the ScreenCast portal on {output}"),
    }
}

/// Record an **existing** sway output — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md` L3). Same chooser mechanism the virtual-output path
/// uses, pointed at a physical connector instead of a headless one we created, so it inherits the
/// "no GUI picker" property a background service needs.
///
/// The keepalive stops the cast and nothing else: sway keeps the monitor, because we never made it.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (fd, node_id, stop) = select_and_cast(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: Some(fd),
        keepalive: Box::new(StopGuard(stop)),
    })
}

/// Every head sway reports, for [`crate::monitors::list`].
///
/// `swaymsg -t get_outputs` reports `rect` in the logical coordinate space (post-scale,
/// post-transform) — what `crate::monitors` documents. An inactive output has no `current_mode`, so
/// its mode reads as zeros rather than a guess.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let parsed = swaymsg_query("get_outputs")?;
    let mut out: Vec<_> = parsed
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| {
            let connector = o.get("name")?.as_str()?.to_string();
            let rect = |k: &str| {
                o.get("rect")
                    .and_then(|r| r.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            };
            let mode = |k: &str| {
                o.get("current_mode")
                    .and_then(|m| m.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            };
            let str_field = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
            Some(crate::monitors::PhysicalMonitor {
                description: crate::monitors::describe(
                    str_field("make"),
                    str_field("model"),
                    &connector,
                ),
                width: mode("width").max(0) as u32,
                height: mode("height").max(0) as u32,
                // sway reports `refresh` in mHz already.
                refresh_mhz: mode("refresh").max(0) as u32,
                x: rect("x") as i32,
                y: rect("y") as i32,
                scale: o
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: o
                    .get("primary")
                    .and_then(|v| v.as_bool())
                    .or_else(|| o.get("focused").and_then(|v| v.as_bool()))
                    .unwrap_or(false),
                enabled: o.get("active").and_then(|v| v.as_bool()).unwrap_or(true),
                // Sway auto-names headless outputs `HEADLESS-N` and that is what `create` adds. A
                // sway started with its own headless output would match too — hence best-effort.
                managed: connector.starts_with("HEADLESS-"),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
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
    let path = base.join("xdg-desktop-portal-wlr").join("config");
    // The two keys we own, set IN PLACE. This used to `fs::write` a complete file over whatever the
    // user had, destroying every other xdpw setting they owned on first connect.
    let mut changed = crate::portal_config::ensure_key(
        &path,
        crate::portal_config::Block::Ini("screencast"),
        "chooser_type",
        "simple",
    )?;
    changed |= crate::portal_config::ensure_key(
        &path,
        crate::portal_config::Block::Ini("screencast"),
        "chooser_cmd",
        &chooser_cmd(),
    )?;
    if !changed {
        return Ok(());
    }
    tracing::info!(path = %path.display(), "pointed xdg-desktop-portal-wlr at the managed output chooser");
    let _ = Command::new("systemctl")
        .args(["--user", "try-restart", "xdg-desktop-portal-wlr.service"])
        .status();
    Ok(())
}

/// The ScreenCast portal handshake (same shape as the capture module's portal thread, but it
/// reports the fd + node id and parks until stopped — the zbus connection is the cast's
/// lifetime). xdpw answers the source selection via the chooser, no dialog.
fn portal_thread(
    setup_tx: Sender<Result<(OwnedFd, u32), String>>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    // Portal cursor mode per the session's channel negotiation (see the struct doc).
    let cursor_mode = if hw_cursor {
        CursorMode::Metadata
    } else {
        CursorMode::Embedded
    };
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
                        .set_cursor_mode(cursor_mode)
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
