//! Hyprland virtual-output backend via `hyprctl` IPC + the xdg ScreenCast portal
//! (xdg-desktop-portal-hyprland / xdph). See `design/hyprland-support.md`.
//!
//! Hyprland dropped wlroots in v0.42 (aquamarine backend) but kept the client-facing wlr
//! protocols, so it shares the wlr virtual-input path with sway — but it needs its own IPC and
//! portal, so it is a **distinct backend** from [`super::wlroots`], not a branch inside it (D1):
//!
//! 1. `hyprctl output create headless PF-<pid>-<n>` adds a named headless output — Hyprland supports
//!    **explicit names**, so no before/after diffing like sway's `HEADLESS-N` (D6). We poll
//!    `hyprctl -j monitors` until the name shows up. The creator's pid rides in the name so a
//!    crashed host's leftovers are attributable, and only those (see [`reclaim_leftovers_once`]).
//! 2. A monitor rule sets the client's exact mode. [`set_monitor_rule`] uses `hyprctl keyword
//!    monitor NAME,WxH@Hz,auto,1` (the hyprlang path — the default config manager on every current
//!    release, ≥0.55 included) and falls back to the Lua `hyprctl eval 'hl.monitor{…}'` only for a
//!    user on the opt-in Lua config manager, confirming the output actually adopted the mode (D5).
//! 3. The xdg ScreenCast portal (served by **xdph**) yields the output's PipeWire node. There is
//!    no GUI to pick an output headlessly, so xdph is steered through its **custom picker**: a
//!    managed config (`~/.config/hypr/xdph.conf`) points `screencopy:custom_picker_binary` at a tiny
//!    installed shim that cats a per-session selection file we write (`[SELECTION]screen:<NAME>`)
//!    right before the handshake — byte-for-byte the xdpw pattern, xdph's picker wire format.
//! 4. Teardown is RAII: drop stops the portal thread (its zbus connection ends the cast) and runs
//!    `hyprctl output remove NAME`.
//!
//! Requirements: the host runs inside (or can reach) the Hyprland session — either
//! `HYPRLAND_INSTANCE_SIGNATURE` is inherited, or [`is_available`] discovers it from
//! `$XDG_RUNTIME_DIR/hypr/` and [`super::super::apply_session_env`] exports it for `hyprctl` — with
//! the ScreenCast interface routed to xdph (`scripts/headless/portals.conf`).
//!
//! Contracts verified on **Hyprland 0.55.4 + xdph 1.3.x** (`design/hyprland-support.md` Phase 0):
//! `hyprctl` subcommands / JSON shapes, the `[SELECTION]screen:<name>` picker format, the
//! `~/.config/hypr/xdph.conf` path + `screencopy:custom_picker_binary` key, and that `eval` needs
//! the Lua config manager. Not yet exercised end-to-end on real DRM hardware: a headless output's
//! GBM/dmabuf allocation (fails on a nested/NVIDIA test box — Sunshine#4197); `set_monitor_rule`
//! surfaces that as a clear error instead of streaming a 0×0 output.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Once};
use std::thread;
use std::time::{Duration, Instant};

/// Per-session file the xdph custom picker reads the selected output from. We write
/// `screen:<NAME>\n` here right before the portal handshake selects sources. Lives under
/// `$XDG_RUNTIME_DIR` (per-user, 0700) — NOT a world-writable /tmp path another local user could
/// pre-create or rewrite between our write and xdph's read (steer capture elsewhere). Mirrors the
/// wlroots chooser file.
fn selection_file() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-output")
}

/// The installed custom-picker shim: a tiny script that cats [`selection_file`]. xdph runs
/// `custom_picker_binary` and reads one selection line from its stdout; an empty read (no session
/// has written the file) leaves xdph to its interactive picker — the graceful fallback.
fn picker_shim_path() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-picker.sh")
}

