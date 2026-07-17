//! `ext-data-control-v1` clipboard backend (`design/clipboard-and-file-transfer.md` §4.1).
//!
//! A dedicated thread owns the `wayland-client` [`EventQueue`] and runs a poll loop that dispatches
//! selection + paste events, emitting them over a channel. Everything else — installing a lazy
//! source (a client's offer) and `receive()`-ing the host selection (a client's fetch) — is issued
//! from the session thread on the shared, `Send + Sync` proxy handles; only *dispatch* is
//! single-threaded (per the wayland-client contract). Templated on `inject/linux/wlr.rs`.
//!
//! The `zwlr-data-control-unstable-v1` fallback for older wlroots/KWin is a mechanical parallel of
//! this file (the protocols are 1:1) — a follow-up.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use super::{ClipEvent, PasteResponder};

/// Upper bound on bytes read from one `receive()` transfer (matches the wire clipboard cap, §7) so a
/// hostile host app can't stream unboundedly into our buffer.
const CLIP_READ_CAP: u64 = 64 << 20;

/// The current host selection, shared between the dispatch thread (writer) and the session thread
/// (reader, for `receive()`).
struct CurrentSelection {
    offer: ExtDataControlOfferV1,
    /// Raw Wayland MIMEs the offer advertises (what `receive()` accepts).
    mimes: Vec<String>,
}

