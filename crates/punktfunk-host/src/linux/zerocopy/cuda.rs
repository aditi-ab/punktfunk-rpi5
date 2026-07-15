//! Minimal CUDA Driver API FFI for the zero-copy path. No Rust crate exposes the GL-interop
//! driver calls we need (`cuGraphicsGLRegisterImage` & co.), so we hand-roll exactly those and
//! `dlopen` `libcuda.so.1` at runtime (the driver library — NOT `libcudart`; NOT a link-time
//! `#[link]`, so one binary runs on NVIDIA and on AMD/Intel where `libcuda` is absent — see
//! [`CudaApi`]). Symbol names verified against
//! `cust_raw` + `cudaGL.h`: the context/mem ops use the `_v2` ABI suffix; the graphics-interop
//! ops are unsuffixed. (We use GL interop, not EGL interop: `cuGraphicsEGLRegisterImage` is
//! Tegra-only on the desktop driver — see [`super::egl`].)
//!
//! One process-wide `CUcontext` is created lazily and shared by the EGL importer (capture
//! thread) and ffmpeg's `hevc_nvenc` (encode thread); each thread makes it current before use.

#![allow(non_camel_case_types, non_snake_case)]
// Every `unsafe` block/impl below carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{bail, Result};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock};

pub type CUresult = c_uint; // CUDA_SUCCESS == 0
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void; // opaque CUctx_st*
pub type CUstream = *mut c_void; // opaque CUstream_st*
pub type CUdeviceptr = u64;
pub type CUgraphicsResource = *mut c_void;
pub type CUarray = *mut c_void;
pub type CUexternalMemory = *mut c_void; // opaque CUextMemory_st*
pub type CUmodule = *mut c_void; // opaque CUmod_st*
pub type CUfunction = *mut c_void; // opaque CUfunc_st*

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

/// `CUipcMemHandle` (cuda.h): an opaque 64-byte struct identifying a device allocation across
/// processes. Produced by `cuIpcGetMemHandle` in the exporting process, consumed by
/// `cuIpcOpenMemHandle` in the importer — passed **by value**, matching the C
/// `struct { char reserved[64]; }`. Plain bytes — safe to ship over a socket.
pub const CU_IPC_HANDLE_SIZE: usize = 64;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUipcMemHandle {
    pub reserved: [u8; CU_IPC_HANDLE_SIZE],
}

/// `CUipcMem_flags`: lazily enable peer access on open (the documented flag for
/// `cuIpcOpenMemHandle`; a no-op for a same-device open, which is our only case).
const CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS: c_uint = 0x1;

