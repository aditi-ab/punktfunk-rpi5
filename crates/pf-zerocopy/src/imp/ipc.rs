//! Worker-IPC rails, shared by every punktfunk worker subprocess (design:
//! `design/zerocopy-worker-isolation.md`, generalized in
//! `design/gpu-priority-capability-worker-implementation-plan.md` §2/WP0). Two halves, both
//! deliberately free of any worker's *vocabulary* — the message enums live with their worker and
//! stay independently versioned:
//!
//! - **Framing.** A `SOCK_SEQPACKET` unix socketpair — reliable, ordered, message-framed (one
//!   `sendmsg` = one message) — carrying serde bodies, with descriptors riding as `SCM_RIGHTS`
//!   control data. Zero-length messages are reserved: `recvmsg` returning 0 on a SEQPACKET socket
//!   is EOF (the peer died/closed), and a serialized message is never empty, so the two can't be
//!   confused.
//! - **Process rails.** Spawn a worker on a *pinned* executable with its socket end on fd 3, kill
//!   the host's children with it (`PR_SET_PDEATHSIG`), and reap them without ever blocking a
//!   caller behind a process wedged in a driver ioctl.
//!
//! The zerocopy worker execs this process's own image ([`self_exe`]); the encode worker
//! (`design/gpu-priority-capability-worker.md`) is a **separate file** — it must never share an
//! inode with `punktfunk-host`, because a shared inode shares the file capability — so it passes
//! its own resolved path to [`spawn_worker`] instead.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Upper bound for one serialized message (the largest real message — a modifier list — is far
/// below this). A message reported truncated at this size is a protocol error.
pub const MAX_MSG: usize = 64 * 1024;

/// Descriptors one message may carry. Four, because a multi-planar dmabuf can hold one fd per
/// plane; the single-fd case ([`send`]/[`recv`]) is the hot path and stays allocation-free.
pub const MAX_FDS: usize = 4;

/// Backing store for the `SCM_RIGHTS` control data, `u64` so it carries the 8-byte alignment
/// `cmsghdr` requires. `MAX_FDS` fds need `CMSG_SPACE(4 * 4) = 32` bytes on 64-bit Linux (a
/// 16-byte `cmsghdr` + 16 bytes of fds); 64 is double that, so the slack absorbs any platform
/// whose header is larger — the `cmsg_store_is_large_enough` test asserts it for real.
type CmsgStore = [u64; 8];

