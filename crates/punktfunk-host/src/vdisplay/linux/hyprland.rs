//! Hyprland virtual-output backend via `hyprctl` IPC + the xdg ScreenCast portal
//! (xdg-desktop-portal-hyprland / xdph). See `design/hyprland-support.md`.
//!
//! Hyprland dropped wlroots in v0.42 (aquamarine backend) but kept the client-facing wlr
//! protocols, so it shares the wlr virtual-input path with sway — but it needs its own IPC and
//! portal, so it is a **distinct backend** from [`super::wlroots`], not a branch inside it (D1):
//!
//! 1. `hyprctl output create headless PF-<n>` adds a named headless output — Hyprland supports
//!    **explicit names**, so no before/after diffing like sway's `HEADLESS-N` (D6). We poll
//!    `hyprctl -j monitors` until the name shows up.
//! 2. A monitor rule sets the client's exact mode. The grammar changed in 0.55 (hyprlang →
//!    Lua), so [`set_monitor_rule`] tries both eras (D5): `hyprctl keyword monitor NAME,WxH@Hz,…`
//!    (≤0.54, still loads on 0.55 during the deprecation window) and `hyprctl eval 'hl.monitor{…}'`
//!    (≥0.55), ordered by the detected version and falling back to the other.
//! 3. The xdg ScreenCast portal (served by **xdph**) yields the output's PipeWire node. There is
//!    no GUI to pick an output headlessly, so xdph is steered through its **custom picker**: a
//!    managed config (`~/.config/hypr/xdph.conf`) points `custom_picker_binary` at a tiny installed
//!    shim that cats a per-session selection file we write (`screen:<NAME>`) right before the
//!    handshake — byte-for-byte the xdpw pattern, different picker wire format.
//! 4. Teardown is RAII: drop stops the portal thread (its zbus connection ends the cast) and runs
//!    `hyprctl output remove NAME`.
//!
//! Requirements: the host runs inside (or can reach) the Hyprland session — either
//! `HYPRLAND_INSTANCE_SIGNATURE` is inherited, or [`is_available`] discovers it from
//! `$XDG_RUNTIME_DIR/hypr/` and [`super::super::apply_session_env`] exports it for `hyprctl` — with
//! the ScreenCast interface routed to xdph (`scripts/headless/portals.conf`).
//!
//! ⚠ **Spike-pending contract** (`design/hyprland-support.md` Phase 0): the xdph `custom_picker_binary`
//! stdout format and config path, and the `hl.monitor{}` Lua signature, are documented best-guesses
//! here. A wrong picker line degrades to the interactive picker (per the plan's risk table), and
//! `set_monitor_rule` tries both eras, so a wrong guess self-heals rather than breaking capture.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Per-session file the xdph custom picker reads the selected output from. We write
/// `screen:<NAME>\n` here right before the portal handshake selects sources. Lives under
/// `$XDG_RUNTIME_DIR` (per-user, 0700) — NOT a world-writable /tmp path another local user could
/// pre-create or rewrite between our write and xdph's read (steer capture elsewhere). Mirrors the
/// wlroots chooser file.
fn selection_file() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/punktfunk-xdph-output")
}

/// The installed custom-picker shim: a tiny script that cats [`selection_file`]. xdph runs
/// `custom_picker_binary` and reads one selection line from its stdout; an empty read (no session
/// has written the file) leaves xdph to its interactive picker — the graceful fallback.
fn picker_shim_path() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/punktfunk-xdph-picker.sh")
}

/// The picker line for output `name`. ⚠ Spike-pending: xdph's custom-picker stdout contract is
/// `screen:<output-name>` (vs `window:<addr>` / `region:…`) to the best of current knowledge — this
/// is the single place to correct once the Phase-0 spike pins it. A wrong value just means xdph
/// falls back to its interactive picker (degraded, not broken).
fn picker_selection_line(name: &str) -> String {
    format!("screen:{name}\n")
}

/// The managed xdph config: point the screencopy custom picker at our shim so headless output
/// selection needs no GUI. xdph reads its config at startup, so a change restarts it (see
/// [`ensure_xdph_config`]). The *selection* is the per-session file, not this static config.
fn xdph_config() -> String {
    format!(
        "# managed by punktfunk (vdisplay/hyprland.rs) — headless per-session output selection.\n\
screencopy {{\n\
    custom_picker_binary = {}\n\
}}\n",
        picker_shim_path()
    )
}

