//! KWin virtual-output backend via the privileged `zkde_screencast_unstable_v1` Wayland
//! protocol (the mechanism KRdp / krfb-virtualmonitor use).
//!
//! `stream_virtual_output(name, width, height, scale, pointer)` asks KWin to create a new output
//! sized to exactly `width`x`height`, rendered natively (no scaling), and hands back a PipeWire
//! node for it. The node lives on the user's default PipeWire daemon, so [`VirtualOutput::remote_fd`]
//! is `None` and capture connects to that daemon directly.
//!
//! Requirements: KWin must expose the privileged `zkde_screencast` global. It is a *restricted*
//! protocol — KWin advertises it only to a client whose installed `.desktop` lists it under
//! `X-KDE-Wayland-Interfaces` (KWin maps the connecting client to a `.desktop` by resolving
//! `/proc/<pid>/exe` against `Exec=`, then caches the grant per-executable for the session's life).
//! So an interactive Plasma session does NOT hand it to a bare client — the host packages ship
//! `io.unom.Punktfunk.Host.desktop` (`Exec=/usr/bin/punktfunk-host`,
//! `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,…`) so it is present before the host first
//! connects. The headless test path instead exposes it to bare clients via
//! `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. The compositor backend must implement
//! `createVirtualOutput`: the **DRM backend** (any version) or the **VirtualBackend since KWin
//! 6.5.6** (`kwin_wayland --virtual`); on `--virtual` < 6.5.6 the request fails with
//! "Could not find output". We talk raw Wayland on `$WAYLAND_DISPLAY`, so the host must run inside
//! the KWin session's environment.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// Generate the client bindings for the vendored protocol XML inline (no build.rs). Path is
// relative to CARGO_MANIFEST_DIR. See wayland-rs' "implementing a custom protocol" docs.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod zkde {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use zkde::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1 as ScreencastStream,
};
use zkde::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1 as Screencast;

/// `pointer` attachment mode (the protocol enum): render the cursor into the stream so the
/// remote sees it move with injected input.
const POINTER_EMBEDDED: u32 = 2;

/// The name we give the created output; KWin exposes it to output-management as `Virtual-<name>`.
const VOUT_NAME: &str = "punktfunk";

/// Highest interface version we drive. KWin currently advertises 5; we rely on the `created`
/// event (deprecated only since v6) for the node id, so cap the bind at 5.
const MAX_VERSION: u32 = 5;

/// The KWin virtual-display driver. Carries the connecting client's cert fingerprint (set before
/// [`create`](VirtualDisplay::create)) so a paired client gets a STABLE per-slot output NAME
/// (`Virtual-punktfunk-<id>`) — KWin persists per-output config (scale/mode) keyed by name in
/// `kwinoutputconfig.json`, so a stable name makes KDE reapply that client's scaling on reconnect
/// (Stage 3). Each `create` spins up its own Wayland connection/thread that owns the output.
#[derive(Default)]
pub struct KwinDisplay {
    client_fp: Option<[u8; 32]>,
    /// The identity slot the last [`create`](VirtualDisplay::create) resolved (the per-client id, or
    /// `None` for shared/anonymous) — reported to the registry via [`last_identity_slot`] so it can key
    /// the group arrangement + `/display/state` slot to the same id this backend named the output with.
    last_slot: Option<u32>,
    /// The base output name the last `create` used (`punktfunk` / `punktfunk-<id>`) — so
    /// [`apply_position`](VirtualDisplay::apply_position) can address the KWin output `Virtual-<name>`.
    last_name: Option<String>,
}

impl KwinDisplay {
    pub fn new() -> Result<Self> {
        Ok(KwinDisplay::default())
    }
}

