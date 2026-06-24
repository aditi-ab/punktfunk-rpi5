//! IddCx adapter bring-up. Adapter creation is DEFERRED to the first `EvtDeviceD0Entry` (the adapter
//! object is only valid after D0), and is ASYNC: `init_adapter` builds the caps and calls
//! `IddCxAdapterInitAsync`; the adapter object arrives later via `EvtIddCxAdapterInitFinished`
//! (`adapter_init_finished` → [`set_adapter`]). FP16 caps + the obligated `*2`/gamma/hdr callbacks (in
//! `callbacks.rs`) together enable HDR. STEP 3.

use std::sync::OnceLock;

use wdk_sys::{iddcx, NTSTATUS, WDFDEVICE};

use crate::STATUS_SUCCESS;

/// A static, null-terminated UTF-16 string pointer (ASCII only) — wdk-sys has no `windows` `w!`.
macro_rules! wstr {
    ($s:literal) => {{
        const N: usize = $s.len() + 1;
        // `static` (NOT `const`) — a const's `.as_ptr()` points to a temporary dropped at the end of the
        // statement (a dangling pointer); IddCx then reads garbage for the endpoint name strings and
        // IddCxAdapterInitAsync fails INVALID_PARAMETER. A `static` has a stable 'static address.
        static W: [u16; N] = {
            let b = $s.as_bytes();
            let mut w = [0u16; N];
            let mut i = 0;
            while i < b.len() {
                w[i] = b[i] as u16;
                i += 1;
            }
            w
        };
        W.as_ptr()
    }};
}

/// The IddCx adapter handle, stashed for later DDIs (e.g. `SET_RENDER_ADAPTER`, STEP 4).
struct SendAdapter(iddcx::IDDCX_ADAPTER);
// SAFETY: an opaque IddCx handle, used only as an argument to IddCx DDIs (themselves the synchronisation
// point) — never dereferenced in Rust. Storing it across threads in a OnceLock is sound.
unsafe impl Send for SendAdapter {}
unsafe impl Sync for SendAdapter {}

static ADAPTER: OnceLock<SendAdapter> = OnceLock::new();

/// A WDF context type for the adapter object (matches the upstream's `init_context_type`); STEP 4 stores
/// adapter state here. `WDF_OBJECT_CONTEXT_TYPE_INFO` holds raw pointers (so a Sync wrapper to allow a
/// `static`); `UniqueType` self-references per `WDF_DECLARE_CONTEXT_TYPE`.
#[repr(transparent)]
struct CtxTypeInfo(wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO);
// SAFETY: immutable 'static type metadata; the inner raw pointers are 'static and never written.
unsafe impl Sync for CtxTypeInfo {}
static ADAPTER_CTX: CtxTypeInfo = CtxTypeInfo(wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO>() as u32,
    ContextName: c"PfVdAdapterCtx".as_ptr().cast(),
    ContextSize: core::mem::size_of::<iddcx::IDDCX_ADAPTER>(),
    UniqueType: &ADAPTER_CTX.0,
    EvtDriverGetUniqueContextType: None,
});

/// Build the adapter caps (FP16/HDR-capable) and kick off the async adapter creation. Called from
/// `EvtDeviceD0Entry`; idempotent across re-entrant D0 transitions.
pub fn init_adapter(device: WDFDEVICE) -> NTSTATUS {
    if ADAPTER.get().is_some() {
        return STATUS_SUCCESS;
    }
    dbglog!("[pf-vd] init_adapter");

    // Firmware/hardware version (telemetry). The oracle points BOTH at one IDDCX_ENDPOINT_VERSION.
    // `version` is a stack local read synchronously by IddCxAdapterInitAsync (same as the oracle). `.Size`
    // is `size_of` throughout — these are the IddCx 1.10 structs and the framework here is 1.10 (= upstream).
    let mut version: iddcx::IDDCX_ENDPOINT_VERSION = unsafe { core::mem::zeroed() };
    version.Size = core::mem::size_of::<iddcx::IDDCX_ENDPOINT_VERSION>() as u32;
    version.MajorVer = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    version.MinorVer = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0);
    version.Build = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0);

    // Endpoint diagnostics. `pEndPointModelName` must be a non-empty string. GammaSupport MUST be set: a
    // zeroed value is IDDCX_FEATURE_IMPLEMENTATION_UNINITIALIZED (0), which the framework's adapter Validate
    // rejects with INVALID_PARAMETER (ddivalidation.cpp:797) — set it to NONE (1) like upstream. THIS was
    // the on-glass adapter-init blocker.
    let mut diag: iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO = unsafe { core::mem::zeroed() };
    diag.Size = core::mem::size_of::<iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32;
    diag.GammaSupport = iddcx::IDDCX_FEATURE_IMPLEMENTATION::IDDCX_FEATURE_IMPLEMENTATION_NONE;
    diag.TransmissionType = iddcx::IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    diag.pEndPointFriendlyName = wstr!("punktfunk Virtual Display Adapter");
    diag.pEndPointManufacturerName = wstr!("punktfunk");
    diag.pEndPointModelName = wstr!("Virtual Display");
    diag.pFirmwareVersion = (&raw mut version).cast();
    diag.pHardwareVersion = (&raw mut version).cast();

    let mut caps: iddcx::IDDCX_ADAPTER_CAPS = unsafe { core::mem::zeroed() };
    caps.Size = core::mem::size_of::<iddcx::IDDCX_ADAPTER_CAPS>() as u32;
    // Flags = NONE (SDR). The working upstream virtual-display-rs sets NO Flags. CAN_PROCESS_FP16 requires a
    // newer IddCx contract than the INF's UmdfExtensions=IddCx0102 grants, so the framework's adapter-caps
    // Validate (ddivalidation.cpp:797) rejects it with STATUS_INVALID_PARAMETER. HDR/FP16 is deferred to
    // STEP 7 (needs a higher UmdfExtensions binding + the obligated *2/HDR DDIs).
    caps.MaxMonitorsSupported = 16;
    caps.EndPointDiagnostics = diag;

    // The adapter WDF object's attributes: Size + Synchronization/Execution = InheritFromParent (NOT zeroed,
    // since zero = *Invalid*) + the adapter context type (STEP 4 stores adapter state here).
    let mut attr: wdk_sys::WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    attr.Size = core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32;
    attr.ExecutionLevel = wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent;
    attr.SynchronizationScope =
        wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent;
    attr.ContextTypeInfo = &ADAPTER_CTX.0;
    let init = iddcx::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device,
        pCaps: &raw mut caps,
        ObjectAttributes: &raw mut attr,
    };
    let mut out: iddcx::IDARG_OUT_ADAPTER_INIT = unsafe { core::mem::zeroed() };
    // SAFETY: `init`/`out` are valid local storage; IddCxAdapterInitAsync reads the caps synchronously
    // (the adapter object itself is delivered later via adapter_init_finished). Called once per device.
    let st = unsafe { wdk_iddcx::IddCxAdapterInitAsync(&init, &mut out) };
    dbglog!("[pf-vd] IddCxAdapterInitAsync -> {st:#x}");
    st
}

/// Stash the adapter object delivered by `EvtIddCxAdapterInitFinished` (STEP 4 reads it).
pub fn set_adapter(adapter: iddcx::IDDCX_ADAPTER) {
    let _ = ADAPTER.set(SendAdapter(adapter));
}