/// CUDA Driver API entry points, resolved at runtime from `libcuda.so.1` via `dlopen` rather than
/// a link-time `#[link(name = "cuda")]`. This is what lets ONE host binary run on NVIDIA
/// (zero-copy via CUDA → NVENC) *and* on AMD/Intel (VAAPI, where the NVIDIA driver — and thus
/// `libcuda` — is absent): with a hard link the loader would refuse to start the binary at all.
/// Every `cu*` call below goes through a same-named wrapper fn that forwards to this table; when
/// the driver isn't present the table is `None` and the wrappers return a non-zero `CUresult`, so
/// `context()` fails cleanly and the capturer falls back to the CPU path. The `cuda_api()` loader
/// is memoised; the library handle is intentionally leaked (process-lifetime, like the context).
struct CudaApi {
    cuInit: unsafe extern "C" fn(c_uint) -> CUresult,
    cuDeviceGet: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    cuCtxCreate_v2: unsafe extern "C" fn(*mut CUcontext, c_uint, CUdevice) -> CUresult,
    cuCtxSetCurrent: unsafe extern "C" fn(CUcontext) -> CUresult,
    cuMemAllocPitch_v2:
        unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult,
    cuMemFree_v2: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
    cuMemcpy2DAsync_v2: unsafe extern "C" fn(*const CUDA_MEMCPY2D, CUstream) -> CUresult,
    cuStreamSynchronize: unsafe extern "C" fn(CUstream) -> CUresult,
    cuCtxGetStreamPriorityRange: unsafe extern "C" fn(*mut c_int, *mut c_int) -> CUresult,
    cuStreamCreateWithPriority: unsafe extern "C" fn(*mut CUstream, c_uint, c_int) -> CUresult,
    cuGraphicsGLRegisterImage:
        unsafe extern "C" fn(*mut CUgraphicsResource, c_uint, c_uint, c_uint) -> CUresult,
    cuGraphicsMapResources:
        unsafe extern "C" fn(c_uint, *mut CUgraphicsResource, *mut c_void) -> CUresult,
    cuGraphicsUnmapResources:
        unsafe extern "C" fn(c_uint, *mut CUgraphicsResource, *mut c_void) -> CUresult,
    cuGraphicsSubResourceGetMappedArray:
        unsafe extern "C" fn(*mut CUarray, CUgraphicsResource, c_uint, c_uint) -> CUresult,
    cuGraphicsUnregisterResource: unsafe extern "C" fn(CUgraphicsResource) -> CUresult,
    cuImportExternalMemory: unsafe extern "C" fn(
        *mut CUexternalMemory,
        *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
    ) -> CUresult,
    cuExternalMemoryGetMappedBuffer: unsafe extern "C" fn(
        *mut CUdeviceptr,
        CUexternalMemory,
        *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
    ) -> CUresult,
    cuDestroyExternalMemory: unsafe extern "C" fn(CUexternalMemory) -> CUresult,
    cuIpcGetMemHandle: unsafe extern "C" fn(*mut CUipcMemHandle, CUdeviceptr) -> CUresult,
    cuIpcOpenMemHandle: unsafe extern "C" fn(*mut CUdeviceptr, CUipcMemHandle, c_uint) -> CUresult,
    cuIpcCloseMemHandle: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
    // Cursor-overlay blend: a linear device alloc + a PTX module with the blend kernels launched
    // over the cursor's small rectangle (see [`CursorBlend`]).
    cuMemAlloc_v2: unsafe extern "C" fn(*mut CUdeviceptr, usize) -> CUresult,
    cuModuleLoadData: unsafe extern "C" fn(*mut CUmodule, *const c_void) -> CUresult,
    cuModuleUnload: unsafe extern "C" fn(CUmodule) -> CUresult,
    cuModuleGetFunction: unsafe extern "C" fn(*mut CUfunction, CUmodule, *const c_char) -> CUresult,
    #[allow(clippy::type_complexity)]
    cuLaunchKernel: unsafe extern "C" fn(
        CUfunction,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        CUstream,
        *mut *mut c_void,
        *mut *mut c_void,
    ) -> CUresult,
}
// SAFETY: every field is a bare `extern "C" fn` address into the leaked, process-lifetime
// `libcuda` mapping (`cuda_api` `forget`s the `Library`, so it is never unloaded) — an immutable
// value with no interior mutability and no thread affinity. Moving the table to another thread
// cannot dangle (the code it points at stays mapped) or race (the fields are read-only).
unsafe impl Send for CudaApi {}
// SAFETY: as above — the table is a set of immutable fn-pointer addresses with no interior
// mutability, so concurrent shared reads from multiple threads cannot race; the driver entry
// points they address are themselves thread-safe.
unsafe impl Sync for CudaApi {}

/// `CUresult` returned by the wrappers when `libcuda` isn't loaded (no NVIDIA driver). Non-zero so
/// the existing `ck()`/`!= 0` checks treat it as an ordinary driver error; distinct from any real
/// `CUDA_ERROR_*` (all < 1000). Never produced by the actual driver.
const CU_ERROR_NOT_LOADED: CUresult = 999;

static CUDA_API: OnceLock<Option<CudaApi>> = OnceLock::new();

