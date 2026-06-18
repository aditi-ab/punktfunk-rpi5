//! Minimal CUDA Driver API FFI for the zero-copy path. No Rust crate exposes the GL-interop
//! driver calls we need (`cuGraphicsGLRegisterImage` & co.), so we hand-roll exactly those and
//! link `libcuda.so.1` (the driver library — NOT `libcudart`). Symbol names verified against
//! `cust_raw` + `cudaGL.h`: the context/mem ops use the `_v2` ABI suffix; the graphics-interop
//! ops are unsuffixed. (We use GL interop, not EGL interop: `cuGraphicsEGLRegisterImage` is
//! Tegra-only on the desktop driver — see [`super::egl`].)
//!
//! One process-wide `CUcontext` is created lazily and shared by the EGL importer (capture
//! thread) and ffmpeg's `hevc_nvenc` (encode thread); each thread makes it current before use.

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{bail, Result};
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};

pub type CUresult = c_uint; // CUDA_SUCCESS == 0
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void; // opaque CUctx_st*
pub type CUstream = *mut c_void; // opaque CUstream_st*
pub type CUdeviceptr = u64;
pub type CUgraphicsResource = *mut c_void;
pub type CUarray = *mut c_void;
pub type CUexternalMemory = *mut c_void; // opaque CUextMemory_st*

/// `CUmemorytype` (cuda.h): HOST=1, DEVICE=2, ARRAY=3, UNIFIED=4.
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;
pub const CU_MEMORYTYPE_ARRAY: c_uint = 3;

/// `CUctx_flags` (cuda.h): block the CPU on an OS primitive while waiting for the GPU instead of
/// busy-spinning. On this shared box (compositor + send thread on the same cores) spinning a core
/// to detect copy completion steals CPU from the very threads we want scheduled; BLOCKING_SYNC
/// frees it. Default (`CU_CTX_SCHED_AUTO=0`) heuristically picks SPIN vs YIELD by core count.
const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x04;

/// `cuStreamCreateWithPriority` flag: don't implicitly synchronize with the legacy NULL stream.
const CU_STREAM_NON_BLOCKING: c_uint = 0x01;

/// `CUDA_MEMCPY2D` (cuda.h, `_v2` ABI). Field order is load-bearing.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_MEMCPY2D {
    pub srcXInBytes: usize,
    pub srcY: usize,
    pub srcMemoryType: c_uint,
    pub srcHost: *const c_void,
    pub srcDevice: CUdeviceptr,
    pub srcArray: CUarray,
    pub srcPitch: usize,
    pub dstXInBytes: usize,
    pub dstY: usize,
    pub dstMemoryType: c_uint,
    pub dstHost: *mut c_void,
    pub dstDevice: CUdeviceptr,
    pub dstArray: CUarray,
    pub dstPitch: usize,
    pub WidthInBytes: usize,
    pub Height: usize,
}

/// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` (cuda.h, 64-bit layout). `handle` is a union whose
/// largest member is the win32 two-pointer struct (16 bytes, align 8); for the OPAQUE_FD type
/// only the first 4 bytes (the `int fd`) are read.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
    pub type_: c_uint, // CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD = 1
    _pad: u32,
    pub handle: [u64; 2], // union { int fd; {void*,void*} win32; void* nvSciBufObject }
    pub size: u64,
    pub flags: c_uint,
    reserved: [c_uint; 16],
    _pad2: u32,
}

/// `CUDA_EXTERNAL_MEMORY_BUFFER_DESC` (cuda.h, 64-bit layout).
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
    pub offset: u64,
    pub size: u64,
    pub flags: c_uint,
    reserved: [c_uint; 16],
    _pad: u32,
}

pub const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_uint = 1;

