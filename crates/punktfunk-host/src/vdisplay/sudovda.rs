//! Windows virtual-display backend driving **SudoVDA** (the SudoMaker Virtual Display Adapter —
//! the Indirect Display Driver the Apollo Sunshine-fork ships). The Windows analogue of the
//! Linux per-compositor backends: [`create`](VirtualDisplay::create) adds a virtual monitor at the
//! client's exact `WxH@Hz` (the mode is baked into the ADD IOCTL — no EDID seeding), starts the
//! mandatory watchdog ping, and the returned [`VirtualOutput`]'s keepalive `Drop` removes it (RAII).
//!
//! Control surface (verified live against SudoVDA 0.2.1): a device-interface-GUID + `CreateFileW`
//! + `DeviceIoControl` IOCTL protocol. No DLL, no named pipe. See `docs/windows-host.md`.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

/// Monotonic monitor generation. Each [`create_monitor`] stamps the next value onto the [`Monitor`]
/// and its [`MonitorLease`]s, so a lease whose monitor was already torn down + recreated (the IDD-push
/// reconnect-preempt path) is ignored on drop instead of decrementing the NEW monitor's refcount.
static MON_GEN: AtomicU64 = AtomicU64::new(1);

/// The gen of the CURRENTLY-active monitor. A session capturer captures this at open and re-checks it
/// each frame; when it changes (a reconnect preempted + recreated the monitor), the old session bails
/// IMMEDIATELY instead of lingering on the dead ring's 20s frame deadline — which would otherwise hold
/// its NVENC encoder open and exhaust the GPU's encode-session limit under rapid reconnects.
pub(crate) static CURRENT_MON_GEN: AtomicU64 = AtomicU64::new(0);

/// IDD-push mode: a new client connection preempts + recreates the monitor (single-client reconnect),
/// because a REUSED IddCx monitor's swap-chain is dead. Off → monitors are shared across sessions.
fn idd_push_mode() -> bool {
    std::env::var_os("PUNKTFUNK_IDD_PUSH").is_some()
}
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_FORCE_MODE_ENUMERATION,
    SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_TEST, CDS_UPDATEREGISTRY, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_DISPLAY_SETTINGS_MODE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::{Mode, VirtualDisplay, VirtualOutput};

// SudoVDA device-interface GUID (Common/Include/sudovda-ioctl.h).
const SUVDA_INTERFACE: GUID = GUID::from_u128(0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D);

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0).
const fn ctl(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}
const IOCTL_ADD: u32 = ctl(0x800);
const IOCTL_REMOVE: u32 = ctl(0x801);
const IOCTL_SET_RENDER_ADAPTER: u32 = ctl(0x802); // == 0x0022_2008
const IOCTL_GET_WATCHDOG: u32 = ctl(0x803);
/// pf-vdisplay extension (NOT in SudoVDA): tear down every virtual monitor. Sent once on host startup
/// to reap monitors orphaned by a crashed/killed previous host. SudoVDA returns invalid (ignored).
const IOCTL_CLEAR_ALL: u32 = ctl(0x804);
const IOCTL_DRIVER_PING: u32 = ctl(0x888);
const IOCTL_GET_VERSION: u32 = ctl(0x8FF);

