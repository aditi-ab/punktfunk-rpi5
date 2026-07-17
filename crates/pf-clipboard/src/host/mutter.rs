//! GNOME clipboard backend via Mutter's **direct** `org.gnome.Mutter.RemoteDesktop.Session` D-Bus
//! API (`design/clipboard-and-file-transfer.md` §4.1).
//!
//! Mutter implements no wlr/ext `data-control` (a deliberate privacy stance), so [`super::wayland`]
//! can't bind on GNOME. But Mutter's own RemoteDesktop session — the same interface the input
//! injector drives directly to dodge the xdg-portal approval dialog (`inject/linux/libei.rs`
//! `connect_mutter`) — carries the full clipboard surface: `EnableClipboard`, `SetSelection`,
//! `SelectionRead`/`SelectionWrite`/`SelectionWriteDone`, and the `SelectionOwnerChanged` /
//! `SelectionTransfer` signals. We open our **own** standalone session for it (it coexists with the
//! injector's input session; validated on GNOME 50), so this backend is self-contained just like the
//! data-control one.
//!
//! One actor task owns the zbus connection + session and multiplexes the two signals with a command
//! channel; the fds Mutter hands out are **non-blocking**, so reads/writes flip them to blocking and
//! run on a blocking thread. Option/signal dict keys are hyphenated: `mime-types`, `session-is-owner`.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ashpd::zbus::{
    self,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::{ClipEvent, PasteResponder};

const RD_BUS: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_IFACE: &str = "org.gnome.Mutter.RemoteDesktop";
const SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// Upper bound on one `SelectionRead` (matches the wire clipboard cap, §7).
const CLIP_READ_CAP: u64 = 64 << 20;

/// Handle to the Mutter clipboard actor, held (inside a [`super::HostClipboard`]) by the session
/// coordinator.
pub struct MutterClipboard {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    /// Raw MIMEs the current host selection advertises (empty when we own it, or nothing is copied).
    /// Written by the actor on `SelectionOwnerChanged`; read for `current_wire_mimes` / fetches.
    current_raw: Arc<Mutex<Vec<String>>>,
}

enum Cmd {
    SetOffer(Vec<String>),
    ClearOffer,
    ReadCurrent {
        wire: String,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
}

impl MutterClipboard {
    /// Create a standalone Mutter RemoteDesktop session, `Start` + `EnableClipboard` it, and spawn
    /// the actor. Errors when Mutter isn't running (not GNOME) — the caller falls through to
    /// `BACKEND_UNAVAILABLE`.
    pub async fn open() -> Result<(MutterClipboard, mpsc::UnboundedReceiver<ClipEvent>)> {
        let conn = zbus::Connection::session()
            .await
            .context("session D-Bus (Mutter clipboard)")?;
        let rd = zbus::Proxy::new(&conn, RD_BUS, RD_PATH, RD_IFACE)
            .await
            .context("Mutter RemoteDesktop proxy (is gnome-shell running?)")?;
        let session_path: OwnedObjectPath = rd
            .call("CreateSession", &())
            .await
            .context("Mutter RemoteDesktop.CreateSession (clipboard)")?;
        let session = zbus::Proxy::new(&conn, RD_BUS, session_path, SESSION_IFACE)
            .await
            .context("Mutter RemoteDesktop.Session proxy")?;
        session
            .call_method("Start", &())
            .await
            .context("Mutter RemoteDesktop.Session.Start (clipboard)")?;
        let empty: HashMap<&str, Value> = HashMap::new();
        session
            .call_method("EnableClipboard", &(empty,))
            .await
            .context("Mutter EnableClipboard")?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let current_raw = Arc::new(Mutex::new(Vec::new()));

        tokio::spawn(actor(conn, session, cmd_rx, event_tx, current_raw.clone()));
        tracing::info!("clipboard backend bound (Mutter RemoteDesktop direct)");
        Ok((
            MutterClipboard {
                cmd_tx,
                current_raw,
            },
            event_rx,
        ))
    }

    /// Install a client's offered formats as the host selection (fire-and-forget on the actor).
    pub fn set_offer(&self, wire_mimes: &[String]) {
        let _ = self.cmd_tx.send(Cmd::SetOffer(wire_mimes.to_vec()));
    }

    /// Relinquish the selection we own.
    pub fn clear_offer(&self) {
        let _ = self.cmd_tx.send(Cmd::ClearOffer);
    }