impl VirtualDisplay for KwinDisplay {
    fn name(&self) -> &'static str {
        "kwin"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn apply_position(&mut self, x: i32, y: i32) {
        let Some(name) = self.last_name.clone() else {
            return;
        };
        let output = format!("Virtual-{name}");
        // kscreen-doctor position syntax: `output.<name>.position.<x>,<y>`.
        let ok = std::process::Command::new("kscreen-doctor")
            .arg(format!("output.{output}.position.{x},{y}"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            tracing::info!(output, x, y, "KWin: placed output in the desktop layout");
        } else {
            tracing::warn!(output, x, y, "KWin: output position apply failed");
        }
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Per-slot output name (Stage 3): the `identity` policy resolves the client to a stable id →
        // `punktfunk-<id>` (KWin exposes `Virtual-punktfunk-<id>`, whose per-output config KWin
        // persists by name). Shared / anonymous → the base `punktfunk` (today's single name). Linux
        // defaults to Shared when unconfigured, so this is a no-op change until a policy opts in — AND
        // it fixes the latent clash where two concurrent sessions both used `Virtual-punktfunk`.
        let slot = crate::vdisplay::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::vdisplay::policy::Identity::Shared,
        );
        self.last_slot = slot; // reported to the registry for the group arrangement + state slot
        let name = match slot {
            Some(id) => format!("{VOUT_NAME}-{id}"),
            None => VOUT_NAME.to_string(),
        };
        self.last_name = Some(name.clone()); // for apply_position (registry-driven §6.2 layout)
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (width, height) = (mode.width, mode.height);
        let name_thread = name.clone();
        thread::Builder::new()
            .name("punktfunk-kwin-vout".into())
            .spawn(move || virtual_output_thread(width, height, name_thread, setup_tx, stop_thread))
            .context("spawn KWin virtual-output thread")?;

        let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => bail!("KWin virtual output failed: {e}"),
            Err(_) => bail!("timed out creating the KWin virtual output"),
        };
        tracing::info!(node_id, width, height, "KWin virtual output ready");
        // KWin creates virtual outputs at a hardcoded 60 Hz and `stream_virtual_output` has no
        // refresh argument, so above 60 Hz we install + select a custom mode (supported on virtual
        // outputs since KWin 6.6) before capture connects PipeWire, so the stream negotiates at the
        // higher rate. First cut shells out to kscreen-doctor; the in-process
        // kde_output_management_v2 client is a follow-up. `set_custom_refresh` reads back and
        // returns what KWin *actually* achieved so the encoder paces to the real source rate (a
        // rejected custom mode leaves the output at 60 Hz). At ≤60 Hz there's nothing to install —
        // the source runs 60 Hz and the encoder downsamples — so carry the requested rate through.
        let achieved_hz = if mode.refresh_hz > 60 {
            set_custom_refresh(width, height, mode.refresh_hz, &name)
        } else {
            mode.refresh_hz
        };
        // Display-management topology (Stage 2): `Extend` leaves the streamed output an extension;
        // `Primary` makes it the primary output but keeps the bootstrap/physical outputs enabled;
        // `Exclusive` makes it the SOLE desktop (others disabled, restored on teardown) — so
        // plasmashell + windows land on the streamed surface, not the headless `kwin --virtual`
        // bootstrap output. Read from the policy (replacing the PUNKTFUNK_KWIN_VIRTUAL_PRIMARY boolean).
        use crate::vdisplay::policy::Topology;
        let restore = match crate::vdisplay::effective_topology() {
            Topology::Exclusive => apply_virtual_primary(&name),
            Topology::Primary => {
                apply_virtual_primary_only(&name);
                Vec::new() // nothing disabled → nothing to restore
            }
            Topology::Extend | Topology::Auto => Vec::new(),
        };
        // Layout position (§6.2) is applied by the registry via `apply_position` right after create
        // (it owns the display group, so it computes auto-row / manual placement over the whole group).
        Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, achieved_hz)),
            keepalive: Box::new(StopGuard { stop, restore }),
        })
    }
}

