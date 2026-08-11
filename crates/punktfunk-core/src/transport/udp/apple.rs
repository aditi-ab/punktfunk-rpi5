//! Apple/BSD batched UDP receive: Darwin `recvmsg_x`, `recv`-loop fallback on other BSDs.
//! The platform body of [`super::UdpTransport`]'s `recv_batch` override.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): platform syscall-batching glue — `recvmsg_x`
// fills caller-owned buffers; nothing here interprets network bytes. Proofs at each site.
#![allow(unsafe_code)]

use super::{is_transient_io, UdpTransport};

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

// A hand-written mirror of Darwin's `struct msghdr_x` (sys/socket.h), which `libc` does not expose.
// `sendmsg_x`/`recvmsg_x` read and write through it, so a wrong offset would hand the kernel the
// wrong pointer or length — the two fields it uses to decide how much memory to touch. The layout is
// not obvious by inspection because the 32-bit fields force padding before the pointers that follow
// them, which is exactly the kind of thing an edit gets wrong silently.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<MsghdrX>() == 56);
    assert!(offset_of!(MsghdrX, msg_name) == 0);
    assert!(offset_of!(MsghdrX, msg_namelen) == 8);
    assert!(offset_of!(MsghdrX, msg_iov) == 16); // 4 bytes of padding after msg_namelen
    assert!(offset_of!(MsghdrX, msg_iovlen) == 24);
    assert!(offset_of!(MsghdrX, msg_control) == 32); // padding after msg_iovlen
    assert!(offset_of!(MsghdrX, msg_controllen) == 40);
    assert!(offset_of!(MsghdrX, msg_flags) == 44);
    assert!(offset_of!(MsghdrX, msg_datalen) == 48);
};

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

/// Apple batched receive via `recvmsg_x` — drains up to `out.len()` datagrams in one syscall into
/// the caller's reused buffers (the recv counterpart of Linux `recvmmsg`, which Darwin lacks).
/// SAFETY: each `MsghdrX` holds a raw pointer into `iovs`, which holds raw pointers into `out`'s
/// buffers; both `iovs` and `msgs` stay alive and unmoved through the syscall.
#[cfg(target_vendor = "apple")]
fn recv_batch_x(
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
            // SAFETY: MsghdrX is a plain-old-data libc-style struct; all-zeroes is its
            // documented "no ancillary data, no name" initial state.
            let mut m: MsghdrX = unsafe { std::mem::zeroed() };
            m.msg_iov = iov as *mut libc::iovec;
            m.msg_iovlen = 1;
            m
        })
        .collect();
    // SAFETY: `fd` is a live socket owned by `t`; `msgs` holds `n_bufs` initialized headers
    // whose iovecs point into `out`'s live buffers — both outlive the call.
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

pub(super) fn recv_batch(
    t: &UdpTransport,
    out: &mut [Vec<u8>],
    lens: &mut [usize],
) -> std::io::Result<usize> {
    // Apple: prefer the batched `recvmsg_x` syscall when enabled; a surprise error disables it
    // and falls through to the always-correct scalar loop below.
    #[cfg(target_vendor = "apple")]
    if recvx::active() {
        match recv_batch_x(t, out, lens) {
            Ok(n) => return Ok(n),
            Err(_) => recvx::disable(),
        }
    }
    use std::os::fd::AsRawFd;
    let fd = t.socket.as_raw_fd();
    let n_bufs = out.len().min(lens.len());
    let mut got = 0usize;
    while got < n_bufs {
        let buf = &mut out[got];
        // SAFETY: `fd` is a live socket owned by `t`; `buf` is a live mutable buffer whose
        // pointer/len pair is valid for writes for the duration of the call.
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
