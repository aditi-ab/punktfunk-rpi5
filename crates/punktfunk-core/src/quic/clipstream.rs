//! Per-transfer clipboard fetch streams (`design/clipboard-and-file-transfer.md` §3.3).
//!
//! Bulk clipboard / file bytes never ride the control stream (u16-capped) or datagrams (lossy,
//! single-packet). The **requester opens a fresh QUIC bi-stream** toward the data holder, writes a
//! small stream header + a [`ClipFetch`]; the holder replies with a [`ClipFetchHdr`] then raw data
//! chunks until FIN. One transfer per stream ⇒ natural flow control, clean cancelation
//! (`RESET_STREAM`), and no head-of-line blocking against control or other transfers.
//!
//! These helpers are the transport half only; they hold no clipboard state, so the host and the
//! client-core reuse the exact same open/accept/serve wire dance (the accept-loop that dispatches
//! by stream kind lives on each side, since the two sides own their connections differently).

use super::{io, ClipFetch, ClipFetchHdr};

/// First bytes an opener writes on a freshly-opened clipboard bi-stream: a magic keeping this
/// stream namespace disjoint from any future stream kind, plus a 1-byte kind discriminator. A
/// distinct magic means a stream opened for some other future purpose can never be misrouted here.
pub const STREAM_MAGIC: &[u8; 4] = b"PKFs";

/// Stream-kind byte: a clipboard fetch (request/response of one format). Future stream kinds
/// (e.g. a bulk file-content push) mux under the same [`STREAM_MAGIC`] with a different byte.
pub const CLIP_STREAM_KIND_FETCH: u8 = 0x01;

/// QUIC application error code used to `reset`/`stop` a clipboard fetch stream on cancel — sync
/// disabled mid-transfer, paste timed out, size cap exceeded, teardown. Distinct from the
/// connection close codes ([`super::QUIT_CLOSE_CODE`] `0x51` / [`super::APP_EXITED_CLOSE_CODE`]
/// `0x52`), the connection reject code `0x42`, and the pairing-rejection close block
/// `0x60`–`0x67` — stream reset codes and connection close codes are separate QUIC namespaces,
/// but the vocabularies stay disjoint on purpose so a captured code is unambiguous.
pub const CLIP_CANCELLED_CODE: u32 = 0x70;

/// Chunk size for streaming fetch data (64 KiB writes — matches the control-frame bound).
pub const CLIP_CHUNK: usize = 64 * 1024;

/// The `VarInt` form of [`CLIP_CANCELLED_CODE`], for `SendStream::reset` / `RecvStream::stop`.
pub fn cancelled_code() -> quinn::VarInt {
    quinn::VarInt::from_u32(CLIP_CANCELLED_CODE)
}

/// Requester side: open a fresh bi-stream toward the holder, deprioritize it under the control
/// stream, write the stream header + the [`ClipFetch`], and hand back both halves. The send half
/// is returned so the caller can `reset`/`finish` for cancelation; the recv half is positioned to
/// read the [`ClipFetchHdr`] next (see [`read_fetch_hdr`]).
pub async fn open_fetch(
    conn: &quinn::Connection,
    req: &ClipFetch,
) -> std::io::Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut send, recv) = conn.open_bi().await.map_err(std::io::Error::other)?;
    // Yield to the control stream (default priority 0) so a large paste never head-of-line-blocks
    // the input/audio/control traffic sharing this connection.
    let _ = send.set_priority(-1);
    // The opener MUST write before the peer's `accept_bi()` can return (quinn contract), so send
    // the whole request eagerly.
    let mut hdr = Vec::with_capacity(5);
    hdr.extend_from_slice(STREAM_MAGIC);
    hdr.push(CLIP_STREAM_KIND_FETCH);
    send.write_all(&hdr).await.map_err(std::io::Error::other)?;
    io::write_msg(&mut send, &req.encode()).await?;
    Ok((send, recv))
}

/// Holder side, step 1: after `accept_bi()`, read and validate the 5-byte stream header. Returns
/// the kind byte (e.g. [`CLIP_STREAM_KIND_FETCH`]); an unknown magic is an error and the caller
/// should `stop` the stream.
pub async fn read_stream_header(recv: &mut quinn::RecvStream) -> std::io::Result<u8> {
    let mut hdr = [0u8; 5];
    recv.read_exact(&mut hdr)
        .await
        .map_err(std::io::Error::other)?;
    if &hdr[0..4] != STREAM_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad clip stream magic",
        ));
    }
    Ok(hdr[4])
}

/// Holder side, step 2: read the [`ClipFetch`] request that follows the header.
pub async fn read_fetch(recv: &mut quinn::RecvStream) -> std::io::Result<ClipFetch> {
    let raw = io::read_msg(recv).await?;
    ClipFetch::decode(&raw)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad ClipFetch"))
}

/// Holder side, step 3: send the response header (before any data chunks).
pub async fn write_fetch_hdr(
    send: &mut quinn::SendStream,
    hdr: &ClipFetchHdr,
) -> std::io::Result<()> {
    io::write_msg(send, &hdr.encode()).await
}

/// Holder side, step 4 (only when the header was [`super::CLIP_FETCH_OK`]): stream `data` as
/// 64 KiB chunks then FIN so the requester's [`read_data`] terminates.
pub async fn write_data(send: &mut quinn::SendStream, data: &[u8]) -> std::io::Result<()> {
    for chunk in data.chunks(CLIP_CHUNK) {
        send.write_all(chunk).await.map_err(std::io::Error::other)?;
    }
    send.finish().map_err(std::io::Error::other)?;
    Ok(())
}

/// Requester side: read the [`ClipFetchHdr`] the holder sends before any data chunks.
pub async fn read_fetch_hdr(recv: &mut quinn::RecvStream) -> std::io::Result<ClipFetchHdr> {
    let raw = io::read_msg(recv).await?;
    ClipFetchHdr::decode(&raw)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad ClipFetchHdr"))
}

/// Requester side: after an OK [`ClipFetchHdr`], drain the data chunks to a `Vec`, bounded by
/// `max_bytes` (the requester's size cap — a breach errors, and the caller resets the stream).
pub async fn read_data(recv: &mut quinn::RecvStream, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    recv.read_to_end(max_bytes)
        .await
        .map_err(std::io::Error::other)
}