/// Monotonic per-process counter for headless output names (`PF-1`, `PF-2`, …). Named outputs kill
/// the before/after diff race sway needs (D6).
static OUTPUT_SEQ: AtomicU32 = AtomicU32::new(0);

fn next_output_name() -> String {
    format!("PF-{}", OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1)
}

/// The Hyprland virtual-display driver. Stateless — each [`create`](VirtualDisplay::create) adds one
/// named headless output and spins up a portal thread owning the cast on it.
pub struct HyprlandDisplay;

impl HyprlandDisplay {
    pub fn new() -> Result<Self> {
        Ok(HyprlandDisplay)
    }
}

/// Hyprland is usable when a live Hyprland instance for our uid is reachable — signalled by
/// `HYPRLAND_INSTANCE_SIGNATURE` (inherited from the session) **or** a discoverable instance socket
/// under `$XDG_RUNTIME_DIR/hypr/*/.socket.sock` (so the systemd `--user` host works without env
/// import, unlike sway's `SWAYSOCK`; the signature is then exported by `apply_session_env`). Cheap,
/// side-effect-free — safe on the enumeration path.
pub fn is_available() -> bool {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return true;
    }
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => std::path::PathBuf::from(d).join("hypr"),
        None => return false,
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join(".socket.sock").exists())
}

/// Pre-flight for the Hyprland backend: `hyprctl` must reach the compositor (a clear error now
/// beats a create-time failure), and if the permission system is enforcing, warn about the silent
/// black-frame / dropped-input failure mode.
pub fn probe() -> Result<()> {
    hyprctl(&["-j", "version"]).context(
        "hyprctl not reachable — is Hyprland running and HYPRLAND_INSTANCE_SIGNATURE set? (the \
         host must run inside, or be able to reach, the Hyprland session)",
    )?;
    warn_if_permissions_enforced();
    Ok(())
}

impl VirtualDisplay for HyprlandDisplay {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Log the permission-system caveat once per process (silent black frames otherwise).
        preflight_once();

        let name = next_output_name();
        hyprctl_dispatch(&["output", "create", "headless", &name])
            .with_context(|| format!("hyprctl output create headless {name} (is hyprctl reachable?)"))?;
        // Own the output from here on so any later error (or drop) removes it.
        let output = OutputGuard(name.clone());
        wait_monitor_ready(&name, Duration::from_secs(5))
            .with_context(|| format!("waiting for headless output {name} to appear"))?;

        // The client's exact mode (also the frame clock — a headless output is timer-paced from it).
        set_monitor_rule(&name, mode).with_context(|| format!("set monitor rule for {name}"))?;

        // Steer xdph's custom picker at our new output, then run the portal handshake on its own
        // thread (it parks to keep the cast alive, like the other backends).
        ensure_xdph_config()?;
        let sel = selection_file();
        std::fs::write(&sel, picker_selection_line(&name))
            .with_context(|| format!("write {sel}"))?;

        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        thread::Builder::new()
            .name("punktfunk-hypr-vout".into())
            .spawn(move || portal_thread(setup_tx, stop_thread))
            .context("spawn hyprland portal thread")?;

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
            "hyprland headless output ready"
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
            // `remote_fd.is_some()` — same as wlroots.
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
        })
    }
}

/// Drop order matters: stop the portal thread first (zbus connection drop ends the cast), then
/// remove the output (fields drop in declaration order).
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// Dropping this ends the portal keepalive thread, closing its zbus connection — the portal then
/// tears the screencast session down.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Owns the created headless output; dropping it removes it from Hyprland.
struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match hyprctl_dispatch(&["output", "remove", &self.0]) {
            Ok(_) => tracing::info!(output = %self.0, "hyprland headless output removed"),
            Err(e) => {
                tracing::warn!(output = %self.0, error = %format!("{e:#}"), "output remove failed")
            }
        }
    }
}

