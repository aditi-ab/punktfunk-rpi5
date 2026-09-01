//! Safe-ish typed wrappers over the wdk-sys IddCx DDIs, dispatched through the `IddFunctions` table
//! (indexed by `_IDDFUNCENUM::<Name>TableIndex`, with `IddDriverGlobals` as the implicit first arg) —
//! the same model wdk-sys uses for WDF. Graduates the proven dispatch from `wdk-probe/src/iddcx_rt.rs`
//! (M1 step 1) and adds the full DDI set the pf-vdisplay driver needs (M1 step 2 / §14).
//!
//! Each DDI pins its `(_IDDFUNCENUM index, PFN_* type)` pair in exactly ONE place (the macro invocation)
//! — a wrong pairing is the only way the table dispatch can be UB, so it must never be expressed twice
//! (port-plan unsafe_hotspot #1). All wrappers return the raw `NTSTATUS`; the caller classifies it with
//! [`nt_success`] (or, for the HRESULT-shaped SwapChain DDIs, treats a nonzero as the documented retry
//! code — handled at the call site in STEP 5).
#![no_std]
#![allow(non_snake_case, clippy::missing_safety_doc)]
// P0 lint (audit §8): require explicit `unsafe {}` blocks inside `unsafe fn`s + a `// SAFETY:` proof on
// each (this crate is the IddCx DDI dispatch layer — inherently unsafe, so audited, not unsafe-free).

pub use wdk_sys::iddcx;

use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

/// `NT_SUCCESS` — IddCx DDIs that return a true NTSTATUS are errors iff negative.
#[inline]
#[must_use]
pub const fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

/// Read one typed DDI from IddCxStub's runtime-populated flexible table.
///
/// The binding declares a zero-length array, so `wrapping_add` addresses the foreign tail without
/// falsely asserting it lies inside that Rust object. Runtime assertions reject negative indices
/// and a `T` whose width differs from the table's function-pointer slots.
///
/// # Safety
/// `index` and `T` name the same DDI, and IddCxStub has populated that slot.
#[inline]
unsafe fn ddi<T: Copy>(index: i32) -> T {
    assert!(index >= 0, "negative IddCx DDI index");
    assert_eq!(
        core::mem::size_of::<T>(),
        core::mem::size_of::<iddcx::PFN_IDD_CX>(),
        "IddCx DDI type has the wrong width"
    );
    let table = (&raw const iddcx::IddFunctions).cast::<iddcx::PFN_IDD_CX>();
    let slot = table.wrapping_add(index as usize);
    // SAFETY: the contract says IddCxStub populated this foreign tail slot with the matching `T`.
    unsafe { slot.cast::<T>().read() }
}

/// The IddCx driver globals (set by `IddCxStub`), passed as the implicit first arg to every DDI.
///
/// # Safety
/// Only valid once the driver is loaded by the IddCx runtime.
#[inline]
unsafe fn globals() -> iddcx::PIDD_DRIVER_GLOBALS {
    // SAFETY: we only read the pointer value of the stub-provided global.
    unsafe { (&raw const iddcx::IddDriverGlobals).read() }
}

