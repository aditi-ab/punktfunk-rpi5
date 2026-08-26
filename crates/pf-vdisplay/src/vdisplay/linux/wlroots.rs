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
//! Requirements: the host can reach the sway session — `SWAYSOCK` for swaymsg, inherited or
//! discovered and set on each child ([`swaymsg_command`]), plus the portal activation env
//! (`WAYLAND_DISPLAY`/`XDG_CURRENT_DESKTOP=sway` imported into `systemctl --user`, see
//! `scripts/headless/prepare-session.sh`), with the ScreenCast interface routed to xdpw
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
    /// What the portal actually gave us on the most recent [`create`](VirtualDisplay::create) — see
    /// [`VirtualDisplay::last_portal_cursor_mode`], which is how the host learns that a cursor
    /// overlay is never coming instead of inferring it from an absence.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// The topology-restore action the last `create` prepared (re-enable the heads an `exclusive`
    /// topology disabled), pending pickup by the registry via [`take_topology_restore`] — so the
    /// operator's screens come back when the display GROUP's last member drops (design §6.1), not
    /// when this one session ends. A backstop [`Drop`] runs it if the registry never took it, so a
    /// physical head is never left dark. Mirrors `kwin.rs` and the Hyprland twin.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for WlrootsDisplay {
    fn drop(&mut self) {
        // Backstop only: the registry takes the restore right after `create` (moving it into the
        // group), so this is normally `None`. If some path skipped the take, re-enable here rather
        // than strand the operator's heads dark.
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

    /// Apply the effective [`crate::policy::Topology`] for the just-created output `ours`, and stash
    /// the restore for the registry (see [`Self::pending_restore`]).
    ///
    /// Called at the very END of [`create`](VirtualDisplay::create), on purpose: nothing can fail
    /// after it, so there is no path that disables the operator's heads and then unwinds past the
    /// point where the restore is handed over. The cost is that the physical heads stay lit for the
    /// duration of the portal handshake, which is the pre-existing `extend` behaviour anyway.
    fn apply_topology(&mut self, ours: &str) {
        use crate::policy::Topology;
        match crate::effective_topology() {
            // Nothing to do — the headless output joins the desk as one more head, which is what
            // `create` has already built.
            Topology::Extend | Topology::Auto => {}
            Topology::Primary => warn_primary_is_not_expressible(),
            Topology::Exclusive => {
                let disabled = disable_other_heads(ours);
                self.pending_restore = (!disabled.is_empty()).then(|| {
                    Box::new(move || restore_heads(&disabled)) as Box<dyn FnOnce() + Send>
                });
            }
        }
    }
}

/// wlroots/Sway is usable when the host runs inside a Sway session — signalled by an INHERITED
/// `SWAYSOCK` (the IPC socket `swaymsg create_output` needs). Cheap env check for the enumeration
/// path.
///
/// Inherited is now all it can be: `apply_session_env` no longer exports this key (the value goes
/// to the `swaymsg` children instead — see [`swaymsg_command`]), so this can only ever report what
/// the host was launched with, never what we ourselves wrote. That is the honest half: a
/// `systemd --user` host inherits nothing, and [`crate::available`] covers it by asking the `/proc`
/// scan whether a wlroots session is live BEFORE it consults this probe.
///
/// Still under [`crate::with_env_lock`]: it orders the read against this crate's remaining env
/// writers (`apply_session_env`'s four survivors), which is all that lock has ever been able to do.
/// No caller holds it — the mutex is not reentrant.
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

        // Put the compositor's focus on the head we are about to stream, so the windows this
        // session opens land where the client can see them.
        focus_output(&name);

        // Steer xdpw's headless output chooser at our new output, then run the portal handshake on
        // its own thread (it parks to keep the cast alive, like the other backends). Serialized:
        // the chooser is one per-user file, so a concurrent session's write between ours and xdpw's
        // read would silently capture the wrong output (see `SELECTION_LOCK`).
        let (fd, node_id, cursor_mode, stop) = {
            let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            select_and_cast(&name, self.hw_cursor)?
        };
        // Latched for `last_portal_cursor_mode`: xdpw refuses metadata by construction, so this is
        // `embedded` whatever we asked for, and the session's whole cursor behaviour follows from
        // that fact rather than from `hw_cursor`.
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
        // Display-management topology (design §5.2). Last, so no failure path unwinds past the
        // hand-off of the restore — see [`WlrootsDisplay::apply_topology`].
        self.apply_topology(&name);
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

