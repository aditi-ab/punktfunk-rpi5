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

/// A live GL-texture→CUDA registration (mapped). The CUDA array aliases the texture/dmabuf, so
/// we copy out of it immediately; unmap + unregister happen on drop.
pub struct MappedTexture {
    resource: CUgraphicsResource,
    array: CUarray,
}

impl MappedTexture {
    /// Register a `GL_TEXTURE_2D` texture with CUDA, map it, and get its array. The desktop
    /// NVIDIA driver only supports CUDA interop through GL textures (not dmabuf EGLImages
    /// directly), so the EGLImage is first bound to a GL texture by the caller.
    ///
    /// # Safety
    /// The GL context and the shared CUDA context must both be current on this thread, and
    /// `texture` must be a valid `GL_TEXTURE_2D` bound to the source image.
    pub unsafe fn register_gl(texture: u32) -> Result<MappedTexture> {
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
        if cuGraphicsMapResources(1, &mut resource, std::ptr::null_mut()) != 0 {
            let _ = cuGraphicsUnregisterResource(resource);
            bail!("cuGraphicsMapResources failed");
        }
        let mut array: CUarray = std::ptr::null_mut();
        if cuGraphicsSubResourceGetMappedArray(&mut array, resource, 0, 0) != 0 {
            let _ = cuGraphicsUnmapResources(1, &mut resource, std::ptr::null_mut());
            let _ = cuGraphicsUnregisterResource(resource);
            bail!("cuGraphicsSubResourceGetMappedArray failed");
        }
        Ok(MappedTexture { resource, array })
    }

    /// Copy the mapped array into `dst` (array → pitched device memory). The array is the GL
    /// blit's already-linear RGBA8 output, so this is a straight copy. After it returns the
    /// source dmabuf is no longer needed.
    pub fn copy_to(&self, dst: &DeviceBuffer) -> Result<()> {
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: CU_MEMORYTYPE_ARRAY,
            srcArray: self.array,
            dstMemoryType: CU_MEMORYTYPE_DEVICE,
            dstDevice: dst.ptr,
            dstPitch: dst.pitch,
            WidthInBytes: dst.width as usize * 4, // 4 bytes/px (BGRx)
            Height: dst.height as usize,
            ..Default::default()
        };
        unsafe {
            ck(cuMemcpy2D_v2(&copy), "cuMemcpy2D_v2")?;
            // The copy must complete before the dmabuf is requeued / reused.
            ck(cuCtxSynchronize(), "cuCtxSynchronize")?;
        }
        Ok(())
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
    unsafe {
        ck(cuMemcpy2D_v2(&copy), "cuMemcpy2D_v2(dev->dev)")?;
        ck(cuCtxSynchronize(), "cuCtxSynchronize")?;
    }
    Ok(())
}

impl Drop for MappedTexture {
    fn drop(&mut self) {
        if !self.resource.is_null() {
            unsafe {
                let _ = cuGraphicsUnmapResources(1, &mut self.resource, std::ptr::null_mut());
                let _ = cuGraphicsUnregisterResource(self.resource);
            }
        }
    }
}
