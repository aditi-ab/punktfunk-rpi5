//! Minimal CUDA Driver API FFI for the zero-copy path. No Rust crate exposes the EGL-interop
//! driver calls (`cuGraphicsEGLRegisterImage` / `cuGraphicsResourceGetMappedEglFrame`) nor
//! `CUeglFrame`, so we hand-roll exactly what we need and link `libcuda.so.1` (the driver
//! library — NOT `libcudart`). Symbol names verified against `cust_raw` + `cudaEGL.h`: the
//! context/mem ops use the `_v2` ABI suffix; the graphics/EGL-interop ops are unsuffixed.
//!
//! One process-wide `CUcontext` is created lazily and shared by the EGL importer (capture
//! thread) and ffmpeg's `hevc_nvenc` (encode thread); each thread makes it current before use.

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{bail, Result};
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::OnceLock;

pub type CUresult = c_uint; // CUDA_SUCCESS == 0
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void; // opaque CUctx_st*
pub type CUstream = *mut c_void; // opaque CUstream_st*
pub type CUdeviceptr = u64;
pub type CUgraphicsResource = *mut c_void;
pub type CUarray = *mut c_void;

/// `CUmemorytype` (cuda.h): HOST=1, DEVICE=2, ARRAY=3, UNIFIED=4.
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;
pub const CU_MEMORYTYPE_ARRAY: c_uint = 3;

/// `CUeglFrameType`: ARRAY=0, PITCH=1.
pub const CU_EGL_FRAME_TYPE_ARRAY: c_uint = 0;
pub const CU_EGL_FRAME_TYPE_PITCH: c_uint = 1;

/// `CUeglFrame` — exact layout from `cudaEGL.h`. `frame` is a union of `CUarray pArray[3]` and
/// `void* pPitch[3]`; both are three pointers, so `[*mut c_void; 3]` models it.
#[repr(C)]
pub struct CUeglFrame {
    pub frame: [*mut c_void; 3],
    pub width: c_uint,
    pub height: c_uint,
    pub depth: c_uint,
    pub pitch: c_uint,
    pub planeCount: c_uint,
    pub numChannels: c_uint,
    pub frameType: c_uint,
    pub eglColorFormat: c_uint,
    pub cuFormat: c_uint,
}

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

impl Default for CUeglFrame {
    fn default() -> Self {
        // SAFETY: all fields are integers or pointers; zero is a valid bit pattern.
        unsafe { std::mem::zeroed() }
    }
}

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
    fn cuMemcpy2D_v2(copy: *const CUDA_MEMCPY2D) -> CUresult;
    fn cuCtxSynchronize() -> CUresult;

    fn cuGraphicsEGLRegisterImage(
        resource: *mut CUgraphicsResource,
        image: *mut c_void, // EGLImage
        flags: c_uint,      // CU_GRAPHICS_REGISTER_FLAGS_NONE = 0
    ) -> CUresult;
    fn cuGraphicsResourceGetMappedEglFrame(
        egl_frame: *mut CUeglFrame,
        resource: CUgraphicsResource,
        index: c_uint,
        mip_level: c_uint,
    ) -> CUresult;
    fn cuGraphicsUnregisterResource(resource: CUgraphicsResource) -> CUresult;
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
        ck(cuCtxCreate_v2(&mut ctx, 0, dev), "cuCtxCreate_v2")?;
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

/// A device buffer we own (pitched), freed on drop. Used as the zero-copy frame the encoder
/// reads — filled by a device-to-device copy from the EGL-mapped dmabuf so the dmabuf can be
/// returned to the compositor immediately.
pub struct DeviceBuffer {
    pub ptr: CUdeviceptr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
}

impl DeviceBuffer {
    /// Allocate a pitched device buffer for `width`x`height` 4-byte (BGRA) pixels.
    pub fn alloc(width: u32, height: u32) -> Result<DeviceBuffer> {
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
        Ok(DeviceBuffer {
            ptr,
            pitch,
            width,
            height,
        })
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if self.ptr != 0 {
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

/// A live EGL→CUDA registration. The mapped device memory aliases the dmabuf, so we copy out of
/// it immediately and then unregister (the EGL image is destroyed by the caller).
pub struct MappedImage {
    resource: CUgraphicsResource,
    /// `frameType` (ARRAY vs PITCH) determines how to copy out.
    frame: CUeglFrame,
}

impl MappedImage {
    /// Register an `EGLImage` with CUDA and map it to a `CUeglFrame`.
    ///
    /// # Safety
    /// `image` must be a valid `EGLImage`; the shared context must be current on this thread.
    pub unsafe fn register(image: *mut c_void) -> Result<MappedImage> {
        // CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY (0x01): we only read the surface (encode from it).
        let mut resource: CUgraphicsResource = std::ptr::null_mut();
        ck(
            cuGraphicsEGLRegisterImage(&mut resource, image, 0x01),
            "cuGraphicsEGLRegisterImage",
        )?;
        let mut frame = CUeglFrame::default();
        let r = cuGraphicsResourceGetMappedEglFrame(&mut frame, resource, 0, 0);
        if r != 0 {
            let _ = cuGraphicsUnregisterResource(resource);
            bail!("cuGraphicsResourceGetMappedEglFrame error {r}");
        }
        Ok(MappedImage { resource, frame })
    }

    /// Device-to-device copy of this mapped frame into `dst` (de-tiling if the source is a tiled
    /// CUarray). After this returns the dmabuf is no longer needed.
    pub fn copy_to(&self, dst: &DeviceBuffer) -> Result<()> {
        let width_bytes = (self.frame.width as usize).min(dst.width as usize) * 4;
        let height = (self.frame.height as usize).min(dst.height as usize);
        let mut copy = CUDA_MEMCPY2D {
            dstMemoryType: CU_MEMORYTYPE_DEVICE,
            dstDevice: dst.ptr,
            dstPitch: dst.pitch,
            WidthInBytes: width_bytes,
            Height: height,
            ..Default::default()
        };
        match self.frame.frameType {
            CU_EGL_FRAME_TYPE_PITCH => {
                copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
                copy.srcDevice = self.frame.frame[0] as CUdeviceptr;
                copy.srcPitch = self.frame.pitch as usize;
            }
            CU_EGL_FRAME_TYPE_ARRAY => {
                copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
                copy.srcArray = self.frame.frame[0] as CUarray;
            }
            other => bail!("unexpected CUeglFrame frameType {other}"),
        }
        unsafe {
            ck(cuMemcpy2D_v2(&copy), "cuMemcpy2D_v2")?;
            // The copy must complete before the dmabuf is requeued / reused.
            ck(cuCtxSynchronize(), "cuCtxSynchronize")?;
        }
        Ok(())
    }

    pub fn color_format(&self) -> c_uint {
        self.frame.eglColorFormat
    }
    pub fn frame_kind(&self) -> c_uint {
        self.frame.frameType
    }
}

impl Drop for MappedImage {
    fn drop(&mut self) {
        if !self.resource.is_null() {
            unsafe {
                let _ = cuGraphicsUnregisterResource(self.resource);
            }
        }
    }
}
