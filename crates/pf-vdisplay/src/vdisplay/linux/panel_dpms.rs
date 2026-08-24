//! Turning the box's OWN physical panels off — how a gamescope session honors
//! [`Topology::Exclusive`](crate::policy::Topology::Exclusive).
//!
//! A gamescope session is its own compositor: nothing on either owning route (bare spawn, managed
//! takeover) touches the desktop the box is showing, so the physical panel keeps displaying the
//! (idle) desktop for the whole stream — while the same `exclusive` policy on a *desktop* backend
//! turns the physicals off outright. That backend's mechanism is closed to us here: a compositor
//! refuses an output configuration with ZERO enabled outputs, and a gamescope session has no
//! output of its own on that desktop to leave enabled.
//!
//! DPMS is the honest translation. The desk stays exactly where it is — no topology churn, no
//! workspace moves, no window re-homing — the panels just go dark, and any LOCAL input wakes them,
//! which is the right answer for a desktop someone can walk up to. Stream input never wakes them:
//! it is injected into the nested gamescope's own EIS socket and never reaches the desktop.
//!
//! **There is no cross-compositor DPMS protocol**, so this module is a dispatcher. In order, each
//! arm self-gating so a box only pays for the one that answers:
//!
//! | desktop | mechanism |
//! |---|---|
//! | KDE / KWin | in-process `org_kde_kwin_dpms`, then a `kscreen-doctor --dpms` shell-out |
//! | sway (wlroots) | `swaymsg output <name> dpms off` ([`crate::wlroots::dpms_other_heads`]) |
//! | Hyprland | its dpms dispatcher, read-modify-verify ([`crate::hyprland::dpms_other_heads`]) |
//! | none at all | [`crate::drm_dpms`] — the CRTCs off over DRM, no compositor needed |
//! | GNOME / Mutter | **cannot be served** — see below |
//!
//! KDE is driven in-process over the compositor's own Wayland (`Connection::connect_to_env`, the
//! same stack as [`crate::kwin_output_mgmt`] and for the same reason: `kscreen-doctor` rides a
//! separate libkscreen/KDED layer that can be wedged while KWin itself answers fine). sway and
//! Hyprland are driven through their own native IPC, which is how [`crate::wlroots`] and
//! [`crate::hyprland`] already drive them — no second layer to be wedged, so no in-process twin
//! is warranted.
//!
//! Neither of those two is as simple as "send the off command", and the Hyprland one especially
//! is not: its dpms dispatcher is a **toggle** that ignores the state word (measured on 0.55.4 —
//! asking for `on` turned a lit head OFF), and the classic argv does not even parse under its Lua
//! config manager. [`crate::hyprland::dpms_other_heads`] carries the full account; the contract
//! this module depends on is only that each arm returns **the heads it actually changed**, so the
//! re-light moves exactly those and never a head it did not darken.
//!
//! The DRM arm is not an afterthought: a box sitting in Game Mode runs gamescope and NO desktop
//! compositor, and it is *exactly* the deployment whose TV the operator wants dark.
//!
//! ⚠ **GNOME is the one gap, and it is structural.** Mutter exposes no DPMS to clients at all, and
//! its `exclusive` mechanism (an `ApplyMonitorsConfig` that omits the physicals) needs a virtual
//! output of its own to keep enabled — which a gamescope session, being its own compositor, does
//! not have. The DRM floor cannot cover it either: Mutter holds DRM master, so `SET_MASTER` is
//! refused. [`darken`] says so at `warn!` rather than failing silently.
//!
//! This module owns the refcount and the hold for every arm — see [`Darkened`] for how each is
//! undone.
//!
//! **The hold is refcounted here, NOT floated through the registry's per-group restore.** Every
//! gamescope spawn is its own display group (`registry::group_key` — deliberately, they are
//! independent nested sessions), so the §6.1 group machinery alone would run the FIRST session's
//! restore at that session's teardown and re-light the panel under a second, still-streaming
//! session. Instead each exclusive spawn takes one [`acquire_stream_darken`] hold (the 0→1 edge
//! darkens) and registers [`release_stream_darken`] as its per-display topology restore (the 1→0
//! edge re-lights) — the same shape as `sleep_inhibit`'s refcount, riding the registry only for
//! the *timing* of each release.
//!
//! Crash safety comes free: DPMS is non-persistent, so a host that dies holding the panel dark
//! leaves nothing to journal — the screen re-lights on the next local input or compositor
//! restart. (Contrast the Windows `pnp_disable_monitors` path, which needs a recovery journal
//! precisely because its disable survives everything.)

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// Client bindings for the vendored KDE dpms protocol (`protocols/dpms.xml`), generated inline like
// the two in `kwin_output_mgmt`. Self-contained: its only foreign object type is the core
// `wl_output`, which `wayland_client::protocol` already provides.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/dpms.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/dpms.xml");
}

