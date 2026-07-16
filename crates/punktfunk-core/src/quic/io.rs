//! Length-prefixed framing for QUIC control-stream messages: a `u16` length header followed by the
//! payload, bounded at 64 KiB (control messages are tiny).
/// Read one framed message (bounded at 64 KiB — control messages are tiny).
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

/// Write one framed message.
pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
    send.write_all(&super::frame(payload))
        .await
        .map_err(std::io::Error::other)
}
