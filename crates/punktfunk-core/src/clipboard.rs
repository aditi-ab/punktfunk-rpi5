//! Client-side shared-clipboard transport (`design/clipboard-and-file-transfer.md` §5.1).
//!
//! This is the client-core half of Phase 0: the per-session async task that runs the fetch-stream
//! **accept loop** (so the host can pull data the client offered) and drives **outbound fetches**
//! (so the client can pull what the host offered), surfacing everything to the embedder as
//! poll-style [`ClipEventCore`] events. The metadata plane — enabling sync and announcing offers —
//! rides the control stream as ordinary [`ClipControl`]/[`ClipOffer`] control messages
//! ([`crate::client`] routes those); only the bulk bytes flow through here, over the
//! [`crate::quic::clipstream`] fetch bi-streams.
//!
//! There is intentionally **no OS pasteboard code here** — that lives in the native client (macOS
//! first). The event/command seam is what the C ABI ([`crate::abi`]) exposes so a native client
//! polls offers/fetch-requests and answers with bytes.

use std::collections::HashMap;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

use crate::error::PunktfunkStatus;
use crate::quic::{
    clipstream, ClipFetch, ClipFetchHdr, ClipKind, CLIP_FETCH_DENIED, CLIP_FETCH_OK,
    CLIP_FETCH_STALE, CLIP_FETCH_UNAVAILABLE,
};

/// Per-fetch requester-side size cap (bytes). A holder that streams more than this is treated as a
/// cap breach and the fetch fails rather than buffering unboundedly (§7). Phase 0 uses one fixed
/// value; a future host-policy `PUNKTFUNK_CLIP_MAX_MB` tightens it per session.
pub const CLIP_FETCH_CAP: usize = 64 << 20;

/// Inbound-serve `req_id`s carry this high bit so they never collide with the client-assigned
/// outbound-fetch `xfer_id`s (which count up from 1). A single [`ClipCommand::Cancel`] `id` can
/// then be routed to the right table.
pub const INBOUND_REQ_FLAG: u32 = 0x8000_0000;

/// Overall stall bound for one outbound fetch (`design/clipboard-and-file-transfer.md` §3.4): a
/// holder that never answers fails the transfer instead of hanging it.
const FETCH_STALL_SECS: u64 = 60;

/// A clipboard event surfaced to the embedder via `NativeClient::next_clip` (→ the C ABI poll
/// `punktfunk_connection_next_clipboard`).
#[derive(Clone, Debug)]
pub enum ClipEventCore {
    /// The host announced new clipboard content (the host copied). The embedder decides whether to
    /// fetch it — lazily, only when a local app actually pastes.
    RemoteOffer { seq: u32, kinds: Vec<ClipKind> },
    /// Host ack / unsolicited policy or backend update, for the toggle UI.
    State {
        enabled: bool,
        policy: u8,
        reason: u8,
    },
    /// The host is pasting content the client offered: it opened a fetch stream for
    /// `(mime, file_index)`. The embedder must answer with `clip_serve(req_id, …)` (or
    /// `clip_cancel(req_id)`).
    FetchRequest {
        req_id: u32,
        seq: u32,
        file_index: u32,
        mime: String,
    },
    /// Bytes for a fetch the embedder started (`xfer_id` from `clip_fetch`). Phase 0 delivers the
    /// whole payload in one event (`last = true`).
    Data {
        xfer_id: u32,
        bytes: Vec<u8>,
        last: bool,
    },
    /// A transfer was cancelled (by either side).
    Cancelled { id: u32 },
    /// A transfer failed; `code` is a [`PunktfunkStatus`] value (negative).
    Error { id: u32, code: i32 },
}

/// A command from the embedder (via the C ABI) into the clipboard task. `ClipControl`/`ClipOffer`
/// are *not* here — they ride the control stream as ordinary control messages.
pub enum ClipCommand {
    /// Open a fetch of the remote's offered content; `xfer_id` is client-assigned and echoed back
    /// on the resulting [`ClipEventCore::Data`]/`Error`/`Cancelled`.
    Fetch {
        xfer_id: u32,
        seq: u32,
        file_index: u32,
        mime: String,
    },
    /// Provide bytes answering an inbound [`ClipEventCore::FetchRequest`] (`req_id`). Chunks
    /// accumulate; `last` completes the transfer.
    Serve {
        req_id: u32,
        bytes: Vec<u8>,
        last: bool,
    },
    /// Cancel a transfer by id — either an outbound fetch (`xfer_id`) or an inbound serve
    /// (`req_id`, high bit set).
    Cancel { id: u32 },
}