/// Resolve `libcuda.so.1` and its symbols once. `None` when the NVIDIA driver isn't installed
/// (the expected case on AMD/Intel hosts) — logged at debug, not an error.
fn cuda_api() -> Option<&'static CudaApi> {
    CUDA_API
        // SAFETY: `Library::new` runs `libcuda.so.1`'s initializers — it is the trusted NVIDIA
        // driver library, so loading has no unexpected effects; `?`/`None` handle its absence.
        // Each `lib.get::<T>(name)` asserts the symbol's real ABI equals `T`: every NUL-terminated
        // name is a documented CUDA Driver API entry point and `T` is the exact
        // `unsafe extern "C" fn(..)` signature from cuda.h/cudaGL.h (`_v2` for ctx/mem ops). Each
        // `Symbol` only borrows `lib` until the end of the struct-literal statement; we deref-copy
        // the raw fn-pointer out first, then `forget(lib)` leaks the mapping so those addresses
        // stay valid for the whole process. Runs once under the `OnceLock` init — no aliasing.
        .get_or_init(|| unsafe {
            let lib = libloading::Library::new("libcuda.so.1")
                .or_else(|_| libloading::Library::new("libcuda.so"))
                .map_err(|e| {
                    tracing::debug!(error = %e, "libcuda not loadable — CUDA zero-copy unavailable (expected on AMD/Intel)");
                })
                .ok()?;
            // Resolve all symbols; the field types drive `get`'s inference. `lib` is leaked after
            // construction so the fn pointers stay valid for the process lifetime (the temporary
            // `Symbol` borrows end with the struct-literal statement, before the forget).
            let api = CudaApi {
                cuInit: *lib.get(b"cuInit\0").ok()?,
                cuDeviceGet: *lib.get(b"cuDeviceGet\0").ok()?,
                cuCtxCreate_v2: *lib.get(b"cuCtxCreate_v2\0").ok()?,
                cuCtxSetCurrent: *lib.get(b"cuCtxSetCurrent\0").ok()?,
                cuMemAllocPitch_v2: *lib.get(b"cuMemAllocPitch_v2\0").ok()?,
                cuMemFree_v2: *lib.get(b"cuMemFree_v2\0").ok()?,
                cuMemcpy2DAsync_v2: *lib.get(b"cuMemcpy2DAsync_v2\0").ok()?,
                cuStreamSynchronize: *lib.get(b"cuStreamSynchronize\0").ok()?,
                cuCtxGetStreamPriorityRange: *lib.get(b"cuCtxGetStreamPriorityRange\0").ok()?,
                cuStreamCreateWithPriority: *lib.get(b"cuStreamCreateWithPriority\0").ok()?,
                cuGraphicsGLRegisterImage: *lib.get(b"cuGraphicsGLRegisterImage\0").ok()?,
                cuGraphicsMapResources: *lib.get(b"cuGraphicsMapResources\0").ok()?,
                cuGraphicsUnmapResources: *lib.get(b"cuGraphicsUnmapResources\0").ok()?,
                cuGraphicsSubResourceGetMappedArray: *lib
                    .get(b"cuGraphicsSubResourceGetMappedArray\0")
                    .ok()?,
                cuGraphicsUnregisterResource: *lib.get(b"cuGraphicsUnregisterResource\0").ok()?,
                cuImportExternalMemory: *lib.get(b"cuImportExternalMemory\0").ok()?,
                cuExternalMemoryGetMappedBuffer: *lib
                    .get(b"cuExternalMemoryGetMappedBuffer\0")
                    .ok()?,
                cuDestroyExternalMemory: *lib.get(b"cuDestroyExternalMemory\0").ok()?,
                cuIpcGetMemHandle: *lib.get(b"cuIpcGetMemHandle\0").ok()?,
                // CUDA 11 renamed the entry point (per-thread-stream ABI split); every modern
                // driver exports `_v2`, but accept the unsuffixed one too (same signature).
                cuIpcOpenMemHandle: *lib
                    .get(b"cuIpcOpenMemHandle_v2\0")
                    .or_else(|_| lib.get(b"cuIpcOpenMemHandle\0"))
                    .ok()?,
                cuIpcCloseMemHandle: *lib.get(b"cuIpcCloseMemHandle\0").ok()?,
                cuMemAlloc_v2: *lib.get(b"cuMemAlloc_v2\0").ok()?,
                cuModuleLoadData: *lib.get(b"cuModuleLoadData\0").ok()?,
                cuModuleUnload: *lib.get(b"cuModuleUnload\0").ok()?,
                cuModuleGetFunction: *lib.get(b"cuModuleGetFunction\0").ok()?,
                cuLaunchKernel: *lib.get(b"cuLaunchKernel\0").ok()?,
            };
            std::mem::forget(lib); // keep libcuda mapped for the fn pointers' lifetime (process)
            Some(api)
        })
        .as_ref()
}

