//! Wire protocol between the PipeWire capture thread and the isolated zero-copy GPU-import
//! worker process (`punktfunk-host zerocopy-worker`; design:
//! [`design/zerocopy-worker-isolation.md`]). Transport is a `SOCK_SEQPACKET` unix socketpair —
//! reliable, ordered, message-framed (one `sendmsg` = one message) — with dmabuf fds riding as
//! `SCM_RIGHTS` control data. Bodies are small serde_json blobs (~200 B/frame); pixels never
//! cross the socket (they move GPU-side via CUDA IPC, see [`super::cuda::ipc_export`]).
//!
//! Zero-length messages are reserved: `recvmsg` returning 0 on a SEQPACKET socket is EOF (the
//! peer died/closed), and every serialized message here is non-empty JSON, so the two can't be
//! confused.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// Bumped on any wire change; the worker echoes it in [`Reply::Ready`] and the host refuses a
/// mismatch. Host and worker are the same binary (`/proc/self/exe`), so this only ever trips on
/// exotic deployment mistakes (a stale binary re-exec'd across an upgrade).
pub const PROTO_VERSION: u32 = 1;

/// Upper bound for one serialized message (the largest real message — a modifier list — is far
/// below this). A message reported truncated at this size is a protocol error.
pub const MAX_MSG: usize = 64 * 1024;

/// How a dmabuf should be imported — mirrors the `EglImporter` entry points.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// Tiled dmabuf → EGL/GL de-tile blit → BGRx CUDA buffer.
    Tiled,
    /// Tiled dmabuf → EGL/GL NV12 convert → two-plane CUDA buffer (`PUNKTFUNK_NV12`).
    TiledNv12,
    /// LINEAR dmabuf → Vulkan bridge → BGRx CUDA buffer (gamescope's only offer).
    Linear,
    /// Tiled dmabuf → EGL/GL planar-YUV444 convert → ONE stacked 3-plane CUDA buffer (a 4:4:4
    /// session). APPENDED last: the worker can outlive a replaced host binary, so the earlier
    /// variants' wire tags must never shift — an old worker receiving this fails the decode and
    /// the import-fail machinery handles it like any other worker error.
    Tiled444,
    /// LINEAR dmabuf → Vulkan-bridge compute CSC → two-plane NV12 CUDA buffer (latency plan
    /// T2.5b — the gamescope analogue of [`TiledNv12`](Self::TiledNv12)). Appended last, same
    /// wire-tag rule as [`Tiled444`](Self::Tiled444).
    LinearNv12,
}

/// host → worker.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Request {
    /// The EGL-importable DRM modifiers for `fourcc` (startup, before the stream connects —
    /// the host advertises these to PipeWire).
    Modifiers { fourcc: u32 },
    /// Import one frame. `key` identifies the underlying dmabuf across frames (the host uses
    /// the fd's `st_ino` — unique per dma-buf object); the fd itself rides along as
    /// `SCM_RIGHTS` only on first sight of `key` (`has_fd`), and the worker keeps its dup.
    Import {
        key: u64,
        kind: ImportKind,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        offset: u32,
        stride: u32,
        has_fd: bool,
    },
    /// The frame buffer previously delivered as `id` is no longer in use — recycle it into the
    /// worker's pool. Fire-and-forget (no reply); may be sent from any host thread.
    Release { id: u32 },
    /// The PipeWire stream renegotiated its format: the buffer pool is gone, so drop all cached
    /// per-`key` state (stored fds, Vulkan per-fd imports). Fire-and-forget.
    ClearCache,
}

/// worker → host.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Reply {
    /// Sent once at startup after EGL + CUDA came up.
    Ready {
        version: u32,
    },
    /// Startup failed (no NVIDIA driver, EGL error, …) — the host falls back to the CPU path,
    /// exactly like an in-process `EglImporter::new()` failure.
    InitErr {
        message: String,
    },
    Modifiers {
        modifiers: Vec<u64>,
    },
    /// The imported frame is complete (the GPU copy already synced worker-side) in buffer `id`.
    /// `desc` rides along the first time `id` is ever delivered — the host opens its CUDA IPC
    /// handles once and caches the mapping for every later frame in the same buffer.
    Frame {
        id: u32,
        desc: Option<BufferDesc>,
    },
    /// The worker has no cached fd for the import's `key` (evicted, or the two sides' caches
    /// diverged) — the host forgets its "already sent" note and retries once WITH the fd.
    NeedFd,
    /// This import failed but the worker is alive (e.g. `EGL_BAD_MATCH` on one buffer).
    Err {
        message: String,
    },
}

