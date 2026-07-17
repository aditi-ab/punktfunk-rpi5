//! Host clipboard coordinator (`design/clipboard-and-file-transfer.md` §4.2).
//!
//! One async task per streaming session that bridges the real session clipboard (whichever
//! [`super::HostClipboard`] backend this platform opened) to the QUIC clipboard plane. It owns all
//! four data paths:
//!
//! * **host copy → client** — a backend [`ClipEvent::Selection`] becomes a [`ClipOffer`] pushed to the
//!   control loop (`offer_tx`), which forwards it to the client.
//! * **client fetch of the host clipboard** — the fetch-stream `accept_bi` loop lives here; each
//!   stream is answered by reading the current host selection ([`ClipboardBackend::read_current`]).
//! * **client copy → host** — a [`ClipCoordCmd::RemoteOffer`] installs the client's formats as a lazy
//!   host selection ([`HostClipboard::set_offer`]).
//! * **host paste of client content** — a backend [`ClipEvent::Paste`] triggers an *outbound* fetch to
//!   the client, whose bytes are handed to the backend's [`PasteResponder`].
//!
//! The coordinator is backend-agnostic (Linux data-control / Mutter, Windows Win32); the control loop
//! reaches it through the portable [`ClipCoordCmd`] channel so the host's native control loop
//! compiles on every host platform.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use punktfunk_core::clipboard::CLIP_FETCH_CAP;
use punktfunk_core::quic::{
    clipstream, ClipFetch, ClipFetchHdr, ClipKind, ClipOffer, CLIP_FETCH_OK, CLIP_FETCH_STALE,
    CLIP_FETCH_UNAVAILABLE, CLIP_FILE_INDEX_NONE,
};

use super::{ClipEvent, HostClipboard, PasteResponder};
use crate::ClipCoordCmd;

/// Upper bound on one outbound fetch (host pasting client content). A client that never answers must
/// not hang the pasting host app's pipe read (§3.4) — the paste falls back to empty instead.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Open whichever host clipboard backend this session supports (data-control, else Mutter direct) and
/// spawn the coordinator. Returns `true` when a live backend was bound (the caller's control loop
/// then serves real clipboard data); `false` when none is available (gamescope, no live compositor),
/// in which case the channels are dropped so the control loop reports `CLIP_REASON_BACKEND_UNAVAILABLE`
/// and declines fetches defensively.
pub async fn start(
    conn: quinn::Connection,
    clip_enabled: Arc<AtomicBool>,
    cmd_rx: UnboundedReceiver<ClipCoordCmd>,
    offer_tx: UnboundedSender<ClipOffer>,
) -> bool {
    match HostClipboard::open().await {
        Ok((backend, clip_rx)) => {
            tokio::spawn(run(
                conn,
                Arc::new(backend),
                clip_rx,
                clip_enabled,
                cmd_rx,
                offer_tx,
            ));
            true
        }
        Err(e) => {
            tracing::info!(error = %format!("{e:#}"), "clipboard backend unavailable — fetches will be declined");
            false
        }
    }
}

/// The coordinator loop. Multiplexes control-loop commands, backend clipboard events, and inbound
/// fetch streams; exits when any of the three peers goes away (session ending).
async fn run(
    conn: quinn::Connection,
    backend: Arc<HostClipboard>,
    mut clip_rx: UnboundedReceiver<ClipEvent>,
    clip_enabled: Arc<AtomicBool>,
    mut cmd_rx: UnboundedReceiver<ClipCoordCmd>,
    offer_tx: UnboundedSender<ClipOffer>,
) {
    // Seq of the offer the host most recently announced; a client fetch naming a different seq is
    // stale (the host clipboard moved on) and is declined.
    let host_seq = Arc::new(AtomicU32::new(0));
    let mut next_seq: u32 = 1;
    // Seq of the client's most recent offer, echoed on the outbound fetch we open when a host app
    // pastes client content (informational for the client's serve side).
    let mut client_seq: u32 = 0;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // control loop gone → session ending
                match cmd {
                    ClipCoordCmd::SetEnabled(true) => {
                        // A just-enabled client should see whatever the host already has copied.
                        let mimes = backend.current_wire_mimes();
                        if !mimes.is_empty() {
                            let _ = offer_tx.send(build_offer(&mut next_seq, &host_seq, mimes));
                        }
                    }
                    ClipCoordCmd::SetEnabled(false) => {
                        if let Err(e) = backend.clear_offer() {
                            tracing::debug!(error = %e, "clipboard clear_offer failed");
                        }
                    }
                    ClipCoordCmd::RemoteOffer { seq, mimes } => {
                        client_seq = seq;
                        let res = if mimes.is_empty() {
                            backend.clear_offer()
                        } else {
                            backend.set_offer(&mimes)
                        };
                        if let Err(e) = res {
                            tracing::debug!(error = %e, "clipboard apply remote offer failed");
                        }
                    }
                }
            }
            ev = clip_rx.recv() => {
                let Some(ev) = ev else { break }; // backend dispatch thread ended
                match ev {
                    ClipEvent::Selection { mimes } => {
                        // Forward host copies (empty `mimes` = the clipboard was cleared) only while
                        // the client has sync on — the offer is metadata; bytes still cross lazily.
                        if clip_enabled.load(Ordering::SeqCst) {
                            let _ = offer_tx.send(build_offer(&mut next_seq, &host_seq, mimes));
                        }
                    }
                    ClipEvent::Paste { mime, responder } => {
                        // A host app is pasting the client's offered content: pull that format from
                        // the client and hand it to the backend's responder. Off-task so the loop
                        // keeps serving.
                        tokio::spawn(fetch_into_pipe(conn.clone(), client_seq, mime, responder));
                    }
                    ClipEvent::Closed => break,
                }
            }
            accepted = conn.accept_bi() => {
                let Ok((send, recv)) = accepted else { break }; // connection gone
                // The control stream is already accepted at the handshake, so every stream here is a
                // clipboard fetch. Serve it off-task (the read blocks on the source app's pipe).
                tokio::spawn(serve_fetch(
                    send,
                    recv,
                    Arc::clone(&backend),
                    Arc::clone(&host_seq),
                    clip_enabled.load(Ordering::SeqCst),
                ));
            }
        }
    }
    // Session ending: don't leave our lazy source as the compositor's active selection.
    let _ = backend.clear_offer();
}