// Same-named wrappers so the call sites below are unchanged. Each forwards through the dlopen'd
// table, or returns `CU_ERROR_NOT_LOADED` when the driver is absent (AMD/Intel) — which the
// `CUresult` checks already handle. Only `context()` is reachable before the driver is confirmed
// present; every other entry runs after `context()` succeeded, so its wrapper always hits `Some`.
unsafe fn cuInit(flags: c_uint) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuInit)(flags),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuDeviceGet(device: *mut CUdevice, ordinal: c_int) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuDeviceGet)(device, ordinal),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuCtxCreate_v2(pctx: *mut CUcontext, flags: c_uint, dev: CUdevice) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxCreate_v2)(pctx, flags, dev),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuCtxSetCurrent(ctx: CUcontext) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxSetCurrent)(ctx),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMemAllocPitch_v2(
    dptr: *mut CUdeviceptr,
    pitch: *mut usize,
    width_bytes: usize,
    height: usize,
    element_size: c_uint,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemAllocPitch_v2)(dptr, pitch, width_bytes, height, element_size),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMemFree_v2(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemFree_v2)(dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMemAlloc_v2(dptr: *mut CUdeviceptr, size: usize) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemAlloc_v2)(dptr, size),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuModuleLoadData(m: *mut CUmodule, image: *const c_void) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleLoadData)(m, image),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuModuleUnload(m: CUmodule) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleUnload)(m),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuModuleGetFunction(f: *mut CUfunction, m: CUmodule, name: *const c_char) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleGetFunction)(f, m, name),
        None => CU_ERROR_NOT_LOADED,
    }
}
#[allow(clippy::too_many_arguments)]
unsafe fn cuLaunchKernel(
    f: CUfunction,
    gx: c_uint,
    gy: c_uint,
    gz: c_uint,
    bx: c_uint,
    by: c_uint,
    bz: c_uint,
    shmem: c_uint,
    stream: CUstream,
    params: *mut *mut c_void,
    extra: *mut *mut c_void,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuLaunchKernel)(f, gx, gy, gz, bx, by, bz, shmem, stream, params, extra),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemcpy2DAsync_v2)(copy, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuStreamSynchronize(stream: CUstream) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuStreamSynchronize)(stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuCtxGetStreamPriorityRange(least: *mut c_int, greatest: *mut c_int) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxGetStreamPriorityRange)(least, greatest),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuStreamCreateWithPriority(
    stream: *mut CUstream,
    flags: c_uint,
    priority: c_int,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuStreamCreateWithPriority)(stream, flags, priority),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuGraphicsGLRegisterImage(
    resource: *mut CUgraphicsResource,
    texture: c_uint,
    target: c_uint,
    flags: c_uint,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsGLRegisterImage)(resource, texture, target, flags),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuGraphicsMapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsMapResources)(count, resources, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuGraphicsUnmapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsUnmapResources)(count, resources, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuGraphicsSubResourceGetMappedArray(
    array: *mut CUarray,
    resource: CUgraphicsResource,
    array_index: c_uint,
    mip_level: c_uint,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsSubResourceGetMappedArray)(array, resource, array_index, mip_level),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuGraphicsUnregisterResource(resource: CUgraphicsResource) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsUnregisterResource)(resource),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuImportExternalMemory(
    ext_mem_out: *mut CUexternalMemory,
    mem_handle_desc: *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuImportExternalMemory)(ext_mem_out, mem_handle_desc),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuExternalMemoryGetMappedBuffer(
    dev_ptr: *mut CUdeviceptr,
    ext_mem: CUexternalMemory,
    buffer_desc: *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuExternalMemoryGetMappedBuffer)(dev_ptr, ext_mem, buffer_desc),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuDestroyExternalMemory(ext_mem: CUexternalMemory) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuDestroyExternalMemory)(ext_mem),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuIpcGetMemHandle(handle: *mut CUipcMemHandle, dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcGetMemHandle)(handle, dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuIpcOpenMemHandle(
    dptr: *mut CUdeviceptr,
    handle: CUipcMemHandle,
    flags: c_uint,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcOpenMemHandle)(dptr, handle, flags),
        None => CU_ERROR_NOT_LOADED,
    }
}
unsafe fn cuIpcCloseMemHandle(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcCloseMemHandle)(dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}