/// A UNIQUE-per-session SudoVDA monitor GUID. The monitor is keyed by GUID for IOCTL_ADD/REMOVE, so a
/// FIXED GUID makes overlapping sessions (a client reconnecting after a freeze before the old session
/// has torn down, or genuine concurrent sessions) all map to the SAME monitor — then one session's
/// IOCTL_REMOVE on teardown tears the monitor down OUT FROM UNDER a still-live session ("display
/// disconnected" sound + freeze, even with no context change — observed live). Make it unique per
/// (process, session): base GUID with the low 48-bit node = (pid << 16 | session#).
fn next_monitor_guid() -> GUID {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id() as u128;
    GUID::from_u128(0x70756E6B_7466_756E_6B30_000000000000u128 | (pid << 16) | (n & 0xFFFF))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddParams {
    width: u32,
    height: u32,
    refresh: u32,
    guid: GUID,
    device_name: [u8; 14],
    serial: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddOut {
    luid: LUID,
    target_id: u32,
}

// SET_RENDER_ADAPTER input — byte-identical to SudoVDA's `{ LUID AdapterLuid; }` (8 bytes). The
// windows `LUID` is `{ LowPart: u32, HighPart: i32 }` == the C `LUID`, so `#[repr(C)]` is exact.
#[repr(C)]
#[derive(Clone, Copy)]
struct SetRenderAdapterParams {
    luid: LUID,
}

/// Pin the SudoVDA IDD's RENDER GPU to `luid` (Apollo's `SetRenderAdapter`). No output buffer. MUST be
/// issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target renders on — on a
/// multi-adapter box (SudoVDA IDD + a discrete GPU) this stops DXGI from reparenting the virtual
/// output onto a different adapter than the one we duplicate/encode on (the ACCESS_LOST storm).
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let p = SetRenderAdapterParams { luid };
    let bytes = std::slice::from_raw_parts(
        &p as *const _ as *const u8,
        size_of::<SetRenderAdapterParams>(),
    );
    let mut none: [u8; 0] = [];
    ioctl(h, IOCTL_SET_RENDER_ADAPTER, bytes, &mut none)
        .map(|_| ())
        .context("SudoVDA SET_RENDER_ADAPTER")
}

/// Resolve the LUID of the GPU that should RENDER the virtual display = the GPU that drives NVENC +
/// Desktop Duplication (e.g. the RTX 4090). Default: the discrete adapter with the most
/// `DedicatedVideoMemory`, skipping WARP / Basic-Render and the SudoVDA software adapter (≈0 VRAM).
/// `PUNKTFUNK_RENDER_ADAPTER=<substring>` forces a match by Description (Apollo's `adapter_name`).
/// `pub(crate)` so the IDD direct-push capturer can create its shared textures on the same discrete
/// GPU it pins here (and where NVENC runs).
pub(crate) unsafe fn resolve_render_adapter_luid() -> Option<LUID> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let want = std::env::var("PUNKTFUNK_RENDER_ADAPTER")
        .ok()
        .filter(|s| !s.is_empty());
    let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
    let mut best: Option<(LUID, u64, String)> = None;
    let mut i = 0u32;
    while let Ok(a) = factory.EnumAdapters1(i) {
        i += 1;
        let Ok(d) = a.GetDesc1() else { continue };
        let name = String::from_utf16_lossy(&d.Description);
        let name = name.trim_end_matches('\u{0}').to_string();
        let lname = name.to_ascii_lowercase();
        if lname.contains("basic render") || lname.contains("warp") {
            continue; // never pin to the software rasterizer
        }
        if let Some(w) = &want {
            if lname.contains(&w.to_ascii_lowercase()) {
                tracing::info!(
                    adapter = name,
                    "render adapter chosen by PUNKTFUNK_RENDER_ADAPTER"
                );
                return Some(d.AdapterLuid);
            }
            continue;
        }
        let vram = d.DedicatedVideoMemory as u64; // SudoVDA software adapter ≈ 0 → loses to the dGPU
        if best.as_ref().is_none_or(|(_, v, _)| vram > *v) {
            best = Some((d.AdapterLuid, vram, name));
        }
    }
    match best {
        Some((luid, vram, name)) => {
            tracing::info!(
                adapter = name,
                vram_mb = vram / (1024 * 1024),
                "render adapter chosen (max VRAM)"
            );
            Some(luid)
        }
        None => {
            tracing::warn!("no suitable render adapter found for SET_RENDER_ADAPTER");
            None
        }
    }
}

#[repr(C)]
struct RemoveParams {
    guid: GUID,
}

/// One `DeviceIoControl` round trip (METHOD_BUFFERED). `input`/`output` may be empty.
unsafe fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
    DeviceIoControl(
        h,
        code,
        inp,
        input.len() as u32,
        outp,
        output.len() as u32,
        Some(&mut returned),
        None,
    )
    .with_context(|| format!("DeviceIoControl(code={code:#x})"))?;
    Ok(returned)
}

/// Resolve the `\\.\DisplayN` GDI name for a SudoVDA target id via the CCD API. Returns `None`
/// until the OS activates the target into the desktop topology (needs a real WDDM GPU; on a
/// GPU-less box this stays `None` even though ADD succeeded).
pub(crate) unsafe fn resolve_gdi_name(target_id: u32) -> Option<String> {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        return None;
    }
    for p in paths.iter().take(np as usize) {
        if p.targetInfo.id == target_id {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) == 0 {
                let name = String::from_utf16_lossy(&src.viewGdiDeviceName);
                return Some(name.trim_end_matches('\u{0}').to_string());
            }
        }
    }
    None
}

/// Toggle the SudoVDA target's advanced-color (HDR) state via the CCD API. Disabling HDR while on the
/// secure (Winlogon) desktop makes it render SDR/composed so DXGI Desktop Duplication can capture it
/// (the HDR fullscreen independent-flip otherwise storms `ACCESS_LOST` → black); re-enable on return so
/// WGC keeps HDR on the normal desktop. Returns true on a successful `DisplayConfigSetDeviceInfo`.
pub(crate) unsafe fn set_advanced_color(target_id: u32, enable: bool) -> bool {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        return false;
    }
    for p in paths.iter().take(np as usize) {
        if p.targetInfo.id == target_id {
            let mut s = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE::default();
            s.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
            s.header.size = size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>() as u32;
            s.header.adapterId = p.targetInfo.adapterId;
            s.header.id = p.targetInfo.id;
            s.Anonymous.value = enable as u32; // bit 0 = enableAdvancedColor
            let rc = DisplayConfigSetDeviceInfo(&s.header);
            tracing::info!(
                target_id,
                enable,
                rc,
                "SudoVDA set advanced-color (HDR) state"
            );
            return rc == 0;
        }
    }
    tracing::warn!(
        target_id,
        "set_advanced_color: target not found in active paths"
    );
    false
}

/// Read the SudoVDA target's CURRENT advanced-color (HDR) state via the CCD API — i.e. whether HDR is
/// actually ON for the virtual display right now (e.g. because the user toggled it in Windows display
/// settings). The capture/encode pipeline follows the monitor's real colorspace (WGC → FP16 → NVENC
/// Main10 BT.2020 PQ), so this is the authoritative "is this an HDR session" signal — NOT the
/// handshake-negotiated bit depth. Returns false if the target isn't found / the query fails.
pub(crate) unsafe fn advanced_color_enabled(target_id: u32) -> bool {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        return false;
    }
    for p in paths.iter().take(np as usize) {
        if p.targetInfo.id == target_id {
            let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            info.header.size = size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            info.header.adapterId = p.targetInfo.adapterId;
            info.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut info.header) == 0 {
                // value bit 1 = advancedColorEnabled (bit 0 = advancedColorSupported).
                return (info.Anonymous.value & 0x2) != 0;
            }
            return false;
        }
    }
    false
}

