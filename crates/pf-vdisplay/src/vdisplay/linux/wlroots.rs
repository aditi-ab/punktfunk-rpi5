//! wlroots/Sway virtual-output backend via sway IPC + xdg-desktop-portal-wlr.
//!
//! 1. `swaymsg create_output` adds a headless output (`HEADLESS-N`). Sway must run the
//!    headless backend (or co-load it). The name is the before/after diff of
//!    `swaymsg -t get_outputs`.
//! 2. `swaymsg output <NAME> mode --custom WxH@HzHz` sets the client's mode. A fresh
//!    headless output needs a real mode for a refresh clock or it produces no frames.
//! 3. The ScreenCast portal yields the PipeWire node. There is no GUI picker, so a
//!    managed `~/.config/xdg-desktop-portal-wlr/config` sets `chooser_type=simple` and a
//!    `chooser_cmd` that cats a per-session file (`Monitor: <NAME>` — xdpw 0.8 parses
//!    that prefix strictly). Written once; the portal restarts on change.
//! 4. Teardown is ordered: drop closes the ScreenCast session and waits for the portal
//!    to confirm, then `swaymsg output <NAME> unplug` (sway ≥1.8). See [`StopGuard`].
//!
//! Requirements: `SWAYSOCK` inherited or discovered per child ([`swaymsg_command`]),
//! portal env via `scripts/headless/prepare-session.sh`, ScreenCast routed to xdpw
//! (`scripts/headless/portals.conf`).

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Per-session file xdpw's chooser cats (`Monitor: <NAME>\n`). Under `$XDG_RUNTIME_DIR`
/// (0700), not a world-writable /tmp another user could pre-create or rewrite between
/// our write and xdpw's read.
fn chooser_file() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdpw-output")
}

/// xdpw runs this via `/bin/sh -c` and reads stdout. The `|| echo` fallback is a guess
/// at sway's own first headless output — right only when that backend has one. [`ChooserFile`]
/// removes the per-session file with the handshake so it cannot name an already-unplugged
/// output.
fn chooser_cmd() -> String {
    format!(
        "cat {} 2>/dev/null || echo 'Monitor: HEADLESS-1'",
        chooser_file()
    )
}