/// Could `name` be a headless output a punktfunk host created? sway names these itself, so unlike
/// Hyprland's `PF-<pid>-<n>` there is nothing in the name to attribute — the prefix is the whole
/// answer, and a headless output the operator made by hand is indistinguishable. That is why the two
/// callers are both narrow: [`unplug_strays`] additionally requires the output to have appeared
/// during our own `create_output`, and [`super::super::focus_streamed_output`] only ever passes the
/// name of the head this session is streaming.
pub(crate) fn is_managed_output(name: &str) -> bool {
    name.starts_with("HEADLESS-")
}

/// Unplug any headless output that appeared since `before` and that nothing owns — the cleanup for a
/// `create_output` whose output we could not identify in time. Only `HEADLESS-*` is touched: a
/// physical hotplug in the same window is the operator's, not ours, and `unplug` on a real connector
/// would take their screen away. Best-effort by construction, and it runs with [`CREATE_LOCK`] held
/// so nothing else in this process can have created the strays it sees.
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

/// Point sway's focus at the head we are about to stream, so the windows this session opens land
/// where the client can see them.
///
/// sway opens a new window on the focused workspace, and `create_output` does not focus what it
/// creates — focus stays on whatever head already had it, which on a box with a physical monitor is
/// that monitor. Nothing else in the session moves it (the client's pointer is confined to the
/// streamed output), so without this every app the host launches for the session opens where the
/// client cannot see it. The Hyprland twin of this is `hyprland::focus_output`; both are the
/// EXTEND-topology answer to window placement, and neither touches the operator's heads.
///
/// Best-effort: a failure costs window placement, not the session.
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

/// The `swaymsg` argv that focuses `name`, split out so a test pins its SHAPE.
///
/// sway's command is `focus output <name>` — the noun comes SECOND, unlike every other call in this
/// file (`output <name> mode|enable|unplug`), where it comes first. Transposing it yields
/// `output focus <name>`, which sway rejects, and the field symptom is the very bug this fixes.
///
/// ⚠ Unlike the Hyprland twin, this shape is **from sway's documented command surface, not yet
/// exercised on a live sway** (no box in the fleet runs one — the 2026-08-17 probe had Hyprland
/// only). It is the safer of the two to get wrong: [`swaymsg`] passes these through `--` as a sway
/// *command* and rejects a non-zero exit, and sway exits non-zero on an invalid command (the
/// `Unknown/invalid command` path [`swaymsg_query`] documents), so a bad shape surfaces as the
/// logged warning rather than as a silent success the way `hyprctl`'s exit-0 rejection would.
fn focus_argv(name: &str) -> [&str; 3] {
    ["focus", "output", name]
}

/// `topology: primary` has no expression on this compositor, and saying so once per create is the
/// honest implementation — design §5.2 spells this row out: "**unsupported** (no primary concept)
/// → log + treat as extend".
///
/// Wayland has no primary-output concept, and sway's nearest equivalent is the *focused* output —
/// which [`focus_output`] already points at the streamed head for every session, whatever the
/// topology says. So `primary` is not silently dropped so much as already granted, as far as this
/// compositor can express it; what an operator does NOT get is a persistent designation other
/// clients can read. Distinct from the `exclusive` path, which really does change the desk.
fn warn_primary_is_not_expressible() {
    tracing::info!(
        "wlroots: `topology: primary` has no equivalent here — Wayland has no primary output and \
         sway has only a FOCUSED output, which the streamed head already holds. Treating it as \
         `extend`; use `exclusive` to actually disable the operator's heads."
    );
}