/// Dispatch-thread state. Also collects the manager + seat during the bind roundtrip.
struct State {
    mgr: Option<ExtDataControlManagerV1>,
    seat: Option<WlSeat>,
    /// Offers accumulating their MIME list before the `selection` event promotes one.
    pending: HashMap<ObjectId, Vec<String>>,
    current: Arc<Mutex<Option<CurrentSelection>>>,
    /// Pending count of our own `set_selection`s whose `selection` echo must be dropped rather than
    /// announced back to the client (loop prevention, §3.4). Bumped by the session before each set;
    /// each of our sets produces exactly one echo on wlroots/KWin, so one decrement per echo pairs
    /// them up — a counter (not a bool) keeps rapid back-to-back offers from leaking a self-echo.
    suppress_echoes: Arc<AtomicU32>,
    tx: tokio::sync::mpsc::UnboundedSender<ClipEvent>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
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
            match interface.as_str() {
                "ext_data_control_manager_v1" => {
                    state.mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// Manager + seat emit nothing we consume.
impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _dev: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            // A new offer is being introduced; its `offer` events follow before `selection`.
            Event::DataOffer { id } => {
                state.pending.insert(id.id(), Vec::new());
            }
            // The active selection changed. `Some` = a new clipboard; `None` = cleared.
            Event::Selection { id } => {
                // Consume one pending self-echo if any (atomic vs. the session thread's bumps; the
                // dispatch thread is the only decrementer). `Ok` = there was one → suppress.
                let suppressed = state
                    .suppress_echoes
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_sub(1))
                    .is_ok();
                match id {
                    Some(offer) => {
                        let mimes = state.pending.remove(&offer.id()).unwrap_or_default();
                        if suppressed {
                            // Our own source's echo — don't store it as the host clipboard and
                            // don't announce it back to the client.
                            return;
                        }
                        let wire = super::offer_wire_mimes(&mimes)
                            .into_iter()
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        *state.current.lock().unwrap() = Some(CurrentSelection { offer, mimes });
                        let _ = state.tx.send(ClipEvent::Selection { mimes: wire });
                    }
                    None => {
                        *state.current.lock().unwrap() = None;
                        if !suppressed {
                            let _ = state.tx.send(ClipEvent::Selection { mimes: Vec::new() });
                        }
                    }
                }
            }
            Event::Finished => {
                let _ = state.tx.send(ClipEvent::Closed);
            }
            // Primary selection is out of scope for the shared clipboard.
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Some(list) = state.pending.get_mut(&offer.id()) {
                list.push(mime_type);
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _src: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_source_v1::Event;
        match event {
            // A host app pasted our (the client's) offered data.
            Event::Send { mime_type, fd } => match super::wayland_to_wire(&mime_type) {
                Some(wire) => {
                    let _ = state.tx.send(ClipEvent::Paste {
                        mime: wire.to_string(),
                        responder: PasteResponder::Fd(fd),
                    });
                }
                // We can't satisfy this format — closing the fd yields an empty paste.
                None => drop(fd),
            },
            // Our source was superseded (a host app or another client set a new selection).
            Event::Cancelled => {}
            _ => {}
        }
    }
}

/// The host clipboard backend handle used by the session thread.
pub struct ClipboardBackend {
    conn: Connection,
    mgr: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    qh: QueueHandle<State>,
    current: Arc<Mutex<Option<CurrentSelection>>>,
    suppress_echoes: Arc<AtomicU32>,
    active_source: Mutex<Option<ExtDataControlSourceV1>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardBackend {
    /// Connect to the active session's Wayland display (env already applied by
    /// `vdisplay::apply_session_env`), bind `ext_data_control`, and start the dispatch thread.
    /// Returns the handle plus the event stream. Errors if the compositor lacks the protocol
    /// (caller reports `BackendUnavailable`).
    pub fn open() -> Result<(
        ClipboardBackend,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        let conn = Connection::connect_to_env()
            .context("connect to Wayland for clipboard (WAYLAND_DISPLAY/XDG_RUNTIME_DIR set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let current = Arc::new(Mutex::new(None));
        let suppress_echoes = Arc::new(AtomicU32::new(0));
        let mut state = State {
            mgr: None,
            seat: None,
            pending: HashMap::new(),
            current: current.clone(),
            suppress_echoes: suppress_echoes.clone(),
            tx,
        };
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let mgr = state
            .mgr
            .clone()
            .context("compositor lacks ext_data_control_manager_v1")?;
        let seat = state
            .seat
            .clone()
            .context("compositor advertised no wl_seat")?;
        let device = mgr.get_data_device(&seat, &qh, ());
        // Second roundtrip: the compositor sends the initial selection for the freshly-bound device
        // (the current host clipboard), which the session announces to the client.
        queue
            .roundtrip(&mut state)
            .context("Wayland get_data_device roundtrip")?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let conn = conn.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("punktfunk-clipboard".into())
                .spawn(move || dispatch_loop(conn, queue, state, stop))
                .context("spawn clipboard dispatch thread")?
        };

        Ok((
            ClipboardBackend {
                conn,
                mgr,
                device,
                qh,
                current,
                suppress_echoes,
                active_source: Mutex::new(None),
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// Install a lazy source advertising a client's offered formats (wire MIMEs) as the host
    /// selection. A later host-app paste fires a [`ClipEvent::Paste`]. Replaces any previous offer.
    pub fn set_offer(&self, wire_mimes: &[String]) -> Result<()> {
        let wl_mimes = super::wayland_offers_for(wire_mimes);
        if wl_mimes.is_empty() {
            return self.clear_offer();
        }
        let src = self.mgr.create_data_source(&self.qh, ());
        for m in &wl_mimes {
            src.offer(m.clone());
        }
        // Suppress the selection echo our own set triggers (loop prevention).
        self.suppress_echoes.fetch_add(1, Ordering::SeqCst);
        self.device.set_selection(Some(&src));
        self.conn.flush().context("flush set_selection")?;
        let mut slot = self.active_source.lock().unwrap();
        if let Some(old) = slot.take() {
            old.destroy();
        }
        *slot = Some(src);
        Ok(())
    }

    /// Drop the host selection we own (client disabled sync / offered nothing).
    pub fn clear_offer(&self) -> Result<()> {
        let mut slot = self.active_source.lock().unwrap();
        if let Some(old) = slot.take() {
            self.suppress_echoes.fetch_add(1, Ordering::SeqCst);
            self.device.set_selection(None);
            old.destroy();
            self.conn.flush().context("flush clear selection")?;
        }
        Ok(())
    }

    /// The current host selection's wire MIMEs (what a client offer announcement would carry), or
    /// empty if the clipboard is empty. Used to answer an immediate query.
    pub fn current_wire_mimes(&self) -> Vec<String> {
        match self.current.lock().unwrap().as_ref() {
            Some(sel) => super::offer_wire_mimes(&sel.mimes)
                .into_iter()
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Read one format (`wire_mime`) of the current host selection into a byte vector — a client's
    /// lazy fetch. BLOCKS on the pipe until the source app finishes, so call from a blocking
    /// context (e.g. `spawn_blocking`). Errors if there is no selection or the format isn't offered.
    pub fn read_current(&self, wire_mime: &str) -> Result<Vec<u8>> {
        let (offer, wl_mime) = {
            let cur = self.current.lock().unwrap();
            let sel = cur.as_ref().context("no current host selection")?;
            let wl = super::pick_wayland_mime(wire_mime, &sel.mimes)
                .context("format not offered by the host clipboard")?;
            (sel.offer.clone(), wl)
        };
        let (read_fd, write_fd) = make_pipe()?;
        offer.receive(wl_mime, write_fd.as_fd());
        self.conn.flush().context("flush receive")?;
        // Close our write end so the pipe reaches EOF once the source app closes its dup.
        drop(write_fd);
        let mut buf = Vec::new();
        // `read_fd` is a fresh, uniquely-owned pipe read end; `File` takes sole ownership and closes
        // it on drop.
        let file = std::fs::File::from(read_fd);
        file.take(CLIP_READ_CAP)
            .read_to_end(&mut buf)
            .context("read clipboard transfer")?;
        Ok(buf)
    }
}

impl Drop for ClipboardBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The dispatch thread: poll the Wayland socket with a short timeout so `stop` is honored promptly,
/// dispatching selection/paste events into `state`.
fn dispatch_loop(
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    mut state: State,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        if queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        if conn.flush().is_err() {
            break;
        }
        let Some(guard) = conn.prepare_read() else {
            // Events are already queued; loop to dispatch them.
            continue;
        };
        let raw_fd = guard.connection_fd().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd: raw_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a single valid pollfd; `poll` reads/writes exactly it for 200 ms.
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            drop(guard);
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR — recheck stop, retry
            }
            break;
        }
        if rc == 0 {
            drop(guard); // timeout — recheck stop
            continue;
        }
        if pfd.revents & libc::POLLIN != 0 {
            if guard.read().is_err() {
                break;
            }
        } else {
            drop(guard); // POLLHUP / POLLERR — connection gone
            break;
        }
    }
    let _ = state.tx.send(ClipEvent::Closed);
}

/// Create a `pipe2(O_CLOEXEC)`, returning `(read_end, write_end)` as owned fds.
fn make_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe2` fully initializes the 2-element `fds` on success (returns 0); on failure (-1)
    // we bail before reading it. Each returned fd is fresh and owned by exactly one `OwnedFd`.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc < 0 {
        return Err(anyhow!("pipe2 failed: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `fds[0]`/`fds[1]` are the fresh, uniquely-owned pipe ends from the checked `pipe2`.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above for the write end.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

/// On-glass tests against a **live** `data-control` compositor (Hyprland / Sway / KWin). `#[ignore]`d
/// — run explicitly under such a session with `wl-clipboard` present:
///
/// ```text
/// WAYLAND_DISPLAY=wayland-1 cargo test -p punktfunk-host --bin punktfunk-host \
///     -- --ignored --nocapture clipboard::wayland::live
/// ```
///
/// Each test skips (does not fail) when `open()` finds no backend — so `--ignored` on GNOME (no
/// data-control) or a headless CI runner is a clean no-op instead of a false failure.
#[cfg(test)]
mod live {
    use super::*;
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Poll the event channel (sync `try_recv`, no runtime) until `pred` matches or `timeout`.
    fn wait_event(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&ClipEvent) -> bool,
    ) -> Option<ClipEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// Set the compositor selection from a "host app" (`wl-copy`, which forks a server that holds it).
    fn wl_copy(bytes: &[u8], mime: &str) {
        let mut child = Command::new("wl-copy")
            .arg("--type")
            .arg(mime)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn wl-copy");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(bytes)
            .expect("write to wl-copy");
        let _ = child.wait(); // foreground exits; the fork keeps serving
        std::thread::sleep(Duration::from_millis(150));
    }

    fn open_or_skip() -> Option<(
        ClipboardBackend,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        if Command::new("wl-copy").arg("--version").output().is_err() {
            eprintln!("SKIP: wl-clipboard not installed");
            return None;
        }
        match ClipboardBackend::open() {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("SKIP: no data-control backend on this compositor: {e:#}");
                None
            }
        }
    }

    /// Host copy → we observe a `Selection` and can `read_current` the exact bytes back — both text
    /// and PNG (§3.5 format normalization end to end).
    #[test]
    #[ignore = "needs a live data-control compositor (WAYLAND_DISPLAY)"]
    fn live_host_copy_is_readable() {
        let Some((backend, mut rx)) = open_or_skip() else {
            return;
        };

        // Text.
        wl_copy(b"hello-from-host-app", "text/plain;charset=utf-8");
        let ev = wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Selection { mimes } if mimes.iter().any(|m| m == super::super::WIRE_TEXT))
        })
        .expect("Selection event carrying text after wl-copy");
        assert!(matches!(ev, ClipEvent::Selection { .. }));
        assert_eq!(
            backend.read_current(super::super::WIRE_TEXT).unwrap(),
            b"hello-from-host-app"
        );

