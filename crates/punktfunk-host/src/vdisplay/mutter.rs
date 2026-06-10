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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const BUS_RD: &str = "org.gnome.Mutter.RemoteDesktop";
const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";

/// Mutter cursor mode: render the cursor into the stream (matches the KWin/gamescope backends).
const CURSOR_EMBEDDED: u32 = 1;

/// The Mutter virtual-display driver. Each [`create`](VirtualDisplay::create) spins up a
/// keepalive thread owning the D-Bus sessions behind the virtual monitor.
pub struct MutterDisplay;

impl MutterDisplay {
    pub fn new() -> Result<Self> {
        Ok(MutterDisplay)
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

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        thread::Builder::new()
            .name("punktfunk-mutter-vout".into())
            .spawn(move || session_thread(setup_tx, stop_thread))
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
/// node id, then hold the connection until stopped.
fn session_thread(setup_tx: Sender<Result<u32, String>>, stop: Arc<AtomicBool>) {
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
        let session = match connect().await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        let _ = setup_tx.send(Ok(session.node_id));
        // Park, keeping `session` (and its zbus connection) alive until told to stop.
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Best-effort explicit teardown before the connection drops.
        let _ = session.rd_session.call_method("Stop", &()).await;
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
async fn connect() -> Result<MutterSession> {
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

    // 3. The virtual monitor. Size/refresh follow the PipeWire format negotiation.
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert("cursor-mode", Value::from(CURSOR_EMBEDDED));
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
