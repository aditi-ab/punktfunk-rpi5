//! GNOME/Mutter virtual-display backend via Mutter's *direct* D-Bus APIs (the same path
//! gnome-remote-desktop uses for headless sessions — not the xdg portal, which needs an
//! interactive grant):
//!
//! 1. `org.gnome.Mutter.RemoteDesktop.CreateSession()` → a remote-desktop session (read its
//!    `SessionId`). The cast is anchored to it, and it's also the future input path.
//! 2. `org.gnome.Mutter.ScreenCast.CreateSession({"remote-desktop-session-id": id})`.
//! 3. `ScreenCast.Session.RecordVirtual({"cursor-mode": embedded})` → Mutter creates a **virtual
//!    monitor** and returns a Stream object.
//! 4. `RemoteDesktop.Session.Start()` → the Stream signals `PipeWireStreamAdded(node_id)`.
//!
//! The virtual monitor's *size* follows the PipeWire format negotiation — Mutter adapts it to
//! what the consumer asks for — so the client's exact WxH is plumbed into our consumer's format
//! pod as the preferred size ([`VirtualOutput::preferred_mode`]) rather than passed here.
//! Sessions die with the D-Bus connection, so a keepalive thread owns it (RAII teardown).
//!
//! Requires a running Mutter (`gnome-shell` session, or `gnome-shell --headless` for the
//! headless host) on the session bus. GNOME is detected via `XDG_CURRENT_DESKTOP=GNOME` or
//! forced with `PUNKTFUNK_COMPOSITOR=mutter`.

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const BUS_RD: &str = "org.gnome.Mutter.RemoteDesktop";
const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";
const BUS_DC: &str = "org.gnome.Mutter.DisplayConfig";
/// `ApplyMonitorsConfig` method: 1 = temporary (auto-reverts on the next monitor change —
/// e.g. when our virtual output is torn down — so we never persist a layout to monitors.xml).
const APPLY_TEMPORARY: u32 = 1;

/// Mutter cursor mode: render the cursor into the stream (matches the KWin/gamescope backends).
const CURSOR_EMBEDDED: u32 = 1;

/// The Mutter virtual-display driver. Each [`create`](VirtualDisplay::create) spins up a
/// keepalive thread owning the D-Bus sessions behind the virtual monitor.
pub struct MutterDisplay {
    /// Whether this display is the FIRST of its group (§6.1) — set by the registry before `create`.
    /// A later sibling **extends** into the already-exclusive desktop instead of re-applying the
    /// sole-monitor config (which would disable the first session's virtual). Defaults true (a lone
    /// session establishes topology as before).
    first_in_group: bool,
}

impl MutterDisplay {
    pub fn new() -> Result<Self> {
        Ok(MutterDisplay {
            first_in_group: true,
        })
    }
}

/// Mutter is usable when the host runs inside a GNOME session (its `RecordVirtual` D-Bus API
/// drives the *live* compositor). Cheap signal: `XDG_CURRENT_DESKTOP` names GNOME — same basis
/// as [`super::detect`], avoiding a blocking D-Bus round-trip on the enumeration path.
pub fn is_available() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.to_ascii_uppercase().contains("GNOME"))
        .unwrap_or(false)
}

impl VirtualDisplay for MutterDisplay {
    fn name(&self) -> &'static str {
        "mutter"
    }

    fn set_first_in_group(&mut self, first: bool) {
        self.first_in_group = first;
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let first_in_group = self.first_in_group;
        thread::Builder::new()
            .name("punktfunk-mutter-vout".into())
            .spawn(move || session_thread(setup_tx, stop_thread, mode, first_in_group))
            .context("spawn Mutter virtual-output thread")?;

        let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => bail!("Mutter virtual monitor failed: {e}"),
            Err(_) => bail!("timed out creating the Mutter virtual monitor"),
        };
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            "Mutter virtual monitor ready"
        );
        Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(StopGuard(stop)),
        })
    }
}