/// wlroots/Sway virtual-display driver. Each [`create`](VirtualDisplay::create) adds one
/// headless output; a portal thread owns the cast.
pub struct WlrootsDisplay {
    /// Out-of-band cursor request: prefer portal `CursorMode::Metadata` (`SPA_META_Cursor`
    /// for the channel + blend). Off: prefer `Embedded` (compositor paints the pointer).
    ///
    /// Preference only: [`crate::portal_cursor`] matches what xdpw advertises — an
    /// unadvertised mode closes the session. xdpw refuses metadata by construction, so
    /// this backend always degrades to `Embedded`.
    hw_cursor: bool,
    /// Last portal-negotiated cursor mode. The host must read this rather than infer
    /// overlay absence from `hw_cursor`.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Restore for heads the last `create` disabled (`exclusive`). Written only through
    /// [`stash_topology_restore`](crate::backend::stash_topology_restore) — first-wins:
    /// one instance serves the host retry loop and only attempt 1 finds heads to disable.
    ///
    /// The registry never takes it: a sway display carries a portal fd, so
    /// `registry::acquire` returns pass-through before `take_topology_restore()`.
    /// [`Drop`] is the only runner.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for WlrootsDisplay {
    fn drop(&mut self) {
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl WlrootsDisplay {
    pub fn new() -> Result<Self> {
        Ok(WlrootsDisplay {
            hw_cursor: false,
            last_cursor_mode: None,
            pending_restore: None,
        })
    }

    /// Apply [`crate::policy::Topology`] for `ours` and stash the restore this instance
    /// runs on drop (the registry does not take a pass-through display's restore).
    ///
    /// Last step of [`create`](VirtualDisplay::create): nothing fails after it, so no
    /// path disables heads and then unwinds past the restore hand-off. Physical heads
    /// stay lit through the portal handshake (same as `extend`).
    fn apply_topology(&mut self, ours: &str) {
        use crate::policy::Topology;
        match crate::effective_topology() {
            Topology::Extend | Topology::Auto => {}
            Topology::Primary => warn_primary_is_not_expressible(),
            Topology::Exclusive => {
                let disabled = disable_other_heads(ours);
                let prepared = (!disabled.is_empty()).then(|| {
                    Box::new(move || restore_heads(&disabled)) as Box<dyn FnOnce() + Send>
                });
                // First restore wins: retry loops must not replace attempt 1's list, and
                // the registry never drains this slot (portal fd → pass-through).
                crate::backend::stash_topology_restore(&mut self.pending_restore, prepared);
            }
        }
    }
}

/// True when the host inherited `SWAYSOCK` (the IPC socket `swaymsg` needs).
/// Children get the socket via [`swaymsg_command`], so a `systemd --user` host
/// never sees it here. [`crate::available`] asks the `/proc` scan first.
///
/// Under [`crate::with_env_lock`] against this crate's remaining env writers. The mutex
/// is not reentrant; no caller holds it.
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

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Snapshot → create → identify under CREATE_LOCK. Sway names the output
        // (`HEADLESS-N`); two concurrent creates would each adopt the other's head
        // (silent mis-capture). The lock also serializes unplug on the failure path:
        // the output exists before `wait_new_output` can fail.
        let output = {
            let _create = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let before = output_names().context(
                "swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)",
            )?;
            swaymsg(&["create_output"])
                .context("swaymsg create_output (sway needs the headless backend loaded)")?;
            // Own it from here so error unwind unplugs it; the output is usually already listed.
            match wait_new_output(&before, Duration::from_secs(5)) {
                Ok(name) => OutputGuard(name),
                Err(e) => {
                    // create_output succeeded; an unidentified HEADLESS-* would stay in the layout.
                    unplug_strays(&before);
                    return Err(e);
                }
            }
        };
        let name = output.0.clone();

        // Client mode is also the refresh clock; without it the output produces no frames.
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

        focus_output(&name);

        // One chooser file per user: a concurrent write between ours and xdpw's read
        // captures the wrong output. Handshake holds SELECTION_LOCK, not just the write.
        let (fd, node_id, cursor_mode, stop) = {
            let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            select_and_cast(&name, self.hw_cursor)?
        };
        // xdpw refuses metadata, so this is `embedded` regardless of `hw_cursor`.
        self.last_cursor_mode = Some(cursor_mode);
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            cursor = cursor_mode.name(),
            "sway headless output ready"
        );
        // Last: no failure path unwinds past the restore hand-off.
        self.apply_topology(&name);
        Ok(VirtualOutput {
            node_id,
            remote_fd: Some(fd),
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(Keepalive {
                _stop: stop,
                _output: output,
            }),
            // Owned, not poolable: the portal fd cannot reopen per attach, so the
            // registry pass-throughs on `remote_fd.is_some()`.
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            // Absolute input aims at this `wl_output.name`; with real heads the
            // HEADLESS-* sits beside them.
            output_name: Some(name),
        })
    }
}

/// [`StopGuard`] blocks until the ScreenCast session is closed; [`OutputGuard`] then
/// unplugs. Fields drop in declaration order.
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// 3 s to wait for portal Close before unplugging under a live session.
const CAST_CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// Whole ScreenCast handshake; sits under the caller's 20 s wait.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(15);

/// Signals the portal thread, then waits until it has closed the ScreenCast session
/// so the caller may unplug the output.
///
/// Only `Session.Close` tears an xdpw session down (`src/core/session.c`); xdpw has
/// no peer-vanished watcher and relies on xdg-desktop-portal's `peer_died_cb`, which
/// runs after our bus name is gone — after an unplug-first Drop would already have
/// yanked the captured output. `screencast.c` then busy-waits
/// `while (cast->node_id == SPA_ID_INVALID) pw_loop_iterate(..., 0)` with no timeout.
/// Close before unplug. Proof also in `hyprland.rs`'s [`StopGuard`].
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Signalled once the portal thread has closed the session. `None` if no cast ran.
    closed: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(closed) = self.closed.take() else {
            return;
        };
        match closed.recv_timeout(CAST_CLOSE_BUDGET) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                budget_s = CAST_CLOSE_BUDGET.as_secs(),
                "the ScreenCast session did not close in time — unplugging the output underneath \
                 it; the next cast may find the portal busy"
            ),
        }
    }
}

