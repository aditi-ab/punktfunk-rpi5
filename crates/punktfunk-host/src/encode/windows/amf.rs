//! AMD **AMF** hardware encoder (Windows, D3D11 input) — the direct-SDK replacement for the
//! libavcodec `*_amf` path (design/native-amf-encoder.md), the AMD analogue of [`super::nvenc`].
//!
//! Why not libavcodec: its AMF wrapper holds ~2 frames before releasing the oldest AU (measured
//! 36 ms p50 at 720p60 on the Ryzen 7000 iGPU — ~2 frame periods of pure pipeline latency no
//! knob removes; see the `poll` doc in `ffmpeg_win.rs`), and it flattens every driver wedge into
//! forever-EAGAIN, which only the ~2 s encode-stall watchdog can catch. The AMF runtime itself
//! returns typed `AMF_RESULT` codes (`AMF_INPUT_FULL`, device-lost, …), so this path sees a wedge
//! on the frame it happens, and its bounded-blocking poll ([`vaapi.rs::poll`]'s model) ships each
//! AU the same tick it finishes (~1–5 ms VCN encode at streaming settings).
//!
//! Drives the AMF runtime through its **C vtable ABI**: the GPUOpen public headers define
//! C-compatible vtable structs for every interface, and FFmpeg's `amfenc.c` (plain C) drives AMF
//! exclusively through them, so that ABI — not the C++ classes — is the stable, supported
//! surface. The FFI below mirrors ONLY the interfaces/slots we call, written against header
//! version **v1.4.36** (`AMF_FULL_VERSION` 1.4.36.0). At load the runtime is accepted down to a
//! stable-ABI floor of **v1.4.34** (the `AMFQueryVersion` gate); the 1.4.35/1.4.36-only encoder
//! features are string-keyed properties that degrade individually on older drivers, not vtable
//! changes (see [`sys::AMF_MIN_VERSION`]). The runtime is
//! loaded at runtime from the driver-installed `amfrt64.dll` — exactly as `nvenc.rs` loads
//! `nvEncodeAPI64.dll` — so this compiles unconditionally on Windows (**no build feature, no new
//! dependency**). Since Phase 3 (design §7) this is the sole AMD dispatch: a box without a
//! working AMD AMF runtime fails [`AmfEncoder::open`] with an "update the AMD driver" message and
//! the **session fails** (the libavcodec AMF fallback + the `PUNKTFUNK_AMF_FFMPEG` hatch were
//! deleted; FFmpeg now serves QSV only).
//!
//! Input is zero-copy by construction (design §3.2): a small owned D3D11 NV12/P010 texture ring
//! on the **capturer's own device** (same-device requirement as every backend — the capture
//! textures are not shared-handle), `CopySubresourceRegion` of the captured texture into the next
//! slot (GPU-local), then `CreateSurfaceFromDX11Native` + `SubmitInput`. There is no readback
//! path: a capturer that fell back to Bgra/Rgb10a2 (no video processor) or CPU frames is rejected
//! at open/submit. **Phase-3 caveat:** with the ffmpeg readback fallback gone, that rejection now
//! ends the session instead of degrading — so if the video-processor format fallback ever fires
//! in the field, the fix is the native AMFVideoConverter front-end (design §3.2), NOT restoring
//! the libavcodec path. Not yet observed on lab hardware (the video processor yields NV12/P010).
//!
//! Scope (design §7): **Phase 1** — AVC + HEVC (SDR 8-bit NV12 / HDR 10-bit P010), bounded poll,
//! native `reset()`; **Phase 2** — AV1 (RDNA3+; probed, never assumed), the intra-refresh wave
//! (`PUNKTFUNK_INTRA_REFRESH`, the same opt-in as Linux NVENC — heals FEC-unrecoverable loss
//! without the 20-40× full-IDR spike), in-band HDR mastering/CLL metadata (`*InHDRMetadata` →
//! HEVC SEI / AV1 metadata OBU), and the native codec probe ([`probe_can_encode`], feeding the
//! GameStream advertisement); **Phase 3** — this is now the sole AMD dispatch (the libavcodec
//! fallback + `PUNKTFUNK_AMF_FFMPEG` hatch are gone). 4:4:4 is **permanently** out: VCN hardware
//! does not encode 4:4:4.

// Every `unsafe` block / impl in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

// ---------------------------------------------------------------------------------------------
// Mirrored AMF C ABI (written against GPUOpen header release v1.4.36 — amf/public/include; every
// slot below is a base-interface slot whose layout is stable since <= v1.4.34, the loader's
// accepted ABI floor, so the mirror is valid on every runtime the loader admits).
//
// Layout rules this mirror relies on: every AMF interface is a struct whose sole member is a
// pointer to a C vtable; derived interfaces PREPEND their base's slots in order (AMFInterface →
// AMFPropertyStorage → AMFData → AMFBuffer/AMFSurface), so a derived pointer is usable through a
// base vtable mirror. Slots we never call are declared as bare `*const c_void` placeholders —
// same size/alignment as the function pointer they stand in for. `AMF_STD_CALL` is `__stdcall`
// (Rust `extern "system"`); the two DLL entry points are `__cdecl` (`extern "C"`); on x86_64
// both collapse to the one Windows calling convention.
// ---------------------------------------------------------------------------------------------
mod sys {
    use std::ffi::c_void;

    /// `AMF_RESULT` (core/Result.h) — a plain C enum, sequential from 0. Only the codes this
    /// module branches on are named; everything else is reported numerically via [`result_name`].
    pub type AmfResult = i32;
    pub const AMF_OK: AmfResult = 0;
    pub const AMF_EOF: AmfResult = 23;
    pub const AMF_REPEAT: AmfResult = 24;
    pub const AMF_INPUT_FULL: AmfResult = 25;
    pub const AMF_NEED_MORE_INPUT: AmfResult = 44;

    /// Human-readable name for an `AMF_RESULT` (diagnostics only — the numeric value rides along
    /// so an unnamed code is still identifiable against Result.h).
    pub fn result_name(r: AmfResult) -> &'static str {
        match r {
            0 => "AMF_OK",
            1 => "AMF_FAIL",
            2 => "AMF_UNEXPECTED",
            3 => "AMF_ACCESS_DENIED",
            4 => "AMF_INVALID_ARG",
            5 => "AMF_OUT_OF_RANGE",
            6 => "AMF_OUT_OF_MEMORY",
            7 => "AMF_INVALID_POINTER",
            8 => "AMF_NO_INTERFACE",
            9 => "AMF_NOT_IMPLEMENTED",
            10 => "AMF_NOT_SUPPORTED",
            11 => "AMF_NOT_FOUND",
            12 => "AMF_ALREADY_INITIALIZED",
            13 => "AMF_NOT_INITIALIZED",
            14 => "AMF_INVALID_FORMAT",
            15 => "AMF_WRONG_STATE",
            17 => "AMF_NO_DEVICE",
            18 => "AMF_DIRECTX_FAILED",
            23 => "AMF_EOF",
            24 => "AMF_REPEAT",
            25 => "AMF_INPUT_FULL",
            26 => "AMF_RESOLUTION_CHANGED",
            28 => "AMF_INVALID_DATA_TYPE",
            29 => "AMF_INVALID_RESOLUTION",
            30 => "AMF_CODEC_NOT_SUPPORTED",
            31 => "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
            32 => "AMF_SURFACE_MUST_BE_SHARED",
            36 => "AMF_ENCODER_NOT_PRESENT",
            44 => "AMF_NEED_MORE_INPUT",
            _ => "AMF_<unnamed>",
        }
    }

    /// The AMF header release this FFI mirror was written against: `AMF_FULL_VERSION` for 1.4.36.0
    /// (core/Version.h `AMF_MAKE_FULL_VERSION`). This is the version claimed to `AMFInit` — but
    /// capped at the runtime's own reported version (see `load_factory`), so an older-but-accepted
    /// runtime is asked only for the ABI it actually provides.
    pub const AMF_HEADER_VERSION: u64 = (1u64 << 48) | (4u64 << 32) | (36u64 << 16);

    /// The oldest AMF runtime the loader accepts (`AMF_FULL_VERSION` 1.4.34.0 — AMD Adrenalin
    /// 24.6.1). This is an **ABI floor, not a feature floor**: every vtable slot mirrored in this
    /// module belongs to a base interface (`AMFFactory`/`AMFContext`/`AMFComponent`/`AMFData`/
    /// `AMFBuffer`) whose layout has been stable — append-only, no mid-vtable insertions — since
    /// well before 1.4.34, so a 1.4.34 runtime is guaranteed to expose every mirrored slot at its
    /// mirrored offset. Everything 1.4.35/1.4.36 added that this path can touch (new HQ presets,
    /// AV1 B-frame / picture management) is a *string-keyed encoder property*, applied through
    /// [`set_prop`] with `required=false` — a runtime that lacks it rejects the property (logged)
    /// and the feature degrades, rather than shifting any vtable offset. Below this floor the
    /// mirror is not guaranteed to match, so the loader declines cleanly (an old-driver decline,
    /// never UB).
    pub const AMF_MIN_VERSION: u64 = (1u64 << 48) | (4u64 << 32) | (34u64 << 16);

    /// `AMF_SURFACE_FORMAT` (core/Surface.h).
    pub const AMF_SURFACE_NV12: i32 = 1;
    pub const AMF_SURFACE_P010: i32 = 10;

    /// `AMF_DX_VERSION::AMF_DX11_1` (core/Data.h) — the `InitDX11` version argument.
    pub const AMF_DX11_1: i32 = 111;

    /// `AMF_MEMORY_TYPE::AMF_MEMORY_HOST` (core/Data.h) — the `AllocBuffer` memory type for the
    /// CPU-filled HDR-metadata buffer.
    pub const AMF_MEMORY_HOST: i32 = 1;

    /// `AMFHDRMetadata` (components/ColorSpace.h) — the payload of the `*InHDRMetadata` encoder
    /// property (an `AMFBuffer` holding exactly this struct). Same units as the HEVC ST.2086 SEI
    /// and [`punktfunk_core::quic::HdrMeta`]: chromaticities in 1/50000, mastering luminance in
    /// 0.0001 cd/m², CLL/FALL in nits. 28 bytes, no padding.
    #[repr(C)]
    pub struct AmfHdrMetadata {
        pub red_primary: [u16; 2],
        pub green_primary: [u16; 2],
        pub blue_primary: [u16; 2],
        pub white_point: [u16; 2],
        pub max_mastering_luminance: u32,
        pub min_mastering_luminance: u32,
        pub max_content_light_level: u16,
        pub max_frame_average_light_level: u16,
    }

    /// `AMFGuid` (core/Platform.h) — data41..data48 flattened into an array (identical layout).
    #[repr(C)]
    pub struct AmfGuid {
        pub data1: u32,
        pub data2: u16,
        pub data3: u16,
        pub data4: [u8; 8],
    }

    /// `IID_AMFBuffer` (core/Buffer.h `AMF_DECLARE_IID`) — for `QueryInterface` on the encoder's
    /// output `AMFData` to reach `GetNative`/`GetSize`.
    pub const IID_AMF_BUFFER: AmfGuid = AmfGuid {
        data1: 0xb04b_7248,
        data2: 0xb6f0,
        data3: 0x4321,
        data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
    };

    // `AMF_VARIANT_TYPE` (core/Variant.h) — the tags this module writes/reads.
    pub const AMF_VARIANT_BOOL: i32 = 1;
    pub const AMF_VARIANT_INT64: i32 = 2;
    pub const AMF_VARIANT_RATE: i32 = 7;
    pub const AMF_VARIANT_INTERFACE: i32 = 12;

    /// `AMFVariantStruct` (core/Variant.h): a 4-byte C-enum tag + a 16-byte union whose largest
    /// members are pointer/`amf_int64`/`AMFFloatVector4D` (align 8) → 24 bytes total, tag at 0,
    /// payload at 8. Passed BY VALUE to `SetProperty` (Win64 passes >8-byte aggregates by hidden
    /// reference on both sides, so declaring it by value matches the C compiler). The payload is
    /// stored as two fully-initialised `u64`s — little-endian packing puts a bool in byte 0, an
    /// `amf_int64` in word 0, and an `AMFRate{num,den}` as `num | den << 32`, exactly the union's
    /// in-memory layout — so no partially-initialised union bytes ever cross the FFI.
    #[repr(C)]
    pub struct AmfVariant {
        pub vtype: i32,
        pub payload: [u64; 2],
    }

    impl AmfVariant {
        pub fn zeroed() -> Self {
            AmfVariant {
                vtype: 0, // AMF_VARIANT_EMPTY
                payload: [0, 0],
            }
        }
        pub fn from_i64(v: i64) -> Self {
            AmfVariant {
                vtype: AMF_VARIANT_INT64,
                payload: [v as u64, 0],
            }
        }
        pub fn from_bool(v: bool) -> Self {
            AmfVariant {
                vtype: AMF_VARIANT_BOOL,
                payload: [v as u64, 0],
            }
        }
        /// `AMFRate { num, den }` — two little-endian `amf_uint32`s in the union's first 8 bytes.
        pub fn from_rate(num: u32, den: u32) -> Self {
            AmfVariant {
                vtype: AMF_VARIANT_RATE,
                payload: [num as u64 | ((den as u64) << 32), 0],
            }
        }
        /// An `AMFInterface*` payload (`pInterface` in the union's first 8 bytes). The property
        /// storage AddRefs the interface when it copies the variant in (the C++ template
        /// `SetProperty(name, AMFVariant(value))` passes a temporary whose destructor releases,
        /// so `SetProperty` must take its own reference) — the caller keeps sole ownership of the
        /// reference it already holds.
        pub fn from_interface(p: *mut c_void) -> Self {
            AmfVariant {
                vtype: AMF_VARIANT_INTERFACE,
                payload: [p as usize as u64, 0],
            }
        }
        /// Read back an `amf_int64` payload (only valid when `vtype == AMF_VARIANT_INT64`).
        pub fn as_i64(&self) -> Option<i64> {
            (self.vtype == AMF_VARIANT_INT64).then_some(self.payload[0] as i64)
        }
    }

    /// Placeholder for a vtable slot this module never calls — same size/align as the function
    /// pointer it stands in for, present only to keep the following slots at their C offsets.
    pub type Slot = *const c_void;

    // -- AMFFactory (core/Factory.h; NOT refcounted — a process singleton) ----------------------
    #[repr(C)]
    pub struct AmfFactory {
        pub vtbl: *const AmfFactoryVtbl,
    }
    #[repr(C)]
    pub struct AmfFactoryVtbl {
        pub create_context:
            unsafe extern "system" fn(*mut AmfFactory, *mut *mut AmfContext) -> AmfResult,
        pub create_component: unsafe extern "system" fn(
            *mut AmfFactory,
            *mut AmfContext,
            *const u16,
            *mut *mut AmfComponent,
        ) -> AmfResult,
        pub set_cache_folder: Slot,
        pub get_cache_folder: Slot,
        pub get_debug: Slot,
        pub get_trace: Slot,
        pub get_programs: Slot,
    }

    // -- AMFContext (core/Context.h) ------------------------------------------------------------
    #[repr(C)]
    pub struct AmfContext {
        pub vtbl: *const AmfContextVtbl,
    }
    #[repr(C)]
    pub struct AmfContextVtbl {
        // AMFInterface
        pub acquire: Slot,
        pub release: unsafe extern "system" fn(*mut AmfContext) -> i32,
        pub query_interface: Slot,
        // AMFPropertyStorage
        pub set_property: Slot,
        pub get_property: Slot,
        pub has_property: Slot,
        pub get_property_count: Slot,
        pub get_property_at: Slot,
        pub clear: Slot,
        pub add_to: Slot,
        pub copy_to: Slot,
        pub add_observer: Slot,
        pub remove_observer: Slot,
        // AMFContext
        pub terminate: unsafe extern "system" fn(*mut AmfContext) -> AmfResult,
        pub init_dx9: Slot,
        pub get_dx9_device: Slot,
        pub lock_dx9: Slot,
        pub unlock_dx9: Slot,
        pub init_dx11: unsafe extern "system" fn(*mut AmfContext, *mut c_void, i32) -> AmfResult,
        pub get_dx11_device: Slot,
        pub lock_dx11: Slot,
        pub unlock_dx11: Slot,
        pub init_opencl: Slot,
        pub get_opencl_context: Slot,
        pub get_opencl_command_queue: Slot,
        pub get_opencl_device_id: Slot,
        pub get_opencl_compute_factory: Slot,
        pub init_opencl_ex: Slot,
        pub lock_opencl: Slot,
        pub unlock_opencl: Slot,
        pub init_opengl: Slot,
        pub get_opengl_context: Slot,
        pub get_opengl_drawable: Slot,
        pub lock_opengl: Slot,
        pub unlock_opengl: Slot,
        pub init_xv: Slot,
        pub get_xv_device: Slot,
        pub lock_xv: Slot,
        pub unlock_xv: Slot,
        pub init_gralloc: Slot,
        pub get_gralloc_device: Slot,
        pub lock_gralloc: Slot,
        pub unlock_gralloc: Slot,
        pub alloc_buffer: unsafe extern "system" fn(
            *mut AmfContext,
            i32, // AMF_MEMORY_TYPE
            usize,
            *mut *mut AmfBuffer,
        ) -> AmfResult,
        pub alloc_surface: Slot,
        pub alloc_audio_buffer: Slot,
        pub create_buffer_from_host_native: Slot,
        pub create_surface_from_host_native: Slot,
        pub create_surface_from_dx9_native: Slot,
        /// Out-param is `AMFSurface**` in the header; declared as the `AmfData` base here because
        /// every surface call this module makes (`SetPts`, `SetProperty`, `Release`,
        /// `SubmitInput`) lives in the `AMFData` vtable prefix, which `AMFSurfaceVtbl` reproduces
        /// slot-for-slot (single inheritance, same object pointer).
        pub create_surface_from_dx11_native: unsafe extern "system" fn(
            *mut AmfContext,
            *mut c_void,
            *mut *mut AmfData,
            *mut c_void,
        ) -> AmfResult,
        pub create_surface_from_opengl_native: Slot,
        pub create_surface_from_gralloc_native: Slot,
        pub create_surface_from_opencl_native: Slot,
        pub create_buffer_from_opencl_native: Slot,
        pub get_compute: Slot,
    }

    // -- AMFComponent (components/Component.h) --------------------------------------------------
    #[repr(C)]
    pub struct AmfComponent {
        pub vtbl: *const AmfComponentVtbl,
    }
    #[repr(C)]
    pub struct AmfComponentVtbl {
        // AMFInterface
        pub acquire: Slot,
        pub release: unsafe extern "system" fn(*mut AmfComponent) -> i32,
        pub query_interface: Slot,
        // AMFPropertyStorage
        pub set_property:
            unsafe extern "system" fn(*mut AmfComponent, *const u16, AmfVariant) -> AmfResult,
        pub get_property: Slot,
        pub has_property: Slot,
        pub get_property_count: Slot,
        pub get_property_at: Slot,
        pub clear: Slot,
        pub add_to: Slot,
        pub copy_to: Slot,
        pub add_observer: Slot,
        pub remove_observer: Slot,
        // AMFPropertyStorageEx
        pub get_properties_info_count: Slot,
        pub get_property_info_at: Slot,
        pub get_property_info: Slot,
        pub validate_property: Slot,
        // AMFComponent
        pub init: unsafe extern "system" fn(*mut AmfComponent, i32, i32, i32) -> AmfResult,
        pub reinit: Slot,
        pub terminate: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
        pub drain: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
        pub flush: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
        pub submit_input: unsafe extern "system" fn(*mut AmfComponent, *mut AmfData) -> AmfResult,
        pub query_output:
            unsafe extern "system" fn(*mut AmfComponent, *mut *mut AmfData) -> AmfResult,
        pub get_context: Slot,
        pub set_output_data_allocator_cb: Slot,
        pub get_caps: Slot,
        pub optimize: Slot,
    }

    // -- AMFData (core/Data.h) — also the usable prefix of AMFSurface --------------------------
    #[repr(C)]
    pub struct AmfData {
        pub vtbl: *const AmfDataVtbl,
    }
    #[repr(C)]
    pub struct AmfDataVtbl {
        // AMFInterface
        pub acquire: Slot,
        pub release: unsafe extern "system" fn(*mut AmfData) -> i32,
        pub query_interface:
            unsafe extern "system" fn(*mut AmfData, *const AmfGuid, *mut *mut c_void) -> AmfResult,
        // AMFPropertyStorage
        pub set_property:
            unsafe extern "system" fn(*mut AmfData, *const u16, AmfVariant) -> AmfResult,
        pub get_property:
            unsafe extern "system" fn(*mut AmfData, *const u16, *mut AmfVariant) -> AmfResult,
        pub has_property: Slot,
        pub get_property_count: Slot,
        pub get_property_at: Slot,
        pub clear: Slot,
        pub add_to: Slot,
        pub copy_to: Slot,
        pub add_observer: Slot,
        pub remove_observer: Slot,
        // AMFData
        pub get_memory_type: Slot,
        pub duplicate: Slot,
        pub convert: Slot,
        pub interop: Slot,
        pub get_data_type: Slot,
        pub is_reusable: Slot,
        pub set_pts: unsafe extern "system" fn(*mut AmfData, i64),
        pub get_pts: Slot,
        pub set_duration: Slot,
        pub get_duration: Slot,
    }

    // -- AMFBuffer (core/Buffer.h) — the encoder's output object -------------------------------
    #[repr(C)]
    pub struct AmfBuffer {
        pub vtbl: *const AmfBufferVtbl,
    }
    #[repr(C)]
    pub struct AmfBufferVtbl {
        // AMFInterface + AMFPropertyStorage + AMFData prefix (identical order to AmfDataVtbl).
        pub acquire: Slot,
        pub release: unsafe extern "system" fn(*mut AmfBuffer) -> i32,
        pub query_interface: Slot,
        pub set_property: Slot,
        pub get_property: Slot,
        pub has_property: Slot,
        pub get_property_count: Slot,
        pub get_property_at: Slot,
        pub clear: Slot,
        pub add_to: Slot,
        pub copy_to: Slot,
        pub add_observer: Slot,
        pub remove_observer: Slot,
        pub get_memory_type: Slot,
        pub duplicate: Slot,
        pub convert: Slot,
        pub interop: Slot,
        pub get_data_type: Slot,
        pub is_reusable: Slot,
        pub set_pts: Slot,
        pub get_pts: Slot,
        pub set_duration: Slot,
        pub get_duration: Slot,
        // AMFBuffer
        pub set_size: Slot,
        pub get_size: unsafe extern "system" fn(*mut AmfBuffer) -> usize,
        pub get_native: unsafe extern "system" fn(*mut AmfBuffer) -> *mut c_void,
        pub add_observer_buffer: Slot,
        pub remove_observer_buffer: Slot,
    }

    // -- DLL entry points (core/Factory.h; AMF_CDECL_CALL) --------------------------------------
    pub type AmfQueryVersionFn = unsafe extern "C" fn(*mut u64) -> AmfResult;
    pub type AmfInitFn = unsafe extern "C" fn(u64, *mut *mut AmfFactory) -> AmfResult;
}