/// Best-effort: raise the just-created virtual output's refresh above KWin's default 60 Hz by
/// installing + selecting a custom mode via `kscreen-doctor` (the output is `Virtual-<VOUT_NAME>`,
/// refresh given in mHz), then **read back the active mode** and return the refresh KWin actually
/// gave us. The apply command can report success yet leave the output at 60 Hz (mode rejected),
/// and a silent rate mismatch surfaces downstream as judder / duplicated frames — so the caller
/// paces the encoder to the *achieved* rate, not the requested one.
fn set_custom_refresh(width: u32, height: u32, hz: u32, name: &str) -> u32 {
    let output = format!("Virtual-{name}");
    let mhz = hz.saturating_mul(1000);
    let run = |arg: String| {
        std::process::Command::new("kscreen-doctor")
            .arg(arg)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // Add the custom mode (a fresh output has none), then select it.
    let _ = run(format!(
        "output.{output}.addCustomMode.{width}.{height}.{mhz}.full"
    ));
    let applied = run(format!("output.{output}.mode.{width}x{height}@{hz}"));
    match read_active_refresh(&output) {
        Some(achieved) if achieved >= hz => {
            tracing::info!(
                output,
                requested = hz,
                achieved,
                "KWin virtual output: custom refresh applied"
            );
            achieved
        }
        Some(achieved) => {
            tracing::warn!(
                output,
                requested = hz,
                achieved,
                applied,
                "KWin virtual output refresh below requested — pacing the encoder to the achieved \
                 rate (custom-mode install rejected? is kscreen-doctor up to date?)"
            );
            achieved.max(1)
        }
        None => {
            tracing::warn!(
                output,
                requested = hz,
                applied,
                "could not read back KWin virtual output refresh — assuming 60 Hz (is \
                 kscreen-doctor installed?)"
            );
            60
        }
    }
}

/// Read the active refresh (Hz, rounded) of `output` from `kscreen-doctor -j`. `None` if the
/// tool, the output, or its current mode can't be found. Mode/output ids come through as either
/// JSON strings or numbers depending on the KWin version, so both are accepted.
fn read_active_refresh(output: &str) -> Option<u32> {
    let out = std::process::Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .ok()?;
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let o = doc
        .get("outputs")?
        .as_array()?
        .iter()
        .find(|o| o.get("name").and_then(|n| n.as_str()) == Some(output))?;
    let current = o.get("currentModeId").and_then(as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(as_id).as_deref() == Some(current.as_str()))?;
    let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?;
    Some(hz.round() as u32)
}

/// The prefix EVERY managed KWin output shares — Stage 3 names them `punktfunk` / `punktfunk-<id>`,
/// which KWin exposes as `Virtual-punktfunk` / `Virtual-punktfunk-<id>`. Group membership (§6.1) is
/// recognised by this prefix, so we never have to thread the live set through the backend.
const MANAGED_PREFIX: &str = "Virtual-punktfunk";

/// Names of currently-ENABLED outputs that are **not managed by us** — the headless session's
/// bootstrap output(s) + any physical monitor, i.e. exactly what `exclusive` must disable.
/// **Group-aware (§6.1):** excludes the WHOLE managed family (the [`MANAGED_PREFIX`]), not just this
/// session's own output — so a 2nd `exclusive` session (with a distinct per-slot name) never disables
/// the 1st session's live output. Parsed from `kscreen-doctor -j` (same source as [`read_active_refresh`]).
fn other_enabled_outputs() -> Vec<String> {
    let out = match std::process::Command::new("kscreen-doctor")
        .arg("-j")
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
                .filter_map(|o| o.get("name").and_then(|n| n.as_str()))
                .filter(|n| !n.starts_with(MANAGED_PREFIX))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// True if any managed group member (the [`MANAGED_PREFIX`] family) is ALREADY the KWin primary —
/// first-slot-wins support (§6.1) so a later exclusive session doesn't steal primary from the group's
/// first member. Best-effort: if kscreen reports no primary flag we treat it as "none" (the session
/// then sets itself primary — the pre-group behavior). Recent kscreen marks the primary with
/// `"priority": 1`; older builds used a `"primary": true` bool — accept either.
fn a_managed_output_is_primary() -> bool {
    let Ok(out) = std::process::Command::new("kscreen-doctor").arg("-j").output() else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return false;
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter().any(|o| {
                let managed = o
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
                let primary = o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1);
                managed && primary
            })
        })
        .unwrap_or(false)
}

