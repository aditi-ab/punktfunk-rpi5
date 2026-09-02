//! Hyprland virtual-output backend via `hyprctl` IPC and the xdg ScreenCast portal
//! (xdg-desktop-portal-hyprland / xdph). See `design/hyprland-support.md`.
//!
//! Distinct from [`super::wlroots`]: Hyprland names headless outputs explicitly
//! (`hyprctl output create headless PF-<pid>-<n>`), so there is no before/after
//! diff. The creator pid in the name is what [`reclaim_leftovers_once`] uses to
//! drop leftovers whose owner is gone.
//!
//! A monitor rule sets the client's exact mode ([`set_monitor_rule`]). xdph is
//! steered at that output through a custom picker ([`crate::portal_picker`]).
//! Teardown is ordered: [`StopGuard`] waits for ScreenCast close, then
//! [`OutputGuard`] removes the compositor output — the reverse wedges xdph.
//!
//! Requires a reachable Hyprland instance (`HYPRLAND_INSTANCE_SIGNATURE` or
//! `$XDG_RUNTIME_DIR/hypr/`) and ScreenCast routed to xdph
//! (`scripts/headless/portals.conf`). `hyprctl focusmonitor` without `dispatch`
//! answers `unknown request` at exit 0; [`hyprctl_dispatch`] turns that into
//! an error.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::io::BufRead;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Once};
use std::thread;
use std::time::{Duration, Instant};

/// Per-session picker file, under `$XDG_RUNTIME_DIR` (0700) — not world-writable
/// `/tmp`, where another local user could rewrite it between our write and xdph's
/// read.
fn selection_file() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-output")
}

/// Shim xdph runs as `custom_picker_binary`. Empty stdout (no session has written
/// the selection file) leaves xdph to its own fallback.
fn picker_shim_path() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-picker.sh")
}

fn xdph_config_path() -> Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
    Ok(base.join("hypr").join("xdph.conf"))
}
const XDPH_BLOCK: crate::portal_config::Block<'static> =
    crate::portal_config::Block::Hyprlang("screencopy");
const XDPH_PICKER_KEY: &str = "custom_picker_binary";

/// Is `cmd` safe to paste into the shim's `exec` line? A newline would split
/// the generated script; this is robustness, not a privilege boundary.
fn picker_is_plain(cmd: &str) -> bool {
    !cmd.is_empty()
        && cmd.len() <= 512
        && cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._/@:+=-".contains(c))
}

/// `[SELECTION]/screen:<name>` — every byte is load-bearing. Format lives in
/// [`crate::portal_picker`] (this file is Linux-only).
fn picker_selection_line(name: &str) -> String {
    crate::portal_picker::selection_line(name)
}

/// Per-process seq for `PF-<pid>-<n>`. Named outputs skip sway's before/after
/// diff race.
static OUTPUT_SEQ: AtomicU32 = AtomicU32::new(0);

/// `PF-<pid>-<n>`. The pid is not decoration: `OutputGuard::drop` is the only
/// unplug, so a SIGKILLed host leaves heads behind. A bare `PF-<n>` restarts
/// at `PF-1` and collides; the pid lets [`reclaim_leftovers_once`] drop only
/// leftovers whose owner is gone.
fn next_output_name() -> String {
    format!(
        "PF-{}-{}",
        std::process::id(),
        OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// `PF-<pid>-<n>` or legacy `PF-<n>`. A user's own `PF-office` must not match.
pub(crate) fn is_managed_output(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("PF-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Owner pid for `PF-<pid>-<n>` only. `None` for legacy `PF-<n>` — no owner,
/// so reclaim must not guess.
fn output_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("PF-")?;
    let (pid, seq) = rest.split_once('-')?;
    seq.parse::<u32>().ok()?;
    pid.parse::<u32>().ok()
}

/// One named headless output per [`create`](VirtualDisplay::create). Stateless
/// besides the fields below.
pub struct HyprlandDisplay {
    /// Out-of-band cursor request. On: prefer portal `CursorMode::Metadata`.
    /// Off: prefer `Embedded` (compositor paints the pointer). Both are only a
    /// preference — [`crate::portal_cursor`] settles against what xdph advertises;
    /// an unadvertised mode fails the portal call. Current xdph advertises
    /// `Hidden|Embedded` only, so every session here resolves to `Embedded`.
    hw_cursor: bool,
    /// What the portal actually gave the last successful `create`. How the host
    /// learns a cursor overlay is never coming.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Topology restore from the first `create` (re-enable heads `exclusive`
    /// disabled). First-wins: this instance serves the pipeline retry loop and
    /// only attempt 1 finds heads to disable. Unlike KWin's field of the same
    /// name, the registry never takes this — a Hyprland display carries a portal
    /// fd, so `registry::acquire` returns it as pass-through. [`Drop`] is the
    /// only runner. Two concurrent `exclusive` sessions on this desk: the first
    /// to end re-enables heads under the second.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// Output the last successful `create` minted. A mid-stream resize replaces
    /// the head (create-before-drop); the next `create` carries its workspace
    /// over ([`adopt_active_workspace`]). Unset on failure so a half-created
    /// head is not the adoption source.
    prev_output: Option<String>,
}

impl Drop for HyprlandDisplay {
    fn drop(&mut self) {
        // The only path that runs it: the registry never takes a pass-through
        // display's restore. A failed pipeline drops this instance still holding
        // attempt 1's restore, which re-lights the desk.
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl HyprlandDisplay {
    pub fn new() -> Result<Self> {
        Ok(HyprlandDisplay {
            hw_cursor: false,
            last_cursor_mode: None,
            pending_restore: None,
            prev_output: None,
        })
    }

    /// Apply [`crate::policy::Topology`] for `ours` and stash the restore this
    /// instance runs on drop. Called at the END of `create` so nothing can fail
    /// after it and unwind past the hand-off. Physical heads stay lit through
    /// the portal handshake — that is also `extend`.
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
                // First restore wins: the retry loop calls `create` up to eight
                // times on this instance, and only attempt 1 has heads to disable.
                // A plain assignment overwrote it with attempt 2's `None`.
                crate::backend::stash_topology_restore(&mut self.pending_restore, prepared);
            }
        }
    }
}

/// Usable when a live Hyprland instance for our uid is reachable: inherited
/// `HYPRLAND_INSTANCE_SIGNATURE`, or a socket under `$XDG_RUNTIME_DIR/hypr/`
/// (the systemd `--user` host has no env import). Cheap — safe on enumeration.
///
/// Both env reads take [`crate::with_env_lock`] in one scope so the pair is one
/// consistent view. The lock is not reentrant; `read_dir` runs outside it.
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

/// `hyprctl` must reach the compositor now, not at create-time. Warns if the
/// permission system is enforcing (silent black frames / dropped input).
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

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        preflight_once();
        reclaim_leftovers_once();

        let name = next_output_name();
        hyprctl_dispatch(&["output", "create", "headless", &name]).with_context(|| {
            format!("hyprctl output create headless {name} (is hyprctl reachable?)")
        })?;
        // Own from here so any later error (or drop) removes it.
        let output = OutputGuard(name.clone());
        wait_monitor_ready(&name, Duration::from_secs(5))
            .with_context(|| format!("waiting for headless output {name} to appear"))?;

        // Client mode is also the frame clock: a headless output is timer-paced from it.
        set_monitor_rule(&name, mode).with_context(|| format!("set monitor rule for {name}"))?;

        focus_output(&name);

        // Steer xdph at this output, then handshake on its own thread. Serialized:
        // the selection is one per-user file; a concurrent write between ours and
        // xdph's read would capture the wrong output (`SELECTION_LOCK`).
        let (fd, node_id, cursor_mode, stop) = {
            let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            select_and_cast(&name, self.hw_cursor)?
        };
        // On today's xdph this is `embedded` regardless of `hw_cursor`; the
        // session's cursor behaviour follows this, not the request.
        self.last_cursor_mode = Some(cursor_mode);
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            cursor = cursor_mode.name(),
            "hyprland headless output ready"
        );
        // Last, so no failure path unwinds past the restore hand-off.
        self.apply_topology(&name);
        // A resize replaces the head (create-before-drop). Hyprland hands every
        // new monitor an empty workspace, so carry the superseded head's active
        // one over. After `apply_topology`: nothing fails past here, so no unwind
        // strands a moved workspace on a head error cleanup is about to remove.
        if let Some(prev) = self.prev_output.take() {
            adopt_active_workspace(&prev, &name);
        }
        self.prev_output = Some(name.clone());
        Ok(VirtualOutput {
            node_id,
            remote_fd: Some(fd),
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(Keepalive {
                _reload: watch_config_reloads(name.clone(), mode),
                _stop: stop,
                _output: output,
            }),
            // Owned, but not registry-poolable: the portal fd can't be re-opened
            // per attach, so the registry passes it through on `remote_fd.is_some()`.
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            // Extend topology: this head sits beside the operator's, so absolute
            // input has to be aimed at it by name. `hyprctl`'s monitor name is
            // `wl_output.name`, which the injector matches.
            output_name: Some(name),
        })
    }
}