/// Dropping this ends the keepalive thread, closing the D-Bus connection — Mutter then tears
/// the remote-desktop + screencast sessions (and the virtual monitor) down.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Keepalive thread: run the D-Bus handshake on a private tokio runtime, report the PipeWire
/// node id, then hold the connection until stopped. `first_in_group` gates the topology change (a
/// non-first sibling extends into the group's already-exclusive desktop instead of re-clobbering it).
fn session_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    mode: Mode,
    first_in_group: bool,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        // Display-management topology (Stage 2): the console policy's level, resolved to a concrete
        // value. `Extend` leaves the virtual output an extension (no config change); `Primary` makes
        // it the primary monitor but keeps the physicals as secondaries; `Exclusive` makes it the
        // SOLE output (physicals disabled). `Auto` never reaches here — it's resolved upstream.
        use crate::vdisplay::policy::Topology;
        let topo = crate::vdisplay::effective_topology();
        let topo_policy = matches!(topo, Topology::Primary | Topology::Exclusive);
        // Group-aware (§6.1): only the FIRST display of the group establishes the topology. A later
        // sibling extends into the already-exclusive desktop — re-applying the sole-monitor config would
        // disable the first session's virtual output (Mutter connectors are un-nameable, so we can't
        // build a config that keeps all group virtuals; skipping is the safe choice). *Concurrent
        // Mutter exclusive is on-glass-validation-pending; the APPLY_TEMPORARY revert when the FIRST
        // session leaves under a live sibling is a documented residual (design §7).*
        let want_config = first_in_group && topo_policy;
        if topo_policy && !first_in_group {
            tracing::info!(
                "mutter: joining an existing display group — extending (the first session owns the \
                 exclusive/primary topology)"
            );
        }
        let exclusive = matches!(topo, Topology::Exclusive);
        // Snapshot the monitor layout BEFORE the virtual output exists (so we can tell the new
        // connector apart and restore on teardown) whenever we're going to touch the topology.
        let dc_pre = if want_config {
            match display_config().await {
                Ok(dc) => match get_state(&dc).await {
                    Ok(state) => Some((dc, state)),
                    Err(e) => {
                        tracing::warn!("mutter: GetCurrentState (pre) failed ({e:#}); leaving displays as-is");
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!("mutter: DisplayConfig unavailable ({e:#}); leaving displays as-is");
                    None
                }
            }
        } else {
            None
        };

        let session = match connect(mode).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        let _ = setup_tx.send(Ok(session.node_id));

        // Make the freshly-created virtual output the PRIMARY monitor so the GNOME shell + new
        // windows land on the surface we stream. Without this, on a host that also has a physical
        // monitor attached, the virtual output is an empty extended desktop — you stream only the
        // wallpaper. Best-effort: any failure just logs and streaming continues unchanged.
        if let Some((dc, pre)) = &dc_pre {
            match make_virtual_primary(dc, mode, pre, exclusive).await {
                Ok(()) => tracing::info!(
                    exclusive,
                    "mutter: virtual output set as the primary monitor (physicals {})",
                    if exclusive { "disabled" } else { "kept" }
                ),
                Err(e) => tracing::warn!(
                    "mutter: could not set the virtual output primary ({e:#}); streaming continues — the desktop may render on the physical monitor"
                ),
            }
        }

        // Park, keeping `session` (and its zbus connection) alive until told to stop.
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Tear down: STOP the screencast so Mutter removes the virtual output. We deliberately do NOT
        // re-assert the physical layout with our own ApplyMonitorsConfig. Issuing a monitor reconfig
        // while the just-removed high-refresh virtual output is still tearing down SIGSEGVs gnome-shell
        // on Mutter 50 + NVIDIA — observed live on home-worker-3: the teardown ApplyMonitorsConfig
        // returned "recipient disconnected from message bus" because the shell crashed mid-call, after
        // which GDM's crash-loop guard dropped to the greeter and wedged EVERY subsequent reconnect.
        // make_virtual_primary applied an APPLY_TEMPORARY config; Mutter reverts that on its own once
        // the virtual output disappears and our DisplayConfig connection (`dc_pre`) closes — so we just
        // drop it here and let the revert happen Mutter-side, never touching the layout ourselves.
        let _ = session.rd_session.call_method("Stop", &()).await;
        drop(dc_pre);
    });
}

/// The live session objects (held for the stream's lifetime) + the PipeWire node id.
struct MutterSession {
    rd_session: zbus::Proxy<'static>,
    _sc_session: zbus::Proxy<'static>,
    _conn: zbus::Connection,
    node_id: u32,
}