#[link(name = "cuda")]
extern "C" {
    fn cuInit(flags: c_uint) -> CUresult;
    fn cuDeviceGet(device: *mut CUdevice, ordinal: c_int) -> CUresult;
    fn cuCtxCreate_v2(pctx: *mut CUcontext, flags: c_uint, dev: CUdevice) -> CUresult;
    fn cuCtxSetCurrent(ctx: CUcontext) -> CUresult;
    fn cuMemAllocPitch_v2(
        dptr: *mut CUdeviceptr,
        pitch: *mut usize,
        width_bytes: usize,
        height: usize,
        element_size: c_uint,
    ) -> CUresult;
    fn cuMemFree_v2(dptr: CUdeviceptr) -> CUresult;
    fn cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream) -> CUresult;
    fn cuStreamSynchronize(stream: CUstream) -> CUresult;
    // Greatest/least stream priority the driver exposes (greatest = numerically lowest).
    fn cuCtxGetStreamPriorityRange(least: *mut c_int, greatest: *mut c_int) -> CUresult;
    fn cuStreamCreateWithPriority(
        stream: *mut CUstream,
        flags: c_uint,
        priority: c_int,
    ) -> CUresult;

    // GL interop (cudaGL.h) — these symbols have NO `_v2` suffix. `cuGraphicsEGLRegisterImage`
    // is Tegra-only on the desktop driver, so we go EGLImage → GL texture → register the texture.
    fn cuGraphicsGLRegisterImage(
        resource: *mut CUgraphicsResource,
        texture: c_uint, // GLuint
        target: c_uint,  // GL_TEXTURE_2D = 0x0DE1
        flags: c_uint,   // CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY = 0x01
    ) -> CUresult;
    fn cuGraphicsMapResources(
        count: c_uint,
        resources: *mut CUgraphicsResource,
        stream: *mut c_void,
    ) -> CUresult;
    fn cuGraphicsUnmapResources(
        count: c_uint,
        resources: *mut CUgraphicsResource,
        stream: *mut c_void,
    ) -> CUresult;
    fn cuGraphicsSubResourceGetMappedArray(
        array: *mut CUarray,
        resource: CUgraphicsResource,
        array_index: c_uint,
        mip_level: c_uint,
    ) -> CUresult;
    fn cuGraphicsUnregisterResource(resource: CUgraphicsResource) -> CUresult;

    // External memory (cuda.h, no `_v2` suffix) — imports a (Vulkan-exported) dmabuf fd as
    // device memory. Used for LINEAR dmabufs (gamescope), which EGL/GL interop can't sample.
    fn cuImportExternalMemory(
        ext_mem_out: *mut CUexternalMemory,
        mem_handle_desc: *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
    ) -> CUresult;
    fn cuExternalMemoryGetMappedBuffer(
        dev_ptr: *mut CUdeviceptr,
        ext_mem: CUexternalMemory,
        buffer_desc: *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
    ) -> CUresult;
    fn cuDestroyExternalMemory(ext_mem: CUexternalMemory) -> CUresult;
}

#[inline]
fn ck(r: CUresult, what: &str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        bail!("CUDA driver error {r} in {what}")
    }
}

/// The shared process-wide CUDA context (created once). Wrapped so it's `Send`/`Sync` to live
/// in a `OnceLock`; the raw `CUcontext` is thread-safe to make current from any thread.
#[derive(Clone, Copy)]
pub struct Context(pub CUcontext);
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// Get (lazily creating) the shared CUDA context on device 0.
pub fn context() -> Result<CUcontext> {
    if let Some(c) = CONTEXT.get() {
        return Ok(c.0);
    }
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
    unsafe { ck(cuCtxSetCurrent(ctx), "cuCtxSetCurrent") }
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
    let stream = copy_stream();
    ck(cuMemcpy2DAsync_v2(copy, stream), what)?;
    ck(cuStreamSynchronize(stream), "cuStreamSynchronize")
}

