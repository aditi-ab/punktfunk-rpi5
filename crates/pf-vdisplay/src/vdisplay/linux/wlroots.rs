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
//! 4. Teardown is RAII **and ordered**: drop closes the ScreenCast session and WAITS for the portal
//!    to confirm it, and only then runs `swaymsg output <NAME> unplug` (headless outputs support
//!    unplug since sway 1.8). See [`StopGuard`] — and the long root-cause note on `hyprland.rs`'s
//!    copy, which is where this was measured.
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
/// plain portal capture (`--source portal`) working when no session of ours is mid-handshake — it
/// is a GUESS at sway's own first headless output, right on a box whose sway loads the headless
/// backend with one output of its own and wrong (a cast of nothing) otherwise. It is reachable
/// again: the per-session file is removed with the handshake it steers ([`ChooserFile`]), so it no
/// longer sits there naming an output we have since unplugged.
fn chooser_cmd() -> String {
    format!(
        "cat {} 2>/dev/null || echo 'Monitor: HEADLESS-1'",
        chooser_file()
    )
}

/// The wlroots/Sway virtual-display driver. Stateless — each [`create`](VirtualDisplay::create)
/// adds one headless output and spins up a portal thread owning the cast on it.
pub struct WlrootsDisplay {
    /// Out-of-band cursor request (`set_hw_cursor`, the negotiated cursor channel): PREFER portal
    /// `CursorMode::Metadata` — shapes/positions ride `SPA_META_Cursor` for the channel + the
    /// composite blend. Off (every non-channel session): prefer `Embedded` — the compositor paints
    /// the pointer into frames, zero host-side cursor work (the pre-channel default this backend
    /// always had).
    ///
    /// Both are only a PREFERENCE: [`crate::portal_cursor`] settles it against what xdpw actually
    /// advertises, because requesting an unadvertised mode closes the session outright. xdpw
    /// refuses metadata by construction (see the portal thread), so on this backend the channel can
    /// never be served out-of-band: it now degrades to `Embedded` and streams, where it used to
    /// cancel the cast and hand the client a black screen.
    hw_cursor: bool,
}

impl WlrootsDisplay {
    pub fn new() -> Result<Self> {
        Ok(WlrootsDisplay { hw_cursor: false })
    }
}

/// wlroots/Sway is usable when the host runs inside a Sway session — signalled by `SWAYSOCK`
/// (the IPC socket `swaymsg create_output` needs). Cheap env check for the enumeration path.
///
/// Under [`crate::with_env_lock`]: this runs on a management worker (`/host/compositors` →
/// [`crate::available`]) concurrently with another connect's `apply_session_env`, which `set_var`s
/// — and, when no sway session is live, `remove_var`s — this very key. A glibc `getenv` racing a
/// `setenv` is the `environ` realloc data race ENV_LOCK exists for, and it is UB whichever key each
/// side names. No caller holds the lock (the mutex is not reentrant).
pub fn is_available() -> bool {
    crate::with_env_lock(|| std::env::var_os("SWAYSOCK")).is_some()
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
        warn_topology_is_extend_only();
        // Snapshot → create → identify, all under CREATE_LOCK. sway names the headless output
        // itself (`HEADLESS-N`), so the only way to know which one is ours is "the name that was not
        // there before" — and two concurrent creates each picking the other's output is a silent
        // mis-capture, not a failure (mutter's TOPOLOGY_LOCK exists for exactly this class). The
        // lock also gives the failure path somewhere safe to unplug from: the output already exists
        // by the time `wait_new_output` can fail, and nothing else may have created one meanwhile.
        let output = {
            let _create = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let before = output_names().context(
                "swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)",
            )?;
            swaymsg(&["create_output"])
                .context("swaymsg create_output (sway needs the headless backend loaded)")?;
            // The output appears synchronously in practice; poll briefly to be safe, and own it
            // from here on so error unwinding unplugs it.
            match wait_new_output(&before, Duration::from_secs(5)) {
                Ok(name) => OutputGuard(name),
                Err(e) => {
                    // `create_output` reported success, so an output very probably exists — it just
                    // never showed up in time (or showed up a moment after we gave up). Unowned, it
                    // would sit in the operator's sway layout forever.
                    unplug_strays(&before);
                    return Err(e);
                }
            }
        };
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
                _stop: stop,
                _output: output,
            }),
            // Owned (the compositor output is ours to tear down), but not registry-poolable: the
            // portal fd can't be re-opened per attach, so the registry passes it through on
            // `remote_fd.is_some()` (keep-alive stays off for wlroots until fresh-portal re-attach).
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            // Same EXTEND problem as Hyprland: on a sway session with real heads this `HEADLESS-N`
            // sits beside them, and absolute input must be aimed at it by name. `swaymsg`'s output
            // name is the head's `wl_output.name`, which is what the injector matches.
            output_name: Some(name),
        })
    }
}

