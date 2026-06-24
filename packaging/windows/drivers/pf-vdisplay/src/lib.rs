//! pf-vdisplay — the all-Rust UMDF IddCx virtual-display driver (M1 step-2 rewrite, on wdk-sys + the
//! owned pf-vdisplay-proto ABI). See docs/windows-host-rewrite.md §14 for the full port plan.
//!
//! STEP 2: the IddCx driver SKELETON — DriverEntry → driver_add builds the full `IDD_CX_CLIENT_CONFIG`
//! (14 IddCx callbacks + the PnP `EvtDeviceD0Entry`, all stubs) sized via the versioned
//! [`size::idd_cx_client_config_size`], runs `IddCxDeviceInitConfig` → `WdfDeviceCreate` →
//! `WdfDeviceCreateDeviceInterface`(the owned pf-vdisplay GUID) → `IddCxDeviceInitialize`, and exports
//! `IddMinimumVersionRequired`. This links `IddCxStub` (the CI gate). The real adapter init (STEP 3),
//! control plane + monitor/modes (STEP 4), and swap-chain/IDD-push (STEP 5-6) fill the stubs in.

#![allow(non_snake_case, clippy::missing_safety_doc)]

mod adapter;
mod callbacks;
#[allow(dead_code)] // salvaged verbatim; wired into the mode callbacks in STEP 4
mod edid;
mod entry;
mod size;

use wdk_sys::NTSTATUS;

// NTSTATUS codes the driver returns (wdk-sys doesn't surface all of these as constants).
pub(crate) const STATUS_SUCCESS: NTSTATUS = 0;
pub(crate) const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;
pub(crate) const STATUS_NOT_FOUND: NTSTATUS = 0xC000_0225u32 as NTSTATUS;

/// IddCx (stub mode) requires the driver to export the minimum IddCx framework version it needs — the
/// `#ifndef IDD_STUB` branch of `IddCxFuncEnum.h` that normally emits it is compiled out under
/// `IDD_STUB`. `4` matches the proven `wdf-umdf` oracle.
#[unsafe(no_mangle)]
pub static IddMinimumVersionRequired: wdk_sys::ULONG = 4;