    /// The current host selection's wire MIMEs (empty = nothing / we own it).
    pub fn current_wire_mimes(&self) -> Vec<String> {
        super::offer_wire_mimes(&self.current_raw.lock().unwrap())
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Read one wire format of the current host selection (a client's fetch). Round-trips the actor
    /// (SelectionRead + a blocking fd read).
    pub async fn read_current(&self, wire: &str) -> Result<Vec<u8>> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::ReadCurrent {
                wire: wire.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("Mutter clipboard actor gone"))?;
        rx.await
            .map_err(|_| anyhow!("Mutter clipboard read dropped"))?
    }
}

/// The actor: owns the connection + session, subscribes to the two clipboard signals, and serves
/// commands. Exits when the command channel closes (session ending) or a signal stream ends.
async fn actor(
    conn: zbus::Connection,
    session: zbus::Proxy<'static>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    event_tx: mpsc::UnboundedSender<ClipEvent>,
    current_raw: Arc<Mutex<Vec<String>>>,
) {
    let (mut owner, mut transfer) = match (
        session.receive_signal("SelectionOwnerChanged").await,
        session.receive_signal("SelectionTransfer").await,
    ) {
        (Ok(o), Ok(t)) => (o, t),
        _ => {
            tracing::warn!("Mutter clipboard: could not subscribe to selection signals");
            let _ = event_tx.send(ClipEvent::Closed);
            return;
        }
    };

    loop {
        tokio::select! {
            sig = owner.next() => {
                let Some(msg) = sig else { break };
                let Ok((opts,)) = msg.body().deserialize::<(HashMap<String, OwnedValue>,)>() else {
                    continue;
                };
                let is_owner = dict_bool(&opts, "session-is-owner").unwrap_or(false);
                let raw = dict_mimes(&opts, "mime-types");
                if is_owner {
                    // Our own offer (the client's content) — not host clipboard; don't announce it,
                    // and clear `current_raw` so a fetch never reads our own source back.
                    current_raw.lock().unwrap().clear();
                } else {
                    *current_raw.lock().unwrap() = raw.clone();
                    let wire = super::offer_wire_mimes(&raw)
                        .into_iter()
                        .map(str::to_string)
                        .collect();
                    if event_tx.send(ClipEvent::Selection { mimes: wire }).is_err() {
                        break;
                    }
                }
            }
            sig = transfer.next() => {
                let Some(msg) = sig else { break };
                let Ok((mime, serial)) = msg.body().deserialize::<(String, u32)>() else {
                    continue;
                };
                match super::wayland_to_wire(&mime) {
                    Some(wire) => {
                        // A host app pastes our offer: hand the fetch to the coordinator, then serve
                        // the returned bytes into the SelectionWrite fd off-task. NB Mutter issues
                        // *two* transfers per read (a size probe + the real read), so the coordinator
                        // fetches from the client twice per paste — correct, just not deduplicated.
                        let (tx, rx) = oneshot::channel();
                        if event_tx
                            .send(ClipEvent::Paste {
                                mime: wire.to_string(),
                                responder: PasteResponder::Channel(tx),
                            })
                            .is_err()
                        {
                            break;
                        }
                        let session = session.clone();
                        tokio::spawn(async move {
                            let bytes = rx.await.unwrap_or_default();
                            serve_write(&session, serial, bytes).await;
                        });
                    }
                    // Format we don't sync — fail the transfer cleanly.
                    None => serve_write(&session, serial, Vec::new()).await,
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // coordinator gone → session ending
                match cmd {
                    Cmd::SetOffer(wire) => {
                        let wl = super::wayland_offers_for(&wire);
                        if let Err(e) = set_selection(&session, &wl).await {
                            tracing::debug!(error = %e, "Mutter SetSelection failed");
                        }
                    }
                    Cmd::ClearOffer => {
                        if let Err(e) = set_selection(&session, &[]).await {
                            tracing::debug!(error = %e, "Mutter clear selection failed");
                        }
                    }
                    Cmd::ReadCurrent { wire, reply } => {
                        let raw = current_raw.lock().unwrap().clone();
                        let _ = reply.send(read_selection(&session, &wire, &raw).await);
                    }
                }
            }
        }
    }
    // Keep the connection owned for the actor's whole life (Mutter ties the session to it).
    drop(conn);
    let _ = event_tx.send(ClipEvent::Closed);
}

/// Offer `wl_mimes` as the host selection; an empty list relinquishes ownership.
async fn set_selection(session: &zbus::Proxy<'_>, wl_mimes: &[String]) -> Result<()> {
    let mut opts: HashMap<&str, Value> = HashMap::new();
    if !wl_mimes.is_empty() {
        let refs: Vec<&str> = wl_mimes.iter().map(String::as_str).collect();
        opts.insert("mime-types", Value::from(refs));
    }
    session
        .call_method("SetSelection", &(opts,))
        .await
        .context("Mutter SetSelection")?;
    Ok(())
}

/// Read the current selection's `wire` format: pick a concrete offered MIME, `SelectionRead` it, and
/// read the (non-blocking) fd to EOF on a blocking thread.
async fn read_selection(session: &zbus::Proxy<'_>, wire: &str, raw: &[String]) -> Result<Vec<u8>> {
    let mime =
        super::pick_wayland_mime(wire, raw).context("format not offered by the host clipboard")?;
    let fd: zbus::zvariant::OwnedFd = session
        .call("SelectionRead", &(mime.as_str(),))
        .await
        .context("Mutter SelectionRead")?;
    let fd = OwnedFd::from(fd);
    tokio::task::spawn_blocking(move || read_fd_to_end(fd))
        .await
        .map_err(|e| anyhow!("SelectionRead join: {e}"))?
}

/// Serve one `SelectionTransfer`: `SelectionWrite` → write `bytes` → `SelectionWriteDone`. Any write
/// failure still reports done (success=false) so Mutter completes the transfer.
async fn serve_write(session: &zbus::Proxy<'_>, serial: u32, bytes: Vec<u8>) {
    let ok = match write_selection(session, serial, bytes).await {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %e, "Mutter SelectionWrite failed");
            false
        }
    };
    let _ = session
        .call_method("SelectionWriteDone", &(serial, ok))
        .await;
}

async fn write_selection(session: &zbus::Proxy<'_>, serial: u32, bytes: Vec<u8>) -> Result<()> {
    let fd: zbus::zvariant::OwnedFd = session
        .call("SelectionWrite", &(serial,))
        .await
        .context("Mutter SelectionWrite")?;
    let fd = OwnedFd::from(fd);
    tokio::task::spawn_blocking(move || write_fd(fd, &bytes))
        .await
        .map_err(|e| anyhow!("SelectionWrite join: {e}"))?
}

/// Read a Mutter clipboard fd to EOF (capped). The fd is `O_NONBLOCK`; flip it to blocking first.
fn read_fd_to_end(fd: OwnedFd) -> Result<Vec<u8>> {
    set_blocking(&fd)?;
    let file = std::fs::File::from(fd);
    let mut buf = Vec::new();
    file.take(CLIP_READ_CAP)
        .read_to_end(&mut buf)
        .context("read SelectionRead fd")?;
    Ok(buf)
}

/// Write `bytes` into a Mutter clipboard fd and close it (EOF). Flip the `O_NONBLOCK` fd to blocking.
fn write_fd(fd: OwnedFd, bytes: &[u8]) -> Result<()> {
    set_blocking(&fd)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).context("write SelectionWrite fd")?;
    Ok(())
}