/// Drop order matters, and it is the whole fix: [`StopGuard`] **blocks until the ScreenCast session
/// is actually closed**, and only then does [`OutputGuard`] unplug the output (fields drop in
/// declaration order). This used to unplug first — see [`StopGuard`].
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// How long teardown waits for the portal to confirm the ScreenCast session is closed before giving
/// up and unplugging the output anyway. See `hyprland.rs`'s twin.
const CAST_CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// Ceiling on the whole ScreenCast handshake, under the caller's 20 s wait — see the note at the
/// handshake, and the longer one on `hyprland.rs`'s copy.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(15);

/// Ends the cast: signals the portal thread, then **waits for it to have closed the ScreenCast
/// session**, so the caller may safely unplug the output afterwards.
///
/// 🛑 THE WAIT IS THE POINT. Root-caused on the Hyprland leg (see the long note on `hyprland.rs`'s
/// `StopGuard`, which carries the measurements); the defect is the same here, and this is NOT an
/// assumption of symmetry — xdpw was read to confirm it, against `emersion/xdg-desktop-portal-wlr`:
///
/// * **Only `Close` tears a session down.** `src/core/session.c` gives the session object exactly
///   one method — `SD_BUS_METHOD("Close", …, method_close, …)` — and nothing else calls
///   `xdpw_session_destroy` for a live cast. Like xdph, xdpw has no peer-vanished watcher of its own
///   and depends entirely on xdg-desktop-portal's `peer_died_cb` calling `Close` for us, which
///   happens only after our bus name goes away, asynchronously, and therefore after the old
///   `StopGuard` had already let `OutputGuard` unplug the output.
/// * **The same unbounded busy-wait is waiting for it.** `src/screencast/screencast.c:599-605`:
///   `while (cast->node_id == SPA_ID_INVALID) { pw_loop_iterate(state->pw_loop, 0); }` — timeout 0,
///   i.e. non-blocking, i.e. a hot spin on the portal's only loop with no escape if the stream never
///   gets a node id. xdph's copy (`Screencopy.cpp:307-313`) is this code; that is the one measured
///   pinning a core solid until it was restarted.
///
/// So sway's `output unplug` yanks a captured output out from under a live session exactly the way
/// Hyprland's `output remove` did. Whether xdpw wedges *identically* has not been observed on glass
/// — no sway box was available — but the two preconditions are present in its source, and closing
/// the session before unplugging is the correct order regardless of what the backend does with it.
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Signalled by the portal thread once it has closed the ScreenCast session. `None` when no cast
    /// was ever established — nothing to close, and nothing worth spending the budget on.
    closed: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(closed) = self.closed.take() else {
            return;
        };
        match closed.recv_timeout(CAST_CLOSE_BUDGET) {
            // Closed, or the thread is gone without confirming — either way nothing holds the cast.
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                budget_s = CAST_CLOSE_BUDGET.as_secs(),
                "the ScreenCast session did not close in time — unplugging the output underneath \
                 it; the next cast may find the portal busy"
            ),
        }
    }
}

/// Serializes **snapshot → `create_output` → identify-the-new-name**, process-wide. sway names its
/// headless outputs itself, so ownership is established by a before/after diff and two concurrent
/// creates would each adopt the other's output — which does not fail, it silently streams the wrong
/// one. Mutter's `TOPOLOGY_LOCK` is the same guard for the same reason; Hyprland needs none because
/// it lets us NAME the output (D6).
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Unplug any headless output that appeared since `before` and that nothing owns — the cleanup for a
/// `create_output` whose output we could not identify in time. Only `HEADLESS-*` is touched: a
/// physical hotplug in the same window is the operator's, not ours, and `unplug` on a real connector
/// would take their screen away. Best-effort by construction, and it runs with [`CREATE_LOCK`] held
/// so nothing else in this process can have created the strays it sees.
fn unplug_strays(before: &[String]) {
    let Ok(now) = output_names() else { return };
    for name in now
        .into_iter()
        .filter(|n| n.starts_with("HEADLESS-") && !before.iter().any(|b| b == n))
    {
        match swaymsg(&["output", &name, "unplug"]) {
            Ok(_) => tracing::warn!(output = %name, "unplugged a headless output we created but \
                 could not identify in time"),
            Err(e) => tracing::warn!(output = %name, error = %format!("{e:#}"), "could not unplug \
                 the headless output left behind by a failed create"),
        }
    }
}