use sys::{result_name, AmfVariant};

/// `Ok(())` or a named-code error, mirroring `nvenc.rs`'s `NvStatusExt::nv_ok`.
fn amf_ok(r: sys::AmfResult, what: &str) -> Result<()> {
    if r == sys::AMF_OK {
        Ok(())
    } else {
        Err(anyhow!("{what}: {} ({r})", result_name(r)))
    }
}

/// Format an `AMF_FULL_VERSION` u64 as `major.minor.patch` (the build field is dropped — every
/// version comparison and log line in this module ignores it).
fn amf_version_str(v: u64) -> String {
    format!(
        "{}.{}.{}",
        (v >> 48) & 0xffff,
        (v >> 32) & 0xffff,
        (v >> 16) & 0xffff
    )
}

/// Best-effort on-disk identity of the loaded `amfrt64.dll`: `(full path, file-version resource)`.
/// The file-version is the driver build baked into the DLL (e.g. `31.0.24033.1003`), which — unlike
/// the AMF runtime version — is directly comparable to the display-driver version. This pins down
/// the Boot Camp failure mode: the display driver can report 25.x while the `amfrt64.dll` actually
/// loaded (System32, via the SYSTEM32-only search) is a stale build whose AMF + file versions lag
/// it. Diagnostics only — any failure yields `None` and never affects the load.
///
/// # Safety
/// `module` must be a live module handle owned by the caller (here, the never-unloaded
/// `amfrt64.dll` from `LoadLibraryExW`).
unsafe fn loaded_dll_identity(module: HMODULE) -> (Option<String>, Option<String>) {
    let mut buf = [0u16; 512];
    let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
    // n == 0 → failed; n >= len → path truncated (no guaranteed NUL) — bail either way. Otherwise
    // `GetModuleFileNameW` NUL-terminates at `buf[n]`, so `buf` is a valid PCWSTR for the query.
    if n == 0 || n >= buf.len() {
        return (None, None);
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    (Some(path), dll_file_version(PCWSTR(buf.as_ptr())))
}

/// Read a DLL's `\`-root `VS_FIXEDFILEINFO` file version as `a.b.c.d`. `None` if the file has no
/// version resource or any step fails (diagnostics only).
///
/// # Safety
/// `path` must be a valid NUL-terminated wide string pointing at a readable file path.
unsafe fn dll_file_version(path: PCWSTR) -> Option<String> {
    let size = GetFileVersionInfoSizeW(path, None);
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    GetFileVersionInfoW(path, None, size, block.as_mut_ptr() as *mut c_void).ok()?;
    let mut value: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;
    let ok = VerQueryValueW(
        block.as_ptr() as *const c_void,
        w!("\\"),
        &mut value,
        &mut len,
    );
    if !ok.as_bool() || value.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: on success `VerQueryValueW` points `value` at a `VS_FIXEDFILEINFO` living inside
    // `block` and valid for `len` bytes (checked >= its size); `block` outlives this read.
    let ffi = &*(value as *const VS_FIXEDFILEINFO);
    let (ms, ls) = (ffi.dwFileVersionMS, ffi.dwFileVersionLS);
    Some(format!(
        "{}.{}.{}.{}",
        ms >> 16,
        ms & 0xffff,
        ls >> 16,
        ls & 0xffff
    ))
}

// ---------------------------------------------------------------------------------------------
// Runtime loader (the analogue of nvenc.rs `load_api`): resolve amfrt64.dll's two exports once
// per process, gate on the pinned header version, and keep the factory singleton forever.
// ---------------------------------------------------------------------------------------------

struct AmfLib {
    factory: *mut sys::AmfFactory,
    version: u64,
}
// SAFETY: `factory` is the process-global AMF factory singleton returned by `AMFInit`; the AMF
// runtime documents the factory and its object creation as thread-safe, the DLL is never
// unloaded, and this struct is only ever handed out as `&'static` from a `OnceLock` — no interior
// mutation happens on the Rust side.
unsafe impl Send for AmfLib {}
// SAFETY: as above — shared references only ever read the two plain fields; all mutation happens
// inside the (thread-safe) AMF runtime.
unsafe impl Sync for AmfLib {}

/// Resolve the AMF runtime once per process. `Err` = AMF genuinely unavailable here (no AMD
/// driver / `amfrt64.dll`, or a runtime older than the minimum-supported v1.4.34 — see
/// [`sys::AMF_MIN_VERSION`]) — callers fail their open cleanly with an "update the AMD driver"
/// message (the session then fails; since Phase 3 there is no libavcodec AMF fallback).
fn try_factory() -> std::result::Result<&'static AmfLib, &'static str> {
    static LIB: std::sync::OnceLock<std::result::Result<AmfLib, String>> =
        std::sync::OnceLock::new();
    LIB.get_or_init(|| {
        let lib = load_factory();
        if let Err(e) = &lib {
            // Once per process; only reachable when the backend resolved to AMF on this box.
            tracing::warn!("native AMF runtime unavailable: {e}");
        }
        lib
    })
    .as_ref()
    .map_err(|e| e.as_str())
}

fn load_factory() -> std::result::Result<AmfLib, String> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names; the
    // System32-only search path keeps a planted DLL out of the SYSTEM-service process (same
    // hardening as the NVENC loader). The two transmutes cast the resolved exports to their
    // documented prototypes (core/Factory.h `AMFQueryVersion_Fn`/`AMFInit_Fn`).
    // `AMFQueryVersion` writes one u64 through a live pointer; `AMFInit` is passed the header
    // version capped at the runtime's own (never newer than what the runtime provides) and fills
    // `factory` with the process-global singleton only on AMF_OK (null-checked after). The module
    // is never freed, so the factory and both entry points stay valid for the process lifetime.
    // `loaded_dll_identity` only reads that module's own path + version resource (diagnostics).
    unsafe {
        let module = LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| {
                format!("amfrt64.dll not loadable (install/update the AMD driver): {e}")
            })?;
        let query_version = GetProcAddress(module, s!("AMFQueryVersion"))
            .ok_or("amfrt64.dll exports no AMFQueryVersion")?;
        let init = GetProcAddress(module, s!("AMFInit")).ok_or("amfrt64.dll exports no AMFInit")?;
        let query_version: sys::AmfQueryVersionFn = std::mem::transmute(query_version);
        let init: sys::AmfInitFn = std::mem::transmute(init);

        let mut version = 0u64;
        let r = query_version(&mut version);
        if r != sys::AMF_OK {
            return Err(format!("AMFQueryVersion failed: {} ({r})", result_name(r)));
        }
        // On-disk identity of the DLL we actually loaded (System32's amfrt64.dll, via the
        // SYSTEM32-only search above) — the Boot Camp diagnostic: the display driver can read 25.x
        // while THIS file is a stale build whose AMF + file versions lag it, so an "update the
        // driver" decline is confusing (they did — this DLL just didn't follow).
        let (dll_path, dll_file_ver) = loaded_dll_identity(module);
        let dll_desc = format!(
            "{}{}",
            dll_path.as_deref().unwrap_or("amfrt64.dll"),
            dll_file_ver
                .as_deref()
                .map(|v| format!(" (file version {v})"))
                .unwrap_or_default(),
        );
        // Accept any runtime at or above the ABI floor (AMF_MIN_VERSION): every vtable slot this
        // module mirrors predates it, so its layout is guaranteed; 1.4.35/1.4.36-only encoder
        // features are string-keyed properties that degrade via `set_prop(required=false)`, not
        // vtable changes. Below the floor the mirror is not guaranteed — decline cleanly (a clear
        // old-driver session error, never UB).
        if version < sys::AMF_MIN_VERSION {
            return Err(format!(
                "AMF runtime {amf} (loaded from {dll_desc}) is older than the minimum supported \
                 1.4.34 — update the AMD driver (Adrenalin 24.6.1+; 25.1.1+ for the \
                 fully-validated feature set). If the display driver already reports a newer \
                 version, this amfrt64.dll did not update — reboot, then DDU + reinstall so \
                 System32's copy is refreshed.",
                amf = amf_version_str(version),
            ));
        }
        // Claim no more than the runtime provides: passing a version NEWER than the runtime can
        // make AMFInit reject an otherwise-usable older driver, and this path only ever calls ABI
        // present at/below the runtime's version. On a >=1.4.36 runtime this is a no-op (== header).
        let init_version = sys::AMF_HEADER_VERSION.min(version);
        let mut factory: *mut sys::AmfFactory = ptr::null_mut();
        let r = init(init_version, &mut factory);
        if r != sys::AMF_OK {
            return Err(format!("AMFInit failed: {} ({r})", result_name(r)));
        }
        if factory.is_null() {
            return Err("AMFInit returned a null factory".into());
        }
        // Visible once per process (this runs inside `try_factory`'s OnceLock init). Both the AMF
        // runtime version AND the loaded DLL's path + file version are logged, so a field report
        // shows "display driver says 25.x but amfrt64.dll is an old build" at a glance.
        if version >= sys::AMF_HEADER_VERSION {
            tracing::info!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime loaded (meets the validated 1.4.36 baseline)"
            );
        } else {
            tracing::warn!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime is older than the validated 1.4.36 baseline — accepted (the core \
                 encode ABI is stable), but advanced features (LTR / intra-refresh recovery, AV1 \
                 coded-size alignment, in-band HDR metadata) validated on 1.4.36 may be \
                 unavailable on this driver and will degrade individually (see the per-property \
                 logs below). Update to AMD Adrenalin 25.1.1+ for the fully-validated path."
            );
        }
        Ok(AmfLib { factory, version })
    }
}