/// Serializes snapshot → `create_output` → identify, process-wide. Sway names the
/// output; a concurrent pair would each stream the other's head (no error). Mutter's
/// `TOPOLOGY_LOCK` is the same class; Hyprland names the output itself.
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `HEADLESS-` prefix only: sway names these itself, so an operator-made headless
/// output is indistinguishable. Callers stay narrow: [`unplug_strays`] also requires
/// it appeared during our `create_output`; [`super::super::focus_streamed_output`]
/// only passes the head this session is streaming.
pub(crate) fn is_managed_output(name: &str) -> bool {
    name.starts_with("HEADLESS-")
}

/// Unplug `HEADLESS-*` that appeared since `before` and nothing owns. A physical
/// hotplug in the same window is the operator's; `unplug` on a real connector takes
/// their screen. Runs with [`CREATE_LOCK`] held.
fn unplug_strays(before: &[String]) {
    let Ok(now) = output_names() else { return };
    for name in now
        .into_iter()
        .filter(|n| is_managed_output(n) && !before.iter().any(|b| b == n))
    {
        match swaymsg(&["output", &name, "unplug"]) {
            Ok(_) => tracing::warn!(output = %name, "unplugged a headless output we created but \
                 could not identify in time"),
            Err(e) => tracing::warn!(output = %name, error = %format!("{e:#}"), "could not unplug \
                 the headless output left behind by a failed create"),
        }
    }
}

/// Focus the head we are about to stream so session windows land where the client
/// can see them.
///
/// sway opens a new window on the focused workspace; `create_output` does not move
/// focus. The client's pointer is confined to the streamed output, so without this
/// every launch opens on the physical monitor. Best-effort: failure costs placement,
/// not the session.
pub(crate) fn focus_output(name: &str) {
    match swaymsg(&focus_argv(name)) {
        Ok(_) => tracing::info!(output = %name, "focused the streamed headless output"),
        Err(e) => tracing::warn!(
            output = %name, error = %format!("{e:#}"),
            "could not focus the streamed headless output — apps this session launches may open on \
             a physical monitor instead of on the stream"
        ),
    }
}

/// `focus output <name>` — noun second, unlike every other call in this file
/// (`output <name> mode|enable|unplug`). `output focus <name>` is rejected.
/// [`swaymsg`] passes through `--` and treats a non-zero exit as failure, so a bad
/// shape logs rather than succeeding silently (`hyprctl` would exit 0).
fn focus_argv(name: &str) -> [&str; 3] {
    ["focus", "output", name]
}

/// `topology: primary` has no expression here: Wayland has no primary output, and
/// sway's nearest equivalent is the focused output, which [`focus_output`] already
/// points at the streamed head. Log and treat as extend. `exclusive` actually
/// changes the desk.
fn warn_primary_is_not_expressible() {
    tracing::info!(
        "wlroots: `topology: primary` has no equivalent here — Wayland has no primary output and \
         sway has only a FOCUSED output, which the streamed head already holds. Treating it as \
         `extend`; use `exclusive` to actually disable the operator's heads."
    );
}

/// Enabled, not ours, not managed. Pure so the sibling-spare rule is testable
/// without a compositor. `managed` is the `HEADLESS-` prefix, so a concurrent
/// session's output is never blacked out.
///
/// The prefix is blunt: sway's own bootstrap `HEADLESS-1` is spared too. Leaving a
/// headless box's only screen lit is the cheaper failure vs disabling a sibling.
fn heads_to_disable(heads: &[crate::monitors::PhysicalMonitor], ours: &str) -> Vec<String> {
    heads
        .iter()
        .filter(|h| h.enabled && !h.managed && h.connector != ours)
        .map(|h| h.connector.clone())
        .collect()
}