/// The picker line for output `name`. Verified against xdph 1.3.x / hyprland-share-picker on
/// Hyprland 0.55.4: xdph reads the custom picker's stdout and requires the `[SELECTION]` marker
/// followed by `screen:<name>` (or `window:<addr>` / `region:…`); anything else is rejected as
/// "strange output" and falls back to the interactive picker. So a monitor selection is
/// `[SELECTION]screen:<name>`.
fn picker_selection_line(name: &str) -> String {
    format!("[SELECTION]screen:{name}\n")
}

/// Monotonic per-process counter for headless output names (`PF-<pid>-1`, `PF-<pid>-2`, …). Named
/// outputs kill the before/after diff race sway needs (D6).
static OUTPUT_SEQ: AtomicU32 = AtomicU32::new(0);

/// The name for our next headless output: `PF-<pid>-<n>`.
///
/// The pid is not decoration. `OutputGuard::drop` is the only thing that removes an output, so a
/// host that was SIGKILLed leaves its outputs in the compositor — and a bare `PF-<n>` counter starts
/// again at `PF-1` in the next process, colliding with the corpses it just inherited. Stamping the
/// creator's pid into the name makes a leftover both recognisable and *attributable*, which is what
/// lets [`reclaim_leftovers_once`] remove only the ones whose owner is gone.
fn next_output_name() -> String {
    format!(
        "PF-{}-{}",
        std::process::id(),
        OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// Is `name` an output some punktfunk host created (`PF-<pid>-<n>`, or a legacy `PF-<n>`)? Pure —
/// this is what [`list_monitors`] reports as `managed`, so a user's own monitor called `PF-office`
/// must not qualify.
fn is_managed_output(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("PF-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The pid of the host that created `name`, for `PF-<pid>-<n>` only. `None` for anything else —
/// including a legacy `PF-<n>` from a host older than this naming scheme, which carries no owner and
/// therefore may not be reclaimed on a guess.
fn output_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("PF-")?;
    let (pid, seq) = rest.split_once('-')?;
    seq.parse::<u32>().ok()?;
    pid.parse::<u32>().ok()
}

/// The Hyprland virtual-display driver. Stateless — each [`create`](VirtualDisplay::create) adds one
/// named headless output and spins up a portal thread owning the cast on it.
pub struct HyprlandDisplay {
    /// Out-of-band cursor request (`set_hw_cursor`, the negotiated cursor channel): portal
    /// `CursorMode::Metadata` — shapes/positions ride `SPA_META_Cursor` for the channel + the
    /// composite blend. Off (every non-channel session): `Embedded` — the compositor paints the
    /// pointer into frames, zero host-side cursor work (the pre-channel default this backend
    /// always had). ⚠️ Metadata is UNTESTED on-glass for this backend (Phase B wired it so the
    /// channel isn't silently dead here; KWin/Mutter are the validated legs).
    hw_cursor: bool,
}

impl HyprlandDisplay {
    pub fn new() -> Result<Self> {
        Ok(HyprlandDisplay { hw_cursor: false })
    }
}

/// Hyprland is usable when a live Hyprland instance for our uid is reachable — signalled by
/// `HYPRLAND_INSTANCE_SIGNATURE` (inherited from the session) **or** a discoverable instance socket
/// under `$XDG_RUNTIME_DIR/hypr/*/.socket.sock` (so the systemd `--user` host works without env
/// import, unlike sway's `SWAYSOCK`; the signature is then exported by `apply_session_env`). Cheap,
/// side-effect-free — safe on the enumeration path.
///
/// Both env reads take [`crate::with_env_lock`] — in ONE scope, so the pair is sampled from a single
/// consistent view. This runs on a management worker (`/host/compositors` → [`crate::available`])
/// concurrently with another connect's `apply_session_env`, which `set_var`s the signature for a
/// live Hyprland session and `remove_var`s it for anything else; a glibc `getenv` racing that
/// `setenv`/`unsetenv` is the `environ` realloc data race ENV_LOCK exists for. No caller holds the
/// lock (it is not reentrant), and the `read_dir` below deliberately runs outside it.
pub fn is_available() -> bool {
    let (sig, runtime) = crate::with_env_lock(|| {
        (
            std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
    });
    if sig.is_some() {
        return true;
    }
    let dir = match runtime {
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
    if let Some((maj, min, pat)) = hyprland_version() {
        tracing::info!(version = %format!("{maj}.{min}.{pat}"), "Hyprland backend ready");
    }
    warn_if_permissions_enforced();
    Ok(())
}

impl VirtualDisplay for HyprlandDisplay {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Log the permission-system caveat once per process (silent black frames otherwise).
        preflight_once();
        // Remove any output a PREVIOUS host left in this compositor, before we mint our first.
        reclaim_leftovers_once();
        warn_topology_is_extend_only();

        let name = next_output_name();
        hyprctl_dispatch(&["output", "create", "headless", &name]).with_context(|| {
            format!("hyprctl output create headless {name} (is hyprctl reachable?)")
        })?;
        // Own the output from here on so any later error (or drop) removes it.
        let output = OutputGuard(name.clone());
        wait_monitor_ready(&name, Duration::from_secs(5))
            .with_context(|| format!("waiting for headless output {name} to appear"))?;

        // The client's exact mode (also the frame clock — a headless output is timer-paced from it).
        set_monitor_rule(&name, mode).with_context(|| format!("set monitor rule for {name}"))?;

        // Steer xdph's custom picker at our new output, then run the portal handshake on its own
        // thread (it parks to keep the cast alive, like the other backends). Serialized: the
        // selection is one per-user file, so a concurrent session's write between ours and xdph's
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
            "hyprland headless output ready"
        );
        Ok(VirtualOutput {
            node_id,
            remote_fd: Some(fd),
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(Keepalive {
                _stop: stop,
                _output: output,
            }),
            // Owned (the compositor output is ours to tear down), but not registry-poolable: the
            // portal fd can't be re-opened per attach, so the registry passes it through on
            // `remote_fd.is_some()` — same as wlroots.
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
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

/// Remove the `PF-<pid>-<n>` outputs left behind by host processes that are **gone**, once per
/// process before we create our first.
///
/// [`OutputGuard::drop`] is the only unplug path there is, so a host that was SIGKILLed, OOM-killed
/// or crashed leaves its headless outputs in the compositor for as long as the Hyprland session
/// lives — a dead `PF-…` head in the operator's layout, forever, with the next host start happily
/// adding more beside it. Reclaim is keyed on the OWNER pid in the name and only removes an output
/// whose creator no longer exists, so a second live host on the same session (or this very process)
/// can never have its output pulled out from under it. `Once` puts the sweep strictly before this
/// process owns anything, and blocks a concurrent first `create` until it is done.
fn reclaim_leftovers_once() {
    static RECLAIMED: Once = Once::new();
    RECLAIMED.call_once(|| {
        let Ok(names) = monitor_names() else { return };
        for name in names {
            let Some(pid) = output_owner_pid(&name) else {
                // Either not ours, or a legacy `PF-<n>` with no owner recorded — which we must not
                // remove on a guess, because a still-running older host may be streaming it.
                if is_managed_output(&name) {
                    tracing::debug!(output = %name, "a managed headless output with no owner pid in \
                         its name (an older host build) — left alone");
                }
                continue;
            };
            if pid == std::process::id() || std::path::Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            match hyprctl_dispatch(&["output", "remove", &name]) {
                Ok(()) => tracing::info!(output = %name, owner_pid = pid, "removed a headless \
                     output left behind by a host that is no longer running"),
                Err(e) => tracing::warn!(output = %name, owner_pid = pid, error = %format!("{e:#}"),
                    "could not remove a leftover headless output"),
            }
        }
    });
}

/// The configured [`crate::policy::Topology`] is not implemented on this backend — say so once per
/// create instead of leaving the management API's echo as the only signal that the pin was dropped
/// (sweep 13.18). The Hyprland headless output is always an EXTENSION: nothing here promotes it to
/// primary or disables the operator's heads.
fn warn_topology_is_extend_only() {
    let topology = crate::effective_topology();
    if !matches!(
        topology,
        crate::policy::Topology::Extend | crate::policy::Topology::Auto
    ) {
        tracing::warn!(
            ?topology,
            "hyprland: this backend implements EXTEND only — the headless output is added beside \
             the operator's heads and nothing is promoted or disabled. Configure `topology: extend` \
             to stop the console promising otherwise."
        );
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

/// Budget for one `hyprctl` call ([`crate::proc`]).
///
/// `hyprctl` is a client of the compositor it drives — it connects to the instance socket and waits
/// for a reply, so against a wedged Hyprland it never returns. These calls run on the session's
/// stream thread, whose only way to end a session is to return, so one hung query used to wedge the
/// session for good. Generous next to a healthy call (single-digit milliseconds), and every call
/// site already has a failed-query path.
const HYPRCTL_BUDGET: Duration = Duration::from_secs(5);

/// Budget for the one-shot xdph restart. `systemctl --user try-restart` waits for the user manager's
/// job to settle, so it is the slowest helper on this path — and its result is already ignored.
const PORTAL_RESTART_BUDGET: Duration = Duration::from_secs(10);

/// Run `hyprctl <args>`, returning stdout. `hyprctl` reads `HYPRLAND_INSTANCE_SIGNATURE` from the
/// env (exported by `apply_session_env`) to reach the right instance socket. It exits non-zero on a
/// hard failure, but for dispatch commands it can print an error with status 0 — see
/// [`hyprctl_dispatch`].
fn hyprctl(args: &[&str]) -> Result<String> {
    let out = crate::proc::output_within(Command::new("hyprctl").args(args), HYPRCTL_BUDGET)
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

/// Serializes **write-the-selection → complete-the-handshake**, process-wide — see the wlroots
/// backend's `SELECTION_LOCK`. The xdph selection is likewise one per-user file, so a concurrent
/// write between ours and xdph's read would silently steer capture at the other session's output.
static SELECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The per-session selection file, removed when the handshake it steers is over.
///
/// Its lifetime is the HANDSHAKE, not the session: the shim cats it once, inside
/// [`select_and_cast`]'s critical section, and everything after that is the cast's own business.
/// Left behind (as it was) the stale `[SELECTION]screen:PF-…` outlives the output `Drop` has since
/// removed, and it permanently shadows xdph's documented empty-read fallback — every later capture
/// that reaches the picker without a session of ours is steered at an output that is gone. Tying
/// removal to the CAST instead would be worse: the file is one per user, so a session ending hours
/// later would delete a *sibling's* selection out from under its picker.
struct SelectionFile(String);

impl Drop for SelectionFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.0, error = %e, "could not remove the xdph selection file");
            }
        }
    }
}

/// Point xdph's custom picker at `output` and run the ScreenCast handshake, returning the portal fd
/// + node id and the guard that stops the cast. The caller must hold [`SELECTION_LOCK`].
fn select_and_cast(output: &str, hw_cursor: bool) -> Result<(OwnedFd, u32, StopGuard)> {
    ensure_xdph_config()?;
    let sel = selection_file();
    std::fs::write(&sel, picker_selection_line(output)).with_context(|| format!("write {sel}"))?;
    // Owned from the write on: every arm below (and every `?`) leaves the handshake, which is the
    // only thing that reads it.
    let _sel_file = SelectionFile(sel);
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-hypr-cast".into())
        .spawn(move || portal_thread(setup_tx, stop_thread, hw_cursor))
        .context("spawn hyprland portal thread")?;
    // Built BEFORE the wait so EVERY error arm below sets the flag on its way out — as Mutter's
    // `create` does. Returning the bare `Arc` and letting the CALLER wrap it left the two failure
    // arms dropping an un-set flag: the thread's `send` can still LAND in the queue in the window
    // between `recv_timeout` giving up and `setup_rx` being dropped, so it reports success and then
    // parks forever on `while !stop`, holding a live ScreenCast session, its zbus connection, an
    // `OwnedFd` and a 2-worker tokio runtime — one more set per slow-portal connect, for the host's
    // lifetime, against an output that no longer exists.
    let guard = StopGuard(stop);
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id))) => Ok((fd, node_id, guard)),
        Ok(Err(e)) => bail!("ScreenCast portal on {output} failed: {e}"),
        Err(_) => bail!("timed out waiting for the ScreenCast portal on {output}"),
    }
}

/// Record an **existing** Hyprland monitor — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md` L3): the same custom-picker mechanism the virtual-output
/// path uses, pointed at a physical connector, so no GUI picker is involved.
///
/// The keepalive stops the cast only — the monitor is Hyprland's, not ours.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (fd, node_id, stop) = select_and_cast(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: Some(fd),
        keepalive: Box::new(stop),
    })
}