/// Run `hyprctl <args>`, returning stdout. `hyprctl` reads `HYPRLAND_INSTANCE_SIGNATURE` from the
/// env (exported by `apply_session_env`) to reach the right instance socket. It exits non-zero on a
/// hard failure, but for dispatch commands it can print an error with status 0 — see
/// [`hyprctl_dispatch`].
fn hyprctl(args: &[&str]) -> Result<String> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .context("run hyprctl (is Hyprland installed?)")?;
    if !out.status.success() {
        bail!(
            "hyprctl {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a `hyprctl` **dispatch** command (`output …`, `keyword …`, `eval …`) that reports success by
/// printing `ok`. hyprctl often exits 0 even when the command is rejected, printing the error to
/// stdout, so treat a known error marker as failure (this is also how [`set_monitor_rule`] tells the
/// two config eras apart).
fn hyprctl_dispatch(args: &[&str]) -> Result<()> {
    let out = hyprctl(args)?;
    let t = out.trim();
    let lc = t.to_ascii_lowercase();
    if lc.contains("invalid")
        || lc.contains("not found")
        || lc.contains("couldn't")
        || lc.contains("could not")
        || lc.contains("unknown")
        || lc.contains("no such")
        || lc.contains("error")
    {
        bail!("hyprctl {:?} rejected: {t}", args);
    }
    Ok(())
}

/// Wait until the named headless output shows up in `hyprctl -j monitors` (it appears near-instantly
/// in practice; poll briefly to be safe).
fn wait_monitor_ready(name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if monitor_exists(name)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("output create succeeded but monitor {name} never appeared");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Is a monitor named `name` present in `hyprctl -j monitors` (JSON)?
fn monitor_exists(name: &str) -> Result<bool> {
    let out = hyprctl(&["-j", "monitors"])?;
    let monitors: serde_json::Value =
        serde_json::from_str(&out).context("parse hyprctl -j monitors")?;
    Ok(monitors
        .as_array()
        .map(|a| {
            a.iter()
                .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(name))
        })
        .unwrap_or(false))
}

/// Set the client's exact mode on `name`, supporting both config eras (D5). Encapsulates the whole
/// era split: ≤0.54 uses `hyprctl keyword monitor NAME,WxH@Hz,auto,1`; ≥0.55 (hyprlang → Lua)
/// prefers `hyprctl eval 'hl.monitor{…}'`. Whichever era we detect, we try its form first and fall
/// back to the other — hyprlang configs still load during the 0.55 deprecation window, so `keyword`
/// keeps working for a while, and the fallback makes a wrong version guess self-heal.
fn set_monitor_rule(name: &str, mode: Mode) -> Result<()> {
    let hz = mode.refresh_hz.max(1);
    let spec = format!("{name},{}x{}@{hz},auto,1", mode.width, mode.height);
    let lua = format!(
        "hl.monitor{{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"auto\", scale = 1 }}",
        mode.width, mode.height
    );
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    let attempts: [&Vec<&str>; 2] = if hyprland_version_at_least(0, 55) {
        [&eval, &keyword]
    } else {
        [&keyword, &eval]
    };
    let mut errs = Vec::new();
    for a in attempts {
        match hyprctl_dispatch(a) {
            Ok(()) => return Ok(()),
            Err(e) => errs.push(format!("{e:#}")),
        }
    }
    bail!(
        "hyprctl monitor rule failed on both config eras: {}",
        errs.join(" | ")
    )
}

/// The running Hyprland `(major, minor, patch)` from `hyprctl -j version` (`tag` like `v0.55.0`),
/// cached for the process. A compositor upgrade + restart mid-host-life would leave this stale, but
/// [`set_monitor_rule`] tries both eras regardless, so the cache is an optimization, not correctness.
fn hyprland_version() -> Option<(u16, u16, u16)> {
    static V: OnceLock<Option<(u16, u16, u16)>> = OnceLock::new();
    *V.get_or_init(|| {
        let out = hyprctl(&["-j", "version"]).ok()?;
        let json: serde_json::Value = serde_json::from_str(&out).ok()?;
        parse_version_tag(json.get("tag").and_then(|t| t.as_str())?)
    })
}

/// Parse a Hyprland `tag` (`v0.55.0`, or a dev `v0.41.2-13-gabcdef`) to `(major, minor, patch)`.
fn parse_version_tag(tag: &str) -> Option<(u16, u16, u16)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split(['.', '-', '_', '+']);
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Is the running Hyprland at least `major.minor`? Unknown version → `false` (assume the older
/// `keyword` era, which the fallback covers anyway).
fn hyprland_version_at_least(major: u16, minor: u16) -> bool {
    matches!(hyprland_version(), Some((a, b, _)) if (a, b) >= (major, minor))
}

/// Log the permission-system caveat at most once per process: with
/// `ecosystem.enforce_permissions = true` (0.49+, off by default), direct screencopy/virtual-input
/// clients can be denied — and denial is **silent black frames / dropped input**, not an error.
fn preflight_once() {
    static WARNED: Once = Once::new();
    WARNED.call_once(warn_if_permissions_enforced);
}

fn warn_if_permissions_enforced() {
    let Ok(out) = hyprctl(&["-j", "getoption", "ecosystem:enforce_permissions"]) else {
        return;
    };
    let on = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|j| j.get("int").and_then(|v| v.as_i64()))
        .is_some_and(|v| v != 0);
    if on {
        tracing::warn!(
            "Hyprland ecosystem.enforce_permissions is ON — screencopy/virtual-input may be denied \
             as SILENT black frames / dropped input. Grant the host with hl.permission rules \
             (screencopy + virtual pointer/keyboard) — see docs/hyprland."
        );
    }
}

/// Make sure xdph uses our custom picker: install the shim (once) and write the managed config,
/// restarting xdph if the config changed (it reads config only at startup). Mirrors the wlroots
/// `ensure_xdpw_config` pattern.
fn ensure_xdph_config() -> Result<()> {
    // 1. Install the picker shim (idempotent — content is fixed).
    let shim = picker_shim_path();
    let sel = selection_file();
    let shim_body = format!("#!/bin/sh\nexec cat \"{sel}\" 2>/dev/null\n");
    if std::fs::read_to_string(&shim).is_ok_and(|c| c == shim_body) {
        // already installed
    } else {
        std::fs::write(&shim, &shim_body).with_context(|| format!("write {shim}"))?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod {shim}"))?;
        }
    }

    // 2. Write the managed xdph config and restart xdph on change.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
    let dir = base.join("hypr");
    let path = dir.join("xdph.conf");
    let cfg = xdph_config();
    if std::fs::read_to_string(&path).is_ok_and(|c| c == cfg) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    std::fs::write(&path, &cfg).with_context(|| format!("write {}", path.display()))?;
    tracing::info!(path = %path.display(), "wrote managed xdg-desktop-portal-hyprland config");
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "try-restart",
            "xdg-desktop-portal-hyprland.service",
        ])
        .status();
    Ok(())
}