/// Disable every non-managed head for `exclusive`. Returns those actually disabled
/// ([`restore_heads`]). One refusal costs that screen, not the session.
fn disable_other_heads(ours: &str) -> Vec<String> {
    let heads = match list_monitors() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "wlroots: could not enumerate outputs for `topology: exclusive` — leaving the \
                 operator's heads enabled (the session still streams, as `extend`)"
            );
            return Vec::new();
        }
    };
    let targets = heads_to_disable(&heads, ours);
    if targets.is_empty() {
        tracing::info!(
            "wlroots: `topology: exclusive` had nothing to disable — no enabled output besides the \
             headless ones (a headless box, or a sibling session already took the desk)"
        );
        return Vec::new();
    }
    let mut disabled = Vec::new();
    for name in targets {
        match disable_head(&name) {
            Ok(()) => disabled.push(name),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "wlroots: could not disable this output for `topology: exclusive` — it stays lit"
            ),
        }
    }
    if !disabled.is_empty() {
        tracing::info!(
            ?disabled,
            "wlroots: `topology: exclusive` — the streamed output is now the desk"
        );
        // Disable moves workspaces; sway picks a new focus. Re-assert ours so
        // launches still land on the stream.
        focus_output(ours);
    }
    disabled
}

/// `swaymsg output <name> disable`, then read back. A bad command already fails
/// [`swaymsg`] (unlike `hyprctl`'s exit 0). The poll proves the output went
/// inactive — the state teardown must undo.
fn disable_head(name: &str) -> Result<()> {
    swaymsg(&disable_argv(name)).with_context(|| format!("swaymsg output {name} disable"))?;
    if wait_head_enabled_is(name, false, DISABLE_BUDGET) {
        return Ok(());
    }
    bail!("swaymsg accepted `output {name} disable` but the output never went inactive")
}

/// `output <name> disable` — noun first, opposite of [`focus_argv`]. Test-pinned.
fn disable_argv(name: &str) -> [&str; 3] {
    ["output", name, "disable"]
}

/// `output <name> dpms on|off`. Same noun-first shape as [`disable_argv`], different
/// axis: `dpms off` leaves the output enabled (workspaces stay) and stops the panel.
fn dpms_argv(name: &str, on: bool) -> [&str; 4] {
    ["output", name, "dpms", if on { "on" } else { "off" }]
}

/// DPMS every non-ours, non-sibling head for a **gamescope** `Topology::Exclusive`
/// ([`crate::panel_dpms`]).
///
/// Not [`disable_other_heads`]: gamescope is its own compositor and owns no sway
/// output, so disable would move workspaces for a stream that is not on this
/// compositor. Empty `ours` still spares a concurrent session's `HEADLESS-*`.
/// Returns the heads actually changed. One refusal costs a lit screen, not the stream.
pub(crate) fn dpms_other_heads(on: bool) -> Vec<String> {
    let Ok(heads) = list_monitors() else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for name in heads_to_disable(&heads, "") {
        match swaymsg(&dpms_argv(&name, on)) {
            Ok(_) => changed.push(name),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "wlroots: could not DPMS this output for `topology: exclusive`"
            ),
        }
    }
    changed
}

/// `output <name> enable`. Sway keeps a disabled output's config, so this restores
/// mode/position/scale; Hyprland needs `reload` instead.
fn enable_argv(name: &str) -> [&str; 3] {
    ["output", name, "enable"]
}

/// 3 s for `disable`/`enable` to show in `get_outputs`. A miss is reported, never assumed.
const DISABLE_BUDGET: Duration = Duration::from_secs(3);