/// Force the freshly-added SudoVDA monitor to the client's exact `WxH@Hz`. The ADD IOCTL only
/// ADVERTISES the mode; Windows otherwise activates an IDD target at a 1280x720 default, so the
/// ACTIVE mode (what DXGI Desktop Duplication captures) must be set explicitly. CDS_TEST first so a
/// mode the driver didn't advertise just leaves the default instead of erroring the session.
fn set_active_mode(gdi_name: &str, mode: Mode) {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();

    // Enumerate the modes the driver actually advertises for this output and pick the best match for
    // the requested RESOLUTION: the exact refresh if present, else the highest advertised refresh
    // <= requested, else the highest available at that resolution. The SudoVDA ADD IOCTL advertises
    // the client mode, but a very high pixel rate (e.g. 5120x1440@240 = 1.77 Gpix/s) can be clamped
    // or absent — falling back to a lower refresh AT THE SAME RESOLUTION keeps the client's
    // resolution (what the user sees) instead of collapsing to the 1280x720/1920x1080 OS default.
    let mut at_res: Vec<u32> = Vec::new();
    let mut res_set: std::collections::BTreeSet<(u32, u32)> = std::collections::BTreeSet::new();
    let mut i = 0u32;
    loop {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(wname.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(i),
                &mut dm,
            )
        }
        .as_bool();
        if !ok {
            break;
        }
        i += 1;
        res_set.insert((dm.dmPelsWidth, dm.dmPelsHeight));
        if dm.dmPelsWidth == mode.width && dm.dmPelsHeight == mode.height {
            at_res.push(dm.dmDisplayFrequency);
        }
    }
    let chosen_hz = if at_res.contains(&mode.refresh_hz) {
        mode.refresh_hz
    } else if let Some(hz) = at_res
        .iter()
        .copied()
        .filter(|&hz| hz <= mode.refresh_hz)
        .max()
    {
        hz
    } else if let Some(hz) = at_res.iter().copied().max() {
        hz
    } else {
        mode.refresh_hz // resolution not advertised at all; attempt anyway (likely -> OS default)
    };
    if at_res.is_empty() {
        tracing::warn!(
            "{gdi_name}: driver advertises no {}x{} mode (top advertised: {:?}); attempting @{} anyway",
            mode.width,
            mode.height,
            res_set.iter().rev().take(8).collect::<Vec<_>>(),
            mode.refresh_hz
        );
    } else if chosen_hz != mode.refresh_hz {
        tracing::info!(
            "{gdi_name}: {}x{}@{} not advertised; using {}x{}@{} (advertised refreshes here: {:?})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            mode.width,
            mode.height,
            chosen_hz,
            at_res
        );
    }

    // Set ONLY this output's mode in place (size/refresh/bpp; NO DM_POSITION). Do NOT promote it to
    // PRIMARY here and do NOT write a GLOBAL topology: promoting the IDD to primary at (0,0) while the
    // box's leftover basic display is still active contests the topology and storms
    // DXGI_ERROR_MODE_CHANGE_IN_PROGRESS (measured live). The IDD is made the sole → primary →
    // DWM-composited display by the CCD isolation in create() (which deactivates the other display
    // first), so a sole display is already primary and needs no CDS_SET_PRIMARY here.
    let dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL,
        dmBitsPerPel: 32,
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: chosen_hz,
        ..Default::default()
    };
    let test = unsafe {
        ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_TEST, None)
    };
    if test != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!(
            result = test.0,
            "{gdi_name}: driver rejected {}x{}@{} (mode not advertised?) — leaving OS default",
            mode.width,
            mode.height,
            chosen_hz
        );
        return;
    }
    let apply = unsafe {
        ChangeDisplaySettingsExW(
            PCWSTR(wname.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY,
            None,
        )
    };
    if apply == DISP_CHANGE_SUCCESSFUL {
        tracing::info!(
            "{gdi_name}: active mode set to {}x{}@{}",
            mode.width,
            mode.height,
            chosen_hz
        );
    } else {
        tracing::warn!(
            result = apply.0,
            "{gdi_name}: failed to apply {}x{}@{}",
            mode.width,
            mode.height,
            chosen_hz
        );
    }
}

/// Saved active display topology, for restoring on teardown.
type SavedConfig = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

/// `DISPLAYCONFIG_PATH_ACTIVE` (wingdi.h) — the `flags` bit marking a path active. The `windows` crate
/// doesn't export it, so define it here.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