/// CUDA IPC identity of one pooled device buffer (sent once per buffer, then referenced by id).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BufferDesc {
    pub width: u32,
    pub height: u32,
    /// `cuIpcGetMemHandle` blob for the (Y or BGRx) plane — exactly 64 bytes.
    pub y_handle: Vec<u8>,
    pub y_pitch: usize,
    /// NV12 only: the interleaved chroma plane's `(handle, pitch)`.
    pub uv: Option<(Vec<u8>, usize)>,
}

/// A CLOEXEC `SOCK_SEQPACKET` socketpair — `(host_end, worker_end)`.
pub fn socketpair_seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `socketpair` writes two fds into `fds`, a live 2-element stack array matching the
    // API contract; it reads no other Rust memory. The result is checked before the fds are used,
    // and each returned fd is fresh (owned by no other wrapper), so the two `OwnedFd::from_raw_fd`
    // each take sole ownership of a distinct, valid descriptor — no alias, no double-close.
    unsafe {
        if libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

/// Set (or clear) the receive timeout: a blocked [`recv`] then fails with
/// `ErrorKind::WouldBlock`. Used by the host so a hung worker can't wedge the capture thread.
pub fn set_recv_timeout(sock: BorrowedFd, timeout: Option<Duration>) -> io::Result<()> {
    let tv = match timeout {
        Some(d) => libc::timeval {
            tv_sec: d.as_secs() as libc::time_t,
            tv_usec: d.subsec_micros() as libc::suseconds_t,
        },
        None => libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    // SAFETY: `setsockopt(SO_RCVTIMEO)` reads `size_of::<timeval>()` bytes from `&tv`, a live
    // stack `timeval` that outlives this synchronous call; `sock` is the caller's live socket fd.
    // Nothing is retained or written through Rust pointers.
    let r = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Send one message (+ optionally one fd as `SCM_RIGHTS`) as a single SEQPACKET datagram.
/// Atomic per message, so concurrent senders on the same socket (the capture thread's imports,
/// the encode thread's releases) need no lock. `MSG_NOSIGNAL` turns a dead peer into `EPIPE`
/// instead of `SIGPIPE`.
pub fn send<T: Serialize>(
    sock: BorrowedFd,
    msg: &T,
    pass_fd: Option<BorrowedFd>,
) -> io::Result<()> {
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    debug_assert!(
        !body.is_empty(),
        "zero-length messages are reserved for EOF"
    );
    if body.len() > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zerocopy proto message too large",
        ));
    }
    let mut iov = libc::iovec {
        iov_base: body.as_ptr() as *mut libc::c_void,
        iov_len: body.len(),
    };
    // Control buffer for one fd: CMSG_SPACE(4) = 24 bytes on 64-bit; [u64; 4] gives 32 bytes at
    // the 8-byte alignment `cmsghdr` requires.
    let mut cmsg_store = [0u64; 4];
    // SAFETY: `mhdr` is a plain-old-data C struct for which all-zero is a valid value.
    let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
    mhdr.msg_iov = &mut iov;
    mhdr.msg_iovlen = 1;
    if let Some(fd) = pass_fd {
        mhdr.msg_control = cmsg_store.as_mut_ptr() as *mut libc::c_void;
        // SAFETY: `CMSG_SPACE`/`CMSG_LEN` are pure size computations (no memory access).
        // `CMSG_FIRSTHDR(&mhdr)` returns a pointer into `cmsg_store` (non-null: msg_controllen
        // ≥ one cmsghdr), which is live, 8-aligned, and large enough (32 ≥ CMSG_SPACE(4) = 24)
        // for the header fields and the 4-byte fd written via `CMSG_DATA`; `write_unaligned`
        // handles the data area's byte alignment. All writes stay within `cmsg_store`, which
        // outlives the synchronous `sendmsg` below.
        unsafe {
            mhdr.msg_controllen = libc::CMSG_SPACE(4) as _;
            let c = libc::CMSG_FIRSTHDR(&mhdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut i32, fd.as_raw_fd());
        }
    }
    // SAFETY: `sock` is the caller's live socket; `mhdr` points at the live `iov` (over `body`,
    // which outlives the call) and — when an fd is passed — at `cmsg_store` (ditto). `sendmsg`
    // only reads these buffers. The kernel dups the fd into the message; our `BorrowedFd` stays
    // owned by the caller.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &mhdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != body.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short sendmsg on SEQPACKET socket",
        ));
    }
    Ok(())
}

