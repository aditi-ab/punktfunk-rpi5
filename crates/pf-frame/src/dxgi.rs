//! The Windows DXGI capture identity + shared D3D11 device creation (plan §W6): the capture
//! target descriptor ([`WinCaptureTarget`]), the GPU-resident captured texture ([`D3d11Frame`]),
//! the adapter-LUID packer ([`pack_luid`]), and [`make_device`] — a fresh D3D11 device/context on
//! a chosen adapter, applying the process GPU scheduling-priority hardening. Extracted from the
//! host's `capture/windows/dxgi.rs` so the capture IDD-push path, the encode D3D11 backends, and
//! pf-vdisplay all share ONE identity type + device factory (no capture↔encode↔vdisplay cycle).
//! The win32u GPU-preference hook, the HDR/video-engine converters, and the self-tests stay in the
//! capture crate — they are capture mechanics, not shared identity.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice, IDXGIDevice1};

#[derive(Clone)]
pub struct WinCaptureTarget {
    /// Packed DXGI adapter LUID (`(HighPart << 32) | (LowPart & 0xffff_ffff)`).
    pub adapter_luid: i64,
    /// The output's GDI device name, e.g. `\\.\DISPLAY3`. Can CHANGE across a secure-desktop switch.
    pub gdi_name: String,
    /// Stable virtual-display (IddCx) target id — re-resolved to the current GDI name on every recovery.
    pub target_id: u32,
    /// The pf-vdisplay driver's WUDFHost pid (from the ADD reply) — the process the IDD-push capturer
    /// duplicates the sealed frame channel's handles INTO (`idd_push::ChannelBroker`). `0` = unknown
    /// (a pre-v2 pairing can't occur — the version handshake is hard — so this only guards misuse).
    pub wudf_pid: u32,
}

/// A GPU-resident captured texture (future NVENC-D3D11 zero-copy path).
pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
}
// SAFETY: `D3d11Frame` owns an `ID3D11Texture2D` + `ID3D11Device`, which are COM interface pointers.
// D3D11 devices/resources use thread-safe (interlocked) COM reference counting, and the device is
// created free-threaded (`make_device` passes no `D3D11_CREATE_DEVICE_SINGLETHREADED`), so handing
// ownership of the frame to another thread — the capture→encode handoff — and releasing it there is
// sound. The value is moved, never aliased (no `Sync`), so there is no concurrent use of the
// single-threaded immediate context.
unsafe impl Send for D3d11Frame {}

pub fn pack_luid(luid: LUID) -> i64 {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64 & 0xffff_ffff)
}

/// Create a fresh D3D11 device + context on a specific adapter (driver_type UNKNOWN with an explicit
/// adapter). Used at open and on every ACCESS_LOST: a device created on one desktop cannot sustain a
/// duplication on a *different* desktop (perpetual ACCESS_LOST), so the secure-desktop switch needs a
/// device made while the thread is attached to that desktop.
///
/// # Safety
/// `adapter` must be a live `IDXGIAdapter1` for the duration of the call. The fn calls the D3D11 /
/// DXGI FFI (`D3D11CreateDevice`, GPU scheduling-priority hardening) but forms no lasting alias to
/// `adapter`; the returned device/context are the sole owners of the new COM objects.
pub unsafe fn make_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .context("D3D11CreateDevice")?;
    let device = device.context("null D3D11 device")?;
    let context = context.context("null D3D11 context")?;

    // GPU scheduling hardening — the same approach Sunshine/Apollo use, reimplemented here via the
    // documented D3DKMT/DXGI APIs (no GPL source copied). Our capture+encode
    // shares the GPU with the streamed game; when the game saturates the GPU our process is starved of
    // GPU time slices, so NVENC sits near-idle yet `lock_bitstream` waits ~20 ms for our context to be
    // scheduled — capping the stream (~47 fps measured at 5K@240) and stuttering. Per-frame copy/convert
    // is NOT the cause (zero-copy + thread-priority alone didn't move it); the PROCESS-level GPU
    // scheduling priority class is the decisive cross-process lever. Secondary: the absolute per-device
    // GPU thread priority and a 1-frame latency cap.
    elevate_process_gpu_priority();
    if let Ok(dxgi_dev) = device.cast::<IDXGIDevice>() {
        // The absolute max GPU thread priority (0x4000001E; the same value Sunshine/Apollo use); fall back to relative +7.
        if dxgi_dev.SetGPUThreadPriority(0x4000_001E).is_err()
            && dxgi_dev.SetGPUThreadPriority(7).is_err()
        {
            tracing::warn!("SetGPUThreadPriority failed (run as admin/SYSTEM for GPU priority)");
        }
    }
    if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
        let _ = dxgi1.SetMaximumFrameLatency(1);
    }
    Ok((device, context))
}