// ---------------------------------------------------------------------------------------------
// Per-codec property tables (names verified against the v1.4.36 headers —
// components/VideoEncoderVCE.h, VideoEncoderHEVC.h and VideoEncoderAV1.h; a name a pre-1.4.36
// runtime doesn't recognise is applied through `set_prop(required=false)`, which logs and
// continues, so an older driver degrades that one feature rather than failing. The enum VALUES differ
// between the codecs, e.g. CBR is 1 on AVC but 3 on HEVC/AV1, SPEED is 1 vs 10 vs 100, and AV1
// swaps the ULTRA_LOW_LATENCY/LOW_LATENCY usage values relative to AVC/HEVC).
// ---------------------------------------------------------------------------------------------

/// `AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED`.
const HEVC_HEADER_IDR_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED`.
const AV1_HEADER_KEY_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10`.
const HEVC_PROFILE_MAIN_10: i64 = 2;
/// `AMF_COLOR_BIT_DEPTH_10` (components/ColorSpace.h).
const COLOR_BIT_DEPTH_10: i64 = 10;
/// `AMF_VIDEO_ENCODER_AV1_ALIGNMENT_MODE_NO_RESTRICTIONS` / `_64X16_1080P_CODED_1082` — the AV1
/// coded-size alignment escape hatches (the driver default `64X16_ONLY` rejects heights that are
/// not multiples of 16, i.e. 1080p).
const AV1_ALIGNMENT_NO_RESTRICTIONS: i64 = 3;
const AV1_ALIGNMENT_1080P_CODED_1082: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_ENCODING_LATENCY_MODE_LOWEST_LATENCY`.
const AV1_LATENCY_LOWEST: i64 = 3;
// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM` (components/ColorSpace.h): studio-range 709 / 2020.
const COLOR_PROFILE_709: i64 = 1;
const COLOR_PROFILE_2020: i64 = 2;
// `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM` / `AMF_COLOR_PRIMARIES_ENUM` (CICP code points).
const TRANSFER_BT709: i64 = 1;
const TRANSFER_SMPTE2084: i64 = 16;
const PRIMARIES_BT709: i64 = 1;
const PRIMARIES_BT2020: i64 = 9;

/// The per-codec property/enum split between `AMFVideoEncoderVCE_AVC`, `AMFVideoEncoderHW_HEVC`
/// and `AMFVideoEncoderHW_AV1`.
struct CodecProps {
    /// `factory->CreateComponent` id.
    component: PCWSTR,
    usage: PCWSTR,
    rc_method: PCWSTR,
    /// `RATE_CONTROL_METHOD_CBR` — 1 on AVC, **3** on HEVC and AV1.
    rc_cbr: i64,
    target_bitrate: PCWSTR,
    peak_bitrate: PCWSTR,
    vbv_size: PCWSTR,
    enforce_hrd: PCWSTR,
    filler_data: PCWSTR,
    quality_preset: PCWSTR,
    /// `QUALITY_PRESET_SPEED` — 1 on AVC, **10** on HEVC, **100** on AV1.
    quality_speed: i64,
    /// Low-latency submission knob: AVC/HEVC share the literal name `L"LowLatencyInternal"`
    /// (bool); AV1 uses `Av1EncodingLatencyMode` (enum) — value in `lowlatency_value`.
    lowlatency: PCWSTR,
    /// `true` payload for the bool knob (AVC/HEVC), or the AV1 latency-mode enum value.
    lowlatency_value: AmfVariantKind,
    framerate: PCWSTR,
    /// Periodic-IDR knob: AVC `IDRPeriod` (frames), HEVC `HevcGOPSize`, AV1 `Av1GOPSize` — set to
    /// `idr_period_value` (i32::MAX for AVC/HEVC = the validated ffmpeg path's "effectively
    /// infinite GOP" value; **0** for AV1, whose header defines 0 as "key frame at first frame
    /// only"). Forced IDRs still ride the per-surface frame type.
    idr_period: PCWSTR,
    idr_period_value: i64,
    /// Per-surface forced-keyframe property + the value that means "IDR/KEY" (2 = PICTURE_TYPE_IDR
    /// on AVC/HEVC, **1** = FORCE_FRAME_TYPE_KEY on AV1).
    force_picture_type: PCWSTR,
    force_idr_value: i64,
    /// Read from the output buffer: `*_OUTPUT_DATA_TYPE_*` / `Av1OutputFrameType`. A type ≤
    /// `output_key_max` is a keyframe: IDR=0/I=1 on AVC/HEVC; on AV1 only KEY=0 counts
    /// (INTRA_ONLY=1 does not reset the reference buffers, so it is not a join point).
    output_data_type: PCWSTR,
    output_key_max: i64,
    out_color_profile: PCWSTR,
    out_transfer: PCWSTR,
    out_primaries: PCWSTR,
    /// The `*InHDRMetadata` property (an `AMFBuffer` of [`sys::AmfHdrMetadata`]) — mastering/CLL
    /// SEI (HEVC) / metadata OBU (AV1) emitted in-band by the encoder. `None` on AVC (H.264 HDR
    /// is not a thing the wire negotiates).
    hdr_metadata: Option<PCWSTR>,
    /// Intra-refresh wave: (units-per-slot property, block edge px) — AVC macroblocks (16 px),
    /// HEVC 64-px CTBs. `None` on AV1 (v1.4.36 exposes only a mode enum, no slot-size control —
    /// loss recovery stays IDR there).
    intra_refresh: Option<(PCWSTR, u32)>,
    /// LTR-RFI recovery property names (design: the AMD twin of NVENC intra-refresh recovery).
    /// `None` on AV1 — its reference management uses a frame-marking OBU mechanism this path does
    /// not drive, so LTR recovery is AVC/HEVC-only.
    ltr: Option<LtrProps>,
}

/// The four AMF LTR (long-term-reference) property names, codec-prefixed (AVC bare, HEVC `Hevc*`).
/// Two are static (`max_*`, set once at open); two are per-frame (`mark`/`force`, set on the input
/// surface each `submit`). Together they let a loss re-reference a known-good older frame — a clean
/// P-frame instead of a 20–40× IDR spike.
struct LtrProps {
    /// `MaxOfLTRFrames` — number of user LTR slots (we request [`NUM_LTR_SLOTS`]).
    max_ltr_frames: PCWSTR,
    /// `MaxNumRefFrames` — reference-picture budget; must exceed 1 for LTR to engage.
    max_num_ref_frames: PCWSTR,
    /// `MarkCurrentWithLTRIndex` (per-frame) — tag the current frame as long-term reference slot N.
    mark_ltr_index: PCWSTR,
    /// `ForceLTRReferenceBitfield` (per-frame) — force the current frame to reference only the LTR
    /// slots in the bitfield (`1<<N`), breaking the corrupted short-term chain after a loss.
    force_ltr_bitfield: PCWSTR,
}

/// The two payload shapes `lowlatency` takes across codecs.
enum AmfVariantKind {
    Bool(bool),
    I64(i64),
}

impl AmfVariantKind {
    fn to_variant(&self) -> AmfVariant {
        match self {
            AmfVariantKind::Bool(b) => AmfVariant::from_bool(*b),
            AmfVariantKind::I64(v) => AmfVariant::from_i64(*v),
        }
    }
}

