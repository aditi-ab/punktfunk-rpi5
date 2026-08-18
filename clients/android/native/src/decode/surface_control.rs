//! The `ASurfaceControl` compositor layer behind the ASurfaceControl presenter backend.
//!
//! This is the Android analogue of what the Apple client gets from `CAMetalDisplayLink` +
//! `preferredFrameLatency = 1`: a present path that schedules each frame against the panel's own
//! timeline and hands back the *real* present feedback, instead of the MediaCodec→SurfaceView→
//! BufferQueue path that predicts the latch and hopes the `OnFrameRendered` callbacks arrive.
//!
//! A `Layer` owns one `ASurfaceControl` created as a child of the SurfaceView's `ANativeWindow`;
//! the decoder renders into an `AImageReader` and the
//! presenter composites each acquired `AHardwareBuffer` onto this layer via an `ASurfaceTransaction`
//! that carries a desired present time (the single actuator both present modes drive) and an
//! acquire fence. Every applied transaction registers a one-shot completion callback that reports
//! the frame's real latch time and the *previous* buffer's release fence back through the decode
//! loop's event channel — the truthful present clock the cadence loop and the glass budget were
//! missing.
//!
//! Every `ASurface*` entry point is **API 29** — above the crate's minSdk-28 floor — so all are
//! `dlsym`-resolved from `libandroid.so`, exactly as [`crate::adpf`] and [`super::vsync`] resolve
//! their own >-floor symbols; a hard import of any of them would make `System.loadLibrary` fail on
//! every API-28 device even where this backend is never selected. Absent (or a null layer) ⇒
//! [`Layer::create`] returns `None` and the caller falls back to the SurfaceView presenter.

use ndk::hardware_buffer::HardwareBuffer;
use ndk::native_window::NativeWindow;
use std::ffi::c_void;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc};

use super::async_loop::DecodeEvent;

// ---- Opaque native types (not in `ndk-sys 0.6`) ------------------------------------------------

#[repr(C)]
struct ASurfaceControl {
    _p: [u8; 0],
}
#[repr(C)]
struct ASurfaceTransaction {
    _p: [u8; 0],
}
#[repr(C)]
struct ASurfaceTransactionStats {
    _p: [u8; 0],
}

/// `ARect` — the `setGeometry` source/destination rectangle (`android/native_window.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct ARect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `ANATIVEWINDOW_TRANSFORM_IDENTITY` — no rotation/flip; the decoder already emits upright frames.
const TRANSFORM_IDENTITY: i32 = 0;
/// `ASURFACE_TRANSACTION_VISIBILITY_SHOW`.
const VISIBILITY_SHOW: i8 = 1;

// ---- The `dlsym`-resolved entry-point table ----------------------------------------------------

type CreateFromWindowFn = unsafe extern "C" fn(
    *mut ndk_sys::ANativeWindow,
    *const std::ffi::c_char,
) -> *mut ASurfaceControl;
type AcReleaseFn = unsafe extern "C" fn(*mut ASurfaceControl);
type TxnCreateFn = unsafe extern "C" fn() -> *mut ASurfaceTransaction;
type TxnDeleteFn = unsafe extern "C" fn(*mut ASurfaceTransaction);
type TxnApplyFn = unsafe extern "C" fn(*mut ASurfaceTransaction);
type TxnSetBufferFn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *mut ndk_sys::AHardwareBuffer,
    RawFd,
);
type TxnSetVisibilityFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i8);
type TxnSetZOrderFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i32);
type TxnSetGeometryFn = unsafe extern "C" fn(
    *mut ASurfaceTransaction,
    *mut ASurfaceControl,
    *const ARect,
    *const ARect,
    i32,
);
type TxnSetDesiredPresentTimeFn = unsafe extern "C" fn(*mut ASurfaceTransaction, i64);
type TxnSetBufferDataSpaceFn =
    unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, i32);
type TxnSetFrameRateFn =
    unsafe extern "C" fn(*mut ASurfaceTransaction, *mut ASurfaceControl, f32, i8);
type OnCompleteCb = unsafe extern "C" fn(*mut c_void, *mut ASurfaceTransactionStats);
type TxnSetOnCompleteFn = unsafe extern "C" fn(*mut ASurfaceTransaction, *mut c_void, OnCompleteCb);
type StatsGetLatchTimeFn = unsafe extern "C" fn(*mut ASurfaceTransactionStats) -> i64;
type StatsGetPrevReleaseFenceFn =
    unsafe extern "C" fn(*mut ASurfaceTransactionStats, *mut ASurfaceControl) -> RawFd;

