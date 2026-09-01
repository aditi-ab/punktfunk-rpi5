//! The Windows DXGI capture identity + shared D3D11 device creation (plan §W6): the capture
//! target descriptor ([`WinCaptureTarget`]), the GPU-resident captured texture ([`D3d11Frame`]),
//! the adapter-LUID packer ([`pack_luid`]), and [`make_device`] — a fresh D3D11 device/context on
//! a chosen adapter, applying the process GPU scheduling-priority hardening. Extracted from the
//! host's `capture/windows/dxgi.rs` so the capture IDD-push path, the encode D3D11 backends, and
//! pf-vdisplay all share ONE identity type + device factory (no capture↔encode↔vdisplay cycle).
//! The win32u GPU-preference hook, the HDR/video-engine converters, and the self-tests stay in the
//! capture crate — they are capture mechanics, not shared identity.

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
    /// The ADD reply flagged the ADAPTER as carrying an IRREVOCABLE IddCx hardware-cursor declare
    /// from an earlier session (remote-desktop-sweep §8.6; reach is adapter-wide, not per-target —
    /// on-glass 2026-07-23, a GameStream session's fresh target streamed cursor-less): DWM
    /// excludes the pointer from its frames until adapter reset, so a session WITHOUT the cursor
    /// channel must self-composite (the IDD-push capturer's forced-composite gate) or the
    /// streamed desktop has no cursor at all.
    pub cursor_excluded: bool,
}

/// The PyroWave (Windows) zero-copy sharing payload attached to a captured frame: the SECOND plane
/// texture + the cross-device fence the wavelet encoder needs (design/pyrowave-windows-host-
/// zerocopy.md). The wavelet encoder ingests **two SEPARATE** shareable plane textures — the full-res
/// `R8_UNORM` **Y** rides [`D3d11Frame::texture`], and the half-res `R8G8_UNORM` **CbCr** rides
/// [`cbcr`](Self::cbcr) — because importing a single *planar* NV12 texture into Vulkan is unreliable
/// on NVIDIA at arbitrary sizes; separate single/two-component textures import reliably. `None` on
/// every non-PyroWave frame (NVENC/AMF/QSV encode the in-place NV12/BGRA and need no cross-device
/// fence). The encoder makes each texture's shared handle on demand.
pub struct PyroFrameShare {
    /// The half-res `R8G8_UNORM` interleaved CbCr plane (created `SHARED | SHARED_NTHANDLE`). The
    /// full-res Y plane is [`D3d11Frame::texture`].
    pub cbcr: ID3D11Texture2D,
    /// The shared D3D11/D3D12 **fence** NT handle (raw), passed on EVERY frame; the encoder imports
    /// it (duplicating) whenever it has no timeline yet (first frame or after an encoder rebuild).
    pub fence_handle: Option<isize>,
    /// The fence value the capturer signalled after THIS frame's convert. The encoder's Vulkan
    /// acquire waits on it, so the wavelet read is ordered after the D3D11 CSC.
    pub fence_value: u64,
    /// The capturer's ring generation, bumped every time it recreates its texture ring. The
    /// PyroWave encoder caches its plane imports keyed on the texture's COM address, which carries
    /// no reference — after a recreate those addresses can be recycled by the allocator, so a
    /// cached import may describe a texture that no longer exists. The encoder flushes its import
    /// cache whenever this changes, making cache identity independent of allocator behaviour.
    pub ring_gen: u32,
}

/// A GPU-resident captured texture (the Windows zero-copy path: NVENC/AMF/QSV encode it in place;
/// the PyroWave backend imports it — plus the second plane in [`pyro`](Self::pyro) — into its own
/// Vulkan device). For a PyroWave frame, `texture` is the full-res `R8_UNORM` Y plane.
pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
    /// PyroWave zero-copy sharing info (the CbCr plane + fence); `None` unless this is a PyroWave
    /// session. See [`PyroFrameShare`].
    pub pyro: Option<PyroFrameShare>,
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
    // SAFETY: `adapter` is live for the call (caller contract). The two out-params are local
    // `Option`s the callee only writes, and both are checked below before use.
    unsafe {
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
    }
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
        // SAFETY: `dxgi_dev` is a live interface just obtained by a checked `cast`; both calls take
        // a scalar and only report failure through their return value.
        if unsafe { dxgi_dev.SetGPUThreadPriority(0x4000_001E) }.is_err()
            // SAFETY: same live interface, same scalar-in/HRESULT-out contract as the call above.
            // Deliberately its own block rather than one around the whole chain, which would
            // destroy the short-circuit and always issue the relative-priority call too.
            && unsafe { dxgi_dev.SetGPUThreadPriority(7) }.is_err()
        {
            tracing::warn!("SetGPUThreadPriority failed (run as admin/SYSTEM for GPU priority)");
        }
    }
    if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
        // SAFETY: `dxgi1` is a live interface from a checked `cast`; the arg is a scalar.
        let _ = unsafe { dxgi1.SetMaximumFrameLatency(1) };
    }
    Ok((device, context))
}

/// The configured GPU scheduling-priority policy (`PUNKTFUNK_GPU_PRIORITY_CLASS`).
enum PrioMode {
    /// Leave the OS default untouched (`off`).
    Off,
    /// A fixed class (`normal`=2 / `high`=4 / `realtime`=5 — the default).
    Static(i32),
}

