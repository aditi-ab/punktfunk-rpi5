//! Minimal DRM timeline-syncobj operations — the consumer side of PipeWire explicit sync
//! (`SPA_META_SyncTimeline`).
//!
//! RETAINED BUT CURRENTLY UNUSED: producer-driven explicit sync is the "right" fix, but no
//! compositor we target produces a usable sync_fd today — Mutter+NVIDIA fails buffer allocation
//! (`error alloc buffers`, no cogl sync_fd), KWin/gamescope blit so they don't race at all. We sync
//! zero-copy from the consumer side instead (see `pf_zerocopy::dmabuf_fence`). This module is kept,
//! verified (ioctl numbers + a live signal→wait round trip), ready to wire in the moment a producer
//! gains working `SPA_META_SyncTimeline`.
#![allow(dead_code)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]
//!
//! Compositors that render directly into the PipeWire buffer pool (Mutter's virtual
//! monitors) hand buffers over at GPU-submit time; on drivers without implicit dmabuf
//! fencing (NVIDIA) reading immediately races the render and shows the buffer's
//! *previous* contents. With explicit sync the producer attaches a timeline syncobj:
//! wait the acquire point before touching the buffer, signal the release point when done.
//!
//! Syncobjs are DRM-core objects: any render node can import and wait them, so this
//! opens its own fd independent of the capture GPU path.

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

    /// Import a syncobj fd into a (temporary) handle on our device.
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

    /// Block until `point` on the producer's timeline is signaled (the buffer's contents
    /// are ready), or `timeout_ms` passes.
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

    /// Signal `point` on the consumer release timeline — the producer may reuse the
    /// buffer. Must be called for every buffer that carried sync metadata, even when the
    /// frame was skipped, or the producer stalls waiting for it.
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

// `DrmSync::open` must not panic the PipeWire thread; everything is Result-based and the
// caller degrades to unsynchronized capture (with a loud warning) when it fails.
#[cfg(test)]
mod tests {
    use super::*;

    /// The ioctl numbers must match drm.h exactly — computed, so lock them down.
    #[test]
    fn ioctl_numbers_match_drm_h() {
        assert_eq!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, 0xC010_64C2);
        assert_eq!(DRM_IOCTL_SYNCOBJ_DESTROY, 0xC008_64C0);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, 0xC028_64CA);
        assert_eq!(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, 0xC018_64CD);
    }

    /// Round-trip against the real DRM device when one exists (CI containers skip).
    #[test]
    fn signal_then_wait_roundtrip() {
        let Ok(sync) = DrmSync::open() else {
            eprintln!("no render node — skipping");
            return;
        };
        // Create a fresh syncobj (CREATE = 0xBF), export it, signal point 1, wait point 1.
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