#[inline]
fn ck(r: CUresult, what: &str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        bail!("CUDA driver error {r} in {what}")
    }
}

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
    let stream = copy_stream();
    ck(cuMemcpy2DAsync_v2(copy, stream), what)?;
    ck(cuStreamSynchronize(stream), "cuStreamSynchronize")
}

/// Max cursor-overlay bitmap edge (px) uploaded to the device blend buffer — matches the Vulkan path.
pub const CURSOR_MAX: u32 = 256;

/// GPU cursor-overlay compositor for the NVENC path (cursor-as-metadata): loads the `cursor_blend`
/// PTX module once and blends a straight-alpha RGBA cursor into an encoder-OWNED NVENC input surface
/// (ARGB / NV12 / YUV444) with a small kernel launched over the cursor's rectangle — no full-frame
/// pass, and the compositor's dmabuf is never touched. The cursor bitmap lives in a device buffer
/// re-uploaded only when it changes. Requires `context()` to have succeeded (driver present).
pub struct CursorBlend {
    module: CUmodule,
    f_argb: CUfunction,
    f_nv12: CUfunction,
    f_yuv444: CUfunction,
    cur_buf: CUdeviceptr, // device RGBA staging (CURSOR_MAX²·4, tight rows)
}

// SAFETY: process-lifetime driver handles used only from the encode thread with the shared context
// current — like [`DeviceBuffer`], moving the struct between threads cannot dangle or race.
unsafe impl Send for CursorBlend {}

impl CursorBlend {
    /// Load the embedded PTX image and resolve the three blend kernels + a device cursor buffer.
    pub fn new(ptx: &[u8]) -> Result<CursorBlend> {
        // cuModuleLoadData reads a PTX image as a NUL-terminated string; the embedded .ptx is not,
        // so append a terminator.
        let mut image = ptx.to_vec();
        image.push(0);
        let mut module: CUmodule = std::ptr::null_mut();
        // SAFETY: `&mut module` is a live out-param the driver fills; `image` is a NUL-terminated PTX
        // byte image that outlives the synchronous load. `ck` bails on error before `module` is used.
        unsafe {
            ck(
                cuModuleLoadData(&mut module, image.as_ptr() as *const c_void),
                "cuModuleLoadData(cursor_blend)",
            )?;
        }
        // SAFETY: `module` loaded above; each name is a valid NUL-terminated symbol present in the
        // module (verified in the .ptx `.entry` list); `&mut f` is a live out-param.
        let getf = |name: &CStr| -> Result<CUfunction> {
            let mut f: CUfunction = std::ptr::null_mut();
            unsafe {
                ck(
                    cuModuleGetFunction(&mut f, module, name.as_ptr()),
                    "cuModuleGetFunction",
                )?;
            }
            Ok(f)
        };
        let f_argb = getf(c"blend_argb")?;
        let f_nv12 = getf(c"blend_nv12")?;
        let f_yuv444 = getf(c"blend_yuv444")?;
        let mut cur_buf: CUdeviceptr = 0;
        // SAFETY: `&mut cur_buf` is a live out-param; the size fits the CURSOR_MAX² RGBA buffer.
        unsafe {
            ck(
                cuMemAlloc_v2(&mut cur_buf, (CURSOR_MAX * CURSOR_MAX * 4) as usize),
                "cuMemAlloc(cursor)",
            )?;
        }
        Ok(CursorBlend {
            module,
            f_argb,
            f_nv12,
            f_yuv444,
            cur_buf,
        })
    }

