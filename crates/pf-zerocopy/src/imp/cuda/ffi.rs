//! Raw CUDA Driver API FFI (plan §W4, carved out of the zero-copy CUDA facade): the opaque handle
//! typedefs + struct/const definitions, the `dlopen`'d `libcuda.so.1` symbol table ([`CudaApi`] +
//! [`cuda_api`]), the `unsafe` `cuXxx` wrappers, and the `ck` result check. No higher-level state —
//! the shared `CUcontext`, device buffers, GL/dmabuf interop, and cursor blend all live in [`super`]
//! and drive this layer.

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
pub type CUexternalMemory = *mut c_void; // opaque CUextMemory_st*
pub type CUexternalSemaphore = *mut c_void; // opaque CUextSemaphore_st*

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

/// `CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC` (cuda.h, 64-bit layout). Same union-flattening as the
/// memory desc above: `handle` is a union whose largest member is the win32 two-pointer struct
/// (16 bytes, align 8); for the fd-carrying types only the first 4 bytes (the `int fd`) are read.
/// No `size` field — a semaphore has none.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
    pub type_: c_uint, // CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD = 9
    pub(crate) _pad: u32,
    pub handle: [u64; 2], // union { int fd; {void*,void*} win32; const void* nvSciSyncObj }
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad2: u32,
}

/// `CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS` (cuda.h, 64-bit layout), flattened: `params` nests
/// `fence.value` (the only member we set — the timeline value to signal), the `nvSciSync` union,
/// `keyedMutex.key`, then 12 reserved words; `flags` + 16 reserved words follow. 144 bytes total
/// (layout-asserted in `super`'s tests).
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS {
    pub value: u64, // params.fence.value — the timeline value this signal sets
    pub(crate) _nv_sci_sync: u64,
    pub(crate) _keyed_mutex_key: u64,
    pub(crate) _params_reserved: [c_uint; 12],
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
}

/// `CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS` (cuda.h, 64-bit layout), flattened like the signal
/// params. The C `keyedMutex` member is `{ u64 key; u32 timeoutMs; }` — size 16 with tail
/// padding, hence the explicit pad word before the 10 reserved words. 144 bytes total.
#[repr(C)]
#[derive(Default)]
pub struct CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS {
    pub value: u64, // params.fence.value — wait until the timeline reaches this value
    pub(crate) _nv_sci_sync: u64,
    pub(crate) _keyed_mutex_key: u64,
    pub(crate) _keyed_mutex_timeout: c_uint,
    pub(crate) _keyed_mutex_pad: u32,
    pub(crate) _params_reserved: [c_uint; 10],
    pub flags: c_uint,
    pub(crate) reserved: [c_uint; 16],
    pub(crate) _pad: u32,
}

/// `CUexternalSemaphoreHandleType` (cuda.h): a Vulkan **timeline** semaphore exported as an
/// OPAQUE_FD (`vkGetSemaphoreFdKHR`). Needs driver ≥ 460 (CUDA 11.2) — far below the NVENC 12.1
/// floor this backend already requires, so import failure means "driver refused", not "too old
/// to try".
pub const CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_TIMELINE_SEMAPHORE_FD: c_uint = 9;

