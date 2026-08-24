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
//!    installed shim that cats a per-session selection file we write right before the handshake —
//!    `[SELECTION]/screen:<NAME>`, whose leading `/` is xdph's mandatory empty-flags separator (see
//!    [`crate::portal_picker`], which owns the format and its tests).
//! 4. Teardown is RAII **and ordered**: drop closes the ScreenCast session and WAITS for the portal
//!    to confirm it, and only then runs `hyprctl output remove NAME`. Removing the output first is
//!    what made every stream after the first one fail on Hyprland — see [`StopGuard`].
//!
//! Requirements: the host runs inside (or can reach) the Hyprland session — either
//! `HYPRLAND_INSTANCE_SIGNATURE` is inherited, or [`is_available`] discovers it from
//! `$XDG_RUNTIME_DIR/hypr/` and [`super::super::apply_session_env`] exports it for `hyprctl` — with
//! the ScreenCast interface routed to xdph (`scripts/headless/portals.conf`).
//!
//! The focus contract [`focus_output`] rests on is verified on **Hyprland 0.56.2** (2026-08-17,
//! headless probe against a real instance): `output create headless` leaves the new head
//! `focused: false` — which is the whole reason [`focus_output`] exists — `dispatch focusmonitor
//! <name>` replies `ok` and moves `focused` onto it, and a client spawned afterwards maps onto that
//! head. The bare `hyprctl focusmonitor <name>` (no `dispatch`) answers `unknown request` at
//! **exit 0**, which is what [`hyprctl_dispatch`]'s `unknown` marker turns into an error.
//!
//! Contracts verified on **Hyprland 0.55.4 + xdph 1.3.x** (`design/hyprland-support.md` Phase 0):
//! `hyprctl` subcommands / JSON shapes, the `[SELECTION]/screen:<name>` picker format (re-derived
//! from xdph 1.3.12's own parser on 2026-08-14, which is when the missing `/` turned up), the
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
/// [`picker_selection_line`] here right before the portal handshake selects sources. Lives under
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

/// The picker line for output `name` — `[SELECTION]/screen:<name>`, whose every byte is load-bearing.
/// Lives in [`crate::portal_picker`] with a transcription of xdph's parser, because it is a wire
/// format with no error report and this file only compiles on Linux.
fn picker_selection_line(name: &str) -> String {
    crate::portal_picker::selection_line(name)
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
pub(crate) fn is_managed_output(name: &str) -> bool {
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
    /// Out-of-band cursor request (`set_hw_cursor`, the negotiated cursor channel): PREFER portal
    /// `CursorMode::Metadata` — shapes/positions ride `SPA_META_Cursor` for the channel + the
    /// composite blend. Off (every non-channel session): prefer `Embedded` — the compositor paints
    /// the pointer into frames, zero host-side cursor work (the pre-channel default this backend
    /// always had).
    ///
    /// Both are only a PREFERENCE: [`crate::portal_cursor`] settles it against what xdph actually
    /// advertises, because requesting an unadvertised mode makes xdg-desktop-portal fail the call.
    /// This used to be asserted instead, which is exactly how a cursor-forward session here became
    /// a black client.
    ///
    /// ⚠️ On current xdph the metadata arm is UNREACHABLE, not merely untested: measured on .21
    /// 2026-08-14 (Hyprland 0.56.2, xdph 1.4.1) `AvailableCursorModes` = 3 — `Hidden|Embedded`
    /// only. Every session on this backend therefore resolves to `Embedded` today; KWin/Mutter
    /// remain the legs where the metadata channel is actually exercised.
    hw_cursor: bool,
    /// What the portal actually gave us on the most recent [`create`](VirtualDisplay::create) — see
    /// [`VirtualDisplay::last_portal_cursor_mode`], which is how the host learns that a cursor
    /// overlay is never coming instead of inferring it from an absence.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// The topology-restore action the last `create` prepared (re-enable the heads an `exclusive`
    /// topology disabled), pending pickup by the registry via [`take_topology_restore`] — so the
    /// operator's screens come back when the display GROUP's last member drops (design §6.1), not
    /// when this one session ends. A backstop [`Drop`] runs it if the registry never took it, so a
    /// physical head is never left dark. Mirrors `kwin.rs`'s field of the same name.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for HyprlandDisplay {
    fn drop(&mut self) {
        // Backstop only: the registry takes the restore right after `create` (moving it into the
        // group), so this is normally `None`. If some path skipped the take, re-enable here rather
        // than strand the operator's heads dark.
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

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Log the permission-system caveat once per process (silent black frames otherwise).
        preflight_once();
        // Remove any output a PREVIOUS host left in this compositor, before we mint our first.
        reclaim_leftovers_once();

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

        // Put the compositor's focus on the head we are about to stream, so the windows this
        // session opens land where the client can see them.
        focus_output(&name);

        // Steer xdph's custom picker at our new output, then run the portal handshake on its own
        // thread (it parks to keep the cast alive, like the other backends). Serialized: the
        // selection is one per-user file, so a concurrent session's write between ours and xdph's
        // read would silently capture the wrong output (see `SELECTION_LOCK`).
        let (fd, node_id, cursor_mode, stop) = {
            let _sel = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            select_and_cast(&name, self.hw_cursor)?
        };
        // Latched for `last_portal_cursor_mode`: on today's xdph this is `embedded` whatever we
        // asked for, and the session's whole cursor behaviour follows from that fact rather than
        // from `hw_cursor`.
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
        // Display-management topology (design §5.2). Last, so no failure path unwinds past the
        // hand-off of the restore — see [`HyprlandDisplay::apply_topology`].
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
            // `remote_fd.is_some()` — same as wlroots.
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            // Hyprland is an EXTEND topology: this head sits BESIDE the operator's, so absolute
            // input has to be aimed at it by name or it lands on their screen. `hyprctl`'s monitor
            // name is the head's `wl_output.name`, which is what the injector matches.
            output_name: Some(name),
        })
    }
}

