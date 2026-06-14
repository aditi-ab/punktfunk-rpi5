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

/// UDP GSO enable state (process-wide). Opt-in via `PUNKTFUNK_GSO` — it's new unsafe hot-path code,
/// and the auto-fallback (latch off on any GSO syscall error) covers kernels/paths without support.
#[cfg(target_os = "linux")]
mod gso {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let on = std::env::var_os("PUNKTFUNK_GSO").is_some();
                STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }
    /// Latch GSO off for the process after a GSO syscall error (unsupported kernel/path).
    pub fn disable() {
        STATE.store(2, Ordering::Relaxed);
    }
}

/// True if the send error means UDP GSO isn't supported here (vs a transient/real failure) — so we
/// latch GSO off and fall back to `sendmmsg` rather than tear the stream down.
#[cfg(target_os = "linux")]
fn gso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOPROTOOPT) | Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) | Some(libc::EIO)
    )
}

/// One `sendmsg` carrying a `UDP_SEGMENT` control message: the kernel splits `buf` (a back-to-back
/// concatenation of equal-size datagrams, only the final one allowed shorter) into `gso_size`-byte
/// UDP datagrams to the connected peer — one large GSO skb instead of N. `EAGAIN` (full send buffer)
/// surfaces as a `WouldBlock` error; the caller treats it as a lossy drop.
#[cfg(target_os = "linux")]
fn send_one_gso(fd: libc::c_int, buf: &[u8], gso_size: u16) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // Aligned control buffer for one cmsg(UDP_SEGMENT = u16). 64 B > CMSG_SPACE(2); the union forces
    // cmsghdr alignment (CMSG_FIRSTHDR requires it).
    #[repr(C)]
    union CmsgBuf {
        _align: libc::cmsghdr,
        bytes: [u8; 64],
    }
    let mut control = CmsgBuf { bytes: [0u8; 64] };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let rc = unsafe {
        msg.msg_control = control.bytes.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<u16>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_UDP;
        (*cmsg).cmsg_type = libc::UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&gso_size as *const u16) as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<u16>(),
        );
        libc::sendmsg(fd, &msg, 0)
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Apple (macOS/iOS) batched-receive enable state. Darwin has no `recvmmsg(2)`, so without this our
/// macOS client does one `recv` syscall per packet — at a few hundred Mbps that's ~40-90k syscalls/s
/// on one core, and when the recv loop can't drain fast enough the kernel socket buffer backs up and
/// drops, which the client sees as a sustained stream stalling/freezing around 300-400 Mbps.
/// `recvmsg_x(2)` is the batched equivalent (the recv counterpart of Linux `recvmmsg`), cutting the
/// syscall rate ~30x. **Default ON** (the multi-Gbps Mac path); the `swift test` loopback on the
/// Apple CI runner exercises it, and it auto-falls-back to the scalar loop if the syscall ever errors
/// unexpectedly. Set `PUNKTFUNK_RECVMSG_X=0` to force the scalar fallback.
#[cfg(target_vendor = "apple")]
mod recvx {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                // On unless explicitly disabled with PUNKTFUNK_RECVMSG_X=0.
                let on = std::env::var("PUNKTFUNK_RECVMSG_X")
                    .map(|v| v != "0")
                    .unwrap_or(true);
                STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }
    pub fn disable() {
        STATE.store(2, Ordering::Relaxed);
    }
}

/// `struct msghdr_x` from Darwin `<sys/socket.h>` (the batched-I/O variant — not in the `libc` crate).
#[cfg(target_vendor = "apple")]
#[repr(C)]
struct MsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: libc::size_t,
}

#[cfg(target_vendor = "apple")]
extern "C" {
    /// Darwin batched receive: up to `cnt` datagrams in one syscall; returns the count received and
    /// sets each `msg_datalen` to its byte length. Present in libSystem on all macOS/iOS.
    fn recvmsg_x(
        s: libc::c_int,
        msgp: *mut MsghdrX,
        cnt: libc::c_uint,
        flags: libc::c_int,
    ) -> libc::ssize_t;
}