/// Run the four-step handshake (see module docs).
async fn connect(mode: Mode) -> Result<MutterSession> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus")?;

    // 1. RemoteDesktop session (the anchor; also the future input path).
    let rd = zbus::Proxy::new(
        &conn,
        BUS_RD,
        "/org/gnome/Mutter/RemoteDesktop",
        "org.gnome.Mutter.RemoteDesktop",
    )
    .await
    .context("RemoteDesktop proxy (is gnome-shell / `gnome-shell --headless` running?)")?;
    let rd_path: OwnedObjectPath = rd
        .call("CreateSession", &())
        .await
        .context("RemoteDesktop.CreateSession")?;
    let rd_session = zbus::Proxy::new(
        &conn,
        BUS_RD,
        rd_path,
        "org.gnome.Mutter.RemoteDesktop.Session",
    )
    .await?;
    let session_id: String = rd_session
        .get_property("SessionId")
        .await
        .context("read SessionId")?;

    // 2. ScreenCast session anchored to it.
    let sc = zbus::Proxy::new(
        &conn,
        BUS_SC,
        "/org/gnome/Mutter/ScreenCast",
        "org.gnome.Mutter.ScreenCast",
    )
    .await
    .context("ScreenCast proxy")?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("remote-desktop-session-id", Value::from(session_id));
    let sc_path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .await
        .context("ScreenCast.CreateSession")?;
    let sc_session = zbus::Proxy::new(
        &conn,
        BUS_SC,
        sc_path,
        "org.gnome.Mutter.ScreenCast.Session",
    )
    .await?;

    // 3. The virtual monitor. For >60 Hz we pin the client's exact WxH@Hz via RecordVirtual's
    // "modes" (explicit size + refresh-rate; Mutter ≥ 47) — validated at 5120×1440@240 on Mutter 50
    // + NVIDIA. At ≤60 Hz we let Mutter derive the refresh from the PipeWire framerate (its 60 Hz
    // default is already correct), so the custom-mode path only runs when it buys something.
    // (A high-refresh virtual CRTC used to SIGSEGV gnome-shell on teardown, which is why this was
    // once gated behind PUNKTFUNK_MUTTER_VIRTUAL_REFRESH; the stop-screencast-before-any-monitor-
    // reconfig teardown below fixed the crash, so pinning the client's refresh is now the default.)
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert("cursor-mode", Value::from(CURSOR_EMBEDDED));
    if mode.refresh_hz > 60 {
        let mut vmode: HashMap<&str, Value> = HashMap::new();
        vmode.insert("size", Value::from((mode.width, mode.height)));
        vmode.insert("refresh-rate", Value::from(mode.refresh_hz as f64));
        vmode.insert("is-preferred", Value::from(true));
        rec.insert("modes", Value::from(vec![vmode]));
    }
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordVirtual", &(rec,))
        .await
        .context("Session.RecordVirtual")?;
    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;

    // 4. Subscribe to the node-id signal BEFORE starting, then start the (combined) session.
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .await
        .context("subscribe PipeWireStreamAdded")?;
    rd_session
        .call_method("Start", &())
        .await
        .context("RemoteDesktop.Session.Start")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("PipeWireStreamAdded did not arrive within 10s"))?
        .ok_or_else(|| anyhow!("signal stream ended before PipeWireStreamAdded"))?;
    let (node_id,): (u32,) = msg
        .body()
        .deserialize()
        .context("PipeWireStreamAdded body")?;

    Ok(MutterSession {
        rd_session,
        _sc_session: sc_session,
        _conn: conn,
        node_id,
    })
}

// ---------------------------------------------------------------------------------------------
// Optional: make the per-session virtual output the PRIMARY monitor (PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY).
//
// `RecordVirtual` adds the virtual monitor as an *extended* desktop. On a headless host that's the
// only display, so the shell + windows live there. But when a physical monitor is attached, GNOME
// keeps it primary and the virtual output is an empty extension — the stream shows only the
// wallpaper. We fix that by promoting the virtual output to primary (physical kept on, secondary)
// via `org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig`, and restore on teardown.
// ---------------------------------------------------------------------------------------------

/// `org.gnome.Mutter.DisplayConfig.GetCurrentState` reply shapes (see the interface XML):
///   monitors:         `a((ssss)a(siiddada{sv})a{sv})`
///   logical_monitors: `a(iiduba(ssss)a{sv})`
type MonitorSpec = (String, String, String, String); // connector, vendor, product, serial
type DbusMode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type MonitorInfo = (MonitorSpec, Vec<DbusMode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<MonitorInfo>,
    Vec<LogicalMonitor>,
    HashMap<String, OwnedValue>,
);

