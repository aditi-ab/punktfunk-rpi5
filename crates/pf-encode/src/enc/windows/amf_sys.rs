//! Mirrored AMF C ABI (written against GPUOpen header release v1.4.36 — amf/public/include; every
//! slot below is a base-interface slot whose layout is stable since <= v1.4.34, the loader's
//! accepted ABI floor, so the mirror is valid on every runtime the loader admits).
//!
//! Layout rules this mirror relies on: every AMF interface is a struct whose sole member is a
//! pointer to a C vtable; derived interfaces PREPEND their base's slots in order (AMFInterface →
//! AMFPropertyStorage → AMFData → AMFBuffer/AMFSurface), so a derived pointer is usable through a
//! base vtable mirror. Slots we never call are declared as bare `*const c_void` placeholders —
//! same size/alignment as the function pointer they stand in for. `AMF_STD_CALL` is `__stdcall`
//! (Rust `extern "system"`); the two DLL entry points are `__cdecl` (`extern "C"`); on x86_64
//! both collapse to the one Windows calling convention.

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
    pub get_property:
        unsafe extern "system" fn(*mut AmfComponent, *const u16, *mut AmfVariant) -> AmfResult,
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
    pub query_output: unsafe extern "system" fn(*mut AmfComponent, *mut *mut AmfData) -> AmfResult,
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
    pub set_property: unsafe extern "system" fn(*mut AmfData, *const u16, AmfVariant) -> AmfResult,
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

// -- Layout guards ---------------------------------------------------------------------------
//
// THE CONTRACT, STATED ONCE. Everything above is a hand-written mirror of a C type this crate
// does not own and cannot include. Two classes of drift are possible and NEITHER fails to
// compile on its own:
//
//   1. A POD passed by value (`AmfVariant`, `AmfGuid`, `AmfHdrMetadata`) whose field offsets
//      disagree with the C struct. The runtime then reads a tag or a payload out of the wrong
//      bytes — `AmfVariant` crosses the FFI by value on EVERY `SetProperty`.
//   2. A vtable slot inserted, removed or reordered. `amf.rs` dispatches BY POSITION through
//      these mirrors, so a shifted slot calls an arbitrary function pointer through a
//      mismatched signature. There is no compile error, no runtime signal, and the failure is
//      whatever the neighbouring AMF entry point happens to do with our arguments.
//
// `AMF_MIN_VERSION` does not defend against either: it checks a version NUMBER, not a layout,
// and it is a floor with no ceiling. The assertions below are the actual defence. They are
// `const _: ()` rather than `#[cfg(test)]` deliberately — the three POD checks below used to
// live only in `amf.rs`'s test module, which means they were verified only when someone ran
// pf-encode's tests, on Windows, with AMF enabled, and NEVER in a release build. This is the
// same hole `a8dd348b` closed for the cuda.h mirrors; it was missed here.
//
// Every slot index below was counted against the vtable declarations above. A slot is asserted
// when `amf.rs` calls it — those are the ones whose displacement is directly exploitable — plus
// the total size of each table, which catches an insertion PAST the last called slot (invisible
// to a per-slot check, but still a sign the mirror has drifted from the header).

/// One vtable slot. Every mirrored table is a flat array of these, so an offset in bytes is
/// always `index * SLOT`.
const SLOT: usize = core::mem::size_of::<Slot>();

/// Byte offset of vtable slot `i`. A `const fn` rather than a bare `i * SLOT` expression because
/// clippy's `erasing_op`/`identity_op` reject `0 * SLOT` and `1 * SLOT` under the `-D warnings`
/// the Windows CI leg runs with — and writing those two as bare `0` and `SLOT` would be the one
/// place the slot INDEX stops being visible, which is the entire readability of these assertions.
const fn slot(i: usize) -> usize {
    i * SLOT
}

// Every slot is a plain code pointer, so all five tables are pointer-sized-array-shaped. If this
// ever fails, the tables are not flat arrays any more and every offset below is meaningless.
const _: () = assert!(SLOT == core::mem::size_of::<usize>());
const _: () = assert!(core::mem::align_of::<Slot>() == core::mem::align_of::<usize>());

