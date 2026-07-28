//! In-process KDE output management (`kde_output_management_v2` + `kde_output_device_v2`).
//!
//! Topology — make the streamed output primary, disable the physical/bootstrap outputs, capture
//! their modes for restore, re-enable them on teardown, position the output — used to shell out to
//! `kscreen-doctor` (see [`super::kwin`]). But `kscreen-doctor` drives a *separate* stack:
//! libkscreen picks a backend and, depending on the setup, waits on the kscreen KDED module over
//! D-Bus. On a machine where THAT layer is wedged it blocks in its own connect and never returns —
//! a field report (Nobara, KWin 6.6.4) showed every topology `kscreen-doctor` timing out at its 5 s
//! budget, so the streamed output never became the desktop (`also_disabled=[]`) and bring-up took
//! ~26 s.
//!
//! The compositor's OWN Wayland is provably responsive on that same session — the host just created
//! a virtual output over it via `zkde_screencast` — so we drive `kde_output_management_v2` directly
//! over Wayland here, sidestepping whatever wedges the standalone tool. Every wait is time-bounded,
//! so a genuinely wedged compositor degrades to `handled = false` and the caller falls back to the
//! `kscreen-doctor` path rather than hanging.
//!
//! KWin advertises one `kde_output_device_v2` global per output (the classic model; verified live:
//! `kde_output_management_v2` v19, `kde_output_device_v2` v20 on KWin 6.6.4). We bind them all, read
//! each output's name / enabled / priority / current-mode size, then build a
//! `kde_output_configuration_v2` and `apply()` it, waiting for `applied` / `failed`.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

// Generate the client bindings for the two vendored KDE protocols inline (no build.rs). The
// management protocol references the device interfaces, so they can't share one `__interfaces`
// module (each `generate_interfaces!` emits its own helper items, which collide). Instead — the
// interdependent-protocol pattern from the `wayland-protocols` crate — `device` is a self-contained
// module and `management` pulls in `device`'s interface statics + generated proxy types before its
// own codegen, so its cross-protocol object args (`kde_output_device_v2`, `…_mode_v2`) resolve.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod device {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-output-device-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/kde-output-device-v2.xml");
}

#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod management {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use super::super::device::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-output-management-v2.xml");
    }
    use self::__interfaces::*;
    // The device protocol's generated modules/types, so the foreign object args resolve.
    use super::device::*;

    wayland_scanner::generate_client_code!("protocols/kde-output-management-v2.xml");
}

use device::kde_output_device_mode_v2::{Event as ModeEvent, KdeOutputDeviceModeV2 as DeviceMode};
use device::kde_output_device_registry_v2::{
    Event as RegistryEvent, KdeOutputDeviceRegistryV2 as DeviceRegistry,
};
use device::kde_output_device_v2::{Event as DeviceEvent, KdeOutputDeviceV2 as OutputDevice};
use management::kde_mode_list_v2::KdeModeListV2 as ModeList;
use management::kde_output_configuration_v2::{
    Event as ConfigEvent, KdeOutputConfigurationV2 as OutputConfig,
};
use management::kde_output_management_v2::KdeOutputManagementV2 as OutputManagement;

/// Highest interface versions we drive; we bind `min(advertised, MAX)`. Every request we issue is
/// `since ≤ 2` (`create_configuration`/`enable`/`mode`/`position`/`apply` are v1, `set_primary_output`
/// is v2) and every event we read is `since ≤ 18` (`priority`), so binding high and calling low is
/// always in range on any KWin that advertises the globals.
const MGMT_MAX: u32 = 22;
const DEVICE_MAX: u32 = 24;
/// `kde_output_device_registry_v2` — the newer way KWin hands out output devices (verified live on
/// KWin 6.7.3, which advertises this at v23 and NO per-output `kde_output_device_v2` globals).
const DEVICE_REGISTRY_MAX: u32 = 24;

/// The opcode of `kde_output_device_v2.mode` (0-based event index) — the event that creates a child
/// `kde_output_device_mode_v2`. Kept in sync with the vendored `kde-output-device-v2.xml`.
// The `event_created_child!` macro hard-codes the literal 2, so nothing reads this constant at
// run time; `mode_event_opcode_is_two` asserts the two agree. That guard test is the point — it is
// what catches a re-vendored XML that reorders the events.
#[allow(dead_code)]
const DEVICE_MODE_EVENT_OPCODE: u16 = 2;
/// The opcode of `kde_output_device_registry_v2.output` — the event that creates a child
/// `kde_output_device_v2`. `finished` is event 0, `output` is event 1.
const REGISTRY_OUTPUT_EVENT_OPCODE: u16 = 1;