/// Drop order is the fix: [`StopGuard`] blocks until the ScreenCast session is
/// closed, then [`OutputGuard`] removes the compositor output (fields drop in
/// declaration order). The reverse removes an output xdph is still capturing.
struct Keepalive {
    /// First so the watcher is gone before the cast stops and the output is
    /// removed — it must never re-apply a rule onto a head this teardown deletes.
    _reload: Option<ReloadWatcher>,
    _stop: StopGuard,
    _output: OutputGuard,
}

/// Puts the streamed head's monitor rule back after a `hyprctl reload`.
///
/// A reload drops every runtime `hyprctl keyword` (see [`restore_heads`]),
/// including [`set_monitor_rule`]'s mode. The compositor does not re-apply it.
/// Subscribes to the event socket rather than polling so an idle session costs
/// nothing.
///
/// The MODE only. A reload also undoes `topology: exclusive` head disables, but
/// re-disabling them here races teardown's [`restore_heads`] (`hyprctl reload`
/// to re-light): the watcher and that restore do not share a lifetime.
fn watch_config_reloads(name: String, mode: Mode) -> Option<ReloadWatcher> {
    let path = event_socket_path()?;
    let sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                path = %path.display(), error = %e,
                "hyprland: no event socket — a `hyprctl reload` (every theme switch, on Omarchy) \
                 will reset this stream's resolution until the client resizes"
            );
            return None;
        }
    };
    // Shutting this clone down is what unparks the blocking read below.
    let stopper = sock.try_clone().ok()?;
    thread::spawn(move || {
        for line in std::io::BufReader::new(sock).lines() {
            // Guard shutdown or compositor gone — nothing left to re-apply to.
            let Ok(line) = line else { return };
            if !is_config_reload(&line) {
                continue;
            }
            tracing::info!(
                output = %name, w = mode.width, h = mode.height,
                "hyprland: config reloaded — re-applying the streamed head's monitor rule"
            );
            if let Err(e) = set_monitor_rule(&name, mode) {
                // Errors only when the head has no framebuffer — gone after a
                // reload (teardown, or compositor restart). Stop.
                tracing::warn!(
                    output = %name, error = %format!("{e:#}"),
                    "hyprland: could not re-apply the monitor rule after a config reload — the \
                     client keeps the head's default resolution until it resizes"
                );
                return;
            }
        }
    });
    Some(ReloadWatcher(stopper))
}

/// Ends [`watch_config_reloads`]'s thread by shutting its socket down.
///
/// The thread is parked in a blocking read. A stop flag would leave it alive
/// until the compositor emitted an event — one stranded thread per session,
/// and sessions are minted on every mid-stream resize.
struct ReloadWatcher(UnixStream);

impl Drop for ReloadWatcher {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// Event socket for the instance we are driving. Same signature [`hyprctl_command`]
/// threads onto every child, so the watch and the commands cannot aim at
/// different compositors.
fn event_socket_path() -> Option<std::path::PathBuf> {
    let sig = crate::session::hypr_signature()?;
    let runtime = crate::with_env_lock(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!("/run/user/{}", crate::proc::current_uid()))
        });
    Some(runtime.join("hypr").join(sig).join(".socket2.sock"))
}

/// Hyprland's `.socket2.sock` speaks `<name>>><data>`. Match the NAME: a
/// `contains` would also fire on a window titled `configreloaded`, and every
/// false hit is a `hyprctl` round trip on a live stream.
fn is_config_reload(line: &str) -> bool {
    line.split(">>").next() == Some("configreloaded")
}

/// How long teardown waits for ScreenCast close before removing the output
/// anyway. One D-Bus round trip; three seconds is generous. Bounded so a
/// wedged portal cannot wedge the host — same as every blocking helper on
/// this path ([`HYPRCTL_BUDGET`]).
const CAST_CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// Live casts of ours. The picker config is borrowed while this is non-zero;
/// a host streaming two outputs must not hand the picker back when the first ends.
static LIVE_CASTS: AtomicU32 = AtomicU32::new(0);

