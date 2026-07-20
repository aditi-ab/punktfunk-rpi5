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

/// The control stream is read from a `select!` arm on both peers, so the read future is dropped
/// routinely — and quinn documents `read_exact` (what `io::read_msg` uses) as NOT cancel-safe.
/// [`io::MsgReader`] must survive that: the partial frame lives in the reader, not the future.
#[cfg(test)]
mod tests {
    use crate::quic::io;
    use crate::quic::test_util::connect_pair;

    /// A frame whose halves land in different wakeups, with the read cancelled in between, must
    /// still be delivered whole — and the NEXT frame must decode correctly too. Without a
    /// resumable reader the consumed length prefix is lost, the following read takes two payload
    /// bytes as a length, and every later control message is garbage for the rest of the session.
    #[tokio::test]
    async fn cancelled_mid_frame_read_resumes_without_desync() {
        let (_server_ep, _client_ep, host_conn, client_conn) = connect_pair().await;

        let first = b"the-frame-that-straddles-two-wakeups".to_vec();
        let second = b"the-frame-after-it".to_vec();
        let (f1, f2) = (first.clone(), second.clone());

        let writer = tokio::spawn(async move {
            let (mut send, _recv) = host_conn.open_bi().await.expect("open bi");
            let framed = crate::quic::frame(&f1);
            // Length prefix + only part of the payload, then a real pause: this is the ClipOffer
            // -sized frame split across two QUIC packets that made the bug reachable.
            let split = 2 + f1.len() / 3;
            send.write_all(&framed[..split]).await.expect("write head");
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            send.write_all(&framed[split..]).await.expect("write tail");
            send.write_all(&crate::quic::frame(&f2))
                .await
                .expect("write second");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            host_conn
        });

        let (_send, recv) = client_conn.accept_bi().await.expect("accept bi");
        let mut reader = io::MsgReader::new(recv);

        // Cancel mid-frame — exactly what a sibling `select!` arm does.
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(30), reader.read_msg()).await;
        assert!(
            cancelled.is_err(),
            "the head-only frame must not complete yet (test setup)"
        );

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_msg())
            .await
            .expect("first frame must arrive after resuming")
            .expect("first frame reads cleanly");
        assert_eq!(got, first, "the cancelled read must resume, not lose bytes");

        let got2 = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_msg())
            .await
            .expect("second frame must arrive")
            .expect("second frame reads cleanly");
        assert_eq!(got2, second, "stream must still be framed correctly");

        let _host_conn = writer.await.unwrap();
    }

    /// A zero-length frame is a legal encoding and must not stall the reader or eat the next one.
    #[tokio::test]
    async fn zero_length_frame_round_trips() {
        let (_server_ep, _client_ep, host_conn, client_conn) = connect_pair().await;
        let writer = tokio::spawn(async move {
            let (mut send, _recv) = host_conn.open_bi().await.expect("open bi");
            send.write_all(&crate::quic::frame(&[])).await.unwrap();
            send.write_all(&crate::quic::frame(b"after")).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            host_conn
        });
        let (_send, recv) = client_conn.accept_bi().await.expect("accept bi");
        let mut reader = io::MsgReader::new(recv);
        assert!(reader.read_msg().await.unwrap().is_empty());
        assert_eq!(reader.read_msg().await.unwrap(), b"after");
        let _host_conn = writer.await.unwrap();
    }
}
