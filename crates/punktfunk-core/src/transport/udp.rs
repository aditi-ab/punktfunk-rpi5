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
    /// Target kernel socket-buffer size. A high-resolution frame is a burst (a 5120×1440
    /// keyframe is ~130 packets the send thread hands to `sendmmsg` at once); the default
    /// UDP buffer (~208 KB on Linux) overflows on it, which EAGAINs the host send (dropping
    /// packets) or drops on the client recv — and with infinite-GOP a single lost frame
    /// freezes the decode until the next RFI refresh. Requested large; the OS clamps to
    /// `net.core.{wmem,rmem}_max` (Linux) / `kern.ipc.maxsockbuf` (macOS).
    const TARGET_SOCKBUF: usize = 8 * 1024 * 1024;

    /// Bind `local` and `connect` to `peer`, so `send`/`recv` need no address and the
    /// kernel filters to this peer. Non-blocking, matching the [`Transport`] contract.
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.connect(peer)?;
        Self::grow_buffers(&socket);
        socket.set_nonblocking(true)?;
        Ok(UdpTransport { socket })
    }

    /// Best-effort grow of SO_SNDBUF/SO_RCVBUF (see [`TARGET_SOCKBUF`]). A failure isn't fatal
    /// (the stream just runs lossier); a grant far below the request means the OS cap is too
    /// low for clean 4K/5K streaming, so warn once with the knob to raise.
    fn grow_buffers(socket: &UdpSocket) {
        let sock = socket2::SockRef::from(socket);
        let _ = sock.set_send_buffer_size(Self::TARGET_SOCKBUF);
        let _ = sock.set_recv_buffer_size(Self::TARGET_SOCKBUF);
        // The kernel reports back the (possibly clamped, Linux-doubled) granted size.
        let granted = sock
            .send_buffer_size()
            .unwrap_or(0)
            .min(sock.recv_buffer_size().unwrap_or(0));
        if granted < Self::TARGET_SOCKBUF / 4 {
            tracing::warn!(
                granted_kb = granted / 1024,
                "UDP socket buffer capped well below target — high-resolution streaming may drop \
                 frames; raise net.core.wmem_max / net.core.rmem_max (Linux) for clean 4K/5K"
            );
        }
    }
}

impl Transport for UdpTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        match self.socket.send(packet) {
            Ok(_) => Ok(()),
            // The kernel UDP send buffer is momentarily full (a frame burst saturated the
            // tx queue — common right after attaching to an already-running source that
            // emits at full rate). Drop this packet rather than fail the whole stream: the
            // data plane is lossy + FEC-protected and the next frame/RFI keyframe recovers,
            // whereas blocking would queue stale frames and add latency, and erroring tears
            // the session down. Mirrors the `recv` WouldBlock handling above.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
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