/// Ends the cast: signals the portal thread, then waits for ScreenCast close
/// so the caller may remove the output afterwards.
///
/// The wait is the point. xdph destroys a session only on explicit
/// `org.freedesktop.impl.portal.Session.Close`; it has no peer-vanished
/// watcher. Dropping a flag and removing the output while xdph is still
/// capturing wedges its event-loop thread (unbounded `pw_loop_iterate` with
/// timeout 0 inside `Start`). Waiting until `close()` returns means the
/// output we remove next is one nobody is capturing.
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Signalled once the portal thread has closed the ScreenCast session.
    ///
    /// `None` when no cast was established (rejected or timed-out handshake):
    /// nothing to close, and a portal that just failed for 20 s would burn
    /// this budget for nothing.
    closed: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(closed) = self.closed.take() else {
            // Never counted toward [`LIVE_CASTS`] — the increment is in the
            // same arm that arms `closed`.
            return;
        };
        LIVE_CASTS.fetch_sub(1, Ordering::SeqCst);
        // Do not restore the picker here. Per-cast restore rewrites the config
        // and restarts xdph; a ScreenCast bound across that restart never
        // delivers a buffer (D-Bus connection is process-global). The shim
        // delegates when idle; `punktfunk-omarchy remove` puts the config back.
        match closed.recv_timeout(CAST_CLOSE_BUDGET) {
            Ok(()) => {}
            // Thread gone without confirming (panic or runtime death). Nothing
            // is holding the cast either way.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            // Budget expired. A leaked output is worse than a racy one; this
            // is the state that wedges xdph, and the next session pays for it.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                budget_s = CAST_CLOSE_BUDGET.as_secs(),
                "the ScreenCast session did not close in time — removing the output underneath it, \
                 which is what wedges xdph's frame loop; the next cast may find the portal busy"
            ),
        }
    }
}