/// Every head Hyprland reports, for [`crate::monitors::list`].
///
/// `hyprctl -j monitors all` (rather than plain `monitors`) so **disabled** heads are listed too —
/// a picker that silently omits the monitor the operator is trying to name is worse than one that
/// shows it greyed out. Hyprland reports geometry post-transform in logical pixels, which is the
/// space `crate::monitors` documents.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let raw = hyprctl(&["-j", "monitors", "all"])?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).context("parse hyprctl -j monitors all")?;
    let mut out: Vec<_> = parsed
        .as_array()
        .context("hyprctl monitors: not an array")?
        .iter()
        .filter_map(|m| {
            let connector = m.get("name")?.as_str()?.to_string();
            let num = |k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            // Hyprland's `description` is already a "make model (connector)" string; treat it as
            // the make and let the shared helper drop it when it is empty/Unknown.
            let description = crate::monitors::describe(
                m.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "",
                &connector,
            );
            Some(crate::monitors::PhysicalMonitor {
                connector,
                description,
                width: num("width").max(0) as u32,
                height: num("height").max(0) as u32,
                // `refreshRate` is Hz as a float.
                refresh_mhz: (m.get("refreshRate").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0)
                    as u32,
                x: num("x") as i32,
                y: num("y") as i32,
                scale: m
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false),
                enabled: !m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
                // Our headless outputs are named `PF-<pid>-<n>` (see `next_output_name`); the shape
                // is checked, not just the prefix, so a user's own `PF-office` stays theirs.
                managed: m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_managed_output),
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
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
        // `hyprctl eval` on a hyprlang (non-Lua) config: "eval is only supported with the lua
        // config manager" — a rejection hyprctl reports with exit 0 and no other marker.
        || lc.contains("only supported")
        || lc.contains("not supported")
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

/// Every monitor name Hyprland reports, **disabled ones included** (`-j monitors all`) — a leftover
/// output from a dead host may well have ended up disabled, and [`reclaim_leftovers_once`] must see
/// it anyway.
fn monitor_names() -> Result<Vec<String>> {
    let out = hyprctl(&["-j", "monitors", "all"])?;
    let monitors: serde_json::Value =
        serde_json::from_str(&out).context("parse hyprctl -j monitors all")?;
    Ok(monitors
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
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
/// era split (D5). `hyprctl keyword monitor NAME,WxH@Hz,auto,1` is the hyprlang path — the default
/// config manager on **every** current release, including ≥0.55 (verified on 0.55.4: version does
/// NOT imply the Lua era — a stock 0.55.4 rejects `eval` with "only supported with the lua config
/// manager"). So we try `keyword` first and fall back to the Lua `hyprctl eval 'hl.monitor{…}'` only
/// for a user who opted into the Lua config manager (where `keyword` is gone). Either way we confirm
/// the output actually adopted the mode — some forms print `ok` for a command they ignored.
///
/// A headless output starts at 0×0 and only gets a framebuffer once a mode commits; if neither form
/// makes it adopt a usable (non-zero) size the compositor couldn't back the mode (a headless GBM /
/// dmabuf allocation failure — Sunshine#4197, seen on some NVIDIA setups), which we surface clearly
/// rather than streaming a 0×0 corpse.
fn set_monitor_rule(name: &str, mode: Mode) -> Result<()> {
    let hz = mode.refresh_hz.max(1);
    let spec = format!("{name},{}x{}@{hz},auto,1", mode.width, mode.height);
    let lua = format!(
        "hl.monitor{{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"auto\", scale = 1 }}",
        mode.width, mode.height
    );
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    // What each form actually said. hyprctl reports a rejection in its OUTPUT TEXT ("eval is only
    // supported with the lua config manager", "invalid monitor rule", a permission denial), and
    // dropping it on the floor with `.is_err()` is what left the failure below guessing at GBM when
    // the compositor had already named the real cause.
    let mut attempts: Vec<String> = Vec::new();
    for a in [&keyword, &eval] {
        // A wrong-era command errors (`keyword` gone under Lua, or `eval` under hyprlang) — skip to
        // the other form. A command that's accepted then has up to the timeout to take effect.
        if let Err(e) = hyprctl_dispatch(a) {
            let said = format!("{e:#}");
            tracing::debug!(output = %name, cmd = ?a, error = %said, "hyprctl rejected this monitor-rule form — trying the other config era");
            attempts.push(said);
            continue;
        }
        if wait_exact_mode(name, mode, Duration::from_millis(1500)) {
            tracing::debug!(output = %name, cmd = ?a, w = mode.width, h = mode.height, "monitor adopted the requested mode");
            return Ok(());
        }
        attempts.push(format!(
            "hyprctl {a:?} was accepted but the mode never took effect"
        ));
    }
    let said = if attempts.is_empty() {
        "nothing (no form was attempted)".to_string()
    } else {
        attempts.join("; ")
    };
    // Neither form produced the exact mode. Distinguish "usable but different size" (proceed with a
    // warning — a working stream beats none) from "0×0 / gone" (the output has no framebuffer at all).
    match monitor_size(name)? {
        Some((w, h)) if w > 0 && h > 0 => {
            tracing::warn!(
                output = %name,
                requested = %format!("{}x{}", mode.width, mode.height),
                got = %format!("{w}x{h}"),
                hyprctl = %said,
                "Hyprland did not adopt the exact requested mode — streaming at the output's current size"
            );
            Ok(())
        }
        // The output has no framebuffer at all. Lead with what hyprctl SAID: if every form was
        // rejected the cause is named right there (wrong config era, a permission denial, a bad
        // rule) and no allocation was ever attempted; only a form that was accepted and still left
        // the output at 0×0 points at the compositor failing to back the mode.
        _ => bail!(
            "headless output {name} never got a framebuffer (stayed 0x0) after the monitor rule for \
             {}x{}@{hz}. hyprctl said: {said}. If a form was accepted, the compositor could not back \
             the mode — likely a headless GBM/dmabuf allocation failure (GPU driver; cf. \
             Sunshine#4197). Check the Hyprland log.",
            mode.width,
            mode.height
        ),
    }
}

/// Poll until monitor `name` reports exactly `mode`'s width×height (the rule applies asynchronously),
/// up to `timeout`. Returns `false` on timeout.
fn wait_exact_mode(name: &str, mode: Mode, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(monitor_size(name), Ok(Some((w, h))) if w == mode.width as u64 && h == mode.height as u64)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Monitor `name`'s current `(width, height)` from `hyprctl -j monitors all` (includes a disabled
/// output), or `None` if it isn't present. A freshly-created headless output reports `0×0` until a
/// mode commits.
fn monitor_size(name: &str) -> Result<Option<(u64, u64)>> {
    let out = hyprctl(&["-j", "monitors", "all"])?;
    let monitors: serde_json::Value =
        serde_json::from_str(&out).context("parse hyprctl -j monitors")?;
    let Some(arr) = monitors.as_array() else {
        return Ok(None);
    };
    for m in arr {
        if m.get("name").and_then(|n| n.as_str()) == Some(name) {
            let w = m.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
            let h = m.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
            return Ok(Some((w, h)));
        }
    }
    Ok(None)
}

/// The running Hyprland `(major, minor, patch)` from `hyprctl -j version` (`tag` like `v0.55.4`),
/// for a diagnostic log — the mode-rule path is version-independent (see [`set_monitor_rule`]).
fn hyprland_version() -> Option<(u16, u16, u16)> {
    let out = hyprctl(&["-j", "version"]).ok()?;
    let json: serde_json::Value = serde_json::from_str(&out).ok()?;
    parse_version_tag(json.get("tag").and_then(|t| t.as_str())?)
}

/// Parse a Hyprland `tag` (`v0.55.4`, or a dev `v0.41.2-13-gabcdef`) to `(major, minor, patch)`.
fn parse_version_tag(tag: &str) -> Option<(u16, u16, u16)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split(['.', '-', '_', '+']);
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
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
        // Mode set AT CREATION, not chmod-ed after: xdph EXECUTES this file, and a
        // write-then-chmod leaves it briefly at the umask default. (It also lives in a 0700
        // runtime dir now — see `session::runtime_dir` — so this is defence in depth.)
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(&shim)
            .with_context(|| format!("write {shim}"))?;
        f.write_all(shim_body.as_bytes())
            .with_context(|| format!("write {shim}"))?;
    }

    // 2. Write the managed xdph config and restart xdph on change.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
    let path = base.join("hypr").join("xdph.conf");
    // ONE key, in place. This used to `fs::write` a complete file over whatever the user had,
    // destroying every other xdph setting they owned on first connect.
    let changed = crate::portal_config::ensure_key(
        &path,
        crate::portal_config::Block::Hyprlang("screencopy"),
        "custom_picker_binary",
        &shim,
    )?;
    if !changed {
        return Ok(());
    }
    tracing::info!(path = %path.display(), "pointed xdg-desktop-portal-hyprland at the managed picker shim");
    // Bounded: `systemctl --user` blocks on the user manager's job queue, and this runs on the
    // session's stream thread. Its result was already ignored — a timeout just means xdph picks the
    // new config up whenever it next starts.
    let _ = crate::proc::status_within(
        Command::new("systemctl").args([
            "--user",
            "try-restart",
            "xdg-desktop-portal-hyprland.service",
        ]),
        PORTAL_RESTART_BUDGET,
    );
    Ok(())
}

/// The ScreenCast portal handshake — the xdg ScreenCast portal is backend-neutral (served here by
/// xdph), so this mirrors the wlroots portal thread: it reports the fd + node id and parks until
/// stopped (the zbus connection is the cast's lifetime). xdph answers source selection via our
/// custom picker, no dialog. (Kept separate from wlroots' copy so each wlr-family backend stays
/// self-owned per D1; unify if they ever diverge no further.)
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
                        .set_cursor_mode(cursor_mode)
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

    /// The name carries the creating host's pid, which is what makes a leftover attributable — a
    /// reclaim that could not tell whose output it was would have to remove a LIVE sibling's or
    /// nothing at all.
    #[test]
    fn a_name_carries_its_owner_pid_and_only_ours_does() {
        let mine = next_output_name();
        assert_eq!(output_owner_pid(&mine), Some(std::process::id()));
        assert!(is_managed_output(&mine));

        // A legacy `PF-<n>` from an older host: recognisably managed, but with no owner recorded —
        // so it may be reported, never reclaimed on a guess.
        assert!(is_managed_output("PF-1"));
        assert_eq!(output_owner_pid("PF-1"), None);

        // Not ours: a user's own monitor name that happens to start with the prefix, and the
        // connectors every wlr-family compositor mints.
        for theirs in ["PF-office", "PF-", "PF-12-abc", "HEADLESS-1", "DP-1", ""] {
            assert!(!is_managed_output(theirs), "{theirs:?} is not ours");
            assert_eq!(output_owner_pid(theirs), None, "{theirs:?} has no owner");
        }
    }

    #[test]
    fn picker_line_carries_the_selection_marker() {
        // xdph requires the `[SELECTION]` prefix; a bare `screen:NAME` is rejected as strange output.
        assert_eq!(picker_selection_line("PF-1"), "[SELECTION]screen:PF-1\n");
    }
}
