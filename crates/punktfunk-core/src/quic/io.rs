//! Length-prefixed framing for QUIC control-stream messages: a `u16` length header followed by the
//! payload, bounded at 64 KiB (control messages are tiny).
/// Read one framed message (bounded at 64 KiB — control messages are tiny).
///
/// **Not cancel-safe**: it frames with two `quinn::RecvStream::read_exact` calls, and quinn
/// documents `read_exact` as not cancel-safe (the bytes it has already taken out of the stream
/// live only in the future's own buffer, and nothing puts them back on drop). Dropping a
/// partially-progressed future therefore destroys the bytes it consumed and misaligns every
/// subsequent read on that stream. Use it only where the read runs to completion — the sequential
/// handshake/pairing exchanges. Anything driving a read from a `select!` arm or a
/// `tokio::time::timeout` must use [`MsgReader`] instead.
pub async fn read_msg(recv: &mut quinn::RecvStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    recv.read_exact(&mut len)
        .await
        .map_err(std::io::Error::other)?;
    let n = u16::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    recv.read_exact(&mut buf)
        .await
        .map_err(std::io::Error::other)?;
    Ok(buf)
}

/// Cancel-safe framed reader for a long-lived control stream.
///
/// Keeps the frame in progress in `buf` rather than inside the read future, so dropping the future
/// — which both control loops do on every iteration where a sibling `select!` arm wins, and which
/// [`clock_sync`](super::clock_sync) does on a read timeout — resumes instead of losing bytes.
/// With the plain [`read_msg`] a control frame that straddles two wakeups (a ~2 KB `ClipOffer`
/// exceeds one QUIC packet; so does any frame whose second half is lost or reordered) left the
/// stream permanently misaligned: the next read took two payload bytes as a length, every later
/// message decoded as garbage and was silently ignored, and a bogus 64 KiB length parked the read
/// forever — killing mode switches, adaptive bitrate, clock re-sync and clipboard for the rest of
/// the session with nothing but a `warn!` in the log.
pub struct MsgReader {
    recv: quinn::RecvStream,
    /// The frame in progress, length prefix included.
    buf: Vec<u8>,
    /// Bytes `buf` must reach: 2 while reading the prefix, then `2 + payload length`.
    need: usize,
}

impl MsgReader {
    pub fn new(recv: quinn::RecvStream) -> Self {
        MsgReader {
            recv,
            buf: Vec::new(),
            need: 2,
        }
    }

    /// Read one framed message. Cancel-safe: dropping the future keeps the partial frame, so the
    /// next call resumes where this one stopped.
    pub async fn read_msg(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            while self.buf.len() < self.need {
                let mut chunk = [0u8; 2048];
                let want = (self.need - self.buf.len()).min(chunk.len());
                // `read` IS cancel-safe: it only reports bytes it hands back, and they are
                // committed to `self.buf` before the next await point.
                match self
                    .recv
                    .read(&mut chunk[..want])
                    .await
                    .map_err(std::io::Error::other)?
                {
                    Some(n) => self.buf.extend_from_slice(&chunk[..n]),
                    None => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "control stream finished mid-frame",
                        ))
                    }
                }
            }
            if self.need == 2 {
                self.need = 2 + u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
                if self.need == 2 {
                    self.buf.clear();
                    return Ok(Vec::new()); // zero-length frame
                }
            } else {
                let msg = self.buf.split_off(2);
                self.buf.clear();
                self.need = 2;
                return Ok(msg);
            }
        }
    }
}

/// Write one framed message.
pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
    send.write_all(&super::frame(payload))
        .await
        .map_err(std::io::Error::other)
}