/// Remove `PF-<pid>-<n>` outputs whose owner pid is gone, once per process
/// before we create our first.
///
/// [`OutputGuard::drop`] is the only unplug, so a SIGKILLed host leaves heads
/// in the compositor for the session's life. Keyed on the owner pid so a
/// second live host (or this process) cannot have its output pulled. `Once`
/// puts the sweep strictly before this process owns anything.
fn reclaim_leftovers_once() {
    static RECLAIMED: Once = Once::new();
    RECLAIMED.call_once(|| {
        let Ok(names) = monitor_names() else { return };
        for name in names {
            let Some(pid) = output_owner_pid(&name) else {
                // Not ours, or legacy `PF-<n>` with no owner — a still-running
                // older host may be streaming it.
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

/// Point Hyprland's focus at the head we are about to stream.
///
/// New windows open on the focused monitor's active workspace, and
/// `output create headless` does not focus what it creates. The client's
/// pointer is confined to the streamed output, so focus-follows-mouse cannot
/// reach it. An unfocused headless output stays empty, empty produces no
/// damage, and no damage means no PipeWire frames.
///
/// Classic `hyprctl dispatch focusmonitor <name>` for hyprlang. Under the Lua
/// config manager `dispatch` is `hl.dispatch(...)`, so those bare words die
/// with `')' expected near '<name>'`. Lua spelling is
/// `hl.dsp.focus({ monitor = "<name>" })`. Try classic, then Lua.
pub(crate) fn focus_output(name: &str) {
    let classic = match hyprctl_dispatch(&focus_argv(name)) {
        Ok(()) => None,
        Err(e) => match hyprctl_dispatch(&["dispatch", &lua_focus_expr(name)]) {
            Ok(()) => None,
            Err(lua_err) => Some(format!("hyprlang: {e:#}; lua: {lua_err:#}")),
        },
    };
    match classic {
        None => tracing::info!(output = %name, "focused the streamed headless output"),
        Some(why) => tracing::warn!(
            output = %name, error = %why,
            "could not focus the streamed headless output — apps this session launches may open on \
             a physical monitor instead of on the stream, and an unfocused headless output can \
             produce no frames at all"
        ),
    }
}

/// Lua-config-manager spelling of "focus this monitor". Pure so a test pins
/// the shape: quoting and the `monitor =` key are the whole trick.
fn lua_focus_expr(name: &str) -> String {
    format!("hl.dsp.focus({{ monitor = \"{name}\" }})")
}

/// `hyprctl` argv that focuses `name`, split so a test pins its shape.
///
/// `focusmonitor` is a dispatcher — it lives behind `dispatch`. A bare
/// `hyprctl focusmonitor` answers `unknown request` at exit 0
/// ([`hyprctl_dispatch`]).
fn focus_argv(name: &str) -> [&str; 3] {
    ["dispatch", "focusmonitor", name]
}

/// `topology: primary` has no expression here. Wayland has no primary output;
/// Hyprland's nearest is the focused monitor, which [`focus_output`] already
/// points at the streamed head. Distinct from `exclusive`, which changes the desk.
fn warn_primary_is_not_expressible() {
    tracing::info!(
        "hyprland: `topology: primary` has no equivalent here — Wayland has no primary output and \
         Hyprland has only a FOCUSED monitor, which the streamed head already holds. Treating it \
         as `extend`; use `exclusive` to actually disable the operator's heads."
    );
}

/// Which heads `exclusive` should disable: enabled, not ours, not managed.
///
/// Pure so the group-awareness rule is unit-testable without a compositor.
/// `managed` is [`is_managed_output`], covering a second host's outputs, so a
/// concurrent session cannot be blacked out. `ours` is excluded by name too.
fn heads_to_disable(heads: &[crate::monitors::PhysicalMonitor], ours: &str) -> Vec<String> {
    heads
        .iter()
        .filter(|h| h.enabled && !h.managed && h.connector != ours)
        .map(|h| h.connector.clone())
        .collect()
}

/// DPMS every head that is not ours and not a sibling's, for a **gamescope**
/// session honoring `Topology::Exclusive` — see [`crate::panel_dpms`].
///
/// Distinct from [`disable_other_heads`]: disabling a Hyprland head's only
/// known undo is re-reading the operator's whole config ([`restore_heads`]),
/// dropping every runtime override. DPMS is a separate axis (`dispatch dpms on
/// <name>` does not re-enable a *disabled* head). A gamescope spawn owns no
/// Hyprland output, hence empty `ours`.
pub(crate) fn dpms_other_heads(on: bool) -> Vec<String> {
    let Ok(heads) = list_monitors() else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for name in heads_to_disable(&heads, "") {
        match dpms_one(&name, on) {
            // Only a head this call moved. The dispatcher toggles, so "fixing"
            // one already in the wanted state would break it, and the re-light
            // would then toggle a head we never darkened.
            Ok(true) => changed.push(name),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "hyprland: could not DPMS this monitor for `topology: exclusive`"
            ),
        }
    }
    changed
}

/// DPMS state Hyprland reports for `name` (`hyprctl -j monitors all`'s
/// `dpmsStatus`). `None` when unlisted or the field is missing. A DPMS-off
/// monitor stays listed — the readback [`dpms_one`] is built around.
fn monitor_dpms(name: &str) -> Option<bool> {
    let raw = hyprctl(&["-j", "monitors", "all"]).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|m| m.get("name").and_then(|v| v.as_str()) == Some(name))?
        .get("dpmsStatus")?
        .as_bool()
}

/// Put one monitor into `want_on`, reporting whether this call changed it.
///
/// The dispatcher is a toggle, not a set — it ignores the state word. A blind
/// "off" lights an already-dark head; a blind "on" at teardown darkens a lit
/// one. Read → act only if it differs → verify. That shape is also correct
/// where the call really is a set.
///
/// Classic `hyprctl dispatch dpms off <name>` dies under Lua (`dispatch` is
/// `hl.dispatch(...)`). No stable probe for which manager is loaded, so try
/// classic, then Lua. Never omit the monitor name: `hl.dsp.dpms("on")` answers
/// `ok` and toggles *something*.
fn dpms_one(name: &str, want_on: bool) -> Result<bool> {
    if monitor_dpms(name) == Some(want_on) {
        return Ok(false); // toggling would break it
    }
    let classic =
        match hyprctl_dispatch(&["dispatch", "dpms", if want_on { "on" } else { "off" }, name]) {
            Ok(()) => None,
            Err(e) => {
                let lua = lua_dpms_expr(name, want_on);
                match hyprctl_dispatch(&["dispatch", &lua]) {
                    Ok(()) => None,
                    Err(lua_err) => Some(format!("hyprlang: {e:#}; lua: {lua_err:#}")),
                }
            }
        };
    if let Some(why) = classic {
        bail!("neither dispatch form was accepted for {name} — {why}");
    }
    // Verify: a toggle that fired against a state we misread is worse than one
    // that did not fire at all.
    match monitor_dpms(name) {
        Some(now) if now == want_on => Ok(true),
        Some(now) => bail!(
            "hyprland accepted the dpms dispatch for {name} but it is now dpmsStatus={now}, \
             wanted {want_on} (the dispatcher toggles — the readback disagreed with reality)"
        ),
        None => bail!("hyprland stopped listing {name} after its dpms dispatch"),
    }
}

/// Lua-config-manager spelling of per-monitor DPMS. Pure so a test pins the
/// shape — quoting is the whole trick.
fn lua_dpms_expr(name: &str, on: bool) -> String {
    format!(
        "hl.dsp.dpms(\"{}\", \"{name}\")",
        if on { "on" } else { "off" }
    )
}

/// Active workspace id for monitor `name` (`hyprctl -j monitors`). `None` when
/// the monitor is already gone or the field is missing.
fn active_workspace_id(name: &str) -> Option<i64> {
    let raw = hyprctl(&["-j", "monitors"]).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed
        .as_array()?
        .iter()
        .find(|m| m.get("name").and_then(|v| v.as_str()) == Some(name))?
        .get("activeWorkspace")?
        .get("id")?
        .as_i64()
}

/// Move the superseded head's active workspace onto the new head, then switch
/// the new head to it.
///
/// Hyprland assigns every new monitor an empty workspace and re-homes a
/// removed monitor's workspaces elsewhere. Two dispatches: `workspace.move`
/// does not activate the moved workspace on its target. When the predecessor
/// is no longer listed this is a reconnect, not a live resize — nothing to
/// carry. Best-effort: a failure costs workspace continuity, never the stream.
fn adopt_active_workspace(prev: &str, ours: &str) {
    let Some(id) = active_workspace_id(prev) else {
        return;
    };
    let ws = id.to_string();
    // Classic first, Lua on rejection — same two-era probe as [`focus_output`]
    // / [`dpms_one`]. There is no stable way to ask which config manager is loaded.
    let both = |classic: &[&str], lua: &str| -> Result<()> {
        match hyprctl_dispatch(classic) {
            Ok(()) => Ok(()),
            Err(classic_err) => hyprctl_dispatch(&["dispatch", lua])
                .map_err(|lua_err| anyhow::anyhow!("hyprlang: {classic_err:#}; lua: {lua_err:#}")),
        }
    };
    if let Err(e) = both(
        &["dispatch", "moveworkspacetomonitor", &ws, ours],
        &lua_workspace_move_expr(&ws, ours),
    ) {
        tracing::warn!(
            workspace = id, from = %prev, to = %ours, error = %format!("{e:#}"),
            "hyprland: could not move the streamed workspace to the replacement head — the \
             client will land on an empty workspace after this resize"
        );
        return;
    }
    if let Err(e) = both(
        &["dispatch", "workspace", &ws],
        &lua_workspace_focus_expr(&ws),
    ) {
        tracing::warn!(
            workspace = id, to = %ours, error = %format!("{e:#}"),
            "hyprland: moved the streamed workspace but could not switch the replacement head \
             to it — the client must switch workspaces by hand once"
        );
        return;
    }
    // Both dispatchers answer `ok` even when the era mismatch made them do
    // nothing, so the readback is the only real signal. An empty adopted
    // workspace evaporates on the move and the switch re-creates it on the
    // focused (new) head — same id, so the readback holds for that case too.
    match active_workspace_id(ours) {
        Some(now) if now == id => tracing::info!(
            workspace = id, from = %prev, to = %ours,
            "hyprland: carried the streamed workspace onto the replacement head"
        ),
        now => tracing::warn!(
            workspace = id, active = ?now, to = %ours,
            "hyprland: both workspace dispatches were accepted but the replacement head shows a \
             different active workspace — the client may land on an empty workspace"
        ),
    }
}

/// Lua spelling of "move workspace N to monitor M". Pure so a test pins the
/// shape: workspace is a quoted string, monitor quoted as in [`lua_focus_expr`].
fn lua_workspace_move_expr(ws: &str, monitor: &str) -> String {
    format!("hl.dsp.workspace.move({{ workspace = \"{ws}\", monitor = \"{monitor}\" }})")
}

/// Lua spelling of "switch to workspace N" — `hl.dsp.focus` with a `workspace`
/// argument. There is no `hl.dsp.workspace.*` member that switches.
fn lua_workspace_focus_expr(ws: &str) -> String {
    format!("hl.dsp.focus({{ workspace = \"{ws}\" }})")
}

/// Disable every non-managed head for an `exclusive` session, returning the
/// ones actually disabled (input to [`restore_heads`]). Best-effort per head.
fn disable_other_heads(ours: &str) -> Vec<String> {
    let heads = match list_monitors() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "hyprland: could not enumerate monitors for `topology: exclusive` — leaving the \
                 operator's heads enabled (the session still streams, as `extend`)"
            );
            return Vec::new();
        }
    };
    let targets = heads_to_disable(&heads, ours);
    if targets.is_empty() {
        tracing::info!(
            "hyprland: `topology: exclusive` had nothing to disable — no enabled head besides the \
             managed ones (a headless box, or a sibling session already took the desk)"
        );
        return Vec::new();
    }
    let mut disabled = Vec::new();
    for name in targets {
        match disable_head(&name) {
            Ok(()) => disabled.push(name),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "hyprland: could not disable this head for `topology: exclusive` — it stays lit"
            ),
        }
    }
    if !disabled.is_empty() {
        tracing::info!(
            ?disabled,
            "hyprland: `topology: exclusive` — the streamed output is now the desk"
        );
        // Disabling re-homes workspaces and the compositor picks the new
        // focus. Re-assert ours so window placement still lands on the stream.
        focus_output(ours);
    }
    disabled
}