/// Robust display isolation via the CCD API. The naive GDI approach (EnumDisplayDevices +
/// ChangeDisplaySettings) MISSES displays on a hybrid box — an iGPU-attached physical monitor isn't
/// flagged `ATTACHED_TO_DESKTOP` in the GDI enum, so it's never detached and the secure desktop /
/// lock screen lands on IT while our virtual output freezes. `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`
/// sees every active path; we deactivate all of them EXCEPT the SudoVDA target's, leaving the virtual
/// display as the sole desktop so ALL content (incl. Winlogon) renders to it. Apollo isolates the same
/// way (CCD). Returns the original active config to restore on teardown.
unsafe fn isolate_displays_ccd(keep_target_id: u32) -> Option<SavedConfig> {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        return None;
    }
    paths.truncate(np as usize);
    modes.truncate(nm as usize);
    let saved = (paths.clone(), modes.clone());
    let mut others = 0u32;
    for p in paths.iter_mut() {
        if p.targetInfo.id == keep_target_id {
            continue;
        }
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
            p.flags &= !DISPLAYCONFIG_PATH_ACTIVE; // mark this path inactive
            others += 1;
        }
    }
    if others == 0 {
        // The virtual path shows active in the CCD database (from set_active_mode's legacy
        // ChangeDisplaySettingsExW), but a legacy mode-set does NOT drive the IddCx adapter's
        // EVT_IDD_CX_ADAPTER_COMMIT_MODES — and without COMMIT_MODES the OS never calls
        // ASSIGN_SWAPCHAIN, so the driver never receives composed frames. Force an explicit CCD
        // SetDisplayConfig commit of the (sole) virtual path so the IddCx path actually activates.
        // SDC_FORCE_MODE_ENUMERATION makes the OS re-enumerate + re-commit even though the CCD DB
        // already lists the path active.
        let rc = SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_SAVE_TO_DATABASE
                | SDC_FORCE_MODE_ENUMERATION,
        );
        tracing::info!("display isolate (CCD): forced CCD re-commit of sole virtual path {keep_target_id} rc={rc:#x} (drives IddCx COMMIT_MODES → ASSIGN_SWAPCHAIN)");
        return Some(saved);
    }
    let rc = SetDisplayConfig(
        Some(paths.as_slice()),
        Some(modes.as_slice()),
        SDC_APPLY
            | SDC_USE_SUPPLIED_DISPLAY_CONFIG
            | SDC_ALLOW_CHANGES
            | SDC_FORCE_MODE_ENUMERATION,
    );
    if rc == 0 {
        tracing::info!("display isolate (CCD): deactivated {others} other display(s) — SudoVDA target {keep_target_id} is now the sole desktop");
    } else {
        tracing::warn!("display isolate (CCD): SetDisplayConfig failed rc={rc:#x} (tried to deactivate {others} path(s))");
    }
    Some(saved)
}

/// Restore the topology saved by [`isolate_displays_ccd`] (teardown, before the virtual output is
/// removed), re-activating the displays we deactivated.
unsafe fn restore_displays_ccd(saved: &SavedConfig) {
    let (paths, modes) = saved;
    if paths.is_empty() {
        return;
    }
    let rc = SetDisplayConfig(
        Some(paths.as_slice()),
        Some(modes.as_slice()),
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
    );
    tracing::info!("display isolate (CCD): restored original topology rc={rc:#x}");
}

unsafe fn open_device() -> Result<HANDLE> {
    let hdev = SetupDiGetClassDevsW(
        Some(&SUVDA_INTERFACE),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    )
    .context("SetupDiGetClassDevsW(SudoVDA) — is the SudoVDA driver installed?")?;

    let mut idata = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    SetupDiEnumDeviceInterfaces(hdev, None, &SUVDA_INTERFACE, 0, &mut idata)
        .context("SetupDiEnumDeviceInterfaces(SudoVDA)")?;

    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &idata, None, 0, Some(&mut required), None);
    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    SetupDiGetDeviceInterfaceDetailW(hdev, &idata, Some(detail), required, None, None)
        .context("SetupDiGetDeviceInterfaceDetailW(SudoVDA)")?;

    let handle = CreateFileW(
        PCWSTR((*detail).DevicePath.as_ptr()),
        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(SudoVDA device)")?;
    let _ = SetupDiDestroyDeviceInfoList(hdev);
    Ok(handle)
}

// ── Host-level reference-counted SudoVDA monitor lifecycle ──────────────────────────────────────
//
// The virtual monitor is created on the first session and REUSED across sessions. When the last
// session disconnects the monitor LINGERS for a grace window (PUNKTFUNK_MONITOR_LINGER_MS, default
// 10 s): a reconnect within the window reuses it instantly (no new screen, no PnP connect/disconnect
// chime, no teardown/recreate kernel churn); after the window a background timer REMOVEs it so a
// physical-screen user gets their screen back. Overlapping sessions share one monitor via the
// refcount (teardown only at refs==0 + expired grace), so a stale session can never REMOVE a live
// session's monitor (the earlier collision). The control-device HANDLE is opened once and kept for
// the host lifetime — it's a handle, not a screen, so it creates no phantom display.

/// The resources backing one live SudoVDA monitor (owned by [`MGR`], not by any session).
struct Monitor {
    guid: GUID,
    target_id: u32,
    luid: LUID,
    gdi_name: Option<String>,
    mode: Mode,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    ccd_saved: Option<SavedConfig>,
    /// Generation stamp ([`MON_GEN`]); a [`MonitorLease`] only releases if its gen still matches.
    gen: u64,
}

enum MgrState {
    Idle,
    Active { mon: Monitor, refs: u32 },
    Lingering { mon: Monitor, until: Instant },
}

struct Mgr {
    /// Control-device handle (raw isize; `HANDLE` isn't `Send`). Opened once, kept for the host life.
    device: Option<isize>,
    watchdog_s: u32,
    state: MgrState,
}

static MGR: Mutex<Mgr> = Mutex::new(Mgr {
    device: None,
    watchdog_s: 3,
    state: MgrState::Idle,
});