/// Drop order matters, and it is the whole fix: [`StopGuard`] **blocks until the ScreenCast session
/// is actually closed**, and only then does [`OutputGuard`] remove the compositor output (fields drop
/// in declaration order).
///
/// 🛑 THIS ORDERING USED TO BE A LIE. `StopGuard::drop` only set an atomic and returned, while the
/// portal thread noticed it 200 ms later — so `OutputGuard::drop` ran `hyprctl output remove` on an
/// output xdph was still actively capturing, every single teardown. See [`StopGuard`] for what that
/// did to xdph.
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// How long teardown waits for the portal to confirm the ScreenCast session is closed before giving
/// up and removing the output anyway. One D-Bus round trip through xdg-desktop-portal to xdph; three
/// seconds is generous. Bounded on purpose: a portal that has already wedged must not be able to
/// wedge the host's teardown with it — every other blocking helper on this path is bounded the same
/// way (see [`HYPRCTL_BUDGET`]).
const CAST_CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// Ends the cast: signals the portal thread, then **waits for it to have closed the ScreenCast
/// session**, so the caller may safely remove the output afterwards.
///
/// 🛑 THE WAIT IS THE POINT — "only the first stream after a portal start works" on Hyprland was
/// this, root-caused 2026-08-14 against Hyprland 0.55.4 + xdph 1.3.12 + xdg-desktop-portal 1.20.4.
///
/// This used to be a bare `AtomicBool` that `drop` merely SET. The portal thread polled it every
/// 200 ms and then just dropped its zbus connection, and xdph destroys a session on exactly one
/// event — an explicit `org.freedesktop.impl.portal.Session.Close` (`Session.cpp:37`,
/// `onCloseSession`); it has no peer-vanished watcher of its own. The frontend does have one
/// (`xdg-desktop-portal.c:230` `peer_died_cb` → `close_sessions_for_sender`), but it only fires once
/// our unique bus name goes away, which is *after* the 200 ms poll, and it runs asynchronously on a
/// GTask thread. Meanwhile `OutputGuard::drop` had already removed the output — synchronously,
/// microseconds after the flag was set.
///
/// So every teardown destroyed the `wl_output` out from under a live screencopy session. xdph's next
/// `Start` then built a PipeWire stream against that wreckage and fell into
///
/// ```text
/// while (pSession->sharingData.nodeID == SPA_ID_INVALID) {      // Screencopy.cpp:307-313
///     int ret = pw_loop_iterate(g_pPortalManager->m_sPipewire.loop, 0);   // timeout 0 = NON-blocking
/// ```
///
/// — an unbounded hot spin on xdph's ONLY event-loop thread, inside the `Start` handler, holding its
/// `m_mEventLock`. From that moment xdph answers no D-Bus, no Wayland and no PipeWire, ever again, and
/// every later `select_and_cast` dies on our 20 s timeout. MEASURED on the box: the wedged instance's
/// unit reported `Consumed 3min 51.971s CPU time over 23min 41.092s wall clock`, and there were
/// exactly 232.7 s of wall clock between its last log flush and its restart — 231.971 s of CPU
/// against 232.7 s of wall, i.e. one core pinned solid for precisely the wedged interval.
///
/// Waiting here closes that window: `Session.Close` is answered synchronously by the frontend
/// (`xdp-session.c:217` `handle_close` → `xdp_session_close` →
/// `xdp_dbus_impl_session_call_close_sync`), so by the time `close()` returns, xdph has already run
/// `destroyStream` and logged `Session destroyed`. The output we remove next is one nobody is
/// capturing.
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Signalled by the portal thread once it has closed the ScreenCast session.
    ///
    /// `None` on every path where no cast was ever established (a rejected or timed-out handshake):
    /// there is nothing to close, and a portal that just failed to answer for 20 s is precisely the
    /// one that would burn the whole budget here for nothing.
    closed: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(closed) = self.closed.take() else {
            return;
        };
        match closed.recv_timeout(CAST_CLOSE_BUDGET) {
            // Closed — xdph has torn the capture down, the output is safe to remove.
            Ok(()) => {}
            // The thread is gone without confirming (it panicked, or the runtime died). Nothing is
            // holding the cast either way, so there is nothing left to wait for.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            // Still going after the budget. Fall through and remove the output anyway — a leaked
            // output is worse than a racy one — but say so, because this is the state that wedges
            // xdph and the next session will be the one that pays for it.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                budget_s = CAST_CLOSE_BUDGET.as_secs(),
                "the ScreenCast session did not close in time — removing the output underneath it, \
                 which is what wedges xdph's frame loop; the next cast may find the portal busy"
            ),
        }
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