    /// Upload the cursor RGBA (`cw*ch*4`, tight rows) into the device blend buffer. Call only when
    /// the bitmap changes; position moves are just kernel args.
    pub fn upload(&self, rgba: &[u8], cw: u32, ch: u32) -> Result<()> {
        let cw = cw.min(CURSOR_MAX);
        let ch = ch.min(CURSOR_MAX);
        let row = cw as usize * 4;
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: 1, // HOST
            srcHost: rgba.as_ptr() as *const c_void,
            srcPitch: row,
            dstMemoryType: CU_MEMORYTYPE_DEVICE,
            dstDevice: self.cur_buf,
            dstPitch: row,
            WidthInBytes: row,
            Height: ch as usize,
            ..Default::default()
        };
        // SAFETY: HOST→DEVICE 2D copy of `row*ch` bytes; `rgba` covers at least that (caller passes
        // `cw*ch*4`), `cur_buf` is the CURSOR_MAX²·4 device alloc (row ≤ CURSOR_MAX·4, ch ≤ CURSOR_MAX).
        // Synchronous via `copy_blocking`. Requires the context current (caller's contract).
        unsafe { copy_blocking(&copy, "cursor HtoD") }
    }

    /// Blend into a packed 4-byte (NVENC ARGB) owned surface at `(ox,oy)`.
    pub fn blend_argb(
        &self,
        surf: CUdeviceptr,
        pitch: usize,
        w: u32,
        h: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        let (mut a_surf, mut a_cur) = (surf, self.cur_buf);
        let (mut a_pitch, mut a_w, mut a_h) = (pitch as i32, w as i32, h as i32);
        let (mut a_cw, mut a_ch) = (cw.min(CURSOR_MAX) as i32, ch.min(CURSOR_MAX) as i32);
        let (mut a_ox, mut a_oy) = (ox, oy);
        let mut args: [*mut c_void; 9] = [
            &mut a_surf as *mut _ as *mut c_void,
            &mut a_pitch as *mut _ as *mut c_void,
            &mut a_w as *mut _ as *mut c_void,
            &mut a_h as *mut _ as *mut c_void,
            &mut a_cur as *mut _ as *mut c_void,
            &mut a_cw as *mut _ as *mut c_void,
            &mut a_ch as *mut _ as *mut c_void,
            &mut a_ox as *mut _ as *mut c_void,
            &mut a_oy as *mut _ as *mut c_void,
        ];
        self.launch(self.f_argb, a_cw as u32, a_ch as u32, &mut args)
    }

    /// Blend into an owned planar YUV444 surface (3 stacked full-res planes) at `(ox,oy)`.
    pub fn blend_yuv444(
        &self,
        base: CUdeviceptr,
        pitch: usize,
        w: u32,
        h: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        let (mut a_base, mut a_cur) = (base, self.cur_buf);
        let (mut a_pitch, mut a_w, mut a_h) = (pitch as i32, w as i32, h as i32);
        let (mut a_cw, mut a_ch) = (cw.min(CURSOR_MAX) as i32, ch.min(CURSOR_MAX) as i32);
        let (mut a_ox, mut a_oy) = (ox, oy);
        let mut args: [*mut c_void; 9] = [
            &mut a_base as *mut _ as *mut c_void,
            &mut a_pitch as *mut _ as *mut c_void,
            &mut a_w as *mut _ as *mut c_void,
            &mut a_h as *mut _ as *mut c_void,
            &mut a_cur as *mut _ as *mut c_void,
            &mut a_cw as *mut _ as *mut c_void,
            &mut a_ch as *mut _ as *mut c_void,
            &mut a_ox as *mut _ as *mut c_void,
            &mut a_oy as *mut _ as *mut c_void,
        ];
        self.launch(self.f_yuv444, a_cw as u32, a_ch as u32, &mut args)
    }

    /// Blend into an owned NV12 surface (Y plane at `base`, interleaved UV at `base + pitch*h`).
    pub fn blend_nv12(
        &self,
        base: CUdeviceptr,
        pitch: usize,
        w: u32,
        h: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        let (mut a_yb, mut a_uvb, mut a_cur) = (base, base + pitch as u64 * h as u64, self.cur_buf);
        let (mut a_yp, mut a_uvp) = (pitch as i32, pitch as i32);
        let (mut a_w, mut a_h) = (w as i32, h as i32);
        let (mut a_cw, mut a_ch) = (cw.min(CURSOR_MAX) as i32, ch.min(CURSOR_MAX) as i32);
        let (mut a_ox, mut a_oy) = (ox, oy);
        let mut args: [*mut c_void; 11] = [
            &mut a_yb as *mut _ as *mut c_void,
            &mut a_yp as *mut _ as *mut c_void,
            &mut a_uvb as *mut _ as *mut c_void,
            &mut a_uvp as *mut _ as *mut c_void,
            &mut a_w as *mut _ as *mut c_void,
            &mut a_h as *mut _ as *mut c_void,
            &mut a_cur as *mut _ as *mut c_void,
            &mut a_cw as *mut _ as *mut c_void,
            &mut a_ch as *mut _ as *mut c_void,
            &mut a_ox as *mut _ as *mut c_void,
            &mut a_oy as *mut _ as *mut c_void,
        ];
        // One thread per 2x2 luma block → grid over ceil(cw/2) × ceil(ch/2).
        self.launch(
            self.f_nv12,
            (a_cw as u32).div_ceil(2),
            (a_ch as u32).div_ceil(2),
            &mut args,
        )
    }

    /// Launch `f` over a `work_w × work_h` grid (16×16 blocks) on the copy stream, then synchronize.
    fn launch(
        &self,
        f: CUfunction,
        work_w: u32,
        work_h: u32,
        args: &mut [*mut c_void],
    ) -> Result<()> {
        if work_w == 0 || work_h == 0 {
            return Ok(());
        }
        const B: u32 = 16;
        let stream = copy_stream();
        // SAFETY: `f` is a resolved kernel from our loaded module; `args` holds pointers to live
        // locals whose types match the kernel's C parameters (per the call site above); grid/block
        // dims are non-zero. Launched on the copy stream (ordered after the input-surface copy that
        // `copy_into_slot` already synchronized) then synchronized. Requires the context current.
        unsafe {
            ck(
                cuLaunchKernel(
                    f,
                    work_w.div_ceil(B),
                    work_h.div_ceil(B),
                    1,
                    B,
                    B,
                    1,
                    0,
                    stream,
                    args.as_mut_ptr(),
                    std::ptr::null_mut(),
                ),
                "cuLaunchKernel(cursor)",
            )?;
            ck(cuStreamSynchronize(stream), "cuStreamSynchronize(cursor)")
        }
    }
}