/// The Windows virtual-display backend. A marker — the monitor lifecycle lives in the global [`MGR`].
pub struct SudoVdaDisplay;

impl SudoVdaDisplay {
    pub fn new() -> Result<Self> {
        // Open the control device once (validates the driver is present) + log version/watchdog.
        let mut g = MGR.lock().unwrap();
        mgr_ensure_device(&mut g)?;
        Ok(Self)
    }
}

impl Drop for SudoVdaDisplay {
    fn drop(&mut self) {
        // Nothing: the control device + monitor lifecycle are host-level (owned by MGR) and
        // deliberately outlive any single session so a reconnect can reuse the monitor.
    }
}

impl VirtualDisplay for SudoVdaDisplay {
    fn name(&self) -> &'static str {
        "sudovda"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Delegate to the host-level manager: create the monitor, reuse a lingering one on reconnect,
        // or join the live one — and hand back a lease whose Drop releases the refcount.
        mgr_acquire(mode)
    }
}

/// Create a fresh SudoVDA monitor at `mode` on the (host-level) control `device`. The old per-session
/// `create()` body, now owned by the manager: ADD the target, start the watchdog ping, resolve the
/// GDI name, force the client mode + (default) isolate to a sole composited display. Returns the
/// [`Monitor`] resources; the manager tracks its lifecycle (refcount + linger).
unsafe fn create_monitor(device: isize, mode: Mode, watchdog_s: u32) -> Result<Monitor> {
    let dev = HANDLE(device as *mut c_void);
    {
        let mut device_name = [0u8; 14];
        let nm = b"punktfunk";
        device_name[..nm.len()].copy_from_slice(nm);
        // Fresh GUID per created monitor (the manager refcount, not the GUID, prevents the
        // cross-session REMOVE collision now).
        let session_guid = next_monitor_guid();
        let add = AddParams {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_hz,
            guid: session_guid,
            device_name,
            serial: [0u8; 14],
        };
        // SET_RENDER_ADAPTER is OPT-IN. Apollo runs with an EMPTY config and NEVER pins the render
        // adapter, yet captures the SudoVDA cleanly at the client mode on the 4090 (verified live on
        // this exact box: no ACCESS_LOST, no MODE_CHANGE storm). On this box our pin is IGNORED by the
        // driver AND the IDD lands on a DIFFERENT adapter (0x23664) than the one its DXGI output is
        // enumerated under (the 4090, where we make the capture device) — a cross-GPU mismatch that is
        // the real source of the perpetual ACCESS_LOST + MODE_CHANGE_IN_PROGRESS storm. So default to
        // NOT pinning — let the IDD use its natural adapter like Apollo. Opt in with
        // PUNKTFUNK_RENDER_ADAPTER=<name substring> only on a box that genuinely needs steering.
        let pinned = if std::env::var("PUNKTFUNK_RENDER_ADAPTER").is_ok() {
            unsafe { resolve_render_adapter_luid() }
        } else if std::env::var_os("PUNKTFUNK_IDD_PUSH").is_some() {
            // P2 direct frame push: the host opens the driver's shared textures AND runs NVENC on the
            // RENDER adapter, so on a hybrid box (4090 + iGPU) it MUST be the discrete encoder GPU —
            // an iGPU-rendered surface is untouchable by NVENC. pf-vdisplay HONORS SET_RENDER_ADAPTER
            // (SudoVDA ignored it), so pin the discrete GPU. The driver also reports the resulting
            // render LUID in the shared header, so the host binds correctly even if this is overridden.
            tracing::info!("IDD push: pinning the discrete render GPU (SET_RENDER_ADAPTER)");
            unsafe { resolve_render_adapter_luid() }
        } else {
            tracing::info!(
                "SudoVDA SET_RENDER_ADAPTER skipped (Apollo-parity: no render pin — avoids cross-GPU \
                 mismatch; set PUNKTFUNK_RENDER_ADAPTER=<name> to force a specific render GPU)"
            );
            None
        };
        if let Some(luid) = pinned {
            match unsafe { set_render_adapter(dev, luid) } {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                Err(e) => tracing::warn!("SudoVDA SET_RENDER_ADAPTER failed (continuing): {e:#}"),
            }
        }

        let add_bytes = unsafe {
            std::slice::from_raw_parts(&add as *const _ as *const u8, size_of::<AddParams>())
        };
        let mut out = [0u8; size_of::<AddOut>()];
        unsafe { ioctl(dev, IOCTL_ADD, add_bytes, &mut out) }.with_context(|| {
            format!(
                "SudoVDA ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        let ao = unsafe { *(out.as_ptr() as *const AddOut) };
        tracing::info!(
            "SudoVDA created {}x{}@{} (target_id={}, adapter_luid={:#x})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            ao.target_id,
            ao.luid.LowPart
        );
        if let Some(luid) = pinned {
            if ao.luid.LowPart == luid.LowPart && ao.luid.HighPart == luid.HighPart {
                tracing::info!("SudoVDA ADD render adapter matches the pinned GPU (pin took)");
            } else {
                tracing::warn!(
                    add = format!("{:08x}:{:08x}", ao.luid.HighPart, ao.luid.LowPart),
                    pinned = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA ADD render adapter DIFFERS from pinned — driver ignored SET_RENDER_ADAPTER?"
                );
            }
        }

        // Mandatory keepalive: ping inside the watchdog window or the driver tears all displays down.
        let stop = Arc::new(AtomicBool::new(false));
        let device_raw = device;
        let interval = Duration::from_millis(watchdog_s as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let pinger = thread::spawn(move || {
            let h = HANDLE(device_raw as *mut c_void);
            let mut warned = false;
            while !stop_t.load(Ordering::Relaxed) {
                let mut none: [u8; 0] = [];
                match unsafe { ioctl(h, IOCTL_DRIVER_PING, &[], &mut none) } {
                    Ok(_) => warned = false,
                    // A persistently failing PING means the cached control handle went invalid — the
                    // driver watchdog will then tear the monitor down mid-session. Surface it once
                    // (the old `let _ =` swallowed it, which masked exactly this during the bad-state churn).
                    Err(e) => {
                        if !warned {
                            tracing::warn!(
                                "SudoVDA keepalive PING failed (control handle lost?): {e:#}"
                            );
                            warned = true;
                        }
                    }
                }
                thread::sleep(interval);
            }
        });

        // Resolve the capture target. May be None on a GPU-less box (target added but not activated
        // into a WDDM path); the Windows capture backend will re-resolve once a GPU is present.
        let mut gdi_name = None;
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            if let Some(n) = unsafe { resolve_gdi_name(ao.target_id) } {
                gdi_name = Some(n);
                break;
            }
        }
        let mut ccd_saved: Option<SavedConfig> = None;
        match &gdi_name {
            Some(n) => {
                tracing::info!("SudoVDA target {} -> {n}", ao.target_id);
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Make the SudoVDA the SOLE active display (default). On this box an EXTENDED
                // (non-primary) IDD is NOT DWM-composited → Desktop Duplication gets a born-lost
                // ACCESS_LOST (measured live: MODE_CHANGE storm fixed, but the extended IDD then
                // born-lost). Apollo reaches the same end state ("Virtual Desktop: WxH" — the IDD is the
                // whole desktop, hence primary + composited) via Windows AUTO-promoting the real WDDM
                // display over the box's leftover 1024x768 basic display; Windows does NOT auto-promote
                // for us, so we deactivate the other display(s) explicitly via the clean atomic CCD path.
                // Deactivating FIRST means set_active_mode's primary-promotion has nothing to contest →
                // no MODE_CHANGE_IN_PROGRESS storm (that storm came from promoting primary WHILE the
                // basic display stayed active). Opt out with PUNKTFUNK_NO_ISOLATE=1 (a box with a real
                // second monitor to keep live). The legacy GDI detach is skipped — it misses
                // iGPU-attached monitors on a hybrid box and churns per-device; CCD is atomic.
                if std::env::var("PUNKTFUNK_NO_ISOLATE").is_err() {
                    ccd_saved = unsafe { isolate_displays_ccd(ao.target_id) };
                } else {
                    tracing::info!(
                        "display isolation skipped (PUNKTFUNK_NO_ISOLATE) — IDD stays extended"
                    );
                }
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens
            }
            None => tracing::warn!(
                "SudoVDA target {} not yet an active display path (needs a WDDM GPU to activate)",
                ao.target_id
            ),
        }

        Ok(Monitor {
            guid: session_guid,
            target_id: ao.target_id,
            luid: ao.luid,
            gdi_name,
            mode,
            stop,
            pinger: Some(pinger),
            ccd_saved,
            gen: MON_GEN.fetch_add(1, Ordering::Relaxed),
        })
    }
}

impl Monitor {
    /// The capture target handed to a session (`None` until the GDI name resolves).
    fn target(&self) -> Option<crate::capture::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| crate::capture::dxgi::WinCaptureTarget {
                adapter_luid: crate::capture::dxgi::pack_luid(self.luid),
                gdi_name: n,
                // target_id is stable across secure-desktop topology rebuilds; the GDI name is NOT,
                // so capture re-resolves the name from this on every recovery.
                target_id: self.target_id,
            })
    }

    /// Stop the watchdog ping, re-attach the displays we detached, then REMOVE the monitor (by GUID).
    /// `device` is the host-level control handle. Consumes the monitor.
    unsafe fn teardown(mut self, device: isize) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.pinger.take() {
            let _ = j.join();
        }
        // Re-attach detached display(s) BEFORE the REMOVE so the box is never left with zero displays.
        if let Some(saved) = &self.ccd_saved {
            restore_displays_ccd(saved);
        }
        let rp = RemoveParams { guid: self.guid };
        let rp_bytes =
            std::slice::from_raw_parts(&rp as *const _ as *const u8, size_of::<RemoveParams>());
        let mut none: [u8; 0] = [];
        let h = HANDLE(device as *mut c_void);
        if let Err(e) = ioctl(h, IOCTL_REMOVE, rp_bytes, &mut none) {
            tracing::warn!("SudoVDA REMOVE failed: {e:#}");
        } else {
            tracing::info!("SudoVDA monitor removed");
        }
    }
}

