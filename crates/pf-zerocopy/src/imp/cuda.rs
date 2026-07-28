//! CUDA driver-side state for the zero-copy path, layered over the raw driver-API FFI in `ffi`
//! (the `dlopen`'d `libcuda.so.1` symbol table — hand-rolled because no Rust crate exposes the
//! GL-interop calls, and runtime-loaded so one binary runs on NVIDIA *and* on AMD/Intel where
//! `libcuda` is absent). This facade owns the higher-level pieces on top of that layer:
//!
//! * one process-wide `CUcontext`, created lazily and shared by the EGL importer (capture thread)
//!   and ffmpeg's `hevc_nvenc` (encode thread) — each thread makes it current before use;
//! * device memory: pitched allocations, the reusable `BufferPool`/`DeviceBuffer`, IPC
//!   export/import, host readback, and the plane copies;
//! * GL / external-memory interop (`RegisteredTexture`, `ExternalDmabuf`).
//!
//! (The CUDA cursor-blend PTX kernel that used to live here is retired: vendored PTX is JIT'd
//! against the driver's ISA ceiling and silently dies on older drivers. The NVENC cursor blend
//! is now the SPIR-V compute pass in [`super::vkslot`], dispatched over Vulkan-allocated,
//! CUDA-imported input slots.)
//!
//! (We use GL interop, not EGL interop: `cuGraphicsEGLRegisterImage` is Tegra-only on the desktop
//! driver — see [`super::egl`].)

#![allow(non_camel_case_types, non_snake_case)]
// Every `unsafe` block/impl below carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{bail, Result};
use std::os::raw::{c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};

#[path = "cuda/ffi.rs"]
mod ffi;
// `pub` (not `pub(crate)`): the raw driver-API vocabulary (`CUdeviceptr`, …) is consumed across
// the crate boundary by the encode backends' CUDA-frame paths.
pub use ffi::*;

/// Copy a pitched device plane `(src_ptr, src_pitch)` down to a tightly-packed host buffer of
/// `width_bytes`×`height` (no row padding). Synchronous on the priority stream. Used by the NV12
/// self-test to read planes back for the colour comparison; not on the hot path.
pub fn read_plane_to_host(
    src_ptr: CUdeviceptr,
    src_pitch: usize,
    width_bytes: usize,
    height: usize,
) -> Result<Vec<u8>> {
    let mut host = vec![0u8; width_bytes * height];
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src_ptr,
        srcPitch: src_pitch,
        dstMemoryType: 1, // CU_MEMORYTYPE_HOST
        dstHost: host.as_mut_ptr() as *mut c_void,
        dstPitch: width_bytes,
        WidthInBytes: width_bytes,
        Height: height,
        ..Default::default()
    };
    // SAFETY: `copy_blocking` is unsafe because it issues a CUDA copy; its contract is a valid
    // descriptor with the shared context current (the caller's responsibility — self-test path).
    // `&copy` is a live local `#[repr(C)] CUDA_MEMCPY2D` that outlives the synchronous call:
    // `srcDevice`/`srcPitch` are the caller's live pitched device plane, `dstHost` addresses the
    // freshly-allocated `host` `Vec` of exactly `width_bytes*height` bytes, and `WidthInBytes`×
    // `Height` fit both. The copy is synchronous, so `host` is fully written before we return it.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(dev->host)")? };
    Ok(host)
}

/// Export a device allocation (from `cuMemAllocPitch`/`cuMemAlloc`) as a cross-process CUDA IPC
/// handle — an opaque 64-byte blob another process opens with [`ipc_open`]. The allocation must
/// stay alive for as long as any importer has it open. The shared context must be current.
pub fn ipc_export(ptr: CUdeviceptr) -> Result<[u8; CU_IPC_HANDLE_SIZE]> {
    let mut handle = CUipcMemHandle {
        reserved: [0; CU_IPC_HANDLE_SIZE],
    };
    // SAFETY: `&mut handle` is a live, correctly-sized stack out-param the driver fills with the
    // opaque IPC blob; `ptr` is the caller's live device allocation (by-value integer). The call is
    // synchronous and retains no pointer into Rust memory. Wrapper → live table (context current).
    unsafe { ck(cuIpcGetMemHandle(&mut handle, ptr), "cuIpcGetMemHandle")? };
    Ok(handle.reserved)
}

/// Open an IPC handle exported by *another* process ([`ipc_export`]); returns a device pointer
/// valid in this process until [`ipc_close`]. The shared context must be current.
pub fn ipc_open(handle: &[u8; CU_IPC_HANDLE_SIZE]) -> Result<CUdeviceptr> {
    let h = CUipcMemHandle { reserved: *handle };
    let mut ptr: CUdeviceptr = 0;
    // SAFETY: `h` is passed by value (matching the C `CUipcMemHandle` struct ABI); `&mut ptr` is a
    // live zero-init stack out-param the driver writes the mapped device address into. Synchronous
    // call, distinct locals, no aliasing. Wrapper → live table (context current).
    unsafe {
        ck(
            cuIpcOpenMemHandle(&mut ptr, h, CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS),
            "cuIpcOpenMemHandle",
        )?
    };
    Ok(ptr)
}

/// Close a mapping opened with [`ipc_open`] (best-effort teardown; makes the shared context
/// current itself since drops may run off-thread).
pub fn ipc_close(ptr: CUdeviceptr) {
    if ptr == 0 {
        return;
    }
    // SAFETY: `ptr` is a device pointer previously returned by `cuIpcOpenMemHandle` (the only
    // caller path), closed exactly once by the owning cache. We make the shared context current
    // first because this runs from `Drop` on whatever thread holds the last reference. Result
    // ignored (best-effort teardown). Wrapper → live table (the mapping exists ⇒ driver present).
    unsafe {
        if let Some(c) = CONTEXT.get() {
            let _ = cuCtxSetCurrent(c.0);
        }
        let _ = cuIpcCloseMemHandle(ptr);
    }
}

/// The shared process-wide CUDA context (created once). Wrapped so it's `Send`/`Sync` to live
/// in a `OnceLock`; the raw `CUcontext` is thread-safe to make current from any thread.
#[derive(Clone, Copy)]
pub struct Context(pub CUcontext);
// SAFETY: `CUcontext` is an opaque CUDA driver handle, not a dereferenceable Rust pointer. It is
// created once and never destroyed (process lifetime), and the only thing done with it is
// `cuCtxSetCurrent`, which the Driver API explicitly allows from any thread — so transferring the
// handle to another thread cannot dangle or race (the driver owns the synchronization).
unsafe impl Send for Context {}
// SAFETY: as above — the wrapped handle is an immutable opaque address and the driver does all the
// synchronization, so sharing `&Context` across threads is sound.
unsafe impl Sync for Context {}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// Get (lazily creating) the shared CUDA context on device 0.
pub fn context() -> Result<CUcontext> {
    if let Some(c) = CONTEXT.get() {
        return Ok(c.0);
    }
    if cuda_api().is_none() {
        bail!("libcuda.so.1 not available — no NVIDIA driver (CUDA zero-copy disabled)");
    }
    // SAFETY: we returned above unless `cuda_api()` is `Some`, so every wrapper here forwards into
    // the live, leaked `libcuda` table rather than the not-loaded stub. `cuInit(0)` passes the
    // API-required flags value 0. `&mut dev`/`&mut ctx` are live, zero/null-initialized stack
    // out-params the driver writes the device handle / new context into; each outlives its
    // synchronous call and they are distinct locals (no aliasing). `cuCtxCreate_v2` yields a valid
    // `CUcontext` on success (`ck` bails otherwise), which becomes the block's value.
    let ctx = unsafe {
        ck(cuInit(0), "cuInit")?;
        let mut dev: CUdevice = 0;
        ck(cuDeviceGet(&mut dev, 0), "cuDeviceGet")?;
        let mut ctx: CUcontext = std::ptr::null_mut();
        ck(
            cuCtxCreate_v2(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev),
            "cuCtxCreate_v2",
        )?;
        ctx
    };
    // Racy first-init is fine: the winner's context is used; a loser leaks one context (rare,
    // process-lifetime). `get_or_init` keeps a single shared value.
    Ok(CONTEXT.get_or_init(|| Context(ctx)).0)
}