use protocol::org_kde_kwin_dpms::{Event as DpmsEvent, OrgKdeKwinDpms as Dpms};
use protocol::org_kde_kwin_dpms_manager::OrgKdeKwinDpmsManager as DpmsManager;

// The wire enum `org_kde_kwin_dpms.mode`. The XML types the `mode` request/event args as plain
// `uint` (no `enum=` attribute), so the generated signatures take/deliver `u32` — these constants
// are the protocol's values, kept in sync with the vendored `dpms.xml`.
const DPMS_MODE_ON: u32 = 0;
const DPMS_MODE_OFF: u32 = 3;

/// `org_kde_kwin_dpms_manager` is a frozen v1 protocol (its own header warns it may change
/// without a version bump, but no v2 has appeared since 2015); bind `min(advertised, 1)`.
const MANAGER_MAX: u32 = 1;
/// `wl_output.name` — the connector name used for logging — arrived in v4. Everything else we do
/// works at v1, so a lower advert just costs the log its names.
const WL_OUTPUT_MAX: u32 = 4;

/// Overall budget for one darken/re-light operation (mirrors `kwin_output_mgmt::OP_BUDGET`):
/// generous next to a healthy roundtrip, and only there so a wedged compositor can't pin the
/// session-create (or group-teardown) thread.
const OP_BUDGET: Duration = Duration::from_secs(3);

/// Poll slice while waiting on the Wayland fd (matches `kwin_output_mgmt`).
const POLL_MS: i32 = 100;

/// One output's accumulated state on this connection, keyed by its `wl_output` global name.
#[derive(Default)]
struct OutputState {
    proxy: Option<WlOutput>,
    /// Connector name (`DP-1`) from `wl_output.name` (v4) — logging only; the global number is
    /// the address everything operates on.
    connector: Option<String>,
    dpms: Option<Dpms>,
    /// `org_kde_kwin_dpms.supported` — `None` until the bind burst arrives.
    supported: Option<bool>,
    /// The last `org_kde_kwin_dpms.mode` seen — kept current, so the post-`set` wait can watch it
    /// flip.
    mode: Option<u32>,
}

/// Everything one connection's queue accumulates.
#[derive(Default)]
struct State {
    manager: Option<DpmsManager>,
    /// Keyed by the `wl_output` GLOBAL NAME — a stable address for the compositor's lifetime, and
    /// the identity the darken records so the re-light (a separate, later connection) can find the
    /// same outputs again.
    outputs: HashMap<u32, OutputState>,
    /// Highest `wl_callback` serial whose `done` has arrived — the barrier the pump waits on.
    sync_done: u32,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == DpmsManager::interface().name {
                    let v = version.min(MANAGER_MAX);
                    state.manager = Some(registry.bind::<DpmsManager, _, _>(name, v, qh, ()));
                } else if interface == WlOutput::interface().name {
                    let v = version.min(WL_OUTPUT_MAX);
                    // The global name rides in the UserData so the output's own events (and the
                    // dpms object's, which gets the same stamp) can find this entry.
                    let out = registry.bind::<WlOutput, _, _>(name, v, qh, name);
                    state.outputs.entry(name).or_default().proxy = Some(out);
                }
            }
            // An output unplugged mid-operation: drop the entry so we never `set` on its corpse.
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.get_mut(global) {
                o.connector = Some(name);
            }
        }
    }
}