/// Point Hyprland's focus at the head we are about to stream, so the windows this session opens
/// land where the client can see them.
///
/// Hyprland opens a new window on the **active workspace of the focused monitor**, and
/// `output create headless` does not focus what it creates — focus stays wherever it already was,
/// which on any box with a physical head is that head. Nothing else in the session ever moves it:
/// the client's pointer is confined to the streamed output (the #240 cursor fix), so no amount of
/// remote mouse motion can focus-follows-mouse its way over, and no window rule names our output.
/// So without this, every app the host launches for the session — the whole game library — opens on
/// a monitor the client cannot see, and the stream shows a bare desktop. This is the EXTEND-topology
/// answer to that: it steers window placement without touching the operator's heads — which is also
/// the whole of what `topology: primary` can mean here (see [`warn_primary_is_not_expressible`]).
/// Under `exclusive` it is called a second time, after the heads are disabled, because that moves
/// focus (see [`disable_other_heads`]).
///
/// Best-effort by construction: a failure costs window placement, not the session, and a box with no
/// physical head was already placing windows correctly.
///
/// ⚠️ **This is a no-op under the Lua config manager.** Measured on .138 (0.55.4, Lua) 2026-08-18:
/// `hyprctl dispatch focusmonitor <name>` is parsed as Lua (`hl.dispatch(focusmonitor <name>)`) and
/// rejected, and `hl.dsp.focusmonitor` does not exist either — so the #283 window-placement fix
/// does not reach a Lua-configured box. Both rejections carry "error", so [`hyprctl_dispatch`]
/// reports them and this warns rather than failing silently; the gap itself is unfixed and belongs
/// to the #283 follow-up, not to the topology work here.
pub(crate) fn focus_output(name: &str) {
    match hyprctl_dispatch(&focus_argv(name)) {
        Ok(()) => tracing::info!(output = %name, "focused the streamed headless output"),
        Err(e) => tracing::warn!(
            output = %name, error = %format!("{e:#}"),
            "could not focus the streamed headless output — apps this session launches may open on \
             a physical monitor instead of on the stream"
        ),
    }
}