fn wait_head_enabled_is(name: &str, want: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(head_is_enabled(name), Ok(Some(got)) if got == want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Sway's `active` for `name`. `None` if absent. A disabled output stays in
/// `get_outputs` with `"active": false` — that is the read-back, not presence.
fn head_is_enabled(name: &str) -> Result<Option<bool>> {
    let parsed = swaymsg_query("get_outputs")?;
    let Some(arr) = parsed.as_array() else {
        return Ok(None);
    };
    for o in arr {
        if o.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(Some(
                o.get("active").and_then(|v| v.as_bool()).unwrap_or(true),
            ));
        }
    }
    Ok(None)
}

/// Re-enable heads `exclusive` disabled. The registry runs this when the group's
/// last member tears down, and **before** that member's output is unplugged — sway
/// must not see zero enabled outputs. `enable` is the inverse of `disable`
/// (Hyprland needs `hyprctl reload`). A miss logs the hand command.
fn restore_heads(disabled: &[String]) {
    for name in disabled {
        match swaymsg(&enable_argv(name)) {
            Ok(_) => {
                if wait_head_enabled_is(name, true, DISABLE_BUDGET) {
                    tracing::info!(output = %name, "wlroots: re-enabled the output `topology: exclusive` disabled");
                } else {
                    tracing::warn!(
                        output = %name,
                        "wlroots: `output enable` was accepted but the output is still inactive — \
                         re-enable it by hand with `swaymsg output {name} enable`"
                    );
                }
            }
            Err(e) => tracing::error!(
                output = %name, error = %format!("{e:#}"),
                "wlroots: could not re-enable this output — it is still dark. Run \
                 `swaymsg output {name} enable` by hand."
            ),
        }
    }
}

struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match swaymsg(&["output", &self.0, "unplug"]) {
            Ok(_) => tracing::info!(output = %self.0, "sway headless output unplugged"),
            Err(e) => tracing::warn!(output = %self.0, error = %format!("{e:#}"), "unplug failed"),
        }
    }
}

/// 5 s per `swaymsg` ([`crate::proc`]). Against a wedged sway the client blocks in
/// connect forever; these calls run on the stream thread, whose only end is return.
/// Every call site already has a failed-query path.
const SWAYMSG_BUDGET: Duration = Duration::from_secs(5);

/// 10 s for `systemctl --user try-restart` (waits for the job; result is ignored).
const PORTAL_RESTART_BUDGET: Duration = Duration::from_secs(10);

/// Bare `swaymsg` with `SWAYSOCK` on the child only. `Command::env` avoids a process
/// `setenv` racing every `getenv` on a live host. `sock` is `None` when no IPC is
/// found: leave the child's env alone so an inherited socket still wins.
fn swaymsg_command(sock: Option<String>) -> Command {
    let mut cmd = Command::new("swaymsg");
    if let Some(sock) = sock {
        cmd.env("SWAYSOCK", sock);
    }
    cmd
}