/// The ScreenCast portal handshake — the xdg ScreenCast portal is backend-neutral (served here by
/// xdph), so this mirrors the wlroots portal thread: it reports the fd + node id and parks until
/// stopped (the zbus connection is the cast's lifetime). xdph answers source selection via our
/// custom picker, no dialog. (Kept separate from wlroots' copy so each wlr-family backend stays
/// self-owned per D1; unify if they ever diverge no further.)
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
                "connect ScreenCast portal (is xdg-desktop-portal running with the hyprland backend/xdph?)",
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
                        // xdph offers MONITOR; the custom picker selects our output.
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
                .context("start response (custom picker declined? check the xdph config/shim/selection file)")?;
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

            // Park, keeping `proxy` + `session` (the zbus connection) alive until stopped — the cast
            // is torn down when the connection drops.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tag_parses_release_and_dev_builds() {
        assert_eq!(parse_version_tag("v0.55.0"), Some((0, 55, 0)));
        assert_eq!(parse_version_tag("0.41.2"), Some((0, 41, 2)));
        // Dev builds tack the commit distance + hash on with a dash.
        assert_eq!(parse_version_tag("v0.41.2-13-gabcdef"), Some((0, 41, 2)));
        // Missing patch defaults to 0; garbage is rejected.
        assert_eq!(parse_version_tag("v1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version_tag("wat"), None);
    }

    #[test]
    fn output_names_are_unique_and_prefixed() {
        let a = next_output_name();
        let b = next_output_name();
        assert!(a.starts_with("PF-") && b.starts_with("PF-"));
        assert_ne!(a, b);
    }

    #[test]
    fn picker_line_is_screen_scoped() {
        assert_eq!(picker_selection_line("PF-1"), "screen:PF-1\n");
    }
}