struct Api {
    create_from_window: CreateFromWindowFn,
    ac_release: AcReleaseFn,
    txn_create: TxnCreateFn,
    txn_delete: TxnDeleteFn,
    txn_apply: TxnApplyFn,
    txn_set_buffer: TxnSetBufferFn,
    txn_set_visibility: TxnSetVisibilityFn,
    txn_set_z_order: TxnSetZOrderFn,
    txn_set_geometry: TxnSetGeometryFn,
    txn_set_present_time: TxnSetDesiredPresentTimeFn,
    /// `setBufferDataSpace` is present from API 29 in practice but historically under-declared —
    /// resolved optionally, so an SDR stream (which never touches it) works even where it is absent.
    txn_set_dataspace: Option<TxnSetBufferDataSpaceFn>,
    /// `setFrameRate` is **API 30** — optional, `None` on API 29.
    txn_set_frame_rate: Option<TxnSetFrameRateFn>,
    txn_set_on_complete: TxnSetOnCompleteFn,
    stats_latch_time: StatsGetLatchTimeFn,
    stats_prev_release_fence: StatsGetPrevReleaseFenceFn,
}

impl Api {
    /// Resolve the whole `ASurface*` table from `libandroid.so`, or `None` on API < 29 (any required
    /// symbol absent). The two optional entries (`setBufferDataSpace`, `setFrameRate`) do not gate.
    fn resolve() -> Option<Api> {
        // SAFETY: `dlopen` of the always-mapped `libandroid.so` (only bumps its refcount; never
        // closed — a process-lifetime handle). Each `dlsym` returns null when the symbol is absent
        // (device below API 29), checked before transmuting the non-null pointer to its fn type.
        unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let req = |name: &std::ffi::CStr| -> Option<*mut c_void> {
                let p = libc::dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            Some(Api {
                create_from_window: std::mem::transmute::<*mut c_void, CreateFromWindowFn>(req(
                    c"ASurfaceControl_createFromWindow",
                )?),
                ac_release: std::mem::transmute::<*mut c_void, AcReleaseFn>(req(
                    c"ASurfaceControl_release",
                )?),
                txn_create: std::mem::transmute::<*mut c_void, TxnCreateFn>(req(
                    c"ASurfaceTransaction_create",
                )?),
                txn_delete: std::mem::transmute::<*mut c_void, TxnDeleteFn>(req(
                    c"ASurfaceTransaction_delete",
                )?),
                txn_apply: std::mem::transmute::<*mut c_void, TxnApplyFn>(req(
                    c"ASurfaceTransaction_apply",
                )?),
                txn_set_buffer: std::mem::transmute::<*mut c_void, TxnSetBufferFn>(req(
                    c"ASurfaceTransaction_setBuffer",
                )?),
                txn_set_visibility: std::mem::transmute::<*mut c_void, TxnSetVisibilityFn>(req(
                    c"ASurfaceTransaction_setVisibility",
                )?),
                txn_set_z_order: std::mem::transmute::<*mut c_void, TxnSetZOrderFn>(req(
                    c"ASurfaceTransaction_setZOrder",
                )?),
                txn_set_geometry: std::mem::transmute::<*mut c_void, TxnSetGeometryFn>(req(
                    c"ASurfaceTransaction_setGeometry",
                )?),
                txn_set_present_time: std::mem::transmute::<*mut c_void, TxnSetDesiredPresentTimeFn>(
                    req(c"ASurfaceTransaction_setDesiredPresentTime")?,
                ),
                txn_set_dataspace: req(c"ASurfaceTransaction_setBufferDataSpace")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetBufferDataSpaceFn>(p)),
                txn_set_frame_rate: req(c"ASurfaceTransaction_setFrameRate")
                    .map(|p| std::mem::transmute::<*mut c_void, TxnSetFrameRateFn>(p)),
                txn_set_on_complete: std::mem::transmute::<*mut c_void, TxnSetOnCompleteFn>(req(
                    c"ASurfaceTransaction_setOnComplete",
                )?),
                stats_latch_time: std::mem::transmute::<*mut c_void, StatsGetLatchTimeFn>(req(
                    c"ASurfaceTransactionStats_getLatchTime",
                )?),
                stats_prev_release_fence: std::mem::transmute::<
                    *mut c_void,
                    StatsGetPrevReleaseFenceFn,
                >(req(
                    c"ASurfaceTransactionStats_getPreviousReleaseFenceFd",
                )?),
            })
        }
    }
}

/// The `ASurfaceControl` handle, reference-counted so it outlives every in-flight transaction. The
/// layer holds one `Arc`; each pending completion callback's context holds another. `release` is
/// called exactly once — when the layer is dropped AND the last outstanding callback has fired — so
/// a completion that lands after teardown never indexes a freed control (the render-callback
/// reclaim hazard, in the transaction world).
struct ScHandle {
    sc: *mut ASurfaceControl,
    release: AcReleaseFn,
}