/// The configured [`crate::policy::Topology`] is not implemented on this backend — say so once per
/// create instead of leaving the management API's echo as the only signal that the pin was dropped
/// (sweep 13.18). sway's virtual output is always an EXTENSION: nothing here promotes it to primary
/// or disables the operator's heads.
fn warn_topology_is_extend_only() {
    let topology = crate::effective_topology();
    if !matches!(
        topology,
        crate::policy::Topology::Extend | crate::policy::Topology::Auto
    ) {
        tracing::warn!(
            ?topology,
            "wlroots: this backend implements EXTEND only — the headless output is added beside the \
             operator's heads and nothing is promoted or disabled. Configure `topology: extend` to \
             stop the console promising otherwise."
        );
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

/// Budget for one `swaymsg` call ([`crate::proc`]).
///
/// swaymsg is a CLIENT of the compositor it drives: against a wedged sway it blocks in its own
/// connect to the IPC socket and never returns — and these calls run on the session's stream thread,
/// whose only way to end a session is to return, so one hung query used to wedge the session
/// permanently. Generous next to a healthy call (single-digit milliseconds), and every call site
/// here already has a failed-query path, so a timeout lands on behaviour that already exists.
const SWAYMSG_BUDGET: Duration = Duration::from_secs(5);

/// Budget for the one-shot xdpw restart. `systemctl --user try-restart` waits for the unit's job to
/// settle, so it is the slowest helper on this path — and its result is already ignored.
const PORTAL_RESTART_BUDGET: Duration = Duration::from_secs(10);

/// Run `swaymsg -- <args>`, returning stdout (`--` so command tokens like `--custom` reach
/// sway instead of swaymsg's own getopt). swaymsg exits non-zero (with the error on stderr/
/// stdout) when the command fails, so checking the status covers `{"success": false}` too.
fn swaymsg(args: &[&str]) -> Result<String> {
    let out =
        crate::proc::output_within(Command::new("swaymsg").arg("--").args(args), SWAYMSG_BUDGET)
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
    let out = crate::proc::output_within(
        Command::new("swaymsg").args(["-t", kind, "--raw"]),
        SWAYMSG_BUDGET,
    )
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

/// The per-session chooser file, removed when the handshake it steers is over.
///
/// Its lifetime is the HANDSHAKE, not the session: xdpw reads it once, inside
/// [`select_and_cast`]'s critical section, and everything after that is the cast's own business.
/// Left behind (as it was) the stale `Monitor: HEADLESS-3` outlives the output `Drop` has since
/// unplugged, and it permanently shadows [`chooser_cmd`]'s `|| echo` fallback — so a later
/// `--source portal` capture with no session of ours running steers at a connector that is gone.
/// Tying removal to the CAST instead would be worse still: the file is one per user, so a session
/// ending hours later would delete a *sibling's* selection out from under its picker.
struct ChooserFile(String);

impl Drop for ChooserFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.0, error = %e, "could not remove the xdpw chooser file");
            }
        }
    }
}

/// Point xdpw's chooser at `output` and run the ScreenCast handshake, returning the portal fd +
/// node id and the guard that stops the cast. The caller must hold [`SELECTION_LOCK`].
fn select_and_cast(output: &str, hw_cursor: bool) -> Result<(OwnedFd, u32, StopGuard)> {
    ensure_xdpw_config()?;
    let chooser = chooser_file();
    std::fs::write(&chooser, format!("Monitor: {output}\n"))
        .with_context(|| format!("write {chooser}"))?;
    // Owned from the write on: every arm below (and every `?`) leaves the handshake, which is the
    // only thing that reads it.
    let _chooser = ChooserFile(chooser);
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32), String>>();
    // The teardown handshake: the thread signals this once it has closed the ScreenCast session, and
    // `StopGuard::drop` waits on it before the output is unplugged (see `StopGuard`).
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-wlr-cast".into())
        .spawn(move || portal_thread(setup_tx, closed_tx, stop_thread, hw_cursor))
        .context("spawn wlroots portal thread")?;
    // Built BEFORE the wait so EVERY error arm below sets the flag on its way out — as Mutter's
    // `create` does. Returning the bare `Arc` and letting the CALLER wrap it left the two failure
    // arms dropping an un-set flag: the thread's `send` can still LAND in the queue in the window
    // between `recv_timeout` giving up and `setup_rx` being dropped, so it reports success and then
    // parks forever on `while !stop`, holding a live ScreenCast session, its zbus connection, an
    // `OwnedFd` and a 2-worker tokio runtime — one more set per slow-portal connect, for the host's
    // lifetime, against an output that no longer exists.
    let mut guard = StopGuard { stop, closed: None };
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id))) => {
            // A cast exists now, so teardown has something to close and must wait for it.
            guard.closed = Some(closed_rx);
            Ok((fd, node_id, guard))
        }
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
        keepalive: Box::new(stop),
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
    // Bounded: `systemctl --user` blocks on the user manager's job queue, and this runs on the
    // session's stream thread. Its result was already ignored — a timeout just means the portal
    // picks the new config up whenever it next starts.
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "try-restart", "xdg-desktop-portal-wlr.service"]),
        PORTAL_RESTART_BUDGET,
    );
    Ok(())
}

