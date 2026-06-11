//! Pluggable packet I/O. The hot path calls [`Transport::send`] / [`Transport::recv`]
//! directly — no async runtime is involved.

mod loopback;
mod udp;

pub use loopback::{loopback_pair, LoopbackTransport};
pub use udp::UdpTransport;

/// A datagram transport. `recv` is non-blocking: it returns `Ok(None)` when no packet
/// is currently available, so the caller (decode/present thread) never blocks here.
pub trait Transport: Send + Sync {
    /// Send one packet. `Ok(true)` = handed to the kernel; `Ok(false)` = dropped locally because
    /// the send buffer was momentarily full (WouldBlock) — a non-fatal loss the FEC/keyframe path
    /// recovers, surfaced so the caller can count it (`packets_send_dropped`) instead of it being
    /// invisible. `Err` = a real send failure.
    fn send(&self, packet: &[u8]) -> std::io::Result<bool>;

    /// Send a whole frame's packets in as few syscalls as possible, returning how many were
    /// handed to the kernel (the caller counts `packets.len() - sent` as send-buffer drops). This
    /// is the 1 Gbps+ lever: the [`UdpTransport`](super::UdpTransport) override uses `sendmmsg`
    /// (~64 packets/syscall) instead of one `send` each — at ~125k pkt/s that is the difference
    /// between ~2k and ~125k syscalls/sec. The default is the scalar `send` loop (correct for the
    /// loopback transport and non-Linux builds). On a full send buffer it stops early and reports
    /// the partial count rather than blocking — same lossy, FEC-protected contract as `send`.
    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        let mut sent = 0;
        for p in packets {
            if self.send(p)? {
                sent += 1;
            }
        }
        Ok(sent)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;
}