/// Receive one message (+ up to one `SCM_RIGHTS` fd). `buf` is a caller-owned scratch buffer
/// (grown to [`MAX_MSG`] once, then reused frame to frame). Errors:
/// `UnexpectedEof` = the peer is gone; `WouldBlock` = the [`set_recv_timeout`] expired.
pub fn recv<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
) -> io::Result<(T, Option<OwnedFd>)> {
    buf.resize(MAX_MSG, 0);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut cmsg_store = [0u64; 4];
    // SAFETY: `mhdr` is a plain-old-data C struct for which all-zero is a valid value.
    let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
    mhdr.msg_iov = &mut iov;
    mhdr.msg_iovlen = 1;
    mhdr.msg_control = cmsg_store.as_mut_ptr() as *mut libc::c_void;
    mhdr.msg_controllen = std::mem::size_of_val(&cmsg_store) as _;
    // SAFETY: `sock` is the caller's live socket. `recvmsg` writes at most `iov_len` bytes into
    // `buf` (live for the call) and at most `msg_controllen` control bytes into `cmsg_store`
    // (live, 8-aligned). `MSG_CMSG_CLOEXEC` makes any received fd CLOEXEC atomically.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut mhdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "zerocopy proto peer closed",
        ));
    }
    // Collect a passed fd (if any) BEFORE any early return below, so it can't leak.
    let mut got_fd: Option<OwnedFd> = None;
    // SAFETY: `CMSG_FIRSTHDR`/`CMSG_NXTHDR` walk the control area the kernel just wrote inside
    // `cmsg_store` (bounded by the updated `mhdr.msg_controllen`), returning either null or a
    // pointer to a complete `cmsghdr` within it — each dereference reads kernel-initialized
    // fields in bounds. For an `SCM_RIGHTS` cmsg the data area holds whole `i32` fds; we read the
    // first via `read_unaligned`. The kernel gave us ownership of that fd (it is a fresh
    // descriptor in our table), so `OwnedFd::from_raw_fd` takes sole ownership — any previously
    // collected `got_fd` is dropped (closed) first, so nothing leaks even with multiple cmsgs.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&mhdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const i32);
                if fd >= 0 {
                    got_fd = Some(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&mhdr, c);
        }
    }
    if mhdr.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zerocopy proto message truncated",
        ));
    }
    let msg = serde_json::from_slice(&buf[..n as usize])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((msg, got_fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    #[test]
    fn round_trip_no_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let req = Request::Import {
            key: 0xdead_beef_u64,
            kind: ImportKind::TiledNv12,
            width: 5120,
            height: 1440,
            fourcc: 0x3432_5258,
            modifier: Some(0x0300_0000_0000_1234),
            offset: 0,
            stride: 5120 * 4,
            has_fd: false,
        };
        send(a.as_fd(), &req, None).unwrap();
        let (got, fd) = recv::<Request>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, req);
        assert!(fd.is_none());

        let reply = Reply::Frame {
            id: 7,
            desc: Some(BufferDesc {
                width: 5120,
                height: 1440,
                y_handle: vec![1u8; 64],
                y_pitch: 5632,
                uv: Some((vec![2u8; 64], 5632)),
            }),
        };
        send(b.as_fd(), &reply, None).unwrap();
        let (got, fd) = recv::<Reply>(a.as_fd(), &mut buf).unwrap();
        assert_eq!(got, reply);
        assert!(fd.is_none());
    }

    #[test]
    fn passes_an_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        // A pipe stands in for a dmabuf: pass the read end, write through the original write end,
        // and read the bytes back through the RECEIVED fd.
        let (mut pr, mut pw) = std::io::pipe().unwrap();
        send(a.as_fd(), &Request::ClearCache, Some(pr.as_fd())).unwrap();
        let (got, fd) = recv::<Request>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, Request::ClearCache);
        let fd = fd.expect("fd should have been passed");
        pw.write_all(b"hello").unwrap();
        drop(pw);
        let mut file = std::fs::File::from(fd);
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        // The original read end still works independently of the passed dup.
        let mut nothing = [0u8; 1];
        assert_eq!(pr.read(&mut nothing).unwrap(), 0);
    }

    #[test]
    fn eof_when_peer_closes() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(a);
        let mut buf = Vec::new();
        let err = recv::<Reply>(b.as_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn send_to_dead_peer_is_epipe_not_sigpipe() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(b);
        let err = send(a.as_fd(), &Request::ClearCache, None).unwrap_err();
        // MSG_NOSIGNAL: a dead peer surfaces as EPIPE (BrokenPipe), never a process-killing signal.
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn recv_timeout_fires() {
        let (a, _b) = socketpair_seqpacket().unwrap();
        set_recv_timeout(a.as_fd(), Some(Duration::from_millis(50))).unwrap();
        let mut buf = Vec::new();
        let err = recv::<Reply>(a.as_fd(), &mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {err:?}"
        );
    }
}
