//! CUDA driver-API facade over `ffi` (`dlopen` of `libcuda.so.1`). The FFI is hand-rolled: no
//! crate exposes the GL-interop calls, and the load is runtime so one binary still runs where
//! `libcuda` is absent (AMD/Intel).
//!
//! Owns the process-wide `CUcontext` (lazy; shared by the EGL importer and NVENC; each thread
//! makes it current), pitched device memory (`BufferPool` / `DeviceBuffer` / IPC / plane copies),
//! and GL / external-memory interop (`RegisteredTexture`, `ExternalDmabuf`).
//!
//! GL interop, not EGL: `cuGraphicsEGLRegisterImage` is Tegra-only on the desktop driver
//! ([`super::egl`]). Cursor blend is the SPIR-V pass in [`super::vkslot`].

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{bail, Result};
use std::os::raw::{c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};

#[path = "cuda/ffi.rs"]
mod ffi;
// `pub` (not `pub(crate)`): the raw driver-API vocabulary (`CUdeviceptr`, …) is consumed across
// the crate boundary by the encode backends' CUDA-frame paths.
pub use ffi::*;

/// Packed host readback of a pitched device plane. Synchronous; self-test only, not the hot path.
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
    // SAFETY: `copy` outlives the synchronous copy. `srcDevice`/`srcPitch` are the caller's pitched
    // plane; `dstHost` is `host` (`width_bytes*height` bytes). Context current is the caller's job.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(dev->host)")? };
    Ok(host)
}

/// Packed host→pitched-device upload. Synchronous. Benchmarks only: uninitialised device memory
/// comes back zeroed, and CBR then has nothing to code.
pub fn write_plane_from_host(
    dst_ptr: CUdeviceptr,
    dst_pitch: usize,
    src: &[u8],
    width_bytes: usize,
    height: usize,
) -> Result<()> {
    anyhow::ensure!(
        src.len() >= width_bytes * height,
        "write_plane_from_host: source is {} bytes, need {}",
        src.len(),
        width_bytes * height
    );
    let copy = CUDA_MEMCPY2D {
        srcMemoryType: 1, // CU_MEMORYTYPE_HOST
        srcHost: src.as_ptr() as *const c_void,
        srcPitch: width_bytes,
        dstMemoryType: CU_MEMORYTYPE_DEVICE,
        dstDevice: dst_ptr,
        dstPitch: dst_pitch,
        WidthInBytes: width_bytes,
        Height: height,
        ..Default::default()
    };
    // SAFETY: `copy` outlives the synchronous copy. `srcHost` is `src` (≥ `width_bytes*height`);
    // `dstDevice`/`dstPitch` are the caller's pitched plane. Sync, so `src` need not outlive return.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(host->dev)") }
}

/// Export `ptr` as a 64-byte IPC handle. The allocation must outlive every importer; context current.
pub fn ipc_export(ptr: CUdeviceptr) -> Result<[u8; CU_IPC_HANDLE_SIZE]> {
    let mut handle = CUipcMemHandle {
        reserved: [0; CU_IPC_HANDLE_SIZE],
    };
    // SAFETY: `&mut handle` is a live out-param the driver fills; `ptr` is the caller's live
    // allocation. Synchronous; retains no Rust pointer. Context current.
    unsafe { ck(cuIpcGetMemHandle(&mut handle, ptr), "cuIpcGetMemHandle")? };
    Ok(handle.reserved)
}

/// Map an IPC handle from another process. Valid until [`ipc_close`]. Context current.
pub fn ipc_open(handle: &[u8; CU_IPC_HANDLE_SIZE]) -> Result<CUdeviceptr> {
    let h = CUipcMemHandle { reserved: *handle };
    let mut ptr: CUdeviceptr = 0;
    // SAFETY: `h` is passed by value (`CUipcMemHandle` ABI); `&mut ptr` is a live out-param for the
    // mapped address. Synchronous. Context current.
    unsafe {
        ck(
            cuIpcOpenMemHandle(&mut ptr, h, CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS),
            "cuIpcOpenMemHandle",
        )?
    };
    Ok(ptr)
}