impl Dispatch<Dpms, u32> for State {
    fn event(
        state: &mut Self,
        _: &Dpms,
        event: DpmsEvent,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(global) else {
            return;
        };
        match event {
            DpmsEvent::Supported { supported } => o.supported = Some(supported != 0),
            DpmsEvent::Mode { mode } => o.mode = Some(mode),
            DpmsEvent::Done => {}
        }
    }
}

// The manager has no events; the impl exists because `WlRegistry::bind` demands one.
impl Dispatch<DpmsManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &DpmsManager,
        _: protocol::org_kde_kwin_dpms_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        serial: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = state.sync_done.max(*serial);
        }
    }
}

/// Why [`Session::open`] declined — the same honest-decline discipline as
/// `kwin_output_mgmt::OpenFailure`: which rung said no decides both the log level and whether the
/// `kscreen-doctor` fallback is worth attempting.
enum OpenFailure {
    /// No Wayland connection at all (`WAYLAND_DISPLAY` unset/stale). The common case for the bare
    /// spawn's natural habitat — a headless plain-distro box with no desktop to darken.
    Connect(String),
    /// The compositor accepted the connection but did not answer the registry barrier in budget:
    /// a live but wedged session — the case the shell-out fallback exists for.
    RegistryBarrier,
    /// Connected and answering, but `org_kde_kwin_dpms_manager` is not advertised — not KWin. A
    /// definitive answer: no fallback can succeed here either (`kscreen-doctor` drives the same
    /// KDE-only machinery), so this rung declines without one.
    NoDpmsGlobal,
    /// The manager is there but the per-output DPMS state bursts never completed in budget.
    StateBarrier,
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenFailure::Connect(e) => write!(f, "no Wayland connection ({e})"),
            OpenFailure::RegistryBarrier => {
                write!(
                    f,
                    "the compositor did not answer the registry roundtrip in budget"
                )
            }
            OpenFailure::NoDpmsGlobal => {
                write!(f, "org_kde_kwin_dpms_manager is not advertised (not KWin)")
            }
            OpenFailure::StateBarrier => {
                write!(
                    f,
                    "the outputs' DPMS state never finished announcing in budget"
                )
            }
        }
    }
}

/// A connected session with the manager bound and every output's DPMS state read.
struct Session {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    next_sync: u32,
}

impl Session {
    /// [`Session::connect`] for the operation named by `op`, logging the decline at a level that
    /// matches what it means: `Connect`/`NoDpmsGlobal` are the everyday non-KDE answers (most
    /// bare-spawn boxes have no desktop at all) and log at debug; the two barrier failures mean a
    /// LIVE session stopped answering — on a KDE box that is a panel left lit, so they warn.
    fn open(op: &'static str) -> Result<Session, OpenFailure> {
        let opened = Session::connect();
        if let Err(reason) = &opened {
            match reason {
                OpenFailure::Connect(_) | OpenFailure::NoDpmsGlobal => {
                    tracing::debug!(op, %reason, "KWin DPMS unavailable");
                }
                OpenFailure::RegistryBarrier | OpenFailure::StateBarrier => {
                    tracing::warn!(
                        op,
                        %reason,
                        "KWin DPMS: in-process path unavailable — falling back to kscreen-doctor"
                    );
                }
            }
        }
        opened
    }

