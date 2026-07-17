//! Raw CUDA Driver API FFI (plan §W4, carved out of the zero-copy CUDA facade): the opaque handle
//! typedefs + struct/const definitions, the `dlopen`'d `libcuda.so.1` symbol table ([`CudaApi`] +
//! [`cuda_api`]), the `unsafe` `cuXxx` wrappers, and the `ck` result check. No higher-level state —
//! the shared `CUcontext`, device buffers, GL/dmabuf interop, and cursor blend all live in [`super`]
//! and drive this layer.

#![allow(non_camel_case_types, non_snake_case)]
// Every `unsafe` block/impl below carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{bail, Result};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

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
pub(crate) const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x04;

/// `cuStreamCreateWithPriority` flag: don't implicitly synchronize with the legacy NULL stream.
pub(crate) const CU_STREAM_NON_BLOCKING: c_uint = 0x01;

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
    pub(crate) _pad: u32,
    pub handle: [u64; 2], // union { int fd; {void*,void*} win32; void* nvSciBufObject }
    pub size: u64,
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad2: u32,
}

/// `CUDA_EXTERNAL_MEMORY_BUFFER_DESC` (cuda.h, 64-bit layout).
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_MEMORY_BUFFER_DESC {
    pub offset: u64,
    pub size: u64,
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
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
pub(crate) const CU_IPC_MEM_LAZY_ENABLE_PEER_ACCESS: c_uint = 0x1;

/// CUDA Driver API entry points, resolved at runtime from `libcuda.so.1` via `dlopen` rather than
/// a link-time `#[link(name = "cuda")]`. This is what lets ONE host binary run on NVIDIA
/// (zero-copy via CUDA → NVENC) *and* on AMD/Intel (VAAPI, where the NVIDIA driver — and thus
/// `libcuda` — is absent): with a hard link the loader would refuse to start the binary at all.
/// Every `cu*` call below goes through a same-named wrapper fn that forwards to this table; when
/// the driver isn't present the table is `None` and the wrappers return a non-zero `CUresult`, so
/// `context()` fails cleanly and the capturer falls back to the CPU path. The `cuda_api()` loader
/// is memoised; the library handle is intentionally leaked (process-lifetime, like the context).
pub(crate) struct CudaApi {
    cuInit: unsafe extern "C" fn(c_uint) -> CUresult,
    cuDeviceGet: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    cuCtxCreate_v2: unsafe extern "C" fn(*mut CUcontext, c_uint, CUdevice) -> CUresult,
    cuCtxDestroy_v2: unsafe extern "C" fn(CUcontext) -> CUresult,
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
pub(crate) const CU_ERROR_NOT_LOADED: CUresult = 999;

pub(crate) static CUDA_API: OnceLock<Option<CudaApi>> = OnceLock::new();

/// Resolve `libcuda.so.1` and its symbols once. `None` when the NVIDIA driver isn't installed
/// (the expected case on AMD/Intel hosts) — logged at debug, not an error.
pub(crate) fn cuda_api() -> Option<&'static CudaApi> {
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
                cuCtxDestroy_v2: *lib.get(b"cuCtxDestroy_v2\0").ok()?,
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
pub(crate) unsafe fn cuInit(flags: c_uint) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuInit)(flags),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuDeviceGet(device: *mut CUdevice, ordinal: c_int) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuDeviceGet)(device, ordinal),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxCreate_v2(
    pctx: *mut CUcontext,
    flags: c_uint,
    dev: CUdevice,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxCreate_v2)(pctx, flags, dev),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxDestroy_v2(ctx: CUcontext) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxDestroy_v2)(ctx),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxSetCurrent(ctx: CUcontext) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxSetCurrent)(ctx),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuMemAllocPitch_v2(
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
pub(crate) unsafe fn cuMemFree_v2(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemFree_v2)(dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuMemAlloc_v2(dptr: *mut CUdeviceptr, size: usize) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemAlloc_v2)(dptr, size),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuModuleLoadData(m: *mut CUmodule, image: *const c_void) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleLoadData)(m, image),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuModuleUnload(m: CUmodule) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleUnload)(m),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuModuleGetFunction(
    f: *mut CUfunction,
    m: CUmodule,
    name: *const c_char,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuModuleGetFunction)(f, m, name),
        None => CU_ERROR_NOT_LOADED,
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn cuLaunchKernel(
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
pub(crate) unsafe fn cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuMemcpy2DAsync_v2)(copy, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuStreamSynchronize(stream: CUstream) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuStreamSynchronize)(stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxGetStreamPriorityRange(
    least: *mut c_int,
    greatest: *mut c_int,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuCtxGetStreamPriorityRange)(least, greatest),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuStreamCreateWithPriority(
    stream: *mut CUstream,
    flags: c_uint,
    priority: c_int,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuStreamCreateWithPriority)(stream, flags, priority),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsGLRegisterImage(
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
pub(crate) unsafe fn cuGraphicsMapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsMapResources)(count, resources, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsUnmapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsUnmapResources)(count, resources, stream),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsSubResourceGetMappedArray(
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
pub(crate) unsafe fn cuGraphicsUnregisterResource(resource: CUgraphicsResource) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuGraphicsUnregisterResource)(resource),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuImportExternalMemory(
    ext_mem_out: *mut CUexternalMemory,
    mem_handle_desc: *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuImportExternalMemory)(ext_mem_out, mem_handle_desc),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuExternalMemoryGetMappedBuffer(
    dev_ptr: *mut CUdeviceptr,
    ext_mem: CUexternalMemory,
    buffer_desc: *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuExternalMemoryGetMappedBuffer)(dev_ptr, ext_mem, buffer_desc),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuDestroyExternalMemory(ext_mem: CUexternalMemory) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuDestroyExternalMemory)(ext_mem),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcGetMemHandle(handle: *mut CUipcMemHandle, dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcGetMemHandle)(handle, dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcOpenMemHandle(
    dptr: *mut CUdeviceptr,
    handle: CUipcMemHandle,
    flags: c_uint,
) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcOpenMemHandle)(dptr, handle, flags),
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcCloseMemHandle(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        Some(a) => (a.cuIpcCloseMemHandle)(dptr),
        None => CU_ERROR_NOT_LOADED,
    }
}

#[inline]
pub(crate) fn ck(r: CUresult, what: &str) -> Result<()> {
    if r == 0 {
        Ok(())
    } else {
        bail!("CUDA driver error {r} in {what}")
    }
}