/// Close an [`ipc_open`] mapping. Best-effort; makes the shared context current (Drop may be off-thread).
pub fn ipc_close(ptr: CUdeviceptr) {
    if ptr == 0 {
        return;
    }
    // SAFETY: `ptr` came from `cuIpcOpenMemHandle` and is closed once by the owning cache. Context
    // is set current first: this runs from `Drop` on whichever thread holds the last reference.
    unsafe {
        if let Some(c) = CONTEXT.get() {
            let _ = cuCtxSetCurrent(c.0);
        }
        let _ = cuIpcCloseMemHandle(ptr);
    }
}

/// Process-wide CUDA context. `Send`/`Sync` so it can live in a `OnceLock`; the driver allows
/// `cuCtxSetCurrent` from any thread.
#[derive(Clone, Copy)]
pub struct Context(pub CUcontext);
// SAFETY: `CUcontext` is an opaque driver handle, not a Rust pointer. Created once, never
// destroyed (process lifetime). The only use is `cuCtxSetCurrent`, which the Driver API allows
// from any thread — transferring the handle cannot dangle or race.
unsafe impl Send for Context {}
// SAFETY: the wrapped handle is an immutable opaque address; the driver owns synchronization.
unsafe impl Sync for Context {}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// Shared CUDA context on device 0, created once.
pub fn context() -> Result<CUcontext> {
    if let Some(c) = CONTEXT.get() {
        return Ok(c.0);
    }
    if cuda_api().is_none() {
        bail!("libcuda.so.1 not available — no NVIDIA driver (CUDA zero-copy disabled)");
    }
    // SAFETY: `cuda_api()` is `Some` (checked above), so wrappers hit the live `libcuda` table.
    // `cuInit(0)`: flags 0 is the API-required value. `&mut dev`/`&mut ctx` are live out-params
    // that outlive their synchronous calls. `ck` bails unless `ctx` is a valid `CUcontext`.
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
    // Racy first-init: the winner's context is used; a loser leaks one context (process lifetime).
    Ok(CONTEXT.get_or_init(|| Context(ctx)).0)
}

/// Bind the shared context to this thread. Required before any CUDA op here.
pub fn make_current() -> Result<()> {
    let ctx = context()?;
    // SAFETY: `ctx` is the live shared `CUcontext` from `context()?`. `cuCtxSetCurrent` binds it
    // to this thread only; it takes no Rust pointer.
    unsafe { ck(cuCtxSetCurrent(ctx), "cuCtxSetCurrent") }
}

/// Run `probe` on a throwaway device-0 context, then restore the shared one. Diagnostic: splits a
/// bad shared context from a driver-wide failure. Never a hot path.
pub fn with_fresh_context<R>(probe: impl FnOnce(CUcontext) -> R) -> Result<R> {
    if cuda_api().is_none() {
        bail!("libcuda.so.1 not available");
    }
    // SAFETY: driver table present (checked above). `cuInit(0)` is idempotent. `&mut dev`/`&mut ctx`
    // are live out-params. `ctx` is destroyed once below; creation left it current, so restore the
    // shared context afterwards.
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
    /// Per-thread copy stream. `None` until first use; `Some(null)` = creation failed, use the
    /// NULL stream. Per-thread so `cuStreamSynchronize` waits only this worker's copies.
    static COPY_STREAM: std::cell::Cell<Option<CUstream>> = const { std::cell::Cell::new(None) };
}

