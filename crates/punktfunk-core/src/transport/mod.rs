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

    /// Send a frame's equal-size packets using UDP Generic Segmentation Offload where available:
    /// one `sendmsg` hands the kernel a big buffer it splits into `gso_size` UDP datagrams, building
    /// ~1 GSO skb per ≤64 segments instead of one skb per packet. This is the multi-Gbps lever —
    /// research shows ~2.4× throughput at equal CPU and ~40× fewer syscalls, and that `sendmmsg`
    /// batching alone is insufficient (it still builds one skb per datagram). The
    /// [`UdpTransport`](super::UdpTransport) Linux override implements it (opt-in via `PUNKTFUNK_GSO`,
    /// auto-fallback on any GSO error); the default just delegates to [`send_batch`](Self::send_batch),
    /// correct for loopback and non-Linux. Same lossy, FEC-protected short-count contract as `send_batch`.
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        self.send_batch(packets)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;

    /// Receive up to `out.len()` datagrams in as few syscalls as possible, writing each into its
    /// `out[i]` buffer (sized ≥ a max datagram) and its byte count into `lens[i]`; returns how many
    /// arrived (`0` = none available; non-blocking). The recv counterpart of [`send_batch`]: the
    /// [`UdpTransport`](super::UdpTransport) override uses `recvmmsg` into a caller-owned, reused
    /// buffer ring — no per-packet allocation or syscall at line rate. The default does a single
    /// scalar [`recv`](Self::recv) into `out[0]` (correct for the loopback transport + non-Linux).
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        match self.recv()? {
            Some(pkt) => {
                let n = pkt.len().min(out[0].len());
                out[0][..n].copy_from_slice(&pkt[..n]);
                lens[0] = n;
                Ok(1)
            }
            None => Ok(0),
        }
    }
}