/// Data-plane NAT/firewall hole-punch marker. The video data plane is a raw UDP socket distinct
/// from the QUIC control connection; on a flat LAN the host can send straight to the client, but
/// across a NAT or a stateful inter-VLAN firewall the unsolicited host→client video is rejected
/// (ICMP port-unreachable). So the client sends these tiny datagrams FROM its data socket TO the
/// host's data port: that opens the firewall/NAT return path and lets the host learn the client's
/// *observed* source (the NAT-translated address, not the client's reported private one). It's the
/// only thing a client ever sends on the data plane (video is host→client), so the host treats any
/// punch-magic datagram purely as a source-address probe and never as stream data.
pub const PUNCH_MAGIC: &[u8] = b"PFpunch1";

/// Spawn the client-side data-plane hole-punch keepalive. `sock` is a clone of the data socket
/// (already `connect`ed to the host's data port — see [`UdpTransport::try_clone_socket`]). Bursts
/// fast at first to open the NAT/firewall path before the host's punch-wait expires, then steady
/// keepalive so a stateful firewall's idle timeout can't close the path during a static, low-bitrate
/// scene. Stops when `stop` is set (session teardown) or the socket closes. No-op cost on a flat LAN.
pub fn spawn_data_punch(sock: UdpSocket, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::Builder::new()
        .name("punktfunk-data-punch".into())
        .spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if sock.send(PUNCH_MAGIC).is_err() {
                    break;
                }
                let delay_ms = if i < 15 { 200 } else { 2000 };
                i = i.saturating_add(1);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
        })
        .ok();
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

    /// Host side of the data plane for clients that may sit behind NAT / a stateful inter-VLAN
    /// firewall. Bind `local`, then block up to `punch_timeout` for the client's first
    /// [`PUNCH_MAGIC`] datagram and `connect` to its *observed* source — so video flows back
    /// through the path the client just opened, to the address+port the host actually sees (the
    /// NAT-translated one, which can differ from the client-reported `fallback_peer`). If no punch
    /// arrives (a client that doesn't hole-punch), fall back to `fallback_peer` — the same flat-LAN
    /// behaviour as [`connect`](Self::connect). Returns `(transport, punched)`.
    pub fn connect_via_punch(
        local: &str,
        fallback_peer: &str,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
        let socket = UdpSocket::bind(local)?;
        socket.set_read_timeout(Some(punch_timeout))?;
        let deadline = std::time::Instant::now() + punch_timeout;
        let mut buf = [0u8; 64];
        let mut observed: Option<std::net::SocketAddr> = None;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, src))
                    if n >= PUNCH_MAGIC.len() && &buf[..PUNCH_MAGIC.len()] == PUNCH_MAGIC =>
                {
                    observed = Some(src);
                    break;
                }
                Ok(_) => {} // stray datagram — keep waiting for a real punch
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e),
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        let punched = observed.is_some();
        let target = observed.map(|s| s.to_string());
        socket.connect(target.as_deref().unwrap_or(fallback_peer))?;
        socket.set_read_timeout(None)?;
        Self::grow_buffers(&socket);
        socket.set_nonblocking(true)?;
        Ok((UdpTransport { socket }, punched))
    }

    /// A second handle to the data socket, for sending hole-punch keepalives ([`PUNCH_MAGIC`])
    /// while the [`Session`](crate::Session) owns the transport. The socket is already `connect`ed
    /// to the host's data port, so `clone.send(PUNCH_MAGIC)` reaches it with no address.
    pub fn try_clone_socket(&self) -> std::io::Result<UdpSocket> {
        self.socket.try_clone()
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

    /// Apple batched receive via `recvmsg_x` — drains up to `out.len()` datagrams in one syscall into
    /// the caller's reused buffers (the recv counterpart of Linux `recvmmsg`, which Darwin lacks).
    /// SAFETY: each `MsghdrX` holds a raw pointer into `iovs`, which holds raw pointers into `out`'s
    /// buffers; both `iovs` and `msgs` stay alive and unmoved through the syscall.
    #[cfg(target_vendor = "apple")]
    fn recv_batch_x(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.socket.as_raw_fd();
        let n_bufs = out.len().min(lens.len());
        if n_bufs == 0 {
            return Ok(0);
        }
        let mut iovs: Vec<libc::iovec> = out[..n_bufs]
            .iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            })
            .collect();
        let mut msgs: Vec<MsghdrX> = iovs
            .iter_mut()
            .map(|iov| {
                let mut m: MsghdrX = unsafe { std::mem::zeroed() };
                m.msg_iov = iov as *mut libc::iovec;
                m.msg_iovlen = 1;
                m
            })
            .collect();
        let n = unsafe {
            recvmsg_x(
                fd,
                msgs.as_mut_ptr(),
                n_bufs as libc::c_uint,
                libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
        for (i, m) in msgs[..n as usize].iter().enumerate() {
            lens[i] = m.msg_datalen;
        }
        Ok(n as usize)
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

    /// UDP GSO send (see [`Transport::send_gso`]). Coalesces the frame's equal-size packets into a
    /// reused scratch buffer and hands the kernel ≤64-segment super-buffers via `sendmsg(UDP_SEGMENT)`
    /// — one GSO skb per chunk instead of one per packet, the multi-Gbps lever. Opt-in
    /// (`PUNKTFUNK_GSO`); falls back to `send_batch` when off, when packets aren't uniform-size, or on
    /// any GSO error (which also latches it off for the process). Same lossy short-count contract.
    #[cfg(target_os = "linux")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        if packets.is_empty() {
            return Ok(0);
        }
        if !gso::active() {
            return self.send_batch(packets);
        }
        // GSO needs every segment but the last to be exactly `seg` bytes. Our wire packets are all
        // identical size (shards zero-padded to shard_payload), but guard and fall back if not.
        let seg = packets[0].len();
        let last = packets.len() - 1;
        if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
            return self.send_batch(packets);
        }
        let fd = self.socket.as_raw_fd();
        // A GSO super-buffer is capped at 64 segments AND 65535 payload bytes (kernel limits).
        let max_seg = (65535 / seg).clamp(1, 64);
        let mut scratch: Vec<u8> = Vec::with_capacity(seg * max_seg);
        let mut sent = 0usize;
        for chunk in packets.chunks(max_seg) {
            scratch.clear();
            for p in chunk {
                scratch.extend_from_slice(p);
            }
            match send_one_gso(fd, &scratch, seg as u16) {
                Ok(()) => sent += chunk.len(),
                // Send buffer momentarily full — drop the rest (counted by the caller), never block.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // GSO unsupported on this kernel/path — latch off and finish via sendmmsg.
                Err(e) if gso_unsupported(&e) => {
                    gso::disable();
                    return Ok(sent + self.send_batch(&packets[sent..])?);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
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
        // Apple: prefer the batched `recvmsg_x` syscall when enabled; a surprise error disables it
        // and falls through to the always-correct scalar loop below.
        #[cfg(target_vendor = "apple")]
        if recvx::active() {
            match self.recv_batch_x(out, lens) {
                Ok(n) => return Ok(n),
                Err(_) => recvx::disable(),
            }
        }
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

    /// `send_one_gso` must split one buffer into N separate UDP datagrams of `gso_size` bytes each
    /// (the kernel UDP GSO segmentation) — the multi-Gbps send lever. Loopback supports GSO; if the
    /// CI kernel doesn't, skip rather than fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn gso_segments_into_separate_datagrams() {
        use std::os::fd::AsRawFd;
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let rx_addr = rx.local_addr().unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.connect(rx_addr).unwrap();

        let seg = 1000usize;
        let segs = 5usize;
        let mut buf = vec![0u8; seg * segs];
        for i in 0..segs {
            buf[i * seg..(i + 1) * seg].fill(i as u8 + 1);
        }
        if let Err(e) = send_one_gso(tx.as_raw_fd(), &buf, seg as u16) {
            if gso_unsupported(&e) {
                eprintln!("UDP GSO unsupported on this kernel — skipping");
                return;
            }
            panic!("gso sendmsg failed: {e}");
        }
        // Each segment arrives as its own datagram, full size, content intact.
        let mut rbuf = vec![0u8; 4096];
        for i in 0..segs {
            let n = rx.recv(&mut rbuf).expect("recv GSO segment");
            assert_eq!(n, seg, "segment {i} should be a full {seg}-byte datagram");
            assert!(
                rbuf[..n].iter().all(|&b| b == i as u8 + 1),
                "segment {i} content"
            );
        }
    }

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
