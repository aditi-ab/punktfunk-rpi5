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
//!
//! **Per-client scaling** (`identity` policy §5.4): GNOME persists per-monitor scale to
//! `monitors.xml` keyed by connector+vendor+product+**serial**, but Mutter mints a fresh serial
//! (`0x%.6x`, a per-shell counter) for every `RecordVirtual` monitor and the API offers no way to
//! pass a stable identity — so GNOME's own persistence can never rematch our virtual output. The
//! host persists the scale instead ([`identity::ScaleMap`](crate::identity), keyed per
//! client / per the policy): reapplied at connect via the mode's `preferred-scale` plus the
//! topology `ApplyMonitorsConfig`, and the user's mid-session changes are polled from
//! DisplayConfig and written back.

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

/// Mutter cursor mode: ship the pointer as `SPA_META_Cursor` metadata instead of burning it into
/// the frames. The capturer always negotiates the meta (pf-capture `meta_param`) and the encoder
/// blend composites it for sessions where the client does not draw the cursor itself — while a
/// cursor-forwarding session strips the overlay and sends shape/state over the cursor channel.
/// This is the mode for EVERY session whose host plans a composite or forward (`set_hw_cursor`
/// on), channel or not: Mutter's embedded painting is a fiction on a virtual stream — since
/// Mutter 48 (commit `7ff5334a`, hw-cursor inhibition removed) the software cursor overlay is
/// suppressed STAGE-GLOBALLY whenever any physical head realizes a hardware cursor, so
/// dmabuf-recorded frames blit the view without a pointer, and cursor-only motion schedules no
/// re-record either (mutter#4939). Probed on-glass (Mutter 50.3): embedded + relative motion =
/// frozen frame counter; metadata positions kept flowing in the same setup.
const CURSOR_METADATA: u32 = 2;
/// `cursor-mode` embedded (mutter enum 1): Mutter composites the pointer into frames itself.
/// Kept only as the can't-blend fallback (`set_hw_cursor` off — the resolved encode backend
/// cannot composite a metadata cursor, so metadata would strand the pointer in meta nothing
/// draws). Know its limits (above): on a virtual stream it paints only into the MemFd/SHM
/// record path (`FORCE_CURSORS`) and only refreshes on unrelated damage.
const CURSOR_EMBEDDED: u32 = 1;

/// Serializes, process-wide, every Mutter operation that adds/removes a virtual monitor or applies
/// a monitor configuration. Each of these makes Mutter rebuild its monitor topology, and
/// *concurrent* rebuilds have segfaulted gnome-shell on-glass twice now: the teardown-side race is
/// documented at the teardown below, and on 2026-07-10 three simultaneous session setups (three
/// `RecordVirtual` calls within ~200 µs plus an `ApplyMonitorsConfig`) crashed the shell inside
/// `meta_monitor_manager_rebuild` — dropping the box to the GDM greeter until a DM restart. One
/// mutation at a time also keeps [`wait_virtual_connector`] sound: with two virtual outputs
/// appearing at once, "the connector absent from MY pre-snapshot" can name a sibling's monitor.
/// Each session runs on its own dedicated thread (see [`session_thread`]), so blocking on a std
/// mutex — including across the awaits of its single-threaded setup future — is safe.
static TOPOLOGY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The Mutter virtual-display driver. Each [`create`](VirtualDisplay::create) spins up a
/// keepalive thread owning the D-Bus sessions behind the virtual monitor.
pub struct MutterDisplay {
    /// Whether this display is the FIRST of its group (§6.1) — set by the registry before `create`.
    /// A later sibling **extends** into the already-exclusive desktop instead of re-applying the
    /// sole-monitor config (which would disable the first session's virtual). Defaults true (a lone
    /// session establishes topology as before).
    first_in_group: bool,
    /// Out-of-band cursor request (`set_hw_cursor`): metadata cursor-mode at creation; off =
    /// embedded (see [`CURSOR_EMBEDDED`]).
    hw_cursor: bool,
    /// The connecting client's cert fingerprint (set before [`create`](VirtualDisplay::create)) —
    /// keys the per-client persisted **scale** (GNOME can't persist it itself: Mutter mints a fresh
    /// EDID serial per `RecordVirtual` monitor, so `monitors.xml` never rematches; see
    /// [`identity::ScaleMap`](crate::identity)).
    client_fp: Option<[u8; 32]>,
    /// The identity slot the last `create` resolved — reported to the registry via
    /// [`last_identity_slot`](VirtualDisplay::last_identity_slot) to key the group arrangement +
    /// `/display/state` slot, like the KWin backend.
    last_slot: Option<u32>,
}

impl MutterDisplay {
    pub fn new() -> Result<Self> {
        Ok(MutterDisplay {
            first_in_group: true,
            hw_cursor: false,
            client_fp: None,
            last_slot: None,
        })
    }
}