/// Allocate one pitched device buffer for `width`x`height` 4-byte pixels; returns `(ptr, pitch)`.
fn alloc_pitched(width: u32, height: u32) -> Result<(CUdeviceptr, usize)> {
    let mut ptr: CUdeviceptr = 0;
    let mut pitch: usize = 0;
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

/// Free-list of recycled device allocations for one resolution. Shared (via `Arc`) between the
/// capture thread that hands out buffers and the encode thread where a [`DeviceBuffer`] drops and
/// returns its allocation here. Bulk-freed when the last reference drops.
struct PoolInner {
    free: Vec<CUdeviceptr>,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        unsafe {
            if let Some(c) = CONTEXT.get() {
                let _ = cuCtxSetCurrent(c.0);
            }
            for &p in &self.free {
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
}

impl BufferPool {
    /// Create a pool for `width`x`height` 4-byte buffers (allocates one up front to learn the
    /// driver's pitch, which is constant for a given width).
    pub fn new(width: u32, height: u32) -> Result<BufferPool> {
        let (ptr, pitch) = alloc_pitched(width, height)?;
        Ok(BufferPool {
            inner: Arc::new(Mutex::new(PoolInner { free: vec![ptr] })),
            width,
            height,
            pitch,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Take a buffer — recycled if one is free, else freshly allocated. The buffer returns to this
    /// pool when dropped (after the consumer has synchronized, so the GPU is done with it).
    pub fn get(&self) -> Result<DeviceBuffer> {
        let reuse = self.inner.lock().unwrap().free.pop();
        let ptr = match reuse {
            Some(p) => p,
            None => alloc_pitched(self.width, self.height)?.0,
        };
        Ok(DeviceBuffer {
            ptr,
            pitch: self.pitch,
            width: self.width,
            height: self.height,
            pool: Some(self.inner.clone()),
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
    pool: Option<Arc<Mutex<PoolInner>>>,
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
            pool: None,
        })
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if self.ptr == 0 {
            return;
        }
        if let Some(pool) = &self.pool {
            // Recycle (the consumer synchronized before dropping, so the GPU is done with it).
            pool.lock().unwrap().free.push(self.ptr);
        } else {
            // The buffer may be freed on the encode thread; cuMemFree needs a current context.
            unsafe {
                if let Some(c) = CONTEXT.get() {
                    let _ = cuCtxSetCurrent(c.0);
                }
                let _ = cuMemFree_v2(self.ptr);
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

    /// Map the texture for this frame, copy its (already-linear RGBA8) array into `dst`, then
    /// unmap. The copy is synchronized (on our priority stream) before unmap so `dst` is ready
    /// before the source dmabuf is recycled. Always unmaps, even if the copy errors.
    pub fn copy_mapped_to(&mut self, dst: &DeviceBuffer) -> Result<()> {
        unsafe {
            ck(
                cuGraphicsMapResources(1, &mut self.resource, std::ptr::null_mut()),
                "cuGraphicsMapResources",
            )?;
            let mut array: CUarray = std::ptr::null_mut();
            if cuGraphicsSubResourceGetMappedArray(&mut array, self.resource, 0, 0) != 0 {
                let _ = cuGraphicsUnmapResources(1, &mut self.resource, std::ptr::null_mut());
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
            let _ = cuGraphicsUnmapResources(1, &mut self.resource, std::ptr::null_mut());
            res
        }
    }
}

/// Copy a pitched device buffer into another device region (device→device), e.g. our imported
/// [`DeviceBuffer`] into a pooled CUDA surface NVENC owns. Both are 4-byte (BGRx) pixels.
/// The caller must have the shared context current on this thread (see [`make_current`]).
pub fn copy_device_to_device(
    src: &DeviceBuffer,
    dst_ptr: CUdeviceptr,
    dst_pitch: usize,
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
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(dev->dev)") }
}

impl Drop for RegisteredTexture {
    fn drop(&mut self) {
        if !self.resource.is_null() {
            unsafe {
                let _ = cuGraphicsUnregisterResource(self.resource);
            }
        }
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

// Raw driver handles; used from the single capture thread but moved with the importer.
unsafe impl Send for ExternalDmabuf {}

impl ExternalDmabuf {
    /// Import `fd` (NOT consumed — an internal `dup` is handed to the driver, which owns it
    /// from then on) and map its full `size` bytes to a device pointer. The shared context
    /// must be current.
    pub fn import(fd: i32, size: u64) -> Result<ExternalDmabuf> {
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
        let r = unsafe { cuImportExternalMemory(&mut ext, &desc) };
        if r != 0 {
            unsafe { libc::close(dup) }; // import failed → the driver did not take the fd
            bail!("cuImportExternalMemory failed ({r}) — LINEAR dmabuf import unsupported?");
        }
        let buf = CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
            offset: 0,
            size,
            ..Default::default()
        };
        let mut ptr: CUdeviceptr = 0;
        let r = unsafe { cuExternalMemoryGetMappedBuffer(&mut ptr, ext, &buf) };
        if r != 0 {
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
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(ext->dev)") }
}