/// The ScreenCast portal handshake (same shape as the capture module's portal thread, but it
/// reports the fd + node id and parks until stopped — the zbus connection is the cast's
/// lifetime). xdpw answers the source selection via the chooser, no dialog.
fn portal_thread(
    setup_tx: Sender<Result<(OwnedFd, u32), String>>,
    closed_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Multi-thread runtime: the zbus background reader must be pumped across the
    // create_session → select_sources → start handshake (see capture/linux.rs).
    // The SHARED, never-dropped runtime — see [`crate::portal_rt`] and the long note on
    // `hyprland.rs`'s copy: a per-cast runtime kills ashpd's process-global cached connection when
    // the cast ends, and every later handshake in the process then hangs.
    let rt = match crate::portal_rt::portal_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(e));
            return;
        }
    };
    let err_tx = setup_tx.clone();

    rt.block_on(async move {
        let result: Result<()> = async {
            // Bounded, like `hyprland.rs`'s copy: an orphaned cached connection hangs HERE, before
            // any handshake call, so a bound that starts later never fires.
            let connect = async {
                Screencast::new().await.context(
                    "connect ScreenCast portal (is xdg-desktop-portal running with the wlr backend?)",
                )
            };
            let proxy = match tokio::time::timeout(HANDSHAKE_BUDGET, connect).await {
                Ok(v) => v?,
                Err(_) => bail!(
                    "connecting to the ScreenCast portal did not return within {}s",
                    HANDSHAKE_BUDGET.as_secs()
                ),
            };
            // NEGOTIATED against what xdpw advertises, never asserted from `hw_cursor` alone — see
            // the xdph copy in `hyprland.rs` for the incident. xdpw is the sharper case: its
            // screencast.c refuses the mode outright —
            //     if (sess->screencast_data.cursor_mode & METADATA) {
            //         logprint(ERROR, "dbus: unsupported cursor mode requested, cancelling");
            // — so EVERY cursor-forward session on this backend asked for a mode that cancelled the
            // cast. Different wording from xdph's "unavailable cursor mode 4", same dead session.
            let cursor_mode = crate::portal_cursor::negotiate(&proxy, hw_cursor, "xdpw").await;
            // Bounded for the same reason as `hyprland.rs`'s copy (the long note lives there): an
            // await on a wedged portal never returns, the `stop` flag is only read by the park loop
            // further down, so the thread leaks — and a leaked half-handshake poisons every later
            // portal request from this process. xdpw has the identical unbounded node-id spin as
            // xdph (`screencast.c`), so it can wedge the same way.
            let handshake = async {
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
                    .context(
                        "start response (chooser declined? check the xdpw config/chooser file)",
                    )?;
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
                Ok::<_, anyhow::Error>((session, fd, node_id))
            };
            let (session, fd, node_id) =
                match tokio::time::timeout(HANDSHAKE_BUDGET, handshake).await {
                    Ok(v) => v?,
                    Err(_) => bail!(
                        "the ScreenCast portal did not complete the handshake within {}s — \
                         abandoning it instead of parking this thread on it forever (a hung \
                         request poisons every later one from this process)",
                        HANDSHAKE_BUDGET.as_secs()
                    ),
                };

            setup_tx
                .send(Ok((fd, node_id)))
                .map_err(|_| anyhow!("virtual-output opener went away"))?;

            // Park, keeping `proxy` + `session` alive until stopped. Polled at 20 ms rather than the
            // 200 ms this used to use, because teardown now WAITS on what follows.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // 🛑 CLOSE THE SESSION, AND CLOSE IT *BEFORE* THE OUTPUT IS UNPLUGGED. `Session.Close` is
            // the only thing that ends an xdpw session (`src/core/session.c`); dropping the
            // connection and trusting the peer to notice is not the contract. The caller is blocked
            // in `StopGuard::drop` on the signal below — see `StopGuard`. Bounded, so an
            // already-wedged portal cannot hang teardown with it.
            match tokio::time::timeout(CAST_CLOSE_BUDGET, session.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    error = %e,
                    "closing the ScreenCast session failed — the next cast may find the portal busy"
                ),
                Err(_) => tracing::warn!(
                    budget_s = CAST_CLOSE_BUDGET.as_secs(),
                    "the ScreenCast portal did not answer Session.Close in time — it is probably \
                     already wedged"
                ),
            }
            // Release the teardown. Best-effort: the receiver is gone if the caller already gave up.
            let _ = closed_tx.send(());
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
}
