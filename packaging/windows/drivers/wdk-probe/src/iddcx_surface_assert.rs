//! M1 step-2 de-risk: a compile-time assertion that the WHOLE IddCx driver surface the port needs is
//! actually emitted by the wdk-sys `iddcx` bindgen and RESOLVES (every `*2`/HDR struct's field types,
//! the inbound callback PFN typedefs, and the struct-size/version machinery). Per the port-plan critique
//! this turns "the `(?i).*iddcx.*` allowlist may miss a symbol (e.g. DISPLAYCONFIG_* embedded by
//! IDDCX_TARGET_MODE, or LUID)" from a STEP-2/3 surprise on the ephemeral box into a CI compile error on
//! the persistent runner. No runtime effect — `const _` asserts only.
#![allow(dead_code)]

use wdk_sys::iddcx;

/// Every struct the driver constructs/reads must be `Sized` — which fails to compile if any field type
/// (DISPLAYCONFIG_VIDEO_SIGNAL_INFO, LUID, GUID, DXGI_*, …) isn't in scope. This is the real test that
/// the binding is complete for the full driver, not just adapter init.
macro_rules! assert_sized {
    ($($t:ident),+ $(,)?) => { $( const _: usize = core::mem::size_of::<iddcx::$t>(); )+ };
}

assert_sized!(
    // adapter / device init
    IDD_CX_CLIENT_CONFIG,
    IDARG_IN_ADAPTER_INIT,
    IDARG_OUT_ADAPTER_INIT,
    IDDCX_ADAPTER_CAPS,
    IDDCX_ENDPOINT_VERSION,
    // monitor create / arrival
    IDARG_IN_MONITORCREATE,
    IDARG_OUT_MONITORCREATE,
    IDARG_OUT_MONITORARRIVAL,
    IDDCX_MONITOR_INFO,
    // mode reporting — v1 + *2 (these embed DISPLAYCONFIG_* and were the critique's flagged risk)
    IDDCX_MONITOR_MODE,
    IDDCX_MONITOR_MODE2,
    IDDCX_TARGET_MODE,
    IDDCX_TARGET_MODE2,
    IDDCX_PATH,
    IDDCX_PATH2,
    IDARG_IN_PARSEMONITORDESCRIPTION,
    IDARG_OUT_PARSEMONITORDESCRIPTION,
    IDARG_IN_QUERYTARGETMODES,
    IDARG_OUT_QUERYTARGETMODES,
    IDARG_IN_COMMITMODES,
    // swap-chain + frame acquire (HDR *2 path)
    IDARG_IN_SETSWAPCHAIN,
    IDARG_IN_ADAPTERSETRENDERADAPTER,
    IDARG_IN_RELEASEANDACQUIREBUFFER2,
    IDARG_OUT_RELEASEANDACQUIREBUFFER2,
    IDDCX_METADATA2,
);

/// Every inbound `IDD_CX_CLIENT_CONFIG` callback type must exist and be a nullable `extern fn` (`Option`).
macro_rules! assert_pfn {
    ($($t:ident),+ $(,)?) => { $( const _: iddcx::$t = None; )+ };
}

assert_pfn!(
    PFN_IDD_CX_DEVICE_IO_CONTROL,
    PFN_IDD_CX_ADAPTER_INIT_FINISHED,
    PFN_IDD_CX_ADAPTER_COMMIT_MODES,
    PFN_IDD_CX_ADAPTER_COMMIT_MODES2,
    PFN_IDD_CX_PARSE_MONITOR_DESCRIPTION,
    PFN_IDD_CX_PARSE_MONITOR_DESCRIPTION2,
    PFN_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES,
    PFN_IDD_CX_MONITOR_QUERY_TARGET_MODES,
    PFN_IDD_CX_MONITOR_QUERY_TARGET_MODES2,
    PFN_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN,
    PFN_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN,
    PFN_IDD_CX_MONITOR_SET_GAMMA_RAMP,
    PFN_IDD_CX_MONITOR_SET_DEFAULT_HDR_METADATA,
    PFN_IDD_CX_ADAPTER_QUERY_TARGET_INFO,
);

/// The versioned struct-size machinery (`IDD_STRUCTURE_SIZE!` in the oracle) needs these three to exist
/// and link — confirming the port can size `IDD_CX_CLIENT_CONFIG` correctly against the live framework
/// rather than guessing `size_of`. `IddStructures`/`IddStructureCount` are stub-provided statics.
const _: fn() = || {
    let _structs = &raw const iddcx::IddStructures;
    let _count = &raw const iddcx::IddStructureCount;
    let _higher = &raw const iddcx::IddClientVersionHigherThanFramework;
    // the per-struct size-table indices the macro uses
    let _i0 = iddcx::_IDDSTRUCTENUM::INDEX_IDD_CX_CLIENT_CONFIG;
    let _i1 = iddcx::_IDDSTRUCTENUM::INDEX_IDARG_IN_ADAPTER_INIT;
};

/// The FP16/HDR adapter flag + high-color-space target cap the driver MUST set (these gate the whole
/// `*2` callback requirement). Confirms the ModuleConsts paths the port uses — note these enums have NO
/// `_`-prefixed module (unlike `_IDDFUNCENUM`/`_IDDSTRUCTENUM`).
const _: u32 = iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16;
const _: u32 = iddcx::IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE;
