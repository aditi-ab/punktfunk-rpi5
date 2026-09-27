//! IddCx hardware-cursor worker (proto v5, remote-desktop-sweep M2c).
//!
//! When the host ADDs a monitor with `hw_cursor` set and delivers a [`CursorShm`] section
//! (`IOCTL_SET_CURSOR_CHANNEL`), we declare a hardware cursor to the OS
//! (`IddCxMonitorSetupHardwareCursor`) — DWM then EXCLUDES the pointer from the desktop image
//! it renders into our swap-chain and instead signals our event on every cursor change. This
//! worker thread drains those signals (`IddCxMonitorQueryHardwareCursor`) and seqlock-publishes
//! shape + position + visibility into the host-created section; the host polls it at its
//! encode-tick pace (no event crosses the process boundary).
//!
//! Coordinates are published VERBATIM (`IDARG_OUT_QUERY_HWCURSOR::X/Y` = the shape's top-left
//! in this monitor's "screen co-ordinates", negative past its top-left edge).
//! Shape pixels are the OS's 32-bpp rows at `Pitch` — BGRA for ALPHA cursors, color+mask for
//! MASKED_COLOR — copied raw; the host converts.
//!
//! The same publish also lands in the monitor's [`CursorCell`] as frame-relative RGBA, which
//! the encode pool blends into its slots when the client draws no pointer.

use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::sync::Arc;

use pf_driver_proto::cursor::{
    CURSOR_MAGIC, CURSOR_SHAPE_BYTES, CURSOR_SHAPE_MAX, CURSOR_SHAPE_OFFSET, CURSOR_SHM_SIZE,
    CursorShm, shape_extent, shape_rgba,
};
use wdk_iddcx::nt_success;
use wdk_sys::iddcx;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::cursor_cell::{CursorCell, CursorImage};
use crate::worker::{OwnedHandle, OwnedView, Worker};

/// The host's `IOCTL_SET_CURSOR_CHANNEL` delivery: the [`CursorShm`] mapping handle VALUE,
/// already duplicated into this WUDFHost process. Owning a `CursorChannel` means owning the
/// handle; `Drop` closes it unless [`into_unowned`](Self::into_unowned) disarmed that — the
/// not-adopted reject path (the host reaps remotely), or [`setup_and_spawn`], which moves the
/// handle into an [`OwnedHandle`] that closes it instead.
pub struct CursorChannel {
    handle: u64,
    owned: bool,
}

impl CursorChannel {
    pub fn from_request(req: &pf_driver_proto::control::SetCursorChannelRequest) -> Option<Self> {
        if req.header_handle == 0 {
            return None;
        }
        Some(CursorChannel {
            handle: req.header_handle,
            owned: true,
        })
    }

    /// Disarm the Drop (delivery rejected — the handle stays for the host to reap remotely).
    pub fn into_unowned(mut self) {
        self.owned = false;
    }
}

impl Drop for CursorChannel {
    fn drop(&mut self) {
        if self.owned && self.handle != 0 {
            // SAFETY: we own this duplicated handle value; closing at most once (owned is our flag).
            unsafe {
                let _ = CloseHandle(HANDLE(self.handle as *mut core::ffi::c_void));
            }
        }
    }
}

