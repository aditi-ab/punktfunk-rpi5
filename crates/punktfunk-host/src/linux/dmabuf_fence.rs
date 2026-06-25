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
//! wait, no harm, and `waited=false` tells us the driver doesn't fence (so zero-copy would still race).

use std::os::fd::RawFd;

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

/// Wait until the producer's writes to `dmabuf_fd` complete (or `timeout_ms` elapses). Returns:
/// - `Ok(true)`  — a render was still in flight and we waited on its fence (the race was real, now closed).
/// - `Ok(false)` — no fence / already signaled (the driver attaches no implicit fence; zero-copy can race).
/// - `Err`       — the ioctl failed (e.g. the kernel/driver lacks `EXPORT_SYNC_FILE`).
pub fn wait_read_ready(dmabuf_fd: RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    let mut req = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    let r = unsafe { libc::ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut req) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sync_fd = req.fd;
    if sync_fd < 0 {
        return Ok(false); // no sync_file exported
    }
    let mut pfd = libc::pollfd {
        fd: sync_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Non-blocking probe: not-yet-signaled (poll==0) means the producer is still rendering.
    let pending = unsafe { libc::poll(&mut pfd, 1, 0) } == 0;
    if pending {
        pfd.revents = 0;
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) }; // block until the render fence signals
    }
    unsafe { libc::close(sync_fd) };
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ioctl number must match linux/dma-buf.h exactly — it's computed, so lock it down.
    #[test]
    fn ioctl_number_matches_dma_buf_h() {
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xC008_6202);
    }
}