/// Make the shared context current on the calling thread (required before any CUDA op here).
pub fn make_current() -> Result<()> {
    let ctx = context()?;
    // SAFETY: `ctx` came from `context()?`, so it is the live shared `CUcontext` and the driver
    // table is present. `cuCtxSetCurrent` binds that opaque handle to the calling thread; it takes
    // no Rust-memory pointer and is thread-safe (affects only this thread's current context), so
    // there is no aliasing or lifetime hazard.
    unsafe { ck(cuCtxSetCurrent(ctx), "cuCtxSetCurrent") }
}

/// DIAGNOSTIC-ONLY: create a fresh dedicated context on device 0, run `probe` with it, destroy it,
/// and restore the shared context as current. Used by the NVENC backend's session-open
/// self-diagnosis to split "this process's shared context is in a bad state" (probe succeeds on
/// the fresh context) from a driver-level condition like version skew or session exhaustion
/// (probe fails there too). Never used on a hot path.
pub fn with_fresh_context<R>(probe: impl FnOnce(CUcontext) -> R) -> Result<R> {
    if cuda_api().is_none() {
        bail!("libcuda.so.1 not available");
    }
    // SAFETY: the driver table is present (checked above). `cuInit(0)` is idempotent. `&mut dev`/
    // `&mut ctx` are live, distinct stack out-params for their synchronous calls. On success `ctx`
    // is a valid dedicated context, destroyed exactly once below; the shared context is restored
    // as current afterwards (creation left the fresh one current on this thread).
    unsafe {
        ck(cuInit(0), "cuInit")?;
        let mut dev: CUdevice = 0;
        ck(cuDeviceGet(&mut dev, 0), "cuDeviceGet")?;
        let mut ctx: CUcontext = std::ptr::null_mut();
        ck(
            cuCtxCreate_v2(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, dev),
            "cuCtxCreate_v2 (diagnostic)",
        )?;
        let r = probe(ctx);
        let _ = cuCtxDestroy_v2(ctx);
        if let Some(c) = CONTEXT.get() {
            let _ = cuCtxSetCurrent(c.0);
        }
        Ok(r)
    }
}

thread_local! {
    /// Per-thread copy stream. `None` until first use; `Some(null)` means "creation failed, use the
    /// default (NULL) stream". Per-thread (not shared) so each worker's `cuStreamSynchronize` waits
    /// only on ITS OWN copies — the old per-frame `cuCtxSynchronize` was context-wide and also
    /// blocked on the other worker thread's in-flight NULL-stream copies.
    static COPY_STREAM: std::cell::Cell<Option<CUstream>> = const { std::cell::Cell::new(None) };
}

/// The calling thread's highest-priority copy stream (lazily created; context must be current).
/// Carries the greatest stream priority the driver exposes — a scheduler hint that nudges our
/// copies ahead of the game's queued compute. NOTE: stream priority is an intra-process hint and
/// NVIDIA's Linux driver may ignore it / not preempt a saturating game's graphics context; this is
/// "measure-then-keep", and it never regresses (falls back to the NULL stream). The greatest
/// priority is the numerically-lowest value (`greatest` from `cuCtxGetStreamPriorityRange`).
fn copy_stream() -> CUstream {
    COPY_STREAM.with(|cell| {
        if let Some(s) = cell.get() {
            return s;
        }
        // SAFETY: `copy_stream` runs with the shared context current (its doc contract), so the
        // wrappers forward into the live `libcuda` table. `&mut least`/`&mut greatest` are live
        // stack `i32`s the driver fills with the priority range; `&mut s` is a live null-init
        // `CUstream` the driver writes the new stream into. All out-params outlive their
        // synchronous calls and are distinct locals. On any non-zero result we fall back to a null
        // (NULL-stream) value and never read an uninitialized handle.
        let stream = unsafe {
            let (mut least, mut greatest) = (0i32, 0i32);
            if cuCtxGetStreamPriorityRange(&mut least, &mut greatest) != 0 {
                std::ptr::null_mut()
            } else {
                let mut s: CUstream = std::ptr::null_mut();
                if cuStreamCreateWithPriority(&mut s, CU_STREAM_NON_BLOCKING, greatest) != 0 {
                    std::ptr::null_mut()
                } else {
                    tracing::debug!(
                        priority = greatest,
                        "CUDA high-priority copy stream created"
                    );
                    s
                }
            }
        };
        cell.set(Some(stream));
        stream
    })
}

/// Issue `copy` on this thread's priority stream and block until it completes. Replaces the
/// per-frame `cuMemcpy2D_v2` + context-wide `cuCtxSynchronize` pair: same completion guarantee
/// (the source dmabuf is safe to recycle once this returns), but the wait is scoped to our own
/// stream and the copy carries the high priority hint.
unsafe fn copy_blocking(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller contract: the shared context is current and `copy` describes live, in-bounds
    // device/host memory (each caller carries that proof). Wrapper -> live table; `&copy` outlives
    // the synchronous call.
    unsafe {
        let stream = copy_stream();
        ck(cuMemcpy2DAsync_v2(copy, stream), what)?;
        ck(cuStreamSynchronize(stream), "cuStreamSynchronize")
    }
}

/// Issue `copy` on this thread's priority stream WITHOUT waiting — for stream-ordered consumers
/// only (the direct-NVENC submit path with `NvEncSetIOCudaStreams` bound to this stream): the
/// stream, not the CPU, orders completion, so the SOURCE must stay valid until the downstream
/// stream work (the encode) has finished.
unsafe fn copy_async(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller contract: the shared context is current and `copy` describes live, in-bounds
    // device/host memory that stays valid until the stream work completes (each caller carries that
    // proof). Wrapper -> live table.
    unsafe { ck(cuMemcpy2DAsync_v2(copy, copy_stream()), what) }
}

/// Block until everything enqueued on THIS THREAD's copy stream completed — the shared tail of
/// the multi-plane blocking copies (stream FIFO: one sync after the last enqueue covers every
/// plane, where the per-plane `copy_blocking` paid one exposed CPU wait EACH — and under a
/// game's GPU load each exposed wait eats scheduling latency). The shared context must be
/// current.
unsafe fn sync_copy_stream() -> Result<()> {
    // SAFETY: caller contract: the shared context is current. Wrapper -> live table; synchronizing
    // the dedicated copy stream touches no Rust memory.
    unsafe { ck(cuStreamSynchronize(copy_stream()), "cuStreamSynchronize") }
}

/// `copy_blocking` when `sync`, else `copy_async` — the shared tail of the public `copy_*_to_device`
/// helpers, whose `sync: false` mode carries `copy_async`'s source-lifetime contract.
unsafe fn copy_issue(copy: &CUDA_MEMCPY2D, what: &str, sync: bool) -> Result<()> {
    // SAFETY: caller contract: the shared context is current and `copy` describes live, in-bounds
    // memory (each caller carries that proof). The stream handle is the once-created per-process
    // copy stream. Wrapper -> live table.
    unsafe {
        if sync {
            copy_blocking(copy, what)
        } else {
            copy_async(copy, what)
        }
    }
}

/// The calling thread's copy/launch stream as a raw handle, for binding external stream-ordering
/// (the direct-NVENC `NvEncSetIOCudaStreams` hookup). Null = the NULL stream (priority-stream
/// creation failed) — callers should treat null as "stream-ordering unavailable" and keep their
/// blocking copies. The shared context must be current on this thread.
pub fn copy_stream_handle() -> *mut c_void {
    copy_stream() // CUstream IS *mut c_void (opaque CUstream_st*)
}

/// Max cursor-overlay bitmap edge (px) uploaded to the device blend buffer — matches the Vulkan path.
pub const CURSOR_MAX: u32 = 256;