/// Mutter is usable when the host runs inside a GNOME session (its `RecordVirtual` D-Bus API
/// drives the *live* compositor). Cheap env signal, avoiding a blocking D-Bus round-trip on the
/// enumeration path.
///
/// This is the *fallback* answer only: [`crate::available`] treats a running `gnome-shell` (the
/// `/proc` scan) as the authority, because this var belongs to the session and a host launched
/// outside it — `systemd --user`, a TTY, ssh — never inherited it. Deliberately still just the ONE
/// var: `XDG_CURRENT_DESKTOP` is the one [`crate::apply_session_env`] owns end to end (it writes it
/// per connect and *scrubs* it when nothing is live), so sniffing `DESKTOP_SESSION` /
/// `XDG_SESSION_DESKTOP` alongside would resurrect the bug that scrub exists to prevent — a stale
/// `gnome` there after a gnome-shell crash reports Mutter usable and routes the next client into a
/// dead session (45 s create timeouts instead of a crisp handshake error).
///
/// The read takes [`crate::with_env_lock`]: this runs on a management worker (`/host/compositors` →
/// [`crate::available`]) concurrently with another connect's `apply_session_env`, which `set_var`s
/// this key for a live session and `remove_var`s it when nothing is — and a glibc `getenv` racing
/// that is the `environ` realloc data race ENV_LOCK exists for, torn answer at best and a host
/// segfault mid-connect at worst. Read-then-drop; no caller holds the lock (it is not reentrant).
pub fn is_available() -> bool {
    crate::with_env_lock(|| std::env::var("XDG_CURRENT_DESKTOP"))
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

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Identity (§5.4): resolve the client's stable slot per the `identity` policy (Linux
        // defaults to Shared when unconfigured, like KWin) — it keys the registry's group
        // arrangement/state. Mutter can't carry the slot into the monitor's EDID (RecordVirtual
        // owns the identity), so the per-client scaling that policy promises is host-persisted
        // instead: the session thread reapplies the remembered scale and records the user's
        // in-session changes under `scale_key`.
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        let scale_key = crate::identity::scale_key(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        let remembered_scale = crate::identity::scales().lock().unwrap().get(&scale_key);
        if let Some(scale) = remembered_scale {
            tracing::info!(scale, "mutter: reapplying the client's saved display scale");
        }
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let first_in_group = self.first_in_group;
        let hw_cursor = self.hw_cursor;
        thread::Builder::new()
            .name("punktfunk-mutter-vout".into())
            .spawn(move || {
                session_thread(
                    setup_tx,
                    stop_thread,
                    mode,
                    first_in_group,
                    hw_cursor,
                    scale_key,
                    remembered_scale,
                )
            })
            .context("spawn Mutter virtual-output thread")?;

        // Built BEFORE the wait so EVERY error arm below sets the flag on its way out — a thread
        // that finishes after we gave up then parks for at most one 200 ms tick. `report_node` is
        // the primary defence (it stops the session outright); this is the belt-and-braces half,
        // and it also covers a thread that is somewhere else entirely when the timeout fires.
        let guard = StopGuard(stop);

        // 45 s (was 20 s): setups now queue on TOPOLOGY_LOCK, so a session behind a slow sibling
        // (whose guard spans up to a ~10 s stream wait + 6 s connector wait + the apply) must
        // outwait it plus its own handshake before this fires.
        let node_id = match setup_rx.recv_timeout(Duration::from_secs(45)) {
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
        Ok(VirtualOutput::owned(
            node_id,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(guard),
        ))
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
/// `scale_key`/`remembered_scale` carry the per-client persisted scale: reapplied at connect,
/// and the user's in-session changes are recorded back under the key (GNOME itself can't — see
/// [`identity::ScaleMap`](crate::identity)).
// TOPOLOGY_LOCK is deliberately held across the awaits of the setup/teardown sequences: each
// session owns this dedicated OS thread and its own single-future runtime, so the guard never
// blocks a shared executor — it blocks exactly the sibling session threads, which is the point
// (see TOPOLOGY_LOCK).
#[allow(clippy::await_holding_lock)]
fn session_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    mode: Mode,
    first_in_group: bool,
    hw_cursor: bool,
    scale_key: String,
    remembered_scale: Option<f64>,
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
        // The whole setup — pre-snapshot → RecordVirtual → ApplyMonitorsConfig — is one
        // read-modify-write on Mutter's monitor state; hold TOPOLOGY_LOCK across it so concurrent
        // sessions can't interleave rebuilds (gnome-shell SIGSEGV) or poison each other's
        // connector diffs. Released before the keepalive park below.
        let topology_guard = TOPOLOGY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Display-management topology (Stage 2): the console policy's level, resolved to a concrete
        // value. `Extend` leaves the virtual output an extension (no config change); `Primary` makes
        // it the primary monitor but keeps the physicals as secondaries; `Exclusive` makes it the
        // SOLE output (physicals disabled). `Auto` never reaches here — it's resolved upstream.
        use crate::policy::Topology;
        let topo = crate::effective_topology();
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
        // Snapshot the monitor layout BEFORE the virtual output exists — it's how we tell the new
        // connector apart, both for the topology apply and for tracking the scale the user sets on
        // it. Taken unconditionally now (scale tracking wants it even when we won't touch the
        // topology); failure just degrades to no-topology + no-scale-persistence, as before.
        let dc_pre = match display_config().await {
            Ok(dc) => match get_state(&dc).await {
                Ok(state) => Some((dc, state)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "mutter: GetCurrentState (pre) failed; topology + scale persistence off");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "mutter: DisplayConfig unavailable; topology + scale persistence off");
                None
            }
        };

        let session = match connect(mode, hw_cursor, remembered_scale).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        // Report the node id, and STOP if nobody is listening any more. Everything below this line
        // mutates the operator's desktop topology on behalf of a session that, past this point,
        // would have no way to undo it.
        if !report_node(&setup_tx, &session).await {
            return;
        }
        // The send can also LAND in the moment the opener's `recv_timeout` gives up — the value sits
        // in the queue, so the send reports success while `create` is already bailing. That drops
        // the `StopGuard`, so check the flag HERE, before the topology work, rather than only at the
        // park loop below: the point is that a doomed session never applies a sole-monitor config at
        // all, instead of applying one and reverting it a tick later.
        if stop.load(Ordering::Relaxed) {
            tracing::warn!(
                "mutter: the opener gave up as the handshake completed — stopping without touching \
                 the desktop topology"
            );
            let _ = session.rd_session.call_method("Stop", &()).await;
            return;
        }

        // Identify the virtual connector (present now, absent in the pre-snapshot), then — when this
        // session owns the topology — make it the PRIMARY monitor so the GNOME shell + new windows
        // land on the surface we stream. Without this, on a host that also has a physical monitor
        // attached, the virtual output is an empty extended desktop — you stream only the wallpaper.
        // Best-effort: any failure just logs and streaming continues unchanged.
        let mut tracked: Option<(zbus::Proxy<'static>, CurrentState, String)> = None;
        if let Some((dc, pre)) = dc_pre {
            match wait_virtual_connector(&dc, &pre, mode).await {
                Ok((vconn, state)) => {
                    if want_config {
                        match make_virtual_primary(
                            &dc,
                            mode,
                            &pre,
                            &state,
                            &vconn,
                            exclusive,
                            remembered_scale,
                        )
                        .await
                        {
                            Ok(()) => tracing::info!(
                                exclusive,
                                "mutter: virtual output set as the primary monitor (physicals {})",
                                if exclusive { "disabled" } else { "kept" }
                            ),
                            Err(e) => tracing::warn!(
                                error = %format!("{e:#}"),
                                "mutter: could not set the virtual output primary; streaming continues — the desktop may render on the physical monitor"
                            ),
                        }
                    }
                    tracked = Some((dc, pre, vconn));
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "mutter: virtual connector not identified; topology + scale persistence off"
                ),
            }
        }

        drop(topology_guard);

        // Park, keeping `session` (and its zbus connection) alive until told to stop. Every ~5 s,
        // read the virtual output's logical-monitor scale and persist a change the user made (GNOME
        // Settings mid-stream) under the client's key — polled rather than teardown-only so a host
        // crash/redeploy doesn't lose it.
        let mut known = remembered_scale.unwrap_or(1.0);
        let mut ticks: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            ticks = ticks.wrapping_add(1);
            if ticks % 25 == 0 {
                if let Some((dc, _, vconn)) = &tracked {
                    persist_scale_change(dc, vconn, &scale_key, &mut known).await;
                }
            }
        }
        // Final scale read BEFORE Stop (the virtual output must still exist to be read).
        if let Some((dc, _, vconn)) = &tracked {
            persist_scale_change(dc, vconn, &scale_key, &mut known).await;
        }

        // Tear down: STOP the screencast so Mutter removes the virtual output. We deliberately do NOT
        // re-assert the physical layout with our own ApplyMonitorsConfig. Issuing a monitor reconfig
        // while the just-removed high-refresh virtual output is still tearing down SIGSEGVs gnome-shell
        // on Mutter 50 + NVIDIA — observed live on home-worker-3: the teardown ApplyMonitorsConfig
        // returned "recipient disconnected from message bus" because the shell crashed mid-call, after
        // which GDM's crash-loop guard dropped to the greeter and wedged EVERY subsequent reconnect.
        // make_virtual_primary applied an APPLY_TEMPORARY config; Mutter reverts that on its own once
        // the virtual output disappears and our DisplayConfig connection (in `tracked`) closes — so we
        // just drop it here and let the revert happen Mutter-side, never touching the layout ourselves.
        // The Stop (+ the revert it triggers) is a topology mutation too — take TOPOLOGY_LOCK so a
        // sibling's teardown or setup can't interleave with the rebuild it causes.
        let _topology_guard = TOPOLOGY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = session.rd_session.call_method("Stop", &()).await;
        drop(tracked);
    });
}