/// Mint a [`ClipOffer`] for `mimes`, advancing the host offer seq (skipping 0, the "never offered"
/// sentinel) and publishing it as the current one for staleness checks.
fn build_offer(next_seq: &mut u32, host_seq: &AtomicU32, mimes: Vec<String>) -> ClipOffer {
    let seq = *next_seq;
    *next_seq = next_seq.wrapping_add(1);
    if *next_seq == 0 {
        *next_seq = 1;
    }
    host_seq.store(seq, Ordering::SeqCst);
    let kinds = mimes
        .into_iter()
        .map(|mime| ClipKind { mime, size_hint: 0 })
        .collect();
    ClipOffer { seq, kinds }
}

/// Serve one inbound fetch stream (a client pulling the host clipboard): validate the header +
/// request, then answer with the current host selection's bytes for the requested wire MIME.
async fn serve_fetch(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    backend: Arc<HostClipboard>,
    host_seq: Arc<AtomicU32>,
    enabled: bool,
) {
    let _ = send.set_priority(-1);
    match clipstream::read_stream_header(&mut recv).await {
        Ok(k) if k == clipstream::CLIP_STREAM_KIND_FETCH => {}
        _ => {
            let _ = send.reset(clipstream::cancelled_code());
            return;
        }
    }
    let req = match clipstream::read_fetch(&mut recv).await {
        Ok(r) => r,
        Err(_) => return,
    };

    let decline = |status: u8| ClipFetchHdr {
        status,
        total_size: 0,
    };
    if !enabled {
        let _ = clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_UNAVAILABLE)).await;
        return;
    }
    if req.seq != host_seq.load(Ordering::SeqCst) {
        let _ = clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_STALE)).await;
        return;
    }

    // `read_current` reads the host selection (a blocking pipe read, offloaded by the backend).
    match backend.read_current(&req.mime).await {
        Ok(data) => {
            let hdr = ClipFetchHdr {
                status: CLIP_FETCH_OK,
                total_size: data.len() as u64,
            };
            if clipstream::write_fetch_hdr(&mut send, &hdr).await.is_ok() {
                let _ = clipstream::write_data(&mut send, &data).await;
            }
        }
        // The format vanished (clipboard changed mid-fetch) or the read failed → nothing to send.
        Err(_) => {
            let _ = clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_UNAVAILABLE)).await;
        }
    }
}

/// Pull `mime` of the client's current offer (`seq`) over an outbound fetch stream and hand the bytes
/// to the backend's paste `responder`. Any failure (timeout, decline, I/O) responds with empty bytes
/// so the pasting host app gets an empty paste instead of hanging.
async fn fetch_into_pipe(
    conn: quinn::Connection,
    seq: u32,
    mime: String,
    responder: PasteResponder,
) {
    let req = ClipFetch {
        seq,
        file_index: CLIP_FILE_INDEX_NONE,
        mime,
    };
    let fetched = tokio::time::timeout(FETCH_TIMEOUT, async {
        let (send, mut recv) = clipstream::open_fetch(&conn, &req).await.ok()?;
        let hdr = clipstream::read_fetch_hdr(&mut recv).await.ok()?;
        if hdr.status != CLIP_FETCH_OK {
            return None;
        }
        let data = clipstream::read_data(&mut recv, CLIP_FETCH_CAP)
            .await
            .ok()?;
        drop(send); // clean close of our half
        Some(data)
    })
    .await
    .ok()
    .flatten();

    responder.respond(fetched.unwrap_or_default()).await;
}