/// Allocate one pitched device buffer for `width`x`height` 4-byte pixels; returns `(ptr, pitch)`.
fn alloc_pitched(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: `cuMemAllocPitch_v2` allocates a pitched device buffer (the wrapper forwards to the
    // live table on any path that reached allocation). `&mut ptr` (`CUdeviceptr`) and `&mut pitch`
    // (`usize`) are live, distinct stack out-params the driver writes the allocation pointer and
    // its pitch into; both outlive the synchronous call. Width/height/element-size are by-value
    // ints. No aliasing — two separate locals.
    unsafe {
        ck(
            cuMemAllocPitch_v2(
                &mut ptr,
                &mut pitch,
                width as usize * 4,
                height as usize,
                16,
            ),
            "cuMemAllocPitch_v2",
        )?;
    }
    Ok((ptr, pitch))
}

/// Allocate ONE pitched buffer holding the three stacked full-res 1-byte planes of a planar
/// YUV444 surface: `3·height` rows of `width` bytes at the driver's pitch, rows `[0, H)` = Y,
/// `[H, 2H)` = U, `[2H, 3H)` = V. A single allocation keeps the buffer single-plane on the
/// worker↔host wire (one IPC handle), like the RGB path.
fn alloc_pitched_yuv444(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: `cuMemAllocPitch_v2` allocates a pitched device buffer (wrapper → live table).
    // `&mut ptr`/`&mut pitch` are live, distinct stack out-params outliving the synchronous call;
    // width/height/element-size are by-value ints. No aliasing.
    unsafe {
        ck(
            cuMemAllocPitch_v2(
                &mut ptr,
                &mut pitch,
                width as usize,      // 1 byte/px per plane
                height as usize * 3, // Y + U + V stacked
                16,
            ),
            "cuMemAllocPitch_v2(YUV444)",
        )?;
    }
    Ok((ptr, pitch))
}

/// Allocate the two pitched planes of an NV12 surface (8-bit BT.709 4:2:0): a `width`-byte Y plane
/// (W×H, 1 byte/px) and an interleaved chroma plane (W/2 × H/2 samples, 2 bytes/sample → W bytes
/// wide). Both planes share the driver's Y pitch (the wider request), so the encoder's two-plane
/// surface and ours line up. Returns `((y_ptr, y_pitch), (uv_ptr, uv_pitch))`.
fn alloc_pitched_nv12(
    width: u32,
    height: u32,
) -> Result<((CUdeviceptr, usize), (CUdeviceptr, usize))> {
    let mut y_ptr: CUdeviceptr = 0;
    let mut y_pitch: usize = 0;
    let mut uv_ptr: CUdeviceptr = 0;
    let mut uv_pitch: usize = 0;
    // SAFETY: two independent `cuMemAllocPitch_v2` calls (wrapper → live table). `&mut y_ptr`/
    // `&mut y_pitch` and `&mut uv_ptr`/`&mut uv_pitch` are live, distinct stack out-params the
    // driver writes each plane's pointer and pitch into; all outlive their synchronous calls. The
    // dimension/element-size args are by-value ints. No aliasing — four separate locals. If the UV
    // allocation fails, the just-created Y allocation is freed before the error propagates — this
    // runs per frame under VRAM pressure (`BufferPool::get`'s pool-miss fallback), so a leak here
    // would compound exactly when memory is already scarce.
    unsafe {
        ck(
            cuMemAllocPitch_v2(
                &mut y_ptr,
                &mut y_pitch,
                width as usize,
                height as usize,
                16,
            ),
            "cuMemAllocPitch_v2(Y)",
        )?;
        // Chroma is W/2 samples wide at 2 bytes each = W bytes; H/2 rows.
        if let Err(e) = ck(
            cuMemAllocPitch_v2(
                &mut uv_ptr,
                &mut uv_pitch,
                (width as usize / 2) * 2,
                (height as usize / 2).max(1),
                16,
            ),
            "cuMemAllocPitch_v2(UV)",
        ) {
            let _ = cuMemFree_v2(y_ptr);
            return Err(e);
        }
    }
    Ok(((y_ptr, y_pitch), (uv_ptr, uv_pitch)))
}

/// Allocate ONE pitched buffer holding a *contiguous* NV12 surface — Y rows `[0, H)` immediately
/// followed by interleaved-chroma rows `[H, 3H/2)`, all at the driver's single pitch. Unlike
/// [`alloc_pitched_nv12`] (two separate allocations, the capture/IPC layout) this is the layout the
/// direct-SDK NVENC encoder registers as a single `CUDADEVICEPTR` input: NVENC reads the UV plane
/// at `ptr + pitch*height`. Used only by [`InputSurface`] (encode side), never the wire.
fn alloc_pitched_nv12_contiguous(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // Y is `width` bytes/row × H rows; the interleaved chroma plane is W/2 samples × 2 bytes =
    // `width` bytes/row × H/2 rows. One allocation of `H + H/2` rows keeps them contiguous under a
    // single pitch so NVENC finds UV at `ptr + pitch*H`.
    let rows = height as usize + (height as usize / 2).max(1);
    // SAFETY: `cuMemAllocPitch_v2` (wrapper → live table) writes the allocation pointer and pitch
    // into the two live, distinct stack out-params `&mut ptr`/`&mut pitch`, which outlive the
    // synchronous call; width/rows/element-size are by-value ints. No aliasing.
    unsafe {
        ck(
            cuMemAllocPitch_v2(&mut ptr, &mut pitch, width as usize, rows, 16),
            "cuMemAllocPitch_v2(NV12 contiguous)",
        )?;
    }
    Ok((ptr, pitch))
}

/// An encoder-owned, contiguous pitched CUDA surface that the direct-SDK NVENC Linux backend
/// (`encode/linux/nvenc_cuda.rs`, design/linux-direct-nvenc.md) registers **once** as a
/// `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR` input and copies each captured frame into (via the
/// `copy_*_to_device` helpers) before `encode_picture`. Distinct from [`DeviceBuffer`]: these are
/// laid out exactly as NVENC's single-pointer register expects — NV12 = Y then interleaved-UV under
/// one pitch, YUV444 = Y|U|V stacked, RGB = packed 4-byte — and are never pooled or sent on the
/// wire. Frees its allocation on drop (context made current first, since drop may run off-thread).
pub struct InputSurface {
    /// Base device pointer NVENC registers. For NV12 the chroma plane lives at `ptr + pitch*height`;
    /// for YUV444 the U/V planes at `ptr + pitch*height` / `ptr + 2*pitch*height`.
    pub ptr: CUdeviceptr,
    /// Row stride in bytes (the driver's pitch), shared by every plane of the surface.
    pub pitch: usize,
    /// Luma height in rows — the plane stride multiplier NVENC / the copy helpers key off.
    pub height: u32,
}

impl InputSurface {
    /// Contiguous NV12 (8-bit 4:2:0): one allocation, Y then interleaved UV under one pitch.
    pub fn alloc_nv12(width: u32, height: u32) -> Result<InputSurface> {
        let (ptr, pitch) = alloc_pitched_nv12_contiguous(width, height)?;
        Ok(InputSurface { ptr, pitch, height })
    }

    /// Planar YUV444 (8-bit 4:4:4): one allocation, Y|U|V full-res planes stacked (see
    /// `alloc_pitched_yuv444`).
    pub fn alloc_yuv444(width: u32, height: u32) -> Result<InputSurface> {
        let (ptr, pitch) = alloc_pitched_yuv444(width, height)?;
        Ok(InputSurface { ptr, pitch, height })
    }

    /// Packed 4-byte RGB/BGRx: one contiguous pitched allocation (NVENC does the internal CSC when
    /// registered as an `ABGR`/`ARGB` input).
    pub fn alloc_rgb(width: u32, height: u32) -> Result<InputSurface> {
        let (ptr, pitch) = alloc_pitched(width, height)?;
        Ok(InputSurface { ptr, pitch, height })
    }
}

impl Drop for InputSurface {
    fn drop(&mut self) {
        if self.ptr == 0 {
            return;
        }
        // SAFETY: this surface exclusively owns `self.ptr` (a single `cuMemAllocPitch_v2` allocation
        // from one of the constructors above), freed exactly once here — `drop` runs once and the
        // `ptr == 0` guard skips a moved-out/empty surface, so no double-free. The shared context is
        // made current first because drop may run on a thread where it isn't, and `cuMemFree_v2`
        // needs it. Wrapper → live table; result ignored (best-effort teardown).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            let _ = cuMemFree_v2(self.ptr);
        }
    }
}