/// Peel any `Value::Value` (variant) wrappers to the concrete value. The `a{sv}` dict values Mutter
/// sends arrive as variants, so a plain `TryFrom<OwnedValue>` (which matches the concrete type) never
/// sees through them — this strips the layer first.
fn peel<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    let mut cur = v;
    while let Value::Value(inner) = cur {
        cur = inner;
    }
    cur
}

/// Extract a boolean dict entry (e.g. `session-is-owner`), unwrapping the variant.
fn dict_bool(opts: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    match peel(opts.get(key)?) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// Extract a string-array dict entry (e.g. `mime-types`), unwrapping the variant. Mutter wraps the
/// array in a single-field struct (`(as)`, seen on `SelectionOwnerChanged`), so unwrap that too.
fn dict_mimes(opts: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    let Some(v) = opts.get(key) else {
        return Vec::new();
    };
    let mut val = peel(v);
    if let Value::Structure(s) = val {
        match s.fields().first() {
            Some(first) => val = peel(first),
            None => return Vec::new(),
        }
    }
    let Value::Array(arr) = val else {
        return Vec::new();
    };
    arr.inner()
        .iter()
        .filter_map(|e| match peel(e) {
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// Clear `O_NONBLOCK` on a Mutter clipboard fd so a blocking `spawn_blocking` read/write works.
fn set_blocking(fd: &OwnedFd) -> Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is a valid fd owned by `fd` for the duration of these fcntl calls.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(anyhow!(
            "fcntl F_GETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: as above; clearing O_NONBLOCK on our own fd.
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(anyhow!(
            "fcntl F_SETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// On-glass tests against a **live GNOME/Mutter** session (`WAYLAND_DISPLAY=wayland-0`). `#[ignore]`d
/// — run explicitly under GNOME (Mutter has no `wl-clipboard`, so a second Mutter session plays the
/// "host app"):
///
/// ```text
/// cargo test -p punktfunk-host --bin punktfunk-host -- --ignored --nocapture clipboard::mutter::live
/// ```
///
/// Skips (does not fail) when Mutter isn't running, so `--ignored` off-GNOME is a clean no-op.
#[cfg(test)]
mod live {
    use super::*;
    use std::time::Duration;

    /// A second Mutter session standing in for a host clipboard app.
    struct Helper {
        session: zbus::Proxy<'static>,
        _conn: zbus::Connection,
    }

    impl Helper {
        async fn open() -> Result<Helper> {
            let conn = zbus::Connection::session().await?;
            let rd = zbus::Proxy::new(&conn, RD_BUS, RD_PATH, RD_IFACE).await?;
            let path: OwnedObjectPath = rd.call("CreateSession", &()).await?;
            let session = zbus::Proxy::new(&conn, RD_BUS, path, SESSION_IFACE).await?;
            session.call_method("Start", &()).await?;
            let empty: HashMap<&str, Value> = HashMap::new();
            session.call_method("EnableClipboard", &(empty,)).await?;
            Ok(Helper {
                session,
                _conn: conn,
            })
        }

        /// Own the selection offering plain text, serving `payload` on every transfer request.
        async fn offer_text(&self, payload: &'static [u8]) {
            let mut transfer = self
                .session
                .receive_signal("SelectionTransfer")
                .await
                .unwrap();
            let session = self.session.clone();
            tokio::spawn(async move {
                while let Some(msg) = transfer.next().await {
                    if let Ok((_mime, serial)) = msg.body().deserialize::<(String, u32)>() {
                        serve_write(&session, serial, payload.to_vec()).await;
                    }
                }
            });
            set_selection(
                &self.session,
                &[
                    "text/plain;charset=utf-8".to_string(),
                    "text/plain".to_string(),
                ],
            )
            .await
            .unwrap();
        }

        /// Paste the current selection's plain text.
        async fn read_text(&self) -> Vec<u8> {
            let fd: zbus::zvariant::OwnedFd = self
                .session
                .call("SelectionRead", &("text/plain;charset=utf-8",))
                .await
                .unwrap();
            let fd = OwnedFd::from(fd);
            tokio::task::spawn_blocking(move || read_fd_to_end(fd))
                .await
                .unwrap()
                .unwrap()
        }
    }

    async fn next_selection(
        rx: &mut mpsc::UnboundedReceiver<ClipEvent>,
        timeout: Duration,
    ) -> Option<Vec<String>> {
        tokio::time::timeout(timeout, async {
            loop {
                match rx.recv().await {
                    Some(ClipEvent::Selection { mimes }) if !mimes.is_empty() => {
                        return Some(mimes)
                    }
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[test]
    #[ignore = "needs a live GNOME/Mutter session (WAYLAND_DISPLAY=wayland-0)"]
    fn live_mutter_roundtrip() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (backend, mut rx) = match MutterClipboard::open().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("SKIP: no Mutter clipboard (not GNOME?): {e:#}");
                    return;
                }
            };
            let helper = Helper::open().await.expect("helper Mutter session");

            // --- host copy → backend observes Selection + read_current returns the bytes ---
            helper.offer_text(b"gnome-host-copied").await;
            let mimes = next_selection(&mut rx, Duration::from_secs(3))
                .await
                .expect("Selection after the helper offered text");
            assert!(
                mimes.iter().any(|m| m == super::super::WIRE_TEXT),
                "offer carries wire text: {mimes:?}"
            );
            let got = backend
                .read_current(super::super::WIRE_TEXT)
                .await
                .expect("read_current text");
            assert_eq!(got, b"gnome-host-copied");

            // --- backend offers client content → the host app (helper) pastes it ---
            backend.set_offer(&[super::super::WIRE_TEXT.to_string()]);
            tokio::time::sleep(Duration::from_millis(500)).await; // let SetSelection take effect
                                                                  // Answer every Paste request the host app (helper) triggers, until the read completes.
            let paste_side = async {
                while let Some(ev) = rx.recv().await {
                    if let ClipEvent::Paste { responder, .. } = ev {
                        responder.respond(b"punktfunk-served".to_vec()).await;
                    }
                }
            };
            let read = tokio::select! {
                r = helper.read_text() => r,
                _ = paste_side => Vec::new(),
            };
            assert_eq!(read, b"punktfunk-served");
        });
    }
}