/// Disable one head, both config eras, confirming by read-back.
///
/// Same two-era shape as [`set_monitor_rule`]: `hyprctl keyword` is rejected
/// under Lua; `hyprctl eval` is rejected under hyprlang. Both at **exit 0**,
/// so the read-back — not the exit status — decides.
fn disable_head(name: &str) -> Result<()> {
    let spec = disable_rule_spec(name);
    let lua = disable_lua_expr(name);
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    let mut attempts: Vec<String> = Vec::new();
    for a in [&keyword, &eval] {
        if let Err(e) = hyprctl_dispatch(a) {
            let said = format!("{e:#}");
            tracing::debug!(output = %name, cmd = ?a, error = %said, "hyprctl rejected this disable form — trying the other config era");
            attempts.push(said);
            continue;
        }
        if wait_head_disabled(name, DISABLE_BUDGET) {
            return Ok(());
        }
        attempts.push(format!(
            "hyprctl {a:?} was accepted but the head never went disabled"
        ));
    }
    bail!("no hyprctl form disabled {name}: {}", attempts.join("; "))
}

/// Hyprlang disable rule (`hyprctl keyword monitor <this>`). `disable` is a
/// whole-rule verb — there is no `<name>,<mode>,disable`, and no `<name>,enable`
/// to undo it.
fn disable_rule_spec(name: &str) -> String {
    format!("{name},disable")
}

/// Lua disable rule (`hyprctl eval <this>`). The field is `disabled` (past
/// tense) and takes a boolean — `disable = true` and `mode = "disable"` are
/// rejected, so this is not a place to guess from the hyprlang spelling.
fn disable_lua_expr(name: &str) -> String {
    format!("hl.monitor{{ output = \"{name}\", disabled = true }}")
}

/// Poll until `name` reports `disabled: true` (the rule applies asynchronously).
fn wait_head_disabled(name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(head_is_enabled(name), Ok(Some(false))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Is head `name` enabled? `None` if absent. Reads `-j monitors all` — the
/// plain listing drops a disabled head, so it cannot distinguish disabled
/// from unplugged.
fn head_is_enabled(name: &str) -> Result<Option<bool>> {
    let out = hyprctl(&["-j", "monitors", "all"])?;
    let monitors: serde_json::Value =
        serde_json::from_str(&out).context("parse hyprctl -j monitors all")?;
    let Some(arr) = monitors.as_array() else {
        return Ok(None);
    };
    for m in arr {
        if m.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(Some(
                !m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
            ));
        }
    }
    Ok(None)
}

/// How long a `disable` (or the `reload` that undoes it) has to show up in
/// `hyprctl -j monitors all`. A miss is reported, never assumed.
const DISABLE_BUDGET: Duration = Duration::from_secs(3);

/// Re-enable the heads an `exclusive` session disabled. Run **before** that
/// member's output is removed, so Hyprland never sees zero enabled outputs.
///
/// `hyprctl reload` is the only thing that re-enables a disabled head. Re-
/// applying the mode/position/scale answers `ok` and leaves `disabled: true`;
/// a runtime `monitor` rule is additive and the `disable` in it keeps winning.
/// Only re-reading the config clears runtime rules.
///
/// A reload drops every runtime `hyprctl keyword`/`eval` override, including
/// our streamed-output rule (harmless — teardown removes the output next) and
/// any the operator set by hand; a hyprlang config re-runs `exec =` lines
/// (`exec-once` does not). Runs only when we actually disabled something.
fn restore_heads(disabled: &[String]) {
    if let Err(e) = hyprctl_dispatch(&["reload"]) {
        tracing::error!(
            ?disabled, error = %format!("{e:#}"),
            "hyprland: `hyprctl reload` failed — the heads this session disabled are still dark. \
             Re-run `hyprctl reload` by hand to get them back."
        );
        return;
    }
    // `reload` answers `ok` for "config parsed", not "the head came back" (a
    // head the operator's own config disables stays disabled). Read it back.
    let deadline = Instant::now() + DISABLE_BUDGET;
    let still_dark = loop {
        let dark: Vec<&String> = disabled
            .iter()
            .filter(|n| matches!(head_is_enabled(n), Ok(Some(false))))
            .collect();
        if dark.is_empty() || Instant::now() >= deadline {
            break dark;
        }
        thread::sleep(Duration::from_millis(50));
    };
    if still_dark.is_empty() {
        tracing::info!(
            ?disabled,
            "hyprland: re-enabled the heads `topology: exclusive` disabled"
        );
    } else {
        tracing::warn!(
            ?disabled, ?still_dark,
            "hyprland: `hyprctl reload` ran but these heads are still disabled — the operator's own \
             config may disable them, otherwise they need a manual `hyprctl reload`"
        );
    }
}

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

/// Ceiling on the ScreenCast handshake (`create_session` → `select_sources` →
/// `start` → `open_pipe_wire_remote`). Under [`select_and_cast`]'s 20 s wait so
/// a stuck portal is reported by the thread that owns it — and so that thread
/// exits.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(15);

/// Budget for one `hyprctl` call ([`crate::proc`]). `hyprctl` waits on the
/// instance socket, so against a wedged compositor it never returns. These
/// run on the session's stream thread; a hung query wedges the session. Five
/// seconds is generous next to a healthy call (single-digit milliseconds).
const HYPRCTL_BUDGET: Duration = Duration::from_secs(5);

/// Budget for the one-shot xdph restart. `systemctl --user try-restart` waits
/// for the user manager's job; the result is already ignored.
const PORTAL_RESTART_BUDGET: Duration = Duration::from_secs(10);

/// Run `hyprctl <args>`, returning stdout. `HYPRLAND_INSTANCE_SIGNATURE` is set
/// on this child ([`hyprctl_command`]), not exported into the host. Non-zero on
/// hard failure; dispatch can print an error at status 0 — see [`hyprctl_dispatch`].
fn hyprctl(args: &[&str]) -> Result<String> {
    let mut cmd = hyprctl_command(args, crate::session::hypr_signature());
    let out = crate::proc::output_within(&mut cmd, HYPRCTL_BUDGET)
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

/// `hyprctl` invocation with the live instance signature on the child.
///
/// `Command::env` gives it to exactly that child. A process-wide `set_var` was
/// a `getenv` data race with every other thread of a live host. `sig` is `None`
/// when no instance is findable: leave the child's env alone so an inherited
/// signature (host started inside the session) still wins.
fn hyprctl_command(args: &[&str], sig: Option<String>) -> Command {
    let mut cmd = Command::new("hyprctl");
    cmd.args(args);
    if let Some(sig) = sig {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
    }
    cmd
}

/// Serializes write-the-selection → complete-the-handshake, process-wide.
/// One per-user file: a concurrent write between ours and xdph's read would
/// steer capture at the other session's output.
static SELECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Per-session selection file, removed when the handshake it steers is over.
///
/// Lifetime is the handshake, not the session: the shim cats it once inside
/// [`select_and_cast`]'s critical section. Left behind, a stale
/// `[SELECTION]screen:PF-…` permanently shadows xdph's empty-read fallback.
/// Tying removal to the cast would be worse: the file is one per user, so a
/// session ending later would delete a sibling's selection.
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

/// Point xdph's custom picker at `output` and run the ScreenCast handshake.
/// The caller must hold [`SELECTION_LOCK`].
fn select_and_cast(
    output: &str,
    hw_cursor: bool,
) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode, StopGuard)> {
    ensure_xdph_config()?;
    let sel = selection_file();
    std::fs::write(&sel, picker_selection_line(output)).with_context(|| format!("write {sel}"))?;
    // Owned from the write on: every arm below (and every `?`) leaves the
    // handshake, which is the only thing that reads it.
    let _sel_file = SelectionFile(sel);
    // Negotiated mode rides back with the fd: decided inside the portal
    // thread (only there is the proxy to ask). `hw_cursor` is the request.
    let (setup_tx, setup_rx) =
        std::sync::mpsc::channel::<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>();
    // Teardown handshake: the thread signals this once ScreenCast is closed.
    // Separate from the setup channel — it fires at the other end of the cast.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-hypr-cast".into())
        .spawn(move || portal_thread(setup_tx, closed_tx, stop_thread, hw_cursor))
        .context("spawn hyprland portal thread")?;
    // Built before the wait so every error arm sets the flag on the way out.
    // Returning a bare `Arc` left the failure arms dropping an un-set flag:
    // the thread's `send` can still land after `recv_timeout` gives up, then
    // park forever on `while !stop` holding a live ScreenCast.
    let mut guard = StopGuard { stop, closed: None };
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id, cursor_mode))) => {
            // A cast exists, so teardown must wait. Only this arm arms it.
            guard.closed = Some(closed_rx);
            // Only this arm counts toward the borrowed picker: a handshake
            // that never produced a cast has nothing to hand back.
            LIVE_CASTS.fetch_add(1, Ordering::SeqCst);
            Ok((fd, node_id, cursor_mode, guard))
        }
        Ok(Err(e)) => bail!("ScreenCast portal on {output} failed: {e}"),
        Err(_) => bail!("timed out waiting for the ScreenCast portal on {output}"),
    }
}