/// Free-list of recycled device allocations for one resolution. Shared (via `Arc`) between the
/// capture thread that hands out buffers and the encode thread where a [`DeviceBuffer`] drops and
/// returns its allocation here. Bulk-freed when the last reference drops. For NV12 each free entry
/// is the Y plane *and* its paired UV plane (allocated/recycled/freed together).
struct PoolInner {
    free: Vec<CUdeviceptr>,
    /// NV12 only: the UV plane paired with each Y plane in `free` (same index, same length).
    free_uv: Vec<CUdeviceptr>,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        // SAFETY: the pool only exists because allocation succeeded, so the driver table is live.
        // `PoolInner` drops only once every `DeviceBuffer` that referenced it (each holds an `Arc`
        // clone) has been recycled, so `free`/`free_uv` hold every outstanding allocation exactly
        // once and nothing else still uses them — no double-free or use-after-free. We make the
        // shared context current first (drop may run off the allocating thread) so `cuMemFree_v2`
        // targets the right context. Each `p` is a `CUdeviceptr` previously returned by
        // `cuMemAllocPitch_v2`; results are ignored (best-effort teardown).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            for &p in &self.free {
                let _ = cuMemFree_v2(p);
            }
            for &p in &self.free_uv {
                let _ = cuMemFree_v2(p);
            }
        }
    }
}

/// A pool of reusable pitched device buffers for a fixed resolution. Eliminates the per-frame
/// `cuMemAllocPitch`/`cuMemFree` (a ~29 MB allocation at 5K) that takes the device allocator lock
/// and serializes against the GPU every frame.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Mutex<PoolInner>>,
    width: u32,
    height: u32,
    pitch: usize,
    /// NV12 pools carry a second (chroma) pitch; `Some` ⇒ buffers from this pool have a UV plane.
    uv_pitch: Option<usize>,
    /// YUV444 pools: one allocation of 3·`height` stacked 1-byte planes (see `alloc_pitched_yuv444`).
    yuv444: bool,
}

impl BufferPool {
    /// Create a pool for `width`x`height` 4-byte buffers (allocates one up front to learn the
    /// driver's pitch, which is constant for a given width).
    pub fn new(width: u32, height: u32) -> Result<BufferPool> {
        let (ptr, pitch) = alloc_pitched(width, height)?;
        Ok(BufferPool {
            inner: Arc::new(Mutex::new(PoolInner {
                free: vec![ptr],
                free_uv: Vec::new(),
            })),
            width,
            height,
            pitch,
            uv_pitch: None,
            yuv444: false,
        })
    }

    /// Create a pool of NV12 two-plane surfaces (Y + interleaved UV) for `width`x`height`. Allocates
    /// one pair up front to learn the driver's per-plane pitches (constant for a given width).
    pub fn new_nv12(width: u32, height: u32) -> Result<BufferPool> {
        let ((y_ptr, y_pitch), (uv_ptr, uv_pitch)) = alloc_pitched_nv12(width, height)?;
        Ok(BufferPool {
            inner: Arc::new(Mutex::new(PoolInner {
                free: vec![y_ptr],
                free_uv: vec![uv_ptr],
            })),
            width,
            height,
            pitch: y_pitch,
            uv_pitch: Some(uv_pitch),
            yuv444: false,
        })
    }

    /// Create a pool of planar YUV444 surfaces: ONE pitched allocation per buffer holding the
    /// three full-res 1-byte planes stacked as rows `[Y | U | V]` (3·height rows at the same
    /// pitch), so the two-plane wire protocol and IPC path carry it like a single-plane buffer.
    pub fn new_yuv444(width: u32, height: u32) -> Result<BufferPool> {
        let (ptr, pitch) = alloc_pitched_yuv444(width, height)?;
        Ok(BufferPool {
            inner: Arc::new(Mutex::new(PoolInner {
                free: vec![ptr],
                free_uv: Vec::new(),
            })),
            width,
            height,
            pitch,
            uv_pitch: None,
            yuv444: true,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Take a buffer — recycled if one is free, else freshly allocated. The buffer returns to this
    /// pool when dropped (after the consumer has synchronized, so the GPU is done with it). For an
    /// NV12 pool the returned buffer carries both the Y and the paired UV plane.
    pub fn get(&self) -> Result<DeviceBuffer> {
        if let Some(uv_pitch) = self.uv_pitch {
            let reuse = {
                let mut g = self.inner.lock().unwrap();
                g.free.pop().map(|y| (y, g.free_uv.pop()))
            };
            let (ptr, uv_ptr) = match reuse {
                // Y and UV are pushed/popped together, so a popped Y always has its UV.
                Some((y, Some(uv))) => (y, uv),
                _ => {
                    let ((y, _), (uv, _)) = alloc_pitched_nv12(self.width, self.height)?;
                    (y, uv)
                }
            };
            return Ok(DeviceBuffer {
                ptr,
                pitch: self.pitch,
                width: self.width,
                height: self.height,
                uv: Some((uv_ptr, uv_pitch)),
                yuv444: false,
                pool: Some(self.inner.clone()),
                remote_release: None,
            });
        }
        let reuse = self.inner.lock().unwrap().free.pop();
        let ptr = match reuse {
            Some(p) => p,
            None if self.yuv444 => alloc_pitched_yuv444(self.width, self.height)?.0,
            None => alloc_pitched(self.width, self.height)?.0,
        };
        Ok(DeviceBuffer {
            ptr,
            pitch: self.pitch,
            width: self.width,
            height: self.height,
            uv: None,
            yuv444: self.yuv444,
            pool: Some(self.inner.clone()),
            remote_release: None,
        })
    }
}

/// A pitched device buffer holding one captured frame. Filled by a copy from the EGL-mapped
/// dmabuf (so the dmabuf can be returned to the compositor immediately) and read by the encoder.
/// When it came from a [`BufferPool`] it recycles on drop; otherwise it frees.
pub struct DeviceBuffer {
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    /// NV12 only: the interleaved chroma plane `(ptr, pitch)` paired with the Y plane in [`ptr`](Self::ptr).
    /// `None` for the default 4-byte RGB/BGRx path. When `Some`, [`ptr`](Self::ptr) is the Y plane (1 byte/px).
    pub uv: Option<(CUdeviceptr, usize)>,
    /// Planar YUV444: [`ptr`](Self::ptr) is ONE allocation of 3·[`height`](Self::height) rows at
    /// [`pitch`](Self::pitch) — the full-res 1-byte Y, U, V planes stacked in that order
    /// (`uv` stays `None`; the single-plane wire/IPC path carries it unchanged).
    pub yuv444: bool,
    pool: Option<Arc<Mutex<PoolInner>>>,
    /// Set for buffers whose device memory is owned by ANOTHER process (the zero-copy import
    /// worker, reached via CUDA IPC): drop runs this exactly once (telling the owner to recycle)
    /// and must neither free nor pool-recycle the pointers locally.
    remote_release: Option<Box<dyn FnOnce() + Send>>,
}

impl DeviceBuffer {
    /// Allocate a standalone (un-pooled) pitched buffer. Prefer [`BufferPool`] on the hot path.
    pub fn alloc(width: u32, height: u32) -> Result<DeviceBuffer> {
        let (ptr, pitch) = alloc_pitched(width, height)?;
        Ok(DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
            uv: None,
            yuv444: false,
            pool: None,
            remote_release: None,
        })
    }

    /// Allocate a standalone (un-pooled) NV12 two-plane buffer. Prefer [`BufferPool::new_nv12`] on
    /// the hot path; used by the self-test.
    pub fn alloc_nv12(width: u32, height: u32) -> Result<DeviceBuffer> {
        let ((y_ptr, y_pitch), (uv_ptr, uv_pitch)) = alloc_pitched_nv12(width, height)?;
        Ok(DeviceBuffer {
            ptr: y_ptr,
            pitch: y_pitch,
            width,
            height,
            uv: Some((uv_ptr, uv_pitch)),
            yuv444: false,
            pool: None,
            remote_release: None,
        })
    }

    /// Allocate a standalone (un-pooled) planar-YUV444 stacked buffer. Prefer
    /// [`BufferPool::new_yuv444`] on the hot path; used by the self-test.
    pub fn alloc_yuv444(width: u32, height: u32) -> Result<DeviceBuffer> {
        let (ptr, pitch) = alloc_pitched_yuv444(width, height)?;
        Ok(DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
            uv: None,
            yuv444: true,
            pool: None,
            remote_release: None,
        })
    }

    /// True if this buffer carries an NV12 chroma plane.
    pub fn is_nv12(&self) -> bool {
        self.uv.is_some()
    }

    /// Wrap device planes owned by ANOTHER process (opened here via [`ipc_open`]) as a frame
    /// buffer. `release` runs exactly once on drop — it tells the owning process to recycle the
    /// buffer; nothing is freed or pooled locally (the IPC mapping itself is closed by the cache
    /// that opened it, after the last remote buffer referencing it has dropped).
    /// `yuv444` marks a stacked 3-plane YUV444 allocation (the wire carries no format — the host
    /// knows what it REQUESTED, `ImportKind::Tiled444`).
    pub fn remote(
        ptr: CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        uv: Option<(CUdeviceptr, usize)>,
        yuv444: bool,
        release: Box<dyn FnOnce() + Send>,
    ) -> DeviceBuffer {
        DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
            uv,
            yuv444,
            pool: None,
            remote_release: Some(release),
        }
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if let Some(release) = self.remote_release.take() {
            // Remote (IPC) buffer: the worker owns the memory — just hand it back.
            release();
            return;
        }
        if self.ptr == 0 {
            return;
        }
        if let Some(pool) = &self.pool {
            // Recycle (the consumer synchronized before dropping, so the GPU is done with it). Y and
            // its paired UV go back together so `get` can repair them as a unit.
            let mut g = pool.lock().unwrap();
            g.free.push(self.ptr);
            if let Some((uv_ptr, _)) = self.uv {
                g.free_uv.push(uv_ptr);
            }
        } else {
            // The buffer may be freed on the encode thread; cuMemFree needs a current context.
            // SAFETY: this is the un-pooled branch (`pool` is `None`), so this `DeviceBuffer`
            // exclusively owns `self.ptr` (and `self.uv`'s `uv_ptr`), each returned by
            // `cuMemAllocPitch_v2` and freed exactly once here — `drop` runs once and the
            // `self.ptr == 0` guard above skips the sentinel/empty case, so no double-free. We set
            // the shared context current first because drop may run on a thread where it isn't, and
            // `cuMemFree_v2` needs it. Wrapper → live table; results ignored (teardown).
            unsafe {
                if let Some(c) = CONTEXT.get() {
                    let _ = cuCtxSetCurrent(c.0);
                }
                let _ = cuMemFree_v2(self.ptr);
                if let Some((uv_ptr, _)) = self.uv {
                    let _ = cuMemFree_v2(uv_ptr);
                }
            }
        }
    }
}