/// Declare (or RE-declare) the hardware cursor for `monitor` against `data_event`, returning
/// the DDI status. Called at initial channel delivery AND on every swap-chain assignment, since
/// a mode commit reverts the path to a software cursor.
///
/// `data_event` is a BORROWED value: the monitor entry owns the event and closes it only after
/// joining the worker, so the handle this registers with the OS stays open for as long as the
/// OS may signal it. Must run OUTSIDE the monitors lock — the DDI may call back into the mode
/// callbacks, which take it.
pub fn setup_hardware_cursor(monitor: iddcx::IDDCX_MONITOR, data_event: isize) -> i32 {
    let caps = iddcx::IDDCX_CURSOR_CAPS {
        Size: core::mem::size_of::<iddcx::IDDCX_CURSOR_CAPS>() as u32,
        // On-glass finding (2026-07-22, .173): the hardware-cursor query delivers ONLY
        // `IDDCX_CURSOR_SHAPE_TYPE_ALPHA` shapes, in EVERY configuration — all three
        // ColorXorCursorSupport levels, event-driven AND ~30 Hz polled. Masked/monochrome
        // cursors (I-beam, resize, SIZEALL, app hand cursors) never arrive: NONE rejects the
        // query outright (STATUS_NOT_SUPPORTED); EMULATION software-composites them into the
        // frame (double cursor vs the client's stale local shape); FULL excludes them from the
        // frame but never delivers them (shape freezes on the last alpha cursor). FULL is the
        // right resting state: the frame stays cursor-free for ALL cursor types, and the
        // full-fidelity SHAPE comes from the session-side cursor source in the host, not from
        // this query (design/remote-desktop-sweep.md §8).
        ColorXorCursorSupport: iddcx::IDDCX_XOR_CURSOR_SUPPORT::IDDCX_XOR_CURSOR_SUPPORT_FULL,
        MaxX: CURSOR_SHAPE_MAX,
        MaxY: CURSOR_SHAPE_MAX,
        AlphaCursorSupport: 1,
    };
    let setup = iddcx::IDARG_IN_SETUP_HWCURSOR {
        CursorInfo: caps,
        hNewCursorDataAvailable: data_event as *mut core::ffi::c_void,
    };
    // SAFETY: `monitor` is a live IddCx monitor; `setup` outlives the call; `data_event` is a
    // live event handle owned by the monitor entry, which outlives every worker it declares.
    unsafe { wdk_iddcx::IddCxMonitorSetupHardwareCursor(monitor, &setup) }
}

// There is NO un-declare path: empty caps are rejected `STATUS_INVALID_PARAMETER`. The
// composite flip is a flag plus a mode re-commit: `monitor::set_cursor_forward(false)` stores
// the flag, the host forces a same-mode re-commit, and the OS's per-commit software-cursor
// default sticks because `Monitor::resetup_cursor` skips the flagged monitor.

/// Map the delivered section and start the query→publish worker for `monitor`.
///
/// Ownership is the point of this function. The section handle is adopted out of `ch` into an
/// [`OwnedView`] that unmaps the view and closes the mapping on EVERY exit — including a failed
/// spawn, which drops the closure holding it, so nothing is left behind in the host process's
/// handle table. That view then travels into the thread and is released when the thread returns.
/// `data_event` is only borrowed: the monitor entry owns it and closes it after the join.
///
/// `None` on any failure (mapping, magic, DDI); the caller keeps the composited cursor, which is
/// also what the host falls back to when no seqlock publish arrives.
pub fn setup_and_spawn(
    monitor: iddcx::IDDCX_MONITOR,
    ch: CursorChannel,
    declare: bool,
    data_event: isize,
    cell: Arc<CursorCell>,
) -> Option<Worker> {
    // SAFETY: the host duplicated this section handle into our process and `CursorChannel` hands
    // its ownership over here — `into_unowned` below disarms its own close.
    let mapping = unsafe { OwnedHandle::from_raw(HANDLE(ch.handle as *mut core::ffi::c_void)) };
    ch.into_unowned();
    // SAFETY: `mapping` is the section handle we just adopted; size is the fixed contract size.
    // FILE_MAP_READ|WRITE because we write the cursor state and the host reads it.
    let view = unsafe {
        MapViewOfFile(
            mapping.as_raw(),
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            CURSOR_SHM_SIZE,
        )
    };
    if view.Value.is_null() {
        dbglog!("[pf-vd] cursor: MapViewOfFile failed — keeping composited cursor");
        return None; // `mapping` drops here and closes
    }
    // SAFETY: `view` is the single mapping of `mapping` we just made; from here one value owns
    // both, and every return below unmaps and closes them.
    let view = unsafe { OwnedView::from_raw(view.Value, mapping) };
    let shm = view.base().cast::<CursorShm>();
    // SAFETY: the view spans CURSOR_SHM_SIZE >= size_of::<CursorShm>(); reading the host stamp.
    if unsafe { core::ptr::addr_of!((*shm).magic).read_volatile() } != CURSOR_MAGIC {
        dbglog!("[pf-vd] cursor: section magic mismatch — rejecting");
        return None;
    }

    // Spawn BEFORE declaring: a declaration names `data_event`, which the caller closes when
    // this returns `None`. `declare = false` (a delivery landing in COMPOSITE render mode)
    // already spawns undeclared, so a later enable-flip has an event to declare against.
    // The IddCx monitor handle travels as an integer; the view is `Send` itself.
    let monitor_v = monitor as usize;
    let worker = Worker::spawn("pf-vd-cursor", move |stop| {
        // The view unmaps when this thread returns.
        run_worker(monitor_v, view.base() as usize, data_event, stop, &cell);
    })?;

    if declare {
        let st = setup_hardware_cursor(monitor, data_event);
        if !nt_success(st) {
            dbglog!(
                "[pf-vd] cursor: IddCxMonitorSetupHardwareCursor failed 0x{:08x}",
                st as u32
            );
            // `worker` drops here → stops and joins the thread it just started.
            return None;
        }
        dbglog!("[pf-vd] cursor: hardware cursor declared — worker started");
    } else {
        dbglog!("[pf-vd] cursor: channel adopted UNdeclared (composite mode) — worker started");
    }
    Some(worker)
}

