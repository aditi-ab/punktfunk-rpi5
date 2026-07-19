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
    // REALTIME auto-gate (gpu-contention §5.C / latency plan T2.3) — needs the device's adapter,
    // so it runs here, after creation; internally once-per-process.
    auto_priority_gate(&device);
    Ok((device, context))
}

/// The configured GPU scheduling-priority policy (`PUNKTFUNK_GPU_PRIORITY_CLASS`).
enum PrioMode {
    /// Leave the OS default untouched (`off`).
    Off,
    /// A fixed class the operator pinned (`normal`=2 / `high`=4 / `realtime`=5).
    Static(i32),
    /// The default: HIGH immediately, then upgrade to REALTIME when it is safe — HAGS off, or
    /// HAGS on with comfortable VRAM headroom (with a monitor that downgrades the moment VRAM
    /// tightens). REALTIME is the proven ceiling-raiser (it is how our brief encode preempts a
    /// saturating game), but REALTIME + NVIDIA + HAGS + near-full VRAM is a documented NVENC
    /// hang — the gate takes the win everywhere it cannot hit the hazard.
    Auto,
}

/// Resolve `PUNKTFUNK_GPU_PRIORITY_CLASS` (`off|normal|high|realtime|auto`, default **auto**).
/// D3DKMT_SCHEDULINGPRIORITYCLASS: IDLE 0, BELOW_NORMAL 1, NORMAL 2, ABOVE_NORMAL 3, HIGH 4,
/// REALTIME 5. `realtime` pins REALTIME statically (no gate — the operator owns the hazard);
/// `high` restores the pre-T2.3 static default.
fn configured_gpu_priority_mode() -> PrioMode {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => PrioMode::Off,
        Some("normal") => PrioMode::Static(2),
        Some("high") => PrioMode::Static(4),
        Some("realtime") => PrioMode::Static(5),
        _ => PrioMode::Auto,
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
/// alone, which we measured as no help) lets our brief encode preempt the game. Default is the
/// T2.3 `auto` mode: HIGH immediately here, then [`auto_priority_gate`] upgrades to REALTIME
/// where the NVIDIA+HAGS+full-VRAM NVENC-hang hazard cannot bite (and a monitor downgrades when
/// it could). Runs once per process; best-effort.
/// `PUNKTFUNK_GPU_PRIORITY_CLASS = off|normal|high|realtime|auto` (default auto; `high` = the
/// pre-gate static behavior; `realtime` = pinned, operator owns the hazard). Best-effort:
/// silently no-ops under a UAC-filtered token (the process will not hold SE_INC_BASE_PRIORITY,
/// so the D3DKMT call is a no-op).
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
        let prio = match configured_gpu_priority_mode() {
            PrioMode::Off => {
                tracing::info!("GPU process scheduling priority class left at default (off)");
                return;
            }
            PrioMode::Static(p) => p,
            // Auto: HIGH is the immediately-safe floor; `auto_priority_gate` (running once a
            // device exists, so it can see the adapter) decides the REALTIME upgrade.
            PrioMode::Auto => 4,
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

// --- REALTIME auto-gate (gpu-contention §5.C / latency plan T2.3) --------------------------------
//
// REALTIME GPU scheduling priority is the genuine cross-process ceiling-raiser under a saturating
// game (a higher-priority context preempts at pixel granularity — the Async-TimeWarp mechanism),
// and our SYSTEM service uniquely holds the SE_INC_BASE_PRIORITY it needs. The one documented
// hazard: REALTIME + NVIDIA + HAGS-on + near-full VRAM can hang NVENC. So: probe HAGS once via
// D3DKMT; HAGS off ⇒ REALTIME unconditionally; HAGS on ⇒ REALTIME gated on LOCAL-segment VRAM
// headroom, with a monitor thread that downgrades to HIGH the moment usage crosses
// [`VRAM_DOWNGRADE_PCT`] of the OS budget and restores REALTIME after it has stayed under
// [`VRAM_RESTORE_PCT`] for [`VRAM_RESTORE_TICKS`] consecutive polls (hysteresis against flapping
// on the boundary of the hazard window).

/// Downgrade REALTIME→HIGH when local VRAM usage exceeds this share of the OS budget.
const VRAM_DOWNGRADE_PCT: u64 = 92;
/// Restore HIGH→REALTIME once usage has stayed at/below this share…
const VRAM_RESTORE_PCT: u64 = 85;
/// …for this many consecutive 2 s polls.
const VRAM_RESTORE_TICKS: u32 = 3;

/// `KMTQAITYPE_WDDM_2_7_CAPS` — the adapter-info query that carries the HAGS (hardware GPU
/// scheduling) state. `D3DKMT_WDDM_2_7_CAPS` is a 4-byte bitfield: bit 0 `HwSchSupported`,
/// bit 1 `HwSchEnabled` (the one that matters — "is HAGS actually ON for this adapter").
const KMTQAITYPE_WDDM_2_7_CAPS: u32 = 70;

/// Probe whether HAGS (WDDM hardware scheduling) is ENABLED on the adapter with `luid`, via the
/// gdi32 D3DKMT surface (loaded by name — no stable windows-rs bindings, same as the priority
/// setter). `None` = could not determine (missing exports / query failed) — the caller treats
/// unknown as "assume the hazard exists".
///
/// # Safety
/// Calls gdi32 exports through by-name transmuted pointers with locally built, correctly sized
/// `repr(C)` argument structs; the adapter handle is closed before returning on every path.
unsafe fn hags_enabled(luid: LUID) -> Option<bool> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    #[repr(C)]
    struct OpenFromLuid {
        luid: LUID,
        h_adapter: u32,
    }
    #[repr(C)]
    struct CloseAdapter {
        h_adapter: u32,
    }
    #[repr(C)]
    struct QueryInfo {
        h_adapter: u32,
        ty: u32,
        private_data: *mut std::ffi::c_void,
        private_data_size: u32,
    }
    let gdi32 = LoadLibraryA(s!("gdi32.dll")).ok()?;
    let open = GetProcAddress(gdi32, s!("D3DKMTOpenAdapterFromLuid"))?;
    let query = GetProcAddress(gdi32, s!("D3DKMTQueryAdapterInfo"))?;
    let close = GetProcAddress(gdi32, s!("D3DKMTCloseAdapter"))?;
    type OpenFn = unsafe extern "system" fn(*mut OpenFromLuid) -> i32;
    type QueryFn = unsafe extern "system" fn(*mut QueryInfo) -> i32;
    type CloseFn = unsafe extern "system" fn(*mut CloseAdapter) -> i32;
    let open: OpenFn = std::mem::transmute(open);
    let query: QueryFn = std::mem::transmute(query);
    let close: CloseFn = std::mem::transmute(close);

    let mut oa = OpenFromLuid { luid, h_adapter: 0 };
    if open(&mut oa) != 0 {
        return None;
    }
    let mut caps: u32 = 0;
    let mut qi = QueryInfo {
        h_adapter: oa.h_adapter,
        ty: KMTQAITYPE_WDDM_2_7_CAPS,
        private_data: (&mut caps as *mut u32).cast(),
        private_data_size: std::mem::size_of::<u32>() as u32,
    };
    let st = query(&mut qi);
    let mut ca = CloseAdapter {
        h_adapter: oa.h_adapter,
    };
    let _ = close(&mut ca);
    if st != 0 {
        return None; // pre-WDDM-2.7 driver: the query type doesn't exist ⇒ HAGS can't be on
    }
    Some(caps & 0x2 != 0) // bit 1 = HwSchEnabled
}

/// Apply the auto-gate decision for `device`'s adapter (no-op unless the mode is `Auto`; runs
/// once per process). HAGS off ⇒ REALTIME now. HAGS on (or unknown) ⇒ spawn the VRAM monitor,
/// which flips REALTIME⇄HIGH on headroom. See the section comment above for the policy.
fn auto_priority_gate(device: &ID3D11Device) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !matches!(configured_gpu_priority_mode(), PrioMode::Auto) {
            return;
        }
        // The adapter identity this device runs on.
        let luid = match device
            .cast::<IDXGIDevice>()
            .and_then(|d| {
                // SAFETY: `d` is a live IDXGIDevice from the cast; GetAdapter returns an owned
                // COM wrapper that drops with its windows-rs handle.
                unsafe { d.GetAdapter() }
            })
            .and_then(|a| {
                // SAFETY: `a` is the live adapter from GetAdapter; GetDesc fills a plain
                // out-struct by value.
                unsafe { a.GetDesc() }
            }) {
            Ok(desc) => desc.AdapterLuid,
            Err(e) => {
                tracing::warn!(error = %e, "REALTIME auto-gate: no adapter LUID — staying HIGH");
                return;
            }
        };
        // SAFETY: `hags_enabled` builds all its FFI arguments locally and closes the adapter
        // handle before returning (see its own contract); `luid` is a plain value.
        let hags = unsafe { hags_enabled(luid) };
        match hags {
            Some(false) => {
                // No HAGS ⇒ the NVENC-hang hazard cannot occur: take REALTIME outright.
                // SAFETY: `GetCurrentProcess` returns the always-valid pseudo-handle; the setter
                // loads gdi32 by name (its own contract).
                let st = unsafe {
                    d3dkmt_set_scheduling_priority_class(
                        windows::Win32::System::Threading::GetCurrentProcess(),
                        5,
                    )
                };
                match st {
                    Some(0) => tracing::info!(
                        "GPU priority REALTIME (auto: HAGS off — hang hazard not possible)"
                    ),
                    _ => {
                        tracing::warn!("REALTIME auto-gate: could not set REALTIME (staying HIGH)")
                    }
                }
            }
            hags => {
                let unknown = hags.is_none();
                tracing::info!(
                    hags_unknown = unknown,
                    "GPU priority auto-gate: HAGS on (or undeterminable) — REALTIME rides VRAM \
                     headroom (monitor thread)"
                );
                spawn_vram_gate(luid);
            }
        }
    });
}