// LAYOUT GUARDS for the six hand-flattened cuda.h structs above.
//
// These are filled here and handed straight to the CUDA driver, which reads them by offset. A
// transcription slip does not fail to compile and does not reliably crash — it makes the driver read
// a pitch, a size or an fd from the wrong bytes. Three of the six were already asserted, but only in
// `#[cfg(test)]`, so the check ran when someone ran the tests and never in a release build; these
// are `const`, so they hold on every compilation and cannot be skipped. The other three
// (`CUDA_MEMCPY2D` and the two external-memory descs) had no check at all until now, and
// `CUDA_MEMCPY2D` is the one used on every zero-copy frame.
const _: () = {
    use std::mem::{offset_of, size_of};

    // 16 fields, all pointer-sized or padded to it: 16 * 8 = 128. The two `c_uint` memory-type
    // fields are each followed by padding to align the pointer after them, which is the part a
    // hand-transcription gets wrong.
    assert!(size_of::<CUDA_MEMCPY2D>() == 128);
    assert!(offset_of!(CUDA_MEMCPY2D, srcMemoryType) == 16);
    assert!(offset_of!(CUDA_MEMCPY2D, srcHost) == 24); // padded past srcMemoryType
    assert!(offset_of!(CUDA_MEMCPY2D, srcPitch) == 48);
    assert!(offset_of!(CUDA_MEMCPY2D, dstMemoryType) == 72);
    assert!(offset_of!(CUDA_MEMCPY2D, dstHost) == 80); // padded past dstMemoryType
    assert!(offset_of!(CUDA_MEMCPY2D, dstPitch) == 104);
    assert!(offset_of!(CUDA_MEMCPY2D, WidthInBytes) == 112);
    assert!(offset_of!(CUDA_MEMCPY2D, Height) == 120);

    // type(4)+pad(4)+union(16)+size(8)+flags(4)+reserved[16](64) = 104.
    assert!(size_of::<CUDA_EXTERNAL_MEMORY_HANDLE_DESC>() == 104);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, handle) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, size) == 24);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_HANDLE_DESC, flags) == 32);

    // offset(8)+size(8)+flags(4)+reserved[16](64), tail-padded to 8 = 88.
    assert!(size_of::<CUDA_EXTERNAL_MEMORY_BUFFER_DESC>() == 88);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_BUFFER_DESC, size) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_MEMORY_BUFFER_DESC, flags) == 16);

    // type(4)+pad(4)+union(16)+flags(4)+reserved[16](64) = 96. No `size` — a semaphore has none.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC>() == 96);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC, handle) == 8);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC, flags) == 24);

    // params{fence(8)+nvSciSync(8)+keyedMutex(8)+reserved[12](48)} = 72, flags at 72 → 144.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS>() == 144);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS, value) == 0);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS, flags) == 72);

    // As signal, but `keyedMutex` is `{u64 key; u32 timeoutMs;}` = 16 with tail padding, hence the
    // explicit pad word before the 10 reserved words. Still 72 to `flags` → 144.
    assert!(size_of::<CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS>() == 144);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS, value) == 0);
    assert!(offset_of!(CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS, flags) == 72);
};

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
    cuImportExternalSemaphore: unsafe extern "C" fn(
        *mut CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC,
    ) -> CUresult,
    cuDestroyExternalSemaphore: unsafe extern "C" fn(CUexternalSemaphore) -> CUresult,
    cuSignalExternalSemaphoresAsync: unsafe extern "C" fn(
        *const CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS,
        c_uint,
        CUstream,
    ) -> CUresult,
    cuWaitExternalSemaphoresAsync: unsafe extern "C" fn(
        *const CUexternalSemaphore,
        *const CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS,
        c_uint,
        CUstream,
    ) -> CUresult,
    cuIpcGetMemHandle: unsafe extern "C" fn(*mut CUipcMemHandle, CUdeviceptr) -> CUresult,
    cuIpcOpenMemHandle: unsafe extern "C" fn(*mut CUdeviceptr, CUipcMemHandle, c_uint) -> CUresult,
    cuIpcCloseMemHandle: unsafe extern "C" fn(CUdeviceptr) -> CUresult,
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
                // External-semaphore interop (the stream-ordered cursor blend): all four are
                // CUDA 10.0 entry points, far older than anything else this table requires.
                cuImportExternalSemaphore: *lib.get(b"cuImportExternalSemaphore\0").ok()?,
                cuDestroyExternalSemaphore: *lib.get(b"cuDestroyExternalSemaphore\0").ok()?,
                cuSignalExternalSemaphoresAsync: *lib
                    .get(b"cuSignalExternalSemaphoresAsync\0")
                    .ok()?,
                cuWaitExternalSemaphoresAsync: *lib.get(b"cuWaitExternalSemaphoresAsync\0").ok()?,
                cuIpcGetMemHandle: *lib.get(b"cuIpcGetMemHandle\0").ok()?,
                // CUDA 11 renamed the entry point (per-thread-stream ABI split); every modern
                // driver exports `_v2`, but accept the unsuffixed one too (same signature).
                cuIpcOpenMemHandle: *lib
                    .get(b"cuIpcOpenMemHandle_v2\0")
                    .or_else(|_| lib.get(b"cuIpcOpenMemHandle\0"))
                    .ok()?,
                cuIpcCloseMemHandle: *lib.get(b"cuIpcCloseMemHandle\0").ok()?,
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
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuInit)(flags) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuDeviceGet(device: *mut CUdevice, ordinal: c_int) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuDeviceGet)(device, ordinal) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxCreate_v2(
    pctx: *mut CUcontext,
    flags: c_uint,
    dev: CUdevice,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuCtxCreate_v2)(pctx, flags, dev) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxDestroy_v2(ctx: CUcontext) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuCtxDestroy_v2)(ctx) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxSetCurrent(ctx: CUcontext) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuCtxSetCurrent)(ctx) },
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
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe {
            (a.cuMemAllocPitch_v2)(dptr, pitch, width_bytes, height, element_size)
        },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuMemFree_v2(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuMemFree_v2)(dptr) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuMemcpy2DAsync_v2)(copy, stream) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuStreamSynchronize(stream: CUstream) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuStreamSynchronize)(stream) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuCtxGetStreamPriorityRange(
    least: *mut c_int,
    greatest: *mut c_int,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuCtxGetStreamPriorityRange)(least, greatest) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuStreamCreateWithPriority(
    stream: *mut CUstream,
    flags: c_uint,
    priority: c_int,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuStreamCreateWithPriority)(stream, flags, priority) },
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
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuGraphicsGLRegisterImage)(resource, texture, target, flags) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsMapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuGraphicsMapResources)(count, resources, stream) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsUnmapResources(
    count: c_uint,
    resources: *mut CUgraphicsResource,
    stream: *mut c_void,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuGraphicsUnmapResources)(count, resources, stream) },
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
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe {
            (a.cuGraphicsSubResourceGetMappedArray)(array, resource, array_index, mip_level)
        },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuGraphicsUnregisterResource(resource: CUgraphicsResource) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuGraphicsUnregisterResource)(resource) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuImportExternalMemory(
    ext_mem_out: *mut CUexternalMemory,
    mem_handle_desc: *const CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuImportExternalMemory)(ext_mem_out, mem_handle_desc) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuExternalMemoryGetMappedBuffer(
    dev_ptr: *mut CUdeviceptr,
    ext_mem: CUexternalMemory,
    buffer_desc: *const CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuExternalMemoryGetMappedBuffer)(dev_ptr, ext_mem, buffer_desc) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuDestroyExternalMemory(ext_mem: CUexternalMemory) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuDestroyExternalMemory)(ext_mem) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuImportExternalSemaphore(
    ext_sem_out: *mut CUexternalSemaphore,
    sem_handle_desc: *const CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuImportExternalSemaphore)(ext_sem_out, sem_handle_desc) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuDestroyExternalSemaphore(ext_sem: CUexternalSemaphore) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuDestroyExternalSemaphore)(ext_sem) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuSignalExternalSemaphoresAsync(
    ext_sems: *const CUexternalSemaphore,
    params: *const CUDA_EXTERNAL_SEMAPHORE_SIGNAL_PARAMS,
    count: c_uint,
    stream: CUstream,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuSignalExternalSemaphoresAsync)(ext_sems, params, count, stream) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuWaitExternalSemaphoresAsync(
    ext_sems: *const CUexternalSemaphore,
    params: *const CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS,
    count: c_uint,
    stream: CUstream,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuWaitExternalSemaphoresAsync)(ext_sems, params, count, stream) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcGetMemHandle(handle: *mut CUipcMemHandle, dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuIpcGetMemHandle)(handle, dptr) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcOpenMemHandle(
    dptr: *mut CUdeviceptr,
    handle: CUipcMemHandle,
    flags: c_uint,
) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuIpcOpenMemHandle)(dptr, handle, flags) },
        None => CU_ERROR_NOT_LOADED,
    }
}
pub(crate) unsafe fn cuIpcCloseMemHandle(dptr: CUdeviceptr) -> CUresult {
    match cuda_api() {
        // SAFETY: forwards this unsafe wrapper's arguments to the live dlopen'd entry point (the
        // table is never unloaded — `mem::forget(lib)`); the caller upholds the driver-API contract,
        // with the site-specific proof at each call site.
        Some(a) => unsafe { (a.cuIpcCloseMemHandle)(dptr) },
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