/// Generate a typed outbound-DDI wrapper. Body is identical for every DDI (table lookup → call with the
/// globals); only the `(index, PFN_*, args)` triple varies, and it appears here exactly once.
macro_rules! iddcx_ddi {
    (
        $(#[$meta:meta])*
        $name:ident ( $( $arg:ident : $aty:ty ),* $(,)? ) @ $idx:ident as $pfn:ident
    ) => {
        $(#[$meta])*
        ///
        /// # Safety
        /// Call only after the driver is loaded by IddCx; pointers must satisfy the IddCx contract.
        #[inline]
        pub unsafe fn $name( $( $arg: $aty ),* ) -> NTSTATUS {
            // SAFETY: `$idx`/`$pfn` are the matched IddCx table index + PFN type (pinned by this macro
            // invocation), and the table is populated once the driver is loaded (this fn's contract).
            let f: iddcx::$pfn = unsafe { ddi(iddcx::_IDDFUNCENUM::$idx) };
            // SAFETY: only reads the stub-provided globals pointer; valid post-load per the contract.
            let g = unsafe { globals() };
            // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
            unsafe { (f.unwrap())(g, $( $arg ),* ) }
        }
    };
    // void-returning variant (e.g. IddCxAdapterSetRenderAdapter): the DDI's PFN returns `()`.
    (
        $(#[$meta:meta])*
        $name:ident ( $( $arg:ident : $aty:ty ),* $(,)? ) @ $idx:ident as $pfn:ident -> ()
    ) => {
        $(#[$meta])*
        ///
        /// # Safety
        /// Call only after the driver is loaded by IddCx; pointers must satisfy the IddCx contract.
        #[inline]
        pub unsafe fn $name( $( $arg: $aty ),* ) {
            // SAFETY: `$idx`/`$pfn` are the matched IddCx table index + PFN type (pinned by this macro
            // invocation), and the table is populated once the driver is loaded (this fn's contract).
            let f: iddcx::$pfn = unsafe { ddi(iddcx::_IDDFUNCENUM::$idx) };
            // SAFETY: only reads the stub-provided globals pointer; valid post-load per the contract.
            let g = unsafe { globals() };
            // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
            unsafe { (f.unwrap())(g, $( $arg ),* ) }
        }
    };
}

iddcx_ddi!(
    /// Configure a `WDFDEVICE_INIT` for IddCx (call before `WdfDeviceCreate`).
    IddCxDeviceInitConfig(device_init: PWDFDEVICE_INIT, config: *const iddcx::IDD_CX_CLIENT_CONFIG)
        @ IddCxDeviceInitConfigTableIndex as PFN_IDDCXDEVICEINITCONFIG
);
iddcx_ddi!(
    /// Finish IddCx device init (call after `WdfDeviceCreate`).
    IddCxDeviceInitialize(device: WDFDEVICE)
        @ IddCxDeviceInitializeTableIndex as PFN_IDDCXDEVICEINITIALIZE
);
iddcx_ddi!(
    /// Create the WDDM adapter asynchronously; `out.AdapterObject` is the `IDDCX_ADAPTER`.
    IddCxAdapterInitAsync(
        in_args: *const iddcx::IDARG_IN_ADAPTER_INIT,
        out_args: *mut iddcx::IDARG_OUT_ADAPTER_INIT,
    ) @ IddCxAdapterInitAsyncTableIndex as PFN_IDDCXADAPTERINITASYNC
);
iddcx_ddi!(
    /// Create a monitor on the adapter; `out.MonitorObject` is the `IDDCX_MONITOR`.
    IddCxMonitorCreate(
        adapter: iddcx::IDDCX_ADAPTER,
        in_args: *const iddcx::IDARG_IN_MONITORCREATE,
        out_args: *mut iddcx::IDARG_OUT_MONITORCREATE,
    ) @ IddCxMonitorCreateTableIndex as PFN_IDDCXMONITORCREATE
);
iddcx_ddi!(
    /// Announce monitor arrival; `out.OsTargetId`/`out.OsAdapterLuid` come back.
    IddCxMonitorArrival(
        monitor: iddcx::IDDCX_MONITOR,
        out_args: *mut iddcx::IDARG_OUT_MONITORARRIVAL,
    ) @ IddCxMonitorArrivalTableIndex as PFN_IDDCXMONITORARRIVAL
);
iddcx_ddi!(
    /// Depart a monitor (host disconnect / session teardown).
    IddCxMonitorDeparture(monitor: iddcx::IDDCX_MONITOR)
        @ IddCxMonitorDepartureTableIndex as PFN_IDDCXMONITORDEPARTURE
);
iddcx_ddi!(
    /// Declare hardware-cursor support for a monitor (proto v5 cursor channel): the OS stops
    /// compositing the pointer into the desktop image and signals `hNewCursorDataAvailable`
    /// on every cursor change instead (drained via [`IddCxMonitorQueryHardwareCursor`]).
    IddCxMonitorSetupHardwareCursor(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_SETUP_HWCURSOR,
    ) @ IddCxMonitorSetupHardwareCursorTableIndex as PFN_IDDCXMONITORSETUPHARDWARECURSOR
);
iddcx_ddi!(
    /// Drain one hardware-cursor update: position/visibility always; shape bytes are copied
    /// into `in_args.pShapeBuffer` only when the OS's shape id moved past `LastShapeId`.
    IddCxMonitorQueryHardwareCursor(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_QUERY_HWCURSOR,
        out_args: *mut iddcx::IDARG_OUT_QUERY_HWCURSOR,
    ) @ IddCxMonitorQueryHardwareCursorTableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR
);
iddcx_ddi!(
    /// The v3 hardware-cursor drain — same input, richer output (adds `PositionValid` /
    /// `PositionId` / `SdrWhiteLevel`). The base [`IddCxMonitorQueryHardwareCursor`] slot is
    /// stubbed to `STATUS_NOT_SUPPORTED` on current IddCx (observed on-glass, WDK 26100), so
    /// this is the live query DDI.
    IddCxMonitorQueryHardwareCursor3(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_QUERY_HWCURSOR,
        out_args: *mut iddcx::IDARG_OUT_QUERY_HWCURSOR3,
    ) @ IddCxMonitorQueryHardwareCursor3TableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR3
);
iddcx_ddi!(
    /// Set the preferred render adapter (LUID) for the virtual adapter.
    IddCxAdapterSetRenderAdapter(
        adapter: iddcx::IDDCX_ADAPTER,
        in_args: *const iddcx::IDARG_IN_ADAPTERSETRENDERADAPTER,
    ) @ IddCxAdapterSetRenderAdapterTableIndex as PFN_IDDCXADAPTERSETRENDERADAPTER -> ()
);
iddcx_ddi!(
    /// Refresh a LIVE monitor's target-mode list (the HDR `*2` variant, IddCx 1.10 — the same API
    /// family as the `*2` mode/buffer DDIs this driver already requires): the OS re-evaluates which
    /// modes the target supports WITHOUT a monitor departure, so the host can then mode-set to a
    /// freshly-advertised mode in place (the mid-stream resize, latency plan P2).
    IddCxMonitorUpdateModes2(
        monitor: iddcx::IDDCX_MONITOR,
        in_args: *const iddcx::IDARG_IN_UPDATEMODES2,
    ) @ IddCxMonitorUpdateModes2TableIndex as PFN_IDDCXMONITORUPDATEMODES2
);
iddcx_ddi!(
    /// Bind a D3D device to an assigned swap-chain. HRESULT-shaped (0x887A0026 → retry on monitor flap).
    IddCxSwapChainSetDevice(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *const iddcx::IDARG_IN_SWAPCHAINSETDEVICE,
    ) @ IddCxSwapChainSetDeviceTableIndex as PFN_IDDCXSWAPCHAINSETDEVICE
);
iddcx_ddi!(
    /// Release the previous frame and acquire the next (HDR/FP16 variant). HRESULT-shaped; E_PENDING
    /// (0x8000000A) means wait on the surface-available event.
    IddCxSwapChainReleaseAndAcquireBuffer2(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *mut iddcx::IDARG_IN_RELEASEANDACQUIREBUFFER2,
        out_args: *mut iddcx::IDARG_OUT_RELEASEANDACQUIREBUFFER2,
    ) @ IddCxSwapChainReleaseAndAcquireBuffer2TableIndex as PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER2
);
iddcx_ddi!(
    /// Signal that the acquired frame has been processed. HRESULT-shaped.
    IddCxSwapChainFinishedProcessingFrame(swap_chain: iddcx::IDDCX_SWAPCHAIN)
        @ IddCxSwapChainFinishedProcessingFrameTableIndex as PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME
);
iddcx_ddi!(
    /// Report per-frame processing statistics for the last processed frame (frame number, status,
    /// acquire/send QPC times). The OS-side per-output scheduling consumes these; the canonical MS
    /// sample leaves the call as a TODO, and this driver historically never made it — added for the
    /// 2026-08 case-#4 A/B (virtual-head-selective compose starvation: DWM composes a lit sibling
    /// head at full rate while this head's swap-chain drains E_PENDING for 150–2200 ms with damage
    /// pending). HRESULT-shaped.
    IddCxSwapChainReportFrameStatistics(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *const iddcx::IDARG_IN_REPORTFRAMESTATISTICS,
    ) @ IddCxSwapChainReportFrameStatisticsTableIndex as PFN_IDDCXSWAPCHAINREPORTFRAMESTATISTICS
);

iddcx_ddi!(
    /// Raise the swap-chain's processing D3D device to realtime GPU scheduling priority — "higher
    /// than any regular application can set" (IddCx 1.9) — so buffer processing outruns ordinary
    /// GPU contention. It does NOT help against adapter-wide display servicing (modeset-class DDIs
    /// idle the hardware outright); it defends the contention case only. Best-effort at the call
    /// site: the DDI itself may decline (e.g. E_NOTIMPL on WDDM < 3.0 hardware).
    ///
    /// Table-slot availability rests on the driver's `IddMinimumVersionRequired = 10` export
    /// (pf-vdisplay lib.rs): the loader refuses to bind a framework older than IddCx 1.10, so
    /// every slot of our compiled 1.10 surface — this 1.9 one included — is populated wherever the
    /// driver runs at all.
    IddCxSetRealtimeGPUPriority(
        swap_chain: iddcx::IDDCX_SWAPCHAIN,
        in_args: *const iddcx::IDARG_IN_SETREALTIMEGPUPRIORITY,
    ) @ IddCxSetRealtimeGPUPriorityTableIndex as PFN_IDDCXSETREALTIMEGPUPRIORITY
);