/// The VRAM-headroom monitor (auto mode, HAGS on): flips the process class REALTIME⇄HIGH on the
/// LOCAL memory segment's usage-vs-budget, with hysteresis. Its own DXGI factory/adapter (COM
/// objects never cross threads); polling a 2 s cadence — VRAM exhaustion is a seconds-scale
/// process, and the downgrade only has to beat the *next* NVENC submission pile-up, not a frame.
fn spawn_vram_gate(luid: LUID) {
    let _ = std::thread::Builder::new()
        .name("pf-gpu-prio".into())
        .spawn(move || {
            use windows::Win32::Graphics::Dxgi::{
                CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory4, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
                DXGI_QUERY_VIDEO_MEMORY_INFO,
            };
            use windows::Win32::System::Threading::GetCurrentProcess;
            // SAFETY: plain DXGI object creation + LUID lookup; the COM objects are created on
            // and confined to this thread.
            let adapter: Option<IDXGIAdapter3> = unsafe {
                CreateDXGIFactory1::<IDXGIFactory4>()
                    .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter3>(luid))
                    .ok()
            };
            let Some(adapter) = adapter else {
                tracing::warn!("pf-gpu-prio: adapter lookup failed — staying HIGH");
                return;
            };
            let mut realtime = false; // we start at the HIGH floor
            let mut clean_ticks = 0u32;
            loop {
                let mut mi = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                // SAFETY: `adapter` is a live IDXGIAdapter3 owned by this thread; the query
                // fills the local out-struct `mi`.
                let info = unsafe {
                    adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut mi)
                };
                if info.is_ok() {
                    let (usage, budget) = (mi.CurrentUsage, mi.Budget);
                    // checked_div = the budget>0 guard (a fresh/lost adapter reports 0).
                    // usage is bytes; *100 cannot overflow u64 at any real VRAM size.
                    if let Some(pct) = (usage * 100).checked_div(budget) {
                        if realtime && pct > VRAM_DOWNGRADE_PCT {
                            // SAFETY: pseudo-handle + by-name gdi32 call (setter's contract).
                            let st = unsafe {
                                d3dkmt_set_scheduling_priority_class(GetCurrentProcess(), 4)
                            };
                            if st == Some(0) {
                                realtime = false;
                                clean_ticks = 0;
                                tracing::warn!(
                                    vram_pct = pct,
                                    "GPU priority REALTIME→HIGH (VRAM tightened — NVENC-hang \
                                     hazard window)"
                                );
                            }
                        } else if !realtime && pct <= VRAM_RESTORE_PCT {
                            clean_ticks += 1;
                            if clean_ticks >= VRAM_RESTORE_TICKS {
                                // SAFETY: same setter contract as above.
                                let st = unsafe {
                                    d3dkmt_set_scheduling_priority_class(GetCurrentProcess(), 5)
                                };
                                if st == Some(0) {
                                    realtime = true;
                                    tracing::info!(
                                        vram_pct = pct,
                                        "GPU priority HIGH→REALTIME (auto: VRAM headroom \
                                         comfortable)"
                                    );
                                } else {
                                    // Can't ever reach REALTIME (privilege) — stop burning polls.
                                    tracing::info!(
                                        "pf-gpu-prio: REALTIME unavailable — monitor exiting \
                                         (HIGH stands)"
                                    );
                                    return;
                                }
                            }
                        } else if !realtime {
                            clean_ticks = 0;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
}