/// Control bytes the kernel may use for `n` descriptors. (`CMSG_SPACE` is pure size arithmetic —
/// the `unsafe` is libc's signature, not a contract.)
fn cmsg_space(n: usize) -> usize {
    // SAFETY: `CMSG_SPACE` performs alignment arithmetic on its argument and touches no memory.
    unsafe { libc::CMSG_SPACE((n * std::mem::size_of::<RawFd>()) as u32) as usize }
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
/// `ErrorKind::WouldBlock`. Used by the host so a hung worker can't wedge the calling thread.
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

/// Send one message (+ optionally one fd as `SCM_RIGHTS`) as a single SEQPACKET datagram — the
/// single-descriptor fast path over [`send_fds`] (a borrowed one-element slice; no fd list is
/// built).
pub fn send<T: Serialize>(
    sock: BorrowedFd,
    msg: &T,
    pass_fd: Option<BorrowedFd>,
) -> io::Result<()> {
    match pass_fd {
        Some(fd) => send_fds(sock, msg, &[fd]),
        None => send_fds(sock, msg, &[]),
    }
}

/// Send one message plus up to [`MAX_FDS`] descriptors as a single SEQPACKET datagram. Atomic per
/// message, so concurrent senders on the same socket (e.g. the capture thread's imports and the
/// encode thread's releases) need no lock. `MSG_NOSIGNAL` turns a dead peer into `EPIPE` instead
/// of `SIGPIPE`. More than [`MAX_FDS`] is a caller bug, refused like an over-long body rather than
/// panicking.
pub fn send_fds<T: Serialize>(sock: BorrowedFd, msg: &T, fds: &[BorrowedFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "worker ipc: {} fds in one message (max {MAX_FDS})",
                fds.len()
            ),
        ));
    }
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    debug_assert!(
        !body.is_empty(),
        "zero-length messages are reserved for EOF"
    );
    if body.len() > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker ipc message too large",
        ));
    }
    let mut iov = libc::iovec {
        iov_base: body.as_ptr() as *mut libc::c_void,
        iov_len: body.len(),
    };
    let mut cmsg_store: CmsgStore = [0; 8];
    // SAFETY: `mhdr` is a plain-old-data C struct for which all-zero is a valid value.
    let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
    mhdr.msg_iov = &mut iov;
    mhdr.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = (fds.len() * std::mem::size_of::<RawFd>()) as u32;
        debug_assert!(cmsg_space(fds.len()) <= std::mem::size_of_val(&cmsg_store));
        mhdr.msg_control = cmsg_store.as_mut_ptr() as *mut libc::c_void;
        // SAFETY: `CMSG_SPACE`/`CMSG_LEN` are pure size computations (no memory access).
        // `CMSG_FIRSTHDR(&mhdr)` returns a pointer into `cmsg_store` (non-null: msg_controllen
        // ≥ one cmsghdr), which is live, 8-aligned, and large enough — the store holds
        // `CMSG_SPACE(MAX_FDS * 4)` and `fds.len() <= MAX_FDS` was checked above — for the header
        // fields plus one 4-byte fd per element written through `CMSG_DATA`; `write_unaligned`
        // handles the data area's byte alignment. All writes stay within `cmsg_store`, which
        // outlives the synchronous `sendmsg` below.
        unsafe {
            mhdr.msg_controllen = libc::CMSG_SPACE(bytes) as _;
            let c = libc::CMSG_FIRSTHDR(&mhdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
            let data = libc::CMSG_DATA(c) as *mut RawFd;
            for (i, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(data.add(i), fd.as_raw_fd());
            }
        }
    }
    // SAFETY: `sock` is the caller's live socket; `mhdr` points at the live `iov` (over `body`,
    // which outlives the call) and — when fds are passed — at `cmsg_store` (ditto). `sendmsg`
    // only reads these buffers. The kernel dups the fds into the message; our `BorrowedFd`s stay
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

/// Receive one message (+ up to one `SCM_RIGHTS` fd) — the single-descriptor fast path over
/// [`recv_fds`]. Any further descriptors the peer attached are dropped, i.e. closed, so a
/// protocol mix-up cannot leak fds into a caller that only knows about one.
pub fn recv<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
) -> io::Result<(T, Option<OwnedFd>)> {
    let (msg, fds) = recv_fds(sock, buf)?;
    Ok((msg, fds.into_iter().next()))
}