/// The wait→query→publish loop, exiting when `stop` signals.
///
/// This thread owns NOTHING it has to release. `stop` belongs to the [`Worker`] that spawned it
/// and `data_v` to the monitor entry, both of which close after the join; the mapping behind
/// `view_v` is unmapped by the [`OwnedView`] the spawning closure moved in here. Returning is
/// the whole cleanup.
fn run_worker(monitor_v: usize, view_v: usize, data_v: isize, stop: HANDLE, cell: &CursorCell) {
    let monitor = monitor_v as iddcx::IDDCX_MONITOR;
    let shm = view_v as *mut CursorShm;
    let shape_dst = (view_v + CURSOR_SHAPE_OFFSET) as *mut u8;
    let mut shape_buf = vec![0u8; CURSOR_SHAPE_BYTES];
    let mut last_shape_id: u32 = 0;
    // Consecutive failed queries: ~100 ms at the poll rate before the blend drops the pointer.
    const QUERY_FAILS_HIDE: u32 = 3;
    let mut query_fails: u32 = 0;
    let mut published = false;
    // The pool's copy of the latest publish; its position outlives shape-less ticks.
    let mut image: Option<CursorImage> = None;
    let handles = [stop, HANDLE(data_v as *mut core::ffi::c_void)];
    loop {
        // Poll as well as wait: the OS signals the event only for hardware-plane cursors, so
        // a ~30 Hz timeout keeps position and visibility fresh across the ones it never
        // signals for. The query with the current `LastShapeId` is a no-op when nothing moved.
        const POLL_MS: u32 = 33;
        // SAFETY: both handles are live for the worker's lifetime (owner drops after join).
        let w = unsafe { WaitForMultipleObjects(&handles, false, POLL_MS) };
        if w == WAIT_OBJECT_0 {
            return; // stop
        }
        // Query on the data event OR the poll timeout; anything else is a wait failure.
        if w != WAIT_TIMEOUT && w.0 != WAIT_OBJECT_0.0 + 1 {
            dbglog!("[pf-vd] cursor: wait returned {:#x} — worker exiting", w.0);
            return; // wait failed — owner is tearing down
        }
        let in_args = iddcx::IDARG_IN_QUERY_HWCURSOR {
            LastShapeId: last_shape_id,
            ShapeBufferSizeInBytes: CURSOR_SHAPE_BYTES as u32,
            pShapeBuffer: shape_buf.as_mut_ptr(),
        };
        // Zeroed, the OS writes every field it reports. v3: the base query DDI slot is stubbed
        // to NOT_SUPPORTED on current IddCx.
        let mut out = iddcx::IDARG_OUT_QUERY_HWCURSOR3::default();
        // SAFETY: `monitor` is live (departure drops this worker FIRST), args outlive the call.
        let st =
            unsafe { wdk_iddcx::IddCxMonitorQueryHardwareCursor3(monitor, &in_args, &mut out) };
        if !nt_success(st) {
            query_fails += 1;
            if query_fails == 1 {
                dbglog!(
                    "[pf-vd] cursor: query failed 0x{:08x} (logged once)",
                    st as u32
                );
            }
            // A lost declare answers STATUS_NOT_SUPPORTED for good: past a few polls, stop the
            // pool blending a pointer frozen where it last was.
            if query_fails == QUERY_FAILS_HIDE {
                // SAFETY: `shm` points at the mapped CursorShm for the worker's lifetime.
                let hdr = unsafe { core::ptr::read_volatile(shm) };
                cell.publish(&mut image, &hdr, None, false);
            }
            continue;
        }
        query_fails = 0;
        if !published {
            published = true;
            // The raw position: monitor-relative per the IddCx docs; this line shows it.
            dbglog!(
                "[pf-vd] cursor: publishes live (x={} y={} posvalid={})",
                out.X,
                out.Y,
                out.PositionValid
            );
        }
        // Log each distinct SHAPE (human-paced): type (1=masked_color, 2=alpha), dims,
        // visibility. Shows which cursors reach us (does VSCode's hand arrive?) and their
        // type (masked ⇒ the OS may still software-composite it → the double-cursor report).
        if out.IsCursorShapeUpdated != 0 {
            dbglog!(
                "[pf-vd] cursor SHAPE id={} type={} {}x{} vis={} posvalid={}",
                out.CursorShapeInfo.ShapeId,
                out.CursorShapeInfo.CursorType,
                out.CursorShapeInfo.Width,
                out.CursorShapeInfo.Height,
                out.IsCursorVisible,
                out.PositionValid,
            );
        }
        // Seqlock publish: odd → write → even. The header alone changes on position moves;
        // shape bytes are only rewritten when the OS says the image changed, so a reader that
        // skips unchanged shape_ids never observes torn pixels.
        // SAFETY: `shm` points at the mapped CursorShm for the worker's lifetime.
        let seq = unsafe { &*core::ptr::addr_of!((*shm).seq).cast::<AtomicU32>() };
        let s = seq.load(Ordering::Relaxed);
        seq.store(s.wrapping_add(1), Ordering::Relaxed); // odd = mid-update
        fence(Ordering::Release);
        let visible = out.IsCursorVisible != 0;
        let mut shape = None;
        // SAFETY: exclusive writer (single worker per section); plain volatile field writes,
        // then one volatile read of the header for the host-stamped scale.
        let hdr = unsafe {
            core::ptr::addr_of_mut!((*shm).visible).write_volatile(u32::from(visible));
            // v3 `X`/`Y` are only meaningful when `PositionValid`; otherwise keep the prior
            // position (a position-invalid tick still carries a shape/visibility update).
            if out.PositionValid != 0 {
                core::ptr::addr_of_mut!((*shm).x).write_volatile(out.X);
                core::ptr::addr_of_mut!((*shm).y).write_volatile(out.Y);
            }
            if out.IsCursorShapeUpdated != 0 && visible {
                let info = &out.CursorShapeInfo;
                let stamp = CursorShm {
                    cursor_type: info.CursorType,
                    width: info.Width,
                    height: info.Height,
                    pitch: info.Pitch,
                    hot_x: info.XHot,
                    hot_y: info.YHot,
                    ..bytemuck::Zeroable::zeroed()
                };
                let (width, rows, pitch) = shape_extent(&stamp);
                core::ptr::copy_nonoverlapping(shape_buf.as_ptr(), shape_dst, rows * pitch);
                core::ptr::addr_of_mut!((*shm).cursor_type).write_volatile(stamp.cursor_type);
                core::ptr::addr_of_mut!((*shm).width).write_volatile(width as u32);
                core::ptr::addr_of_mut!((*shm).height).write_volatile(rows as u32);
                core::ptr::addr_of_mut!((*shm).pitch).write_volatile(info.Pitch);
                core::ptr::addr_of_mut!((*shm).hot_x).write_volatile(info.XHot);
                core::ptr::addr_of_mut!((*shm).hot_y).write_volatile(info.YHot);
                core::ptr::addr_of_mut!((*shm).shape_id).write_volatile(info.ShapeId);
                last_shape_id = info.ShapeId;
                shape = Some(shape_rgba(&stamp, &shape_buf));
            }
            core::ptr::read_volatile(shm)
        };
        fence(Ordering::Release);
        seq.store(s.wrapping_add(2), Ordering::Release); // even = consistent
        cell.publish(&mut image, &hdr, shape, visible);
    }
}