/// Record an **existing** monitor by connector — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md` L2). Returns the PipeWire node id and the keepalive
/// whose drop stops the recording.
///
/// Same private ScreenCast API as the virtual path, one call different: `RecordMonitor` instead of
/// `RecordVirtual`. So it inherits what makes that path work headlessly — Mutter's *direct* D-Bus
/// API needs no interactive approval, unlike the xdg portal a background service could never answer.
///
/// Deliberately **not** under [`TOPOLOGY_LOCK`]: that lock serializes operations which add/remove a
/// monitor or apply a monitor configuration, and mirroring does neither. It creates nothing, changes
/// no layout, and leaves the head exactly as its owner set it.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let connector_thread = connector.to_string();
    thread::Builder::new()
        .name("punktfunk-mutter-mirror".into())
        .spawn(move || mirror_thread(setup_tx, stop_thread, connector_thread, hw_cursor))
        .context("spawn Mutter monitor-mirror thread")?;
    // As in `create`: built before the wait, so a timeout/failure arm still signals the thread
    // rather than leaving a permanent `RecordMonitor` cast on one of the operator's real heads.
    let guard = MirrorStop(stop);
    let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("Mutter monitor mirror failed: {e}"),
        Err(_) => bail!("timed out recording the Mutter output {connector:?}"),
    };
    Ok(crate::mirror::MirrorStream {
        node_id,
        // Mutter's RecordMonitor node lives on the user's PipeWire daemon (like RecordVirtual).
        remote_fd: None,
        keepalive: Box::new(guard),
    })
}

/// Stops the mirror thread — and with it the recording — when the capturer drops it.
struct MirrorStop(Arc<AtomicBool>);

impl Drop for MirrorStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Owns the D-Bus connection behind a mirrored monitor's cast: connect, hand back the node id,
/// park until stopped, then `Stop` the session.
fn mirror_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    connector: String,
    hw_cursor: bool,
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
        let session = match connect_monitor(&connector, hw_cursor).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        if !report_node(&setup_tx, &session).await {
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Stop the cast. Nothing else to undo: no virtual output was added, so there is no monitor
        // removal for Mutter to rebuild around — the SIGSEGV-adjacent teardown ordering the virtual
        // path has to observe simply doesn't arise here.
        let _ = session.rd_session.call_method("Stop", &()).await;
    });
}

/// The `RecordMonitor` handshake: RemoteDesktop session → ScreenCast session anchored to it →
/// record `connector` → node id on `PipeWireStreamAdded` after `Start`.
async fn connect_monitor(connector: &str, hw_cursor: bool) -> Result<MutterSession> {
    let (conn, rd_session, sc_session) = open_rd_sc().await?;

    // `RecordMonitor(connector, properties) -> stream_path`. Only `cursor-mode` is set: the mode
    // belongs to the monitor's owner, not to us.
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordMonitor", &(connector, rec))
        .await
        .with_context(|| format!("Session.RecordMonitor({connector:?})"))?;

    let session = start_and_await_node(conn, rd_session, sc_session, stream_path).await?;
    tracing::info!(
        connector,
        node_id = session.node_id,
        "mutter: recording an existing monitor"
    );
    Ok(session)
}

/// Steps 1–2 of the handshake (module docs), shared by the virtual (`RecordVirtual`) and the mirror
/// (`RecordMonitor`) paths: session bus → RemoteDesktop session → ScreenCast session anchored to it.
///
/// Hands back the *connection* alongside the two proxies because the connection IS the sessions'
/// lifetime — drop it and Mutter tears them down, which is the whole RAII teardown model here.
async fn open_rd_sc() -> Result<(zbus::Connection, zbus::Proxy<'static>, zbus::Proxy<'static>)> {
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
    Ok((conn, rd_session, sc_session))
}