/// Resolve the configured GPU scheduling-priority class from `PUNKTFUNK_GPU_PRIORITY_CLASS`
/// (`off|normal|high|realtime`, default high). `None` = leave it at the OS default (the `off` opt-out).
/// D3DKMT_SCHEDULINGPRIORITYCLASS: IDLE 0, BELOW_NORMAL 1, NORMAL 2, ABOVE_NORMAL 3, HIGH 4, REALTIME 5.
fn configured_gpu_priority_class() -> Option<i32> {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => None,
        Some("normal") => Some(2),
        Some("realtime") => Some(5),
        _ => Some(4), // HIGH — safe on NVIDIA+HAGS (realtime can freeze NVENC)
    }
}

/// Enable SE_INC_BASE_PRIORITY on the CURRENT process token (best-effort) — the kernel gates the
/// HIGH/REALTIME GPU scheduling-priority bump on it. Held by SYSTEM/Administrators; a UAC-FILTERED
/// token does NOT have it, which is why `elevate_process_gpu_priority` may silently no-op in a
/// restricted service context.
unsafe fn enable_inc_base_priority() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
        SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = HANDLE::default();
    if OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
        &mut token,
    )
    .is_ok()
    {
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_INC_BASE_PRIORITY_NAME, &mut luid).is_ok() {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            if AdjustTokenPrivileges(
                token,
                false,
                Some(&tp as *const TOKEN_PRIVILEGES),
                0,
                None,
                None,
            )
            .is_err()
            {
                tracing::warn!("could not enable SE_INC_BASE_PRIORITY for GPU priority");
            }
        }
        let _ = CloseHandle(token);
    }
}

/// Call `gdi32!D3DKMTSetProcessSchedulingPriorityClass(process, prio)` (no stable windows-rs binding —
/// loaded by name). Returns the NTSTATUS (0 = success) or `None` if the export can't be resolved. The
/// CALLING process must hold SE_INC_BASE_PRIORITY ([`enable_inc_base_priority`]) for HIGH/REALTIME; the
/// kernel checks the caller's privilege whether the target is self or a child we created.
unsafe fn d3dkmt_set_scheduling_priority_class(
    process: windows::Win32::Foundation::HANDLE,
    prio: i32,
) -> Option<i32> {
    use windows::core::s;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    let gdi32 = LoadLibraryA(s!("gdi32.dll")).ok()?;
    let p = GetProcAddress(gdi32, s!("D3DKMTSetProcessSchedulingPriorityClass"))?;
    type SetPrio = unsafe extern "system" fn(HANDLE, i32) -> i32;
    let f: SetPrio = std::mem::transmute(p);
    Some(f(process, prio))
}

/// GPU scheduling-priority hardening — the same approach as Sunshine/Apollo, independently
/// implemented via the documented D3DKMT APIs (no GPL source copied). On a
/// GPU-saturated game our capture+encode process is starved of GPU time slices — NVENC sits ~idle but
/// `lock_bitstream` waits ~20 ms for our context to be scheduled. Elevating the PROCESS GPU scheduling
/// priority class (the strong cross-process lever — far more effective than `SetGPUThreadPriority`
/// alone, which we measured as no help) lets our brief encode preempt the game. Uses HIGH, NOT
/// realtime: realtime on NVIDIA + HAGS can freeze/crash NVENC (Apollo downgrades it for exactly this).
/// Runs once per process; best-effort. `PUNKTFUNK_GPU_PRIORITY_CLASS = off|normal|high|realtime`
/// (default high). Best-effort: silently no-ops under a UAC-filtered token (the process will not
/// hold SE_INC_BASE_PRIORITY, so the D3DKMT call is a no-op).
fn elevate_process_gpu_priority() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    // SAFETY: the closure calls two of this module's `unsafe fn`s — `enable_inc_base_priority`
    // (adjusts the current-process token; it has no caller precondition and builds all its FFI args
    // locally) and `d3dkmt_set_scheduling_priority_class` (loads gdi32 by name and calls the export).
    // The latter requires `process` to be a valid process handle; `GetCurrentProcess()` returns the
    // current-process pseudo-handle, which is always valid and needs no close. Runs once via
    // `Once::call_once`; no raw pointers are dereferenced here.
    ONCE.call_once(|| unsafe {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let Some(prio) = configured_gpu_priority_class() else {
            tracing::info!("GPU process scheduling priority class left at default (off)");
            return;
        };
        enable_inc_base_priority();
        match d3dkmt_set_scheduling_priority_class(GetCurrentProcess(), prio) {
            Some(0) => tracing::info!(
                priority_class = prio,
                "GPU process scheduling priority class set (2=normal 4=high 5=realtime)"
            ),
            Some(st) => tracing::warn!(
                status = format!("0x{st:08X}"),
                "D3DKMTSetProcessSchedulingPriorityClass failed (run as admin/SYSTEM for GPU priority)"
            ),
            None => tracing::warn!("D3DKMTSetProcessSchedulingPriorityClass export not found"),
        }
    });
}