/// Receive one message plus its `SCM_RIGHTS` descriptors (up to [`MAX_FDS`], in the order the
/// sender listed them). `buf` is a caller-owned scratch buffer (grown to [`MAX_MSG`] once, then
/// reused message to message); the returned `Vec` does not allocate when no fd arrived, which is
/// the steady state under an fd-identity cache. Errors: `UnexpectedEof` = the peer is gone;
/// `WouldBlock` = the [`set_recv_timeout`] expired; `InvalidData` = a truncated body or more
/// descriptors than [`MAX_FDS`] (the kernel drops the excess and flags `MSG_CTRUNC`).
pub fn recv_fds<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
) -> io::Result<(T, Vec<OwnedFd>)> {
    buf.resize(MAX_MSG, 0);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut cmsg_store: CmsgStore = [0; 8];
    // SAFETY: `mhdr` is a plain-old-data C struct for which all-zero is a valid value.
    let mut mhdr: libc::msghdr = unsafe { std::mem::zeroed() };
    mhdr.msg_iov = &mut iov;
    mhdr.msg_iovlen = 1;
    mhdr.msg_control = cmsg_store.as_mut_ptr() as *mut libc::c_void;
    // Exactly MAX_FDS worth of control space (not the store's full size): a peer that attaches
    // more gets its excess dropped by the kernel and the message flagged `MSG_CTRUNC`, which is
    // checked below — the cap is enforced by the kernel rather than trusted from the peer.
    debug_assert!(cmsg_space(MAX_FDS) <= std::mem::size_of_val(&cmsg_store));
    mhdr.msg_controllen = cmsg_space(MAX_FDS) as _;
    // SAFETY: `sock` is the caller's live socket. `recvmsg` writes at most `iov_len` bytes into
    // `buf` (live for the call) and at most `msg_controllen` control bytes into `cmsg_store`
    // (live, 8-aligned, and at least that large — asserted above). `MSG_CMSG_CLOEXEC` makes any
    // received fd CLOEXEC atomically.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut mhdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "worker ipc peer closed",
        ));
    }
    // Collect the passed fds (if any) BEFORE any early return below, so they can't leak — an
    // error path drops the `Vec`, which closes every one of them.
    let mut got: Vec<OwnedFd> = Vec::new();
    // SAFETY: `CMSG_FIRSTHDR`/`CMSG_NXTHDR` walk the control area the kernel just wrote inside
    // `cmsg_store` (bounded by the updated `mhdr.msg_controllen`), returning either null or a
    // pointer to a complete `cmsghdr` within it — each dereference reads kernel-initialized
    // fields in bounds. For an `SCM_RIGHTS` cmsg the data area holds whole `RawFd`s, `cmsg_len -
    // CMSG_LEN(0)` bytes of them, all inside that same complete cmsg, so every `read_unaligned`
    // at index < that count is in bounds. The kernel gave us ownership of each fd (they are fresh
    // descriptors in our table), so each `OwnedFd::from_raw_fd` takes sole ownership of a distinct
    // descriptor — no alias, no double-close, and nothing dropped on the floor even with multiple
    // cmsgs or multiple fds per cmsg.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&mhdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let payload = ((*c).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let data = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..payload / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    if fd >= 0 {
                        got.push(OwnedFd::from_raw_fd(fd));
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&mhdr, c);
        }
    }
    if mhdr.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("worker ipc message carried more than {MAX_FDS} descriptors"),
        ));
    }
    if mhdr.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker ipc message truncated",
        ));
    }
    let msg = serde_json::from_slice(&buf[..n as usize])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((msg, got))
}

/// An executable pinned by an open fd, exec'able through [`PinnedExe::exec_path`]. The fd names
/// the file's *inode*, not its path, so it resolves even after the binary was replaced or deleted
/// — and exec'ing it then still runs byte-for-byte the build that was pinned. `current_exe()`
/// instead readlinks to a path: after a package upgrade under a running host that path is
/// `"<path> (deleted)"` and spawning it fails ENOENT — every capture then silently fell back to the
/// CPU copy (the 2026-07-10 canary regression) — and even while the path exists it may hold a
/// newer build whose worker protocol mismatches this process.
pub struct PinnedExe(File);

impl PinnedExe {
    /// Pin the executable at `path`.
    pub fn open(path: &Path) -> io::Result<PinnedExe> {
        let f = File::open(path)?;
        if f.as_raw_fd() != 3 {
            return Ok(PinnedExe(f));
        }
        // Fd 3 is the slot the spawn hands the worker its socket on (the `dup2` in
        // [`spawn_worker`]) — pinned there, the child would clobber it before exec resolves
        // `/proc/self/fd/3`. Re-number: 3 stays occupied by `f` during the clone, so the
        // duplicate cannot land on it.
        let clone = f.try_clone().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("re-numbering the pinned exe fd off fd 3 failed: {e}"),
            )
        })?;
        Ok(PinnedExe(clone))
    }

    /// `/proc/self/fd/<n>` — an exec'able path to the pinned inode. The kernel resolves it at exec
    /// time inside the forked child, whose fd table is a copy of ours (close-on-exec applies only
    /// once the exec succeeds), so it names the pinned inode no matter what sits at the file's
    /// original path by then.
    pub fn exec_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
}