/// Which heads an `exclusive` topology should disable: enabled, not ours, and **not managed**.
///
/// Pure so the group-awareness rule (design §6.1 — "exclusive means the *managed virtual displays*
/// are the only enabled outputs; never disable a sibling slot") is unit-testable without a
/// compositor. `managed` comes from [`list_monitors`], i.e. the `HEADLESS-` prefix, so a concurrent
/// session's output is never blacked out by ours.
///
/// ⚠️ That prefix is [deliberately blunt](is_managed_output): sway names its OWN headless outputs
/// the same way we do, so a sway started on the headless backend has a `HEADLESS-1` of its own that
/// this filter also spares. The failure that buys is the harmless one — a bootstrap head stays lit
/// on a box that has no physical screen anyway — whereas the alternative is disabling a live
/// sibling's output. `ours` is excluded by name too, belt and braces.
fn heads_to_disable(heads: &[crate::monitors::PhysicalMonitor], ours: &str) -> Vec<String> {
    heads
        .iter()
        .filter(|h| h.enabled && !h.managed && h.connector != ours)
        .map(|h| h.connector.clone())
        .collect()
}

/// Disable every non-managed head for an `exclusive` session, returning the ones actually disabled
/// (the input to [`restore_heads`]). Best-effort per head: one that refuses costs exclusivity on
/// that screen, not the session.
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
        // Disabling outputs moves their workspaces, and sway picks the replacement focus itself.
        // Re-assert ours so window placement still lands on the stream (the #283 contract).
        focus_output(ours);
    }
    disabled
}

/// Disable one head: `swaymsg output <name> disable`, confirmed by read-back.
///
/// The read-back is not ceremony. `swaymsg` does report a rejected command with a non-zero exit
/// (unlike `hyprctl`, which answers at exit 0 — see the Hyprland twin), so a bad *command* is
/// caught by [`swaymsg`] itself; what the read-back adds is proof the output actually went
/// inactive, which is the state teardown will have to undo.
fn disable_head(name: &str) -> Result<()> {
    swaymsg(&disable_argv(name)).with_context(|| format!("swaymsg output {name} disable"))?;
    if wait_head_enabled_is(name, false, DISABLE_BUDGET) {
        return Ok(());
    }
    bail!("swaymsg accepted `output {name} disable` but the output never went inactive")
}

/// The `swaymsg` argv that disables `name`, split out so a test pins its SHAPE — the noun comes
/// FIRST here (`output <name> disable`), the opposite of [`focus_argv`]'s `focus output <name>`.
fn disable_argv(name: &str) -> [&str; 3] {
    ["output", name, "disable"]
}

/// The `swaymsg` argv that DPMS-es `name` off or on. Same noun-first shape as [`disable_argv`],
/// and a different axis from it: `dpms off` leaves the output enabled and configured (its
/// workspaces do not move, no window is re-homed) and merely stops driving the panel.
fn dpms_argv(name: &str, on: bool) -> [&str; 4] {
    ["output", name, "dpms", if on { "on" } else { "off" }]
}

/// DPMS every head that is not ours and not a sibling's off (or back on), for a **gamescope**
/// session honoring `Topology::Exclusive` — see [`crate::panel_dpms`].
///
/// Distinct from [`disable_other_heads`], which is what the *wlroots backend's own* exclusive
/// topology does. A gamescope spawn is its own compositor and owns no sway output, so there is
/// nothing here to promote to "the desk" and nothing to focus — and disabling the operator's
/// outputs would move their workspaces around for a stream that is not even on this compositor.
/// DPMS is the honest translation: the desk stays exactly as it is, the panels just go dark.
///
/// Reuses [`heads_to_disable`]'s filter with an empty `ours`, so a concurrent wlroots session's
/// `HEADLESS-*` output is spared for the same reason it is there — blanking it would black out
/// that client's stream.
///
/// Returns the heads actually changed, so the re-light can undo exactly those. Best-effort per
/// head, like its neighbour: one that refuses costs a lit screen, not the stream.
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

/// The `swaymsg` argv that re-enables `name`. sway keeps a disabled output's configuration, so a
/// bare `enable` restores the mode/position/scale it had — there is no need to replay the rule the
/// way the Hyprland twin's `reload` does.
fn enable_argv(name: &str) -> [&str; 3] {
    ["output", name, "enable"]
}