/// Open the control device once + read version/watchdog; cache the handle (raw isize) in `g`.
fn mgr_ensure_device(g: &mut Mgr) -> Result<isize> {
    if let Some(d) = g.device {
        return Ok(d);
    }
    let device = unsafe { open_device()? };
    let mut ver = [0u8; 4];
    if unsafe { ioctl(device, IOCTL_GET_VERSION, &[], &mut ver) }.is_ok() {
        tracing::info!(
            "SudoVDA protocol {}.{}.{} (test={})",
            ver[0],
            ver[1],
            ver[2],
            ver[3]
        );
    }
    let mut wd = [0u8; 8];
    g.watchdog_s = if unsafe { ioctl(device, IOCTL_GET_WATCHDOG, &[], &mut wd) }.is_ok() {
        u32::from_le_bytes([wd[0], wd[1], wd[2], wd[3]]).max(1)
    } else {
        3
    };
    tracing::info!("SudoVDA watchdog timeout {}s", g.watchdog_s);
    // Reap monitors orphaned by a crashed/killed previous host instance before we create ours.
    // pf-vdisplay honors IOCTL_CLEAR_ALL; SudoVDA returns invalid (ignored). Without it an orphan
    // lingers until the driver watchdog fires — but a still-pinging new session keeps resetting that
    // watchdog, so orphans could accumulate (the "5-6 stale monitors that never tear down" failure).
    {
        let mut none: [u8; 0] = [];
        if unsafe { ioctl(device, IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        }
    }
    let raw = device.0 as isize;
    g.device = Some(raw);
    Ok(raw)
}

/// Linger window before a session-less monitor is torn down. A reconnect within it reuses the
/// monitor (no new screen / PnP chime); after it the monitor is REMOVEd so a physical screen returns.
fn linger_ms() -> u64 {
    std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Acquire the shared monitor for a new session: join the live one (refcount++), reuse a lingering
/// one (reconfiguring if the client mode changed), or create one. The returned [`MonitorLease`]
/// releases the refcount on drop.
fn mgr_acquire(mode: Mode) -> Result<VirtualOutput> {
    ensure_linger_timer();
    let mut g = MGR.lock().unwrap();
    let device = mgr_ensure_device(&mut g)?;
    let watchdog_s = g.watchdog_s;

    // IDD-push: a new connection while a monitor is live = a single-client RECONNECT (the prior client
    // is gone — IDD-push is one display, no concurrency). A REUSED IddCx monitor's swap-chain is DEAD,
    // so joining it would hand the new client a black screen until the old session times out. PREEMPT:
    // tear the old monitor down (its Drop restores topology + IOCTL_REMOVEs) and fall through to create
    // a FRESH one. The old session's lease is gen-stamped, so its later drop is ignored (mgr_release
    // no-op) and can't tear down the new monitor.
    if idd_push_mode()
        && matches!(
            g.state,
            MgrState::Active { .. } | MgrState::Lingering { .. }
        )
    {
        if let MgrState::Active { mon, .. } | MgrState::Lingering { mon, .. } =
            std::mem::replace(&mut g.state, MgrState::Idle)
        {
            tracing::info!(
                old_target = mon.target_id,
                "IDD-push reconnect — preempting the prior session, recreating a fresh monitor"
            );
            // teardown() — NOT drop() — sends IOCTL_REMOVE (and restores topology). `Monitor` has NO
            // `Drop` impl, so a bare `drop(mon)` orphaned the IddCx monitor in the driver: it was never
            // departed, so it kept a live D3D device + a stuck swap-chain processor thread, and these
            // accumulated every reconnect (the driver-side churn leak: +1 device, ~36 nvwgf2umx threads,
            // ~50 MB VRAM per session, until it choked). teardown frees it via the driver's do_remove.
            unsafe { mon.teardown(device) };
            // Let the OS finish the ASYNC IddCx monitor departure before the next ADD. A back-to-back
            // REMOVE→ADD races the teardown and the ADD IOCTL is rejected (`DeviceIoControl failed`)
            // under reconnect churn. Held under the MGR lock, but IDD-push setup is already serialized
            // (IDD_SETUP_LOCK), so this only paces the recreate — exactly what a reconnect flood needs.
            thread::sleep(Duration::from_millis(400));
        }
    }

    // A live monitor already exists — join it (refcount++). This covers a concurrent session AND the
    // build-then-drop overlap of a mid-stream Reconfigure / secure-return (the new lease is taken while
    // the old is still held). If the requested mode differs, reconfigure the shared monitor to it so a
    // Reconfigure actually applies (one shared monitor → sessions necessarily share a mode).
    if let MgrState::Active { mon, refs } = &mut g.state {
        *refs += 1;
        let changed = mon.mode.width != mode.width
            || mon.mode.height != mode.height
            || mon.mode.refresh_hz != mode.refresh_hz;
        if changed {
            unsafe { mgr_reconfigure(mon, mode) };
        }
        tracing::info!(
            refs = *refs,
            "SudoVDA monitor reused (concurrent / reconfigure session)"
        );
        let pm = Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz));
        let target = mon.target();
        let gen = mon.gen;
        CURRENT_MON_GEN.store(gen, Ordering::Relaxed);
        return Ok(VirtualOutput {
            node_id: 0,
            preferred_mode: pm,
            win_capture: target,
            keepalive: Box::new(MonitorLease { gen }),
        });
    }

    // Idle or Lingering: repurpose/create a monitor → Active{refs:1}.
    let mon = match std::mem::replace(&mut g.state, MgrState::Idle) {
        MgrState::Lingering { mut mon, .. } => {
            tracing::info!("SudoVDA monitor reused (reconnect within the linger window)");
            let changed = mon.mode.width != mode.width
                || mon.mode.height != mode.height
                || mon.mode.refresh_hz != mode.refresh_hz;
            if changed {
                unsafe { mgr_reconfigure(&mut mon, mode) };
            }
            mon
        }
        MgrState::Idle => unsafe { create_monitor(device, mode, watchdog_s)? },
        MgrState::Active { .. } => unreachable!("handled above"),
    };
    let pm = Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz));
    let target = mon.target();
    let gen = mon.gen;
    CURRENT_MON_GEN.store(gen, Ordering::Relaxed);
    g.state = MgrState::Active { mon, refs: 1 };
    Ok(VirtualOutput {
        node_id: 0,
        preferred_mode: pm,
        win_capture: target,
        keepalive: Box::new(MonitorLease { gen }),
    })
}