/// Overall budget for one enumerate-then-apply operation. Generous next to a healthy roundtrip (a
/// few ms); it exists only so a wedged compositor can't pin the session's stream thread.
const OP_BUDGET: Duration = Duration::from_secs(3);

/// Poll slice while waiting on the Wayland fd (matches the keepalive loop's cadence in `kwin.rs`).
const POLL_MS: i32 = 100;

/// KWin's CVT generator aligns a custom mode's width DOWN to a multiple of this (libxcvt's cell
/// grain), so the mode it builds for a `set_custom_modes` request may be a few px narrower than
/// asked — matches `kwin::CVT_H_GRANULARITY`. Used when matching the generated mode back.
const CVT_H_GRANULARITY: u32 = 8;

/// Which topology to apply once our output is resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopologyKind {
    /// Make ours the sole desktop: primary + disable every other enabled output.
    Exclusive,
    /// Make ours primary but leave the other outputs enabled.
    Primary,
}

/// Outcome of [`apply_topology`].
pub(crate) struct TopologyOutcome {
    /// UUID of our resolved virtual output — a stable per-output id that survives a mode-switch
    /// supersede (unlike the shared name) — for later [`set_position`] / restore addressing.
    pub our_uuid: Option<String>,
    /// The outputs we disabled, each `(name, "WxH@Hz")`, so teardown can restore them at their exact
    /// mode. Empty for `Primary`, or when nothing else was enabled.
    pub disabled: Vec<(String, String)>,
    /// `true` if the in-process path bound management, resolved our output, and applied (or tried to)
    /// a configuration. `false` ⇒ the compositor didn't answer in budget or our output never
    /// appeared, so the caller should fall back to `kscreen-doctor`.
    pub handled: bool,
}

/// One output as read from `kde_output_device_v2`.
#[derive(Default, Clone)]
struct DeviceState {
    /// The global `name` number (higher = more recently advertised) — used to pick the newest of two
    /// same-named outputs during a supersede.
    global: u32,
    name: Option<String>,
    uuid: Option<String>,
    enabled: bool,
    /// Top-left in the compositor's global logical space, from the `geometry` event — the key that
    /// makes a head identifiable when two of them share a size (`monitors::PhysicalMonitor`).
    position: (i32, i32),
    /// `make` / `model` from the same event, for the picker label.
    make: Option<String>,
    model: Option<String>,
    /// Logical scale from the `scale` event. `None` ⇒ the protocol's documented default of 1.
    scale: Option<f64>,
    /// KWin's output priority; 1 is the primary. `None` until the `priority` event (device ≥ v18).
    priority: Option<u32>,
    /// The `current_mode` object id; its size is looked up in [`State::mode_dims`].
    current_mode: Option<ObjectId>,
    /// Every mode this output advertised, in announce order — `(mode object id, proxy)` — so restore
    /// can pick the one matching a captured `WxH@Hz`.
    modes: Vec<(ObjectId, DeviceMode)>,
    /// Set once this output's `done` burst has been seen (its state is coherent to read).
    seen_done: bool,
    proxy: Option<OutputDevice>,
}