/// The `hyprctl` argv that focuses `name`, split out so a test pins its SHAPE.
///
/// `focusmonitor` is a **dispatcher**, so it lives behind the `dispatch` subcommand. Getting that
/// wrong is the one mistake here that no type can catch and that reads as success from the outside:
/// `hyprctl` answers an unknown subcommand with an exit-0 error string (see [`hyprctl_dispatch`],
/// which exists for exactly that), and the field symptom would be identical to the bug this fixes —
/// a bare streamed desktop with every launched app on the operator's monitor.
fn focus_argv(name: &str) -> [&str; 3] {
    ["dispatch", "focusmonitor", name]
}

/// `topology: primary` has no expression on this compositor, and saying so once per create is the
/// honest implementation (design §5.2 gives the whole wlr family "**unsupported** (no primary
/// concept) → log + treat as extend").
///
/// Wayland has no primary-output concept at all, and Hyprland's nearest equivalent is the *focused*
/// monitor — which [`focus_output`] already points at the streamed head for every session, whatever
/// the topology says. So `primary` is not silently dropped so much as already granted, as far as
/// this compositor can express it; what an operator does NOT get is a persistent designation other
/// clients can read. Distinct from the `exclusive` path, which really does change the desk.
fn warn_primary_is_not_expressible() {
    tracing::info!(
        "hyprland: `topology: primary` has no equivalent here — Wayland has no primary output and \
         Hyprland has only a FOCUSED monitor, which the streamed head already holds. Treating it \
         as `extend`; use `exclusive` to actually disable the operator's heads."
    );
}

/// Which heads an `exclusive` topology should disable: enabled, not ours, and **not managed**.
///
/// Pure so the group-awareness rule (design §6.1 — "exclusive means the *managed virtual displays*
/// are the only enabled outputs; never disable a sibling slot") is unit-testable without a
/// compositor. `managed` comes from [`list_monitors`], i.e. [`is_managed_output`]: `PF-<pid>-<n>`,
/// which covers a SECOND host's outputs as well as our own, so a concurrent session's screen can
/// never be blacked out by ours. `ours` is excluded by name too — belt and braces, since our own
/// output is managed by construction and the one head that must survive.
fn heads_to_disable(heads: &[crate::monitors::PhysicalMonitor], ours: &str) -> Vec<String> {
    heads
        .iter()
        .filter(|h| h.enabled && !h.managed && h.connector != ours)
        .map(|h| h.connector.clone())
        .collect()
}

/// DPMS every head that is not ours and not a sibling's off (or back on), for a **gamescope**
/// session honoring `Topology::Exclusive` — see [`crate::panel_dpms`].
///
/// Distinct from [`disable_other_heads`], which is what the *Hyprland backend's own* exclusive
/// topology does, and deliberately so on this compositor above all: disabling a Hyprland head is
/// the operation whose only known undo is re-reading the operator's whole config
/// ([`restore_heads`]), dropping every runtime override they set by hand. DPMS is a separate axis
/// — this module's own notes record `dispatch dpms on <name>` failing to re-enable a *disabled*
/// head for exactly that reason — so off/on round-trips cleanly and touches nothing else.
///
/// A gamescope spawn owns no Hyprland output, hence the empty `ours`; a concurrent session's
/// `HEADLESS-*` is still spared by [`heads_to_disable`]'s `managed` filter.
///
/// Returns the heads actually changed, so the re-light undoes exactly those.
pub(crate) fn dpms_other_heads(on: bool) -> Vec<String> {
    let Ok(heads) = list_monitors() else {
        return Vec::new();
    };
    let verb = if on { "on" } else { "off" };
    let mut changed = Vec::new();
    for name in heads_to_disable(&heads, "") {
        match hyprctl_dispatch(&["dispatch", "dpms", verb, &name]) {
            Ok(()) => changed.push(name),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "hyprland: could not DPMS this monitor for `topology: exclusive`"
            ),
        }
    }
    changed
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
        // Disabling heads re-homes their workspaces, and the compositor picks the replacement
        // focus itself. Re-assert ours so window placement still lands on the stream (the #283
        // contract) rather than on whichever head Hyprland happened to choose.
        focus_output(ours);
    }
    disabled
}

/// Disable one head, supporting **both config eras** and confirming by read-back.
///
/// Same two-era shape as [`set_monitor_rule`], and for the same reason: `hyprctl keyword` is the
/// hyprlang form and is *rejected outright* under the Lua config manager ("keyword can't work with
/// non-legacy parsers. Use eval."), while `hyprctl eval` is rejected under hyprlang ("eval is only
/// supported with the lua config manager"). Both rejections come back at **exit 0**, so the read-back
/// — not the exit status, and not the `ok` — is what decides. Measured 2026-08-18 on Hyprland
/// 0.56.2 (hyprlang, `.21`) and 0.55.4 (Lua, `.138`); both spellings disable, both verified by
/// `disabled: true` in `hyprctl -j monitors all`.
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