/// This process's own executable image, pinned once (lazily) via the `/proc/self/exe` magic link
/// — see [`PinnedExe`] for why the fd and not the path. `None` when it could not be opened, in
/// which case callers fall back to `current_exe()` and inherit that trap.
///
/// Only for a worker that is the same file as its host by construction (the zerocopy worker
/// re-execs this binary). The encode worker must stay a **separate file** — it carries a file
/// capability the host must never have — so it pins its own resolved path with [`PinnedExe::open`].
pub fn self_exe() -> Option<&'static PinnedExe> {
    static SELF_EXE: OnceLock<Option<PinnedExe>> = OnceLock::new();
    SELF_EXE
        .get_or_init(|| match PinnedExe::open(Path::new("/proc/self/exe")) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "cannot pin /proc/self/exe — worker spawns use the current_exe() path, \
                     which breaks if this binary is replaced on disk"
                );
                None
            }
        })
        .as_ref()
}

/// Spawn a worker process on `exe` (normally a [`PinnedExe::exec_path`]) and hand it one end of a
/// fresh SEQPACKET socketpair on **fd 3**; the host end is returned alongside the child.
///
/// `argv0` is what `ps` shows — worth setting, because `exe` is normally an opaque
/// `/proc/self/fd/<n>`. `args` follow it.
pub fn spawn_worker(exe: &Path, argv0: &str, args: &[&str]) -> io::Result<(OwnedFd, Child)> {
    sweep_reaper();
    let (host_end, worker_end) = socketpair_seqpacket()?;
    let mut cmd = Command::new(exe);
    cmd.arg0(argv0);
    cmd.args(args);
    let raw = worker_end.as_raw_fd();
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: `pre_exec` runs between fork and exec, so only async-signal-safe calls are
    // allowed — `prctl`, `getppid`, `dup2` and `fcntl` all are, and the closure captures only
    // `Copy` ints (no allocation, no locks; the error paths use `from_raw_os_error`, which
    // does not allocate). PR_SET_PDEATHSIG makes the kernel SIGKILL the worker when the host
    // dies — without it a crashed host left the worker holding its driver state (order hundreds
    // of MB of VRAM) indefinitely. The `getppid` check closes the standard race: if the host died
    // between fork and the prctl, the signal is never delivered, so refuse to exec instead.
    // `dup2(raw, 3)` installs the socket at the fd number the worker expects and clears CLOEXEC
    // on the copy; if the parent's fd already IS 3, `dup2(3,3)` would preserve CLOEXEC, so that
    // case clears the flag explicitly instead.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if raw == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(raw, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(worker_end); // the child holds its own copy now
    Ok((host_end, child))
}

/// Children whose worker hasn't exited yet at teardown time (a worker exits on socket EOF, i.e.
/// after the last in-flight frame drops). Swept on every spawn and every drop so workers don't
/// linger as zombies for more than one generation.
static REAPER: Mutex<Vec<(Child, Instant)>> = Mutex::new(Vec::new());

/// How long past the caller's reply timeout a parked worker may linger before it is force-killed.
/// A worker wedged INSIDE a driver call never observes socket EOF, so `try_wait` alone would keep
/// it (and its driver state — order hundreds of MB of VRAM) forever.
const REAPER_KILL_DEADLINE: Duration = Duration::from_secs(20);

/// Hand a still-running worker to the reaper. Call from a `Drop` after the socket is (about to
/// be) closed: the worker sees EOF and exits, and the next [`sweep_reaper`] collects it.
pub fn park_child(child: Child) {
    REAPER.lock().unwrap().push((child, Instant::now()));
}