    /// Connect to the desktop's Wayland socket, bind the dpms manager + every `wl_output`, create
    /// a dpms status object per output and drain their state bursts — all bounded by [`OP_BUDGET`].
    fn connect() -> Result<Session, OpenFailure> {
        let conn = Connection::connect_to_env().map_err(|e| OpenFailure::Connect(e.to_string()))?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut s = Session {
            conn,
            queue,
            state: State::default(),
            next_sync: 0,
        };
        let deadline = Instant::now() + OP_BUDGET;
        // Phase 1: process the registry globals (binds the manager + every wl_output).
        if !s.sync_barrier(deadline) {
            return Err(OpenFailure::RegistryBarrier);
        }
        let Some(mgr) = s.state.manager.clone() else {
            return Err(OpenFailure::NoDpmsGlobal);
        };
        // Phase 2: one dpms status object per output (stamped with the output's global name so its
        // events land on the right entry), then a barrier that drains both the outputs' `name`
        // events and the dpms objects' supported/mode/done bursts.
        let qh = s.queue.handle();
        let bound: Vec<(u32, WlOutput)> = s
            .state
            .outputs
            .iter()
            .filter_map(|(g, o)| o.proxy.clone().map(|p| (*g, p)))
            .collect();
        for (global, out) in bound {
            let d = mgr.get(&out, &qh, global);
            if let Some(o) = s.state.outputs.get_mut(&global) {
                o.dpms = Some(d);
            }
        }
        if !s.sync_barrier(deadline) {
            return Err(OpenFailure::StateBarrier);
        }
        Ok(s)
    }

    /// Send a `wl_display.sync` and pump the queue until its `done` arrives or `deadline` passes.
    fn sync_barrier(&mut self, deadline: Instant) -> bool {
        self.next_sync += 1;
        let serial = self.next_sync;
        let qh = self.queue.handle();
        let _cb = self.conn.display().sync(&qh, serial);
        self.pump_until(deadline, |st| st.sync_done >= serial)
    }