/// Set `Virtual-punktfunk` primary and disable the bootstrap output(s) so the managed group becomes
/// the sole desktop (KWin re-homes plasmashell + windows onto it). Returns the disabled outputs for
/// the keepalive to re-enable on teardown. Best-effort: on failure, streaming continues (just possibly
/// showing only the wallpaper) rather than failing the session.
fn apply_virtual_primary(name: &str) -> Vec<String> {
    let ours = format!("Virtual-{name}");
    let kscreen = |args: &[String]| {
        std::process::Command::new("kscreen-doctor")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // First-slot-wins (§6.1): only grab primary if no managed group member is primary yet — so a 2nd
    // exclusive session joins as a secondary monitor of the shared desktop instead of stealing the
    // shell off the 1st session's output. KWin usually then re-homes the desktop + disables the
    // bootstrap on its own; the belt-and-suspenders disable below covers the rest.
    if !a_managed_output_is_primary() {
        if !kscreen(&[format!("output.{ours}.primary")]) {
            tracing::warn!(
                "KWin: could not set the virtual output primary; client may see only the wallpaper"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Disable everything still enabled that ISN'T a managed group member (bootstrap / physical), so
    // the group is unambiguously the desktop — never a sibling session's output (group-aware filter).
    let others = other_enabled_outputs();
    if !others.is_empty() {
        let args: Vec<String> = others
            .iter()
            .map(|o| format!("output.{o}.disable"))
            .collect();
        let _ = kscreen(&args);
    }
    tracing::info!(also_disabled = ?others, "KWin: streamed output set as the sole desktop");
    others
}

/// **Primary** (Stage 2): make the streamed output the primary but KEEP the other outputs enabled
/// (don't disable the bootstrap/physical) — so the shell re-homes onto the streamed surface while a
/// physical screen stays usable. Nothing to restore on teardown (we disabled nothing).
fn apply_virtual_primary_only(name: &str) {
    let ours = format!("Virtual-{name}");
    let ok = std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{ours}.primary"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        tracing::info!("KWin: streamed output set primary (physical outputs kept)");
    } else {
        tracing::warn!("KWin: could not set the virtual output primary");
    }
}

/// Dropping this releases the KWin virtual output: it flips the keepalive thread's `stop`, which
/// drops the Wayland connection and makes KWin reclaim the output.
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Bootstrap output(s) `apply_virtual_primary` disabled to make our streamed output the sole
    /// desktop — re-enabled here FIRST, so KWin is never left with zero enabled outputs as our
    /// output is reclaimed. Empty unless PUNKTFUNK_KWIN_VIRTUAL_PRIMARY is set.
    restore: Vec<String>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.restore.is_empty() {
            let args: Vec<String> = self
                .restore
                .iter()
                .map(|o| format!("output.{o}.enable"))
                .collect();
            let _ = std::process::Command::new("kscreen-doctor")
                .args(&args)
                .status();
            std::thread::sleep(Duration::from_millis(200));
        }
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct State {
    screencast: Option<Screencast>,
    node_id: Option<u32>,
    failed: Option<String>,
    closed: bool,
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
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == Screencast::interface().name {
                let v = version.min(MAX_VERSION);
                state.screencast = Some(registry.bind::<Screencast, _, _>(name, v, qh, ()));
            }
        }
    }
}

// The manager has no events.
impl Dispatch<Screencast, ()> for State {
    fn event(
        _: &mut Self,
        _: &Screencast,
        _: zkde::zkde_screencast_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ScreencastStream, ()> for State {
    fn event(
        state: &mut Self,
        _: &ScreencastStream,
        event: StreamEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => state.node_id = Some(node),
            StreamEvent::Failed { error } => state.failed = Some(error),
            StreamEvent::Closed => state.closed = true,
            // `serial` (v6) — we use the node id from `created`, so ignore.
            _ => {}
        }
    }
}

/// Worker thread: create a `width`x`height` virtual output on KWin, send its PipeWire node id
/// back over `setup_tx`, then keep the Wayland connection alive (so the output isn't destroyed)
/// until `stop` is set. Mirrors the portal thread's "park to keep the session alive".
fn virtual_output_thread(
    width: u32,
    height: u32,
    name: String,
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = run(width, height, &name, &setup_tx, &stop) {
        // If we never delivered a node id, report the failure to the waiting opener.
        let _ = setup_tx.send(Err(format!("{e:#}")));
    }
}

/// Readiness probe: connect to the KWin Wayland socket, roundtrip the registry, and confirm
/// the privileged `zkde_screencast` global is actually advertised. This is exactly what
/// [`run`] needs before it can create a virtual output, so a session-bringup script can poll
/// this to gate on the compositor being *ready* (not merely the socket existing) instead of
/// racing it with a blind sleep. `Ok(())` = ready; `Err` = not ready / no global yet.
pub fn probe() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state).context("registry roundtrip")?;
    if state.screencast.is_none() {
        bail!(
            "KWin is up but does not expose zkde_screencast_unstable_v1 to this client — KWin gates \
             it on the host's .desktop X-KDE-Wayland-Interfaces (install \
             io.unom.Punktfunk.Host.desktop with Exec=/usr/bin/punktfunk-host, then re-login so KWin \
             re-reads it — the grant is cached per-exe on first connect), or set \
             KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 for the headless test; needs KWin ≥ 6.5.6"
        );
    }
    Ok(())
}