/// Reap exited workers; force-kill the ones parked past the kill deadline (20 s).
pub fn sweep_reaper() {
    // Partition under the lock; kill/reap OUTSIDE it. A worker wedged inside a driver ioctl sits
    // in D state and ignores SIGKILL — the old blocking `wait()` under the global mutex would
    // then park every later spawn and drop behind a process that may never die.
    let mut expired: Vec<Child> = Vec::new();
    {
        let mut list = REAPER.lock().unwrap();
        let now = Instant::now();
        let mut i = 0;
        while i < list.len() {
            if matches!(list[i].0.try_wait(), Ok(Some(_))) {
                list.swap_remove(i); // exited on its own → reaped
            } else if now.duration_since(list[i].1) > REAPER_KILL_DEADLINE {
                expired.push(list.swap_remove(i).0);
            } else {
                i += 1;
            }
        }
    }
    for mut c in expired {
        let _ = c.kill();
        // Bounded reap (~100 ms of polls): a SIGKILL'd process reaps near-instantly unless it is
        // in D state — then park it again (re-killing later is harmless) so a future sweep reaps
        // it once the driver unwedges, instead of blocking anyone here forever.
        let mut reaped = false;
        for _ in 0..10 {
            if matches!(c.try_wait(), Ok(Some(_))) {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !reaped {
            tracing::warn!(
                pid = c.id(),
                "worker ignored SIGKILL (likely wedged in a driver call, D state) — \
                 parked for a later sweep"
            );
            park_child(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    /// A stand-in payload: the transport is generic over the serde body, and the workers' own
    /// message enums live with the workers — nothing here may depend on one.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Msg {
        tag: String,
        n: u32,
    }

    fn msg(tag: &str) -> Msg {
        Msg {
            tag: tag.into(),
            n: 7,
        }
    }

    /// A pipe whose read end is passed over the socket, carrying `payload` for the receiver to
    /// read back — the only way to prove the *descriptor* crossed, not just the claim that it did.
    fn marked_pipe(payload: &[u8]) -> (std::io::PipeReader, std::io::PipeWriter) {
        let (pr, mut pw) = std::io::pipe().unwrap();
        pw.write_all(payload).unwrap();
        (pr, pw)
    }

    #[test]
    fn cmsg_store_is_large_enough() {
        assert!(
            cmsg_space(MAX_FDS) <= std::mem::size_of::<CmsgStore>(),
            "CMSG_SPACE({MAX_FDS} fds) = {} > store {}",
            cmsg_space(MAX_FDS),
            std::mem::size_of::<CmsgStore>()
        );
    }

    #[test]
    fn round_trip_no_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        send(a.as_fd(), &msg("hello"), None).unwrap();
        let (got, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("hello"));
        assert!(fd.is_none());
    }

    #[test]
    fn passes_an_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        // A pipe stands in for a dmabuf: pass the read end, write through the original write end,
        // and read the bytes back through the RECEIVED fd.
        let (mut pr, mut pw) = std::io::pipe().unwrap();
        send(a.as_fd(), &msg("one"), Some(pr.as_fd())).unwrap();
        let (got, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("one"));
        let fd = fd.expect("fd should have been passed");
        pw.write_all(b"hello").unwrap();
        drop(pw);
        let mut file = File::from(fd);
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        // The original read end still works independently of the passed dup.
        let mut nothing = [0u8; 1];
        assert_eq!(pr.read(&mut nothing).unwrap(), 0);
    }

    #[test]
    fn round_trip_three_fds() {
        // The multi-planar case (WP0): one message carrying one fd per plane. Each pipe is
        // pre-loaded with a distinct byte, so reading through the RECEIVED descriptors proves
        // both that all three crossed and that they arrived in the sender's order.
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let planes = [marked_pipe(b"Y"), marked_pipe(b"U"), marked_pipe(b"V")];
        {
            let fds: Vec<BorrowedFd> = planes.iter().map(|(pr, _)| pr.as_fd()).collect();
            send_fds(a.as_fd(), &msg("planes"), &fds).unwrap();
        }
        let (got, fds) = recv_fds::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("planes"));
        assert_eq!(fds.len(), 3, "all three descriptors must cross");
        // Drop the write ends so each read sees EOF after its byte.
        drop(planes);
        let read_back: Vec<String> = fds
            .into_iter()
            .map(|fd| {
                let mut s = String::new();
                File::from(fd).read_to_string(&mut s).unwrap();
                s
            })
            .collect();
        assert_eq!(read_back, vec!["Y", "U", "V"]);
    }

    #[test]
    fn max_fds_round_trip_and_one_more_is_refused() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let pipes: Vec<_> = (0..MAX_FDS + 1).map(|_| std::io::pipe().unwrap()).collect();
        let fds: Vec<BorrowedFd> = pipes.iter().map(|(pr, _)| pr.as_fd()).collect();
        send_fds(a.as_fd(), &msg("full"), &fds[..MAX_FDS]).unwrap();
        let (_, got) = recv_fds::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got.len(), MAX_FDS);
        // One over the cap is refused at the sender (a caller bug), not panicked on.
        let err = send_fds(a.as_fd(), &msg("over"), &fds).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn recv_keeps_the_first_fd_and_closes_the_rest() {
        // A peer that attaches more descriptors than the single-fd caller expects must not leak
        // them: `recv` returns the first and drops (closes) the others.
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let (first, second) = (marked_pipe(b"1"), marked_pipe(b"2"));
        send_fds(a.as_fd(), &msg("two"), &[first.0.as_fd(), second.0.as_fd()]).unwrap();
        let (_, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        let fd = fd.expect("the first fd is returned");
        drop(first);
        let mut s = String::new();
        File::from(fd).read_to_string(&mut s).unwrap();
        assert_eq!(s, "1");
        // The second pipe's receiver-side dup was closed with the returned Vec's tail, so the
        // writer sees EPIPE once the local read end goes too.
        drop(second.0);
        let mut pw = second.1;
        assert_eq!(
            pw.write_all(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn eof_when_peer_closes() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(a);
        let mut buf = Vec::new();
        let err = recv::<Msg>(b.as_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn send_to_dead_peer_is_epipe_not_sigpipe() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(b);
        let err = send(a.as_fd(), &msg("gone"), None).unwrap_err();
        // MSG_NOSIGNAL: a dead peer surfaces as EPIPE (BrokenPipe), never a process-killing signal.
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn recv_timeout_fires() {
        let (a, _b) = socketpair_seqpacket().unwrap();
        set_recv_timeout(a.as_fd(), Some(Duration::from_millis(50))).unwrap();
        let mut buf = Vec::new();
        let err = recv::<Msg>(a.as_fd(), &mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {err:?}"
        );
    }

    #[test]
    fn pinned_fd_exec_survives_on_disk_replacement() {
        // The 2026-07-10 canary regression: a package upgrade replaced the installed binary and
        // every worker spawn ENOENT'd (`current_exe()` readlinked to "<path> (deleted)"). The
        // pinned-fd mechanism must keep exec'ing the original image after the file is gone: pin
        // a copy of /bin/sh, delete it, then run it through the fd path.
        let copy = std::env::temp_dir().join(format!("pf-zerocopy-exe-pin-{}", std::process::id()));
        std::fs::copy("/bin/sh", &copy).unwrap();
        let pinned = PinnedExe::open(&copy).unwrap();
        std::fs::remove_file(&copy).unwrap();
        // Retry ETXTBSY: `fs::copy`'s write fd leaks into other tests' concurrently-forked
        // children until their execs clear it (CLOEXEC applies only at exec), and exec'ing a
        // file someone holds open for writing is refused. A harness artifact of copy-then-exec,
        // not the mechanism under test — production pins a read-only fd on a binary nobody
        // write-opens.
        let status = loop {
            match Command::new(pinned.exec_path())
                .arg("-c")
                .arg("exit 42")
                .status()
            {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => break other.expect("exec via /proc/self/fd of a deleted file"),
            }
        };
        assert_eq!(status.code(), Some(42));
    }

    #[test]
    fn spawn_worker_hands_the_socket_on_fd_3() {
        // `sh` echoes back through fd 3 — the inheritance slot every worker reads its socket
        // from. Receiving it here proves the dup2 landed and CLOEXEC was cleared on the copy.
        let (host, mut child) = spawn_worker(
            Path::new("/bin/sh"),
            "pf-test-worker",
            &["-c", r#"printf '"pong"' >&3"#],
        )
        .unwrap();
        let mut buf = Vec::new();
        let (got, fds) = recv_fds::<String>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(got, "pong");
        assert!(fds.is_empty());
        child.wait().unwrap();
    }
}