/// This thread's highest-priority copy stream (lazy; context must be current). `greatest` from
/// `cuCtxGetStreamPriorityRange` is the numerically lowest value. Intra-process hint only; the
/// Linux driver may ignore it. Falls back to the NULL stream.
fn copy_stream() -> CUstream {
    COPY_STREAM.with(|cell| {
        if let Some(s) = cell.get() {
            return s;
        }
        // SAFETY: context is current (doc contract). `&mut least`/`&mut greatest`/`&mut s` are live
        // out-params that outlive their synchronous calls. Non-zero result → null stream; never
        // read an uninitialized handle.
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

/// Enqueue `copy` on this thread's priority stream and wait. The source is safe to recycle once
/// this returns; the wait is this stream only.
unsafe fn copy_blocking(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory. `&copy` outlives
    // the synchronous call.
    unsafe {
        let stream = copy_stream();
        ck(cuMemcpy2DAsync_v2(copy, stream), what)?;
        ck(cuStreamSynchronize(stream), "cuStreamSynchronize")
    }
}

/// Enqueue `copy` with no CPU wait. Stream-ordered consumers only: `src` must stay valid until
/// downstream stream work (the encode) finishes.
unsafe fn copy_async(copy: &CUDA_MEMCPY2D, what: &str) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory that stays valid
    // until the stream work completes.
    unsafe { ck(cuMemcpy2DAsync_v2(copy, copy_stream()), what) }
}

/// Wait for this thread's copy stream. One sync after the last enqueue covers every plane (FIFO).
/// Context must be current.
unsafe fn sync_copy_stream() -> Result<()> {
    // SAFETY: caller: context current. Stream sync touches no Rust memory.
    unsafe { ck(cuStreamSynchronize(copy_stream()), "cuStreamSynchronize") }
}

/// `sync: false` carries `copy_async`'s source-lifetime contract.
unsafe fn copy_issue(copy: &CUDA_MEMCPY2D, what: &str, sync: bool) -> Result<()> {
    // SAFETY: caller: context current and `copy` describes live in-bounds memory.
    unsafe {
        if sync {
            copy_blocking(copy, what)
        } else {
            copy_async(copy, what)
        }
    }
}

/// This thread's copy stream as a raw handle, for `NvEncSetIOCudaStreams`. Null means ordering
/// is unavailable — keep blocking copies. Context must be current.
pub fn copy_stream_handle() -> *mut c_void {
    copy_stream() // CUstream is *mut c_void (opaque CUstream_st*)
}

/// Max cursor-overlay bitmap edge (px) uploaded to the device blend buffer — matches the Vulkan path.
pub const CURSOR_MAX: u32 = 256;

/// Pitched device buffer of `width`×`height` 4-byte pixels. Returns `(ptr, pitch)`.
fn alloc_pitched(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: `&mut ptr`/`&mut pitch` are live out-params that outlive the synchronous alloc. Width,
    // height, and element-size are by-value.
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

/// One pitched allocation of three stacked full-res 1-byte planes: rows `[0,H)` Y, `[H,2H)` U,
/// `[2H,3H)` V. One IPC handle, same as RGB.
fn alloc_pitched_yuv444(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // SAFETY: `&mut ptr`/`&mut pitch` are live out-params that outlive the synchronous alloc.
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

/// Two pitched NV12 planes (8-bit 4:2:0): Y is W×H bytes; UV is W bytes × H/2 (interleaved). Both
/// use the driver's Y pitch so the encoder's two-plane surface matches.
fn alloc_pitched_nv12(
    width: u32,
    height: u32,
) -> Result<((CUdeviceptr, usize), (CUdeviceptr, usize))> {
    let mut y_ptr: CUdeviceptr = 0;
    let mut y_pitch: usize = 0;
    let mut uv_ptr: CUdeviceptr = 0;
    let mut uv_pitch: usize = 0;
    // SAFETY: four live out-params outlive their synchronous allocs. If UV fails, Y is freed
    // before the error returns — a leak here is a per-frame `BufferPool::get` miss.
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
        // Chroma: W/2 samples × 2 bytes = W bytes (even); H/2 rows.
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

/// Contiguous NV12: Y rows `[0,H)` then UV `[H, 3H/2)` at one pitch. NVENC's single `CUDADEVICEPTR`
/// input reads UV at `ptr + pitch*height`. Encode side only ([`InputSurface`]).
fn alloc_pitched_nv12_contiguous(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
    // UV is H/2 rows at the same width-bytes as Y; NVENC finds it at `ptr + pitch*H`.
    let rows = height as usize + (height as usize / 2).max(1);
    // SAFETY: `&mut ptr`/`&mut pitch` are live out-params that outlive the synchronous alloc.
    unsafe {
        ck(
            cuMemAllocPitch_v2(&mut ptr, &mut pitch, width as usize, rows, 16),
            "cuMemAllocPitch_v2(NV12 contiguous)",
        )?;
    }
    Ok((ptr, pitch))
}

/// Encoder-owned contiguous pitched CUDA surface. Registered once as
/// `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR` (`encode/linux/nvenc_cuda.rs`). Layout matches
/// NVENC's single-pointer register: NV12 = Y then UV, YUV444 = Y|U|V stacked, RGB = packed 4-byte.
/// Never pooled or sent on the wire. Frees on drop (context made current; drop may be off-thread).
pub struct InputSurface {
    /// NVENC register pointer. NV12 chroma at `ptr + pitch*height`; YUV444 U/V at `1*`/`2*`.
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    /// Luma rows — the plane-stride multiplier for NVENC and the copy helpers.
    pub height: u32,
}

impl InputSurface {
    /// Contiguous NV12 (Y then UV, one pitch).
    pub fn alloc_nv12(width: u32, height: u32) -> Result<InputSurface> {
        let (ptr, pitch) = alloc_pitched_nv12_contiguous(width, height)?;
        Ok(InputSurface { ptr, pitch, height })
    }

    /// Planar YUV444 stacked Y|U|V (see `alloc_pitched_yuv444`).
    pub fn alloc_yuv444(width: u32, height: u32) -> Result<InputSurface> {
        let (ptr, pitch) = alloc_pitched_yuv444(width, height)?;
        Ok(InputSurface { ptr, pitch, height })
    }

    /// Packed 4-byte RGB/BGRx. NVENC CSCs when registered as `ABGR`/`ARGB`.
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
        // SAFETY: this surface exclusively owns `self.ptr`, freed once (`ptr == 0` skips empty).
        // Context is set current first: drop may run on a thread where it isn't.
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            let _ = cuMemFree_v2(self.ptr);
        }
    }
}

/// Free-list of recycled device allocations for one resolution. Shared (`Arc`) between capture
/// (hands out) and encode (`DeviceBuffer` drop returns here). NV12: Y and UV stay paired.
struct PoolInner {
    free: Vec<CUdeviceptr>,
    /// NV12: UV plane paired with each Y in `free` (same index, same length).
    free_uv: Vec<CUdeviceptr>,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        // SAFETY: drops only after every `DeviceBuffer` `Arc` is gone, so `free`/`free_uv` hold
        // each allocation once and nothing still uses them. Context is set current first: drop
        // may run off the allocating thread. Each `p` came from `cuMemAllocPitch_v2`.
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

/// Reusable pitched device buffers at a fixed resolution. Avoids per-frame `cuMemAllocPitch` /
/// `cuMemFree`, which take the device allocator lock.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Mutex<PoolInner>>,
    width: u32,
    height: u32,
    pitch: usize,
    /// `Some` ⇒ NV12; buffers carry a UV plane at this pitch.
    uv_pitch: Option<usize>,
    /// YUV444: one allocation of 3·`height` stacked 1-byte planes.
    yuv444: bool,
}

impl BufferPool {
    /// Pool of `width`×`height` 4-byte buffers. Allocates one to learn the driver's pitch.
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

    /// Pool of NV12 (Y + interleaved UV). Allocates one pair to learn per-plane pitches.
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

    /// Pool of planar YUV444: one allocation per buffer, stacked `[Y | U | V]`, so the wire/IPC
    /// path carries it like a single-plane buffer.
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

    /// Recycled if free, else freshly allocated. Returns to this pool on drop (consumer must have
    /// synced). NV12: Y and its paired UV.
    pub fn get(&self) -> Result<DeviceBuffer> {
        if let Some(uv_pitch) = self.uv_pitch {
            let reuse = {
                let mut g = self.inner.lock().unwrap();
                g.free.pop().map(|y| (y, g.free_uv.pop()))
            };
            let (ptr, uv_ptr) = match reuse {
                // Pushed/popped together, so a popped Y always has its UV.
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

/// Pitched device buffer for one captured frame. Filled from the EGL-mapped dmabuf so the dmabuf
/// can return to the compositor immediately. Pooled buffers recycle on drop; others free.
pub struct DeviceBuffer {
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
    /// NV12 chroma `(ptr, pitch)` paired with Y in [`ptr`](Self::ptr). `None` for 4-byte RGB/BGRx.
    pub uv: Option<(CUdeviceptr, usize)>,
    /// YUV444: [`ptr`](Self::ptr) is one allocation of 3·[`height`](Self::height) stacked Y,U,V
    /// (`uv` stays `None`; the single-plane wire/IPC path carries it unchanged).
    pub yuv444: bool,
    pool: Option<Arc<Mutex<PoolInner>>>,
    /// IPC import: drop runs this once (owner recycles). Must not free or pool-recycle locally.
    remote_release: Option<Box<dyn FnOnce() + Send>>,
}

impl DeviceBuffer {
    /// Standalone pitched buffer. Prefer [`BufferPool`] on the hot path.
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

    /// Standalone NV12 two-plane buffer. Prefer [`BufferPool::new_nv12`]; used by the self-test.
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

    /// Standalone planar-YUV444 stacked buffer. Prefer [`BufferPool::new_yuv444`]; self-test.
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

    pub fn is_nv12(&self) -> bool {
        self.uv.is_some()
    }

    /// Wrap planes owned by another process ([`ipc_open`]). `release` runs once on drop; nothing
    /// is freed or pooled here (the IPC cache closes the mapping after the last remote buffer).
    /// `yuv444` marks stacked 3-plane YUV444 — the wire carries no format (`ImportKind::Tiled444`).
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
            release();
            return;
        }
        if self.ptr == 0 {
            return;
        }
        if let Some(pool) = &self.pool {
            // Consumer synced before drop. Y and UV go back together so `get` can pop them as a unit.
            let mut g = pool.lock().unwrap();
            g.free.push(self.ptr);
            if let Some((uv_ptr, _)) = self.uv {
                g.free_uv.push(uv_ptr);
            }
        } else {
            // SAFETY: un-pooled: this buffer exclusively owns `self.ptr` and `self.uv`, each from
            // `cuMemAllocPitch_v2`, freed once (`ptr == 0` skipped above). Context is set current
            // first: drop may run on the encode thread, where it isn't.
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

/// Persistent GL-texture→CUDA registration. Desktop NVIDIA CUDA interop is GL textures, not
/// dmabuf EGLImages: the importer renders the dmabuf into a reusable `GL_RGBA8` texture, registers
/// once, then each frame maps → copies → unmaps (the map/unmap pair is the GL↔CUDA sync).
pub struct RegisteredTexture {
    resource: CUgraphicsResource,
}

impl RegisteredTexture {
    /// # Safety
    /// The GL context and the shared CUDA context must both be current on this thread, and
    /// `texture` must be a valid `GL_TEXTURE_2D`.
    pub unsafe fn register_gl(texture: u32) -> Result<RegisteredTexture> {
        // SAFETY: caller: GL context owning `texture` and the shared CUDA context are current;
        // `texture` is a live `GL_TEXTURE_2D`. Out-param is a live stack local.
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

    /// Map, copy the linear RGBA8 array into `dst`, unmap. Syncs on the priority stream before
    /// unmap so `dst` is ready before the dmabuf is recycled. Always unmaps, even on copy error.
    pub fn copy_mapped_to(&mut self, dst: &DeviceBuffer) -> Result<()> {
        // SAFETY: `self.resource` is from `register_gl`. Caller holds GL+CUDA current. Map, copy,
        // and unmap all use `copy_stream()`: map only orders prior GL work before CUDA issued *in
        // that stream*, and `copy_stream` is `CU_STREAM_NON_BLOCKING` (no implicit NULL-stream
        // order). Mapping on NULL let the copy race the GL de-tile. `array` is mip 0; unmap on
        // GetMappedArray failure. `copy` outlives `copy_blocking`; `srcArray` valid while mapped;
        // `dst` live; `width*4`×`height` fit. Always unmap after the copy (even on error).
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

    /// Map and copy into `(dst_ptr, dst_pitch)` for `width_bytes`×`height` (`width` for `R8` luma,
    /// `(width/2)*2` for `RG8` chroma). Syncs before unmap; always unmaps, even on copy error.
    fn copy_mapped_plane(
        &mut self,
        dst_ptr: CUdeviceptr,
        dst_pitch: usize,
        width_bytes: usize,
        height: usize,
    ) -> Result<()> {
        // SAFETY: same as `copy_mapped_to`. `array` is mip 0; unmap on GetMappedArray failure.
        // `copy` outlives `copy_blocking`; `srcArray` valid while mapped; dest plane live;
        // `width_bytes`×`height` fit. Always unmap after the copy.
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

/// Copy registered `R8` luma + `RG8` chroma into `dst`'s NV12 planes (`dst.uv` set). Y is
/// `width`×`height` bytes; UV is `(width/2)·2` × `height/2`. Both copies sync before return.
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

/// Copy three full-res `R8` textures into `dst`'s stacked YUV444 planes (`[0,H)` Y, `[H,2H)` U,
/// `[2H,3H)` V). Each copy syncs before return.
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

/// Device→device copy of a 4-byte (BGRx) [`DeviceBuffer`] into `dst_ptr`. Context must be current.
/// `sync: false`: no CPU wait; `src` must stay valid until downstream stream work completes.
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
    // SAFETY: caller: context current. `copy` outlives the enqueue; `src` and `dst` are live;
    // `width*4`×`height` fit both. `sync: false` shifts source lifetime to the caller.
    unsafe { copy_issue(&copy, "cuMemcpy2DAsync_v2(dev->dev)", sync) }
}

/// Copy imported NV12 into NVENC's two-plane surface (`data[0]`/`data[1]`). Y is `width`×`height`;
/// UV is `(width/2)·2` × `height/2`. Context current. `sync: false`: `src` must stay valid until
/// downstream stream work completes.
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
    // SAFETY: caller: context current. `&y`/`&uv` outlive each enqueue. `src` is a live NV12
    // buffer (`.uv` checked); `y_dst`/`uv_dst` are the caller's NVENC planes. `sync` waits both
    // (FIFO); `sync: false` shifts source lifetime to the caller.
    unsafe {
        // Failed enqueue: drain before return. Caller drops `src` on `Err` (pool recycle); a
        // copy still in flight would race the next frame in that allocation.
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

/// Copy stacked YUV444 into NVENC's three-plane surface (`data[0..3]`). Each plane is
/// `width`×`height`; source at row offsets `0/H/2H`. Context current. `sync: false`: `src` must
/// stay valid until downstream stream work completes.
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
        // SAFETY: caller: context current. `copy` outlives the enqueue. `src.ptr + pitch·h·i`
        // is inside the live 3·H stacked allocation (`yuv444` checked); dest is the caller's
        // NVENC plane. Drain on enqueue failure: earlier planes are queued and caller recycles
        // `src` on `Err`.
        unsafe {
            if let Err(e) = copy_async(&copy, "cuMemcpy2DAsync_v2(yuv444 plane dev->dev)") {
                let _ = sync_copy_stream();
                return Err(e);
            }
        }
    }
    if sync {
        // SAFETY: one stream sync after the last enqueue covers all three planes (FIFO). Context
        // current per the caller.
        unsafe { sync_copy_stream()? };
    }
    Ok(())
}

impl RegisteredTexture {
    /// Unregister now (idempotent; later `Drop` no-ops). Call before deleting the GL texture:
    /// a still-registered texture leaves the driver holding a registration onto freed GL state.
    pub fn release(&mut self) {
        if self.resource.is_null() {
            return;
        }
        // SAFETY: `self.resource` is the exclusive `CUgraphicsResource` from `register_gl`;
        // nulling it after unregister makes Drop a no-op. Context is set current first: teardown
        // may run on a thread where it isn't.
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

/// Dmabuf fd imported as CUDA external memory and mapped to a device pointer. LINEAR path
/// (gamescope): bytes are directly addressable, no GL de-tiling. Cached per PipeWire buffer.
pub struct ExternalDmabuf {
    ext: CUexternalMemory,
    pub ptr: CUdeviceptr,
    pub size: u64,
}

// SAFETY: opaque driver handles, uniquely owned (no `Clone`), destroyed once in `Drop`. Moved
// between threads with the importer; `Send` not `Sync` matches single-thread use.
unsafe impl Send for ExternalDmabuf {}

impl ExternalDmabuf {
    /// Import `fd` without consuming it: a `dup` is handed to the driver. Maps `size` bytes.
    /// Context must be current.
    pub fn import(fd: i32, size: u64) -> Result<ExternalDmabuf> {
        // SAFETY: `dup` reads the integer `fd` (still owned by the caller) and returns a new fd.
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            bail!("dup(dmabuf fd) failed");
        }
        Self::import_owned_fd(dup, size)
    }

    /// Import an fd the caller hands over (Vulkan `OPAQUE_FD`). Driver owns it on success; we
    /// close it on failure.
    pub fn import_owned_fd(dup: i32, size: u64) -> Result<ExternalDmabuf> {
        let mut desc = CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
            type_: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            size,
            ..Default::default()
        };
        desc.handle[0] = dup as u32 as u64; // union member `int fd` (LE low bytes)
        let mut ext: CUexternalMemory = std::ptr::null_mut();
        // SAFETY: `&desc` outlives the call (`OPAQUE_FD`, fd in union `int fd` low bytes, `size`
        // set). `&mut ext` is a live out-param. Driver takes the fd only on success. Context current.
        let r = unsafe { cuImportExternalMemory(&mut ext, &desc) };
        if r != 0 {
            // SAFETY: import failed, so we still own `dup`; close it once. Success never closes it.
            unsafe { libc::close(dup) };
            bail!("cuImportExternalMemory failed ({r}) — LINEAR dmabuf import unsupported?");
        }
        let buf = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
            offset: 0,
            size,
            ..Default::default()
        };
        let mut ptr: CUdeviceptr = 0;
        // SAFETY: `ext` is the just-imported handle. `&buf` (offset 0, full `size`) outlives the
        // call. `&mut ptr` is a live out-param. Context current.
        let r = unsafe { cuExternalMemoryGetMappedBuffer(&mut ptr, ext, &buf) };
        if r != 0 {
            // SAFETY: mapping failed; we exclusively own `ext`. Destroy once here; success moves it
            // into `ExternalDmabuf`, whose `Drop` destroys it.
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
        // SAFETY: exclusive owner of `self.ptr` and `self.ext`, torn down once (`!= 0` / `!null`).
        // Context is set current first: drop may run off the import thread. Free the mapped buffer
        // before destroying its backing external memory.
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            if self.ptr != 0 {
                let _ = cuMemFree_v2(self.ptr); // mapped buffers free like device memory
            }
            if !self.ext.is_null() {
                let _ = cuDestroyExternalMemory(self.ext);
            }
        }
    }
}

/// Vulkan timeline semaphore imported as a CUDA external semaphore. CUDA [`signal`]s a value on
/// this thread's copy stream after the input copy; Vulkan blend waits then advances it; CUDA
/// [`wait`]s that value before encode. One handle per [`VkSlotBlend`](super::vkslot::VkSlotBlend);
/// values monotonic for its life.
pub struct ExternalSemaphore {
    sem: CUexternalSemaphore,
}

// SAFETY: opaque driver handle, uniquely owned, destroyed once in `Drop`. Moved with `VkSlotBlend`;
// `Send` not `Sync` matches single-thread-at-a-time use.
unsafe impl Send for ExternalSemaphore {}

impl ExternalSemaphore {
    /// Import a Vulkan timeline semaphore (`vkGetSemaphoreFdKHR` OPAQUE_FD). Driver owns the fd
    /// on success; we close it on failure. Context must be current.
    pub fn import_owned_timeline_fd(fd: i32) -> Result<ExternalSemaphore> {
        let mut desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
            type_: CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD,
            ..Default::default()
        };
        desc.handle[0] = fd as u32 as u64; // union member `int fd` (LE low bytes)
        let mut sem: CUexternalSemaphore = std::ptr::null_mut();
        // SAFETY: `&desc` outlives the call (`TIMELINE_SEMAPHORE_FD`, fd in union `int fd` low
        // bytes). `&mut sem` is a live out-param. Context current.
        let r = unsafe { cuImportExternalSemaphore(&mut sem, &desc) };
        if r != 0 {
            // SAFETY: import failed, so we still own `fd`; close it once.
            unsafe { libc::close(fd) };
            bail!("cuImportExternalSemaphore failed ({r}) — timeline-semaphore fd export/import unsupported?");
        }
        Ok(ExternalSemaphore { sem })
    }

    /// Enqueue a signal to `value` after prior work on this thread's copy stream. No CPU wait.
    pub fn signal(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: `self.sem` is the live imported handle (destroyed only in `Drop`). `&self.sem`/
        // `&params` outlive the enqueue; driver retains no Rust pointer. This thread's copy stream.
        unsafe {
            ck(
                cuSignalExternalSemaphoresAsync(&self.sem, &params, 1, copy_stream()),
                "cuSignalExternalSemaphoresAsync",
            )
        }
    }

    /// Enqueue a wait: later work on this thread's copy stream runs only once the timeline
    /// reaches `value`. No CPU wait.
    pub fn wait(&self, value: u64) -> Result<()> {
        let params = CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS {
            value,
            ..Default::default()
        };
        // SAFETY: same as `signal` — live handle, live locals across the enqueue, this thread's
        // copy stream.
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
        // SAFETY: exclusive owner, destroyed once. Context is set current first: drop may run off
        // the import thread (`VkSlotBlend` quiesces the GPU first, so no in-flight signal/wait).
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            let _ = cuDestroyExternalSemaphore(self.sem);
        }
    }
}

/// Copy a pitched span at `src_ptr` (e.g. an [`ExternalDmabuf`] mapping) into `dst`. Context
/// must be current.
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
    // SAFETY: caller: context current. `copy` outlives the synchronous call; `src` is the caller's
    // mapped span, `dst` is live; `width*4`×`height` fit both. Sync completes before the dmabuf is
    // requeued.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(ext->dev)") }
}

/// De-stride an NV12 pair from an external mapping into a pooled two-plane [`DeviceBuffer`]: Y
/// (`width` × `height`) and interleaved UV (`width` × ⌈h/2⌉), each from `src_pitch` to the pool
/// pitch. Context must be current.
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
        // W/2 interleaved UV samples × 2 bytes = `width` bytes/row.
        WidthInBytes: dst.width as usize,
        Height: dst.height.div_ceil(2) as usize,
        ..Default::default()
    };
    // SAFETY: caller: context current. Both copies are live locals over the caller's mapping and
    // `dst`'s pooled planes; each `copy_blocking` syncs before return.
    unsafe {
        copy_blocking(&y, "cuMemcpy2DAsync_v2(ext->dev nv12 Y)")?;
        copy_blocking(&uv, "cuMemcpy2DAsync_v2(ext->dev nv12 UV)")
    }
}

// `cuda.h` layouts these calls need are compile-time asserted in `ffi.rs` (`const _`).