fn codec_props(codec: Codec) -> CodecProps {
    match codec {
        Codec::H264 => CodecProps {
            component: w!("AMFVideoEncoderVCE_AVC"),
            usage: w!("Usage"),
            rc_method: w!("RateControlMethod"),
            rc_cbr: 1,
            target_bitrate: w!("TargetBitrate"),
            peak_bitrate: w!("PeakBitrate"),
            vbv_size: w!("VBVBufferSize"),
            enforce_hrd: w!("EnforceHRD"),
            filler_data: w!("FillerDataEnable"),
            quality_preset: w!("QualityPreset"),
            quality_speed: 1,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("FrameRate"),
            idr_period: w!("IDRPeriod"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("ForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("OutputDataType"),
            output_key_max: 1,
            out_color_profile: w!("OutColorProfile"),
            out_transfer: w!("OutColorTransferChar"),
            out_primaries: w!("OutColorPrimaries"),
            hdr_metadata: None,
            intra_refresh: Some((w!("IntraRefreshMBsNumberPerSlot"), 16)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("MaxOfLTRFrames"),
                max_num_ref_frames: w!("MaxNumRefFrames"),
                mark_ltr_index: w!("MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("ForceLTRReferenceBitfield"),
            }),
        },
        Codec::H265 => CodecProps {
            component: w!("AMFVideoEncoderHW_HEVC"),
            usage: w!("HevcUsage"),
            rc_method: w!("HevcRateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("HevcTargetBitrate"),
            peak_bitrate: w!("HevcPeakBitrate"),
            vbv_size: w!("HevcVBVBufferSize"),
            enforce_hrd: w!("HevcEnforceHRD"),
            filler_data: w!("HevcFillerDataEnable"),
            quality_preset: w!("HevcQualityPreset"),
            quality_speed: 10,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("HevcFrameRate"),
            idr_period: w!("HevcGOPSize"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("HevcForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("HevcOutputDataType"),
            output_key_max: 1,
            out_color_profile: w!("HevcOutColorProfile"),
            out_transfer: w!("HevcOutColorTransferChar"),
            out_primaries: w!("HevcOutColorPrimaries"),
            hdr_metadata: Some(w!("HevcInHDRMetadata")),
            intra_refresh: Some((w!("HevcIntraRefreshCTBsNumberPerSlot"), 64)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("HevcMaxOfLTRFrames"),
                max_num_ref_frames: w!("HevcMaxNumRefFrames"),
                mark_ltr_index: w!("HevcMarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("HevcForceLTRReferenceBitfield"),
            }),
        },
        Codec::Av1 => CodecProps {
            component: w!("AMFVideoEncoderHW_AV1"),
            usage: w!("Av1Usage"),
            rc_method: w!("Av1RateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("Av1TargetBitrate"),
            peak_bitrate: w!("Av1PeakBitrate"),
            vbv_size: w!("Av1VBVBufferSize"),
            enforce_hrd: w!("Av1EnforceHRD"),
            filler_data: w!("Av1FillerData"),
            quality_preset: w!("Av1QualityPreset"),
            quality_speed: 100,
            lowlatency: w!("Av1EncodingLatencyMode"),
            lowlatency_value: AmfVariantKind::I64(AV1_LATENCY_LOWEST),
            framerate: w!("Av1FrameRate"),
            idr_period: w!("Av1GOPSize"),
            idr_period_value: 0,
            force_picture_type: w!("Av1ForceFrameType"),
            force_idr_value: 1,
            output_data_type: w!("Av1OutputFrameType"),
            output_key_max: 0,
            out_color_profile: w!("Av1OutputColorProfile"),
            out_transfer: w!("Av1OutputColorTransferChar"),
            out_primaries: w!("Av1OutputColorPrimaries"),
            hdr_metadata: Some(w!("Av1InHDRMetadata")),
            intra_refresh: None,
            ltr: None,
        },
    }
}

/// Map `PUNKTFUNK_AMF_USAGE` (same values the ffmpeg path accepted) to the `*_USAGE_ENUM` value.
/// AVC and HEVC share the numbering; **AV1 swaps ULTRA_LOW_LATENCY (2) and LOW_LATENCY (1)**
/// relative to them (VideoEncoderAV1.h). Unknown values warn and fall back to ultra-low-latency.
fn usage_from_env(codec: Codec) -> i64 {
    let av1 = codec == Codec::Av1;
    let ull = if av1 { 2 } else { 1 };
    let v = std::env::var("PUNKTFUNK_AMF_USAGE").unwrap_or_else(|_| "ultralowlatency".into());
    match v.as_str() {
        "ultralowlatency" => ull,
        "lowlatency" => {
            if av1 {
                1
            } else {
                2
            }
        }
        "lowlatency_high_quality" => 5,
        "transcoding" => 0,
        "highquality" | "high_quality" => 4,
        other => {
            tracing::warn!(
                usage = other,
                "unknown PUNKTFUNK_AMF_USAGE — using ultralowlatency"
            );
            ull
        }
    }
}

/// Whether this session should run the **intra-refresh** loss-recovery mode (`PUNKTFUNK_INTRA_REFRESH`
/// truthy — the same opt-in the Linux NVENC path uses): a moving intra wave refreshes the whole
/// picture every [`intra_refresh_period`] frames, so FEC-unrecoverable loss heals without the
/// 20-40× full-IDR spike, and the session glue rate-limits client keyframe requests
/// ([`EncoderCaps::intra_refresh`]).
fn intra_refresh_requested() -> bool {
    std::env::var("PUNKTFUNK_INTRA_REFRESH")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Intra-refresh wave length in frames (default half a second, `PUNKTFUNK_IR_PERIOD_FRAMES`
/// overrides) — same knob and default as the Linux NVENC intra-refresh mode.
fn intra_refresh_period(fps: u32) -> u32 {
    std::env::var("PUNKTFUNK_IR_PERIOD_FRAMES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v >= 2)
        .unwrap_or_else(|| (fps.max(16) / 2).max(2))
}

/// Number of user-controlled LTR slots. AMD exposes up to 2; two rotating slots hold a sliding pair
/// of recent long-term references, so a loss can re-reference the newest one *before* the loss point.
const NUM_LTR_SLOTS: usize = 2;

/// AMD's real clean loss-recovery path (the NVENC-RFI twin): the encoder marks frames as long-term
/// references, and on loss forces a later frame to re-reference a known-good one — a clean P-frame,
/// not a 20-40× IDR spike. On by default when the driver supports it (AMF intra-refresh cannot heal —
/// no constrained-intra-prediction property exists in the API, header-confirmed + PSNR-proven — and
/// LTR is mutually exclusive with it, so LTR wins). `PUNKTFUNK_NO_AMF_LTR=1` forces the old full-IDR
/// recovery for debugging.
fn ltr_disabled() -> bool {
    std::env::var("PUNKTFUNK_NO_AMF_LTR")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Cadence (frames) between LTR marks — a fresh long-term reference roughly every half second by
/// default (`PUNKTFUNK_LTR_INTERVAL_FRAMES` overrides). With [`NUM_LTR_SLOTS`] slots this keeps ~one
/// second of recent references, so a loss up to ~1 s old still has a known-good frame to force; a
/// smaller interval means the forced reference is more recent (a smaller recovery-frame residual).
fn ltr_mark_interval(fps: u32) -> i64 {
    std::env::var("PUNKTFUNK_LTR_INTERVAL_FRAMES")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or_else(|| (fps.max(2) / 2).max(1) as i64)
}

/// Validation hook (`PUNKTFUNK_LTR_FORCE_AT=N`, spike-only): at `frame_idx == N` the encoder
/// self-triggers its real [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) path, so a
/// headless spike run can exercise LTR recovery end-to-end (mark → force → recovery-anchor tag)
/// without a live client sending an [`RfiRequest`](punktfunk_core::quic::RfiRequest). `None` normally.
fn ltr_test_force_at() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_FORCE_AT")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

// ---------------------------------------------------------------------------------------------
// Owned-pointer guards (release exactly once; Terminate before Release for context/component,
// mirroring amfenc.c's teardown order).
// ---------------------------------------------------------------------------------------------

/// Owned `AMFComponent*` — `Terminate` + `Release` on drop.
struct Component(*mut sys::AmfComponent);
impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null component `CreateComponent` returned with its own
        // reference, owned exclusively by this guard; every call goes through its runtime-provided
        // vtable on the owning thread. Flush-then-Terminate-then-Release is the teardown order
        // `reset()` and design/native-amf-encoder.md §"reset natively" use, and this drop runs
        // exactly once.
        unsafe {
            // Flush BEFORE Terminate so the VCN hardware encode session is released cleanly. An
            // un-flushed Terminate (surfaces still in flight) can leave AMD's limited VCN
            // session slots occupied for a beat, and the NEXT session's `Init` — a reconnect
            // whose teardown overlaps ours, since a client may not signal an explicit exit — then
            // opens onto a busy/wedged session that returns AMF_OK but never emits an AU. That is
            // the "second connection silently dead on AMD" symptom; NVENC has no equivalent
            // per-session cap, so it never shows. Results are best-effort (a wedged component is
            // legal to flush/terminate), logged for the teardown trace.
            ((*(*self.0).vtbl).flush)(self.0);
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF component Terminate returned non-OK on drop"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFContext*` — `Terminate` + `Release` on drop.
struct Ctx(*mut sys::AmfContext);
impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null context `CreateContext` returned with its own
        // reference, owned exclusively by this guard (every component created on it is dropped
        // first — `Inner` declares `comp` before `ctx`). Terminate releases the D3D11 device
        // binding; Release drops the last reference. Runs exactly once on the owning thread.
        unsafe {
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF context Terminate returned non-OK on drop (D3D11 device unbind)"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFData*` (an input surface or an output buffer viewed through its `AMFData` prefix) —
/// `Release` on drop.
struct OwnedData(*mut sys::AmfData);
impl Drop for OwnedData {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null AMF object pointer this guard owns one reference on
        // (from `CreateSurfaceFromDX11Native`, `QueryOutput`, or `QueryInterface` — each returns
        // an owned/AddRef'd reference); `release` is slot 2 of every AMF interface vtable, so the
        // call is valid whichever concrete interface the pointer carries. Runs exactly once.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Set one component property, distinguishing **required** (config the stream contract depends
/// on — a failure aborts the open) from **optional** (varies by VCN generation/driver — a failure
/// logs and continues, per design §3.4). Returns whether the property was actually applied, so a
/// caller can gate advertised capabilities (intra-refresh) on the driver's real answer.
unsafe fn set_prop(
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    value: AmfVariant,
    required: bool,
) -> Result<bool> {
    let r = ((*(*comp).vtbl).set_property)(comp, name.0, value);
    if r == sys::AMF_OK {
        return Ok(true);
    }
    let name = String::from_utf16_lossy(name.as_wide());
    if required {
        Err(anyhow!(
            "AMF SetProperty({name}) failed: {} ({r})",
            result_name(r)
        ))
    } else {
        tracing::debug!(
            property = %name,
            result = %format!("{} ({r})", result_name(r)),
            "optional AMF encoder property rejected (VCN generation/driver) — continuing"
        );
        Ok(false)
    }
}

// ---------------------------------------------------------------------------------------------

/// Input texture ring depth. Every submitted surface wraps a ring slot that AMF keeps reading
/// until its output is retrieved, so **at most `RING - 1` frames may be in flight** or a rotation
/// would overwrite a slot AMF is still encoding (garbage frames). `submit` enforces that bound by
/// draining output before it reuses a slot (`pending.len() < RING`), so the ring is never
/// overwritten under the encoder — the on-glass 2026-07-06 overload cascade was exactly this:
/// in-flight grew to AMF's internal input-queue limit (16) against a ring of 4. `RING - 1` also
/// sets how deep the encoder may fall behind before `submit` starts adding back-pressure latency
/// (rather than resetting), so keep it a few frames but shallow enough to stay low-latency: at
/// depth-1/2 steady state only 1-2 slots are ever live.
const RING: usize = 6;

/// Process-wide count of AMF encoder contexts brought up (`ensure_inner` bumps it on a successful
/// `Init`). Logged per bring-up so the trace distinguishes a first connection (`context #1`) from a
/// reconnect's fresh context (`context #2`, `#3`, …) — the axis the "second connection silently
/// dead on AMD" report lives on. A reconnect whose context number climbs but whose "first AU"
/// line (see [`Inner::note_first_au`]) never follows is a silent VCN-session wedge.
static AMF_CONTEXTS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The live AMF session: context + encoder component on the capturer's device, plus the owned
/// input texture ring. Field order matters: `comp` drops (Flush+Terminate+Release) before `ctx`.
struct Inner {
    comp: Component,
    ctx: Ctx,
    /// The capturer's device this session is bound to (kept alive for the ring textures).
    _device: ID3D11Device,
    /// That device's immediate context, for the ring copy (single-threaded use on this thread).
    dctx: ID3D11DeviceContext,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    /// (pts_ns, forced-IDR, recovery-anchor) per submitted-but-unretrieved frame, FIFO — the AMF
    /// encoder emits AUs in submit order (B-frames are never enabled), pairing with `QueryOutput`.
    /// The third field tags the LTR-RFI re-anchor frame so the AU carries `recovery_anchor` for the
    /// client's freeze-lift. Its length is the count of input surfaces AMF still holds, so `submit`
    /// bounds it below [`RING`] to keep the input ring from being overwritten under it.
    pending: VecDeque<(u64, bool, bool)>,
    /// AUs already pulled by `submit`'s backpressure drain, waiting to be handed out by `poll`
    /// (FIFO, strictly older than anything still in `pending`). Empty in the steady state — only
    /// fills when the encoder falls behind and `submit` drains to free an input slot.
    ready: VecDeque<EncodedFrame>,
    /// The HDR mastering metadata last pushed to THIS component (`*InHDRMetadata`), so `submit`
    /// re-pushes only on change — and a rebuilt component starts clean and gets it again.
    hdr_pushed: Option<punktfunk_core::quic::HdrMeta>,
    /// Whether this context has emitted its first AU yet — gates a single info log confirming the
    /// encoder actually produces output. Its ABSENCE after a `context #N created` line is the
    /// smoking gun for a silently-wedged reconnect (Init succeeded, VCN never encodes).
    first_au_logged: bool,
}

impl Inner {
    /// Log the first AU this context produces, exactly once. The presence of this line pairs a
    /// `context #N created` bring-up with proof the encoder is live; its absence is the diagnostic
    /// for the "no errors, just black" reconnect wedge.
    fn note_first_au(&mut self, au: &EncodedFrame) {
        if !self.first_au_logged {
            self.first_au_logged = true;
            tracing::info!(
                bytes = au.data.len(),
                keyframe = au.keyframe,
                "AMF produced its first AU on this context"
            );
        }
    }
}

pub struct AmfEncoder {
    codec: Codec,
    props: CodecProps,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// Built lazily from the first frame's device; rebuilt when the capturer's device changes
    /// (secure-desktop / HDR / resize transitions), same lifecycle as `ffmpeg_win`'s
    /// `ensure_inner_d3d11`.
    inner: Option<Inner>,
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    /// The source's static HDR mastering metadata (from the capturer via
    /// [`Encoder::set_hdr_meta`], cheap to call every frame) — pushed to the component as the
    /// `*InHDRMetadata` buffer when it changes; the encoder then emits the mastering/CLL SEI
    /// (HEVC) / metadata OBU (AV1) in-band.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// The driver accepted the intra-refresh property on the live component (design §3.4/§3.5) —
    /// gates [`EncoderCaps::intra_refresh`] so keyframe-request rate-limiting only happens when
    /// the wave really runs.
    ir_active: bool,
    // --- Long-Term-Reference reference-frame-invalidation recovery (the AMD RFI path) ---
    /// The driver accepted the LTR properties at open — gates [`EncoderCaps::supports_rfi`] and all
    /// the per-frame LTR marking/forcing below. When true, intra-refresh is NOT set (mutually
    /// exclusive) and loss recovery re-references a known-good LTR instead of forcing a full IDR.
    ltr_active: bool,
    /// The `frame_idx` currently stored in each of the two LTR slots (`None` = never marked). On loss
    /// the newest slot with an index *before* the loss is the known-good reference to force.
    ltr_slots: [Option<i64>; NUM_LTR_SLOTS],
    /// The slot the next LTR mark writes (round-robins `0,1,0,1,…` so the two slots hold a sliding
    /// pair of recent references).
    next_ltr_slot: usize,
    /// Cadence (frames) between LTR marks — a fresh long-term reference roughly this often.
    ltr_mark_interval: i64,
    /// Set by [`invalidate_ref_frames`](Encoder::invalidate_ref_frames): the LTR slot the *next*
    /// submitted frame must force-reference (`ForceLTRReferenceBitfield`). Consumed on that submit.
    pending_force: Option<usize>,
    /// Validation hook (`PUNKTFUNK_LTR_FORCE_AT=N`, spike-only): at `frame_idx == N`, self-trigger the
    /// real [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) path so a headless spike run can
    /// exercise LTR recovery end-to-end without a live client. `None` in normal operation.
    ltr_test_force_at: Option<i64>,
    /// Consecutive [`reset`](Self::reset)s that have NOT been followed by a produced AU (cleared in
    /// `poll` on any output). An in-place `Terminate`+re-`Init` heals a transient component stall,
    /// but it re-inits the SAME context — so if the fault is the context / VCN session itself (the
    /// AMD reconnect wedge), in-place recovery loops forever re-initing a dead session. Once this
    /// reaches 2, `reset` escalates to a FULL context teardown (drop `inner`) so the next submit
    /// brings up a brand-new `CreateContext`+`InitDX11` — which, once the prior session's VCN slot
    /// has drained, actually encodes. Bounded by the session's `MAX_ENCODER_RESETS` either way.
    resets_without_output: u32,
}

// SAFETY: `AmfEncoder` owns raw AMF interface pointers (context/component) and windows-rs COM
// handles (`ID3D11Device`/`ID3D11DeviceContext`/textures) that are not auto-`Send`. The session
// creates the encoder, drives `submit`/`poll`/`flush`/`reset`, and drops it all on one dedicated
// encode thread; it is never shared by reference across threads, and the D3D11 immediate context
// is only ever touched from that thread. The only cross-thread action is the initial move to the
// encode thread, after which every interior pointer is used single-threaded — the same contract
// `FfmpegWinEncoder` and `NvencD3d11Encoder` rely on.
unsafe impl Send for AmfEncoder {}

impl AmfEncoder {
    /// Open the native AMF encoder. Fails cleanly — and since Phase 3 that **fails the session**
    /// (no libavcodec fallback) — when: the AMF runtime is missing/too old, or the capture format
    /// is not the zero-copy NV12/P010 the input ring requires (video-processor fallback / CPU
    /// frames — design §3.2). AV1 support is probed here up front (RDNA3+; the codec advertisement
    /// gated on the same [`probe_can_encode`], so a client never negotiates AV1 this box can't
    /// open).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
    ) -> Result<Self> {
        // The once-per-process runtime version (and the older-than-baseline warning) is logged in
        // `load_factory`; this per-session line ties an individual encoder open to that version.
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        tracing::debug!(
            version = %amf_version_str(lib.version),
            "opening AMF encoder"
        );
        let props = codec_props(codec);
        // AV1 is RDNA3+ — probe at open (never assume), so a pre-RDNA3 box fails HERE with a clear
        // reason instead of at the first frame's opaque lazy `Init`. The advertisement gated AV1
        // on the same probe, so a client shouldn't reach here on such a box anyway. (AVC/HEVC
        // exist on every VCN generation; their Init failures are genuine driver trouble the reset
        // path handles.)
        if codec == Codec::Av1 && !probe_can_encode(Codec::Av1) {
            bail!("this GPU/driver declined AV1 encode (RDNA3+ required) — native AMF probe");
        }
        let ten_bit = bit_depth >= 10 || matches!(format, PixelFormat::P010 | PixelFormat::Rgb10a2);
        // Zero-copy by construction: the input ring is NV12/P010 fed by same-format
        // CopySubresourceRegion. Any other capture format (Bgra/Rgb10a2 video-processor fallback,
        // CPU frames) has no native input path — and since Phase 3 no ffmpeg readback to degrade
        // to, so this ends the session (the AMFVideoConverter front-end is the native fix, §3.2).
        let expected = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if format != expected {
            bail!(
                "native AMF needs the video-processor {expected:?} capture path; capturer \
                 delivered {format:?} (no readback path since Phase 3 — see the AMFVideoConverter \
                 note in §3.2)"
            );
        }
        if ten_bit && codec == Codec::H264 {
            bail!("native AMF: 10-bit is HEVC-only (H.264 High10 is not a VCN mode)");
        }
        // VCN hardware does not encode 4:4:4 (design §3.5 — permanent, not an FFmpeg limitation);
        // `can_encode_444` already answers false for AMF, so a 4:4:4 request here is a contract
        // slip — degrade loudly rather than fail the session.
        if chroma.is_444() {
            tracing::warn!("AMF cannot encode 4:4:4 (VCN hardware limit) — encoding 4:2:0");
        }
        Ok(AmfEncoder {
            codec,
            props,
            width,
            height,
            fps,
            bitrate_bps,
            ten_bit,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            hdr_meta: None,
            ir_active: false,
            ltr_active: false,
            ltr_slots: [None; NUM_LTR_SLOTS],
            next_ltr_slot: 0,
            ltr_mark_interval: ltr_mark_interval(fps),
            pending_force: None,
            ltr_test_force_at: ltr_test_force_at(),
            resets_without_output: 0,
        })
    }

    /// Whether this encoder should *attempt* the LTR-RFI recovery path (design: the AMD twin of
    /// NVENC intra-refresh recovery). Gated to AVC/HEVC — AMF exposes user LTR only for those two
    /// codecs — and defeatable via `PUNKTFUNK_NO_AMF_LTR`. Whether the driver actually *accepts* the
    /// properties is a separate question answered by [`apply_static_props`], which sets `ltr_active`.
    fn ltr_wanted(&self) -> bool {
        !ltr_disabled() && matches!(self.codec, Codec::H264 | Codec::H265)
    }

    /// The VBV/HRD buffer (bits) at `bps`: ~1 frame interval, `PUNKTFUNK_VBV_FRAMES`-scaled — the
    /// same shape every backend ships. Shared by [`apply_static_props`](Self::apply_static_props)
    /// and [`Encoder::reconfigure_bitrate`] so a dynamic retarget rescales the buffer it opened with.
    fn vbv_bits(&self, bps: u64) -> i64 {
        ((bps as f64 / self.fps.max(1) as f64) * crate::encode::vbv_frames_env())
            .clamp(1.0, i32::MAX as f64) as i64
    }

    /// Apply the static encoder configuration (design §3.4 — the native mirror of the ffmpeg
    /// opts block in `open_win_encoder`). Called before `Init`, and again on a `reset()`
    /// re-`Init` (Terminate does not guarantee property retention across every driver).
    /// Returns `(ir_active, ltr_active)`: whether the intra-refresh wave / the LTR-RFI slots were
    /// requested AND accepted by this driver. The two are mutually exclusive (LTR wins when both are
    /// wanted). The caller stores both — `ir_active` so [`Encoder::caps`] only rate-limits keyframe
    /// requests when a wave runs, `ltr_active` so [`Encoder::caps`] advertises `supports_rfi` and the
    /// per-frame mark/force logic in `submit` only fires when the slots exist.
    unsafe fn apply_static_props(&self, comp: *mut sys::AmfComponent) -> Result<(bool, bool)> {
        let p = &self.props;
        // Usage first: it "fully configures parameter set" — everything after is an override.
        set_prop(
            comp,
            p.usage,
            AmfVariant::from_i64(usage_from_env(self.codec)),
            true,
        )?;
        // CBR at target == peak, the streaming rate contract.
        set_prop(comp, p.rc_method, AmfVariant::from_i64(p.rc_cbr), true)?;
        let bps = self.bitrate_bps.min(i64::MAX as u64) as i64;
        set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(
            comp,
            p.framerate,
            AmfVariant::from_rate(self.fps.max(1), 1),
            true,
        )?;
        // ~1-frame VBV (PUNKTFUNK_VBV_FRAMES override, same knob as the ffmpeg path).
        set_prop(
            comp,
            p.vbv_size,
            AmfVariant::from_i64(self.vbv_bits(self.bitrate_bps)),
            false,
        )?;
        set_prop(comp, p.enforce_hrd, AmfVariant::from_bool(true), false)?;
        set_prop(comp, p.filler_data, AmfVariant::from_bool(false), false)?;
        // Latency-first quality; low-latency submission mode (optional — newer VCN/drivers).
        set_prop(
            comp,
            p.quality_preset,
            AmfVariant::from_i64(p.quality_speed),
            false,
        )?;
        set_prop(comp, p.lowlatency, p.lowlatency_value.to_variant(), false)?;
        // No periodic IDR (i32::MAX frames on AVC/HEVC — the validated ffmpeg path's value; 0 on
        // AV1 = "key frame at first frame only"); IDRs come from the per-surface forced type.
        set_prop(
            comp,
            p.idr_period,
            AmfVariant::from_i64(p.idr_period_value),
            false,
        )?;
        // Intra-refresh wave (Phase 2, opt-in like Linux NVENC): spread an intra band so the
        // whole picture refreshes every `period` frames — per-slot units = ceil(total blocks /
        // period). Optional by VCN generation; the return value gates `caps().intra_refresh`.
        let mut ir_active = false;
        let mut ltr_active = false;
        if let Some(ltr) = p.ltr.as_ref().filter(|_| self.ltr_wanted()) {
            // LTR-RFI recovery (design: the AMD twin of NVENC intra-refresh recovery). Request
            // NUM_LTR_SLOTS user-controlled long-term references. LTR needs >1 reference frames and
            // is MUTUALLY EXCLUSIVE with intra-refresh (AMF disables one if both are set), so the
            // intra-refresh block below is skipped whenever LTR engages.
            let ref_ok = set_prop(
                comp,
                ltr.max_num_ref_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            let ltr_ok = set_prop(
                comp,
                ltr.max_ltr_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            ltr_active = ref_ok && ltr_ok;
            if ltr_active {
                tracing::info!(
                    slots = NUM_LTR_SLOTS,
                    mark_interval = self.ltr_mark_interval,
                    "AMF LTR-RFI recovery enabled (loss recovery re-references a known-good LTR, not a full IDR)"
                );
            } else {
                tracing::warn!(
                    ref_ok,
                    ltr_ok,
                    "this VCN/driver rejected an LTR property — loss recovery stays full-IDR"
                );
            }
        } else if let Some((name, block)) = p.intra_refresh {
            if intra_refresh_requested() {
                let period = intra_refresh_period(self.fps);
                let blocks = self.width.div_ceil(block) * self.height.div_ceil(block);
                let per_slot = blocks.div_ceil(period).max(1);
                ir_active = set_prop(comp, name, AmfVariant::from_i64(per_slot as i64), false)?;
                if ir_active {
                    tracing::info!(
                        period_frames = period,
                        units_per_slot = per_slot,
                        "AMF intra-refresh wave enabled (keyframe requests will be rate-limited)"
                    );
                } else {
                    tracing::warn!(
                        "PUNKTFUNK_INTRA_REFRESH requested but this VCN/driver rejected the \
                         intra-refresh property — loss recovery stays full-IDR"
                    );
                }
            }
        }
        match self.codec {
            Codec::H264 => {
                // Never B-frames: a full frame period of latency each (RDNA3+ defaults > 0).
                set_prop(comp, w!("BPicturesPattern"), AmfVariant::from_i64(0), false)?;
                // Limited-range YUV input/output (matches the video processor's NV12).
                set_prop(
                    comp,
                    w!("FullRangeColor"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
            }
            Codec::H265 => {
                // In-band VPS/SPS/PPS on every IDR — the `EncodedFrame` wire contract (clean
                // mid-stream joins). Belt-and-braces: forced-IDR surfaces also set
                // `HevcInsertHeader` per-frame in `submit`.
                set_prop(
                    comp,
                    w!("HevcHeaderInsertionMode"),
                    AmfVariant::from_i64(HEVC_HEADER_IDR_ALIGNED),
                    false,
                )?;
                // Limited (studio) range, matching the NV12/P010 video-processor output.
                set_prop(comp, w!("HevcNominalRange"), AmfVariant::from_i64(0), false)?;
                if self.ten_bit {
                    // Main10 + 10-bit surfaces: required — a silently-8-bit HDR stream is worse
                    // than a clean open failure (which now ends the session; better a visible
                    // error than washed-out HDR).
                    set_prop(
                        comp,
                        w!("HevcProfile"),
                        AmfVariant::from_i64(HEVC_PROFILE_MAIN_10),
                        true,
                    )?;
                    set_prop(
                        comp,
                        w!("HevcColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::Av1 => {
                // Sequence header OBU on every key frame — the AV1 twin of the HEVC IDR-aligned
                // header insertion (self-contained join points on the wire).
                set_prop(
                    comp,
                    w!("Av1HeaderInsertionMode"),
                    AmfVariant::from_i64(AV1_HEADER_KEY_ALIGNED),
                    false,
                )?;
                // The driver-default alignment (64X16_ONLY) rejects heights that are not
                // 16-multiples — i.e. 1080p. Prefer unrestricted coded sizes; fall back to the
                // dedicated 1080p-coded-1082 mode for exactly that height. If neither applies,
                // Init fails and the session fails (no ffmpeg fallback since Phase 3) — but AV1 is
                // gated on the native probe up front, so an unsupported box never reaches here.
                let unrestricted = set_prop(
                    comp,
                    w!("Av1AlignmentMode"),
                    AmfVariant::from_i64(AV1_ALIGNMENT_NO_RESTRICTIONS),
                    false,
                )?;
                if !unrestricted && self.height % 16 != 0 {
                    set_prop(
                        comp,
                        w!("Av1AlignmentMode"),
                        AmfVariant::from_i64(AV1_ALIGNMENT_1080P_CODED_1082),
                        false,
                    )?;
                }
                if self.ten_bit {
                    // 10-bit is part of AV1 Main profile — only the surface depth needs forcing.
                    set_prop(
                        comp,
                        w!("Av1ColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
        }
        // Colour signalling, mirroring `open_win_encoder`: BT.709 limited (SDR) or BT.2020 PQ
        // (HDR) — VUI on AVC/HEVC, sequence-header colour config on AV1. Required when HDR — a
        // missing PQ transfer washes out every client — optional for SDR (decoders default to
        // BT.709).
        let (profile, transfer, primaries) = if self.ten_bit {
            (COLOR_PROFILE_2020, TRANSFER_SMPTE2084, PRIMARIES_BT2020)
        } else {
            (COLOR_PROFILE_709, TRANSFER_BT709, PRIMARIES_BT709)
        };
        set_prop(
            comp,
            p.out_color_profile,
            AmfVariant::from_i64(profile),
            self.ten_bit,
        )?;
        set_prop(
            comp,
            p.out_transfer,
            AmfVariant::from_i64(transfer),
            self.ten_bit,
        )?;
        set_prop(
            comp,
            p.out_primaries,
            AmfVariant::from_i64(primaries),
            self.ten_bit,
        )?;
        Ok((ir_active, ltr_active))
    }

    /// Build (or rebuild, on a capture-device change) the AMF context + encoder component on the
    /// capturer's `ID3D11Device`, plus the owned NV12/P010 input ring on that device.
    fn ensure_inner(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        // SAFETY: `lib.factory` is the live process-global factory (gated above); every vtable
        // call below goes through runtime-provided vtables on this (the encode) thread.
        // `CreateContext`/`CreateComponent` fill their out-pointers only on AMF_OK (null-checked
        // besides), and each returned object is immediately moved into a guard (`Ctx`/
        // `Component`) so every early `?`/`bail!` path releases exactly once. `InitDX11` is
        // handed `device.as_raw()` — a live `ID3D11Device` borrowed for the synchronous call;
        // AMF takes its own reference on the device internally and drops it in `Terminate`
        // (guard drop). `apply_static_props`/`init` operate on the live component; all other
        // arguments are scalars.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                "AMF CreateContext",
            )?;
            if ctx.is_null() {
                bail!("AMF CreateContext returned null");
            }
            let ctx = Ctx(ctx);
            amf_ok(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                "AMF InitDX11 (capturer device)",
            )?;
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    self.props.component.0,
                    &mut comp,
                ),
                "AMF CreateComponent",
            )?;
            if comp.is_null() {
                bail!("AMF CreateComponent returned null");
            }
            let comp = Component(comp);
            let (ir_active, ltr_active) = self.apply_static_props(comp.0)?;
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            amf_ok(
                ((*(*comp.0).vtbl).init)(comp.0, fmt, self.width as i32, self.height as i32),
                "AMF encoder Init",
            )?;
            self.ir_active = ir_active;
            // A rebuilt component starts with fresh (empty) LTR slots — a new context has no
            // reference history, so any prior marks are void and the first frame re-IDRs anyway.
            self.ltr_active = ltr_active;
            if ltr_active {
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            }

            // Owned input ring on the capturer's device (design §3.2): RENDER_TARGET |
            // SHADER_RESOURCE, the same bind flags the validated ffmpeg zero-copy pool uses.
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: if self.ten_bit {
                    DXGI_FORMAT_P010
                } else {
                    DXGI_FORMAT_NV12
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let mut t: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D (AMF input ring)")?;
                ring.push(t.context("AMF input ring texture")?);
            }
            let dctx = device
                .GetImmediateContext()
                .context("ID3D11Device immediate context")?;
            // Bump AFTER a successful Init — a bring-up that failed above never counts. The
            // sequence number is the reconnect axis: `context #1` is the first connection, `#2+`
            // are reconnects; a climbing number with no following "first AU" line is the silent
            // AMD wedge.
            let context_no =
                AMF_CONTEXTS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::info!(
                codec = ?self.codec,
                context = context_no,
                device = format!("{:#x}", device.as_raw() as usize),
                "native AMF encode active (context #{context_no}, {}x{}@{}, zero-copy D3D11 {} ring, runtime {}.{}.{})",
                self.width,
                self.height,
                self.fps,
                if self.ten_bit { "P010" } else { "NV12" },
                (lib.version >> 48) & 0xffff,
                (lib.version >> 32) & 0xffff,
                (lib.version >> 16) & 0xffff,
            );
            self.inner = Some(Inner {
                comp,
                ctx,
                _device: device.clone(),
                dctx,
                ring,
                next: 0,
                pending: VecDeque::new(),
                ready: VecDeque::new(),
                hdr_pushed: None,
                first_au_logged: false,
            });
            Ok(())
        }
    }
}

/// Push the source's static HDR mastering metadata to a live component: allocate a host
/// `AMFBuffer` holding one [`sys::AmfHdrMetadata`], and set it as the `*InHDRMetadata` property
/// (dynamic — settable mid-stream). The encoder then emits the ST.2086 mastering + CLL data
/// in-band (HEVC prefix SEI on IDRs / AV1 metadata OBUs), so any decoder — stock Moonlight
/// included — tone-maps from the source's real grade. [`HdrMeta`]'s units match the struct's
/// exactly; only the primary order changes (ST.2086 wire order G,B,R → labeled R/G/B fields).
///
/// # Safety
/// `ctx` and `comp` must be the live context/component pair owned by the calling encoder, used
/// only on its encode thread.
unsafe fn push_hdr_metadata(
    ctx: *mut sys::AmfContext,
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    meta: &punktfunk_core::quic::HdrMeta,
) -> Result<()> {
    let mut buf: *mut sys::AmfBuffer = ptr::null_mut();
    amf_ok(
        ((*(*ctx).vtbl).alloc_buffer)(
            ctx,
            sys::AMF_MEMORY_HOST,
            std::mem::size_of::<sys::AmfHdrMetadata>(),
            &mut buf,
        ),
        "AMF AllocBuffer(HDR metadata)",
    )?;
    if buf.is_null() {
        bail!("AMF AllocBuffer(HDR metadata) returned null");
    }
    // Release via the AMFData-prefix guard (slot 2 is Release on every AMF vtable). SetProperty
    // stores its own AddRef'd copy of the interface, so dropping our reference afterwards leaves
    // the property alive on the component.
    let guard = OwnedData(buf as *mut sys::AmfData);
    let native = ((*(*buf).vtbl).get_native)(buf) as *mut sys::AmfHdrMetadata;
    if native.is_null() {
        bail!("AMF HDR metadata buffer has no host pointer");
    }
    // A host AMFBuffer's memory is heap-allocated (alignment unknown to us) — write unaligned.
    native.write_unaligned(sys::AmfHdrMetadata {
        red_primary: meta.display_primaries[2],
        green_primary: meta.display_primaries[0],
        blue_primary: meta.display_primaries[1],
        white_point: meta.white_point,
        max_mastering_luminance: meta.max_display_mastering_luminance,
        min_mastering_luminance: meta.min_display_mastering_luminance,
        max_content_light_level: meta.max_cll,
        max_frame_average_light_level: meta.max_fall,
    });
    let r = ((*(*comp).vtbl).set_property)(
        comp,
        name.0,
        AmfVariant::from_interface(guard.0 as *mut c_void),
    );
    amf_ok(r, "AMF SetProperty(InHDRMetadata)")
}

/// Native factory probe (design §4, replacing the ffmpeg open-probe for AMF): can this GPU's AMF
/// runtime actually open a `codec` encoder? Creates a context on the **selected render adapter**
/// (the GPU the session will encode on), creates the codec's component, and `Init`s a tiny
/// encoder — the driver rejects codecs the video engine can't do (AV1 on pre-RDNA3, HEVC on
/// pre-VCN parts). Everything is torn down before returning. `false` on any failure, including
/// no AMF runtime — the caller ([`super::windows_codec_support`]) then consults the libavcodec
/// fallback probe when that path is built.
pub fn probe_can_encode(codec: Codec) -> bool {
    let Some(device) = selected_adapter_device() else {
        return false;
    };
    probe_can_encode_on(&device, codec)
}

/// [`probe_can_encode`] against an explicit device (separated so the live tests can pin the AMD
/// adapter on a hybrid box).
fn probe_can_encode_on(device: &ID3D11Device, codec: Codec) -> bool {
    if try_factory().is_err() {
        return false;
    }
    let props = codec_props(codec);
    // SAFETY: same contracts as `ensure_inner`: the factory is live (gated above); every created
    // object is moved into a guard (`Ctx`/`Component`) immediately, so each early return releases
    // exactly once; `InitDX11` borrows the live `device` for the synchronous call (AMF holds its
    // own device reference until the guard's Terminate). Usage must be set before `Init` (the
    // header marks its default "N/A") — the probe mirrors the session's open order.
    unsafe {
        let Ok(lib) = try_factory() else { return false };
        let mut ctx: *mut sys::AmfContext = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx) != sys::AMF_OK
            || ctx.is_null()
        {
            return false;
        }
        let ctx = Ctx(ctx);
        if ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1) != sys::AMF_OK {
            return false;
        }
        let mut comp: *mut sys::AmfComponent = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_component)(
            lib.factory,
            ctx.0,
            props.component.0,
            &mut comp,
        ) != sys::AMF_OK
            || comp.is_null()
        {
            return false;
        }
        let comp = Component(comp);
        if ((*(*comp.0).vtbl).set_property)(
            comp.0,
            props.usage.0,
            AmfVariant::from_i64(usage_from_env(codec)),
        ) != sys::AMF_OK
        {
            return false;
        }
        ((*(*comp.0).vtbl).init)(comp.0, sys::AMF_SURFACE_NV12, 640, 480) == sys::AMF_OK
    }
}

/// A D3D11 device on the **selected render adapter** (web-console preference /
/// `PUNKTFUNK_RENDER_ADAPTER` / max VRAM — the GPU the capture ring and encoder sit on), the
/// same resolution `nvenc::probe_can_encode_444` uses; falls back to the OS default hardware
/// adapter when the selection can't be resolved.
fn selected_adapter_device() -> Option<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    // SAFETY: a self-contained probe owning every handle it creates. `CreateDXGIFactory1`/
    // `EnumAdapterByLuid` return owned COM objects or err (→ default-adapter fallback).
    // `D3D11CreateDevice` (explicit adapter + UNKNOWN driver type, or NULL adapter + HARDWARE)
    // fills `device` only on success. Everything drops with its COM wrapper.
    unsafe {
        let adapter: Option<IDXGIAdapter1> = crate::win_adapter::resolve_render_adapter_luid()
            .and_then(|luid| {
                let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
                factory.EnumAdapterByLuid(luid).ok()
            });
        let mut device: Option<ID3D11Device> = None;
        let created = match &adapter {
            Some(a) => D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
            None => D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
        };
        if created.is_err() {
            return None;
        }
        device
    }
}

/// Outcome of one `QueryOutput` call ([`drain_one_output`]).
enum DrainOutcome {
    /// A finished AU, already FIFO-paired with its `pending` entry.
    Frame(EncodedFrame),
    /// The encoder has no output yet (AMF_OK / AMF_REPEAT / AMF_NEED_MORE_INPUT with a null data).
    NotReady,
    /// End of stream after a `Drain`/`Flush` (AMF_EOF).
    Eof,
}

/// Pull ONE finished AU via a single `QueryOutput`, FIFO-pairing it with the oldest `pending`
/// entry (the encoder emits in submit order — B-frames are never enabled). Shared by [`poll`]
/// (the bounded output spin) and [`submit`] (the backpressure drain), so a free fn taking the raw
/// pieces rather than `&mut self` — that lets `submit` call it while already holding `&mut Inner`.
///
/// # Safety
/// `comp` must be the live encoder component, `pending` its FIFO, and the call must run on the
/// single encode thread with no other AMF call to this component in flight.
unsafe fn drain_one_output(
    comp: *mut sys::AmfComponent,
    pending: &mut VecDeque<(u64, bool, bool)>,
    output_data_type: PCWSTR,
    output_key_max: i64,
) -> Result<DrainOutcome> {
    // SAFETY (per the fn contract): `QueryOutput` fills `data` with an owned reference only when
    // it returns one; a non-null `data` is immediately moved into `OwnedData` (released exactly
    // once). `get_property` writes a zero-initialised 24-byte `AmfVariant` we own.
    // `QueryInterface(IID_AMFBuffer)` returns an AddRef'd `AMFBuffer*` (released via its own
    // guard — `release` is slot 2 of every AMF vtable). `GetNative`/`GetSize` describe the
    // buffer's host memory, valid until that buffer's release: the `from_raw_parts` slice is
    // copied to a `Vec` BEFORE the guards drop at scope end.
    let mut data: *mut sys::AmfData = ptr::null_mut();
    let r = ((*(*comp).vtbl).query_output)(comp, &mut data);
    if data.is_null() {
        return match r {
            sys::AMF_EOF => Ok(DrainOutcome::Eof),
            sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_NEED_MORE_INPUT => Ok(DrainOutcome::NotReady),
            // A typed failure ON THE FRAME it happens (device-lost etc.) — the caller's
            // error path resets in place.
            other => bail!("AMF QueryOutput failed: {} ({other})", result_name(other)),
        };
    }
    let data = OwnedData(data);
    // Keyframe: the encoder stamps *_OUTPUT_DATA_TYPE on the output (IDR=0/I=1 on AVC/HEVC, KEY=0
    // on AV1); OR with our forced flag so a driver that skips the property still flags the IDRs
    // we forced.
    let mut var = AmfVariant::zeroed();
    let key_prop = ((*(*data.0).vtbl).get_property)(data.0, output_data_type.0, &mut var)
        == sys::AMF_OK
        && var.as_i64().is_some_and(|t| t <= output_key_max);
    let mut buf: *mut c_void = ptr::null_mut();
    amf_ok(
        ((*(*data.0).vtbl).query_interface)(data.0, &sys::IID_AMF_BUFFER, &mut buf),
        "AMF QueryInterface(AMFBuffer)",
    )?;
    if buf.is_null() {
        bail!("AMF output is not an AMFBuffer");
    }
    // Release via the AMFData-prefix guard: slot 2 is Release on every vtable.
    let buf_guard = OwnedData(buf as *mut sys::AmfData);
    let buf = buf_guard.0 as *mut sys::AmfBuffer;
    let size = ((*(*buf).vtbl).get_size)(buf);
    let native = ((*(*buf).vtbl).get_native)(buf);
    if native.is_null() || size == 0 {
        bail!("AMF output buffer is empty");
    }
    let au = std::slice::from_raw_parts(native as *const u8, size).to_vec();
    let (pts_ns, forced, recovery_anchor) = pending.pop_front().unwrap_or((0, false, false));
    Ok(DrainOutcome::Frame(EncodedFrame {
        data: au,
        pts_ns,
        keyframe: key_prop || forced,
        recovery_anchor,
    }))
}

/// How long `submit` will drain output waiting for the encoder to free an input slot before it
/// declares a genuine wedge and escalates to the session loop's in-place reset. Generous relative
/// to a single frame's encode time (so a merely-behind encoder rides it out with added latency,
/// never a reset) yet far under the session watchdog's ~2 s floor.
const INPUT_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

impl Encoder for AmfEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!("native AMF is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        // Mid-session video-processor fallback (Bgra/Rgb10a2): the NV12/P010 ring can't accept a
        // different format group (CopySubresourceRegion would be UB). No native readback exists —
        // error out; the session's bounded reset/teardown handles it (and since Phase 3 there is
        // no ffmpeg path to inherit the session, so persistent fallback ends it — see the §3.2
        // AMFVideoConverter note).
        let expected = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        anyhow::ensure!(
            captured.format == expected,
            "captured format {:?} != AMF input ring {:?} (capturer video-processor fallback \
             mid-session — native AMF has no readback path)",
            captured.format,
            expected
        );
        self.ensure_inner(&frame.device)?;
        let cur_idx = self.frame_idx;
        // A component's FIRST submission must be a forced IDR (stream-start contract: in-band
        // headers + LTR re-anchor). Detected via the fresh ring counter, NOT `frame_idx == 0`:
        // `submit_indexed` pins frame_idx to the wire index, which is non-zero when a mid-session
        // rebuild (bitrate step / reset escalation) brings a new component up.
        let opening = self.inner.as_ref().is_none_or(|i| i.next == 0);
        let forced = std::mem::take(&mut self.force_kf) || opening;
        let pts_100ns = self.frame_idx * 10_000_000 / self.fps.max(1) as i64;
        self.frame_idx += 1;
        // --- LTR-RFI per-frame decisions (design: the AMD twin of NVENC intra-refresh recovery) ---
        // Decided here, before borrowing `inner`, because the test hook re-enters `&mut self`
        // (`invalidate_ref_frames`) and the mark cadence mutates the slot bookkeeping. The two
        // per-frame property names are copied out (PCWSTR is Copy) so the unsafe surface block can
        // set them without re-borrowing `self.props` under the live `inner` borrow.
        let ltr_names = self
            .props
            .ltr
            .as_ref()
            .map(|l| (l.mark_ltr_index, l.force_ltr_bitfield));
        let mut mark_slot: Option<usize> = None;
        let mut force_slot: Option<usize> = None;
        let mut recovery_anchor = false;
        if self.ltr_active {
            if forced {
                // An IDR resets the decoder's reference buffers — every prior LTR mark is void.
                // Re-anchor from scratch: drop the stale slots (the mark cadence below tags the IDR
                // as the first fresh long-term reference) and cancel any force queued against them.
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            } else if self.ltr_test_force_at == Some(cur_idx) {
                // Spike-only validation hook: self-trigger the real invalidate path so a headless
                // run exercises mark → force → recovery-anchor without a live client's RfiRequest.
                let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
                tracing::info!(
                    frame = cur_idx,
                    triggered,
                    "AMF LTR test hook fired invalidate_ref_frames"
                );
            }
            // Apply a queued force (from invalidate_ref_frames / the test hook) to THIS frame: it
            // becomes the clean re-anchor P-frame the client lifts its post-loss freeze on.
            if let Some(slot) = self.pending_force.take() {
                force_slot = Some(slot);
                recovery_anchor = true;
            }
            // Mark cadence: refresh a long-term reference on every IDR and every `ltr_mark_interval`
            // frames — but never on the recovery frame itself (marking rotates `next_ltr_slot` and
            // could overwrite the very slot being forced; the next cadence mark re-establishes it).
            if force_slot.is_none() && (forced || cur_idx % self.ltr_mark_interval == 0) {
                let slot = self.next_ltr_slot;
                self.ltr_slots[slot] = Some(cur_idx);
                self.next_ltr_slot = (self.next_ltr_slot + 1) % NUM_LTR_SLOTS;
                mark_slot = Some(slot);
            }
        }
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Push the HDR mastering metadata when it changed (or a rebuilt component lost it) — a
        // dynamic property, so mid-stream regrades take effect on the next IDR. Best-effort: a
        // rejecting driver leaves the client on the out-of-band 0xCE metadata datagram.
        if let Some(name) = self.props.hdr_metadata {
            if self.ten_bit && inner.hdr_pushed != self.hdr_meta {
                if let Some(m) = self.hdr_meta {
                    // SAFETY: `inner.ctx.0`/`inner.comp.0` are this encoder's live context/
                    // component pair, used on the encode thread — exactly the contract
                    // `push_hdr_metadata` documents.
                    match unsafe { push_hdr_metadata(inner.ctx.0, inner.comp.0, name, &m) } {
                        Ok(()) => tracing::debug!(
                            "AMF HDR mastering metadata attached (in-band on keyframes)"
                        ),
                        Err(e) => tracing::warn!(
                            error = %format!("{e:#}"),
                            "AMF rejected the HDR mastering metadata — no in-band SEI/OBU"
                        ),
                    }
                }
                inner.hdr_pushed = self.hdr_meta;
            }
        }
        // Bound in-flight surfaces below the ring depth BEFORE reusing a slot. Every submitted
        // surface wraps `ring[slot]` and AMF keeps reading it until its output is retrieved, so
        // with more than `RING - 1` frames outstanding, rotating onto `next % RING` would
        // overwrite a slot the encoder is still encoding. When the encoder falls behind (its input
        // queue backs up under overload), drain finished AUs — buffered for `poll` — to free a
        // slot, instead of overrunning the ring or (the pre-fix bug) letting `SubmitInput` hit
        // AMF_INPUT_FULL and tearing the encoder down + forcing an IDR, which only compounded the
        // overload. A drain that makes NO progress for the whole budget is a genuine wedge:
        // escalate to the session loop's in-place reset.
        if inner.pending.len() >= RING {
            let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
            while inner.pending.len() >= RING {
                // SAFETY: `inner.comp.0` is the live component and `inner.pending` its FIFO,
                // touched only on this (encode) thread with no other AMF call to it in flight —
                // `drain_one_output`'s contract. A pulled AU moves into `inner.ready` for `poll`.
                match unsafe {
                    drain_one_output(
                        inner.comp.0,
                        &mut inner.pending,
                        self.props.output_data_type,
                        self.props.output_key_max,
                    )
                }? {
                    DrainOutcome::Frame(f) => inner.ready.push_back(f),
                    DrainOutcome::Eof => break,
                    DrainOutcome::NotReady => {
                        if std::time::Instant::now() >= deadline {
                            self.force_kf = true;
                            bail!(
                                "AMF produced no output for {} ms with {} frame(s) in flight — \
                                 wedged (escalating to reset)",
                                INPUT_DRAIN_BUDGET.as_millis(),
                                inner.pending.len()
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_micros(250));
                    }
                }
            }
        }
        let slot = inner.next % RING;
        inner.next += 1;
        // SAFETY: `src` (the captured texture) and `dst` (our ring slot) are same-format
        // (checked above), same-size (checked above) textures on the SAME device — the ring was
        // created on `frame.device` and `ensure_inner` rebuilds on any device change — so
        // `CopySubresourceRegion` on that device's single-threaded immediate context (only ever
        // used from this thread) is a valid whole-subresource GPU copy.
        // `CreateSurfaceFromDX11Native` wraps the ring texture WITHOUT owning it (null observer);
        // the returned surface holds its own AMF reference and is moved into `OwnedData`, so it
        // is released exactly once on every path below; the wrapped texture outlives it in
        // `inner.ring`. `SetPts`/`SetProperty` are prefix-vtable calls on that live surface.
        // `SubmitInput` passes the surface as its `AMFData` base (same object pointer — single
        // inheritance); AMF AddRefs internally what it keeps, so our release does not free a
        // buffer in flight.
        unsafe {
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = inner.ring[slot].cast().context("ring -> resource")?;
            inner
                .dctx
                .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None);

            let mut surf: *mut sys::AmfData = ptr::null_mut();
            amf_ok(
                ((*(*inner.ctx.0).vtbl).create_surface_from_dx11_native)(
                    inner.ctx.0,
                    inner.ring[slot].as_raw(),
                    &mut surf,
                    ptr::null_mut(),
                ),
                "AMF CreateSurfaceFromDX11Native",
            )?;
            if surf.is_null() {
                bail!("AMF CreateSurfaceFromDX11Native returned null");
            }
            let surf = OwnedData(surf);
            ((*(*surf.0).vtbl).set_pts)(surf.0, pts_100ns);
            if forced {
                // Forced IDR/KEY + in-band headers on the surface (per-submission properties).
                // Log-and-continue: a rejecting driver still encodes; the client's keyframe
                // re-request and the watchdog arbitrate a miss.
                let r = ((*(*surf.0).vtbl).set_property)(
                    surf.0,
                    self.props.force_picture_type.0,
                    AmfVariant::from_i64(self.props.force_idr_value),
                );
                if r != sys::AMF_OK {
                    tracing::warn!(
                        result = %format!("{} ({r})", result_name(r)),
                        "AMF forced-keyframe picture type rejected"
                    );
                }
                match self.codec {
                    Codec::H264 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertSPS").0,
                            AmfVariant::from_bool(true),
                        );
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertPPS").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    Codec::H265 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("HevcInsertHeader").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    // The static KEY_FRAME_ALIGNED header-insertion mode already puts a sequence
                    // header OBU on every key frame; there is no per-surface twin.
                    Codec::Av1 => {}
                }
            }
            // LTR-RFI per-frame properties (design: the AMD twin of NVENC intra-refresh recovery).
            // `mark_slot`/`force_slot` were decided above. Marking tags the current frame as a
            // long-term reference; forcing makes it re-reference a known-good LTR — a clean P-frame
            // that breaks the corrupted short-term chain after a loss, no 20-40× IDR. Best-effort:
            // a rejecting driver just leaves the client on its keyframe-request fallback.
            if let Some((mark_name, force_name)) = ltr_names {
                if let Some(slot) = mark_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        mark_name.0,
                        AmfVariant::from_i64(slot as i64),
                    );
                    if r != sys::AMF_OK {
                        tracing::warn!(
                            slot,
                            result = %format!("{} ({r})", result_name(r)),
                            "AMF LTR mark rejected"
                        );
                    }
                }
                if let Some(slot) = force_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        force_name.0,
                        AmfVariant::from_i64(1_i64 << slot),
                    );
                    if r == sys::AMF_OK {
                        tracing::info!(
                            slot,
                            frame = cur_idx,
                            "AMF LTR-RFI: re-referencing known-good LTR (clean recovery, no IDR)"
                        );
                    } else {
                        tracing::warn!(
                            slot,
                            result = %format!("{} ({r})", result_name(r)),
                            "AMF LTR force-reference rejected — client stays frozen until its IDR fallback"
                        );
                    }
                }
            }
            let mut r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
            // Backstop back-pressure: the in-flight bound above already keeps a slot free, but if
            // AMF's own input queue is momentarily full, AMF_INPUT_FULL is "busy, drain me and
            // retry" — NOT a wedge. Drain output (buffered for `poll`) to free a slot and re-submit
            // the SAME surface, bounded. Only a no-progress budget expiry escalates to a reset —
            // the on-glass overload cascade was this signal being treated as a wedge instead.
            if r == sys::AMF_INPUT_FULL {
                let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
                loop {
                    match drain_one_output(
                        inner.comp.0,
                        &mut inner.pending,
                        self.props.output_data_type,
                        self.props.output_key_max,
                    )? {
                        DrainOutcome::Frame(f) => inner.ready.push_back(f),
                        DrainOutcome::Eof => break,
                        DrainOutcome::NotReady => {
                            std::thread::sleep(std::time::Duration::from_micros(250))
                        }
                    }
                    r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
                    if r != sys::AMF_INPUT_FULL || std::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
            match r {
                // NEED_MORE_INPUT = accepted, no AU owed for this submission alone.
                sys::AMF_OK | sys::AMF_NEED_MORE_INPUT => {}
                // Stayed full for the whole drain budget despite freeing the ring — a genuine
                // wedge. Err → the session loop's submit-failure path runs the in-place reset.
                sys::AMF_INPUT_FULL => {
                    self.force_kf = true; // retried frame stays an IDR candidate
                    bail!("AMF SubmitInput stayed AMF_INPUT_FULL past the drain budget — wedged");
                }
                other => {
                    self.force_kf = true;
                    bail!("AMF SubmitInput failed: {} ({other})", result_name(other));
                }
            }
        }
        inner
            .pending
            .push_back((captured.pts_ns, forced, recovery_anchor));
        Ok(())
    }

    /// Pin this submission's frame number to the wire frame index its AU will carry (see the
    /// trait doc): the LTR slots then store WIRE indexes, so [`invalidate_ref_frames`]'s
    /// pre-loss check (`slot < first`, both in client frame numbers) stays correct across every
    /// encoder rebuild/reset — an internal counter desyncs on the first adaptive-bitrate rebuild,
    /// making the check vacuously true and risking a force-reference to an LTR marked INSIDE the
    /// lost range (a corrupted frame shipped as a clean recovery anchor). `frame_idx` also feeds
    /// the AMF SetPts; a re-pin only ever moves it backward across a reset (fresh component, so a
    /// pts restart is harmless) and forward on a rebuild (monotonic within any one component).
    ///
    /// [`invalidate_ref_frames`]: Encoder::invalidate_ref_frames
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        // Stored; pushed to the component (as the `*InHDRMetadata` buffer) on the next submit
        // when it changed. Cheap to call every frame, exactly like the NVENC path.
        self.hdr_meta = meta;
    }

    /// LTR-RFI recovery (the AMD twin of the Windows NVENC `nvEncInvalidateRefFrames` path): a loss
    /// of client frames `[first, last]` is answered by forcing the *next* submitted frame to
    /// re-reference the newest long-term reference marked *before* the loss — a clean P-frame the
    /// client can decode against a picture it still holds, instead of a 20-40× IDR spike.
    ///
    /// Returns `true` when a usable pre-loss LTR exists (so the caller must NOT also force an IDR);
    /// `false` when the loss predates every live LTR — then the only correct recovery is a keyframe,
    /// and the caller falls back to [`request_keyframe`](Self::request_keyframe). Runs on the encode
    /// thread (like submit/poll); the force is applied on the next `submit`.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live LTR session (driver declined the slots, or AV1 which has no user-LTR path) or a
        // nonsense range → caller forces a full IDR.
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        // Pick the newest LTR strictly OLDER than the loss: the most recent known-good reference the
        // client still holds, so re-referencing it costs the least (smallest recovery-frame residual).
        // `ltr_slots` store the WIRE frame index of the marked frame (`submit_indexed` pins
        // `frame_idx` to it per submission), so they compare directly against the client's `first`
        // — and stay comparable across encoder rebuilds/resets, where an internal counter would
        // make this check vacuous and risk force-referencing an LTR marked INSIDE the lost range.
        let mut best: Option<(usize, i64)> = None;
        for (slot, marked) in self.ltr_slots.iter().enumerate() {
            if let Some(idx) = *marked {
                if idx < first && best.is_none_or(|(_, b)| idx > b) {
                    best = Some((slot, idx));
                }
            }
        }
        match best {
            Some((slot, ltr_frame)) => {
                // Queue the force for the next submit; that frame ships tagged `recovery_anchor`.
                self.pending_force = Some(slot);
                tracing::info!(
                    first,
                    last,
                    slot,
                    ltr_frame,
                    "AMF LTR-RFI: forcing the next frame to re-reference a known-good LTR (no IDR)"
                );
                true
            }
            None => {
                tracing::info!(
                    first,
                    last,
                    "AMF LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            // LTR-RFI: AMD's reference invalidation is the user long-term-reference path (mark a
            // frame, force a later one to re-reference it). True only when the live driver accepted
            // the LTR slots at open — otherwise loss recovery falls back to a full IDR.
            supports_rfi: self.ltr_active,
            // In-band mastering/CLL via `*InHDRMetadata` (HEVC SEI / AV1 metadata OBU); AVC has
            // no such property (and no HDR sessions negotiate H.264).
            supports_hdr_metadata: self.ten_bit && self.props.hdr_metadata.is_some(),
            // Permanent: VCN hardware does not encode 4:4:4.
            chroma_444: false,
            // True only when `PUNKTFUNK_INTRA_REFRESH` asked for the wave AND the live driver
            // accepted the property (queried per loss event, so the post-first-frame value is
            // what the session glue's IDR rate-limiting sees).
            intra_refresh: self.ir_active,
            // Not yet: the AMD VCN wave heals in principle, but its constrained-GDR
            // heal-within-a-period is unvalidated on-glass and AMF emits no recovery-point SEI, so
            // the host keeps the IDR recovery path. Flip both once verified on real hardware.
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    /// Bounded-blocking poll (the `vaapi.rs::poll` model, design §3.3): spin `QueryOutput` with
    /// ~250 µs sleeps up to `min(3/4 frame interval, 12 ms)`, so the AU ships the same tick the
    /// ASIC finishes (~1–5 ms) — this is where the libavcodec path's ~2-frame hold dies. On
    /// expiry return `Ok(None)`: the session loop keeps the frame in flight and the encode-stall
    /// watchdog arbitrates a real wedge. Hands out any AUs `submit`'s back-pressure drain already
    /// buffered (older than anything still in `pending`) before querying for new ones.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let odt = self.props.output_data_type;
        let okm = self.props.output_key_max;
        // Pull one AU (buffered or freshly queried) with the inner borrow scoped, so the produced
        // AU can clear `resets_without_output` on `self` afterward without a borrow conflict.
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            // Back-pressure-buffered AUs first (strictly older than anything still in `pending`).
            if let Some(au) = inner.ready.pop_front() {
                inner.note_first_au(&au);
                Some(au)
            } else {
                let budget = std::time::Duration::from_micros(750_000 / self.fps.max(1) as u64)
                    .min(std::time::Duration::from_millis(12));
                let deadline = std::time::Instant::now() + budget;
                let mut out = None;
                loop {
                    // SAFETY: `inner.comp.0` is the live component and `inner.pending` its FIFO,
                    // used only on this (encode) thread with no other AMF call to it in flight —
                    // `drain_one_output`'s documented contract.
                    match unsafe { drain_one_output(inner.comp.0, &mut inner.pending, odt, okm) }? {
                        DrainOutcome::Frame(au) => {
                            inner.note_first_au(&au);
                            out = Some(au);
                            break;
                        }
                        // Drained (post-`Drain`): nothing further is owed.
                        DrainOutcome::Eof => {
                            inner.pending.clear();
                            break;
                        }
                        DrainOutcome::NotReady => {}
                    }
                    // Not ready: only wait while a frame is actually owed, ~250 µs between checks.
                    if inner.pending.is_empty() || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
                out
            }
        };
        // Any produced AU proves this context encodes — clear the no-output reset streak so a
        // later, unrelated stall starts fresh at the cheap in-place recovery.
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// Encode-stall recovery (design §3.5), cheaper than the ffmpeg path's drop-and-reopen:
    /// discard the wedged pipeline (`Flush`), `Terminate` the component, and re-`Init` it on the
    /// same context. If the in-place rebuild fails, fall back to full teardown — the next
    /// `submit` rebuilds context + component lazily, exactly like first-frame bring-up. Either
    /// way the owed AUs are forfeited and the next frame is a forced IDR.
    ///
    /// In-place re-`Init` reuses the SAME context, so it can't clear a fault that lives in the
    /// context / VCN session (the AMD reconnect wedge: Init returns OK but the hardware session
    /// never encodes). [`resets_without_output`](Self::resets_without_output) counts resets not
    /// followed by an AU; once it reaches 2 this escalates to a FULL context teardown so the next
    /// submit brings up a fresh `CreateContext`+`InitDX11` on a (by then) drained VCN slot.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        if self.inner.is_none() {
            return true; // nothing live — the next submit rebuilds lazily
        }
        // Escalate: an in-place re-Init already ran without producing an AU, so the fault is the
        // context itself — tear it fully down and reopen fresh instead of re-initing a dead session
        // in a loop until MAX_ENCODER_RESETS ends the whole session. Checked before borrowing
        // `inner` so this can drop `self.inner`.
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "AMF stall persisted across in-place re-Init — full context teardown, reopening a \
                 fresh context (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner is Some — checked above and not cleared since");
        inner.pending.clear();
        inner.ready.clear(); // owed + buffered AUs are forfeited; the rebuilt stream restarts at IDR
        inner.hdr_pushed = None; // a re-Init'd component needs the HDR metadata again
                                 // SAFETY: `inner.comp.0` is the live component, used only on this thread with no AMF
                                 // call in flight (the session loop is synchronous). `Flush` discards queued frames,
                                 // `Terminate` tears the hw session down (both legal on a wedged component — results are
                                 // deliberately ignored), `apply_static_props` + `init` then rebuild it; each call goes
                                 // through the runtime's vtable.
        let rebuilt = unsafe {
            let comp = inner.comp.0;
            ((*(*comp).vtbl).flush)(comp);
            ((*(*comp).vtbl).terminate)(comp);
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            match self.apply_static_props(comp) {
                Ok((ir, ltr)) => {
                    self.ir_active = ir;
                    // Re-Init voids the reference history: the rebuilt stream restarts at IDR with
                    // empty LTR slots, so any prior marks are stale and must be dropped.
                    self.ltr_active = ltr;
                    self.ltr_slots = [None; NUM_LTR_SLOTS];
                    self.next_ltr_slot = 0;
                    self.pending_force = None;
                    ((*(*comp).vtbl).init)(comp, fmt, self.width as i32, self.height as i32)
                        == sys::AMF_OK
                }
                Err(_) => false,
            }
        };
        if rebuilt {
            tracing::info!(
                "AMF encoder rebuilt in place (Terminate + re-Init on the same context)"
            );
        } else {
            self.ir_active = false;
            self.ltr_active = false;
            // Full teardown; the next submit reopens context + component on the current device.
            tracing::warn!("AMF in-place re-Init failed — full context teardown, reopening lazily");
            self.inner = None;
            self.bound_device = 0;
        }
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps_i = bps.min(i64::MAX as u64) as i64;
        let vbv = self.vbv_bits(bps);
        let Some(inner) = self.inner.as_ref() else {
            // Nothing live yet — the lazy open applies the new rate via `apply_static_props`.
            self.bitrate_bps = bps;
            return true;
        };
        // `TargetBitrate`/`PeakBitrate`/`VBVBufferSize` are DYNAMIC AMF properties (runtime-
        // changeable on AVC/HEVC/AV1 alike): a SetProperty on the live component retargets the
        // rate controller with no Terminate/re-Init — the reference chain, LTR slots and
        // in-flight frames all survive (no IDR).
        // SAFETY: `inner.comp.0` is the live component, used only on this thread with no AMF
        // call in flight (the session loop is synchronous); `set_prop` is a prefix-vtable call.
        let applied = unsafe {
            let p = &self.props;
            let comp = inner.comp.0;
            let ok = set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps_i), false)
                .unwrap_or(false)
                && set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps_i), false)
                    .unwrap_or(false);
            if ok {
                // Rescale the VBV with the rate. Optional, like at open — a driver that declines
                // keeps the old buffer (a size mismatch the HRD absorbs), not worth a rebuild.
                let _ = set_prop(comp, p.vbv_size, AmfVariant::from_i64(vbv), false);
            }
            ok
        };
        if !applied {
            // A half-applied pair doesn't matter: the caller's rebuild fallback re-authors
            // everything from scratch.
            tracing::warn!(
                mbps = bps / 1_000_000,
                "AMF declined the dynamic bitrate retarget — falling back to a rebuild"
            );
            return false;
        }
        self.bitrate_bps = bps; // future reset()/re-Init paths re-apply the new rate
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // SAFETY: `inner.comp.0` is the live component on the owning thread; `Drain` signals
        // end-of-stream (remaining AUs then surface through `poll` until AMF_EOF).
        let r = unsafe { ((*(*inner.comp.0).vtbl).drain)(inner.comp.0) };
        if r != sys::AMF_OK {
            tracing::debug!(result = %format!("{} ({r})", result_name(r)), "AMF Drain");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirrored `AMFVariantStruct` must match the C layout: 4-byte tag + 4 padding + 16-byte
    /// union = 24 bytes, align 8, payload at offset 8 (it is passed BY VALUE across the FFI).
    #[test]
    fn variant_layout_matches_c() {
        assert_eq!(std::mem::size_of::<AmfVariant>(), 24);
        assert_eq!(std::mem::align_of::<AmfVariant>(), 8);
        assert_eq!(std::mem::offset_of!(AmfVariant, payload), 8);
        let v = AmfVariant::from_rate(60, 1);
        assert_eq!(v.payload[0], 60u64 | (1u64 << 32));
        assert_eq!(AmfVariant::from_i64(-1).payload[0], u64::MAX);
    }

    /// `AMFGuid` is the flattened Win32-GUID layout (16 bytes).
    #[test]
    fn guid_layout_matches_c() {
        assert_eq!(std::mem::size_of::<sys::AmfGuid>(), 16);
    }

    /// `AMFHDRMetadata` (components/ColorSpace.h): 8×u16 + 2×u32 + 2×u16 = 28 bytes, no padding.
    #[test]
    fn hdr_metadata_layout_matches_c() {
        assert_eq!(std::mem::size_of::<sys::AmfHdrMetadata>(), 28);
        assert_eq!(
            std::mem::offset_of!(sys::AmfHdrMetadata, max_mastering_luminance),
            16
        );
        assert_eq!(
            std::mem::offset_of!(sys::AmfHdrMetadata, max_content_light_level),
            24
        );
    }

    /// A representative HDR10 grade for the live tests (BT.2020 primaries, 1000-nit mastering)
    /// in [`HdrMeta`]'s ST.2086 wire units/order (primaries G, B, R).
    fn sample_hdr_meta() -> punktfunk_core::quic::HdrMeta {
        punktfunk_core::quic::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 1000 * 10000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        }
    }

    /// Find the AMD adapter and create a D3D11 device on it (the live tests' stand-in for the
    /// capturer's device). `None` = no AMD GPU here — the caller skips.
    fn amd_d3d11_device() -> Option<ID3D11Device> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
        const VENDOR_AMD: u32 = 0x1002;
        // SAFETY: a self-contained probe owning every handle it creates: `CreateDXGIFactory1` /
        // `EnumAdapters1` / `GetDesc1` return owned COM objects or err; `D3D11CreateDevice`
        // (explicit adapter + UNKNOWN driver type) fills `device` only on success. Everything
        // drops with its COM wrapper; nothing runs concurrently.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            for i in 0.. {
                let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
                let desc = adapter.GetDesc1().ok()?;
                if desc.VendorId != VENDOR_AMD {
                    continue;
                }
                let mut device: Option<ID3D11Device> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    Default::default(),
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .ok()?;
                return device;
            }
            None
        }
    }

    /// A DEFAULT-usage NV12 texture on `device` — the live tests' stand-in for the capturer's
    /// video-processor output (uninitialised GPU memory; content is irrelevant to the timing /
    /// pipeline contract).
    fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `CreateTexture2D` fills the out-param only on success; the live `device` and the
        // returned texture are owned COM handles used on this thread only.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("NV12 texture");
        tex.expect("NV12 texture")
    }

    /// The `p`-quantile of `samples` (µs), sorting in place. `0` when empty.
    fn percentile(samples: &mut [u128], p: f64) -> u128 {
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        let idx = (((samples.len() - 1) as f64) * p).round() as usize;
        samples[idx]
    }

    /// Drive `enc` at the real frame cadence and return each frame's **submit→AU** wall-clock
    /// (µs) — the `encode_us` the punktfunk1 loop records. Mirrors the depth-1 loop exactly:
    /// pace to `1/fps`, timestamp the submit, then drain whatever AUs are ready and FIFO-pair
    /// them to their submit stamps. The libavcodec AMF wrapper's ~2-frame output hold therefore
    /// shows up here as ~2 frame periods (the AU for frame N emerges only once N+2 is submitted),
    /// exactly as it does in production; the native bounded poll returns each AU the same tick,
    /// so its submit→AU is the bare ASIC time. The last ~2 unflushed frames on the ffmpeg path
    /// are left unmeasured (dropped with the encoder) so every recorded sample is a genuine paced
    /// submit→AU.
    #[allow(clippy::too_many_arguments)]
    fn drive_and_measure(
        enc: &mut dyn Encoder,
        device: &ID3D11Device,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
        fps: u32,
        fmt: PixelFormat,
        frames: usize,
    ) -> Vec<u128> {
        use std::time::{Duration, Instant};
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let mut pending: VecDeque<Instant> = VecDeque::new();
        let mut samples: Vec<u128> = Vec::new();
        let mut next = Instant::now();
        for i in 0..frames {
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            next += interval;
            let frame = CapturedFrame {
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: fmt,
                payload: FramePayload::D3d11(crate::capture::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                }),
            };
            let t = Instant::now();
            enc.submit(&frame).expect("bench submit");
            pending.push_back(t);
            // Depth-1 drain: pull every ready AU, FIFO-paired to its submit stamp.
            while let Some(_au) = enc.poll().expect("bench poll") {
                let ts = pending.pop_front().expect("FIFO pairing");
                samples.push(ts.elapsed().as_micros());
            }
        }
        samples
    }

    /// The design's **§5.2 latency A/B** made runnable on the lab iGPU: drive the native AMF
    /// encoder and the libavcodec-AMF encoder with the SAME paced D3D11 NV12 input and compare
    /// `encode_us` (submit→AU). The whole justification for this backend is that the libavcodec
    /// wrapper holds ~2 frames (measured 36 ms p50 at 720p60 on this Ryzen iGPU) while the native
    /// bounded poll ships each AU the same tick — so the native p50 must collapse below one frame
    /// period and land far under the ffmpeg path. Opt-in (`PUNKTFUNK_AMF_BENCH=1`, ~6 s of paced
    /// encode) and gated on the `amf-qsv` feature so the ffmpeg comparator is built; skips cleanly
    /// without the AMD runtime/GPU.
    #[cfg(feature = "amf-qsv")]
    #[test]
    fn amf_latency_ab_bench() {
        if std::env::var("PUNKTFUNK_AMF_BENCH").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set PUNKTFUNK_AMF_BENCH=1 to run the native-vs-ffmpeg latency A/B"
            );
            return;
        }
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (1920u32, 1080u32, 60u32);
        let bitrate = 20_000_000u64;
        let frames = 180usize;
        let tex = nv12_texture(&device, w, h);

        let mut native = AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            bitrate,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("native AMF open");
        let mut native_us = drive_and_measure(
            &mut native,
            &device,
            &tex,
            w,
            h,
            fps,
            PixelFormat::Nv12,
            frames,
        );
        drop(native);

        let mut ffmpeg = crate::encode::ffmpeg_win::FfmpegWinEncoder::open(
            crate::encode::ffmpeg_win::WinVendor::Amf,
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            bitrate,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("libavcodec AMF open");
        let mut ffmpeg_us = drive_and_measure(
            &mut ffmpeg,
            &device,
            &tex,
            w,
            h,
            fps,
            PixelFormat::Nv12,
            frames,
        );
        drop(ffmpeg);

        let iv = 1_000_000u128 / fps as u128;
        let (n50, n99, nc) = (
            percentile(&mut native_us, 0.50),
            percentile(&mut native_us, 0.99),
            native_us.len(),
        );
        let (f50, f99, fc) = (
            percentile(&mut ffmpeg_us, 0.50),
            percentile(&mut ffmpeg_us, 0.99),
            ffmpeg_us.len(),
        );
        eprintln!("=== native AMF vs libavcodec-AMF  encode_us A/B ===");
        eprintln!("mode: {w}x{h}@{fps} HEVC, {frames} paced frames, frame period {iv} us");
        eprintln!(
            "native (direct SDK) : p50={n50} us  p99={n99} us  ({nc} AUs)  = {:.2} frame periods",
            n50 as f64 / iv as f64
        );
        eprintln!(
            "ffmpeg (libavcodec) : p50={f50} us  p99={f99} us  ({fc} AUs)  = {:.2} frame periods",
            f50 as f64 / iv as f64
        );
        if n50 > 0 {
            eprintln!(
                "native p50 is {:.1}x lower than ffmpeg",
                f50 as f64 / n50 as f64
            );
        }
        // The core §5.2 claim: the native path retrieves faster than the libavcodec 2-frame hold,
        // and its per-frame encode_us collapses below one frame period (where the ~2-frame hold
        // used to live). Both are wide-margin on this hardware (design measured ~36 ms vs ~1-5 ms).
        assert!(
            n50 < f50,
            "native encode_us p50 ({n50}) must beat the libavcodec hold ({f50})"
        );
        assert!(
            n50 < iv,
            "native encode_us p50 ({n50} us) should collapse below one frame period ({iv} us)"
        );
    }

    /// Live end-to-end encode through the full [`Encoder`] path on the AMD GPU (design §5.1's
    /// open/probe smoke), per codec: open on an NV12 D3D11 frame, submit + poll a batch, then
    /// exercise the native `reset()` (the encode-stall watchdog's recovery lever — Flush +
    /// Terminate + re-Init) and a second batch, then `flush`-drain. Asserts the stream contract:
    /// Annex-B start codes, IDR keyframes at stream start and after the reset, FIFO pts pairing.
    /// Skips cleanly without the AMD runtime/GPU.
    #[test]
    fn amf_encode_live_smoke() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);

        for codec in [Codec::H265, Codec::H264, Codec::Av1] {
            // AV1 is RDNA3+: consult the native probe on THIS device (the selected-adapter probe
            // inside `open` may resolve a different GPU on a hybrid box).
            if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
                eprintln!("skipping Av1: this AMD GPU's native probe declined it (pre-RDNA3?)");
                continue;
            }
            let mut enc = match AmfEncoder::open(
                codec,
                PixelFormat::Nv12,
                w,
                h,
                fps,
                2_000_000,
                8,
                ChromaFormat::Yuv420,
            ) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("skipping {codec:?}: native AMF open declined ({e:#})");
                    continue;
                }
            };
            let batch = |enc: &mut AmfEncoder, base: u64, n: usize| -> Vec<EncodedFrame> {
                let mut aus = Vec::new();
                for i in 0..n {
                    let frame = CapturedFrame {
                        width: w,
                        height: h,
                        pts_ns: base + i as u64,
                        format: PixelFormat::Nv12,
                        payload: FramePayload::D3d11(crate::capture::dxgi::D3d11Frame {
                            texture: tex.clone(),
                            device: device.clone(),
                        }),
                    };
                    enc.submit(&frame).expect("submit");
                    if let Some(au) = enc.poll().expect("poll") {
                        aus.push(au);
                    }
                }
                aus
            };
            let first_run = batch(&mut enc, 1, 6);
            // Native in-place reset (design §3.5): owed AUs forfeited, next frame a fresh IDR.
            assert!(enc.reset(), "native reset must report rebuilt");
            let mut second_run = batch(&mut enc, 100, 6);
            enc.flush().expect("flush");
            for _ in 0..50 {
                match enc.poll().expect("drain poll") {
                    Some(au) => second_run.push(au),
                    None => break,
                }
            }
            assert!(
                first_run.len() >= 3 && second_run.len() >= 3,
                "{codec:?}: expected most AUs out (got {} + {})",
                first_run.len(),
                second_run.len()
            );
            for run in [&first_run, &second_run] {
                let first = &run[0];
                assert!(
                    first.keyframe,
                    "{codec:?}: stream/reset start must be an IDR"
                );
                if codec == Codec::Av1 {
                    // AV1 is an OBU stream, not Annex-B — just require substance.
                    assert!(!first.data.is_empty(), "Av1: empty key AU");
                } else {
                    assert!(
                        first.data.starts_with(&[0, 0, 0, 1]) || first.data.starts_with(&[0, 0, 1]),
                        "{codec:?}: AU must be Annex-B (got {:02x?})",
                        &first.data[..first.data.len().min(8)]
                    );
                }
            }
            assert_eq!(first_run[0].pts_ns, 1, "FIFO pts pairing");
            assert_eq!(second_run[0].pts_ns, 100, "post-reset FIFO pts pairing");
            eprintln!(
                "live AMF {codec:?} encode: {} + {} AUs across a native reset, first IDR {} bytes",
                first_run.len(),
                second_run.len(),
                first_run[0].data.len()
            );
        }
    }

    /// Live native codec probe (design §4): on a box with the AMD runtime, AVC and HEVC must
    /// probe true (every VCN generation encodes both); AV1's answer is hardware truth (RDNA3+).
    #[test]
    fn amf_native_probe_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let h264 = probe_can_encode_on(&device, Codec::H264);
        let h265 = probe_can_encode_on(&device, Codec::H265);
        let av1 = probe_can_encode_on(&device, Codec::Av1);
        eprintln!("native AMF probe: h264={h264} h265={h265} av1={av1}");
        assert!(h264 && h265, "every VCN generation encodes AVC + HEVC");
    }

    /// Live HDR path: 10-bit P010 HEVC Main10 with the mastering metadata attached — the AU must
    /// encode, and the IDR should carry the in-band mastering-display / content-light-level
    /// prefix SEI (NAL type 39, payload types 137/144). The SEI presence is soft-reported (VCN
    /// generations may differ); the encode contract is the hard assertion.
    #[test]
    fn amf_hdr_encode_live_smoke() {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `CreateTexture2D` fills the out-param only on success; owned COM handles on
        // this thread only.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
        let tex = tex.expect("P010 texture");
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            w,
            h,
            fps,
            4_000_000,
            10,
            ChromaFormat::Yuv420,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF 10-bit open declined ({e:#})");
                return;
            }
        };
        enc.set_hdr_meta(Some(sample_hdr_meta()));
        assert!(
            enc.caps().supports_hdr_metadata,
            "HEVC 10-bit reports HDR SEI capability"
        );
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..6 {
            let frame = CapturedFrame {
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(crate::capture::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                }),
            };
            enc.submit(&frame).expect("submit (P010)");
            if let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        assert!(!aus.is_empty(), "10-bit HDR encode produced no AUs");
        let idr = &aus[0];
        assert!(idr.keyframe, "first AU must be an IDR");
        // Scan the IDR for HEVC prefix-SEI NALs (NUH bytes 0x4E 0x01 after a start code) with
        // payload type 137 (mastering display) / 144 (content light level).
        let mut mastering = false;
        let mut cll = false;
        for i in 0..idr.data.len().saturating_sub(5) {
            let d = &idr.data[i..];
            let nal = if d.starts_with(&[0, 0, 1]) {
                &d[3..]
            } else if d.starts_with(&[0, 0, 0, 1]) {
                &d[4..]
            } else {
                continue;
            };
            if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 {
                match nal[2] {
                    137 => mastering = true,
                    144 => cll = true,
                    _ => {}
                }
            }
        }
        eprintln!(
            "live AMF HEVC Main10 HDR: {} AUs, IDR {} bytes, mastering SEI={mastering}, CLL SEI={cll}",
            aus.len(),
            idr.data.len()
        );
        if !mastering {
            eprintln!("note: no mastering-display SEI found on this VCN/driver — client falls back to the 0xCE datagram");
        }
    }

    /// Live intra-refresh: open with `PUNKTFUNK_INTRA_REFRESH` requested via the property path
    /// directly — `apply_static_props` returns whether the driver accepted the wave, which is
    /// exactly what `caps().intra_refresh` reports to the IDR rate-limiter. Env-var independent:
    /// drives the property through a scratch component rather than mutating process env.
    #[test]
    fn amf_intra_refresh_property_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let Ok(lib) = try_factory() else { return };
        // SAFETY: same contracts as `probe_can_encode_on` — guards own every created object;
        // `set_prop` runs against the live component on this thread.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            assert_eq!(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                sys::AMF_OK
            );
            let ctx = Ctx(ctx);
            assert_eq!(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                sys::AMF_OK
            );
            for codec in [Codec::H264, Codec::H265] {
                let props = codec_props(codec);
                let mut comp: *mut sys::AmfComponent = ptr::null_mut();
                if ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    props.component.0,
                    &mut comp,
                ) != sys::AMF_OK
                    || comp.is_null()
                {
                    eprintln!("skipping {codec:?}: component unavailable");
                    continue;
                }
                let comp = Component(comp);
                let _ = set_prop(
                    comp.0,
                    props.usage,
                    AmfVariant::from_i64(usage_from_env(codec)),
                    true,
                );
                let (name, block) = props.intra_refresh.expect("AVC/HEVC define intra-refresh");
                let blocks = 640u32.div_ceil(block) * 480u32.div_ceil(block);
                let per_slot = blocks.div_ceil(30).max(1);
                let applied = set_prop(comp.0, name, AmfVariant::from_i64(per_slot as i64), false)
                    .expect("optional set_prop never errors");
                eprintln!(
                    "intra-refresh {codec:?}: {per_slot} units/slot accepted={applied} on this VCN"
                );
            }
        }
    }

    /// Back-pressure / ring-bound (the 2026-07-06 on-glass overload fix): submit a burst FASTER
    /// than the encoder drains — no polling between submits — so the in-flight count hits the ring
    /// bound and `submit` must drain output to free slots (buffering into `ready`) instead of
    /// erroring on AMF_INPUT_FULL and resetting. Asserts every AU survives, IDR-first, in strict
    /// FIFO pts order across the ready→pending boundary: no ring overwrite (corruption would not
    /// change ordering but a reset would drop the run), no lost/reordered frames, no wedge bail.
    #[test]
    fn amf_backpressure_burst_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        const BURST: u64 = 48; // >> RING, submitted faster than the ASIC drains
        for i in 1..=BURST {
            let frame = CapturedFrame {
                width: w,
                height: h,
                pts_ns: i,
                format: PixelFormat::Nv12,
                payload: FramePayload::D3d11(crate::capture::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                }),
            };
            // No poll between submits: the in-flight bound in `submit` must drain to make room,
            // never error. A reset cascade (the old behaviour) would surface as a submit error here.
            enc.submit(&frame)
                .expect("burst submit must ride back-pressure, not error");
        }
        enc.flush().expect("flush");
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for _ in 0..(BURST as usize + 100) {
            match enc.poll().expect("drain poll") {
                Some(au) => aus.push(au),
                None => break,
            }
        }
        assert!(
            aus.len() as u64 >= BURST - 2,
            "most AUs must survive the burst without a reset (got {} of {BURST})",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be the IDR");
        for pair in aus.windows(2) {
            assert!(
                pair[1].pts_ns > pair[0].pts_ns,
                "AUs must stay FIFO-monotonic across the ready→pending boundary: {} then {}",
                pair[0].pts_ns,
                pair[1].pts_ns
            );
        }
        eprintln!(
            "back-pressure burst: {} AUs, FIFO-monotonic, IDR-first — ring bound held, no reset",
            aus.len()
        );
    }

    /// Live-gated FFI smoke test (design §6): load the runtime, check the version gate, create a
    /// context (AMF's own device — no adapter pinning needed for a layout check) and an HEVC
    /// encoder component through the mirrored vtables. Skips cleanly on a box without the AMD
    /// runtime; any layout error in the mirror would crash rather than fail politely, so a clean
    /// pass/skip is the assertion.
    #[test]
    fn amf_factory_probe_smoke() {
        let lib = match try_factory() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: AMF runtime unavailable ({e})");
                return;
            }
        };
        assert!(lib.version >= sys::AMF_MIN_VERSION);
        // SAFETY: same contracts as `ensure_inner`, minus the external device: `CreateContext`
        // fills `ctx` only on AMF_OK; `InitDX11(null)` asks AMF to create its own D3D11 device
        // (may fail on exotic boxes — treated as a skip); `CreateComponent` likewise. Guards
        // release every created object exactly once.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx);
            assert_eq!(r, sys::AMF_OK, "CreateContext: {}", result_name(r));
            assert!(!ctx.is_null());
            let ctx = Ctx(ctx);
            let r = ((*(*ctx.0).vtbl).init_dx11)(ctx.0, ptr::null_mut(), sys::AMF_DX11_1);
            if r != sys::AMF_OK {
                eprintln!(
                    "skipping: InitDX11(default device) failed ({})",
                    result_name(r)
                );
                return;
            }
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_component)(
                lib.factory,
                ctx.0,
                w!("AMFVideoEncoderHW_HEVC").0,
                &mut comp,
            );
            if r != sys::AMF_OK || comp.is_null() {
                // A probe answer (no HEVC VCN on this adapter), not a mirror failure.
                eprintln!("note: CreateComponent(HEVC) declined ({})", result_name(r));
                return;
            }
            let _comp = Component(comp);
        }
    }
}