/// A *persistent* GL-texture→CUDA registration. The desktop NVIDIA driver only supports CUDA
/// interop through GL textures (not dmabuf EGLImages directly), so the importer renders the
/// dmabuf into a reusable `GL_RGBA8` texture and registers *that* once — then each frame only
/// maps → copies the mapped array out → unmaps (the map/unmap pair is the GL↔CUDA sync point),
/// instead of registering/unregistering every frame. Unregisters on drop.
pub struct RegisteredTexture {
    resource: CUgraphicsResource,
}

impl RegisteredTexture {
    /// Register a `GL_TEXTURE_2D` once.
    ///
    /// # Safety
    /// The GL context and the shared CUDA context must both be current on this thread, and
    /// `texture` must be a valid `GL_TEXTURE_2D`.
    pub unsafe fn register_gl(texture: u32) -> Result<RegisteredTexture> {
        // SAFETY: caller contract: the GL context owning `texture` and the shared CUDA context are
        // current on this thread, and `texture` is a live, complete GL texture. The out-param is a
        // live stack local; wrapper -> live table.
        unsafe {
            const GL_TEXTURE_2D: c_uint = 0x0DE1;
            const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: c_uint = 0x01;
            let mut resource: CUgraphicsResource = std::ptr::null_mut();
            ck(
                cuGraphicsGLRegisterImage(
                    &mut resource,
                    texture,
                    GL_TEXTURE_2D,
                    CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
                ),
                "cuGraphicsGLRegisterImage",
            )?;
            Ok(RegisteredTexture { resource })
        }
    }

    /// Map the texture for this frame, copy its (already-linear RGBA8) array into `dst`, then
    /// unmap. The copy is synchronized (on our priority stream) before unmap so `dst` is ready
    /// before the source dmabuf is recycled. Always unmaps, even if the copy errors.
    pub fn copy_mapped_to(&mut self, dst: &DeviceBuffer) -> Result<()> {
        // SAFETY: `self.resource` is the valid `CUgraphicsResource` from a successful `register_gl`
        // (its only constructor), so the wrappers forward to the live table; the caller holds the
        // GL+CUDA contexts current (the registration's contract). `cuGraphicsMapResources` maps
        // `count == 1` resource via `&mut self.resource` (a live field). It is issued on
        // `copy_stream()` — NOT the NULL stream — because map's only ordering guarantee is that
        // prior GL work completes before subsequent CUDA work issued IN THE STREAM PASSED TO IT;
        // the copy below runs on `copy_stream()` (a `CU_STREAM_NON_BLOCKING` stream, exempt from
        // implicit NULL-stream ordering), so mapping on NULL left the copy free to race the GL
        // de-tile/CSC that produced this texture (glFlush only, no fence) — intermittent torn or
        // stale frames under GPU load. Map, copy, and unmap now all share `copy_stream()`.
        // `cuGraphicsSubResourceGetMappedArray` writes the mapped `CUarray` into the live local
        // `array` (index 0, mip 0). On failure we unmap and bail (balanced). `&copy` is a live
        // local `CUDA_MEMCPY2D` outliving the synchronous `copy_blocking`: `srcArray` is valid
        // while mapped, `dstDevice`/`dstPitch` are `dst`'s live allocation, `width*4`×`height` fit
        // both. `copy_blocking` syncs before we unmap, so the array stays valid through the copy;
        // we always unmap afterward (even on error), keeping the map/unmap pair balanced.
        unsafe {
            ck(
                cuGraphicsMapResources(1, &mut self.resource, copy_stream()),
                "cuGraphicsMapResources",
            )?;
            let mut array: CUarray = std::ptr::null_mut();
            if cuGraphicsSubResourceGetMappedArray(&mut array, self.resource, 0, 0) != 0 {
                let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
                bail!("cuGraphicsSubResourceGetMappedArray failed");
            }
            let copy = CUDA_MEMCPY2D {
                srcMemoryType: CU_MEMORYTYPE_ARRAY,
                srcArray: array,
                dstMemoryType: CU_MEMORYTYPE_DEVICE,
                dstDevice: dst.ptr,
                dstPitch: dst.pitch,
                WidthInBytes: dst.width as usize * 4, // 4 bytes/px (BGRx)
                Height: dst.height as usize,
                ..Default::default()
            };
            let res = copy_blocking(&copy, "cuMemcpy2DAsync_v2");
            let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
            res
        }
    }

    /// Map this texture for the frame and copy its array into the device plane `(dst_ptr,
    /// dst_pitch)`, taking `width_bytes`×`height` bytes (the GL internal format dictates
    /// `width_bytes`: `width*1` for an `R8` luma target, `(width/2)*2` for an `RG8` chroma target).
    /// Synchronized on our priority stream before unmap (so the source dmabuf is safe to recycle).
    /// Always unmaps, even on copy error.
    fn copy_mapped_plane(
        &mut self,
        dst_ptr: CUdeviceptr,
        dst_pitch: usize,
        width_bytes: usize,
        height: usize,
    ) -> Result<()> {
        // SAFETY: identical contract to `copy_mapped_to` — `self.resource` is the valid
        // `CUgraphicsResource` from `register_gl` (wrappers → live table; caller holds GL+CUDA
        // contexts current). Map `count == 1` resource via the live `&mut self.resource`; the
        // mapped `CUarray` is written into the live local `array` (index 0, mip 0); on failure we
        // unmap and bail (balanced). `&copy` is a live local outliving the synchronous
        // `copy_blocking`: `srcArray` valid while mapped, `dstDevice`/`dstPitch` are the caller's
        // live plane, `width_bytes`×`height` fit it. We always unmap afterward, even on copy error,
        // so the map/unmap pair stays balanced and the array outlives the copy.
        unsafe {
            ck(
                cuGraphicsMapResources(1, &mut self.resource, copy_stream()),
                "cuGraphicsMapResources",
            )?;
            let mut array: CUarray = std::ptr::null_mut();
            if cuGraphicsSubResourceGetMappedArray(&mut array, self.resource, 0, 0) != 0 {
                let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
                bail!("cuGraphicsSubResourceGetMappedArray failed");
            }
            let copy = CUDA_MEMCPY2D {
                srcMemoryType: CU_MEMORYTYPE_ARRAY,
                srcArray: array,
                dstMemoryType: CU_MEMORYTYPE_DEVICE,
                dstDevice: dst_ptr,
                dstPitch: dst_pitch,
                WidthInBytes: width_bytes,
                Height: height,
                ..Default::default()
            };
            let res = copy_blocking(&copy, "cuMemcpy2DAsync_v2(plane)");
            let _ = cuGraphicsUnmapResources(1, &mut self.resource, copy_stream());
            res
        }
    }
}

