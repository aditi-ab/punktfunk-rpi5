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

    // Endpoint diagnostics (telemetry only — not used for OS runtime decisions). `pEndPointModelName`
    // must be a non-empty string; the rest are optional. GammaSupport stays NONE (zeroed).
    let mut diag: iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO = unsafe { core::mem::zeroed() };
    diag.Size = core::mem::size_of::<iddcx::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32;
    diag.TransmissionType = iddcx::IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    diag.pEndPointFriendlyName = wstr!("punktfunk Virtual Display Adapter");
    diag.pEndPointManufacturerName = wstr!("punktfunk");
    diag.pEndPointModelName = wstr!("Virtual Display");

    let mut caps: iddcx::IDDCX_ADAPTER_CAPS = unsafe { core::mem::zeroed() };
    caps.Size = core::mem::size_of::<iddcx::IDDCX_ADAPTER_CAPS>() as u32;
    caps.Flags = iddcx::IDDCX_ADAPTER_FLAGS::IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16;
    caps.MaxMonitorsSupported = 16;
    caps.EndPointDiagnostics = diag;

    let init = iddcx::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device,
        pCaps: &raw mut caps,
        ObjectAttributes: core::ptr::null_mut(),
    };
    let mut out: iddcx::IDARG_OUT_ADAPTER_INIT = unsafe { core::mem::zeroed() };
    // SAFETY: `init`/`out` are valid local storage; IddCxAdapterInitAsync reads the caps synchronously
    // (the adapter object itself is delivered later via adapter_init_finished). Called once per device.
    unsafe { wdk_iddcx::IddCxAdapterInitAsync(&init, &mut out) }
}

/// Stash the adapter object delivered by `EvtIddCxAdapterInitFinished` (STEP 4 reads it).
pub fn set_adapter(adapter: iddcx::IDDCX_ADAPTER) {
    let _ = ADAPTER.set(SendAdapter(adapter));
}