// SAFETY: `sc` is only ever passed back to `ASurface*` C entry points (never dereferenced in Rust),
// and its release is serialised by the `Arc` refcount reaching zero on whichever thread drops last.
unsafe impl Send for ScHandle {}
// SAFETY: as above — the raw handle is opaque to Rust and only handed to the thread-safe `ASurface*`
// C API; shared read access across threads (the completion callback) never mutates it.
unsafe impl Sync for ScHandle {}

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: created by `createFromWindow`; the `Arc` guarantees this is the sole, final release
        // and that no transaction or callback still references `sc`.
        unsafe { (self.release)(self.sc) };
    }
}

/// One presented transaction's real feedback, posted from the completion callback (a binder thread)
/// into the decode loop's event channel. The loop matches `seq` to the buffer it retired and frees
/// it once `prev_release_fence` signals.
pub(super) struct PresentComplete {
    /// The presenter's monotonically increasing submit sequence for this transaction.
    pub seq: u64,
    /// SurfaceFlinger's latch instant for this frame (`CLOCK_MONOTONIC` ns) — the truthful present
    /// clock: consecutive latches are one true panel period apart, and `latch − release` is the
    /// real `latch` stat, both of which the predicted path could only guess at.
    pub latch_ns: i64,
    /// The release fence for the buffer this transaction REPLACED (the previous frame on the
    /// layer), or `None` when the platform reports none. The loop deletes that buffer's image with
    /// this fence so it is returned to the reader's pool only once SurfaceFlinger is done with it.
    pub prev_release_fence: Option<OwnedFd>,
}

/// The completion callback's per-transaction context, leaked as a raw pointer into
/// `setOnComplete` and reclaimed inside the callback (which fires exactly once per applied
/// transaction). Carries only `Send` data so the binder-thread callback is sound.
struct CompleteCtx {
    tx: mpsc::Sender<DecodeEvent>,
    seq: u64,
    /// A shared reference to the layer's `ASurfaceControl`, needed to read the per-surface release
    /// fence out of the stats. Holding the `Arc` keeps the control alive for the callback even if
    /// the layer was already dropped.
    sc: Arc<ScHandle>,
    prev_fence_fn: StatsGetPrevReleaseFenceFn,
    latch_fn: StatsGetLatchTimeFn,
}

/// The `ASurfaceTransaction_OnComplete` trampoline (a binder thread). Reclaims its leaked context,
/// reads the real latch time + the previous buffer's release fence, and forwards them to the decode
/// loop. Panic-free by construction (an unwind out of an `extern "C"` fn would abort the process).
unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ASurfaceTransactionStats) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the `Box<CompleteCtx>` leaked in `Layer::present`; the platform delivers
    // it exactly once per applied transaction, so this single reclaim is correct.
    let ctx = unsafe { Box::from_raw(context as *mut CompleteCtx) };
    let latch_ns = if stats.is_null() {
        0
    } else {
        // SAFETY: `stats` is valid for the duration of this callback (platform contract).
        unsafe { (ctx.latch_fn)(stats) }
    };
    let prev_release_fence = if stats.is_null() {
        None
    } else {
        // SAFETY: valid stats + the layer's live `ASurfaceControl`; a returned fd is owned by us
        // and closed via `OwnedFd`. `-1` means no fence.
        let fd = unsafe { (ctx.prev_fence_fn)(stats, ctx.sc.sc) };
        // SAFETY: a non-negative fd returned by `getPreviousReleaseFenceFd` is a fresh owned fence
        // descriptor whose ownership the API transfers to us; wrapping it in `OwnedFd` closes it.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let _ = ctx.tx.send(DecodeEvent::PresentComplete(PresentComplete {
        seq: ctx.seq,
        latch_ns,
        prev_release_fence,
    }));
}

/// One `ASurfaceControl` layer, a child of the SurfaceView's window, that the presenter composites
/// decoded buffers onto. Owns nothing thread-shared; lives on and is dropped by the decode loop.
pub(super) struct Layer {
    api: Api,
    sc: Arc<ScHandle>,
    /// Destination rectangle (the SurfaceView's pixel size) — the buffer is scaled to fill it.
    dest_w: i32,
    dest_h: i32,
    /// `true` once the first transaction has made the layer visible + set its z-order + frame rate.
    configured: bool,
}