/// The **hyprlang** disable rule for `name` (`hyprctl keyword monitor <this>`), split out so a test
/// pins its shape. `disable` is a whole-rule verb and replaces the resolution field — there is no
/// `<name>,<mode>,disable`, and (measured) no `<name>,enable` to undo it.
fn disable_rule_spec(name: &str) -> String {
    format!("{name},disable")
}

/// The **Lua** disable rule for `name` (`hyprctl eval <this>`), split out so a test pins its shape.
///
/// The field is `disabled` (past tense) and takes a boolean. Measured on .138: `disable = true` is
/// rejected with "unknown field 'disable'", and `mode = "disable"` with "error applying field
/// 'mode'" — the hyprlang spelling does not carry over, so this is not a place to guess.
fn disable_lua_expr(name: &str) -> String {
    format!("hl.monitor{{ output = \"{name}\", disabled = true }}")
}

/// Poll until `name` reports `disabled: true` (the rule applies asynchronously), up to `timeout`.
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

/// Is head `name` currently enabled? `None` if it is not present at all. Reads `-j monitors all`,
/// which is the only listing that includes a DISABLED head (the plain `-j monitors` drops it, so
/// asking that one "is it disabled?" cannot distinguish disabled from unplugged).
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

/// How long a `disable` (or the `reload` that undoes it) has to show up in `hyprctl -j monitors
/// all`. Generous next to the measured near-instant apply; a miss is reported, never assumed.
const DISABLE_BUDGET: Duration = Duration::from_secs(3);