/// Re-apply a (possibly new) mode to a reused monitor on reconnect, re-resolving its GDI name.
unsafe fn mgr_reconfigure(mon: &mut Monitor, mode: Mode) {
    tracing::info!(
        old = format!(
            "{}x{}@{}",
            mon.mode.width, mon.mode.height, mon.mode.refresh_hz
        ),
        new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
        "SudoVDA: reconfiguring reused monitor to the new client mode"
    );
    if let Some(n) = resolve_gdi_name(mon.target_id) {
        mon.gdi_name = Some(n);
    }
    if let Some(n) = &mon.gdi_name {
        set_active_mode(n, mode);
    }
    mon.mode = mode;
}

/// Release a session's hold: refcount-- ; when the last session leaves, LINGER before teardown.
/// `gen` is the lease's monitor generation: a STALE lease (its monitor was already torn down +
/// recreated under it — the IDD-push reconnect-preempt path) does nothing, so it can't decrement the
/// CURRENT (fresh) monitor's refcount and tear it down.
fn mgr_release(gen: u64) {
    let mut g = MGR.lock().unwrap();
    let stale = match &g.state {
        MgrState::Active { mon, .. } | MgrState::Lingering { mon, .. } => mon.gen != gen,
        MgrState::Idle => true,
    };
    if stale {
        return;
    }
    g.state = match std::mem::replace(&mut g.state, MgrState::Idle) {
        MgrState::Active { mon, refs } if refs > 1 => MgrState::Active {
            mon,
            refs: refs - 1,
        },
        MgrState::Active { mon, .. } => {
            let ms = linger_ms();
            tracing::info!(
                linger_ms = ms,
                "SudoVDA: last session left — lingering before teardown"
            );
            MgrState::Lingering {
                mon,
                until: Instant::now() + Duration::from_millis(ms),
            }
        }
        other => other,
    };
}