/// Copy the two NV12 convert targets (registered `R8` luma + `RG8` chroma GL textures) into `dst`'s
/// Y and UV planes. `dst` must be an NV12 buffer (`dst.uv` set). The luma plane is `width`×`height`
/// bytes; the chroma plane is `(width/2)·2` bytes wide × `height/2` rows. Both copies sync on our
/// priority stream before returning, so the dmabuf is safe to recycle once this returns.
pub fn copy_mapped_nv12(
    y_tex: &mut RegisteredTexture,
    uv_tex: &mut RegisteredTexture,
    dst: &DeviceBuffer,
) -> Result<()> {
    let (uv_ptr, uv_pitch) = dst
        .uv
        .ok_or_else(|| anyhow::anyhow!("copy_mapped_nv12 on a non-NV12 buffer"))?;
    let w = dst.width as usize;
    let h = dst.height as usize;
    y_tex.copy_mapped_plane(dst.ptr, dst.pitch, w, h)?;
    uv_tex.copy_mapped_plane(uv_ptr, uv_pitch, (w / 2) * 2, h / 2)
}

/// Copy the three YUV444 convert targets (registered full-res `R8` GL textures) into `dst`'s
/// stacked planes (`dst.yuv444`: rows `[0,H)`=Y, `[H,2H)`=U, `[2H,3H)`=V at `dst.pitch`). Each
/// copy syncs on our priority stream before returning, so the dmabuf is safe to recycle after.
pub fn copy_mapped_yuv444(
    y_tex: &mut RegisteredTexture,
    u_tex: &mut RegisteredTexture,
    v_tex: &mut RegisteredTexture,
    dst: &DeviceBuffer,
) -> Result<()> {
    anyhow::ensure!(dst.yuv444, "copy_mapped_yuv444 on a non-YUV444 buffer");
    let w = dst.width as usize;
    let h = dst.height as usize;
    let plane = |i: usize| dst.ptr + (dst.pitch * h * i) as CUdeviceptr;
    y_tex.copy_mapped_plane(plane(0), dst.pitch, w, h)?;
    u_tex.copy_mapped_plane(plane(1), dst.pitch, w, h)?;
    v_tex.copy_mapped_plane(plane(2), dst.pitch, w, h)
}

/// Copy a pitched device buffer into another device region (device→device), e.g. our imported
/// [`DeviceBuffer`] into a pooled CUDA surface NVENC owns. Both are 4-byte (BGRx) pixels.
/// The caller must have the shared context current on this thread (see [`make_current`]).
/// `sync: false` enqueues without a CPU wait (stream-ordered consumers only — `src` must stay
/// valid until the downstream stream work completes; see [`copy_stream_handle`]).
pub fn copy_device_to_device(
    src: &DeviceBuffer,
    dst_ptr: CUdeviceptr,
    dst_pitch: usize,
    sync: bool,
) -> Result<()> {
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src.ptr,
        srcPitch: src.pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst_ptr,
        dstPitch: dst_pitch,
        WidthInBytes: src.width as usize * 4,
        Height: src.height as usize,
        ..Default::default()
    };
    // SAFETY: `copy_issue` is unsafe (issues a CUDA copy); the caller must have the shared
    // context current (documented). `&copy` is a live local device→device `CUDA_MEMCPY2D` outliving
    // the enqueue: `srcDevice`/`srcPitch` are `src`'s live allocation, `dstDevice`/`dstPitch` the
    // caller's live region, `width*4`×`height` within both; `sync: false` shifts the source-
    // lifetime obligation to the caller (documented above). Wrapper → live table.
    unsafe { copy_issue(&copy, "cuMemcpy2DAsync_v2(dev->dev)", sync) }
}

/// Copy our imported NV12 [`DeviceBuffer`] (Y + UV planes) into NVENC's two-plane CUDA surface
/// `(y_dst, y_pitch)` / `(uv_dst, uv_pitch)` (`av_hwframe_get_buffer`'s `data[0]`/`data[1]` +
/// `linesize[0]`/`linesize[1]`). The Y plane is `width`×`height` bytes; the chroma plane is
/// `(width/2)·2` bytes × `height/2` rows. The caller must have the shared context current.
/// `sync: false` enqueues without a CPU wait (stream-ordered consumers only — `src` must stay
/// valid until the downstream stream work completes; see [`copy_stream_handle`]).
pub fn copy_nv12_to_device(
    src: &DeviceBuffer,
    y_dst: CUdeviceptr,
    y_pitch: usize,
    uv_dst: CUdeviceptr,
    uv_pitch: usize,
    sync: bool,
) -> Result<()> {
    let (src_uv_ptr, src_uv_pitch) = src
        .uv
        .ok_or_else(|| anyhow::anyhow!("copy_nv12_to_device on a non-NV12 buffer"))?;
    let w = src.width as usize;
    let h = src.height as usize;
    let y = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src.ptr,
        srcPitch: src.pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: y_dst,
        dstPitch: y_pitch,
        WidthInBytes: w, // 1 byte/px luma
        Height: h,
        ..Default::default()
    };
    let uv = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src_uv_ptr,
        srcPitch: src_uv_pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: uv_dst,
        dstPitch: uv_pitch,
        WidthInBytes: (w / 2) * 2, // 2 bytes/sample interleaved U,V
        Height: h / 2,
        ..Default::default()
    };
    // SAFETY: two unsafe `copy_async` device→device enqueues + an optional stream sync; the
    // caller must have the shared context current (documented). `&y`/`&uv` are live local
    // `CUDA_MEMCPY2D`s outliving each enqueue. All four device pointers are valid:
    // `src.ptr`/`src_uv_ptr` come from a live NV12 `DeviceBuffer` (its `.uv` presence was
    // checked via `ok_or_else`), `y_dst`/`uv_dst` are the caller's live NVENC surface planes;
    // the luma copy is `w`×`h`, the chroma copy `(w/2)*2`×`h/2`, each within its planes. With
    // `sync` the single trailing stream sync covers both enqueues (FIFO) before we return — the
    // same completion guarantee as the old per-plane blocking copies at half the exposed waits;
    // `sync: false` shifts the source-lifetime obligation to the caller (documented above).
    // Wrappers → live table.
    unsafe {
        // On a failed enqueue, drain the stream before propagating: the caller drops `src` on
        // `Err`, which recycles it into its pool — a copy still in flight would then race the
        // next frame written into the same allocation.
        let r = copy_async(&y, "cuMemcpy2DAsync_v2(nv12 Y dev->dev)")
            .and_then(|()| copy_async(&uv, "cuMemcpy2DAsync_v2(nv12 UV dev->dev)"));
        if r.is_err() {
            let _ = sync_copy_stream();
            return r;
        }
        if sync {
            sync_copy_stream()?;
        }
    }
    Ok(())
}

