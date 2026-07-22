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
//! Coordinates are published VERBATIM in the OS's desktop space (`IDARG_OUT_QUERY_HWCURSOR::X/Y`
//! = the shape's top-left, can be negative); the host subtracts its monitor's desktop origin.
//! Shape pixels are the OS's 32-bpp rows at `Pitch` — BGRA for ALPHA cursors, color+mask for
//! MASKED_COLOR — copied raw; the host converts (keeping this thread dumb and allocation-free
//! after startup).

use core::sync::atomic::{AtomicU32, Ordering, fence};

use pf_driver_proto::cursor::{
    CURSOR_MAGIC, CURSOR_SHAPE_BYTES, CURSOR_SHAPE_MAX, CURSOR_SHAPE_OFFSET, CURSOR_SHM_SIZE,
    CursorShm,
};
use wdk_iddcx::nt_success;
use wdk_sys::iddcx;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

/// The host's `IOCTL_SET_CURSOR_CHANNEL` delivery: the [`CursorShm`] mapping handle VALUE,
/// already duplicated into this WUDFHost process. Owning a `CursorChannel` means owning the
/// handle; `Drop` closes it unless [`into_unowned`](Self::into_unowned) disarmed that (the
/// not-adopted reject path, where the host reaps remotely) or the worker consumed it.
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

/// The live worker: stops + joins on drop (monitor departure / replacement).
pub struct CursorWorker {
    stop: isize,
    /// The OS cursor-data event handle VALUE — kept so a later swap-chain (re)assignment can
    /// RE-ISSUE [`IddCxMonitorSetupHardwareCursor`] on the freshly committed path (the setup is
    /// per-mode-commit: the OS silently reverts to software cursor on every mode commit). The
    /// worker thread still owns closing it on exit; this is a read-only copy for re-setup.
    data_event: isize,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CursorWorker {
    /// The OS cursor-data event, for [`setup_hardware_cursor`] re-issue.
    pub fn data_event(&self) -> isize {
        self.data_event
    }
}

/// Declare (or RE-declare) the hardware cursor for `monitor` against `data_event`. Called at
/// initial channel delivery AND on every swap-chain assignment (a mode commit reverts the path
/// to software cursor). Must run OUTSIDE the monitors lock — the DDI may call back into the
/// mode callbacks. Returns the DDI status.
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
    // live event handle (the worker owns it for its lifetime, and re-setup only runs while the
    // worker is present).
    unsafe { wdk_iddcx::IddCxMonitorSetupHardwareCursor(monitor, &setup) }
}

// NOTE: there is NO un-declare path. Re-issuing `IddCxMonitorSetupHardwareCursor` with empty
// caps (no alpha, XOR NONE, zero max dims) is rejected `STATUS_INVALID_PARAMETER` — observed
// on-glass (26100, driver 9.9.0722.1407). The composite flip therefore works by FLAG + MODE
// RE-COMMIT: `monitor::set_cursor_forward(false)` stores the flag, the host forces a same-mode
// re-commit, and the OS's per-commit software-cursor default sticks because
// `monitor::resetup_cursor` skips the flagged monitor.

// SAFETY: `stop` is an event handle value; the worker owns every other resource.
unsafe impl Send for CursorWorker {}