/// Wait (up to `timeout`) for the active monitor to be RELEASED — i.e. the MGR is no longer `Active`
/// (the prior session dropped its lease → `Lingering`/`Idle`). Used by the IDD-push reconnect preempt:
/// after signalling the old session to stop, we wait here so it tears its monitor down CLEANLY (while
/// frames still flow) before we acquire a fresh one — instead of dropping the monitor out from under a
/// still-live session, which churns the driver's ADD/REMOVE path and wedges it under rapid reconnects.
pub(crate) fn wait_for_monitor_released(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !matches!(MGR.lock().unwrap().state, MgrState::Active { .. }) {
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "IDD-push preempt: prior session didn't release the monitor within {timeout:?} — \
                 proceeding (mgr_acquire will preempt it)"
            );
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Background timer (started once): tear down a monitor that has lingered past its deadline (→ Idle),
/// so a physical-screen user gets their screen back after they stop streaming.
fn ensure_linger_timer() {
    static TIMER: Once = Once::new();
    TIMER.call_once(|| {
        let _ = thread::Builder::new()
            .name("sudovda-linger".into())
            .spawn(|| loop {
                thread::sleep(Duration::from_millis(500));
                let mut g = MGR.lock().unwrap();
                let due = matches!(&g.state, MgrState::Lingering { until, .. } if Instant::now() >= *until);
                if due {
                    let device = g.device.unwrap_or(0);
                    if let MgrState::Lingering { mon, .. } =
                        std::mem::replace(&mut g.state, MgrState::Idle)
                    {
                        drop(g); // release the lock before the REMOVE IOCTL + display restore
                        unsafe { mon.teardown(device) };
                    }
                }
            });
    });
}

/// A session's lease on the shared monitor. Drop releases the refcount (→ linger when it hits 0),
/// UNLESS the monitor was already torn down + recreated under it (gen mismatch — the IDD-push
/// reconnect-preempt path), in which case the drop is a no-op so it can't tear down the new monitor.
struct MonitorLease {
    gen: u64,
}
impl Drop for MonitorLease {
    fn drop(&mut self) {
        mgr_release(self.gen);
    }
}

/// Readiness probe: can we open the SudoVDA control device?
pub fn probe() -> Result<()> {
    let h = unsafe { open_device()? };
    unsafe {
        let _ = CloseHandle(h);
    }
    Ok(())
}

/// Is the SudoVDA driver present (device interface enumerable)?
pub fn is_available() -> bool {
    unsafe { open_device().map(|h| CloseHandle(h)).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live hardware round trip — skipped unless `PUNKTFUNK_SUDOVDA_LIVE=1` (needs the SudoVDA
    /// driver installed). Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    fn live_create_drop() {
        if std::env::var("PUNKTFUNK_SUDOVDA_LIVE").is_err() {
            return;
        }
        let mut vd = SudoVdaDisplay::new().expect("open SudoVDA");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        assert_eq!(vout.preferred_mode, Some((1920, 1080, 60)));
        thread::sleep(Duration::from_secs(3));
        drop(vout); // triggers REMOVE + stops the pinger
    }
}
