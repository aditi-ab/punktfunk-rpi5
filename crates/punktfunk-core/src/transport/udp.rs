//! Real UDP datagram transport — native sockets, no async runtime.
//!
//! M1 uses one `recv` syscall per packet; the latency budget (§7) calls for
//! `sendmmsg`/UDP-GSO batching to cut syscalls, which is a P2 optimization layered on
//! this same [`Transport`] seam.

use super::Transport;
use crate::packet::MAX_DATAGRAM_BYTES;
use std::net::UdpSocket;

/// Receive buffer size. `Config::validate` bounds `shard_payload` so a well-formed
/// datagram (header + shard + crypto overhead) always fits in [`MAX_DATAGRAM_BYTES`];
/// the `+ 1` byte lets us detect an oversized datagram (a full read) instead of
/// silently truncating it.
const RECV_BUF: usize = MAX_DATAGRAM_BYTES + 1;

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind `local` and `connect` to `peer`, so `send`/`recv` need no address and the
    /// kernel filters to this peer. Non-blocking, matching the [`Transport`] contract.
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.connect(peer)?;
        socket.set_nonblocking(true)?;
        Ok(UdpTransport { socket })
    }
}

impl Transport for UdpTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        self.socket.send(packet)?;
        Ok(())
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; RECV_BUF];
        match self.socket.recv(&mut buf) {
            // A read that fills the whole buffer means the datagram was larger than any
            // valid packet — drop it rather than hand a truncated, corrupt packet up.
            Ok(n) if n >= RECV_BUF => Ok(None),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}
