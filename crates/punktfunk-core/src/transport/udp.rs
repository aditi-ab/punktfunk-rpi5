//! Real UDP datagram transport — native sockets, no async runtime.
//!
//! Send is batched via `sendmmsg` ([`Transport::send_batch`], ≤64/syscall) and recv via `recvmmsg`
//! ([`Transport::recv_batch`], ≤32/syscall into a reused ring) on Linux AND Android (which is
//! `target_os = "android"`, not `"linux"` — it needs its own bionic binding, see [`android_mmsg`])
//! — the 1 Gbps+ syscall lever (~125k → a few-k syscalls/sec at line rate). The host additionally
//! paces each frame's send across the frame interval (see `punktfunk1.rs::paced_submit`) so a real
//! NIC doesn't drop a line-rate burst. All three layer on this same [`Transport`] seam (scalar
//! fallbacks for loopback and the remaining targets).

use super::Transport;
use crate::packet::MAX_DATAGRAM_BYTES;
use std::net::UdpSocket;

/// Receive buffer size. `Config::validate` bounds `shard_payload` so a well-formed
/// datagram (header + shard + crypto overhead) always fits in [`MAX_DATAGRAM_BYTES`];
/// the `+ 1` byte lets us detect an oversized datagram (a full read) instead of
/// silently truncating it.
const RECV_BUF: usize = MAX_DATAGRAM_BYTES + 1;

/// True for transient socket conditions that must be a lossy drop / "no data this poll" — NOT a
/// stream teardown. Two cases:
/// - `WouldBlock`: the kernel send/recv buffer is momentarily full (a frame burst saturated the tx
///   queue — the dominant condition at 1 Gbps+). Drop the packet; FEC + the next frame recover.
/// - `ConnectionRefused` / `ConnectionReset`: a *connected* UDP socket received an asynchronous ICMP
///   port-unreachable / reset for an *earlier* datagram. With data-plane hole-punching the path
///   blips — the peer's data socket briefly gone, a NAT rebind, or a stale ICMP from punch setup —
///   so erroring out here kills a stream that the very next packet would resume. If the peer is
///   genuinely gone, the QUIC control plane times out and ends the session cleanly instead. (This is
///   the classic connected-UDP "ICMP errors are advisory" rule, doubly true with hole-punching.)
/// - `ENOBUFS`: a WiFi/wlan driver (e.g. `ath11k` on the Steam Deck) returns this — NOT `EAGAIN`/
///   `WouldBlock` — when its tx queue is momentarily full. Rust maps `ENOBUFS` to
///   `ErrorKind::Uncategorized`, so the `WouldBlock` arm misses it; without this a transient
///   tx-queue burst tears the whole stream down (observed live: the host streamed flawlessly on
///   loopback / under a debugger — anything slow enough not to fill the small wlan0 buffer — but
///   died at full rate over WiFi). Same lossy-drop contract as `WouldBlock`; FEC + the next frame
///   recover. Asynchronous network-path blips (`ENETUNREACH`/`EHOSTUNREACH`/`ENETDOWN`/`EHOSTDOWN`)
///   are droppable for the same reason a stale ICMP is.
fn is_transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, WouldBlock};
    if matches!(e.kind(), WouldBlock | ConnectionRefused | ConnectionReset) {
        return true;
    }
    // `ENOBUFS` & friends have no stable `ErrorKind`, so match the raw errno (unix only).
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOBUFS)
                | Some(libc::ENETUNREACH)
                | Some(libc::EHOSTUNREACH)
                | Some(libc::ENETDOWN)
                | Some(libc::EHOSTDOWN)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// `sendmmsg`/`recvmmsg` + `mmsghdr` for Android, where the `libc` crate binds only the syscall
