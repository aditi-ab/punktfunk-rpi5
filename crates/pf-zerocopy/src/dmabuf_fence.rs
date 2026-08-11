//! Consumer-side implicit-fence wait for dmabuf capture (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`).
//!
//! Mutter renders its virtual monitor DIRECTLY into the PipeWire dmabuf and hands the buffer over
//! at GPU-submit time. With no fencing the consumer can sample mid-render and encode the buffer's
//! *previous* contents — the "stale/old frame" flashing on NVIDIA (KWin/gamescope blit into the
//! buffer so they don't hit this). The producer-driven fix is PipeWire explicit sync, but
//! Mutter+NVIDIA can't produce a sync_fd (`error alloc buffers` / no cogl sync_fd).
//!
//! So sync from the *consumer* side instead: a dmabuf carries its in-flight GPU work as an implicit
//! fence on its reservation object. `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` snapshots that into a sync_file
//! fd we can `poll()` — readable once the producer's writes complete. This makes zero-copy capture
//! race-free WITHOUT the producer doing anything, *iff* the driver actually attaches the fence. If it
//! attaches none, the export yields an already-signaled sync_file (poll returns immediately) — no
//! wait, no harm, and `WaitOutcome::NoFence` tells us the driver doesn't fence (so zero-copy
//! would still race).

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

// linux/dma-buf.h ioctls on the DMA_BUF_BASE ('b' = 0x62) magic. _IOWR = dir(3)<<30 | size<<16 | base<<8 | nr.
const DMA_BUF_BASE: u64 = 0x62;
const fn iowr(nr: u32, size: usize) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | (DMA_BUF_BASE << 8) | nr as u64
}

#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u64 = iowr(2, std::mem::size_of::<DmaBufExportSyncFile>());
/// We will READ the buffer → export the fence(s) we must wait for before reading (the producer's writes).
const DMA_BUF_SYNC_READ: u32 = 1 << 0;

/// What the implicit-fence wait actually observed — the operator-facing diagnostic for "does
/// implicit fencing work on this box" must not conflate these (a timeout or an interrupted wait
/// proceeds with a possibly mid-render buffer; a signaled fence closed the race for real).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitOutcome {
    /// No sync_file / already signaled — the driver attaches no implicit fence (or the render
    /// finished before we looked); zero-copy can still race.
    NoFence,
    /// A render was in flight and we waited until its fence signaled — the race was real, now closed.
    Signaled,
    /// A render was in flight and `timeout_ms` elapsed first — we proceed (fail-open: blocking
    /// longer would stall capture), possibly encoding a mid-render buffer.
    TimedOut,
}

