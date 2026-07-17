//! Linux/Android batched UDP send/recv: `sendmmsg`/`recvmmsg` + Linux UDP GSO.
//! The platform bodies of [`super::UdpTransport`]'s `send_batch`/`send_gso`/`recv_batch`
//! overrides live here (called by the cfg-gated delegators in the parent `impl Transport`).

use super::{is_transient_io, UdpTransport};

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

/// UDP GSO enable state (process-wide). **Opt-in** (`PUNKTFUNK_GSO=1`) — and deliberately so,
/// measured three times on 2026-07-14: GSO cuts send-thread CPU ~30% at 1250 Mbps, but its
/// line-rate super-buffer trains cost real delivered throughput on a constrained fabric (the
/// 2.5GbE-hop pair: peak 2452 → 1909 Mbps, and 0.4% loss at a rate sendmmsg carries clean).
/// The third A/B ran WITH pace-aware chunk scaling landed (plan Phase 1.2/1.3 in
/// `design/throughput-beyond-1gbps.md`) and reproduced the regression bit-for-bit — the trains
/// lose on the hop's queue in the transport path itself (per-AU super-buffers, no video pacer
/// involved), so the default stays opt-in on fabric evidence, not on pacing readiness. Revisit
/// with a bare-metal Linux host on a clean 10G path. NOTE the gate is value-aware:
/// `PUNKTFUNK_GSO=0` explicitly disables (it used to key on env *presence*, so `=0` ENABLED
/// it here while disabling Windows USO).
#[cfg(target_os = "linux")]
mod gso {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                // Opt-in: on only when PUNKTFUNK_GSO is set to something other than "0".
                let on = std::env::var("PUNKTFUNK_GSO").is_ok_and(|v| v != "0");
                STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
                on
            }
        }
    }
    /// Latch GSO off for the process after a GSO syscall error (unsupported kernel/path).
    /// Warns once — a mid-session downshift to sendmmsg should be visible, not silent.
    pub fn disable() {
        if STATE.swap(2, Ordering::Relaxed) != 2 {
            tracing::warn!("Linux UDP GSO unsupported on this path — falling back to sendmmsg");
        }
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

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn send_batch(t: &UdpTransport, packets: &[&[u8]]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    const CHUNK: usize = 64;
    let fd = t.socket.as_raw_fd();
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

#[cfg(target_os = "linux")]
pub(super) fn send_gso(t: &UdpTransport, packets: &[&[u8]]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    if packets.is_empty() {
        return Ok(0);
    }
    if !gso::active() {
        return send_batch(t, packets);
    }
    // GSO needs every segment but the last to be exactly `seg` bytes. Our wire packets are all
    // identical size (shards zero-padded to shard_payload), but guard and fall back if not.
    let seg = packets[0].len();
    let last = packets.len() - 1;
    if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
        return send_batch(t, packets);
    }
    let fd = t.socket.as_raw_fd();
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
                return Ok(sent + send_batch(t, &packets[sent..])?);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn recv_batch(
    t: &UdpTransport,
    out: &mut [Vec<u8>],
    lens: &mut [usize],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let fd = t.socket.as_raw_fd();
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