/// Everything the enumerate/apply queue accumulates on one connection.
#[derive(Default)]
struct State {
    management: Option<OutputManagement>,
    mgmt_name_version: Option<(u32, u32)>,
    /// The `kde_output_device_registry_v2` on KWin ≥ 6.7, held so it keeps announcing outputs for
    /// the life of the session (dropping it would end the announcements).
    device_registry: Option<DeviceRegistry>,
    devices: HashMap<ObjectId, DeviceState>,
    /// mode object id → `(width, height, refresh_mHz)`.
    mode_dims: HashMap<ObjectId, (u32, u32, u32)>,
    /// Highest `wl_callback` serial whose `done` has arrived — the barrier the pump waits on.
    sync_done: u32,
    /// Configuration apply verdict: `Some(true)` = applied, `Some(false)` = failed.
    applied: Option<bool>,
    failure_reason: Option<String>,
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
                if interface == OutputManagement::interface().name {
                    let v = version.min(MGMT_MAX);
                    state.management =
                        Some(registry.bind::<OutputManagement, _, _>(name, v, qh, ()));
                    state.mgmt_name_version = Some((name, v));
                } else if interface == OutputDevice::interface().name {
                    let v = version.min(DEVICE_MAX);
                    // The device's `name` global carries into the device's UserData so the event
                    // handler can record it (newest-wins tie-break during a supersede).
                    let dev = registry.bind::<OutputDevice, _, _>(name, v, qh, name);
                    let id = dev.id();
                    state.devices.entry(id).or_default().proxy = Some(dev);
                } else if interface == DeviceRegistry::interface().name {
                    // KWin ≥ 6.7 (Plasma 6.7.3 verified) no longer advertises ONE
                    // `kde_output_device_v2` global per output — it advertises this registry and
                    // hands the devices out through its `output` events instead. Binding it is what
                    // makes this whole module work on a current KWin: without it the device list
                    // comes back EMPTY, which reads as "no outputs" and silently degrades every
                    // topology apply to the `kscreen-doctor` shell-out this module exists to avoid.
                    // Both models are kept — the per-output globals are still what older KWin sends.
                    let v = version.min(DEVICE_REGISTRY_MAX);
                    state.device_registry =
                        Some(registry.bind::<DeviceRegistry, _, _>(name, v, qh, ()));
                }
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

/// The device registry hands out one `kde_output_device_v2` per output via its `output` event
/// (a `new_id`, so the child is created by the `event_created_child!` binding below). Devices that
/// arrive this way have no global `name` number — the newest-wins supersede tie-break uses 0 for
/// them, which is fine: that tie-break only matters for the per-output-global model.
impl Dispatch<DeviceRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &DeviceRegistry,
        event: RegistryEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let RegistryEvent::Output { output } = event {
            let id = output.id();
            state.devices.entry(id).or_default().proxy = Some(output);
        }
    }

    event_created_child!(State, DeviceRegistry, [
        REGISTRY_OUTPUT_EVENT_OPCODE => (OutputDevice, 0u32),
    ]);
}