/// number (`SYS_recvmmsg`) and neither the wrapper functions nor the struct — even though bionic
/// has exported both since API 21 (below our API-28 floor), and Rust's `target_os = "android"` is
/// NOT `"linux"`, so the batched paths below silently excluded Android and the client fell back to
/// one syscall per datagram. The struct layout is stable kernel ABI (`struct mmsghdr` in
/// `linux/socket.h`): a `msghdr` followed by the received byte count.
#[cfg(target_os = "android")]
mod android_mmsg {
    #[repr(C)]
    #[allow(non_camel_case_types)]
    pub struct mmsghdr {
        pub msg_hdr: libc::msghdr,
        pub msg_len: libc::c_uint,
    }
    extern "C" {
        pub fn sendmmsg(
            sockfd: libc::c_int,
            msgvec: *mut mmsghdr,
            vlen: libc::c_uint,
            flags: libc::c_int,
        ) -> libc::c_int;
        pub fn recvmmsg(
            sockfd: libc::c_int,
            msgvec: *mut mmsghdr,
            vlen: libc::c_uint,
            flags: libc::c_int,
            timeout: *mut libc::timespec,
        ) -> libc::c_int;
    }
}
#[cfg(target_os = "android")]
use android_mmsg::{mmsghdr, recvmmsg, sendmmsg};
#[cfg(target_os = "linux")]
use libc::{mmsghdr, recvmmsg, sendmmsg};

/// Build one `mmsghdr` per `iovec` (each a single-buffer message) for `sendmmsg`/`recvmmsg`. Shared
/// by `send_batch` + `recv_batch` so the raw-pointer scaffolding lives in exactly one place.
///
/// SAFETY (caller's): each returned header holds a raw pointer into `iovs`; the caller MUST keep
/// `iovs` alive and unmoved for as long as the headers are passed to the syscall.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mmsghdrs(iovs: &mut [libc::iovec]) -> Vec<mmsghdr> {
    iovs.iter_mut()
        .map(|iov| {
            let mut h: mmsghdr = unsafe { std::mem::zeroed() };
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

/// True if the send error means UDP GSO isn't usable on this kernel/NIC/path (vs a transient/real
/// failure) — so we latch GSO off and fall back to `sendmmsg` rather than tear the stream down.
/// `EMSGSIZE` is the important one in practice: a NIC/egress path whose effective MTU is below our
/// segment size rejects the whole GSO super-buffer at send time (the kernel validates each segment
/// against the device MTU, which plain `sendmmsg` does not) — observed live as a code-90
/// "Message too long" that instantly killed the stream. Treat it as "no GSO here" and fall back.
#[cfg(target_os = "linux")]
fn gso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOPROTOOPT)
            | Some(libc::EOPNOTSUPP)
            | Some(libc::EINVAL)
            | Some(libc::EIO)
            | Some(libc::EMSGSIZE)
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

/// Windows UDP Send Offload (USO) enable state (process-wide). The Windows analogue of Linux UDP
/// GSO: `WSASendMsg` + `UDP_SEND_MSG_SIZE`. **On by default** (the 1 Gbps+ send lever — Windows
/// otherwise does one `send` syscall per packet, which caps throughput at high packet rates). Kill
/// switch `PUNKTFUNK_GSO=0`; auto-fallback latches it off the first time a send reports it
/// unsupported (old OS / NIC / path). We detect support from the send error rather than a
/// `setsockopt` probe — the probe sets a socket-wide default segment size that would fragment plain
/// `send`s of larger-than-segment packets.
#[cfg(target_os = "windows")]
mod uso {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let off = std::env::var_os("PUNKTFUNK_GSO")
                    .map(|v| v == "0")
                    .unwrap_or(false);
                STATE.store(if off { 2 } else { 1 }, Ordering::Relaxed);
                tracing::info!(
                    enabled = !off,
                    "Windows UDP Send Offload (USO): {} (the 1 Gbps+ send lever; PUNKTFUNK_GSO=0 disables)",
                    if off { "off" } else { "on" }
                );
                !off
            }
        }
    }
    /// Latch USO off for the process after a send that means it isn't usable on this OS/NIC/path.
    pub fn disable() {
        if STATE.swap(2, Ordering::Relaxed) != 2 {
            tracing::warn!(
                "Windows USO unsupported on this path — falling back to per-packet sends"
            );
        }
    }
}