type ServeWaiters = Arc<Mutex<HashMap<u32, oneshot::Sender<Option<Vec<u8>>>>>>;

/// Map a non-OK [`ClipFetchHdr::status`] to the [`PunktfunkStatus`] surfaced on an
/// [`ClipEventCore::Error`].
fn fetch_status_to_code(status: u8) -> i32 {
    let s = match status {
        CLIP_FETCH_STALE => PunktfunkStatus::NoFrame, // stale offer → "nothing to insert"
        CLIP_FETCH_UNAVAILABLE => PunktfunkStatus::Unsupported,
        CLIP_FETCH_DENIED => PunktfunkStatus::InvalidArg,
        _ => PunktfunkStatus::BadPacket,
    };
    s as i32
}

/// The per-session clipboard task. Runs until the connection closes or the embedder drops the
/// command sender. It owns no clipboard *content* — the embedder supplies bytes on demand.
pub async fn run(
    conn: quinn::Connection,
    events: SyncSender<ClipEventCore>,
    mut cmd_rx: UnboundedReceiver<ClipCommand>,
) {
    // Inbound fetch-serve waiters: req_id -> a oneshot the serving stream task parks on until the
    // embedder answers with bytes (`Some`) or denies/cancels (`None`).
    let serve_waiters: ServeWaiters = Arc::new(Mutex::new(HashMap::new()));
    // Accumulation buffers for chunked `clip_serve` (req_id -> bytes so far).
    let mut serve_bufs: HashMap<u32, Vec<u8>> = HashMap::new();
    // Outbound fetch cancel triggers: xfer_id -> a oneshot that aborts the fetch task.
    let mut fetch_cancels: HashMap<u32, oneshot::Sender<()>> = HashMap::new();
    let mut next_req_id: u32 = 1;

    loop {
        tokio::select! {
            // The host opened a fetch bi-stream toward us (it is pasting our offered data).
            accepted = conn.accept_bi() => {
                let Ok((send, recv)) = accepted else { break }; // connection gone
                let req_id = INBOUND_REQ_FLAG | next_req_id;
                next_req_id = next_req_id.wrapping_add(1);
                if next_req_id == 0 {
                    next_req_id = 1;
                }
                let events = events.clone();
                let waiters = serve_waiters.clone();
                tokio::spawn(serve_inbound(send, recv, req_id, events, waiters));
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // NativeClient dropped
                match cmd {
                    ClipCommand::Fetch { xfer_id, seq, file_index, mime } => {
                        // Prune finished fetches first: a completed/failed/timed-out fetch task
                        // drops its cancel receiver, so its sender reads closed. Without this
                        // the map grew by one dead sender per paste for the whole session (only
                        // an explicit Cancel ever removed entries).
                        fetch_cancels.retain(|_, tx| !tx.is_closed());
                        let (cancel_tx, cancel_rx) = oneshot::channel();
                        fetch_cancels.insert(xfer_id, cancel_tx);
                        let conn = conn.clone();
                        let events = events.clone();
                        let req = ClipFetch { seq, file_index, mime };
                        tokio::spawn(run_outbound_fetch(conn, xfer_id, req, events, cancel_rx));
                    }
                    ClipCommand::Serve { req_id, bytes, last } => {
                        // Gate on a genuinely parked fetch (the waiter registers before the
                        // FetchRequest event is emitted, so a live serve always finds it):
                        // bytes served under a stale/unknown/cancelled req_id would otherwise
                        // pool here for the whole session with Ok returned for every chunk.
                        if !serve_waiters.lock().unwrap().contains_key(&req_id) {
                            serve_bufs.remove(&req_id);
                        } else {
                            let buf = serve_bufs.entry(req_id).or_default();
                            if buf.len().saturating_add(bytes.len()) > CLIP_FETCH_CAP {
                                // The requester bounds its read at CLIP_FETCH_CAP — anything
                                // larger is memory burned toward a guaranteed peer rejection.
                                // Fail the transfer NOW: peer reads UNAVAILABLE, embedder gets
                                // an Error instead of silent Ok-per-chunk.
                                serve_bufs.remove(&req_id);
                                if let Some(tx) = serve_waiters.lock().unwrap().remove(&req_id) {
                                    let _ = tx.send(None);
                                }
                                let _ = events.try_send(ClipEventCore::Error {
                                    id: req_id,
                                    code: PunktfunkStatus::InvalidArg as i32,
                                });
                            } else {
                                buf.extend_from_slice(&bytes);
                                if last {
                                    let full = serve_bufs.remove(&req_id).unwrap_or_default();
                                    if let Some(tx) = serve_waiters.lock().unwrap().remove(&req_id)
                                    {
                                        let _ = tx.send(Some(full));
                                    }
                                }
                            }
                        }
                    }
                    ClipCommand::Cancel { id } => {
                        // Route to whichever table owns the id (they are disjoint by the high bit).
                        if let Some(tx) = fetch_cancels.remove(&id) {
                            let _ = tx.send(());
                        }
                        serve_bufs.remove(&id);
                        if let Some(tx) = serve_waiters.lock().unwrap().remove(&id) {
                            let _ = tx.send(None); // deny — the serving task writes UNAVAILABLE
                        }
                    }
                }
            }
        }
    }
}