        // PNG (arbitrary bytes tagged image/png — data-control is format-agnostic).
        let png = b"\x89PNG\r\n\x1a\n-fake-but-tagged-image/png";
        wl_copy(png, "image/png");
        wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Selection { mimes } if mimes.iter().any(|m| m == super::super::WIRE_PNG))
        })
        .expect("Selection event carrying image/png");
        assert_eq!(backend.read_current(super::super::WIRE_PNG).unwrap(), png);
    }

    /// We install a client's offer as the host selection; a host app (`wl-paste`) pasting it fires a
    /// `Paste` event that we fulfill with bytes, and the host app receives exactly those bytes.
    #[test]
    #[ignore = "needs a live data-control compositor (WAYLAND_DISPLAY)"]
    fn live_set_offer_is_pasteable() {
        let Some((backend, mut rx)) = open_or_skip() else {
            return;
        };

        backend
            .set_offer(&[super::super::WIRE_TEXT.to_string()])
            .expect("install offer");

        // A host app pastes our offered selection.
        let child = Command::new("wl-paste")
            .arg("-n")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn wl-paste");

        let paste = wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Paste { .. })
        })
        .expect("Paste event after wl-paste reads our offer");
        match paste {
            ClipEvent::Paste { mime, responder } => {
                assert_eq!(
                    mime,
                    super::super::WIRE_TEXT,
                    "paste requested the text format"
                );
                match responder {
                    PasteResponder::Fd(fd) => {
                        super::super::fulfill_paste(fd, b"served-by-punktfunk").expect("fulfill");
                    }
                    PasteResponder::Channel(_) => panic!("data-control paste must carry an fd"),
                }
            }
            _ => unreachable!(),
        }

        let out = child.wait_with_output().expect("wl-paste output");
        assert_eq!(out.stdout, b"served-by-punktfunk");
    }
}