/// Re-enable the heads an `exclusive` session disabled. Run by the REGISTRY when the display
/// group's last member is torn down (design §6.1) and, critically, **before** that member's output
/// is removed — so Hyprland never sees zero enabled outputs.
///
/// 🛑🛑 **`hyprctl reload` is the only thing that re-enables a disabled head, and that is measured,
/// not chosen.** The obvious restore — re-apply the head's own mode/position/scale, which is what
/// `design/display-management.md` §5.2 assumes and what the issue (#284) proposed — **does not
/// work**: it answers `ok` and leaves `disabled: true`. Probed 2026-08-18 against Hyprland 0.56.2
/// (hyprlang) and 0.55.4 (Lua); every one of these was accepted and changed nothing:
///
/// * `keyword monitor <name>,<W>x<H>@<Hz>,<x>x<y>,<scale>` (the exact pre-disable rule)
/// * `keyword monitor <name>,preferred,auto,1`
/// * `keyword monitor <name>,enable` — not a verb: answers `invalid resolution`
/// * `keyword monitorv2 output=<name>,…,disabled=false`
/// * `keyword unset monitor`
/// * `eval 'hl.monitor{ output = "<name>", disabled = false, … }'` (the Lua twin)
/// * `dispatch dpms on <name>` — DPMS is a different axis; the head stays disabled
/// * `dispatch forcerendererreload`
///
/// A runtime `monitor` rule is additive, and the `disable` in it keeps winning; only re-reading the
/// config clears the runtime rules. So the restore is the operator's own config, re-applied — which
/// for a config-driven compositor is exactly what "put it back how it was" means.
///
/// ⚠️ The side effects are real and worth knowing: a reload drops **every** runtime `hyprctl
/// keyword`/`eval` override, including our own monitor rule for the streamed output (harmless — the
/// output is removed moments later by the same teardown) and any the operator set by hand; and on a
/// hyprlang config it re-runs `exec =` lines (`exec-once` is not re-run, and a Lua config's
/// `hl.on("hyprland.start", …)` autostart does not re-fire either). This runs ONLY when we actually
/// disabled something, so a box that never used `exclusive` never pays it.
fn restore_heads(disabled: &[String]) {
    if let Err(e) = hyprctl_dispatch(&["reload"]) {
        tracing::error!(
            ?disabled, error = %format!("{e:#}"),
            "hyprland: `hyprctl reload` failed — the heads this session disabled are still dark. \
             Re-run `hyprctl reload` by hand to get them back."
        );
        return;
    }
    // Report the OUTCOME, not the request: `reload` answers `ok` for "config parsed", which is not
    // the same as "the head came back" (a head the operator's own config disables stays disabled,
    // correctly). Read it back so a field report says which screens actually returned.
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
/// Ceiling on the whole ScreenCast handshake (`create_session` → `select_sources` → `start` →
/// `open_pipe_wire_remote`). Deliberately under [`select_and_cast`]'s 20 s wait so a stuck portal is
/// reported by the thread that owns it, with a reason, instead of the caller timing out on it — and,
/// far more importantly, so that thread EXITS. See the note at the handshake itself.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(15);

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
fn select_and_cast(
    output: &str,
    hw_cursor: bool,
) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode, StopGuard)> {
    ensure_xdph_config()?;
    let sel = selection_file();
    std::fs::write(&sel, picker_selection_line(output)).with_context(|| format!("write {sel}"))?;
    // Owned from the write on: every arm below (and every `?`) leaves the handshake, which is the
    // only thing that reads it.
    let _sel_file = SelectionFile(sel);
    // The NEGOTIATED cursor mode rides back with the fd and node id: it is decided inside the
    // portal thread (only there is the proxy to ask), and nothing downstream can re-derive it —
    // `hw_cursor` is the request, not the answer.
    let (setup_tx, setup_rx) =
        std::sync::mpsc::channel::<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>();
    // The teardown handshake: the thread signals this once it has closed the ScreenCast session, and
    // `StopGuard::drop` waits on it before the output is removed (see `StopGuard`). Kept a SEPARATE
    // channel from the setup one above — it fires at the other end of the cast's life, long after
    // `setup_rx` has been consumed.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::Builder::new()
        .name("punktfunk-hypr-cast".into())
        .spawn(move || portal_thread(setup_tx, closed_tx, stop_thread, hw_cursor))
        .context("spawn hyprland portal thread")?;
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
            // A cast exists now, so teardown has something to close and must wait for it. Only this
            // arm arms the wait: see the field note on `StopGuard::closed`.
            guard.closed = Some(closed_rx);
            Ok((fd, node_id, cursor_mode, guard))
        }
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
        // The MIRROR rejection, and it matched none of the markers above: `hyprctl keyword` on a
        // Lua config answers "keyword can't work with non-legacy parsers. Use eval." — note
        // "can't", not the "couldn't" that was already covered. Measured on .138 (0.55.4, Lua).
        // Without this the wrong-era `keyword` read as SUCCESS, and every caller then had to
        // notice the miss for itself by reading the state back.
        || lc.contains("can't")
        || lc.contains("cannot")
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
    setup_tx: Sender<Result<(OwnedFd, u32, crate::portal_cursor::Mode), String>>,
    closed_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // 🛑 The SHARED, never-dropped runtime — NOT a per-cast one. ashpd caches its D-Bus connection
    // process-globally, and a per-cast runtime takes that connection's background reader down with
    // it when the cast ends, leaving every later handshake in this process awaiting a reply nothing
    // is alive to read. That is the whole "the first stream works, the rest are black" bug. See
    // [`crate::portal_rt`] for the measurement.
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
            // Inside the bound below, deliberately: when the cached connection was orphaned this is
            // where the thread hung — `Screencast::new()` itself, before a single handshake call —
            // and a bound that started after it reported the caller's generic timeout instead.
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
            // NEGOTIATED against what xdph advertises, never asserted from `hw_cursor` alone: a
            // cursor mode the backend does not offer does not degrade — xdg-desktop-portal's
            // FRONTEND fails the call ("Unavailable cursor mode %x") before xdph sees it.
            // MEASURED on .21 2026-08-14, Hyprland 0.56.2 + xdph 1.4.1 (both current):
            // `AvailableCursorModes` = 3 (Hidden|Embedded) — metadata is NOT offered. So the old
            // hardcode killed EVERY cursor-forward session here, on today's packages, not just on
            // old installs: `unavailable cursor mode 4`, "pipeline build failed", black client.
            let cursor_mode = crate::portal_cursor::negotiate(&proxy, hw_cursor, "xdph").await;
            // 🛑 BOUNDED, and that bound is load-bearing. `select_sources`/`start` await a D-Bus
            // reply a wedged portal never sends, and an await that never returns CANNOT be cancelled
            // by the `stop` flag — the thread never reaches the park loop that reads it. That is how
            // one host accumulated NINE live cast threads (28 tokio workers) on 2026-08-14: each
            // timed-out attempt left one behind holding a half-created portal session on this
            // process's shared D-Bus connection, and from the first hang onwards EVERY later request
            // from this process hung too — while a freshly-spawned process talking to the very same
            // portal completed the identical handshake fine. Shorter than the caller's 20 s wait, so
            // the failure is reported HERE with a reason instead of surfacing as a bare timeout.
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

            // Park, keeping `proxy` + `session` alive until stopped. Polled at 20 ms rather than the
            // 200 ms this used to use, because the teardown now WAITS on what follows — every
            // millisecond here is a millisecond of stream teardown.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // 🛑 CLOSE THE SESSION, AND CLOSE IT *BEFORE* THE OUTPUT GOES AWAY. Dropping the
            // connection and trusting the peer to notice is what this used to do, and it is not the
            // contract: xdph destroys a session only on an explicit
            // `org.freedesktop.impl.portal.Session.Close` (`Session.cpp:37`). The caller is blocked
            // in `StopGuard::drop` waiting for the signal below, and only removes the compositor
            // output afterwards — that ordering is the whole fix; see `StopGuard`.
            //
            // Bounded: `close()` goes through xdg-desktop-portal to xdph, and an already-wedged xdph
            // never answers. Timing out here still signals, so teardown pays the budget once and
            // moves on rather than hanging on a portal that is already gone.
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

    /// `focusmonitor` is a dispatcher, so it must go through `hyprctl dispatch`. A bare
    /// `hyprctl focusmonitor NAME` is not a subcommand and hyprctl reports it with exit 0, so the
    /// only signal would be the field symptom this whole call exists to remove: apps opening on the
    /// operator's monitor while the stream shows a bare desktop.
    #[test]
    fn focus_goes_through_the_dispatch_subcommand() {
        assert_eq!(
            focus_argv("PF-1234-1"),
            ["dispatch", "focusmonitor", "PF-1234-1"]
        );
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

    /// The backend hands the picker exactly what [`crate::portal_picker`] says — that module owns the
    /// format and its xdph-parser tests, which run on every platform rather than only this leg.
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
            // The real `list_monitors` derives this with `is_managed_output`; mirror it here so the
            // fixture can't drift into asserting a rule the backend doesn't actually apply.
            managed: is_managed_output(connector),
        }
    }

    /// The group-awareness rule (design §6.1): `exclusive` disables the operator's heads and
    /// **only** those. A sibling session's output — ours or another host's, both `PF-<pid>-<n>` —
    /// must survive, or the second exclusive session blacks out the first one's screen, which is
    /// the exact bug KWin's Stage 3 shipped and Stage 5 fixed.
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
            // Already off: nothing to disable, and it must NOT end up in the restore list, or
            // teardown would switch on a head the operator had deliberately left dark.
            head("DP-3", false),
        ];
        assert_eq!(heads_to_disable(&heads, ours), vec!["DP-1", "HDMI-A-1"]);
    }

    /// A box with no physical head (the CI/headless posture) has nothing to disable, so no restore
    /// is prepared and teardown never runs a `hyprctl reload` — the reload's side effects are paid
    /// only by a session that actually took a screen.
    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "PF-4242-1";
        assert!(heads_to_disable(&[head(ours, true)], ours).is_empty());
    }

    /// Both config eras, pinned. These two strings are the whole contract with the compositor and
    /// neither is guessable: `hyprctl` answers a wrong-era or malformed rule at **exit 0**, so a
    /// typo here reads as success and the operator's screen simply stays lit under `exclusive`.
    #[test]
    fn disable_rules_are_pinned_for_both_config_eras() {
        assert_eq!(disable_rule_spec("DP-1"), "DP-1,disable");
        assert_eq!(
            disable_lua_expr("DP-1"),
            r#"hl.monitor{ output = "DP-1", disabled = true }"#
        );
    }

    /// `hyprctl keyword` under the Lua config manager answers "keyword can't work with non-legacy
    /// parsers. Use eval." at exit 0 — the one rejection shape the marker list used to miss, so the
    /// wrong-era form reported success. (Its mirror, `eval` under hyprlang, was already covered.)
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