/// `swaymsg -- <args>` (`--` so `--custom` reaches sway, not swaymsg's getopt).
/// Non-zero exit covers `{"success": false}` too.
fn swaymsg(args: &[&str]) -> Result<String> {
    let mut cmd = swaymsg_command(crate::session::sway_socket());
    let out = crate::proc::output_within(cmd.arg("--").args(args), SWAYMSG_BUDGET)
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

/// Query (`-t <kind> --raw`) and parse JSON. Not [`swaymsg`]: that helper inserts
/// `--`, so `-t` is read as a sway command (`Unknown/invalid command '-t'`).
fn swaymsg_query(kind: &str) -> Result<serde_json::Value> {
    let mut cmd = swaymsg_command(crate::session::sway_socket());
    let out = crate::proc::output_within(cmd.args(["-t", kind, "--raw"]), SWAYMSG_BUDGET)
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

fn output_names() -> Result<Vec<String>> {
    let outputs = swaymsg_query("get_outputs")?;
    Ok(outputs
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| o.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect())
}

/// Serializes write-the-chooser → complete-the-handshake, process-wide. One file
/// per user: last writer before xdpw reads wins, and the loser silently captures
/// the other session's output. Held across the handshake because the read is inside it.
static SELECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Per-session chooser file, removed when the handshake it steers ends.
///
/// Lifetime is the handshake, not the cast: xdpw reads once inside [`select_and_cast`].
/// A leftover `Monitor: HEADLESS-N` shadows [`chooser_cmd`]'s fallback after Drop
/// unplugs that output. Tying removal to the cast would delete a sibling's selection.
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

/// Point xdpw's chooser at `output` and run the ScreenCast handshake. Caller holds
/// [`SELECTION_LOCK`].
fn select_and_cast(
    output: &str,
    hw_cursor: bool,
) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode, StopGuard)> {
    ensure_xdpw_config()?;
    let chooser = chooser_file();
    std::fs::write(&chooser, format!("Monitor: {output}\n"))
        .with_context(|| format!("write {chooser}"))?;
    // Drop removes it; every `?` below leaves the handshake, the only reader.
    let _chooser = ChooserFile(chooser);
    // Negotiated inside the portal thread (only there is the proxy). `hw_cursor` is
    // the request, not the answer.
    let (setup_tx, setup_rx) =
        std::sync::mpsc::channel::<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>();
    // Teardown channel, not setup: it fires after `setup_rx` is consumed, when
    // `StopGuard::drop` must wait before unplug.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-wlr-cast".into())
        .spawn(move || portal_thread(setup_tx, closed_tx, stop_thread, hw_cursor))
        .context("spawn wlroots portal thread")?;
    // Build the guard before the wait so every error arm sets `stop` on the way
    // out. Wrapping later left failure arms dropping an unset flag: the thread can
    // still `send` after `recv_timeout`, report success, then park forever on a
    // live ScreenCast against an output that is gone.
    let mut guard = StopGuard { stop, closed: None };
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id, cursor_mode))) => {
            // Cast is live: teardown must wait for Close.
            guard.closed = Some(closed_rx);
            Ok((fd, node_id, cursor_mode, guard))
        }
        Ok(Err(e)) => bail!("ScreenCast portal on {output} failed: {e}"),
        Err(_) => bail!("timed out waiting for the ScreenCast portal on {output}"),
    }
}

/// Cast an existing sway output (monitor-mirror; see
/// `design/per-monitor-portal-capture.md`). Same chooser, physical connector, no
/// GUI picker. Keepalive stops the cast only — we never created the monitor.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (fd, node_id, cursor_mode, stop) = select_and_cast(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: Some(fd),
        cursor_mode: Some(cursor_mode),
        keepalive: Box::new(stop),
    })
}

/// Every head `get_outputs` reports, for [`crate::monitors::list`]. `rect` is
/// logical (post-scale, post-transform). Inactive outputs have no `current_mode`;
/// mode fields read as zeros, not a guess.
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
                // Prefix match only; a sway-owned bootstrap HEADLESS-* counts too.
                managed: connector.starts_with("HEADLESS-"),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// The name `create_output` added: present now, absent from `before`.
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

/// Point xdpw at our chooser. It reads config only at startup, so `try-restart` on
/// change (D-Bus activation starts it later if it is not running). Selection is the
/// chooser file; this config is static.
fn ensure_xdpw_config() -> Result<()> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
    let path = base.join("xdg-desktop-portal-wlr").join("config");
    // Only the two keys we own, in place. A full-file write would wipe the user's other xdpw settings.
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
    // Stream thread: `systemctl --user` blocks on the job queue. Result already ignored.
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "try-restart", "xdg-desktop-portal-wlr.service"]),
        PORTAL_RESTART_BUDGET,
    );
    Ok(())
}

