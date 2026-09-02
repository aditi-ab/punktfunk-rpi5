//! Consumer-side DRM timeline-syncobj wait/signal for PipeWire
//! `SPA_META_SyncTimeline`.
//!
//! Unused: no target compositor ships a usable producer `sync_fd`.
//! Zero-copy waits the consumer dmabuf fence instead
//! (`pf_zerocopy::dmabuf_fence`). Kept compiled and ioctl-locked so a
//! producer can be wired without rediscovering request numbers.
//!
//! Syncobjs are DRM-core; any render node can import and wait them.
//! This opens `/dev/dri/renderD128` independently of the capture GPU.
//!
//! Pin: `ioctl_numbers_match_drm_h`, `signal_then_wait_roundtrip`.
#![allow(dead_code)]

use anyhow::{bail, Result};
use std::os::fd::RawFd;

// drm.h ioctls on the 'd' (0x64) magic. _IOWR = dir(3)<<30 | size<<16 | 0x64<<8 | nr.
const fn iowr(nr: u32, size: usize) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | (0x64u64 << 8) | nr as u64
}

#[repr(C)]
#[derive(Default)]
struct DrmSyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmSyncobjDestroy {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmSyncobjTimelineWait {
    handles: u64,
    points: u64,
    /// Absolute CLOCK_MONOTONIC deadline, nanoseconds.
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmSyncobjTimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = iowr(0xC0, std::mem::size_of::<DrmSyncobjDestroy>());
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u64 = iowr(0xC2, std::mem::size_of::<DrmSyncobjHandle>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u64 =
    iowr(0xCA, std::mem::size_of::<DrmSyncobjTimelineWait>());
const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u64 =
    iowr(0xCD, std::mem::size_of::<DrmSyncobjTimelineArray>());

/// The producer's point may not be attached yet when the buffer reaches us.
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;

pub struct DrmSync {
    fd: RawFd,
}

impl DrmSync {
    pub fn open() -> Result<DrmSync> {
        let path = c"/dev/dri/renderD128";
        // SAFETY: `path` is a 'static NUL-terminated C string literal; `open` only reads it as a
        // filesystem path and returns an fd (or -1). No Rust memory is aliased or handed to the kernel.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            bail!("open /dev/dri/renderD128 for syncobj ops: {}", errno());
        }
        Ok(DrmSync { fd })
    }

    fn import(&self, syncobj_fd: RawFd) -> Result<u32> {
        let mut req = DrmSyncobjHandle {
            fd: syncobj_fd,
            ..Default::default()
        };
        // SAFETY: `self.fd` is the live render-node fd from `open`; the request number encodes
        // `size_of::<DrmSyncobjHandle>()` (the bytes the kernel copies), and `&mut req` is a live,
        // correctly-sized `#[repr(C)]` struct the FD_TO_HANDLE ioctl reads (`fd`) and writes (`handle`).
        let r = unsafe { libc::ioctl(self.fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut req) };
        if r < 0 {
            bail!("SYNCOBJ_FD_TO_HANDLE: {}", errno());
        }
        Ok(req.handle)
    }

    fn destroy(&self, handle: u32) {
        let mut req = DrmSyncobjDestroy {
            handle,
            ..Default::default()
        };
        // SAFETY: `self.fd` is the live render-node fd; `DRM_IOCTL_SYNCOBJ_DESTROY` encodes
        // `size_of::<DrmSyncobjDestroy>()`, and `&mut req` is a live correctly-sized struct the kernel reads.
        unsafe { libc::ioctl(self.fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut req) };
    }

    /// Buffer contents are ready only after this returns Ok.
    pub fn wait_point(&self, syncobj_fd: RawFd, point: u64, timeout_ms: u64) -> Result<()> {
        let handle = self.import(syncobj_fd)?;
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `CLOCK_MONOTONIC` is a valid clock id and `&mut now` is a live `libc::timespec` the
        // kernel fills in; the call returns before `now` is read, so there is no aliasing/lifetime issue.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        let deadline = now.tv_sec * 1_000_000_000 + now.tv_nsec + timeout_ms as i64 * 1_000_000;
        let handles = [handle];
        let points = [point];
        let mut req = DrmSyncobjTimelineWait {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            timeout_nsec: deadline,
            count_handles: 1,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
            ..Default::default()
        };
        // SAFETY: `self.fd` is the live render-node fd; the request number encodes
        // `size_of::<DrmSyncobjTimelineWait>()`; `&mut req` is a live correctly-sized struct. Its
        // `handles`/`points` u64 fields hold the addresses of the local `handles`/`points` arrays, which
        // outlive this synchronous call, and `count_handles == 1` matches their length — so every kernel
        // read through those addresses stays in bounds.
        let r = unsafe { libc::ioctl(self.fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut req) };
        let saved = errno();
        self.destroy(handle);
        if r < 0 {
            bail!("SYNCOBJ_TIMELINE_WAIT(point {point}): {saved}");
        }
        Ok(())
    }

    /// Producer may reuse the buffer. Signal skipped frames too or it stalls.
    pub fn signal_point(&self, syncobj_fd: RawFd, point: u64) -> Result<()> {
        let handle = self.import(syncobj_fd)?;
        let handles = [handle];
        let points = [point];
        let mut req = DrmSyncobjTimelineArray {
            handles: handles.as_ptr() as u64,
            points: points.as_ptr() as u64,
            count_handles: 1,
            flags: 0,
        };
        // SAFETY: `self.fd` is the live render-node fd; the request number encodes
        // `size_of::<DrmSyncobjTimelineArray>()`; `&mut req` is a live correctly-sized struct whose
        // `handles`/`points` u64 fields address the local `handles`/`points` arrays (alive for this
        // synchronous call, `count_handles == 1` matching their length).
        let r = unsafe { libc::ioctl(self.fd, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &mut req) };
        let saved = errno();
        self.destroy(handle);
        if r < 0 {
            bail!("SYNCOBJ_TIMELINE_SIGNAL(point {point}): {saved}");
        }
        Ok(())
    }
}

impl Drop for DrmSync {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is the fd `open` returned; this `DrmSync` owns it exclusively and `close`
        // runs exactly once (here, in `Drop`), so there is no double-close or use-after-close.
        unsafe { libc::close(self.fd) };
    }
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Computed `_IOWR` numbers; lock them to drm.h.
    #[test]
    fn ioctl_numbers_match_drm_h() {
        assert_eq!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, 0xC010_64C2);
        assert_eq!(DRM_IOCTL_SYNCOBJ_DESTROY, 0xC008_64C0);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, 0xC028_64CA);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, 0xC018_64CD);
    }

    /// Live render node; missing `/dev/dri` returns early (CI).
    #[test]
    fn signal_then_wait_roundtrip() {
        let Ok(sync) = DrmSync::open() else {
            eprintln!("no render node — skipping");
            return;
        };
        #[repr(C)]
        #[derive(Default)]
        struct Create {
            handle: u32,
            flags: u32,
        }
        const CREATE: u64 = iowr(0xBF, std::mem::size_of::<Create>());
        const HANDLE_TO_FD: u64 = iowr(0xC1, std::mem::size_of::<DrmSyncobjHandle>());
        let mut c = Create::default();
        // SAFETY: `sync.fd` is the live render-node fd; `CREATE` encodes `size_of::<Create>()`, and
        // `&mut c` is a live correctly-sized struct the kernel fills (`handle`).
        assert!(unsafe { libc::ioctl(sync.fd, CREATE, &mut c) } >= 0);
        let mut h = DrmSyncobjHandle {
            handle: c.handle,
            ..Default::default()
        };
        // SAFETY: `sync.fd` is live; `HANDLE_TO_FD` encodes `size_of::<DrmSyncobjHandle>()`; `&mut h`
        // is a live correctly-sized struct (the kernel reads `handle`, writes `fd`).
        assert!(unsafe { libc::ioctl(sync.fd, HANDLE_TO_FD, &mut h) } >= 0);
        sync.signal_point(h.fd, 1).expect("signal");
        sync.wait_point(h.fd, 1, 100).expect("wait after signal");
        // SAFETY: `h.fd` is the fd HANDLE_TO_FD just exported; we own it and close it exactly once here.
        unsafe { libc::close(h.fd) };
        sync.destroy(c.handle);
    }
}