/// Serve one inbound fetch stream: validate the header + request, emit a [`ClipEventCore::FetchRequest`],
/// then park until the embedder supplies bytes (or denies), and stream them back.
async fn serve_inbound(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    req_id: u32,
    events: SyncSender<ClipEventCore>,
    waiters: ServeWaiters,
) {
    let _ = send.set_priority(-1);
    let kind = match clipstream::read_stream_header(&mut recv).await {
        Ok(k) => k,
        Err(_) => return,
    };
    if kind != clipstream::CLIP_STREAM_KIND_FETCH {
        let _ = send.reset(clipstream::cancelled_code());
        return;
    }
    let req = match clipstream::read_fetch(&mut recv).await {
        Ok(r) => r,
        Err(_) => return,
    };

    // Register the waiter before emitting the event, so an immediate `clip_serve` can't race ahead
    // of the insert.
    let (tx, rx) = oneshot::channel();
    waiters.lock().unwrap().insert(req_id, tx);
    let ev = ClipEventCore::FetchRequest {
        req_id,
        seq: req.seq,
        file_index: req.file_index,
        mime: req.mime,
    };
    if events.try_send(ev).is_err() {
        // The embedder isn't draining events (or the session is ending): refuse cleanly.
        waiters.lock().unwrap().remove(&req_id);
        let _ = clipstream::write_fetch_hdr(
            &mut send,
            &ClipFetchHdr {
                status: CLIP_FETCH_UNAVAILABLE,
                total_size: 0,
            },
        )
        .await;
        return;
    }

    // Overall stall bound (§3.4) — the inbound mirror of `run_outbound_fetch`'s: an embedder
    // that never answers must not hold the waiter, this task, and the accepted bi-stream open
    // for the rest of the session (~100 unanswered pastes exhaust the connection's bidi-stream
    // budget and every later host paste stalls). `send.stopped()` additionally wakes us the
    // moment the peer gives up on its side of the fetch.
    let answer = tokio::select! {
        r = rx => r.ok().flatten(),
        _ = send.stopped() => {
            let _ = events.try_send(ClipEventCore::Cancelled { id: req_id });
            None
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(FETCH_STALL_SECS)) => {
            let _ = events.try_send(ClipEventCore::Cancelled { id: req_id });
            None
        }
    };
    // Idempotent — the answering/denying paths already removed it; the stall paths must.
    waiters.lock().unwrap().remove(&req_id);

    match answer {
        Some(bytes) => {
            if clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_OK,
                    total_size: bytes.len() as u64,
                },
            )
            .await
            .is_ok()
            {
                let _ = clipstream::write_data(&mut send, &bytes).await;
            }
        }
        // Denied, cancelled, or the waiter was dropped (task ending): tell the peer it's gone.
        _ => {
            let _ = clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_UNAVAILABLE,
                    total_size: 0,
                },
            )
            .await;
        }
    }
}