/// `ApplyMonitorsConfig` logical-monitor shape: `(iiduba(ssa{sv}))`, monitor = `(ssa{sv})`.
type ApplyMon = (String, String, HashMap<String, Value<'static>>); // connector, mode_id, props
type ApplyLogical = (i32, i32, f64, u32, bool, Vec<ApplyMon>);

/// A DisplayConfig proxy on its own session-bus connection (owned, so it stays alive for the
/// session — independent of the RemoteDesktop/ScreenCast connection).
async fn display_config() -> Result<zbus::Proxy<'static>> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus (DisplayConfig)")?;
    zbus::Proxy::new(
        &conn,
        BUS_DC,
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .context("DisplayConfig proxy")
}

async fn get_state(dc: &zbus::Proxy<'_>) -> Result<CurrentState> {
    dc.call("GetCurrentState", &())
        .await
        .context("DisplayConfig.GetCurrentState")
}

fn connectors(state: &CurrentState) -> HashSet<String> {
    state.1.iter().map(|m| m.0 .0.clone()).collect()
}

fn mode_flag(md: &DbusMode, key: &str) -> bool {
    matches!(md.6.get(key).map(|v| &**v), Some(&Value::Bool(true)))
}

/// The current (else preferred, else first) mode of `connector` → (mode_id, width, height).
fn current_mode(state: &CurrentState, connector: &str) -> Option<(String, i32, i32)> {
    let mon = state.1.iter().find(|m| m.0 .0 == connector)?;
    let pick = mon
        .1
        .iter()
        .find(|md| mode_flag(md, "is-current"))
        .or_else(|| mon.1.iter().find(|md| mode_flag(md, "is-preferred")))
        .or_else(|| mon.1.first())?;
    Some((pick.0.clone(), pick.1, pick.2))
}

/// Wait for the virtual output to appear in DisplayConfig (its size follows PipeWire negotiation,
/// which lands shortly after the node id), then make it the SOLE primary output (physicals
/// disabled for the session) so the cursor, windows, and keyboard focus stay on the streamed
/// surface. Restored on teardown.
async fn make_virtual_primary(
    dc: &zbus::Proxy<'_>,
    mode: Mode,
    pre: &CurrentState,
    exclusive: bool,
) -> Result<()> {
    let pre_conns = connectors(pre);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let state = get_state(dc).await?;
        // The virtual connector = present now, absent in the pre-snapshot.
        let virt = state
            .1
            .iter()
            .map(|m| m.0 .0.clone())
            .find(|c| !pre_conns.contains(c));
        if let Some(vconn) = virt {
            // Prefer the mode matching the client's WxH; fall back to whatever is current.
            let vmode = state
                .1
                .iter()
                .find(|m| m.0 .0 == vconn)
                .and_then(|m| {
                    m.1.iter()
                        .find(|md| md.1 == mode.width as i32 && md.2 == mode.height as i32)
                        .map(|md| md.0.clone())
                })
                .or_else(|| current_mode(&state, &vconn).map(|(id, _, _)| id));
            let Some(vmode) = vmode else {
                bail!("virtual monitor {vconn} has no usable mode yet");
            };
            // Exclusive: the virtual output alone (physicals omitted → Mutter disables them).
            // Primary: the virtual output primary at (0,0) PLUS the physicals kept as secondaries.
            // (On a headless host with no physicals the two are identical.)
            let config = if exclusive {
                build_exclusive_config(&vconn, &vmode)
            } else {
                build_primary_keeping_physicals(&state, &vconn, &vmode, mode.width as i32)
            };
            let _: () = dc
                .call(
                    "ApplyMonitorsConfig",
                    &(
                        state.0,
                        APPLY_TEMPORARY,
                        config,
                        HashMap::<String, Value<'static>>::new(),
                    ),
                )
                .await
                .context("DisplayConfig.ApplyMonitorsConfig (set virtual primary)")?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the virtual monitor did not appear in DisplayConfig within 6s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// **Exclusive** — the virtual output as the SOLE, primary monitor: physical outputs are omitted, so
/// Mutter disables them for the session. This confines the cursor, windows, and keyboard focus to the
/// streamed surface; keeping the physical enabled as a *secondary* monitor instead lets relative
/// pointer motion and window focus wander onto it (invisible to the client — the cursor seems to
/// vanish). The physical layout is restored on teardown.
fn build_exclusive_config(vconn: &str, vmode: &str) -> Vec<ApplyLogical> {
    vec![(
        0,
        0,
        1.0,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )]
}

/// **Primary** — the virtual output primary at `(0, 0)`, with every currently-active physical
/// monitor KEPT as a secondary (laid left-to-right past the virtual, each at its current mode). So
/// the shell + new windows land on the streamed surface, but the operator's physical screen stays
/// on. On a headless host (no physicals) this is identical to [`build_exclusive_config`].
///
/// *Physical-keep is unvalidated on-glass* — the lab boxes are headless (no attached display to keep
/// on); the layout math is conservative (append to the right) but wants a display-attached box.
fn build_primary_keeping_physicals(
    state: &CurrentState,
    vconn: &str,
    vmode: &str,
    virt_width: i32,
) -> Vec<ApplyLogical> {
    let mut logicals: Vec<ApplyLogical> = vec![(
        0,
        0,
        1.0,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )];
    // Append each physical (non-virtual) connector that has a usable current mode, to the right of
    // the virtual output, as a non-primary secondary.
    let mut x = virt_width.max(0);
    for mon in &state.1 {
        let conn = &mon.0 .0;
        if conn == vconn {
            continue;
        }
        if let Some((mode_id, w, _h)) = current_mode(state, conn) {
            logicals.push((
                x,
                0,
                1.0,
                0,
                false,
                vec![(conn.clone(), mode_id, HashMap::new())],
            ));
            x += w.max(0);
        }
    }
    logicals
}