/// Resolve `PUNKTFUNK_GPU_PRIORITY_CLASS` (`off|normal|high|realtime`, default **REALTIME**).
/// D3DKMT_SCHEDULINGPRIORITYCLASS: IDLE 0, BELOW_NORMAL 1, NORMAL 2, ABOVE_NORMAL 3, HIGH 4,
/// REALTIME 5. REALTIME is the Sunshine/OBS lever: the host's capture/convert/encode contexts
/// preempt a saturating game instead of waiting behind it, which is the whole product. The
/// 2026-08 HIGH default came from an AMD RX 9070 XT A/B that blamed REALTIME for a metronomic
/// capture stall; confirmed cases kept arriving with the downgrade shipped — it masked that
/// still-unattributed stall on some boxes, never fixed it — while regressing loaded NVIDIA
/// boxes into feed starvation (encode-latency spikes, half the frames reaching the encoder
/// under a GPU-bound game), so it was reverted. Do not re-convict REALTIME on that A/B. One
/// known trap: REALTIME + NVIDIA + HAGS + near-full VRAM is a documented NVENC hang — `high`
/// is the escape hatch. Costing the local game fps under load is by design (the remote view
/// is the product).
fn configured_gpu_priority_mode() -> PrioMode {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => PrioMode::Off,
        Some("normal") => PrioMode::Static(2),
        Some("high") => PrioMode::Static(4),
        // `realtime`, unset, and anything unrecognized all land on the REALTIME default.
        _ => PrioMode::Static(5),
    }
}

/// Enable SE_INC_BASE_PRIORITY on the CURRENT process token (best-effort) — the kernel gates the
/// HIGH/REALTIME GPU scheduling-priority bump on it. Held by SYSTEM/Administrators; a UAC-FILTERED
/// token does NOT have it, which is why `elevate_process_gpu_priority` may silently no-op in a
/// restricted service context.
fn enable_inc_base_priority() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
        SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle, always valid and never
    // closed; `token` is a local the callee only writes, and it is only used below if this succeeded.
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .is_ok();
    if opened {
        let mut luid = LUID::default();
        // SAFETY: a null system name means "local system"; `SE_INC_BASE_PRIORITY_NAME` is a static
        // NUL-terminated constant, and `luid` is a local the callee only writes.
        let found =
            unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_INC_BASE_PRIORITY_NAME, &mut luid) }
                .is_ok();
        if found {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // SAFETY: `token` is the live handle opened above; `tp` is a correctly sized local
            // `TOKEN_PRIVILEGES` whose `PrivilegeCount` matches its one-element array, borrowed only
            // for the duration of the call.
            let adjusted = unsafe {
                AdjustTokenPrivileges(
                    token,
                    false,
                    Some(&tp as *const TOKEN_PRIVILEGES),
                    0,
                    None,
                    None,
                )
            };
            if adjusted.is_err() {
                tracing::warn!("could not enable SE_INC_BASE_PRIORITY for GPU priority");
            }
        }
        // SAFETY: `token` was opened above, is owned here, and is closed exactly once on this path.
        let _ = unsafe { CloseHandle(token) };
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
    // SAFETY: both take static NUL-terminated literals; `LoadLibraryA` returns a module handle the
    // process keeps for its lifetime (gdi32 is never unloaded here), and `GetProcAddress` is passed
    // that live handle. Both results are checked by `?` before use.
    let gdi32 = unsafe { LoadLibraryA(s!("gdi32.dll")) }.ok()?;
    // SAFETY: `gdi32` is the live module handle the checked `LoadLibraryA` just returned, and the
    // export name is a static NUL-terminated literal; the result is checked by `?` before use.
    let p = unsafe { GetProcAddress(gdi32, s!("D3DKMTSetProcessSchedulingPriorityClass")) }?;
    type SetPrio = unsafe extern "system" fn(HANDLE, i32) -> i32;
    // SAFETY: `p` is the non-null export just resolved, and `SetPrio` is its documented signature
    // (`NTSTATUS D3DKMTSetProcessSchedulingPriorityClass(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)`,
    // both arguments 4/8-byte scalars). `process` is a valid handle by this fn's own contract.
    let f: SetPrio = unsafe { std::mem::transmute(p) };
    // SAFETY: `f` is that export transmuted to its documented signature directly above; `process`
    // is a valid handle by this fn's own contract and `prio` is a plain scalar. The call returns an
    // NTSTATUS and retains nothing.
    Some(unsafe { f(process, prio) })
}

/// GPU scheduling-priority hardening — the same approach as Sunshine/Apollo, independently
/// implemented via the documented D3DKMT APIs (no GPL source copied). On a
/// GPU-saturated game our capture+encode process is starved of GPU time slices — NVENC sits ~idle but
/// `lock_bitstream` waits ~20 ms for our context to be scheduled. Elevating the PROCESS GPU scheduling
/// priority class (the strong cross-process lever — far more effective than `SetGPUThreadPriority`
/// alone, which we measured as no help) lets our brief encode preempt the game. Default is
/// REALTIME — minimum latency at every layer; see [`configured_gpu_priority_mode`] for the
/// history and the `high` escape hatch. Runs once per process. Best-effort: silently no-ops
/// under a UAC-filtered token (the process will not hold SE_INC_BASE_PRIORITY, so the D3DKMT
/// call is a no-op).
fn elevate_process_gpu_priority() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let prio = match configured_gpu_priority_mode() {
            PrioMode::Off => {
                tracing::info!("GPU process scheduling priority class left at default (off)");
                return;
            }
            PrioMode::Static(p) => p,
        };
        enable_inc_base_priority();
        // SAFETY: `d3dkmt_set_scheduling_priority_class` requires a valid process handle;
        // `GetCurrentProcess()` returns the current-process pseudo-handle, which is always valid and
        // needs no close.
        match unsafe { d3dkmt_set_scheduling_priority_class(GetCurrentProcess(), prio) } {
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