/// Wait until the producer's writes to `dmabuf_fd` complete (or `timeout_ms` elapses; negative =
/// no timeout). `Err` means the ioctl or the poll itself failed (e.g. the kernel/driver lacks
/// `EXPORT_SYNC_FILE`); see [`WaitOutcome`] for the success cases.
pub fn wait_read_ready(dmabuf_fd: RawFd, timeout_ms: i32) -> std::io::Result<WaitOutcome> {
    let mut req = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: `dmabuf_fd` is a live dmabuf fd supplied by the caller (borrowed for this call; we
    // never close it). `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` encodes `size_of::<DmaBufExportSyncFile>()`
    // — the exact byte count the kernel copies — and `&mut req` is a live, correctly-sized
    // `#[repr(C)]` struct the EXPORT_SYNC_FILE ioctl reads (`flags`) and writes (`fd`). `req`
    // outlives this synchronous call and is not aliased elsewhere.
    let r = unsafe { libc::ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut req) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sync_fd = req.fd;
    if sync_fd < 0 {
        return Ok(WaitOutcome::NoFence); // no sync_file exported
    }
    let outcome = poll_readable(sync_fd, timeout_ms);
    // SAFETY: `sync_fd` is the sync_file fd the EXPORT_SYNC_FILE ioctl created and handed us to own;
    // this point is reached only when `sync_fd >= 0`, this `close` runs exactly once on it, and it is
    // never used afterward — no double-close or use-after-close.
    unsafe { libc::close(sync_fd) };
    outcome
}

/// Poll `fd` for `POLLIN`: a non-blocking probe first (already-readable ⇒ [`WaitOutcome::NoFence`]
/// — the fence was signaled before we looked), then a blocking wait up to `timeout_ms` (negative =
/// no timeout). `EINTR` retries with the remaining budget instead of silently skipping the wait —
/// the host spawns subprocesses, so a `SIGCHLD` mid-poll is a real occurrence, and skipping here
/// would reopen the exact stale-frame race this file exists to close.
fn poll_readable(fd: RawFd, timeout_ms: i32) -> std::io::Result<WaitOutcome> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Non-blocking probe: not-yet-readable (poll == 0) means the producer is still rendering.
    let probed = loop {
        // SAFETY: `&mut pfd` points at a single live `libc::pollfd` and `nfds == 1` matches that
        // one element; `fd` is the caller's live sync_file fd. `poll` reads `fd`/`events` and
        // writes `revents` for this non-blocking (timeout 0) probe, then returns — `pfd` outlives
        // the call and aliases nothing.
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r >= 0 {
            break r;
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    };
    if probed > 0 {
        if pfd.revents & libc::POLLIN != 0 {
            return Ok(WaitOutcome::NoFence); // signaled before we looked
        }
        // POLLERR/POLLNVAL without POLLIN — the fd is broken, not signaled.
        return Err(std::io::Error::other(format!(
            "poll(sync_file) revents {:#x} without POLLIN",
            pfd.revents
        )));
    }
    let deadline =
        (timeout_ms >= 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
    loop {
        let remaining = match deadline {
            None => -1, // poll's "no timeout"
            Some(d) => match d.checked_duration_since(Instant::now()) {
                None => return Ok(WaitOutcome::TimedOut),
                // +1: round up so a sub-millisecond remainder still waits instead of busy-polling.
                Some(rem) => (rem.as_millis() as i32).saturating_add(1),
            },
        };
        pfd.revents = 0;
        // SAFETY: same live single-element `pfd` (its `revents` reset to 0 just above), `nfds == 1`,
        // and `fd` still open (closed by the caller only after this function returns). This blocking
        // `poll` (up to `remaining` ms) waits for the fence to signal; it reads `fd`/`events`,
        // writes `revents`, and returns before `pfd` ends.
        let r = unsafe { libc::poll(&mut pfd, 1, remaining) };
        match r {
            0 => return Ok(WaitOutcome::TimedOut),
            r if r > 0 => {
                if pfd.revents & libc::POLLIN != 0 {
                    return Ok(WaitOutcome::Signaled);
                }
                // POLLERR/POLLNVAL without POLLIN — the fd is broken, not signaled.
                return Err(std::io::Error::other(format!(
                    "poll(sync_file) revents {:#x} without POLLIN",
                    pfd.revents
                )));
            }
            _ => {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINTR) {
                    return Err(e);
                }
                // Interrupted — recompute the remaining budget and keep waiting.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ioctl number must match linux/dma-buf.h exactly — it's computed, so lock it down.
    #[test]
    fn ioctl_number_matches_dma_buf_h() {
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xC008_6202);
    }

    /// The poll state machine, driven by a pipe (readable ⇔ signaled): a not-yet-readable fd that
    /// stays quiet is a truthful `TimedOut` (not the old `Ok(true)`), and one already readable at
    /// the probe is `NoFence`.
    #[test]
    fn poll_readable_reports_the_truth() {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let (r, mut w) = std::io::pipe().unwrap();
        assert_eq!(
            poll_readable(r.as_raw_fd(), 10).unwrap(),
            WaitOutcome::TimedOut
        );
        w.write_all(b"x").unwrap();
        assert_eq!(
            poll_readable(r.as_raw_fd(), 10).unwrap(),
            WaitOutcome::NoFence
        );
    }
}
