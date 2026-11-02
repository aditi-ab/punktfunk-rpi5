//! pf-vdisplay — the all-Rust UMDF IddCx virtual-display driver (M1 step-2 rewrite, on wdk-sys + the
//! owned pf-driver-proto ABI). See design/windows-host-rewrite.md §14 for the full port plan.
//!
//! STEP 2: the IddCx driver SKELETON — DriverEntry → driver_add builds the full `IDD_CX_CLIENT_CONFIG`
//! (14 IddCx callbacks + the PnP `EvtDeviceD0Entry`, all stubs) sized via the versioned
//! [`size::idd_cx_client_config_size`], runs `IddCxDeviceInitConfig` → `WdfDeviceCreate` →
//! `WdfDeviceCreateDeviceInterface`(the owned pf-vdisplay GUID) → `IddCxDeviceInitialize`, and exports
//! `IddMinimumVersionRequired`. This links `IddCxStub` (the CI gate). The real adapter init (STEP 3),
//! control plane + monitor/modes (STEP 4), and swap-chain/IDD-push (STEP 5-6) fill the stubs in.

#![allow(non_snake_case, clippy::missing_safety_doc)]
// P0 lint (audit §8): an unsafe op inside an `unsafe fn` must be in an explicit `unsafe {}` block, so the
// fn-level `unsafe` never silently blesses the whole body, AND every `unsafe {}` must carry its proof.
// An IddCx display driver is inherently FFI-bound (D3D11 / IddCx DDIs / cross-process shared
// textures), so it can't be unsafe-FREE the way the gamepad drivers now are (their logic moved onto the
// safe `pf_umdf_util` layer); these gates make it unsafe-AUDITED instead, and stop it regressing.

#[macro_use]
mod log;
mod adapter;
mod callbacks;
mod control;
mod cursor;
mod cursor_cell;
mod direct_3d_device;
mod encode;
#[cfg(feature = "encode-probe")]
mod encode_probe;
mod entry;
mod modes;
mod monitor;
mod registry;
mod swap_chain_processor;
mod watchdog;
mod worker;

use wdk_sys::NTSTATUS;

// NTSTATUS codes the driver returns (wdk-sys doesn't surface all of these as constants).
pub(crate) const STATUS_SUCCESS: NTSTATUS = 0;
pub(crate) const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;
pub(crate) const STATUS_NOT_FOUND: NTSTATUS = 0xC000_0225u32 as NTSTATUS;
pub(crate) const STATUS_DEVICE_NOT_READY: NTSTATUS = 0xC000_00A3u32 as NTSTATUS;
pub(crate) const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;
pub(crate) const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as NTSTATUS;
pub(crate) const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022u32 as NTSTATUS;

/// IddCx (stub mode) requires the driver to export the minimum IddCx framework version it needs — the
/// `#ifndef IDD_STUB` branch of `IddCxFuncEnum.h` that normally emits it is compiled out under
/// `IDD_STUB`.
///
/// `10` = IddCx 1.10, the TRUTHFUL floor: the drain loop calls
/// `IddCxSwapChainReleaseAndAcquireBuffer2` (1.10) unconditionally and the swap-chain worker uses
/// `IddCxSetRealtimeGPUPriority` (1.9); the product floor is already Windows 11 22H2 / build 22621,
/// whose framework is 1.10 (the installer gate — `MIN_HOST_BUILD` in punktfunk-setup —
/// exists precisely for this driver). The oracle's historical `4` predated that floor and would let
/// the driver load against a SHORT `IddFunctions` table, where dispatching any post-1.4 DDI reads
/// past the populated entries. With `10`, an older framework fails the bind cleanly (driver doesn't
/// load) instead of limping into undefined dispatch.
#[unsafe(no_mangle)]
pub static IddMinimumVersionRequired: wdk_sys::ULONG = 10;