/// How long a `disable`/`enable` has to show up in `swaymsg -t get_outputs`. Generous next to a
/// healthy IPC round trip; a miss is reported, never assumed.
const DISABLE_BUDGET: Duration = Duration::from_secs(3);

/// Poll until `name`'s enabled state equals `want`, up to `timeout`. `false` on timeout.
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

/// Is output `name` currently enabled (sway's `active`)? `None` if it is not present at all.
/// A disabled output is still listed by `get_outputs`, with `"active": false` — which is what makes
/// this a usable read-back rather than a presence check.
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

/// Re-enable the outputs an `exclusive` session disabled. Run by the REGISTRY when the display
/// group's last member is torn down (design §6.1) and, critically, **before** that member's output
/// is unplugged — so sway never sees zero enabled outputs.
///
/// ⚠️ **Not exercised on a live sway.** No box in the fleet runs one (the 2026-08-18 probes had
/// Hyprland only), which is the same gap PR #283's `focus output` half shipped with and which
/// `design/display-management.md` records as "wlroots `exclusive` (needs a Sway box)". The argv is
/// sway's documented command surface and is pinned by [`enable_argv`]'s test; the read-back below
/// turns a wrong guess into a logged warning naming the outputs, rather than a screen that silently
/// stays dark. Unlike Hyprland — where re-applying a rule provably does NOT undo a disable and only
/// `hyprctl reload` does — sway's `enable` is the documented inverse of `disable`.
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

/// A bare `swaymsg`, with the live sway IPC socket threaded onto the child.
///
/// `SWAYSOCK` used to ride the process env: `apply_session_env` `set_var`'d it per connect (and
/// `remove_var`'d it when nothing sway-shaped was live) so these children inherited it. Nothing
/// outside pf-vdisplay ever read it, so that bought a per-connect `setenv` — a data race with any
/// `getenv` on any other thread of a live streaming host — for a value two children need.
/// `Command::env` gives it to exactly those children, the way `set_launch_command` carries the
/// launch. `sock` is `None` when there is no sway IPC we can find: leave the child's env alone
/// then, so an inherited one (host started inside the session) still wins.
fn swaymsg_command(sock: Option<String>) -> Command {
    let mut cmd = Command::new("swaymsg");
    if let Some(sock) = sock {
        cmd.env("SWAYSOCK", sock);
    }
    cmd
}

/// Run `swaymsg -- <args>`, returning stdout (`--` so command tokens like `--custom` reach
/// sway instead of swaymsg's own getopt). swaymsg exits non-zero (with the error on stderr/
/// stdout) when the command fails, so checking the status covers `{"success": false}` too.
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

