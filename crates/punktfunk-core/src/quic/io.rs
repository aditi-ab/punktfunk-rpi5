//! Length-prefixed QUIC control frames: `u16` LE length, then payload, capped at 64 KiB.
//!
//! [`read_msg`] is not cancel-safe (two `read_exact`s; quinn keeps consumed bytes only
//! in the future). Handshake/pairing, run-to-completion, only.
//! [`MsgReader`] stores the partial frame in `buf` so a dropped future resumes.
//! Tests at the foot pin mid-frame cancel.
/// Read one framed message. **Not cancel-safe**: two `read_exact`s; drop misaligns
/// the stream. Sequential handshake/pairing only. Use [`MsgReader`] under `select!`
/// or `timeout`.
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

/// Cancel-safe framed reader: the in-progress frame lives in `buf`, not the future.
/// Dropping the future (a `select!` sibling or a read timeout) resumes. [`read_msg`]
/// permanently misaligns a frame that straddles two wakeups.
pub struct MsgReader {
    recv: quinn::RecvStream,
    /// In-progress frame, length prefix included.
    buf: Vec<u8>,
    /// Target length: 2 while reading the prefix, then `2 + payload`.
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

    /// Read one framed message. Cancel-safe: drop keeps the partial frame.
    pub async fn read_msg(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            while self.buf.len() < self.need {
                let mut chunk = [0u8; 2048];
                let want = (self.need - self.buf.len()).min(chunk.len());
                // `read` is cancel-safe: it reports only the bytes it returns, and those
                // land in `self.buf` before the next await.
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
                    return Ok(Vec::new());
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

pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
    send.write_all(&super::frame(payload))
        .await
        .map_err(std::io::Error::other)
}

/// [`MsgReader`] must survive a dropped read future (`select!` / timeout).
#[cfg(test)]
mod tests {
    use crate::quic::io;
    use crate::quic::test_util::connect_pair;

    #[tokio::test]
    async fn cancelled_mid_frame_read_resumes_without_desync() {
        let (_server_ep, _client_ep, host_conn, client_conn) = connect_pair().await;

        let first = b"the-frame-that-straddles-two-wakeups".to_vec();
        let second = b"the-frame-after-it".to_vec();
        let (f1, f2) = (first.clone(), second.clone());

        let writer = tokio::spawn(async move {
            let (mut send, _recv) = host_conn.open_bi().await.expect("open bi");
            let framed = crate::quic::frame(&f1);
            // Prefix + partial payload, then a pause so the timeout can cancel mid-frame.
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

        // Cancel before the tail arrives — same drop as a sibling `select!` arm.
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

    /// Zero-length is a legal encoding; must not stall the reader or eat the next frame.
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