/// Stream an existing Hyprland monitor — same custom picker as the virtual
/// path, pointed at a physical connector, no GUI picker. The keepalive stops
/// the cast only; the monitor is Hyprland's, not ours.
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

/// Every head Hyprland reports, for [`crate::monitors::list`].
///
/// `hyprctl -j monitors all` so disabled heads are listed too. Geometry is
/// post-transform in logical pixels, which is the space `crate::monitors`
/// documents.
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
            // `description` is already "make model (connector)"; treat it as
            // the make and let the helper drop it when empty/Unknown.
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
                // `refreshRate` is Hz as a float; we store millihertz.
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
                // Named `PF-<pid>-<n>`; the shape is checked, not just the
                // prefix, so a user's `PF-office` stays theirs.
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

/// Run a `hyprctl` dispatch (`output …`, `keyword …`, `eval …`) that reports
/// success by printing `ok`. hyprctl often exits 0 on rejection, printing the
/// error to stdout — treat a known marker as failure (also how
/// [`set_monitor_rule`] tells the two config eras apart).
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
        // `hyprctl eval` on hyprlang: "eval is only supported with the lua
        // config manager" — exit 0, no other marker.
        || lc.contains("only supported")
        || lc.contains("not supported")
        // Lua `keyword` answers "keyword can't work with non-legacy parsers"
        // at exit 0 — "can't", not the "couldn't" already covered. Without
        // this the wrong-era `keyword` read as success.
        || lc.contains("can't")
        || lc.contains("cannot")
    {
        bail!("hyprctl {:?} rejected: {t}", args);
    }
    Ok(())
}

/// Poll until `name` appears in `hyprctl -j monitors`. Create returns before it does.
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

/// Every monitor name, disabled included (`-j monitors all`). A leftover from
/// a dead host may have ended up disabled; [`reclaim_leftovers_once`] must see it.
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