impl Drop for CursorWorker {
    fn drop(&mut self) {
        // SAFETY: `stop` is our owned manual-reset event; signal + join, then close.
        unsafe {
            let _ = SetEvent(HANDLE(self.stop as *mut core::ffi::c_void));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // SAFETY: the worker has exited; nothing else references the handle.
        unsafe {
            let _ = CloseHandle(HANDLE(self.stop as *mut core::ffi::c_void));
        }
    }
}

/// Declare the hardware cursor for `monitor` and start the query→publish worker over the
/// delivered section. `None` on any failure (mapping/magic/event/DDI) — the caller logs and the
/// session simply keeps the composited-cursor behavior (the host times out waiting for a first
/// seqlock publish and falls back the same way).
pub fn setup_and_spawn(
    monitor: iddcx::IDDCX_MONITOR,
    ch: CursorChannel,
    declare: bool,
) -> Option<CursorWorker> {
    // Map the host-created section. FILE_MAP_READ|WRITE: we write, the host reads.
    let mapping = HANDLE(ch.handle as *mut core::ffi::c_void);
    // SAFETY: `mapping` is the duplicated section handle we own; size is the fixed contract size.
    let view = unsafe {
        MapViewOfFile(
            mapping,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            CURSOR_SHM_SIZE,
        )
    };
    if view.Value.is_null() {
        dbglog!("[pf-vd] cursor: MapViewOfFile failed — keeping composited cursor");
        return None;
    }
    let shm = view.Value.cast::<CursorShm>();
    // SAFETY: the view spans CURSOR_SHM_SIZE >= size_of::<CursorShm>(); reading the host stamp.
    if unsafe { core::ptr::addr_of!((*shm).magic).read_volatile() } != CURSOR_MAGIC {
        dbglog!("[pf-vd] cursor: section magic mismatch — rejecting");
        // SAFETY: unmapping the view we just mapped.
        unsafe {
            let _ = UnmapViewOfFile(view);
        }
        return None;
    }

    // Auto-reset data event (the OS signals it per cursor update) + manual-reset stop event.
    // SAFETY: plain event creation, no names, no security descriptor.
    let (data_evt, stop_evt) = unsafe {
        match (
            CreateEventW(None, false, false, None),
            CreateEventW(None, true, false, None),
        ) {
            (Ok(d), Ok(s)) => (d, s),
            _ => {
                let _ = UnmapViewOfFile(view);
                dbglog!("[pf-vd] cursor: event creation failed");
                return None;
            }
        }
    };

    // One caps definition for initial setup AND the per-mode-commit re-setup — see
    // `setup_hardware_cursor` for the FULL rationale. `declare = false` (a delivery landing
    // while the session is in the COMPOSITE render mode) skips the declaration: the worker
    // spawns anyway so a later enable-flip has its event to declare against; its queries just
    // fail NOT_SUPPORTED until then (logged once, harmless).
    if declare {
        let st = setup_hardware_cursor(monitor, data_evt.0 as isize);
        if !nt_success(st) {
            dbglog!(
                "[pf-vd] cursor: IddCxMonitorSetupHardwareCursor failed 0x{:08x}",
                st as u32
            );
            // SAFETY: cleaning up the resources created above.
            unsafe {
                let _ = CloseHandle(data_evt);
                let _ = CloseHandle(stop_evt);
                let _ = UnmapViewOfFile(view);
            }
            return None;
        }
        dbglog!("[pf-vd] cursor: hardware cursor declared — worker starting");
    } else {
        dbglog!("[pf-vd] cursor: channel adopted UNdeclared (composite mode) — worker starting");
    }

    // Ownership crossing into the thread as plain values (HANDLE/pointer aren't Send).
    let monitor_v = monitor as usize;
    let view_v = view.Value as usize;
    let data_v = data_evt.0 as isize;
    let stop_v = stop_evt.0 as isize;
    let mapping_v = ch.handle;
    ch.into_unowned(); // the worker owns the mapping handle from here (closed on exit below)

    let join = std::thread::Builder::new()
        .name("pf-vd-cursor".into())
        .spawn(move || {
            run_worker(monitor_v, view_v, data_v, stop_v);
            // SAFETY: the worker is the sole owner of these at exit; close/unmap exactly once.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view_v as *mut core::ffi::c_void,
                });
                let _ = CloseHandle(HANDLE(data_v as *mut core::ffi::c_void));
                let _ = CloseHandle(HANDLE(mapping_v as *mut core::ffi::c_void));
            }
        })
        .ok()?;

    Some(CursorWorker {
        stop: stop_v,
        data_event: data_v,
        join: Some(join),
    })
}