/// Run a swaymsg **query** (`-t <kind> --raw`) and parse its JSON.
///
/// ⚠️ Deliberately NOT [`swaymsg`]: that helper inserts `--` so its arguments are read as a sway
/// *command*, which is right for `create_output` and wrong for a query — `-t` after `--` comes back
/// as `Unknown/invalid command '-t'` (caught on-glass writing the monitor enumeration).
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
fn select_and_cast(
    output: &str,
    hw_cursor: bool,
) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode, StopGuard)> {
    ensure_xdpw_config()?;
    let chooser = chooser_file();
    std::fs::write(&chooser, format!("Monitor: {output}\n"))
        .with_context(|| format!("write {chooser}"))?;
    // Owned from the write on: every arm below (and every `?`) leaves the handshake, which is the
    // only thing that reads it.
    let _chooser = ChooserFile(chooser);
    // The NEGOTIATED cursor mode rides back with the fd and node id: it is decided inside the
    // portal thread (only there is the proxy to ask), and nothing downstream can re-derive it —
    // `hw_cursor` is the request, not the answer.
    let (setup_tx, setup_rx) =
        std::sync::mpsc::channel::<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>();
    // The teardown handshake: the thread signals this once it has closed the ScreenCast session, and
    // `StopGuard::drop` waits on it before the output is unplugged (see `StopGuard`). Kept a
    // SEPARATE channel from the setup one above — it fires at the other end of the cast's life,
    // long after `setup_rx` has been consumed.
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
        Ok(Ok((fd, node_id, cursor_mode))) => {
            // A cast exists now, so teardown has something to close and must wait for it.
            guard.closed = Some(closed_rx);
            Ok((fd, node_id, cursor_mode, guard))
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
    let (fd, node_id, cursor_mode, stop) = select_and_cast(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: Some(fd),
        cursor_mode: Some(cursor_mode),
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
    setup_tx: Sender<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// sway spells this one `focus output <name>` — noun second, unlike the `output <name> …` shape
    /// every other call in this file uses. A transposed `output focus <name>` is rejected, and the
    /// only symptom would be the bug the call exists to fix: apps opening on the operator's monitor
    /// while the stream shows a bare desktop.
    #[test]
    fn focus_names_the_output_after_the_verb() {
        assert_eq!(focus_argv("HEADLESS-2"), ["focus", "output", "HEADLESS-2"]);
    }

    /// The topology pair takes the OTHER shape — `output <name> <verb>`, noun first, like `mode` /
    /// `unplug` and unlike [`focus_argv`]. Both are pinned because this file legitimately uses both
    /// orders, which is exactly the condition under which one gets written the wrong way round.
    #[test]
    fn disable_and_enable_name_the_output_before_the_verb() {
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
        assert_eq!(enable_argv("DP-1"), ["output", "DP-1", "enable"]);
    }

    /// `SWAYSOCK` reaches `swaymsg` as a per-CHILD override, never as a `set_var` on the host's own
    /// environment — that write was a `getenv` data race with every other thread of a live session
    /// (security-review 2026-08-25). Pinning both arms: a known socket is set on the child, and an
    /// unknown one leaves the child's env untouched so an inherited `SWAYSOCK` still reaches sway.
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

    /// The group-awareness rule (design §6.1): `exclusive` disables the operator's outputs and
    /// **only** those. A sibling session's `HEADLESS-N` must survive, or the second exclusive
    /// session blacks out the first one's screen — the exact bug KWin's Stage 3 shipped.
    #[test]
    fn exclusive_disables_the_operators_outputs_and_never_a_headless_sibling() {
        let ours = "HEADLESS-2";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            // A concurrent session's output — and, indistinguishably, a headless sway's own
            // bootstrap output. Both are spared; see `heads_to_disable`.
            head("HEADLESS-1", true),
            // Already off: nothing to disable, and it must NOT end up in the restore list, or
            // teardown would switch on an output the operator had deliberately left dark.
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ours), vec!["DP-1", "HDMI-A-1"]);
    }

    /// `dpms` is a different sway verb from `disable`, and the difference is the whole point of
    /// the gamescope arm: `disable` moves workspaces and re-homes windows on the operator's desk,
    /// `dpms off` leaves the desk alone and only stops driving the panel. Four tokens, not three —
    /// sway spells it `output <name> dpms on|off`.
    #[test]
    fn dpms_is_a_separate_verb_from_disable() {
        assert_eq!(dpms_argv("DP-1", false), ["output", "DP-1", "dpms", "off"]);
        assert_eq!(dpms_argv("DP-1", true), ["output", "DP-1", "dpms", "on"]);
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
    }

    /// The gamescope DPMS arm reuses the disable filter with an EMPTY `ours`: a gamescope spawn
    /// owns no sway output, so nothing of ours needs sparing — but a concurrent wlroots session's
    /// `HEADLESS-*` still must be, or darkening would black out that client's stream.
    #[test]
    fn the_gamescope_dpms_arm_still_spares_a_sibling_headless() {
        let heads = [
            head("DP-1", true),
            head("HEADLESS-1", true),
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ""), vec!["DP-1"]);
    }

    /// A box with no physical output (the CI/headless posture) has nothing to disable, so no
    /// restore is prepared and teardown touches nothing.
    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "HEADLESS-1";
        assert!(heads_to_disable(&[head(ours, true)], ours).is_empty());
    }
}