// Management has no events.
impl Dispatch<OutputManagement, ()> for State {
    fn event(
        _: &mut Self,
        _: &OutputManagement,
        _: management::kde_output_management_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// A client-built custom-mode list has no events; it just needs a Dispatch impl to be created.
impl Dispatch<ModeList, ()> for State {
    fn event(
        _: &mut Self,
        _: &ModeList,
        _: management::kde_mode_list_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The device's UserData is its global `name` number.
impl Dispatch<OutputDevice, u32> for State {
    fn event(
        state: &mut Self,
        device: &OutputDevice,
        event: DeviceEvent,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.devices.entry(device.id()).or_default();
        entry.global = *global;
        if entry.proxy.is_none() {
            entry.proxy = Some(device.clone());
        }
        match event {
            DeviceEvent::Name { name } => entry.name = Some(name),
            DeviceEvent::Uuid { uuid } => entry.uuid = Some(uuid),
            DeviceEvent::Geometry {
                x, y, make, model, ..
            } => {
                entry.position = (x, y);
                entry.make = Some(make);
                entry.model = Some(model);
            }
            // `fixed` decodes to f64 in wayland-rs.
            DeviceEvent::Scale { factor } => entry.scale = Some(factor),
            DeviceEvent::Enabled { enabled } => entry.enabled = enabled != 0,
            DeviceEvent::Priority { priority } => entry.priority = Some(priority),
            DeviceEvent::CurrentMode { mode } => entry.current_mode = Some(mode.id()),
            DeviceEvent::Mode { mode } => entry.modes.push((mode.id(), mode)),
            DeviceEvent::Done => entry.seen_done = true,
            _ => {}
        }
    }

    // The `mode` event hands us a server-created `kde_output_device_mode_v2`. The opcode is a bare
    // literal (the macro's fragment matcher rejects a `const` in some wayland-client versions); it is
    // pinned to `DEVICE_MODE_EVENT_OPCODE` by `mode_event_opcode_is_two` below.
    event_created_child!(State, OutputDevice, [
        2 => (DeviceMode, ()),
    ]);
}

impl Dispatch<DeviceMode, ()> for State {
    fn event(
        state: &mut Self,
        mode: &DeviceMode,
        event: ModeEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.mode_dims.entry(mode.id()).or_insert((0, 0, 0));
        match event {
            ModeEvent::Size { width, height } => {
                entry.0 = width.max(0) as u32;
                entry.1 = height.max(0) as u32;
            }
            ModeEvent::Refresh { refresh } => entry.2 = refresh.max(0) as u32,
            _ => {}
        }
    }
}

impl Dispatch<OutputConfig, ()> for State {
    fn event(
        state: &mut Self,
        _: &OutputConfig,
        event: ConfigEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // No catch-all: `kde_output_configuration_v2` has exactly these three events, so a
            // future re-vendor of the XML that adds one should fail the build here rather than
            // silently drop it — which is what the old `_ => {}` did (the compiler calls it
            // unreachable today).
            ConfigEvent::Applied => state.applied = Some(true),
            ConfigEvent::Failed => state.applied = Some(false),
            ConfigEvent::FailureReason { reason } => state.failure_reason = Some(reason),
        }
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

/// A connected, bound output-management session on its own Wayland connection.
struct Session {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    next_sync: u32,
}

impl Session {
    /// Connect to the KWin Wayland socket, bind `kde_output_management_v2` + every
    /// `kde_output_device_v2`, and read each output's state — all bounded by `OP_BUDGET`. `None` if
    /// we can't connect, the management global isn't advertised, or the compositor doesn't answer in
    /// budget (the wedge case — the caller then falls back to `kscreen-doctor`).
    fn open() -> Option<Session> {
        let conn = Connection::connect_to_env().ok()?;
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
        // Phase 1: process the registry globals (binds management + every device in the handler).
        if !s.sync_barrier(deadline) {
            return None;
        }
        if s.state.management.is_none() {
            tracing::debug!(
                "KWin does not advertise kde_output_management_v2 to this client — kscreen-doctor \
                 fallback"
            );
            return None;
        }
        // Phase 2: flush the device binds issued in phase 1 and drain each output's state burst
        // (name / enabled / priority / current_mode / mode sizes / done).
        if !s.sync_barrier(deadline) {
            return None;
        }
        // Phase 3 (KWin ≥ 6.7, the registry model): the devices themselves only arrive as the
        // registry's `output` events during phase 2, so their property bursts are one round further
        // out than they are for per-output globals. One more barrier — skipped when no device is
        // still waiting for its `done`, so the classic path costs nothing.
        if s.state.device_registry.is_some()
            && s.state.devices.values().any(|d| !d.seen_done)
            && !s.sync_barrier(deadline)
        {
            return None;
        }
        Some(s)
    }

    /// Send a `wl_display.sync` and pump the queue until its `done` arrives or `deadline` passes.
    /// Returns `true` on the barrier, `false` on timeout.
    fn sync_barrier(&mut self, deadline: Instant) -> bool {
        self.next_sync += 1;
        let serial = self.next_sync;
        let qh = self.queue.handle();
        let _cb = self.conn.display().sync(&qh, serial);
        self.pump_until(deadline, |st| st.sync_done >= serial)
    }

    /// Bounded manual event loop: flush, dispatch what's queued, then poll the connection fd for up
    /// to `POLL_MS` and read. Mirrors the keepalive loop in `kwin.rs::run` (blocking_dispatch can't
    /// be interrupted, so we poll the fd instead). Returns `true` once `done(&state)` holds.
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
            // SAFETY: `&mut pfd` points at one live, fully-initialized `libc::pollfd` on the stack and
            // the count `1` matches that single element, so `poll` reads `fd`/`events` and writes
            // `revents` strictly within `pfd`. `pfd.fd` is the Wayland connection's fd, valid because
            // `self.conn` (and the `prepare_read` guard) outlive the call. `poll` blocks up to
            // `timeout` ms and writes only `revents`; `pfd` is a fresh local that aliases nothing.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let _ = guard.read();
            } // else: timeout/signal — drop the guard, re-check the deadline
        }
    }

    /// A fresh `kde_output_configuration_v2` on this connection.
    fn new_config(&self) -> OutputConfig {
        let qh = self.queue.handle();
        self.state
            .management
            .as_ref()
            .unwrap()
            .create_configuration(&qh, ())
    }

    /// A fresh (empty) `kde_mode_list_v2` on this connection, for a `set_custom_modes` request.
    fn new_mode_list(&self) -> ModeList {
        let qh = self.queue.handle();
        self.state
            .management
            .as_ref()
            .unwrap()
            .create_mode_list(&qh, ())
    }

    /// `apply()` the config and pump until `applied`/`failed` or the deadline. Returns the verdict
    /// (`true` applied, `false` failed/timeout).
    fn apply(&mut self, config: &OutputConfig, deadline: Instant) -> bool {
        self.state.applied = None;
        config.apply();
        let ok = self.pump_until(deadline, |st| st.applied.is_some());
        if !ok {
            return false;
        }
        matches!(self.state.applied, Some(true))
    }

    /// The current-mode size of a device as `(w, h, refresh_mHz)`, if known.
    fn current_dims(&self, dev: &DeviceState) -> Option<(u32, u32, u32)> {
        let id = dev.current_mode.as_ref()?;
        self.state.mode_dims.get(id).copied()
    }
}

/// `(width, height, "WxH@Hz")` capture of a device's current mode, Hz rounded — the same shape the
/// `kscreen-doctor` restore path used, so teardown can put a panel back at its real refresh.
fn mode_spec(dims: (u32, u32, u32)) -> String {
    let hz = ((dims.2 as f64) / 1000.0).round() as u32;
    format!("{}x{}@{}", dims.0, dims.1, hz)
}

/// Prefix EVERY managed KWin output shares (mirrors `kwin::MANAGED_PREFIX`) — the streamed outputs
/// are `Virtual-punktfunk` / `Virtual-punktfunk-<id>`, so a same-family sibling session is never
/// treated as a physical to disable, and its primary is never stolen (first-slot-wins).
const MANAGED_PREFIX: &str = "Virtual-punktfunk";

/// Every head KWin reports, for [`crate::monitors::list`].
///
/// Reuses the same bounded enumerate-only session the topology path opens — no configuration is
/// built and nothing is applied, so this is a pure read. A device that never completed its `done`
/// burst is skipped rather than reported half-read (its geometry would be a guess, and geometry is
/// exactly what callers key on).
pub(crate) fn list_monitors() -> anyhow::Result<Vec<crate::monitors::PhysicalMonitor>> {
    let session = Session::open().ok_or_else(|| {
        anyhow::anyhow!(
            "KWin did not answer kde_output_management_v2 (not a KWin session, the protocol is \
             not advertised to this client, or the compositor is wedged)"
        )
    })?;
    let mut out: Vec<_> = session
        .state
        .devices
        .values()
        .filter(|d| d.seen_done)
        .filter_map(|d| {
            let connector = d.name.clone()?;
            let dims = session.current_dims(d);
            Some(crate::monitors::PhysicalMonitor {
                managed: connector.starts_with(MANAGED_PREFIX),
                description: crate::monitors::describe(
                    d.make.as_deref().unwrap_or(""),
                    d.model.as_deref().unwrap_or(""),
                    &connector,
                ),
                // A disabled output has no current mode — report 0s rather than inventing one.
                width: dims.map(|d| d.0).unwrap_or(0),
                height: dims.map(|d| d.1).unwrap_or(0),
                refresh_mhz: dims.map(|d| d.2).unwrap_or(0),
                x: d.position.0,
                y: d.position.1,
                scale: d.scale.filter(|s| *s > 0.0).unwrap_or(1.0),
                // KWin ranks outputs; 1 is the primary.
                primary: d.priority == Some(1),
                enabled: d.enabled,
                connector,
            })
        })
        .collect();
    // The device globals arrive in bind order, which is not stable across runs; sort by desktop
    // position so a picker (and a log line) reads left-to-right the way the desk looks.
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Make the streamed output (name starts with `our_prefix`, current size `our_w`×`our_h`) the
/// primary — and, for `Exclusive`, disable every other enabled output — over `kde_output_management_v2`.
/// See the module docs for why this is done in-process instead of via `kscreen-doctor`.
pub(crate) fn apply_topology(
    our_prefix: &str,
    our_w: u32,
    our_h: u32,
    kind: TopologyKind,
) -> TopologyOutcome {
    let miss = || TopologyOutcome {
        our_uuid: None,
        disabled: Vec::new(),
        handled: false,
    };
    let Some(mut sess) = Session::open() else {
        return miss();
    };
    let deadline = Instant::now() + OP_BUDGET;

    // Resolve OUR output: managed-prefix name AND current size == the birth size (only the
    // just-created output sits there during a supersede); newest global wins the tie.
    let ours = sess
        .state
        .devices
        .values()
        .filter(|d| {
            d.name.as_deref().is_some_and(|n| n.starts_with(our_prefix))
                && sess.current_dims(d).map(|(w, h, _)| (w, h)) == Some((our_w, our_h))
        })
        .max_by_key(|d| d.global)
        .cloned();
    let Some(ours) = ours else {
        tracing::warn!(
            our_prefix,
            our_w,
            our_h,
            "KWin output management: our virtual output hasn't appeared yet — kscreen-doctor fallback"
        );
        return miss();
    };
    let our_uuid = ours.uuid.clone();
    let our_id = ours.proxy.as_ref().map(|p| p.id());

    // First-slot-wins (§6.1): don't steal primary if another managed sibling already holds it
    // (priority 1) — a 2nd exclusive session joins as a secondary of the shared desktop.
    let sibling_is_primary = sess.state.devices.values().any(|d| {
        d.enabled
            && d.priority == Some(1)
            && d.proxy.as_ref().map(|p| p.id()) != our_id
            && d.name
                .as_deref()
                .is_some_and(|n| n.starts_with(MANAGED_PREFIX))
    });

    // The physical/bootstrap outputs to disable for `Exclusive`: enabled, not any managed sibling,
    // not ours. Captured WITH their current mode so teardown restores the exact refresh.
    let mut to_disable: Vec<(OutputDevice, String, String)> = Vec::new();
    if kind == TopologyKind::Exclusive {
        for d in sess.state.devices.values() {
            let is_ours = d.proxy.as_ref().map(|p| p.id()) == our_id;
            let managed = d
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
            if d.enabled && !is_ours && !managed {
                if let (Some(name), Some(proxy)) = (d.name.clone(), d.proxy.clone()) {
                    let spec = sess.current_dims(d).map(mode_spec).unwrap_or_default();
                    to_disable.push((proxy, name, spec));
                }
            }
        }
    }

    // Build one configuration: ensure ours is enabled, take primary (unless a sibling holds it),
    // disable the others. `apply()` is atomic — KWin re-homes the shell onto the remaining desktop.
    let config = sess.new_config();
    if let Some(proxy) = ours.proxy.as_ref() {
        config.enable(proxy, 1);
        if !sibling_is_primary {
            config.set_primary_output(proxy);
        }
    }
    for (proxy, _, _) in &to_disable {
        config.enable(proxy, 0);
    }
    let applied = sess.apply(&config, deadline);
    config.destroy();

    // Always report the outputs we ASKED to disable so teardown restores them: re-enabling an output
    // that never actually got disabled is a harmless no-op, whereas dropping them here would strand a
    // physical dark if KWin processed the disable but the `applied` ack didn't land in budget.
    let disabled: Vec<(String, String)> = to_disable
        .into_iter()
        .map(|(_, name, spec)| (name, spec))
        .collect();
    if applied {
        tracing::info!(
            also_disabled = ?disabled,
            primary_taken = !sibling_is_primary,
            "KWin output management: streamed output set as the desktop (in-process)"
        );
    } else {
        tracing::warn!(
            reason = ?sess.state.failure_reason,
            also_disabled = ?disabled,
            "KWin output management: apply() not confirmed in budget — proceeding (restore will re-enable)"
        );
    }
    // We resolved our output and drove the config over Wayland; don't ALSO run kscreen-doctor — that
    // would double-apply (and on a wedged box it would just add 26 s of timeouts). `handled` is true
    // even on an unconfirmed apply; a genuinely absent management global / unresolved output took the
    // `handled = false` early returns above and falls back to kscreen-doctor.
    TopologyOutcome {
        our_uuid,
        disabled,
        handled: true,
    }
}

/// Install + select a `want_w`×`want_h`@`want_hz` custom mode on the just-created virtual output
/// (name starts with `our_prefix`, currently at its sacrificial birth size `birth_w`×`birth_h`) —
/// entirely over `kde_output_management_v2`, the in-process replacement for the `kscreen-doctor`
/// `addCustomMode` + `mode` shell-out (`set_custom_refresh`).
///
/// `set_custom_modes` hands KWin a one-entry mode list; KWin generates the CVT timing (so the width
/// may align DOWN — see [`CVT_H_GRANULARITY`]) and adds the mode. We then SELECT it, which changes
/// the output's size and triggers the screencast stream's renegotiation to the real refresh (the
/// sacrificial-birth mechanism in `kwin::create`). Returns the ACTIVE mode read back after selection
/// (Hz rounded), or `None` if management is absent, the output/generated mode never appeared, or an
/// apply didn't confirm — the caller then falls back to `kscreen-doctor`. `set_custom_modes` REPLACES
/// the custom list (idempotent across reconnects — no per-connect list growth), and it is `since 18`,
/// so pre-6.6 KWin without it simply takes the `None` → kscreen-doctor path.
pub(crate) fn set_custom_mode(
    our_prefix: &str,
    birth_w: u32,
    birth_h: u32,
    want_w: u32,
    want_h: u32,
    want_hz: u32,
) -> Option<(u32, u32, u32)> {
    let mut sess = Session::open()?;
    let deadline = Instant::now() + OP_BUDGET;

    // `set_custom_modes` is `since 18`; calling it on an older bound management object is a protocol
    // error, so bail to the kscreen-doctor fallback there (pre-6.6 KWin). Our bound version is
    // `min(advertised, MGMT_MAX)`.
    if sess.state.mgmt_name_version.map(|(_, v)| v).unwrap_or(0) < 18 {
        return None;
    }

    // Resolve our output at its birth size (newest global wins a supersede).
    let our_proxy = sess
        .state
        .devices
        .values()
        .filter(|d| {
            d.name.as_deref().is_some_and(|n| n.starts_with(our_prefix))
                && sess.current_dims(d).map(|(w, h, _)| (w, h)) == Some((birth_w, birth_h))
        })
        .max_by_key(|d| d.global)
        .and_then(|d| d.proxy.clone())?;
    let our_key = our_proxy.id();

    let want_mhz = want_hz.saturating_mul(1000);
    // A generated mode IS our custom one iff: exact height, width at/just-below the request (a CVT
    // alignment), and refresh within 1 Hz — which excludes the sacrificial 60 Hz birth mode.
    let mode_matches = move |st: &State, mid: &ObjectId| -> bool {
        st.mode_dims.get(mid).is_some_and(|&(w, h, mhz)| {
            h == want_h
                && w <= want_w
                && want_w - w < CVT_H_GRANULARITY
                && (mhz as i64 - want_mhz as i64).abs() <= 1000
        })
    };

    // Build a one-entry custom-mode list (full blanking, like kscreen-doctor's `.full`) and install it.
    let mode_list = sess.new_mode_list();
    mode_list.set_resolution(want_w, want_h);
    mode_list.set_refresh_rate(want_mhz);
    mode_list.set_reduced_blanking(0);
    mode_list.add_mode();
    let config = sess.new_config();
    config.set_custom_modes(&our_proxy, &mode_list);
    let installed = sess.apply(&config, deadline);
    config.destroy();
    mode_list.destroy();
    if !installed {
        return None;
    }

    // Wait for KWin to generate the mode and advertise it on the output.
    let found = |st: &State| -> bool {
        st.devices
            .get(&our_key)
            .is_some_and(|d| d.modes.iter().any(|(mid, _)| mode_matches(st, mid)))
    };
    if !sess.pump_until(deadline, found) {
        tracing::warn!(
            want_w,
            want_h,
            want_hz,
            "KWin output management: generated custom mode never appeared — kscreen-doctor fallback"
        );
        return None;
    }

    // Grab the generated mode's proxy, then select it (this is what changes the size).
    let mode_proxy = {
        let dev = sess.state.devices.get(&our_key)?;
        dev.modes
            .iter()
            .find(|(mid, _)| mode_matches(&sess.state, mid))
            .map(|(_, p)| p.clone())?
    };
    let config = sess.new_config();
    config.mode(&our_proxy, &mode_proxy);
    let selected = sess.apply(&config, deadline);
    config.destroy();
    if !selected {
        return None;
    }

    // Read back the active mode after selection — the size that really landed paces the encoder.
    let want_dims = sess
        .state
        .mode_dims
        .get(&mode_proxy.id())
        .map(|&(w, h, _)| (w, h));
    let landed = |st: &State| -> bool {
        st.devices
            .get(&our_key)
            .and_then(|d| d.current_mode.as_ref())
            .and_then(|mid| st.mode_dims.get(mid))
            .map(|&(w, h, _)| (w, h))
            == want_dims
    };
    sess.pump_until(deadline, landed);
    let dev = sess.state.devices.get(&our_key)?;
    let (cw, ch, cmhz) = sess.current_dims(dev)?;
    let hz = ((cmhz as f64) / 1000.0).round() as u32;
    tracing::info!(
        want_w,
        want_h,
        want_hz,
        active_w = cw,
        active_h = ch,
        active_hz = hz,
        "KWin output management: custom mode installed + selected (in-process)"
    );
    Some((cw, ch, hz.max(1)))
}

/// Re-enable outputs by name at their captured `WxH@Hz` modes (teardown), in-process. Returns
/// `true` if the config applied; `false` (compositor unresponsive / management absent) tells the
/// caller to fall back to `kscreen-doctor`.
pub(crate) fn reenable_outputs(outputs: &[(String, String)]) -> bool {
    if outputs.is_empty() {
        return true;
    }
    let Some(mut sess) = Session::open() else {
        return false;
    };
    let deadline = Instant::now() + OP_BUDGET;
    let config = sess.new_config();
    for (name, spec) in outputs {
        // Find the device by name (physical names are stable across a session).
        let Some(dev) = sess
            .state
            .devices
            .values()
            .find(|d| d.name.as_deref() == Some(name.as_str()))
            .cloned()
        else {
            continue;
        };
        let Some(proxy) = dev.proxy.as_ref() else {
            continue;
        };
        // Enable first — a bare enable always succeeds, so a physical is never left dark.
        config.enable(proxy, 1);
        // Then re-assert the captured mode so a 120 Hz panel doesn't return at KWin's ~60 Hz default.
        if let Some(mode) = find_mode(&sess, &dev, spec) {
            config.mode(proxy, &mode);
        }
    }
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if ok {
        tracing::info!(reenabled = ?outputs, "KWin output management: restored outputs (in-process)");
    }
    ok
}

/// Position the output identified by `uuid` at `(x, y)` in the desktop layout, in-process. Returns
/// `true` if applied; `false` tells the caller to fall back to `kscreen-doctor`.
pub(crate) fn set_position(uuid: &str, x: i32, y: i32) -> bool {
    let Some(mut sess) = Session::open() else {
        return false;
    };
    let deadline = Instant::now() + OP_BUDGET;
    let Some(dev) = sess
        .state
        .devices
        .values()
        .find(|d| d.uuid.as_deref() == Some(uuid))
        .cloned()
    else {
        return false;
    };
    let Some(proxy) = dev.proxy.as_ref() else {
        return false;
    };
    let config = sess.new_config();
    config.position(proxy, x, y);
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if ok {
        tracing::info!(
            uuid,
            x,
            y,
            "KWin output management: placed output (in-process)"
        );
    }
    ok
}

/// Find a device's advertised mode proxy matching a captured `"WxH@Hz"` spec (Hz rounded), for
/// restore. `None` if the spec is empty or no mode matches (the caller then enables without a mode
/// and lets KWin pick its preferred one).
fn find_mode(sess: &Session, dev: &DeviceState, spec: &str) -> Option<DeviceMode> {
    if spec.is_empty() {
        return None;
    }
    let (wh, hz) = spec.split_once('@')?;
    let (w, h) = wh.split_once('x')?;
    let (w, h, hz): (u32, u32, u32) = (w.parse().ok()?, h.parse().ok()?, hz.parse().ok()?);
    dev.modes.iter().find_map(|(id, proxy)| {
        let (mw, mh, mmhz) = sess.state.mode_dims.get(id).copied()?;
        let mhz = ((mmhz as f64) / 1000.0).round() as u32;
        ((mw, mh, mhz) == (w, h, hz)).then(|| proxy.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `WxH@Hz` capture rounds mHz to whole Hz — the shape teardown parses back.
    #[test]
    fn mode_spec_rounds_millihertz() {
        assert_eq!(mode_spec((2560, 1440, 59940)), "2560x1440@60");
        assert_eq!(mode_spec((1920, 1080, 60000)), "1920x1080@60");
        assert_eq!(mode_spec((3840, 2160, 119880)), "3840x2160@120");
    }

    /// The vendored device XML must keep `mode` at the opcode the `event_created_child!` macro
    /// hardcodes — a reorder there would bind the child to the wrong event and desync mode sizes.
    #[test]
    fn mode_event_opcode_is_two() {
        assert_eq!(DEVICE_MODE_EVENT_OPCODE, 2);
    }

    /// Same hazard for the registry's `output` event (`finished` is 0, `output` is 1): the
    /// `event_created_child!` binding hardcodes the opcode, so a reorder in the vendored XML would
    /// bind the child to the wrong event and produce outputs with no properties.
    #[test]
    fn registry_output_event_opcode_is_one() {
        assert_eq!(REGISTRY_OUTPUT_EVENT_OPCODE, 1);
    }
}