/// KWin is usable iff we're inside a KWin session exposing `zkde_screencast` — exactly what
/// [`probe`] checks, surfaced as a bool for compositor enumeration.
pub fn is_available() -> bool {
    probe().is_ok()
}

fn run(
    width: u32,
    height: u32,
    name: &str,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state).context("registry roundtrip")?;

    let screencast = state.screencast.clone().ok_or_else(|| {
        anyhow!(
            "KWin does not expose zkde_screencast_unstable_v1 to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it, or run KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 (headless test)"
        )
    })?;

    // Create the virtual output sized to the client, cursor composited into the stream.
    let stream = screencast.stream_virtual_output(
        name.to_string(),
        width as i32,
        height as i32,
        1.0, // scale (logical == physical)
        POINTER_EMBEDDED,
        &qh,
        (),
    );
    tracing::info!(
        width,
        height,
        "KWin: requested virtual output; awaiting PipeWire node"
    );

    // Pump events until KWin reports the node id (or an error).
    let node_id = loop {
        queue
            .blocking_dispatch(&mut state)
            .context("wayland dispatch (awaiting created)")?;
        if let Some(node) = state.node_id {
            break node;
        }
        if let Some(e) = state.failed.take() {
            bail!("stream_virtual_output failed: {e}");
        }
        if state.closed {
            bail!("KWin closed the stream before it was created");
        }
    };
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("virtual-output opener went away"))?;

    // Keep the connection (and thus the virtual output) alive until told to stop, observing
    // `closed`. blocking_dispatch can't be interrupted, so poll the connection fd with a short
    // timeout so `stop` is honored within ~200 ms.
    while !stop.load(Ordering::Relaxed) {
        queue
            .dispatch_pending(&mut state)
            .context("dispatch_pending")?;
        if state.closed {
            tracing::warn!("KWin closed the virtual-output stream");
            break;
        }
        conn.flush().context("wayland flush")?;
        let Some(guard) = conn.prepare_read() else {
            continue; // events already queued — loop dispatches them
        };
        let mut pfd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `&mut pfd` points at a single live, fully-initialized `libc::pollfd` on the stack, and
        // the count `1` matches that one-element array, so `poll` reads `fd`/`events` and writes `revents`
        // strictly within `pfd`. `pfd.fd` is the Wayland connection's fd, valid because `conn` (and the
        // `prepare_read` guard) are alive across the call. `poll` blocks up to 200 ms and writes only
        // `revents`; `pfd` outlives the synchronous call and aliases nothing (a fresh local).
        let r = unsafe { libc::poll(&mut pfd, 1, 200) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = guard.read();
        } // else: timeout or signal — drop the guard, re-check `stop`
    }

    // Best-effort clean teardown; dropping the connection also makes KWin reclaim the output.
    stream.close();
    let _ = conn.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::MANAGED_PREFIX;

    /// Group-aware exclusive (§6.1): with two managed group members + a physical panel enabled,
    /// exclusive disables ONLY the non-managed panel — never a sibling session's per-slot output
    /// (the Stage-3 naming would otherwise make a 2nd exclusive session black out the 1st).
    #[test]
    fn exclusive_disables_only_non_managed() {
        let enabled = [
            "Virtual-punktfunk",   // base name (shared identity)
            "Virtual-punktfunk-1", // client A's per-slot output
            "Virtual-punktfunk-7", // client B's per-slot output
            "eDP-1",               // a physical panel
        ];
        let to_disable: Vec<&str> = enabled
            .iter()
            .copied()
            .filter(|n| !n.starts_with(MANAGED_PREFIX))
            .collect();
        assert_eq!(to_disable, vec!["eDP-1"]);
    }
}