/// The wait→query→publish loop. Exits when the stop event signals.
fn run_worker(monitor_v: usize, view_v: usize, data_v: isize, stop_v: isize) {
    let monitor = monitor_v as iddcx::IDDCX_MONITOR;
    let shm = view_v as *mut CursorShm;
    let shape_dst = (view_v + CURSOR_SHAPE_OFFSET) as *mut u8;
    let mut shape_buf = vec![0u8; CURSOR_SHAPE_BYTES];
    let mut last_shape_id: u32 = 0;
    let mut query_warned = false;
    let mut published = false;
    let handles = [
        HANDLE(stop_v as *mut core::ffi::c_void),
        HANDLE(data_v as *mut core::ffi::c_void),
    ];
    loop {
        // POLL, not pure event-wait: the OS fires `hNewCursorDataAvailable` only for cursors it
        // routes through the hardware plane — masked/monochrome cursors (I-beam, hand, move,
        // resize) don't fire it. Polling does NOT recover them either (they are simply not in
        // this query's world — see the caps comment above); the ~30 Hz timeout just keeps the
        // seqlock's position/visibility fresh across missed events, and the query with the
        // current `LastShapeId` is a cheap no-op when nothing changed.
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
        // SAFETY: zero-init is a valid OUT arg (the OS writes every field it reports). v3: the
        // base query DDI slot is stubbed to NOT_SUPPORTED on current IddCx.
        let mut out: iddcx::IDARG_OUT_QUERY_HWCURSOR3 = unsafe { core::mem::zeroed() };
        // SAFETY: `monitor` is live (departure drops this worker FIRST), args outlive the call.
        let st =
            unsafe { wdk_iddcx::IddCxMonitorQueryHardwareCursor3(monitor, &in_args, &mut out) };
        if !nt_success(st) {
            if !query_warned {
                query_warned = true;
                dbglog!(
                    "[pf-vd] cursor: query failed 0x{:08x} (logged once)",
                    st as u32
                );
            }
            continue;
        }
        query_warned = false;
        if !published {
            published = true;
            dbglog!("[pf-vd] cursor: publishes live");
        }
        // Log each distinct SHAPE (human-paced): type (1=masked_color, 2=alpha), dims,
        // visibility. Shows which cursors reach us (does VSCode's hand arrive?) and their
        // type (masked ⇒ the OS may still software-composite it → the double-cursor report).
        if out.IsCursorShapeUpdated != 0 {
            dbglog!(
                "[pf-vd] cursor SHAPE id={} type={} {}x{} vis={} posvalid={}",
                out.CursorShapeInfo.ShapeId,
                out.CursorShapeInfo.CursorType as u32,
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
        // SAFETY: exclusive writer (single worker per section); plain volatile field writes.
        unsafe {
            core::ptr::addr_of_mut!((*shm).visible).write_volatile(if out.IsCursorVisible != 0 {
                1
            } else {
                0
            });
            // v3 `X`/`Y` are only meaningful when `PositionValid`; otherwise keep the prior
            // position (a position-invalid tick still carries a shape/visibility update).
            if out.PositionValid != 0 {
                core::ptr::addr_of_mut!((*shm).x).write_volatile(out.X);
                core::ptr::addr_of_mut!((*shm).y).write_volatile(out.Y);
            }
            if out.IsCursorShapeUpdated != 0 && out.IsCursorVisible != 0 {
                let info = &out.CursorShapeInfo;
                let rows = info.Height.min(CURSOR_SHAPE_MAX);
                let bytes = (rows as usize * info.Pitch as usize).min(CURSOR_SHAPE_BYTES);
                core::ptr::copy_nonoverlapping(shape_buf.as_ptr(), shape_dst, bytes);
                core::ptr::addr_of_mut!((*shm).cursor_type).write_volatile(info.CursorType as u32);
                core::ptr::addr_of_mut!((*shm).width)
                    .write_volatile(info.Width.min(CURSOR_SHAPE_MAX));
                core::ptr::addr_of_mut!((*shm).height).write_volatile(rows);
                core::ptr::addr_of_mut!((*shm).pitch).write_volatile(info.Pitch);
                core::ptr::addr_of_mut!((*shm).hot_x).write_volatile(info.XHot);
                core::ptr::addr_of_mut!((*shm).hot_y).write_volatile(info.YHot);
                core::ptr::addr_of_mut!((*shm).shape_id).write_volatile(info.ShapeId);
                last_shape_id = info.ShapeId;
            }
        }
        fence(Ordering::Release);
        seq.store(s.wrapping_add(2), Ordering::Release); // even = consistent
    }
}