    /// Bounded manual event loop — flush, dispatch, poll the fd. Mirrors
    /// `kwin_output_mgmt::Session::pump_until` (same rationale: `blocking_dispatch` can't be
    /// interrupted, so the fd is polled in [`POLL_MS`] slices against `deadline`).
    fn pump_until(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> bool {
        loop {
            if done(&self.state) {
                return true;
            }
            if self.queue.dispatch_pending(&mut self.state).is_err() {
                return false;
            }
            if done(&self.state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if self.conn.flush().is_err() {
                return false;
            }
            let Some(guard) = self.conn.prepare_read() else {
                continue; // events already queued — loop dispatches them
            };
            let mut pfd = libc::pollfd {
                fd: self.conn.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = (remaining.as_millis() as i32).clamp(0, POLL_MS);
            // SAFETY: `&mut pfd` points at one live, fully-initialized `libc::pollfd` on the stack
            // and the count `1` matches that single element, so `poll` reads `fd`/`events` and
            // writes `revents` strictly within `pfd`. `pfd.fd` is the Wayland connection's fd,
            // valid because `self.conn` (and the `prepare_read` guard) outlive the call. `poll`
            // blocks up to `timeout` ms and writes only `revents`; `pfd` is a fresh local that
            // aliases nothing.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let _ = guard.read();
            } // else: timeout/signal — drop the guard, re-check the deadline
        }
    }

    /// Request `target` on every DPMS-supporting output not already there — restricted to the
    /// globals in `only` when given (the re-light path, which must touch ONLY what the darken
    /// touched: a panel the USER had put to sleep before the stream is theirs to keep dark).
    /// Returns the outputs actually asked to change, `(global, connector)`, then waits (within
    /// budget) for each one's `mode` event to confirm — the protocol is explicit that `set` is a
    /// request the compositor may decline, so the confirmation is watched and its absence logged
    /// rather than assumed.
    fn set_mode(&mut self, target: u32, only: Option<&[u32]>) -> Vec<(u32, Option<String>)> {
        let deadline = Instant::now() + OP_BUDGET;
        let mut touched: Vec<(u32, Option<String>)> = Vec::new();
        for (global, o) in &self.state.outputs {
            if only.is_some_and(|list| !list.contains(global)) {
                continue;
            }
            if o.supported != Some(true) || o.mode == Some(target) {
                continue;
            }
            if let Some(dpms) = &o.dpms {
                dpms.set(target);
                touched.push((*global, o.connector.clone()));
            }
        }
        if touched.is_empty() {
            return touched;
        }
        let want: Vec<u32> = touched.iter().map(|(g, _)| *g).collect();
        // An output that vanished mid-wait (GlobalRemove pruned it) counts as settled — there is
        // nothing left to flip.
        let confirmed = self.pump_until(deadline, |st| {
            want.iter()
                .all(|g| st.outputs.get(g).is_none_or(|o| o.mode == Some(target)))
        });
        if !confirmed {
            tracing::warn!(
                outputs = ?touched,
                target,
                "KWin DPMS: the compositor did not confirm the mode change in budget (the \
                 requests are flushed; it may still land, or KWin may have declined)"
            );
        }
        touched
    }
}

/// What the 0→1 darken actually achieved — the record the 1→0 re-light undoes. Which arm did the
/// work matters: the two are undone through different doors.
enum Darkened {
    /// The in-process path turned these outputs off — `(wl_output global, connector)`. Global
    /// names are stable for the compositor's lifetime, so a later connection re-lights exactly
    /// these. If KWin restarted in between the names match nothing — and that is the CORRECT
    /// no-op, because a fresh KWin brings its outputs up lit anyway.
    Wayland(Vec<(u32, Option<String>)>),
    /// The `kscreen-doctor --dpms off` fallback ran (it takes no per-output address, so the
    /// re-light is the symmetric `--dpms on`).
    Kscreen,
    /// sway (wlroots) turned these outputs off — `swaymsg output <name> dpms off`. Addressed by
    /// connector name, so the re-light undoes exactly the heads we changed and never a sibling's.
    Sway(Vec<String>),
    /// Hyprland turned these monitors off — `hyprctl dispatch dpms off <name>`. Same per-name
    /// discipline as [`Darkened::Sway`], and the same reason.
    Hyprland(Vec<String>),
    /// No desktop to ask, so [`crate::drm_dpms`] turned the CRTCs off over DRM directly. The
    /// re-light is a `drop` — the hold IS a set of open `/dev/dri/cardN` fds, and the kernel
    /// re-lights on last close. Nothing to replay, and crash-safe for the same reason.
    Drm(crate::drm_dpms::DrmDarken),
}

/// The host-wide darken hold — refcounted like `sleep_inhibit`: the 0→1 edge darkens, the 1→0
/// edge re-lights, and everything between is bookkeeping. See the module docs for why the
/// registry's per-group restore float can't provide this (every gamescope spawn is its own group).
struct Holds {
    count: u32,
    /// What the 0→1 darken achieved, held until the 1→0 release undoes it. `None` while count > 0
    /// means the darken found nothing to do (no KDE, panels already dark) — the release then has
    /// nothing to undo, which is exactly right.
    darkened: Option<Darkened>,
}

impl Holds {
    /// Take a hold; `true` on the 0→1 edge — the caller darkens and [`record`](Self::record)s.
    fn acquire_edge(&mut self) -> bool {
        self.count += 1;
        self.count == 1
    }

    /// Store the 0→1 darken's outcome.
    fn record(&mut self, d: Option<Darkened>) {
        self.darkened = d;
    }

    /// Drop a hold; `Some` on the 1→0 edge hands the caller the record to undo. A release with no
    /// hold outstanding is a caller bug (an unbalanced restore) — logged, never underflowed.
    fn release_edge(&mut self) -> Option<Darkened> {
        if self.count == 0 {
            tracing::warn!("KWin DPMS: release without a matching acquire (unbalanced restore)");
            return None;
        }
        self.count -= 1;
        if self.count == 0 {
            self.darkened.take()
        } else {
            None
        }
    }
}

static HOLDS: Mutex<Holds> = Mutex::new(Holds {
    count: 0,
    darkened: None,
});

/// Take one darken hold for an exclusive-topology stream. The first hold turns the live KDE
/// desktop's panels off (best-effort, bounded); later holds just count. Callers MUST balance each
/// call with [`release_stream_darken`] — the gamescope backend does it by registering the release
/// as the display's topology restore, so the registry runs it exactly once per display at
/// teardown (§6.1).
///
/// The lock is deliberately held across the darken itself: a racing second acquire must queue
/// behind it (and then see the recorded outcome), not observe a count of 2 with nothing darkened.
/// Same discipline on the release side, which keeps a teardown-overlapping-connect sequence
/// strictly ordered: re-light completes, then the new stream's darken runs.
pub fn acquire_stream_darken() {
    let mut h = HOLDS.lock().unwrap_or_else(|e| e.into_inner());
    if h.acquire_edge() {
        let d = darken();
        h.record(d);
    }
}

/// Drop one darken hold; the last one out re-lights whatever the first hold's darken achieved.
pub fn release_stream_darken() {
    let mut h = HOLDS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = h.release_edge() {
        relight(d);
    }
}

/// The non-KDE desktops we can ask, in preference order. Each self-gates on its own IPC being
/// reachable — `wlroots::dpms_other_heads` shells out to `swaymsg`, which needs `SWAYSOCK`;
/// Hyprland's needs `HYPRLAND_INSTANCE_SIGNATURE` — so a box only ever pays for the one that
/// answers, and a box running neither falls straight through.
///
/// Both address heads BY NAME and report back the ones they actually changed, so the re-light
/// undoes exactly those and never a concurrent session's headless output.
///
/// **GNOME is absent on purpose.** Mutter exposes no DPMS to clients at all, and its `exclusive`
/// mechanism (`ApplyMonitorsConfig` omitting the physicals) needs a virtual output of its own to
/// keep enabled — which a gamescope spawn, being its own compositor, does not have. There is
/// nothing to call; the `warn!` at the end of [`darken`] names it rather than failing silently.
fn non_kde_desktop_darken() -> Option<Darkened> {
    let sway = crate::wlroots::dpms_other_heads(false);
    if !sway.is_empty() {
        tracing::info!(
            outputs = ?sway,
            "sway: desktop outputs off for the exclusive gamescope stream"
        );
        return Some(Darkened::Sway(sway));
    }
    let hypr = crate::hyprland::dpms_other_heads(false);
    if !hypr.is_empty() {
        tracing::info!(
            outputs = ?hypr,
            "hyprland: desktop monitors off for the exclusive gamescope stream"
        );
        return Some(Darkened::Hyprland(hypr));
    }
    None
}

/// The 0→1 darken: in-process over `org_kde_kwin_dpms` first, `kscreen-doctor --dpms off` as the
/// wedged-compositor fallback, then the other desktops, then DRM. `None` = nothing was darkened
/// (no desktop that answers, panels already off, or every arm declined) — and therefore nothing to
/// restore.
fn darken() -> Option<Darkened> {
    match Session::open("darken") {
        Ok(mut s) => {
            let touched = s.set_mode(DPMS_MODE_OFF, None);
            if touched.is_empty() {
                tracing::debug!(
                    "KWin DPMS: no output to darken (none supported, or all already off)"
                );
                None
            } else {
                tracing::info!(
                    outputs = ?touched,
                    "KWin DPMS: desktop outputs off for the exclusive gamescope stream"
                );
                Some(Darkened::Wayland(touched))
            }
        }
        // Definitive "not KDE" / "no desktop": no fallback can do better (kscreen-doctor drives
        // the same KDE-only machinery). Declining is still right — but NOT quietly. [`darken`] is
        // only ever reached because the operator selected `Topology::Exclusive`, so every decline
        // here is "you asked for your screens off and they stayed on", which is a verdict and not
        // a routine state. It sat at `debug!` in `open`, and that silence is what made the Nobara
        // field report (2026-08-24) undiagnosable: no line anywhere named the panel. Same
        // discipline as [`relight`], which has always said so when it gave up — a lit panel under
        // `exclusive` deserves the honesty a dark one already got.
        Err(e @ (OpenFailure::NoDpmsGlobal | OpenFailure::Connect(_))) => {
            // Not KDE. Try the other desktops we drive, then the compositor-independent floor.
            // Each arm self-gates on its own IPC being reachable, so the order is just preference
            // and a box only ever pays for the ones that answer.
            if let Some(d) = non_kde_desktop_darken() {
                return Some(d);
            }
            match crate::drm_dpms::darken() {
                Some(d) => {
                    tracing::info!(
                        cards = ?d.darkened,
                        "DRM: the box's own CRTCs are off for the exclusive gamescope stream (no \
                         desktop compositor to ask — a session in Game Mode has none)"
                    );
                    Some(Darkened::Drm(d))
                }
                // Nothing on this box was ours to darken: no desktop that answers, and then no
                // `/dev/dri` card that was ours either — every one already mastered by someone
                // else (a live compositor, including the gamescope an Attach route is mirroring,
                // which must NOT be darkened), or nothing lit. Say so: `darken` is only ever
                // reached because the operator selected `Topology::Exclusive`, so this is "you
                // asked for your screens off and they stayed on" — a verdict, not a routine
                // state. It sat at `debug!` in `open`, and that silence is what made the Nobara
                // field report (2026-08-24) undiagnosable: no line anywhere named the panel.
                None => {
                    tracing::warn!(
                        %e,
                        "exclusive topology asked for the box's own screens to go dark: no \
                         desktop compositor on this box could be asked (GNOME/Mutter exposes no \
                         DPMS to clients), and no DRM card was ours to turn off either — the \
                         panel stays as it is for this stream"
                    );
                    None
                }
            }
        }
        // A live session that stopped answering: the standalone tool rides a different stack
        // (libkscreen/KDED) and may still get through — the same rationale as `kwin.rs`'s
        // kscreen fallbacks, honest-verdict discipline included.
        Err(_) => match kscreen_dpms("off") {
            Some(true) => {
                tracing::info!(
                    "KWin DPMS: desktop outputs off for the exclusive gamescope stream \
                     (kscreen-doctor fallback)"
                );
                Some(Darkened::Kscreen)
            }
            // Killed at its budget — NOT a refusal: kscreen-doctor applies first and then waits
            // on the compositor, so a loaded KWin routinely lands the change and still gets
            // killed. Record the darken so the teardown re-light runs either way; a `--dpms on`
            // against a lit panel is a no-op.
            None => Some(Darkened::Kscreen),
            Some(false) => {
                tracing::warn!(
                    "KWin DPMS: could not darken the desktop outputs for the exclusive topology \
                     (in-process path and kscreen-doctor both declined) — the panel stays lit"
                );
                None
            }
        },
    }
}

/// The 1→0 re-light. **This is the last line of defence for a dark monitor**, so every arm that
/// gives up says so loudly (the same discipline as `kwin.rs::reenable_outputs_kscreen`) — a dark
/// panel with no line in the log is the failure mode this chain exists to prevent. The worst case
/// stays self-healing regardless: DPMS is non-persistent, and any local input wakes the panel.
fn relight(d: Darkened) {
    match d {
        Darkened::Wayland(outputs) => {
            let globals: Vec<u32> = outputs.iter().map(|(g, _)| *g).collect();
            match Session::open("re-light") {
                Ok(mut s) => {
                    s.set_mode(DPMS_MODE_ON, Some(&globals));
                    tracing::info!(outputs = ?outputs, "KWin DPMS: desktop outputs back on");
                }
                Err(_) => match kscreen_dpms("on") {
                    Some(true) | None => {
                        tracing::info!(
                            "KWin DPMS: desktop outputs back on (kscreen-doctor fallback)"
                        );
                    }
                    Some(false) => {
                        tracing::error!(
                            outputs = ?outputs,
                            "KWin DPMS: could NOT re-light the desktop outputs (in-process \
                             restore and kscreen-doctor both declined) — the panel stays dark \
                             until local input wakes it"
                        );
                    }
                },
            }
        }
        Darkened::Kscreen => {
            if kscreen_dpms("on") == Some(false) {
                tracing::error!(
                    "KWin DPMS: could NOT re-light the desktop outputs (kscreen-doctor refused \
                     the --dpms on it earlier accepted the off for) — the panel stays dark until \
                     local input wakes it"
                );
            }
        }
        // Per-name, so exactly the heads we darkened come back and a sibling's headless output is
        // never switched on by us. A head the operator unplugged meanwhile just fails its one
        // command and says so — the others still re-light.
        Darkened::Sway(outputs) => {
            let back = crate::wlroots::dpms_other_heads(true);
            if back.is_empty() {
                tracing::error!(
                    ?outputs,
                    "sway: could NOT re-light the desktop outputs — they stay dark until local \
                     input or `swaymsg output '*' dpms on`"
                );
            } else {
                tracing::info!(outputs = ?back, "sway: desktop outputs back on");
            }
        }
        Darkened::Hyprland(outputs) => {
            let back = crate::hyprland::dpms_other_heads(true);
            if back.is_empty() {
                tracing::error!(
                    ?outputs,
                    "hyprland: could NOT re-light the desktop monitors — they stay dark until \
                     local input or `hyprctl dispatch dpms on`"
                );
            } else {
                tracing::info!(outputs = ?back, "hyprland: desktop monitors back on");
            }
        }
        // The one arm that cannot fail: the hold IS the open fds, so dropping it closes them and
        // the kernel's last-close restores the console. No ioctl to be refused, no saved mode to
        // replay — which is why this path needs no "could NOT re-light" line of its own.
        Darkened::Drm(d) => {
            let cards = d.darkened.clone();
            drop(d);
            tracing::info!(?cards, "DRM: the box's own CRTCs released — panel back on");
        }
    }
}

/// `kscreen-doctor --dpms <on|off>` for its verdict, on `kwin.rs`'s shared budget and three-state
/// convention (`Some(true)` ran and succeeded, `Some(false)` refused or unrunnable, `None` killed
/// at the budget — which, for a tool that applies first and waits after, usually means it landed).
fn kscreen_dpms(mode: &'static str) -> Option<bool> {
    crate::kwin::kscreen_verdict(&["--dpms".to_string(), mode.to_string()])
}

#[cfg(test)]
mod tests {
    use super::{Darkened, Holds};

    fn fresh() -> Holds {
        Holds {
            count: 0,
            darkened: None,
        }
    }

    #[test]
    fn first_acquire_darkens_later_ones_count() {
        let mut h = fresh();
        assert!(h.acquire_edge(), "0→1 must darken");
        h.record(Some(Darkened::Kscreen));
        assert!(
            !h.acquire_edge(),
            "a second concurrent stream must not re-darken"
        );
        assert!(!h.acquire_edge());
    }

    #[test]
    fn only_the_last_release_relights() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Wayland(vec![(7, Some("DP-1".into()))])));
        assert!(!h.acquire_edge());
        // First release: a sibling still streams — the panel must stay dark.
        assert!(h.release_edge().is_none());
        // Last release hands back the record to undo.
        let d = h.release_edge();
        assert!(matches!(d, Some(Darkened::Wayland(v)) if v == vec![(7, Some("DP-1".into()))]));
    }

    #[test]
    fn a_darken_that_did_nothing_restores_nothing() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(None); // no KDE / already dark: nothing was changed
        assert!(h.release_edge().is_none(), "nothing to undo");
        assert_eq!(h.count, 0);
    }

    #[test]
    fn unbalanced_release_never_underflows() {
        let mut h = fresh();
        assert!(h.release_edge().is_none());
        assert_eq!(h.count, 0, "count must not wrap");
        // And the state machine still works afterwards.
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Kscreen));
        assert!(matches!(h.release_edge(), Some(Darkened::Kscreen)));
    }

    #[test]
    fn a_full_cycle_rearms_the_darken() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Kscreen));
        assert!(h.release_edge().is_some());
        // A later stream on the same host lifetime darkens again.
        assert!(
            h.acquire_edge(),
            "the 0→1 edge must re-arm after a full cycle"
        );
    }
}