/// Step 4, shared by both record paths: subscribe to `PipeWireStreamAdded` **before** `Start` (the
/// signal can otherwise land while we are still subscribing), start the combined session, and wait
/// out the node id.
async fn start_and_await_node(
    conn: zbus::Connection,
    rd_session: zbus::Proxy<'static>,
    sc_session: zbus::Proxy<'static>,
    stream_path: OwnedObjectPath,
) -> Result<MutterSession> {
    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;
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

/// Hand the node id back to the opener ([`MutterDisplay::create`] / [`stream_existing_output`]).
///
/// A failed send means the opener ALREADY GAVE UP — its `recv_timeout` fired — so nothing will ever
/// drop this session's keepalive. The thread must unwind here rather than run on and park forever
/// holding the D-Bus connection that *is* the monitor's lifetime. Returns `false` for "stop now".
///
/// The other three portal/D-Bus backends have always done this (`kwin.rs`, `hyprland.rs`,
/// `wlroots.rs` all `?` on the send); Mutter discarded it with `let _ =`, which on a timed-out
/// create left an orphan thread that went on to apply a SOLE-monitor topology — every physical head
/// dark, reverted only when the virtual monitor disappears, which the parked thread prevented.
async fn report_node(setup_tx: &Sender<Result<u32, String>>, session: &MutterSession) -> bool {
    if setup_tx.send(Ok(session.node_id)).is_ok() {
        return true;
    }
    tracing::warn!(
        node_id = session.node_id,
        "mutter: the virtual-output opener gave up before the handshake finished — stopping the \
         session instead of parking on it (a parked session keeps the monitor, and its topology, alive)"
    );
    let _ = session.rd_session.call_method("Stop", &()).await;
    false
}

/// The live session objects (held for the stream's lifetime) + the PipeWire node id.
struct MutterSession {
    rd_session: zbus::Proxy<'static>,
    _sc_session: zbus::Proxy<'static>,
    _conn: zbus::Connection,
    node_id: u32,
}

/// Run the four-step handshake (see module docs). `preferred_scale` is the client's remembered
/// desktop scale, passed as the virtual mode's `preferred-scale` so Mutter creates the monitor
/// already scaled (Mutter ≥ 48; older Mutter ignores unknown mode keys) — this covers the
/// `extend` topology, where we never issue our own ApplyMonitorsConfig.
async fn connect(
    mode: Mode,
    hw_cursor: bool,
    preferred_scale: Option<f64>,
) -> Result<MutterSession> {
    // 1–2. RemoteDesktop session (the anchor; also the future input path) + the ScreenCast session
    // anchored to it.
    let (conn, rd_session, sc_session) = open_rd_sc().await?;

    // 3. The virtual monitor. For >60 Hz we pin the client's exact WxH@Hz via RecordVirtual's
    // "modes" (explicit size + refresh-rate; Mutter ≥ 47) — validated at 5120×1440@240 on Mutter 50
    // + NVIDIA. At ≤60 Hz we let Mutter derive the refresh from the PipeWire framerate (its 60 Hz
    // default is already correct), so the custom-mode path only runs when it buys something.
    // (A high-refresh virtual CRTC used to SIGSEGV gnome-shell on teardown, which is why this was
    // once gated behind PUNKTFUNK_MUTTER_VIRTUAL_REFRESH; the stop-screencast-before-any-monitor-
    // reconfig teardown below fixed the crash, so pinning the client's refresh is now the default.)
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    if mode.refresh_hz > 60 || preferred_scale.is_some() {
        let mut vmode: HashMap<&str, Value> = HashMap::new();
        vmode.insert("size", Value::from((mode.width, mode.height)));
        // Only pin the refresh when it buys something (see above) — a remembered scale alone
        // rides Mutter's 60 Hz default, exactly like the no-modes path did.
        if mode.refresh_hz > 60 {
            vmode.insert("refresh-rate", Value::from(mode.refresh_hz as f64));
        }
        if let Some(scale) = preferred_scale {
            vmode.insert("preferred-scale", Value::from(scale));
        }
        vmode.insert("is-preferred", Value::from(true));
        rec.insert("modes", Value::from(vec![vmode]));
    }
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordVirtual", &(rec,))
        .await
        .context("Session.RecordVirtual")?;

    // 4. Subscribe to the node-id signal BEFORE starting, then start the (combined) session.
    start_and_await_node(conn, rd_session, sc_session, stream_path).await
}

// ---------------------------------------------------------------------------------------------
// Optional: make the per-session virtual output the PRIMARY monitor.
//
// `RecordVirtual` adds the virtual monitor as an *extended* desktop. On a headless host that's the
// only display, so the shell + windows live there. But when a physical monitor is attached, GNOME
// keeps it primary and the virtual output is an empty extension — the stream shows only the
// wallpaper. We fix that by promoting the virtual output via
// `org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig`.
//
// Which shape is `crate::effective_topology()`'s call, not this module's: the console policy first,
// then the legacy `PUNKTFUNK_{KWIN,MUTTER}_VIRTUAL_PRIMARY` env, then the Auto default. `Primary`
// keeps the physicals on as secondaries; `Exclusive` omits them, so Mutter disables them for the
// session; `Extend` skips this block entirely.
//
// Applied at APPLY_TEMPORARY, and **MUTTER ITSELF REVERTS IT** when the virtual monitor disappears
// and our DisplayConfig connection closes. We must never re-assert the layout on teardown: the
// banner used to promise a "restore on teardown" that the teardown deliberately does not do, and
// issuing that ApplyMonitorsConfig is what SIGSEGVed gnome-shell on Mutter 50 + NVIDIA and wedged a
// box at the GDM greeter (see the teardown comment in `session_thread`).
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

/// The current (else preferred, else first) mode of `connector` → `(mode_id, width, height, refresh)`.
fn current_mode_full(state: &CurrentState, connector: &str) -> Option<(String, i32, i32, f64)> {
    let mon = state.1.iter().find(|m| m.0 .0 == connector)?;
    let pick = mon
        .1
        .iter()
        .find(|md| mode_flag(md, "is-current"))
        .or_else(|| mon.1.iter().find(|md| mode_flag(md, "is-preferred")))
        .or_else(|| mon.1.first())?;
    Some((pick.0.clone(), pick.1, pick.2, pick.3))
}

/// As [`current_mode_full`] but dropping the refresh (callers that only place by width).
fn current_mode(state: &CurrentState, connector: &str) -> Option<(String, i32, i32)> {
    current_mode_full(state, connector).map(|(id, w, h, _)| (id, w, h))
}

/// Pure mode-pick for a KEPT physical (unit-tested). Given the physical's PRE-connect mode
/// (`pre_mode = (id, w, h, refresh)`; `None` when the connector is new since the snapshot) and the
/// mode list Mutter reports for it in the POST-virtual state
/// (`(id, w, h, refresh, is_current, is_preferred)`), return the `(mode_id, width, height)` to
/// re-apply. The height is not decoration: a head rotated 90°/270° is as wide on the desktop as its
/// mode is tall, and the caller lays the kept heads out side by side.
///
/// Mutter re-derives its layout when the `RecordVirtual` output appears and can silently drop a
/// 120 Hz panel to its EDID-preferred 60 Hz — so the post-virtual `is-current` is *already* 60 Hz.
/// We therefore prefer the PRE mode (its real refresh), resolved to a mode id valid at apply time;
/// only when the physical genuinely no longer offers that mode do we fall back to the post-virtual
/// current (never inventing a mode id `ApplyMonitorsConfig` would reject).
fn pick_keep_mode(
    pre_mode: Option<(String, i32, i32, f64)>,
    state_modes: &[(String, i32, i32, f64, bool, bool)],
) -> Option<(String, i32, i32)> {
    let state_current = || {
        state_modes
            .iter()
            .find(|m| m.4)
            .or_else(|| state_modes.iter().find(|m| m.5))
            .or_else(|| state_modes.first())
            .map(|m| (m.0.clone(), m.1, m.2))
    };
    let Some((pre_id, w, h, hz)) = pre_mode else {
        return state_current();
    };
    // The exact pre mode id, if the connector still offers it (same session ⇒ usually true).
    if state_modes.iter().any(|m| m.0 == pre_id) {
        return Some((pre_id, w, h));
    }
    // Else a re-keyed id with the same geometry + refresh (still the real 120 Hz).
    if let Some(m) = state_modes
        .iter()
        .find(|m| m.1 == w && m.2 == h && (m.3 - hz).abs() < 0.5)
    {
        return Some((m.0.clone(), m.1, m.2));
    }
    // The physical genuinely no longer offers that mode — use whatever is valid now.
    state_current()
}

/// The `(mode_id, width, height)` a kept physical should be RE-APPLIED at — its PRE-connect mode
/// preserved across Mutter's virtual-output layout re-derive. See [`pick_keep_mode`].
fn physical_keep_mode(
    pre: &CurrentState,
    state: &CurrentState,
    conn: &str,
) -> Option<(String, i32, i32)> {
    let pre_mode = current_mode_full(pre, conn);
    let state_modes: Vec<(String, i32, i32, f64, bool, bool)> = state
        .1
        .iter()
        .find(|m| m.0 .0 == conn)
        .map(|mon| {
            mon.1
                .iter()
                .map(|md| {
                    (
                        md.0.clone(),
                        md.1,
                        md.2,
                        md.3,
                        mode_flag(md, "is-current"),
                        mode_flag(md, "is-preferred"),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    pick_keep_mode(pre_mode, &state_modes)
}

/// Wait for the virtual output to appear in DisplayConfig (its size follows PipeWire negotiation,
/// which lands shortly after the node id) and return its connector name (present now, absent in
/// the pre-snapshot) plus the state that contained it.
async fn wait_virtual_connector(
    dc: &zbus::Proxy<'_>,
    pre: &CurrentState,
    mode: Mode,
) -> Result<(String, CurrentState)> {
    let pre_conns = connectors(pre);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let state = get_state(dc).await?;
        // Everything absent from the pre-snapshot. Normally exactly one — TOPOLOGY_LOCK keeps a
        // SIBLING session out of this window — but a physical hotplug is not ours to serialise, and
        // this window is wide (a ~10 s stream wait plus up to 6 s of polling). Plugging a monitor in
        // during it used to hijack the identity outright, so the session's topology apply and its
        // scale persistence would then be aimed at the operator's new panel.
        let chosen = {
            let fresh: Vec<&MonitorInfo> = state
                .1
                .iter()
                .filter(|m| !pre_conns.contains(&m.0 .0))
                .collect();
            let pick = pick_virtual(&fresh, mode);
            if fresh.len() > 1 {
                tracing::warn!(
                    candidates = ?fresh.iter().map(|m| m.0 .0.as_str()).collect::<Vec<_>>(),
                    chosen = pick.map(|m| m.0 .0.as_str()),
                    want = format!("{}x{}", mode.width, mode.height),
                    "mutter: more than one connector appeared while waiting for the virtual monitor \
                     (a physical hotplug?) — picking the one advertising the client's mode"
                );
            }
            pick.map(|m| m.0 .0.clone())
        };
        if let Some(vconn) = chosen {
            return Ok((vconn, state));
        }
        if Instant::now() >= deadline {
            bail!("the virtual monitor did not appear in DisplayConfig within 6s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Which of the connectors that appeared since the pre-snapshot is OURS.
///
/// Disambiguate on what we actually asked Mutter for: the virtual monitor advertises the client's
/// exact WxH (`make_virtual_primary` matches the same pair to pick its mode). Falling back to "the
/// first new connector" keeps the old behaviour wherever that does not single one out — an older
/// Mutter whose `RecordVirtual` mode list omits the size, or the ordinary case of exactly one.
///
/// Split out of the polling loop so the choice is testable without a session bus.
fn pick_virtual<'a>(fresh: &[&'a MonitorInfo], mode: Mode) -> Option<&'a MonitorInfo> {
    fresh
        .iter()
        .find(|m| {
            m.1.iter()
                .any(|md| md.1 == mode.width as i32 && md.2 == mode.height as i32)
        })
        .or(fresh.first())
        .copied()
}

/// Make the virtual output the primary output — SOLE (`exclusive`: physicals disabled for the
/// session) or with the physicals kept as secondaries — so the cursor, windows, and keyboard focus
/// stay on the streamed surface. Applied at the client's `remembered_scale` (validated against the
/// mode's supported scales; 1.0 when none is remembered) so a saved DPI setting survives the
/// reconnect. Reverted by Mutter on teardown (APPLY_TEMPORARY).
async fn make_virtual_primary(
    dc: &zbus::Proxy<'_>,
    mode: Mode,
    pre: &CurrentState,
    state: &CurrentState,
    vconn: &str,
    exclusive: bool,
    remembered_scale: Option<f64>,
) -> Result<()> {
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
        .or_else(|| current_mode(state, vconn).map(|(id, _, _)| id));
    let Some(vmode) = vmode else {
        bail!("virtual monitor {vconn} has no usable mode yet");
    };
    // The scale to apply. Mutter (≥ its `preferred-scale` support) already derived the virtual's
    // logical monitor at the remembered scale we passed to RecordVirtual, PRE-VALIDATED — preserve
    // that instead of forcing a value (forcing 1.0 here was the original scale-clobber bug). On an
    // older Mutter the derived scale stays 1.0 while a scale is remembered — try the remembered
    // value snapped to an integral logical size (Mutter's fractional-scaling validity rule;
    // GetCurrentState reports NO supported-scales for virtual monitors to snap to), and retry at
    // the derived scale if the whole apply is rejected (an invalid scale fails the entire config —
    // losing the primary switch over scaling would be worse).
    let derived = logical_scale(state, vconn)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0);
    let mut scale = match remembered_scale {
        Some(want) if (want - derived).abs() > 1e-3 => {
            snap_integral_scale(want, mode.width, mode.height)
        }
        _ => derived,
    };
    loop {
        // Exclusive: the virtual output alone (physicals omitted → Mutter disables them).
        // Primary: the virtual output primary at (0,0) PLUS the physicals kept as secondaries.
        // (On a headless host with no physicals the two are identical.)
        let config = if exclusive {
            build_exclusive_config(vconn, &vmode, scale)
        } else {
            build_primary_keeping_physicals(pre, state, vconn, &vmode, mode.width as i32, scale)
        };
        let res: zbus::Result<()> = dc
            .call(
                "ApplyMonitorsConfig",
                &(
                    state.0,
                    APPLY_TEMPORARY,
                    config,
                    HashMap::<String, Value<'static>>::new(),
                ),
            )
            .await;
        match res {
            Ok(()) => return Ok(()),
            Err(e) if (scale - derived).abs() > 1e-3 => {
                tracing::warn!(
                    scale,
                    derived,
                    error = %format!("{e:#}"),
                    "mutter: ApplyMonitorsConfig at the remembered scale failed — retrying at the derived scale"
                );
                scale = derived;
            }
            Err(e) => {
                return Err(e).context("DisplayConfig.ApplyMonitorsConfig (set virtual primary)")
            }
        }
    }
}

/// Snap `want` to the nearest scale that gives the mode an **integral logical size** — Mutter only
/// accepts fractional scales where both `width/scale` and `height/scale` are integers, and its
/// GetCurrentState reports no `supported-scales` for virtual monitors to snap to. Searches the few
/// logical widths around the target for one that keeps the aspect exact; falls back to `want`
/// unchanged (the caller retries at the derived scale if Mutter still rejects it). Pure, unit-tested.
fn snap_integral_scale(want: f64, width: u32, height: u32) -> f64 {
    if !want.is_finite() || want <= 0.0 {
        return 1.0;
    }
    let (w, h) = (width as i64, height as i64);
    let target = (w as f64 / want).round() as i64;
    (target - 8..=target + 8)
        .filter(|lw| *lw >= 1 && (h * lw) % w == 0)
        .map(|lw| w as f64 / lw as f64)
        .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
        .unwrap_or(want)
}

/// The `(scale, transform)` of the logical monitor carrying `connector`. `None` means **no logical
/// monitor carries it** — which is how Mutter reports a head the operator has DISABLED, and is the
/// distinction [`keep_head_layout`] turns into "leave it off".
fn logical_placement(state: &CurrentState, connector: &str) -> Option<(f64, u32)> {
    state
        .2
        .iter()
        .find(|l| l.5.iter().any(|spec| spec.0 == connector))
        .map(|l| (l.2, l.3))
}

/// The scale of the logical monitor carrying `connector`, if present.
fn logical_scale(state: &CurrentState, connector: &str) -> Option<f64> {
    logical_placement(state, connector).map(|(scale, _)| scale)
}

/// Whether a kept physical should be re-applied at all, and with what `(scale, transform)`. Pure —
/// unit-tested, because getting it wrong is invisible on a headless lab box and very visible on the
/// operator's desk.
///
/// The rebuild used to hardcode `scale = 1.0`, `transform = 0` and to list every connector Mutter
/// reported, so one connect un-rotated a portrait panel, dropped a 2×-scaled 4K head to native
/// pixels, and switched a deliberately-dark monitor back on. All three facts are in the PRE-connect
/// snapshot: `pre_logical` is the head's logical-monitor entry there, and Mutter reports a disabled
/// head by omitting it from `logical_monitors` entirely. So: carry the pre values when the head was
/// on; leave it out when the connector existed pre-connect and carried no logical monitor (disabled
/// on purpose); and for a connector that was not in the snapshot at all — a hotplug inside our
/// window — keep it on at whatever Mutter has just derived for it, which is the friendlier reading
/// of "the operator plugged this in while we were connecting".
fn keep_head_layout(
    existed_pre: bool,
    pre_logical: Option<(f64, u32)>,
    state_logical: Option<(f64, u32)>,
) -> Option<(f64, u32)> {
    // A non-finite or non-positive scale would fail the whole ApplyMonitorsConfig, taking the
    // primary switch down with it.
    let sane = |(scale, transform): (f64, u32)| {
        (
            if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
            transform,
        )
    };
    match (pre_logical, existed_pre) {
        (Some(l), _) => Some(sane(l)),
        (None, true) => None,
        (None, false) => Some(sane(state_logical.unwrap_or((1.0, 0)))),
    }
}

/// Every head Mutter reports, for [`crate::monitors::list`].
///
/// A pure `GetCurrentState` read on its own short-lived connection + runtime — no session, no
/// `ApplyMonitorsConfig`, so it never touches the topology and never contends [`TOPOLOGY_LOCK`].
/// Geometry comes from the **logical** monitors (`state.2`), which is the coordinate space that
/// matters (see `crate::monitors`); a monitor absent from every logical monitor is disabled, and is
/// reported as such at the origin rather than dropped.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime (monitor enumeration)")?;
    let state = rt.block_on(async {
        let dc = display_config().await?;
        get_state(&dc).await
    })?;
    let mut out: Vec<_> = state
        .1
        .iter()
        .map(|(spec, _modes, _props)| {
            let (connector, vendor, product, _serial) = spec;
            // The logical monitor carrying this connector: its (x, y), scale and primary flag.
            let logical = state
                .2
                .iter()
                .find(|l| l.5.iter().any(|s| &s.0 == connector));
            let (w, h, refresh) = current_mode_full(&state, connector)
                .map(|(_id, w, h, hz)| (w.max(0) as u32, h.max(0) as u32, (hz * 1000.0) as u32))
                .unwrap_or((0, 0, 0));
            crate::monitors::PhysicalMonitor {
                connector: connector.clone(),
                description: crate::monitors::describe(vendor, product, connector),
                width: w,
                height: h,
                refresh_mhz: refresh,
                x: logical.map(|l| l.0).unwrap_or(0),
                y: logical.map(|l| l.1).unwrap_or(0),
                scale: logical.map(|l| l.2).filter(|s| *s > 0.0).unwrap_or(1.0),
                primary: logical.map(|l| l.4).unwrap_or(false),
                enabled: logical.is_some(),
                // Mutter names a `RecordVirtual` monitor indistinguishably from a physical one —
                // no prefix to key on, and the connector is minted per session. See the field doc.
                managed: false,
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Read the virtual output's current scale and, when the user changed it (GNOME Settings
/// mid-stream), persist it under the client's `scale_key` so the next connect reapplies it.
/// Best-effort: read failures (teardown races, shell restart) are silently skipped.
async fn persist_scale_change(dc: &zbus::Proxy<'_>, vconn: &str, scale_key: &str, known: &mut f64) {
    let Ok(state) = get_state(dc).await else {
        return;
    };
    let Some(cur) = logical_scale(&state, vconn) else {
        return;
    };
    if (cur - *known).abs() > 1e-3 {
        crate::identity::scales()
            .lock()
            .unwrap()
            .set(scale_key, cur);
        *known = cur;
        tracing::info!(
            scale = cur,
            "mutter: persisted the client's display scale for the next connect"
        );
    }
}

/// **Exclusive** — the virtual output as the SOLE, primary monitor: physical outputs are omitted, so
/// Mutter disables them for the session. This confines the cursor, windows, and keyboard focus to the
/// streamed surface; keeping the physical enabled as a *secondary* monitor instead lets relative
/// pointer motion and window focus wander onto it (invisible to the client — the cursor seems to
/// vanish). The physical layout is restored on teardown.
fn build_exclusive_config(vconn: &str, vmode: &str, scale: f64) -> Vec<ApplyLogical> {
    vec![(
        0,
        0,
        scale,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )]
}

/// **Primary** — the virtual output primary at `(0, 0)`, with every physical monitor the operator
/// had ENABLED kept as a secondary (laid left-to-right past the virtual, each at its **pre-connect**
/// mode, scale and transform). So the shell + new windows land on the streamed surface, but the
/// operator's physical screen stays exactly as they left it. On a headless host (no physicals) this
/// is identical to [`build_exclusive_config`].
///
/// `pre` is the snapshot taken *before* the virtual output existed (physical still at its true
/// refresh); `state` is the post-virtual state. Everything about a kept head is read from `pre`,
/// because the post-virtual state is already contaminated: Mutter re-derives the layout when the
/// `RecordVirtual` output appears and can knock a 120 Hz panel down to 60 Hz, so reading `state`
/// would cement that 60 Hz (`physical_keep_mode`). Scale, transform and enabled-ness come from the
/// same snapshot for the same reason — and because rebuilding them from scratch is what used to
/// un-rotate portrait panels, flatten a 2× scale and re-light a head the operator had switched off
/// ([`keep_head_layout`]).
///
/// *Physical-keep is unvalidated on-glass* — the lab boxes are headless (no attached display to keep
/// on); the layout math is conservative (append to the right) but wants a display-attached box.
fn build_primary_keeping_physicals(
    pre: &CurrentState,
    state: &CurrentState,
    vconn: &str,
    vmode: &str,
    virt_width: i32,
    scale: f64,
) -> Vec<ApplyLogical> {
    let mut logicals: Vec<ApplyLogical> = vec![(
        0,
        0,
        scale,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )];
    // Append each physical (non-virtual) connector that has a usable mode, to the right of the
    // virtual output, as a non-primary secondary — at its PRE-connect mode (real refresh preserved).
    // Offsets are in the layout's coordinate space: LOGICAL pixels by default on Wayland (the
    // virtual's footprint is width/scale), physical pixels only under layout-mode 2.
    let physical_layout = matches!(
        state.3.get("layout-mode").map(|v| &**v),
        Some(&Value::U32(2))
    );
    let virt_logical_width = if physical_layout {
        virt_width
    } else {
        ((virt_width as f64 / scale).round() as i32).max(1)
    };
    let mut x = virt_logical_width.max(0);
    for mon in &state.1 {
        let conn = &mon.0 .0;
        if conn == vconn {
            continue;
        }
        let existed_pre = pre.1.iter().any(|m| m.0 .0 == *conn);
        let Some((head_scale, transform)) = keep_head_layout(
            existed_pre,
            logical_placement(pre, conn),
            logical_placement(state, conn),
        ) else {
            // Omitted from the config ⇒ Mutter leaves it disabled, which is what the operator asked
            // for. Listing it would switch their dark head on for the length of the session.
            tracing::debug!(
                connector = %conn,
                "mutter: this head was disabled before the session — leaving it disabled"
            );
            continue;
        };
        if let Some((mode_id, w, h)) = physical_keep_mode(pre, state, conn) {
            logicals.push((
                x,
                0,
                head_scale,
                transform,
                false,
                vec![(conn.clone(), mode_id, HashMap::new())],
            ));
            // Advance by the head's own LOGICAL footprint, in the layout's coordinate space — the
            // same space the virtual's advance above uses. A 3840-wide panel at scale 2 occupies
            // 1920, and a head rotated 90°/270° (transform 1/3, or their flipped twins 5/7) is as
            // wide as its mode is TALL. Advancing by raw mode width was only ever *consistent* with
            // the forced scale of 1.0 this rebuild used to apply; preserving the real scale without
            // this would just trade one wrong layout for another (overlapping or gapped heads).
            let rotated = matches!(transform, 1 | 3 | 5 | 7);
            let footprint = if rotated { h } else { w };
            x += if physical_layout {
                footprint.max(0)
            } else {
                ((footprint as f64 / head_scale).round() as i32).max(0)
            };
        }
    }
    logicals
}

#[cfg(test)]
mod tests {
    use super::{
        keep_head_layout, pick_keep_mode, pick_virtual, snap_integral_scale, HashMap, Mode,
        MonitorInfo,
    };

    // (id, w, h, refresh, is_current, is_preferred)
    fn m(
        id: &str,
        w: i32,
        h: i32,
        hz: f64,
        cur: bool,
        pref: bool,
    ) -> (String, i32, i32, f64, bool, bool) {
        (id.to_string(), w, h, hz, cur, pref)
    }

    #[test]
    fn keep_mode_prefers_pre_refresh_over_downgraded_state() {
        // Physical was 2560x1440@120 pre-connect; after the virtual appeared Mutter marked 60 Hz
        // current (the reported bug). We must re-apply the 120 Hz mode, not the state's 60 Hz.
        let pre = Some(("M120".to_string(), 2560, 1440, 120.0));
        let state = vec![
            m("M120", 2560, 1440, 120.0, false, false),
            m("M60", 2560, 1440, 60.0, true, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("M120".to_string(), 2560, 1440))
        );
    }

    #[test]
    fn keep_mode_rekeyed_id_matches_by_geometry_and_refresh() {
        // The pre id is no longer offered (Mutter re-keyed the mode list), but a 120 Hz mode of the
        // same geometry exists — match it so the real refresh survives.
        let pre = Some(("old-120".to_string(), 2560, 1440, 120.0));
        let state = vec![
            m("new-120", 2560, 1440, 119.998, false, false),
            m("new-60", 2560, 1440, 60.0, true, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("new-120".to_string(), 2560, 1440))
        );
    }

    #[test]
    fn keep_mode_falls_back_to_state_current_when_pre_mode_gone() {
        // The physical genuinely no longer offers its pre mode (e.g. cable renegotiated to a lower
        // max) — never invent an id; use the post-virtual current.
        let pre = Some(("gone-165".to_string(), 3440, 1440, 165.0));
        let state = vec![
            m("s-100", 3440, 1440, 100.0, true, false),
            m("s-60", 3440, 1440, 60.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("s-100".to_string(), 3440, 1440))
        );
    }

    #[test]
    fn snap_integral_scale_keeps_valid_scales_and_snaps_odd_ones() {
        // Already-integral scales survive exactly: 1920/1.5 = 1280, 1080/1.5 = 720.
        assert_eq!(snap_integral_scale(1.5, 1920, 1080), 1.5);
        // The GNOME fractional 1.6666… on 3840x2400 (logical 2304x1440) survives.
        let s = snap_integral_scale(1.666_666_6, 3840, 2400);
        assert!((s - 3840.0 / 2304.0).abs() < 1e-9, "got {s}");
        // A scale with no integral logical size nearby snaps to the closest one that has it:
        // 16:9 logical widths must be multiples of 16 → 1.3 snaps to 1920/1472.
        let s = snap_integral_scale(1.3, 1920, 1080);
        assert!((s - 1920.0 / 1472.0).abs() < 1e-9, "got {s}");
        // Junk input degrades to 1.0.
        assert_eq!(snap_integral_scale(f64::NAN, 1920, 1080), 1.0);
        assert_eq!(snap_integral_scale(-2.0, 1920, 1080), 1.0);
    }

    #[test]
    fn keep_mode_no_pre_uses_state_current_then_preferred() {
        // A connector new since the pre-snapshot (no pre mode): is-current wins, else is-preferred.
        let state = vec![
            m("A", 1920, 1080, 60.0, true, false),
            m("B", 1920, 1080, 144.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(None, &state),
            Some(("A".to_string(), 1920, 1080))
        );

        let no_current = vec![
            m("A", 1920, 1080, 60.0, false, false),
            m("B", 1920, 1080, 144.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(None, &no_current),
            Some(("B".to_string(), 1920, 1080))
        );
    }

    /// A kept physical must come back exactly as the operator had it. Rebuilding the layout from
    /// scratch (`scale = 1.0`, `transform = 0`, every connector listed) un-rotated portrait panels,
    /// flattened a 2× scale, and switched a deliberately-dark head back on the moment a client
    /// connected — while the code went to real trouble to preserve the refresh.
    #[test]
    fn a_kept_head_carries_its_pre_connect_scale_and_transform() {
        // Rotated + 2×-scaled, exactly as it was before the virtual output appeared.
        assert_eq!(
            keep_head_layout(true, Some((2.0, 1)), Some((1.0, 0))),
            Some((2.0, 1))
        );
        // Disabled on purpose (present pre-connect, carried by no logical monitor) — stays off.
        assert_eq!(keep_head_layout(true, None, Some((1.0, 0))), None);
        // Hotplugged inside our window: not in the snapshot at all, so keep it on at whatever
        // Mutter derived rather than disabling a monitor the operator just plugged in.
        assert_eq!(
            keep_head_layout(false, None, Some((1.5, 2))),
            Some((1.5, 2))
        );
        assert_eq!(keep_head_layout(false, None, None), Some((1.0, 0)));
        // A junk scale would fail the WHOLE ApplyMonitorsConfig, taking the primary switch with it.
        assert_eq!(keep_head_layout(true, Some((0.0, 3)), None), Some((1.0, 3)));
        assert_eq!(
            keep_head_layout(true, Some((f64::NAN, 0)), None),
            Some((1.0, 0))
        );
    }

    /// Build a `MonitorInfo` advertising exactly the given modes.
    fn mon(connector: &str, modes: &[(i32, i32)]) -> MonitorInfo {
        (
            (
                connector.to_string(),
                "vendor".into(),
                "product".into(),
                "serial".into(),
            ),
            modes
                .iter()
                .map(|&(w, h)| {
                    (
                        format!("{w}x{h}"),
                        w,
                        h,
                        60.0,
                        1.0,
                        vec![1.0],
                        HashMap::new(),
                    )
                })
                .collect(),
            HashMap::new(),
        )
    }

    const M: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };

    /// The ordinary case: one new connector, and it is ours whether or not the size matches (an
    /// older Mutter may not advertise it).
    #[test]
    fn a_lone_new_connector_is_ours() {
        let only = mon("VIRTUAL-1", &[(3840, 2160)]);
        assert_eq!(
            pick_virtual(&[&only], M).map(|m| m.0 .0.as_str()),
            Some("VIRTUAL-1")
        );
    }

    /// A physical hotplug lands in the same window (TOPOLOGY_LOCK only keeps SIBLING sessions out).
    /// The client's exact mode is what tells the two apart — without it the hotplugged panel used to
    /// win whenever it sorted first, and the session's topology apply and scale persistence were
    /// then aimed at the operator's monitor.
    #[test]
    fn a_hotplug_does_not_steal_the_identity() {
        let hotplug = mon("DP-3", &[(2560, 1440), (3840, 2160)]);
        let ours = mon("VIRTUAL-1", &[(1920, 1080)]);
        assert_eq!(
            pick_virtual(&[&hotplug, &ours], M).map(|m| m.0 .0.as_str()),
            Some("VIRTUAL-1"),
            "the connector advertising the client's mode must win regardless of order"
        );
    }

    /// Ambiguous — nothing advertises the client's size. Falling back to the first keeps the old
    /// behaviour rather than failing the session; the caller logs a warning when it happens.
    #[test]
    fn with_no_mode_match_the_first_still_wins() {
        let a = mon("DP-3", &[(2560, 1440)]);
        let b = mon("VIRTUAL-1", &[(3840, 2160)]);
        assert_eq!(
            pick_virtual(&[&a, &b], M).map(|m| m.0 .0.as_str()),
            Some("DP-3")
        );
    }

    #[test]
    fn nothing_new_is_nothing_to_pick() {
        assert!(pick_virtual(&[], M).is_none());
    }

    /// Live GNOME round trip — the on-glass lever for this backend, same convention as the Windows
    /// backend's `live_create_drop`. Exercises the real four-step handshake end to end: create the
    /// virtual monitor, hold it, then drop the keepalive and let the RAII teardown revert it.
    ///
    /// Needs a running `gnome-shell` on the session bus. Run it as:
    /// ```text
    /// PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY=0 \
    ///   cargo test -p pf-vdisplay -- --ignored --nocapture live_mutter_create_drop
    /// ```
    /// **Set that variable** unless you mean to exercise the exclusive topology: without it the
    /// default resolves to `Exclusive`, which disables the box's physical heads for the duration
    /// (they come back on teardown — that is what this test also proves, but on a box you are
    /// sitting at, do it deliberately).
    #[test]
    #[ignore = "needs a live gnome-shell on the session bus; run with --ignored"]
    fn live_mutter_create_drop() {
        use super::{MutterDisplay, VirtualDisplay};

        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let mut vd = MutterDisplay::new().expect("construct the Mutter backend");
        let started = std::time::Instant::now();
        let out = vd.create(mode).expect("create the Mutter virtual monitor");
        println!(
            "created: node_id={} preferred={:?} in {:?}",
            out.node_id,
            out.preferred_mode,
            started.elapsed()
        );
        assert!(out.node_id > 0, "a real PipeWire node id");
        assert_eq!(
            out.preferred_mode,
            Some((mode.width, mode.height, mode.refresh_hz))
        );

        std::thread::sleep(std::time::Duration::from_secs(3));
        drop(out);
        // The keepalive's Drop only SIGNALS the thread; give it more than one 200 ms tick to run
        // the Stop + topology revert before the harness exits and takes the process with it.
        std::thread::sleep(std::time::Duration::from_secs(2));
        println!("dropped — gnome-shell should have removed the monitor and reverted the topology");
    }
}