/// Copy our imported stacked-YUV444 [`DeviceBuffer`] into NVENC's three-plane CUDA surface
/// (`av_hwframe_get_buffer`'s `data[0..3]` + `linesize[0..3]` for a `yuv444p` frames context).
/// Each plane is `width`×`height` bytes; the source planes sit at row offsets `0/H/2H` of the
/// single allocation. The caller must have the shared context current.
/// `sync: false` enqueues without a CPU wait (stream-ordered consumers only — `src` must stay
/// valid until the downstream stream work completes; see [`copy_stream_handle`]).
pub fn copy_yuv444_to_device(
    src: &DeviceBuffer,
    dsts: [(CUdeviceptr, usize); 3],
    sync: bool,
) -> Result<()> {
    anyhow::ensure!(src.yuv444, "copy_yuv444_to_device on a non-YUV444 buffer");
    let w = src.width as usize;
    let h = src.height as usize;
    for (i, (dst_ptr, dst_pitch)) in dsts.into_iter().enumerate() {
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: CU_MEMORYTYPE_DEVICE,
            srcDevice: src.ptr + (src.pitch * h * i) as CUdeviceptr,
            srcPitch: src.pitch,
            dstMemoryType: CU_MEMORYTYPE_DEVICE,
            dstDevice: dst_ptr,
            dstPitch: dst_pitch,
            WidthInBytes: w, // 1 byte/px per plane
            Height: h,
            ..Default::default()
        };
        // SAFETY: unsafe `copy_async` device→device enqueue; the caller must have the shared
        // context current (documented). `&copy` is a live local outliving the enqueue;
        // `src.ptr + pitch·h·i` stays within the live 3·H-row stacked allocation (`yuv444`
        // checked above), `dst_ptr`/`dst_pitch` is the caller's live NVENC plane; `w`×`h` fits
        // both. Completion is the trailing stream sync below (`sync`) or the caller's
        // stream-ordering obligation (`sync: false`, documented above). A failed enqueue drains
        // the stream first — earlier planes are already queued, and the caller drops (recycles)
        // `src` on `Err`. Wrapper → live table.
        unsafe {
            if let Err(e) = copy_async(&copy, "cuMemcpy2DAsync_v2(yuv444 plane dev->dev)") {
                let _ = sync_copy_stream();
                return Err(e);
            }
        }
    }
    if sync {
        // SAFETY: one stream sync after the last enqueue covers all three planes (FIFO) — the
        // same completion guarantee as the old per-plane blocking copies at a third of the
        // exposed waits. Context current per the caller's contract. Wrapper → live table.
        unsafe { sync_copy_stream()? };
    }
    Ok(())
}

impl RegisteredTexture {
    /// Unregister now (idempotent; the later `Drop` then no-ops). Teardown-order helper: the blit
    /// destructors call this to release the CUDA registration BEFORE deleting the GL texture it
    /// wraps — deleting a still-registered texture leaves the driver holding a registration onto
    /// freed GL state, exactly the stale-driver-state class this path once crashed on.
    pub fn release(&mut self) {
        if self.resource.is_null() {
            return;
        }
        // SAFETY: `self.resource` is non-null (just checked) and is the valid `CUgraphicsResource`
        // from `register_gl`, owned exclusively by this `RegisteredTexture`; nulling the field
        // right after makes this (and the `Drop` below) unregister it exactly once — no
        // use-after-free or double-unregister. We make the shared context current first because a
        // release may run during teardown on a thread where it isn't. Wrapper → live table (the
        // resource exists ⇒ the driver was present). Result ignored (best-effort teardown).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            let _ = cuGraphicsUnregisterResource(self.resource);
        }
        self.resource = std::ptr::null_mut();
    }
}

impl Drop for RegisteredTexture {
    fn drop(&mut self) {
        self.release();
    }
}

/// A dmabuf fd imported as CUDA external memory and mapped to a device pointer — the LINEAR
/// path (gamescope): the buffer's bytes are directly addressable, no GL de-tiling needed.
/// Cached per PipeWire buffer (the fd pool is stable for a stream's life); destroyed on drop.
pub struct ExternalDmabuf {
    ext: CUexternalMemory,
    pub ptr: CUdeviceptr,
    pub size: u64,
}

// SAFETY: the fields are opaque CUDA driver handles — an external-memory handle and a device
// pointer — not dereferenceable Rust memory, and the value is uniquely owned (no `Clone`). It is
// used from a single capture thread but constructed on / moved between threads with the importer;
// transferring these handles is sound because uniqueness rules out aliasing and they are destroyed
// exactly once in `Drop`. Only `Send` (not `Sync`) is asserted, matching the single-thread use.
unsafe impl Send for ExternalDmabuf {}

impl ExternalDmabuf {
    /// Import `fd` (NOT consumed — an internal `dup` is handed to the driver, which owns it
    /// from then on) and map its full `size` bytes to a device pointer. The shared context
    /// must be current.
    pub fn import(fd: i32, size: u64) -> Result<ExternalDmabuf> {
        // SAFETY: `libc::dup` only reads the integer `fd` and returns a new descriptor (or -1); it
        // touches no Rust memory and `fd` is the caller's still-owned dmabuf fd (not consumed
        // here). No aliasing or lifetime concern — a pure syscall on an integer.
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            bail!("dup(dmabuf fd) failed");
        }
        Self::import_owned_fd(dup, size)
    }

    /// Import an fd the caller hands over (e.g. a Vulkan-exported `OPAQUE_FD`) — consumed by
    /// the driver on success, closed by us on failure.
    pub fn import_owned_fd(dup: i32, size: u64) -> Result<ExternalDmabuf> {
        let mut desc = CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
            type_: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            size,
            ..Default::default()
        };
        desc.handle[0] = dup as u32 as u64; // union member `int fd` (little-endian low bytes)
        let mut ext: CUexternalMemory = std::ptr::null_mut();
        // SAFETY: `cuImportExternalMemory` imports the memory described by `&desc`, a live local
        // `#[repr(C)] CUDA_EXTERNAL_MEMORY_HANDLE_DESC` (cuda.h 64-bit layout) that outlives this
        // synchronous call: `type_` is OPAQUE_FD, `handle[0]` holds the dup'd fd in the union's
        // `int fd` low bytes, `size` is set. `&mut ext` is a live null-init out-param the driver
        // writes the imported handle into. The driver takes ownership of the fd only on success.
        // Distinct locals → no aliasing. Wrapper → live table (caller holds the context current).
        let r = unsafe { cuImportExternalMemory(&mut ext, &desc) };
        if r != 0 {
            // SAFETY: import failed (`r != 0`), so the driver did NOT take ownership of `dup`; we
            // still own it and close it exactly once here on the error path (the success path never
            // closes it — the driver does). `libc::close` acts on the integer fd alone.
            unsafe { libc::close(dup) }; // import failed → the driver did not take the fd
            bail!("cuImportExternalMemory failed ({r}) — LINEAR dmabuf import unsupported?");
        }
        let buf = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
            offset: 0,
            size,
            ..Default::default()
        };
        let mut ptr: CUdeviceptr = 0;
        // SAFETY: maps a device pointer from `ext` (the valid `CUexternalMemory` just imported) per
        // `&buf`, a live local `CUDA_EXTERNAL_MEMORY_BUFFER_DESC` (offset 0, full `size`) that
        // outlives this synchronous call. `&mut ptr` is a live zero-init out-param the driver writes
        // the mapped device address into; distinct locals → no aliasing. Wrapper → live table
        // (context current).
        let r = unsafe { cuExternalMemoryGetMappedBuffer(&mut ptr, ext, &buf) };
        if r != 0 {
            // SAFETY: mapping failed; `ext` is the valid `CUexternalMemory` we imported and
            // exclusively own. We destroy it exactly once here on the error path (the success path
            // instead moves it into the returned `ExternalDmabuf`, whose `Drop` destroys it),
            // releasing the fd the driver took — no double-destroy or use-after-free.
            unsafe {
                let _ = cuDestroyExternalMemory(ext);
            }
            bail!("cuExternalMemoryGetMappedBuffer failed ({r})");
        }
        Ok(ExternalDmabuf { ext, ptr, size })
    }
}

impl Drop for ExternalDmabuf {
    fn drop(&mut self) {
        // SAFETY: this `ExternalDmabuf` only exists after a successful import, so the driver table
        // is live. It exclusively owns `self.ptr` (the mapped buffer) and `self.ext` (the external
        // memory), each torn down exactly once here (drop runs once; guarded by `!= 0` / `!null`) —
        // no double-free or use-after-free. We make the shared context current first because drop
        // may run off the import thread, and we free the mapped buffer before destroying its
        // backing external memory. Results ignored (best-effort teardown).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            if self.ptr != 0 {
                let _ = cuMemFree_v2(self.ptr); // mapped buffers are freed like device memory
            }
            if !self.ext.is_null() {
                let _ = cuDestroyExternalMemory(self.ext);
            }
        }
    }
}