/// True if a `WSASendMsg` USO error means USO isn't usable here (vs a transient full-buffer
/// `WouldBlock`, handled by [`is_transient_io`]) — latch it off and fall back to per-packet sends.
/// 10022 WSAEINVAL, 10042 WSAENOPROTOOPT, 10045 WSAEOPNOTSUPP, 10040 WSAEMSGSIZE.
#[cfg(target_os = "windows")]
fn uso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(10022) | Some(10042) | Some(10045) | Some(10040)
    )
}

/// One `WSASendMsg` carrying a `UDP_SEND_MSG_SIZE` control message: Winsock splits `buf` (a
/// back-to-back concatenation of equal-size datagrams, only the final one allowed shorter) into
/// `seg_size`-byte UDP datagrams to the connected peer in ONE syscall — the analogue of
/// [`send_one_gso`]. The `WSA_CMSG_*` helpers are C macros not exported by the `windows` crate, so
/// the cmsg layout math is reimplemented here (ported from quinn-udp's Windows backend).
#[cfg(target_os = "windows")]
fn send_one_uso(socket: &std::net::UdpSocket, buf: &[u8], seg_size: u16) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        WSASendMsg, CMSGHDR, IPPROTO_UDP, UDP_SEND_MSG_SIZE, WSABUF, WSAMSG,
    };
    let align_usize = std::mem::align_of::<usize>();
    let align_hdr = std::mem::align_of::<CMSGHDR>();
    let cmsgdata_align = |n: usize| (n + align_usize - 1) & !(align_usize - 1);
    let cmsghdr_align = |n: usize| (n + align_hdr - 1) & !(align_hdr - 1);
    let hdr = std::mem::size_of::<CMSGHDR>();

    // 8-byte-aligned control buffer; 32 B holds one u32 cmsg (WSA_CMSG_SPACE(4) = 24 on x64).
    #[repr(align(8))]
    struct Aligned([u8; 32]);
    let mut ctrl = Aligned([0u8; 32]);

    let mut data = WSABUF {
        len: buf.len() as u32,
        buf: buf.as_ptr() as *mut u8, // WSASendMsg only reads it
    };
    let mut msg = WSAMSG {
        name: std::ptr::null_mut(),
        namelen: 0,
        lpBuffers: &mut data,
        dwBufferCount: 1,
        Control: WSABUF {
            len: 0,
            buf: ctrl.0.as_mut_ptr(),
        },
        dwFlags: 0,
    };
    let cmsg_len = cmsgdata_align(hdr) + std::mem::size_of::<u32>(); // WSA_CMSG_LEN(4)
    let space = cmsgdata_align(hdr + cmsghdr_align(std::mem::size_of::<u32>())); // WSA_CMSG_SPACE(4)
    unsafe {
        let cmsg = ctrl.0.as_mut_ptr() as *mut CMSGHDR;
        (*cmsg).cmsg_len = cmsg_len;
        (*cmsg).cmsg_level = IPPROTO_UDP;
        (*cmsg).cmsg_type = UDP_SEND_MSG_SIZE;
        let data_ptr = (cmsg as usize + cmsgdata_align(hdr)) as *mut u32;
        std::ptr::write_unaligned(data_ptr, seg_size as u32);
        msg.Control.len = space as u32;
        let mut sent = 0u32;
        let rc = WSASendMsg(
            socket.as_raw_socket() as usize,
            &msg,
            0,
            &mut sent,
            std::ptr::null_mut(),
            None,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Reusable Windows USO batch send for callers that own their OWN connected `UdpSocket` and are not
/// the [`UdpTransport`] data plane — specifically the GameStream video sender, whose paced bursts of
/// equal-size RTP/FEC packets are otherwise sent one `send` syscall at a time on Windows. Coalesces
/// the LEADING run of uniform-size packets into ≤512-segment `WSASendMsg(UDP_SEND_MSG_SIZE)`
/// super-buffers and returns how many packets it sent that way; the caller sends any remainder with
/// its own per-packet path. Returns `Ok(0)` (caller sends everything scalar) when USO is disabled
/// (`PUNKTFUNK_GSO=0`) or the packets aren't uniform-size. On a USO-unsupported error it latches USO
/// off process-wide and returns the count sent so far; a transient full-buffer also returns the
/// count-so-far. Same uniform-size rule and `seg`/512 chunking as the [`UdpTransport`] `send_gso`
/// Windows path, reusing its [`send_one_uso`] primitive.
#[cfg(target_os = "windows")]
pub fn send_uso_all(socket: &std::net::UdpSocket, packets: &[&[u8]]) -> std::io::Result<usize> {
    if packets.is_empty() || !uso::active() {
        return Ok(0);
    }
    // USO needs every segment but the last to be exactly `seg` bytes; bail to the scalar caller path
    // otherwise (a frame's final/short packet or a size-mixed burst).
    let seg = packets[0].len();
    let last = packets.len() - 1;
    if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
        return Ok(0);
    }
    let max_seg = 512usize; // Win11 x64 accepts up to ~512 segments per WSASendMsg
    let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
    let mut sent = 0usize;
    for chunk in packets.chunks(max_seg) {
        scratch.clear();
        for p in chunk {
            scratch.extend_from_slice(p);
        }
        match send_one_uso(socket, &scratch, seg as u16) {
            Ok(()) => sent += chunk.len(),
            // Send buffer momentarily full — stop here; the caller sends the rest (and the pacing
            // loop / blocking socket absorbs it). Never block or tear down here.
            Err(e) if is_transient_io(&e) => break,
            // USO unsupported on this OS/NIC/path — latch off; the caller sends the rest scalar and
            // every later burst skips USO via `uso::active()`.
            Err(e) if uso_unsupported(&e) => {
                uso::disable();
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
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
    /// qWAVE flow guard (Windows, opt-in DSCP): declared before `socket` so drop order removes
    /// the flow membership before the socket closes. Always `None` off-Windows.
    _qos_flow: Option<super::qos::QosFlow>,
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind `local` and `connect` to `peer`, so `send`/`recv` need no address and the
    /// kernel filters to this peer. Non-blocking, matching the [`Transport`] contract.
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        Self::from_socket(UdpSocket::bind(local)?, peer)
    }

    /// Adopt an already-bound socket for the data plane: `connect` it to `peer`, tune buffers +
    /// QoS, go non-blocking. Lets the host bind the data port up front (e.g. a fixed `--data-port`)
    /// and keep the *same* socket from handshake through streaming — no drop-then-rebind window in
    /// which a concurrent session could steal a fixed port.
    pub fn from_socket(socket: UdpSocket, peer: &str) -> std::io::Result<Self> {
        socket.connect(peer)?;
        super::qos::grow_socket_buffers(&socket);
        // The native data plane is video-dominant — tag it as the video class (opt-in via
        // PUNKTFUNK_DSCP). Each end marks its own egress; the socket is connected by now, as
        // the Windows qWAVE flow requires.
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok(UdpTransport {
            _qos_flow: qos_flow,
            socket,
        })
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
        Self::from_socket_punch(UdpSocket::bind(local)?, fallback_peer, punch_timeout)
    }

    /// [`connect_via_punch`](Self::connect_via_punch) on an already-bound socket — see
    /// [`from_socket`](Self::from_socket) for why the host binds the data port up front.
    pub fn from_socket_punch(
        socket: UdpSocket,
        fallback_peer: &str,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
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
        super::qos::grow_socket_buffers(&socket);
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok((
            UdpTransport {
                _qos_flow: qos_flow,
                socket,
            },
            punched,
        ))
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
            if is_transient_io(&err) {
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
            Err(e) if is_transient_io(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Batched send via `sendmmsg` (up to 64 datagrams per syscall) — the connected socket needs
    /// no per-message address. The socket is non-blocking, so a full send buffer surfaces as a
    /// short count (or `EAGAIN` with nothing sent); we stop and report what went out rather than
    /// block or retry — the data plane is lossy + FEC-protected, and blocking would queue stale
    /// frames + add latency. Ports the proven GameStream `sendmmsg_all`. Other targets fall back
    /// to the trait's scalar `send` loop (no `sendmmsg`).
    #[cfg(any(target_os = "linux", target_os = "android"))]
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
            let n = unsafe { sendmmsg(fd, hdrs.as_mut_ptr(), hdrs.len() as libc::c_uint, 0) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                // Nothing fit in the send buffer (or a stale ICMP from a connected-socket blip) —
                // drop this + the remaining chunks (counted by the caller). Only a genuine error
                // tears the session down; transient conditions are lossy drops (see is_transient_io).
                if is_transient_io(&err) {
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
                // Send buffer momentarily full, or a stale ICMP from a connected-socket blip — drop
                // the rest (counted by the caller), never block, never tear down (see is_transient_io).
                Err(e) if is_transient_io(&e) => break,
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

    /// UDP USO send (see [`Transport::send_gso`]) — Windows. Coalesces the frame's equal-size packets
    /// and hands Winsock ≤512-segment super-buffers via `WSASendMsg(UDP_SEND_MSG_SIZE)` — one syscall
    /// per chunk instead of one `send` per packet, the 1 Gbps+ lever (Windows analogue of Linux GSO).
    /// On by default (kill: `PUNKTFUNK_GSO=0`); falls back to the scalar `send_batch` when off, when
    /// packets aren't uniform-size, or on a USO-unsupported error (which latches it off for the
    /// process). Same lossy short-count contract.
    #[cfg(target_os = "windows")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        if packets.is_empty() {
            return Ok(0);
        }
        if !uso::active() {
            return self.send_batch(packets);
        }
        // USO needs every segment but the last to be exactly `seg` bytes (same as Linux GSO).
        let seg = packets[0].len();
        let last = packets.len() - 1;
        if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
            return self.send_batch(packets);
        }
        // Win11 x64 accepts up to ~512 segments per WSASendMsg.
        let max_seg = 512usize;
        let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
        let mut sent = 0usize;
        for chunk in packets.chunks(max_seg) {
            scratch.clear();
            for p in chunk {
                scratch.extend_from_slice(p);
            }
            match send_one_uso(&self.socket, &scratch, seg as u16) {
                Ok(()) => sent += chunk.len(),
                // Send buffer momentarily full / connected-socket ICMP blip — drop the rest, never
                // block, never tear down (see is_transient_io).
                Err(e) if is_transient_io(&e) => break,
                // USO unsupported on this OS/NIC/path — latch off and finish via scalar send_batch.
                Err(e) if uso_unsupported(&e) => {
                    uso::disable();
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
            Err(e) if is_transient_io(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Batched receive via `recvmmsg` — drains up to `out.len()` datagrams in one syscall into the
    /// caller's reused buffers (no per-packet allocation). `MSG_DONTWAIT` keeps it non-blocking
    /// (the socket already is); `EAGAIN` → `0`. A datagram larger than a buffer is truncated and
    /// `lens[i]` reaches the buffer size — the reassembler then rejects it as malformed, matching
    /// `recv`'s oversized-drop. Android uses the local bionic binding (see [`android_mmsg`]).
    /// Apple/BSD use the `recv`-loop override below; other non-unix the trait's scalar default.
    #[cfg(any(target_os = "linux", target_os = "android"))]
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
            recvmmsg(
                fd,
                hdrs.as_mut_ptr(),
                n_bufs as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if is_transient_io(&err) {
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
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
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
                if is_transient_io(&err) {
                    break; // socket drained, or a stale connected-socket ICMP — no data this poll
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

    /// A connected UDP socket's stale ICMP (ECONNREFUSED/RESET) and a full buffer (EAGAIN) must all
    /// be classified transient — a lossy drop, never a stream teardown. A real error must not be.
    #[test]
    fn transient_io_covers_connected_udp_blips() {
        use std::io::{Error, ErrorKind};
        for k in [
            ErrorKind::WouldBlock,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
        ] {
            assert!(
                is_transient_io(&Error::from(k)),
                "{k:?} should be transient"
            );
        }
        for k in [ErrorKind::PermissionDenied, ErrorKind::AddrInUse] {
            assert!(!is_transient_io(&Error::from(k)), "{k:?} must stay fatal");
        }
    }

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
