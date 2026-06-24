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
        const W: [u16; N] = {
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

/// Build the adapter caps (FP16/HDR-capable) and kick off the async adapter creation. Called from
/// `EvtDeviceD0Entry`; idempotent across re-entrant D0 transitions.
pub fn init_adapter(device: WDFDEVICE) -> NTSTATUS {
    if ADAPTER.get().is_some() {
        return STATUS_SUCCESS;
    }
    dbglog!("[pf-vd] init_adapter");

    // The framework binds an IddCx version (the INF's UmdfExtensions) that may be OLDER than our 1.10
    // headers, so use ITS expected struct sizes — newer fields (e.g. IDDCX_ADAPTER_CAPS's
    // StaticDesktopReencodeFrameCount) make size_of too big and IddCxAdapterInitAsync rejects it. The
    // table is readable now (post-IddCxDeviceInitialize). Falls back to size_of if unavailable.
    let ver_size = crate::size::framework_struct_size(
        iddcx::_IDDSTRUCTENUM::INDEX_IDDCX_ENDPOINT_VERSION as u32,
    )
    .unwrap_or(core::mem::size_of::<iddcx::IDDCX_ENDPOINT_VERSION>() as u32);
    let diag_size = crate::size::framework_struct_size(
        iddcx::_IDDSTRUCTENUM::INDEX_IDDCX_ENDPOINT_DIAGNOSTIC_INFO as u32,
    )
    .unwrap_or(core::mem::size_of::<iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32);
    let caps_size = crate::size::framework_struct_size(
        iddcx::_IDDSTRUCTENUM::INDEX_IDDCX_ADAPTER_CAPS as u32,
    )
    .unwrap_or(core::mem::size_of::<iddcx::IDDCX_ADAPTER_CAPS>() as u32);
    dbglog!("[pf-vd] fw sizes: caps={caps_size} diag={diag_size} ver={ver_size}");

    // Firmware/hardware version (telemetry). The oracle points BOTH at one IDDCX_ENDPOINT_VERSION.
    // `version` is a stack local read synchronously by IddCxAdapterInitAsync (same as the oracle).
    let mut version: iddcx::IDDCX_ENDPOINT_VERSION = unsafe { core::mem::zeroed() };
    version.Size = ver_size;
    version.MajorVer = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    version.MinorVer = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0);
    version.Build = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0);

    // Endpoint diagnostics. `pEndPointModelName` must be a non-empty string. GammaSupport stays NONE.
    let mut diag: iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO = unsafe { core::mem::zeroed() };
    diag.Size = diag_size;
    diag.TransmissionType = iddcx::IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    diag.pEndPointFriendlyName = wstr!("punktfunk Virtual Display Adapter");
    diag.pEndPointManufacturerName = wstr!("punktfunk");
    diag.pEndPointModelName = wstr!("Virtual Display");
    diag.pFirmwareVersion = (&raw mut version).cast();
    diag.pHardwareVersion = (&raw mut version).cast();

    let mut caps: iddcx::IDDCX_ADAPTER_CAPS = unsafe { core::mem::zeroed() };
    caps.Size = caps_size;
    // CAN_PROCESS_FP16 must be CONSISTENT with the config's callbacks: the config registers the *2 / gamma
    // / set-default-hdr-metadata callbacks (FP16-obligated), so the adapter MUST advertise FP16 or the
    // framework rejects the mismatch at IddCxAdapterInitAsync. The oracle sets both.
    caps.Flags = iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16;
    caps.MaxMonitorsSupported = 16;
    caps.EndPointDiagnostics = diag;

    dbglog!(
        "[pf-vd] caps Size={} Flags={:#x} MaxMon={} diagSize={} cfgSizeOf={} capsSizeOf={}",
        caps.Size,
        caps.Flags,
        caps.MaxMonitorsSupported,
        caps.EndPointDiagnostics.Size,
        core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>(),
        core::mem::size_of::<iddcx::IDDCX_ADAPTER_CAPS>(),
    );
    // The adapter WDF object's attributes. The oracle passes an init'd WDF_OBJECT_ATTRIBUTES (Size +
    // Synchronization/Execution = InheritFromParent — NOT zeroed, since zero = *Invalid*); a null/zeroed
    // one is what IddCxAdapterInitAsync rejected. No context type yet (STEP 3).
    let mut attr: wdk_sys::WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    attr.Size = core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32;
    attr.ExecutionLevel = wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent;
    attr.SynchronizationScope =
        wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent;
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