impl Layer {
    /// Create the compositor layer over `window` (the SurfaceView's `ANativeWindow`), or `None` on
    /// API < 29 / a null layer — the caller then uses the SurfaceView presenter.
    ///
    /// `dest_w/h` are the SurfaceView's **on-screen pixel size** — the coordinate space the child
    /// layer is composited into, which is the display footprint of the (aspect-fitted) video view,
    /// NOT the window's buffer size. `ANativeWindow_getWidth/Height` return the buffer geometry in a
    /// rotated/scaled space (observed 1260×567 for a 2800×1260 full-bleed stream) — using it shrank
    /// the picture to the top-left corner. A non-positive `dest_w/h` (Kotlin couldn't read the view
    /// yet) falls back to that buffer size as the best remaining guess.
    pub(super) fn create(window: &NativeWindow, dest_w: i32, dest_h: i32) -> Option<Layer> {
        let api = Api::resolve()?;
        // SAFETY: `window.ptr()` is the live `ANativeWindow` the decode thread owns; the name is a
        // static NUL-terminated string; the call returns null on failure (checked).
        let sc =
            unsafe { (api.create_from_window)(window.ptr().as_ptr(), c"punktfunk-video".as_ptr()) };
        if sc.is_null() {
            log::warn!("asc: createFromWindow returned null — falling back to SurfaceView");
            return None;
        }
        let dest_w = if dest_w > 0 {
            dest_w
        } else {
            window.width().max(1)
        };
        let dest_h = if dest_h > 0 {
            dest_h
        } else {
            window.height().max(1)
        };
        log::info!(
            "asc: layer created, dest {dest_w}x{dest_h} (window buffer {}x{})",
            window.width(),
            window.height(),
        );
        Some(Layer {
            sc: Arc::new(ScHandle {
                sc,
                release: api.ac_release,
            }),
            api,
            dest_w,
            dest_h,
            configured: false,
        })
    }

    /// Present one decoded buffer at `desired_present_ns` (`CLOCK_MONOTONIC`; `0` = ASAP). Consumes
    /// `acquire_fence` (ownership passes to SurfaceFlinger via `setBuffer`). Registers a one-shot
    /// completion that reports the real latch + the previous buffer's release fence on `ev_tx`,
    /// tagged with `seq`. `dataspace` is the HDR `ADataSpace` value (`0` = leave default/SDR).
    /// `frame_rate` votes the layer's rate once (`0.0` skips). Returns `false` if the transaction
    /// could not be created (the caller then frees the buffer itself).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn present(
        &mut self,
        buffer: &HardwareBuffer,
        src_w: i32,
        src_h: i32,
        acquire_fence: Option<OwnedFd>,
        desired_present_ns: i64,
        dataspace: i32,
        frame_rate: f32,
        seq: u64,
        ev_tx: &mpsc::Sender<DecodeEvent>,
    ) -> bool {
        // SAFETY: `txn_create` returns a fresh transaction or null; every setter below takes that
        // transaction + this layer's live `sc` + valid arguments; `apply`/`delete` consume it once.
        unsafe {
            let txn = (self.api.txn_create)();
            if txn.is_null() {
                // The acquire fence would leak if we returned without consuming it.
                drop(acquire_fence);
                return false;
            }
            let sc = self.sc.sc;
            let fence_fd = acquire_fence
                .map(std::os::fd::IntoRawFd::into_raw_fd)
                .unwrap_or(-1);
            (self.api.txn_set_buffer)(txn, sc, buffer.as_ptr(), fence_fd);
            let src = ARect {
                left: 0,
                top: 0,
                right: src_w.max(1),
                bottom: src_h.max(1),
            };
            let dst = ARect {
                left: 0,
                top: 0,
                right: self.dest_w,
                bottom: self.dest_h,
            };
            (self.api.txn_set_geometry)(txn, sc, &src, &dst, TRANSFORM_IDENTITY);
            if dataspace != 0 {
                if let Some(f) = self.api.txn_set_dataspace {
                    f(txn, sc, dataspace);
                }
            }
            if !self.configured {
                (self.api.txn_set_visibility)(txn, sc, VISIBILITY_SHOW);
                (self.api.txn_set_z_order)(txn, sc, 0);
                if frame_rate > 0.0 {
                    if let Some(f) = self.api.txn_set_frame_rate {
                        // compatibility 1 = FIXED_SOURCE (fixed-rate video the app can't re-pace).
                        f(txn, sc, frame_rate, 1);
                    }
                }
                self.configured = true;
            }
            (self.api.txn_set_present_time)(txn, desired_present_ns);
            // One-shot completion context, reclaimed inside the callback. The `Arc` clone keeps the
            // control alive for the callback even past the layer's own drop.
            let ctx = Box::into_raw(Box::new(CompleteCtx {
                tx: ev_tx.clone(),
                seq,
                sc: self.sc.clone(),
                prev_fence_fn: self.api.stats_prev_release_fence,
                latch_fn: self.api.stats_latch_time,
            }));
            (self.api.txn_set_on_complete)(txn, ctx as *mut c_void, on_complete);
            (self.api.txn_apply)(txn);
            (self.api.txn_delete)(txn);
        }
        true
    }
}