/// Drive one outbound fetch: open the stream, read the header + data (bounded), and emit a
/// [`ClipEventCore::Data`] / `Error` / `Cancelled`.
async fn run_outbound_fetch(
    conn: quinn::Connection,
    xfer_id: u32,
    req: ClipFetch,
    events: SyncSender<ClipEventCore>,
    cancel_rx: oneshot::Receiver<()>,
) {
    let transfer = async {
        let (send, mut recv) = clipstream::open_fetch(&conn, &req)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        let hdr = clipstream::read_fetch_hdr(&mut recv)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        if hdr.status != CLIP_FETCH_OK {
            return Err(fetch_status_to_code(hdr.status));
        }
        let data = clipstream::read_data(&mut recv, CLIP_FETCH_CAP)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        drop(send); // done — dropping the send half is a clean FIN-less close on our side
        Ok(data)
    };

    tokio::select! {
        r = transfer => match r {
            Ok(data) => {
                let _ = events.try_send(ClipEventCore::Data { xfer_id, bytes: data, last: true });
            }
            Err(code) => {
                let _ = events.try_send(ClipEventCore::Error { id: xfer_id, code });
            }
        },
        _ = cancel_rx => {
            // The `transfer` future is dropped here; its streams reset on drop.
            let _ = events.try_send(ClipEventCore::Cancelled { id: xfer_id });
        }
        // Overall stall bound (design/clipboard-and-file-transfer.md §3.4): a holder that never
        // answers must not hang the transfer. Dropping `transfer` resets the streams.
        _ = tokio::time::sleep(std::time::Duration::from_secs(FETCH_STALL_SECS)) => {
            let _ = events.try_send(ClipEventCore::Error {
                id: xfer_id,
                code: PunktfunkStatus::Timeout as i32,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::test_util::connect_pair;
    use crate::quic::CLIP_FILE_INDEX_NONE;

    /// A serve chunk that alone breaches the requester-side CLIP_FETCH_CAP must fail the
    /// transfer immediately — Error to the embedder, UNAVAILABLE to the peer — instead of
    /// accumulating Ok-per-chunk toward a guaranteed peer-side rejection. Also pins the
    /// waiter-membership gate: the serve only accumulated because the fetch was parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_serve_fails_the_transfer_instead_of_buffering() {
        let (_s, _c, host_conn, client_conn) = connect_pair().await;
        let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel(16);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run(client_conn, ev_tx, cmd_rx));

        // Host pastes: it opens a fetch bi-stream toward the client.
        let req = ClipFetch {
            seq: 1,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "text/plain;charset=utf-8".into(),
        };
        let (_send, mut recv) = clipstream::open_fetch(&host_conn, &req).await.unwrap();

        // The client core surfaces the FetchRequest (the waiter is parked now).
        let (req_id, ev_rx) = tokio::task::spawn_blocking(move || {
            match ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            {
                ClipEventCore::FetchRequest { req_id, .. } => (req_id, ev_rx),
                other => panic!("expected FetchRequest, got {other:?}"),
            }
        })
        .await
        .unwrap();

        cmd_tx
            .send(ClipCommand::Serve {
                req_id,
                bytes: vec![0u8; CLIP_FETCH_CAP + 1],
                last: false,
            })
            .unwrap();
        let ev = tokio::task::spawn_blocking(move || {
            ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        })
        .await
        .unwrap();
        match ev {
            ClipEventCore::Error { id, .. } => assert_eq!(id, req_id),
            other => panic!("expected Error, got {other:?}"),
        }
        // The peer's read side sees the transfer refused, not a hang.
        let hdr = clipstream::read_fetch_hdr(&mut recv).await.unwrap();
        assert_eq!(hdr.status, CLIP_FETCH_UNAVAILABLE);
    }
}
