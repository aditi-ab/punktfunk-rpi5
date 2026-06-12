//! Real UDP datagram transport — native sockets, no async runtime.
//!
//! Send is batched via `sendmmsg` ([`Transport::send_batch`], ≤64/syscall) and recv via `recvmmsg`
//! ([`Transport::recv_batch`], ≤32/syscall into a reused ring) — the 1 Gbps+ syscall lever
//! (~125k → a few-k syscalls/sec at line rate). The host additionally paces each frame's send
//! across the frame interval (see `m3.rs::paced_submit`) so a real NIC doesn't drop a line-rate
//! burst. All three layer on this same [`Transport`] seam (scalar fallbacks for loopback/non-Linux).

use super::Transport;
use crate::packet::MAX_DATAGRAM_BYTES;
use std::net::UdpSocket;

/// Receive buffer size. `Config::validate` bounds `shard_payload` so a well-formed
/// datagram (header + shard + crypto overhead) always fits in [`MAX_DATAGRAM_BYTES`];
/// the `+ 1` byte lets us detect an oversized datagram (a full read) instead of
/// silently truncating it.
const RECV_BUF: usize = MAX_DATAGRAM_BYTES + 1;

/// Build one `mmsghdr` per `iovec` (each a single-buffer message) for `sendmmsg`/`recvmmsg`. Shared
/// by `send_batch` + `recv_batch` so the raw-pointer scaffolding lives in exactly one place.
///
/// SAFETY (caller's): each returned header holds a raw pointer into `iovs`; the caller MUST keep
/// `iovs` alive and unmoved for as long as the headers are passed to the syscall.
#[cfg(target_os = "linux")]
fn mmsghdrs(iovs: &mut [libc::iovec]) -> Vec<libc::mmsghdr> {
    iovs.iter_mut()
        .map(|iov| {
            let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
            h.msg_hdr.msg_iov = iov;
            h.msg_hdr.msg_iovlen = 1;
            h
        })
        .collect()
}

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
    ///
    /// Sized for 1 Gbps+: at ~1.2 Gbps on the wire an 8 MB buffer is only ~49 ms of steady state,
    /// and a single multi-MB IDR keyframe (~4 MB ≈ 3300 packets) instantly fills most of it. 32 MB
    /// gives ~200 ms of headroom and absorbs a keyframe burst without EAGAIN drops. (Paced sending
    /// — `m3.rs::paced_submit` — now spreads a big frame's overflow, so this buffer mostly absorbs
    /// the immediate microburst rather than a whole unpaced frame.)
    const TARGET_SOCKBUF: usize = 32 * 1024 * 1024;

    /// Bind `local` and `connect` to `peer`, so `send`/`recv` need no address and the
    /// kernel filters to this peer. Non-blocking, matching the [`Transport`] contract.
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.connect(peer)?;
        Self::grow_buffers(&socket);
        socket.set_nonblocking(true)?;
        Ok(UdpTransport { socket })
    }

    /// The bound local address (e.g. to learn the OS-assigned ephemeral port).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
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
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        match self.socket.send(packet) {
            Ok(_) => Ok(true),
            // The kernel UDP send buffer is momentarily full (a frame burst saturated the
            // tx queue — common right after attaching to an already-running source that
            // emits at full rate, and the dominant failure mode at 1 Gbps+). Drop this packet
            // rather than fail the whole stream: the data plane is lossy + FEC-protected and the
            // next frame/RFI keyframe recovers, whereas blocking would queue stale frames and add
            // latency, and erroring tears the session down. `Ok(false)` surfaces the drop so the
            // session counts it (`packets_send_dropped`) instead of it being invisible. Mirrors
            // the `recv` WouldBlock handling above.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Batched send via `sendmmsg` (up to 64 datagrams per syscall) — the connected socket needs
    /// no per-message address. The socket is non-blocking, so a full send buffer surfaces as a
    /// short count (or `EAGAIN` with nothing sent); we stop and report what went out rather than
    /// block or retry — the data plane is lossy + FEC-protected, and blocking would queue stale
    /// frames + add latency. Ports the proven GameStream `sendmmsg_all`. Non-Linux falls back to
    /// the trait's scalar `send` loop (no `sendmmsg`).
    #[cfg(target_os = "linux")]
    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        const CHUNK: usize = 64;
        let fd = self.socket.as_raw_fd();
        let mut total_sent = 0usize;
        for chunk in packets.chunks(CHUNK) {
            // `hdrs` borrow `iovs` by raw pointer; both stay alive through the `sendmmsg` call.
            let mut iovs: Vec<libc::iovec> = chunk
                .iter()
                .map(|p| libc::iovec {
                    iov_base: p.as_ptr() as *mut libc::c_void,
                    iov_len: p.len(),
                })
                .collect();
            let mut hdrs = mmsghdrs(&mut iovs);
            let n = unsafe { libc::sendmmsg(fd, hdrs.as_mut_ptr(), hdrs.len() as libc::c_uint, 0) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                // Nothing fit in the send buffer — drop this + the remaining chunks (counted by
                // the caller). A real error (not WouldBlock) still tears the session down.
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(err);
            }
            total_sent += n as usize;
            if (n as usize) < chunk.len() {
                break; // buffer filled mid-chunk — drop the remainder
            }
        }
        Ok(total_sent)
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

    /// Batched receive via `recvmmsg` — drains up to `out.len()` datagrams in one syscall into the
    /// caller's reused buffers (no per-packet allocation). `MSG_DONTWAIT` keeps it non-blocking
    /// (the socket already is); `EAGAIN` → `0`. A datagram larger than a buffer is truncated and
    /// `lens[i]` reaches the buffer size — the reassembler then rejects it as malformed, matching
    /// `recv`'s oversized-drop. Apple/BSD use the `recv`-loop override below; other non-unix the
    /// trait's scalar default.
    #[cfg(target_os = "linux")]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.socket.as_raw_fd();
        let n_bufs = out.len().min(lens.len());
        if n_bufs == 0 {
            return Ok(0);
        }
        // `hdrs` borrow `iovs` (one per buffer) by raw pointer; both live through the recvmmsg call.
        let mut iovs: Vec<libc::iovec> = out[..n_bufs]
            .iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            })
            .collect();
        let mut hdrs = mmsghdrs(&mut iovs);
        let n = unsafe {
            libc::recvmmsg(
                fd,
                hdrs.as_mut_ptr(),
                n_bufs as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
        for (i, h) in hdrs[..n as usize].iter().enumerate() {
            lens[i] = h.msg_len as usize;
        }
        Ok(n as usize)
    }

    /// Batched receive for Apple/BSD targets, which have no `recvmmsg(2)`. Drains up to `out.len()`
    /// datagrams per call with `libc::recv(MSG_DONTWAIT)` straight into the caller's reused `out[i]`
    /// buffers — eliminating the per-packet 2 KB `vec!` allocation (and its zeroing + a copy) that
    /// the scalar `recv` + trait-default `recv_batch` incur. THIS is the macOS-client throughput
    /// fix: at line rate the alloc/free churn — not the syscall — was the single-core wall (Moonlight
    /// batches; our client per-packet-allocated). It is still one syscall per datagram (a future
    /// `recvmsg_x` batch would cut that too); `EAGAIN` ends the drain. Oversized datagrams set
    /// `lens[i] == buf.len()` and the caller (`poll_frame`) drops them — same contract as `recvmmsg`.
    #[cfg(all(unix, not(target_os = "linux")))]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.socket.as_raw_fd();
        let n_bufs = out.len().min(lens.len());
        let mut got = 0usize;
        while got < n_bufs {
            let buf = &mut out[got];
            let r = unsafe {
                libc::recv(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    break; // socket drained
                }
                if got > 0 {
                    break; // report what we have; surface the error on the next empty poll
                }
                return Err(err);
            }
            lens[got] = r as usize;
            got += 1;
        }
        Ok(got)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    /// `send_batch` delivers a whole frame's worth of packets over real loopback UDP — exercising
    /// the `sendmmsg` path on Linux (the scalar-loop default elsewhere). 100 × 200 B = 20 KB fits
    /// the socket buffer, so loopback is lossless and every packet must arrive intact + in order.
    #[test]
    fn send_batch_delivers_over_loopback() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let rx_addr = rx.local_addr().unwrap().to_string();
        let tx = UdpTransport::connect("127.0.0.1:0", &rx_addr).unwrap();

        const N: u32 = 100;
        let payloads: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut v = vec![0u8; 200];
                v[0..4].copy_from_slice(&i.to_le_bytes());
                v
            })
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
        let sent = tx.send_batch(&refs).unwrap();
        assert_eq!(
            sent, N as usize,
            "send_batch should hand all packets to the kernel"
        );

        let mut seen = std::collections::HashSet::new();
        let mut buf = [0u8; 2048];
        while seen.len() < N as usize {
            match rx.recv(&mut buf) {
                Ok(n) => {
                    assert_eq!(
                        n, 200,
                        "datagram boundaries preserved (one packet per recv)"
                    );
                    seen.insert(u32::from_le_bytes(buf[0..4].try_into().unwrap()));
                }
                Err(_) => break, // read timeout — stop and let the assert report the shortfall
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every batched packet should arrive over loopback"
        );
    }

    /// `recv_batch` drains many datagrams per call over real loopback UDP — exercising `recvmmsg`
    /// on Linux (the scalar `recv` default elsewhere). Send 50 distinct packets, then drain in
    /// batches and assert every one arrives intact with the right length.
    #[test]
    fn recv_batch_drains_over_loopback() {
        // Receiver is the UdpTransport (the thing under test); sender is a raw socket bound to a
        // known addr so the connected receiver accepts its datagrams.
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx_addr = tx.local_addr().unwrap().to_string();
        let rx = UdpTransport::connect("127.0.0.1:0", &tx_addr).unwrap();
        let rx_addr = rx.local_addr().unwrap();

        const N: u32 = 50;
        for i in 0..N {
            let mut p = vec![0u8; 300];
            p[0..4].copy_from_slice(&i.to_le_bytes());
            tx.send_to(&p, rx_addr).unwrap();
        }

        let mut bufs: Vec<Vec<u8>> = (0..16).map(|_| vec![0u8; RECV_BUF]).collect();
        let mut lens = vec![0usize; 16];
        let mut seen = std::collections::HashSet::new();
        // A few drains absorb scheduling jitter; stop once all N are in or we go dry.
        for _ in 0..50 {
            let n = rx.recv_batch(&mut bufs, &mut lens).unwrap();
            if n == 0 {
                if seen.len() == N as usize {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            for i in 0..n {
                assert_eq!(lens[i], 300, "recvmmsg reports the datagram length");
                seen.insert(u32::from_le_bytes(bufs[i][0..4].try_into().unwrap()));
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every datagram should be drained via recv_batch"
        );
    }
}