/// ScreenCast handshake: report fd + node id, then park. The zbus connection is the
/// cast's lifetime. xdpw selects via the chooser, no dialog.
fn portal_thread(
    setup_tx: Sender<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>,
    closed_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Multi-thread: zbus's reader must run across create_session → select_sources →
    // start. Shared, never dropped ([`crate::portal_rt`]): a per-cast runtime kills
    // ashpd's process-global cached connection and every later handshake hangs.
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
            // Orphaned cached connection hangs here, before any handshake call.
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
            // Negotiate against what xdpw advertises, never from `hw_cursor` alone.
            // screencast.c cancels the session if METADATA is set.
            let cursor_mode = crate::portal_cursor::negotiate(&proxy, hw_cursor, "xdpw").await;
            // A wedged portal never returns; `stop` is only read in the park loop, so
            // an unbounded await leaks a half-handshake and poisons later requests.
            // xdpw has the same unbounded node-id spin as xdph (`screencast.c`).
            let handshake = async {
                let session = proxy
                    .create_session(Default::default())
                    .await
                    .context("create_session")?;
                proxy
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(cursor_mode.to_ashpd())
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
                .send(Ok((fd, node_id, cursor_mode)))
                .map_err(|_| anyhow!("virtual-output opener went away"))?;

            // Keep `proxy` + `session` alive. 20 ms poll: teardown waits on Close.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // Session.Close is the only xdpw teardown (`src/core/session.c`); dropping
            // the connection is not. `StopGuard::drop` is blocked on the send below.
            // Bounded so a wedged portal cannot hang unplug.
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
            // Best-effort: the receiver is gone if the caller already gave up.
            let _ = closed_tx.send(());
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

    /// `focus output <name>` — noun second. `output focus <name>` is rejected, and
    /// the only symptom is apps opening on the operator's monitor.
    #[test]
    fn focus_names_the_output_after_the_verb() {
        assert_eq!(focus_argv("HEADLESS-2"), ["focus", "output", "HEADLESS-2"]);
    }

    /// Topology verbs are `output <name> <verb>` (noun first), unlike [`focus_argv`].
    /// Both orders are pinned because this file uses both.
    #[test]
    fn disable_and_enable_name_the_output_before_the_verb() {
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
        assert_eq!(enable_argv("DP-1"), ["output", "DP-1", "enable"]);
    }

    /// `SWAYSOCK` is a per-child override, never `set_var` on the host (that write
    /// races every `getenv`). Known socket is set; unknown leaves the child's env.
    #[test]
    fn the_sway_socket_travels_on_the_child_not_the_process_env() {
        let overrides = |sock: Option<String>| -> Vec<(String, Option<String>)> {
            swaymsg_command(sock)
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect()
        };
        assert_eq!(
            overrides(Some("/run/user/1000/sway-ipc.1000.42.sock".to_string())),
            [(
                "SWAYSOCK".to_string(),
                Some("/run/user/1000/sway-ipc.1000.42.sock".to_string())
            )]
        );
        assert!(overrides(None).is_empty());
    }

    fn head(connector: &str, enabled: bool) -> crate::monitors::PhysicalMonitor {
        crate::monitors::PhysicalMonitor {
            connector: connector.to_string(),
            description: connector.to_string(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled,
            // The real `list_monitors` derives this from the `HEADLESS-` prefix; mirror it here so
            // the fixture can't drift into asserting a rule the backend doesn't actually apply.
            managed: connector.starts_with("HEADLESS-"),
        }
    }

    /// `exclusive` disables the operator's outputs only. A sibling `HEADLESS-N` must
    /// survive or the second session blacks out the first.
    #[test]
    fn exclusive_disables_the_operators_outputs_and_never_a_headless_sibling() {
        let ours = "HEADLESS-2";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            // Sibling session, or sway's own bootstrap headless — both spared.
            head("HEADLESS-1", true),
            // Already off: must not enter the restore list or teardown would switch it on.
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ours), vec!["DP-1", "HDMI-A-1"]);
    }

    /// `dpms` ≠ `disable`: disable moves workspaces; `dpms off` only stops the panel.
    /// Four tokens: `output <name> dpms on|off`.
    #[test]
    fn dpms_is_a_separate_verb_from_disable() {
        assert_eq!(dpms_argv("DP-1", false), ["output", "DP-1", "dpms", "off"]);
        assert_eq!(dpms_argv("DP-1", true), ["output", "DP-1", "dpms", "on"]);
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
    }

    /// Gamescope DPMS reuses the disable filter with empty `ours`: nothing of ours
    /// to spare, but a concurrent session's `HEADLESS-*` still must not go dark.
    #[test]
    fn the_gamescope_dpms_arm_still_spares_a_sibling_headless() {
        let heads = [
            head("DP-1", true),
            head("HEADLESS-1", true),
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ""), vec!["DP-1"]);
    }

    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "HEADLESS-1";
        assert!(heads_to_disable(&[head(ours, true)], ours).is_empty());
    }
}