impl Drop for CursorBlend {
    fn drop(&mut self) {
        // SAFETY: `cur_buf`/`module` are our own handles, freed exactly once here; the context is
        // current on the encode thread that drops the encoder. Errors are ignored on teardown.
        unsafe {
            let _ = cuMemFree_v2(self.cur_buf);
            let _ = cuModuleUnload(self.module);
        }
    }
}

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
    // dimension/element-size args are by-value ints. No aliasing — four separate locals.
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
        ck(
            cuMemAllocPitch_v2(
                &mut uv_ptr,
                &mut uv_pitch,
                (width as usize / 2) * 2,
                (height as usize / 2).max(1),
                16,
            ),
            "cuMemAllocPitch_v2(UV)",
        )?;
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
    /// [`alloc_pitched_yuv444`]).
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
    /// YUV444 pools: one allocation of 3·`height` stacked 1-byte planes (see [`alloc_pitched_yuv444`]).
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
    /// NV12 only: the interleaved chroma plane `(ptr, pitch)` paired with the Y plane in [`ptr`].
    /// `None` for the default 4-byte RGB/BGRx path. When `Some`, [`ptr`] is the Y plane (1 byte/px).
    pub uv: Option<(CUdeviceptr, usize)>,
    /// Planar YUV444: [`ptr`] is ONE allocation of 3·[`height`](Self::height) rows at
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
        // SAFETY: `self.resource` is the valid `CUgraphicsResource` from a successful `register_gl`
        // (its only constructor), so the wrappers forward to the live table; the caller holds the
        // GL+CUDA contexts current (the registration's contract). `cuGraphicsMapResources` maps
        // `count == 1` resource via `&mut self.resource` (a live field) on the default stream;
        // `cuGraphicsSubResourceGetMappedArray` writes the mapped `CUarray` into the live local
        // `array` (index 0, mip 0). On failure we unmap and bail (balanced). `&copy` is a live
        // local `CUDA_MEMCPY2D` outliving the synchronous `copy_blocking`: `srcArray` is valid
        // while mapped, `dstDevice`/`dstPitch` are `dst`'s live allocation, `width*4`×`height` fit
        // both. `copy_blocking` syncs before we unmap, so the array stays valid through the copy;
        // we always unmap afterward (even on error), keeping the map/unmap pair balanced.
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
                dstDevice: dst_ptr,
                dstPitch: dst_pitch,
                WidthInBytes: width_bytes,
                Height: height,
                ..Default::default()
            };
            let res = copy_blocking(&copy, "cuMemcpy2DAsync_v2(plane)");
            let _ = cuGraphicsUnmapResources(1, &mut self.resource, std::ptr::null_mut());
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
    // SAFETY: `copy_blocking` is unsafe (issues a CUDA copy); the caller must have the shared
    // context current (documented). `&copy` is a live local device→device `CUDA_MEMCPY2D` outliving
    // the synchronous call: `srcDevice`/`srcPitch` are `src`'s live allocation, `dstDevice`/
    // `dstPitch` the caller's live region, `width*4`×`height` within both. Wrapper → live table.
    unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(dev->dev)") }
}