// -- PODs crossing the FFI by value --
// `AMFVariantStruct`: 4-byte tag + 4 padding + 16-byte union = 24 bytes, payload at 8.
const _: () = assert!(core::mem::size_of::<AmfVariant>() == 24);
const _: () = assert!(core::mem::align_of::<AmfVariant>() == 8);
const _: () = assert!(core::mem::offset_of!(AmfVariant, payload) == 8);
// `AMFGuid`: the flattened Win32 GUID.
const _: () = assert!(core::mem::size_of::<AmfGuid>() == 16);
const _: () = assert!(core::mem::align_of::<AmfGuid>() == 4);
// `AMFHDRMetadata` (components/ColorSpace.h): 8×u16 + 2×u32 + 2×u16 = 28 bytes, no padding.
const _: () = assert!(core::mem::size_of::<AmfHdrMetadata>() == 28);
const _: () = assert!(core::mem::offset_of!(AmfHdrMetadata, max_mastering_luminance) == 16);
const _: () = assert!(core::mem::offset_of!(AmfHdrMetadata, max_content_light_level) == 24);

// -- AMFFactory (7 slots) — `create_context` 0, `create_component` 1 --
const _: () = assert!(core::mem::size_of::<AmfFactoryVtbl>() == slot(7));
const _: () = assert!(core::mem::offset_of!(AmfFactoryVtbl, create_context) == slot(0));
const _: () = assert!(core::mem::offset_of!(AmfFactoryVtbl, create_component) == slot(1));

// -- AMFContext (55 slots) = AMFInterface(3) + AMFPropertyStorage(10) + AMFContext(42) --
const _: () = assert!(core::mem::size_of::<AmfContextVtbl>() == slot(55));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, terminate) == slot(13));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, init_dx11) == slot(18));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, alloc_buffer) == slot(43));
const _: () =
    assert!(core::mem::offset_of!(AmfContextVtbl, create_surface_from_dx11_native) == slot(49));

// -- AMFComponent (28 slots) = AMFInterface(3) + PropertyStorage(10) + StorageEx(4) + Component(11) --
const _: () = assert!(core::mem::size_of::<AmfComponentVtbl>() == slot(28));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, set_property) == slot(3));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, init) == slot(17));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, terminate) == slot(19));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, drain) == slot(20));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, flush) == slot(21));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, submit_input) == slot(22));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, query_output) == slot(23));

// -- AMFData (23 slots) = AMFInterface(3) + AMFPropertyStorage(10) + AMFData(10) --
const _: () = assert!(core::mem::size_of::<AmfDataVtbl>() == slot(23));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, query_interface) == slot(2));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, set_property) == slot(3));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, get_property) == slot(4));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, set_pts) == slot(19));

// -- AMFBuffer (28 slots) = the AMFData prefix (23) + AMFBuffer(5) --
const _: () = assert!(core::mem::size_of::<AmfBufferVtbl>() == slot(28));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, get_size) == slot(24));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, get_native) == slot(25));

// -- The shared-prefix agreement --
// `AMFBuffer` derives from `AMFData`, and `create_surface_from_dx11_native` hands back an
// `AMFSurface*` that this module drives through the `AmfData` mirror on the strength of that
// single-inheritance prefix (see the comment on that slot). If the two mirrors ever disagree
// about where a shared slot lives, that reinterpretation is silently wrong — so assert the
// agreement rather than restating it in prose.
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, release) == core::mem::offset_of!(AmfBufferVtbl, release)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, set_property)
        == core::mem::offset_of!(AmfBufferVtbl, set_property)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, get_property)
        == core::mem::offset_of!(AmfBufferVtbl, get_property)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, set_pts) == core::mem::offset_of!(AmfBufferVtbl, set_pts)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, get_duration)
        == core::mem::offset_of!(AmfBufferVtbl, get_duration)
);

// -- DLL entry points (core/Factory.h; AMF_CDECL_CALL) --------------------------------------
pub type AmfQueryVersionFn = unsafe extern "C" fn(*mut u64) -> AmfResult;
pub type AmfInitFn = unsafe extern "C" fn(u64, *mut *mut AmfFactory) -> AmfResult;
