//! KWin virtual-output backend via the privileged `zkde_screencast_unstable_v1` Wayland
//! protocol (the mechanism KRdp / krfb-virtualmonitor use).
//!
//! `stream_virtual_output(name, width, height, scale, pointer)` asks KWin to create a new output
//! sized to exactly `width`x`height`, rendered natively (no scaling), and hands back a PipeWire
//! node for it. The node lives on the user's default PipeWire daemon, so [`VirtualOutput::remote_fd`]
//! is `None` and capture connects to that daemon directly.
//!
//! Requirements: KWin must expose the privileged `zkde_screencast` global — a real Plasma session
//! authorizes it for its own clients; the headless test exposes it to bare clients via
//! `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. The compositor backend must implement
//! `createVirtualOutput`: the **DRM backend** (any version) or the **VirtualBackend since KWin
//! 6.5.6** (`kwin_wayland --virtual`); on `--virtual` < 6.5.6 the request fails with
//! "Could not find output". We talk raw Wayland on `$WAYLAND_DISPLAY`, so the host must run inside
//! the KWin session's environment.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]

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

/// Highest interface version we drive. KWin currently advertises 5; we rely on the `created`
/// event (deprecated only since v6) for the node id, so cap the bind at 5.
const MAX_VERSION: u32 = 5;

/// The KWin virtual-display driver. Stateless — each [`create`](VirtualDisplay::create) spins up
/// its own Wayland connection/thread that owns the resulting output.
pub struct KwinDisplay;

impl KwinDisplay {
    pub fn new() -> Result<Self> {
        Ok(KwinDisplay)
    }
}

impl VirtualDisplay for KwinDisplay {
    fn name(&self) -> &'static str {
        "kwin"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (width, height) = (mode.width, mode.height);
        thread::Builder::new()
            .name("lumen-kwin-vout".into())
            .spawn(move || virtual_output_thread(width, height, setup_tx, stop_thread))
            .context("spawn KWin virtual-output thread")?;

        let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => bail!("KWin virtual output failed: {e}"),
            Err(_) => bail!("timed out creating the KWin virtual output"),
        };
        tracing::info!(node_id, width, height, "KWin virtual output ready");
        Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            keepalive: Box::new(StopGuard(stop)),
        })
    }
}

/// Dropping this releases the KWin virtual output: it flips the keepalive thread's `stop`, which
/// drops the Wayland connection and makes KWin reclaim the output.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
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
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = run(width, height, &setup_tx, &stop) {
        // If we never delivered a node id, report the failure to the waiting opener.
        let _ = setup_tx.send(Err(format!("{e:#}")));
    }
}

fn run(
    width: u32,
    height: u32,
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
            "KWin does not expose zkde_screencast_unstable_v1 (need a real KDE session, or run \
             KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 for the headless test)"
        )
    })?;

    // Create the virtual output sized to the client, cursor composited into the stream.
    let stream = screencast.stream_virtual_output(
        "lumen".to_string(),
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