/// A Vulkan **timeline** semaphore imported as a CUDA external semaphore — the cross-API ordering
/// primitive for the stream-ordered cursor blend (`vkslot.rs`): CUDA [`signal`](Self::signal)s a
/// value on this thread's copy stream once the input copy is enqueued, the Vulkan blend waits for
/// and then advances the timeline on its queue, and CUDA [`wait`](Self::wait)s that advanced
/// value before the encode — all ordering on-device, no CPU sync anywhere. One imported handle
/// per [`VkSlotBlend`](super::vkslot::VkSlotBlend); values are monotonic for its lifetime.
pub struct ExternalSemaphore {
    sem: CUexternalSemaphore,
}

// SAFETY: `CUexternalSemaphore` is an opaque driver handle with no thread affinity (the driver
// API allows use from any thread with the context current). It is uniquely owned here, used from
// the encode thread but moved with its `VkSlotBlend`, and destroyed exactly once in `Drop` —
// `Send` (not `Sync`) matches that single-thread-at-a-time use, like `ExternalDmabuf`.
unsafe impl Send for ExternalSemaphore {}

impl ExternalSemaphore {
    /// Import a Vulkan timeline semaphore exported as an OPAQUE_FD (`vkGetSemaphoreFdKHR`). The
    /// fd is handed over: the driver owns it on success, we close it on failure. The shared
    /// context must be current.
    pub fn import_owned_timeline_fd(fd: i32) -> Result<ExternalSemaphore> {
        let mut desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
            type_: CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD,
            ..Default::default()
        };
        desc.handle[0] = fd as u32 as u64; // union member `int fd` (little-endian low bytes)
        let mut sem: CUexternalSemaphore = std::ptr::null_mut();
        // SAFETY: `cuImportExternalSemaphore` reads `&desc`, a live local `#[repr(C)]`
        // `CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC` (layout-asserted below) outliving the synchronous
        // call: `type_` is TIMELINE_SEMAPHORE_FD and `handle[0]` holds the fd in the union's
        // `int fd` low bytes. `&mut sem` is a live null-init out-param the driver writes the
        // imported handle into. Distinct locals → no aliasing. Wrapper → live table (caller holds
        // the context current).
        let r = unsafe { cuImportExternalSemaphore(&mut sem, &desc) };
        if r != 0 {
            // SAFETY: import failed (`r != 0`), so the driver did NOT take ownership of `fd`; we
            // still own it and close it exactly once here. `libc::close` acts on the integer alone.
            unsafe { libc::close(fd) };
            bail!("cuImportExternalSemaphore failed ({r}) — timeline-semaphore fd export/import unsupported?");
        }
        Ok(ExternalSemaphore { sem })
    }

    /// Enqueue a signal that sets the timeline to `value` once all prior work on THIS THREAD's
    /// copy stream (the stream `copy_stream_handle` exposes) completes. No CPU wait.
    pub fn signal(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: `self.sem` is the live imported handle (this struct only exists after a
        // successful import; destroyed only in `Drop`). `&self.sem`/`&params` are live locals the
        // synchronous enqueue reads (count 1); the driver retains no pointer into Rust memory.
        // The stream is this thread's live copy stream. Wrapper → live table (context current).
        unsafe {
            ck(
                cuSignalExternalSemaphoresAsync(&self.sem, &params, 1, copy_stream()),
                "cuSignalExternalSemaphoresAsync",
            )
        }
    }

    /// Enqueue a wait: work enqueued on THIS THREAD's copy stream after this call runs only once
    /// the timeline reaches `value`. No CPU wait.
    pub fn wait(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: same contract as `signal` — live handle, live locals across the synchronous
        // enqueue, this thread's live copy stream. Wrapper → live table (context current).
        unsafe {
            ck(
                cuWaitExternalSemaphoresAsync(&self.sem, &params, 1, copy_stream()),
                "cuWaitExternalSemaphoresAsync",
            )
        }
    }
}

impl Drop for ExternalSemaphore {
    fn drop(&mut self) {
        // SAFETY: `self.sem` is the valid imported handle this struct exclusively owns, destroyed
        // exactly once here. The shared context is made current first because drop may run off
        // the import thread (`VkSlotBlend` teardown quiesces the GPU before dropping, so no
        // enqueued signal/wait still references the semaphore). Result ignored (best-effort).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            let _ = cuDestroyExternalSemaphore(self.sem);
        }
    }
}

/// Copy a pitched span starting at `src_ptr` (e.g. an [`ExternalDmabuf`] mapping at the chunk
/// offset) into `dst`. The shared context must be current on this thread.
pub fn copy_pitched_to_buffer(
    src_ptr: CUdeviceptr,
    src_pitch: usize,
    dst: &DeviceBuffer,
) -> Result<()> {
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: src_ptr,
        srcPitch: src_pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst.ptr,
        dstPitch: dst.pitch,
        WidthInBytes: dst.width as usize * 4,
        Height: dst.height as usize,
        ..Default::default()
    };
    // copy_blocking syncs our priority stream before returning, so the copy is complete before the
    // dmabuf is requeued to the producer.
    // SAFETY: `copy_blocking` is unsafe (issues a CUDA copy); the caller must have the shared
    // context current (documented). `&copy` is a live local device→device `CUDA_MEMCPY2D` outliving
    // the synchronous call: `srcDevice`/`srcPitch` are the caller's live mapped span (e.g. an
    // `ExternalDmabuf`), `dstDevice`/`dstPitch` are `dst`'s live allocation, `width*4`×`height`
    // within both. Wrapper → live table.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(ext->dev)") }
}

/// De-stride an NV12 pair from an external mapping (the Vulkan bridge's exportable buffer after
/// its compute CSC — latency plan T2.5b) into a pooled two-plane NV12 [`DeviceBuffer`]: the Y
/// plane (`width` bytes × `height` rows) and the interleaved UV plane (`width` bytes × ⌈h/2⌉
/// rows), each de-strided from `src_pitch` to the pool's own plane pitches. Same contract as
/// [`copy_pitched_to_buffer`]: the shared context must be current.
pub fn copy_pitched_nv12_to_buffer(
    y_src: CUdeviceptr,
    uv_src: CUdeviceptr,
    src_pitch: usize,
    dst: &DeviceBuffer,
) -> Result<()> {
    let Some((uv_ptr, uv_pitch)) = dst.uv else {
        anyhow::bail!("copy_pitched_nv12_to_buffer: destination is not an NV12 buffer");
    };
    let y = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: y_src,
        srcPitch: src_pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst.ptr,
        dstPitch: dst.pitch,
        WidthInBytes: dst.width as usize,
        Height: dst.height as usize,
        ..Default::default()
    };
    let uv = CUDA_MEMCPY2D {
        srcMemoryType: CU_MEMORYTYPE_DEVICE,
        srcDevice: uv_src,
        srcPitch: src_pitch,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: uv_ptr,
        dstPitch: uv_pitch,
        // W/2 interleaved UV samples × 2 bytes = `width` bytes per row.
        WidthInBytes: dst.width as usize,
        Height: dst.height.div_ceil(2) as usize,
        ..Default::default()
    };
    // SAFETY: same contract as `copy_pitched_to_buffer` — the caller holds the shared context
    // current; both `CUDA_MEMCPY2D`s are live locals describing spans inside the caller's live
    // external mapping (`y_src`/`uv_src` at `src_pitch`) and `dst`'s live pooled planes; each
    // `copy_blocking` synchronizes before returning.
    unsafe {
        copy_blocking(&y, "cuMemcpy2DAsync_v2(ext->dev nv12 Y)")?;
        copy_blocking(&uv, "cuMemcpy2DAsync_v2(ext->dev nv12 UV)")
    }
}

// The cuda.h struct layouts these calls depend on are asserted at COMPILE time in `ffi.rs`
// (`const _`), which covers all six structs and every build — the runtime test that used to live
// here checked three of them and only when someone ran it.