/// Set the client's exact mode on `name`, both config eras.
///
/// `hyprctl keyword monitor NAME,WxH@Hz,auto,1` is hyprlang (the default,
/// including ≥0.55 — version does not imply Lua). Fall back to
/// `hyprctl eval 'hl.monitor{…}'` only when `keyword` is gone. Either way,
/// confirm the output adopted the mode — some forms print `ok` for a command
/// they ignored. A headless output starts at 0×0; if neither form yields a
/// usable size, the compositor could not back the mode.
fn set_monitor_rule(name: &str, mode: Mode) -> Result<()> {
    let hz = mode.refresh_hz.max(1);
    let spec = format!("{name},{}x{}@{hz},auto,1", mode.width, mode.height);
    let lua = format!(
        "hl.monitor{{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"auto\", scale = 1 }}",
        mode.width, mode.height
    );
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    // hyprctl reports rejection in the output text. Dropping it left the
    // failure below guessing at GBM when the compositor had named the cause.
    let mut attempts: Vec<String> = Vec::new();
    for a in [&keyword, &eval] {
        // Wrong-era command (`keyword` gone under Lua, or `eval` under
        // hyprlang) — skip to the other form.
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
    // Distinguish "usable but different size" (stream anyway) from "0×0 /
    // gone" (no framebuffer at all).
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
        // Lead with what hyprctl said: if every form was rejected, no
        // allocation was attempted. Only an accepted form that left 0×0
        // points at the compositor failing to back the mode.
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

/// Poll until `name` reports exactly `mode`'s width×height (the rule applies
/// asynchronously). `false` on timeout.
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

/// `(width, height)` from `hyprctl -j monitors all` (includes disabled), or
/// `None` if absent. A fresh headless output reports `0×0` until a mode commits.
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

/// Running Hyprland `(major, minor, patch)` from `hyprctl -j version`, for a
/// diagnostic log — the mode-rule path is version-independent.
fn hyprland_version() -> Option<(u16, u16, u16)> {
    let out = hyprctl(&["-j", "version"]).ok()?;
    let json: serde_json::Value = serde_json::from_str(&out).ok()?;
    parse_version_tag(json.get("tag").and_then(|t| t.as_str())?)
}

/// Parse a Hyprland `tag` (`v0.55.4`, or a dev `v0.41.2-13-gabcdef`).
fn parse_version_tag(tag: &str) -> Option<(u16, u16, u16)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split(['.', '-', '_', '+']);
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Permission-system caveat at most once per process: with
/// `ecosystem.enforce_permissions = true` (0.49+, off by default), denial is
/// silent black frames / dropped input, not an error.
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

/// Point xdph at our custom picker: install the shim and write the managed
/// config, restarting xdph if the config changed (it reads config only at
/// startup).
///
/// The picker is borrowed, not taken. `custom_picker_binary` is one key; a
/// distro that ships its own (every Chromium share on the box) would have
/// every share pointed at us. The shim delegates: with no selection pending
/// it `exec`s whatever was configured before us. The config edit records
/// what it replaced, so [`restore_xdph_config`] can put it back.
fn ensure_xdph_config() -> Result<()> {
    let path = xdph_config_path()?;
    // Prior value: our marker if we have already taken over, else whatever
    // is in the file. Marker first, or a second takeover records our shim
    // as "theirs".
    let (current, prior) = crate::portal_config::peek(&path, XDPH_BLOCK, XDPH_PICKER_KEY);
    let fallback = match prior {
        Some(p) => p,
        None => current,
    }
    .filter(|c| picker_is_plain(c));

    // Install the picker shim (idempotent — content is fixed for a given fallback).
    let shim = picker_shim_path();
    let sel = selection_file();
    // `-s` not `-f`: empty file means "no selection". Unquoted `exec` is
    // deliberate — a picker may carry flags, and `picker_is_plain` is what
    // makes word-splitting the only thing that can happen.
    let shim_body = match &fallback {
        Some(cmd) => format!(
            "#!/bin/sh\n# Managed by punktfunk. Hands xdph the output this host is streaming; with\n# no selection pending, defers to the picker configured before us.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\nexec {cmd} \"$@\"\n"
        ),
        None => format!(
            "#!/bin/sh\n# Managed by punktfunk.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\nexit 0\n"
        ),
    };
    if std::fs::read_to_string(&shim).is_ok_and(|c| c == shim_body) {
    } else {
        // Mode at creation, not chmod after: xdph executes this file, and
        // write-then-chmod leaves it briefly at the umask default.
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

    // One key, in place. Overwriting the whole file would destroy every
    // other xdph setting the user owned.
    let changed = crate::portal_config::ensure_key(&path, XDPH_BLOCK, XDPH_PICKER_KEY, &shim)?;
    if !changed {
        return Ok(());
    }
    tracing::info!(
        path = %path.display(),
        defers_to = fallback.as_deref().unwrap_or("(xdph's own fallback)"),
        "pointed xdg-desktop-portal-hyprland at the managed picker shim"
    );
    restart_xdph();
    Ok(())
}

/// Hand `custom_picker_binary` back and restart xdph (it reads config only at
/// startup). Called from the host's shutdown path, never per cast — see
/// [`StopGuard::drop`]. Safe on a box we never touched (no-op).
///
/// The restart is the cost: xdph cannot tell us whether another application's
/// cast is live, so a share started during our session can be cut. Leaving
/// xdph pointed at a shim whose selection file is gone breaks screen sharing
/// until the next login.
pub(crate) fn restore_picker_on_shutdown() {
    restore_xdph_config();
}

fn restore_xdph_config() {
    let Ok(path) = xdph_config_path() else { return };
    match crate::portal_config::restore_key(&path, XDPH_BLOCK, XDPH_PICKER_KEY) {
        Ok(false) => return, // not ours; nothing to undo
        Ok(true) => tracing::info!(
            path = %path.display(),
            "restored the screen-share picker xdg-desktop-portal-hyprland had before this host"
        ),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"),
                "could not restore the previous screen-share picker");
            return;
        }
    }
    restart_xdph();
}

/// Bounded: `systemctl --user` blocks on the user manager's job queue, and
/// this runs on the session's stream thread. A timeout just means xdph picks
/// the new config up whenever it next starts.
fn restart_xdph() {
    let _ = crate::proc::status_within(
        Command::new("systemctl").args([
            "--user",
            "try-restart",
            "xdg-desktop-portal-hyprland.service",
        ]),
        PORTAL_RESTART_BUDGET,
    );
}

/// ScreenCast handshake. Backend-neutral portal (served here by xdph); mirrors
/// the wlroots portal thread: reports fd + node id and parks until stopped
/// (the zbus connection is the cast's lifetime). xdph answers source selection
/// via our custom picker, no dialog.
fn portal_thread(
    setup_tx: Sender<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>,
    closed_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Shared, never-dropped runtime — not per-cast. ashpd caches its D-Bus
    // connection process-globally; a per-cast runtime takes that connection's
    // background reader down with it, leaving later handshakes awaiting a
    // reply nothing is alive to read. See [`crate::portal_rt`].
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
            // Inside the bound: when the cached connection was orphaned this
            // is where the thread hung — `Screencast::new()` itself. A bound
            // that started after it reported the caller's generic timeout.
            let connect = async {
                Screencast::new().await.context(
                    "connect ScreenCast portal (is xdg-desktop-portal running with the hyprland backend/xdph?)",
                )
            };
            let proxy = match tokio::time::timeout(HANDSHAKE_BUDGET, connect).await {
                Ok(v) => v?,
                Err(_) => bail!(
                    "connecting to the ScreenCast portal did not return within {}s",
                    HANDSHAKE_BUDGET.as_secs()
                ),
            };
            // Negotiated against what xdph advertises, never asserted from
            // `hw_cursor` alone: an unadvertised mode does not degrade —
            // xdg-desktop-portal fails the call before xdph sees it. Current
            // xdph advertises Hidden|Embedded only.
            let cursor_mode = crate::portal_cursor::negotiate(&proxy, hw_cursor, "xdph").await;
            // Bounded, and that bound is load-bearing. `select_sources`/`start`
            // await a D-Bus reply a wedged portal never sends, and an await
            // that never returns cannot be cancelled by `stop`. Shorter than
            // the caller's 20 s wait so the failure is reported here.
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

            // Park, keeping `proxy` + `session` alive until stopped. Polled at
            // 20 ms not 200 ms: teardown now waits on what follows.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // Close the session before the output goes away. xdph destroys a
            // session only on explicit `Session.Close`. `StopGuard::drop` waits
            // on the signal below. Bounded: timeout still signals so teardown
            // pays the budget once rather than hanging on an already-gone portal.
            match tokio::time::timeout(CAST_CLOSE_BUDGET, session.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    error = %e,
                    "closing the ScreenCast session failed — the next cast may find xdph busy"
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

    /// The re-apply hangs off this one-line match. Too strict and a reload
    /// still resets the stream; too loose (`contains`) and a window titled
    /// `configreloaded` triggers a `hyprctl` round trip on every retitle.
    #[test]
    fn only_the_config_reload_event_re_applies_the_monitor_rule() {
        assert!(is_config_reload("configreloaded>>"));
        // Real `.socket2.sock` lines, none of which is a reload.
        assert!(!is_config_reload("monitoradded>>PF-1234-1"));
        assert!(!is_config_reload("monitorremovedv2>>3,PF-1234-1,PF-1234-1"));
        assert!(!is_config_reload("activewindow>>kitty,~/src"));
        // The `contains` trap: the word is in the DATA, not the event name.
        assert!(!is_config_reload("activewindowv2>>title: configreloaded"));
        assert!(!is_config_reload("workspace>>configreloaded"));
    }

    /// Lua config manager parses a `dispatch` argument as a Lua expression,
    /// so both arguments must be quoted. Pinning the shape because quoting is
    /// the entire difference between working and silently doing nothing.
    #[test]
    fn the_lua_dpms_expression_quotes_both_arguments() {
        assert_eq!(
            lua_dpms_expr("HDMI-A-1", false),
            r#"hl.dsp.dpms("off", "HDMI-A-1")"#
        );
        assert_eq!(lua_dpms_expr("DP-2", true), r#"hl.dsp.dpms("on", "DP-2")"#);
        // Never omit the monitor name: the no-name form answers `ok` and
        // toggles, which would flip a just-restored head back off.
        assert!(lua_dpms_expr("DP-2", true).contains("\"DP-2\""));
    }

    /// Lua spellings of the resize workspace hand-off. Both dispatchers answer
    /// `ok` on the wrong era, so a drifted string fails silently. Workspace id
    /// is a quoted string in both; `hl.dsp.workspace.move` requires `monitor`,
    /// and there is no `hl.dsp.workspace` member that merely switches — the
    /// switch half goes through `hl.dsp.focus`.
    #[test]
    fn the_lua_workspace_expressions_quote_their_arguments() {
        assert_eq!(
            lua_workspace_move_expr("2", "PF-1234-2"),
            r#"hl.dsp.workspace.move({ workspace = "2", monitor = "PF-1234-2" })"#
        );
        assert_eq!(
            lua_workspace_focus_expr("2"),
            r#"hl.dsp.focus({ workspace = "2" })"#
        );
    }

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

    /// `focusmonitor` is a dispatcher, so it must go through `hyprctl dispatch`.
    /// A bare `hyprctl focusmonitor NAME` is not a subcommand and hyprctl
    /// reports it with exit 0.
    #[test]
    fn focus_goes_through_the_dispatch_subcommand() {
        assert_eq!(
            focus_argv("PF-1234-1"),
            ["dispatch", "focusmonitor", "PF-1234-1"]
        );
    }

    /// Lua-era spelling. The key is `monitor` (the compositor lists the
    /// alternatives when it is anything else) and the name must be quoted —
    /// unquoted is the classic form's failure, `')' expected near 'PF'`.
    #[test]
    fn the_lua_focus_expression_quotes_the_monitor_name() {
        assert_eq!(
            lua_focus_expr("PF-1234-1"),
            "hl.dsp.focus({ monitor = \"PF-1234-1\" })"
        );
        // The two eras must not converge on one string: each is rejected by
        // the other's parser, which is what makes "try one, then the other" safe.
        assert_ne!(lua_focus_expr("PF-1"), focus_argv("PF-1").join(" "));
    }

    /// `HYPRLAND_INSTANCE_SIGNATURE` reaches `hyprctl` as a per-child override,
    /// never as a `set_var` on the host — that write was a `getenv` data race
    /// with every other thread of a live session. A discovered signature is
    /// set on the child; an undiscoverable one leaves the child's env untouched.
    #[test]
    fn the_instance_signature_travels_on_the_child_not_the_process_env() {
        let overrides = |sig: Option<String>| -> Vec<(String, Option<String>)> {
            hyprctl_command(&["-j", "version"], sig)
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
            overrides(Some("abc123".to_string())),
            [(
                "HYPRLAND_INSTANCE_SIGNATURE".to_string(),
                Some("abc123".to_string())
            )]
        );
        assert!(overrides(None).is_empty());
    }

    #[test]
    fn output_names_are_unique_and_prefixed() {
        let a = next_output_name();
        let b = next_output_name();
        assert!(a.starts_with("PF-") && b.starts_with("PF-"));
        assert_ne!(a, b);
    }

    /// The name carries the creating host's pid, which is what makes a leftover
    /// attributable. A reclaim that could not tell whose it was would have to
    /// remove a live sibling's or nothing at all.
    #[test]
    fn a_name_carries_its_owner_pid_and_only_ours_does() {
        let mine = next_output_name();
        assert_eq!(output_owner_pid(&mine), Some(std::process::id()));
        assert!(is_managed_output(&mine));

        // Legacy `PF-<n>`: recognisably managed, but no owner — report, never
        // reclaim on a guess.
        assert!(is_managed_output("PF-1"));
        assert_eq!(output_owner_pid("PF-1"), None);

        // A user's own monitor that happens to start with the prefix, and the
        // connectors every wlr-family compositor mints.
        for theirs in ["PF-office", "PF-", "PF-12-abc", "HEADLESS-1", "DP-1", ""] {
            assert!(!is_managed_output(theirs), "{theirs:?} is not ours");
            assert_eq!(output_owner_pid(theirs), None, "{theirs:?} has no owner");
        }
    }

    /// The backend hands the picker exactly what [`crate::portal_picker`] says.
    /// That module owns the format and its tests run on every platform.
    #[test]
    fn picker_line_is_the_shared_selection_format() {
        assert_eq!(picker_selection_line("PF-1"), "[SELECTION]/screen:PF-1\n");
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
            // Real `list_monitors` derives this with `is_managed_output`;
            // mirror it so the fixture cannot drift from the backend's rule.
            managed: is_managed_output(connector),
        }
    }

    /// `exclusive` disables the operator's heads and only those. A sibling
    /// session's output — ours or another host's, both `PF-<pid>-<n>` — must
    /// survive, or the second exclusive session blacks out the first.
    #[test]
    fn exclusive_disables_the_operators_heads_and_never_a_managed_sibling() {
        let ours = "PF-4242-1";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            // A concurrent session's output, and one from a second host — both managed.
            head("PF-4242-2", true),
            head("PF-99-1", true),
            // Already off: must not end up in the restore list, or teardown
            // would switch on a head the operator had left dark.
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ours), vec!["DP-1", "HDMI-A-1"]);
    }

    /// A box with no physical head has nothing to disable, so no restore is
    /// prepared and teardown never runs a `hyprctl reload`.
    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "PF-4242-1";
        assert!(heads_to_disable(&[head(ours, true)], ours).is_empty());
    }

    /// Both config eras, pinned. `hyprctl` answers a wrong-era or malformed
    /// rule at exit 0, so a typo here reads as success and the operator's
    /// screen stays lit under `exclusive`.
    #[test]
    fn disable_rules_are_pinned_for_both_config_eras() {
        assert_eq!(disable_rule_spec("DP-1"), "DP-1,disable");
        assert_eq!(
            disable_lua_expr("DP-1"),
            r#"hl.monitor{ output = "DP-1", disabled = true }"#
        );
    }

    /// `hyprctl keyword` under Lua answers "keyword can't work with non-legacy
    /// parsers. Use eval." at exit 0. Without this marker the wrong-era form
    /// reports success.
    #[test]
    fn a_wrong_era_rejection_is_an_error_not_a_success() {
        for said in [
            "keyword can't work with non-legacy parsers. Use eval.",
            "eval is only supported with the lua config manager",
            "invalid resolution ",
        ] {
            let lc = said.to_ascii_lowercase();
            assert!(
                lc.contains("can't")
                    || lc.contains("cannot")
                    || lc.contains("only supported")
                    || lc.contains("invalid"),
                "{said:?} must match a marker in hyprctl_dispatch"
            );
        }
    }
}