/// Copy our imported NV12 [`DeviceBuffer`] (Y + UV planes) into NVENC's two-plane CUDA surface
/// `(y_dst, y_pitch)` / `(uv_dst, uv_pitch)` (`av_hwframe_get_buffer`'s `data[0]`/`data[1]` +
/// `linesize[0]`/`linesize[1]`). The Y plane is `width`×`height` bytes; the chroma plane is
/// `(width/2)·2` bytes × `height/2` rows. The caller must have the shared context current.
pub fn copy_nv12_to_device(
    src: &DeviceBuffer,
    y_dst: CUdeviceptr,
    y_pitch: usize,
    uv_dst: CUdeviceptr,
    uv_pitch: usize,
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
    // SAFETY: two unsafe `copy_blocking` device→device copies; the caller must have the shared
    // context current (documented). `&y`/`&uv` are live local `CUDA_MEMCPY2D`s outliving each
    // synchronous call. All four device pointers are valid: `src.ptr`/`src_uv_ptr` come from a live
    // NV12 `DeviceBuffer` (its `.uv` presence was checked via `ok_or_else`), `y_dst`/`uv_dst` are
    // the caller's live NVENC surface planes; the luma copy is `w`×`h`, the chroma copy
    // `(w/2)*2`×`h/2`, each within its planes. Wrappers → live table.
    unsafe {
        copy_blocking(&y, "cuMemcpy2DAsync_v2(nv12 Y dev->dev)")?;
        copy_blocking(&uv, "cuMemcpy2DAsync_v2(nv12 UV dev->dev)")
    }
}

/// Copy our imported stacked-YUV444 [`DeviceBuffer`] into NVENC's three-plane CUDA surface
/// (`av_hwframe_get_buffer`'s `data[0..3]` + `linesize[0..3]` for a `yuv444p` frames context).
/// Each plane is `width`×`height` bytes; the source planes sit at row offsets `0/H/2H` of the
/// single allocation. The caller must have the shared context current.
pub fn copy_yuv444_to_device(src: &DeviceBuffer, dsts: [(CUdeviceptr, usize); 3]) -> Result<()> {
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
        // SAFETY: unsafe `copy_blocking` device→device copy; the caller must have the shared
        // context current (documented). `&copy` is a live local outliving the synchronous call;
        // `src.ptr + pitch·h·i` stays within the live 3·H-row stacked allocation (`yuv444`
        // checked above), `dst_ptr`/`dst_pitch` is the caller's live NVENC plane; `w`×`h` fits
        // both. Wrapper → live table.
        unsafe { copy_blocking(&copy, "cuMemcpy2DAsync_v2(yuv444 plane dev->dev)")? };
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
